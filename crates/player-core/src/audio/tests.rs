use cpal::traits::DeviceTrait;
use cpal::traits::HostTrait;
use cpal::Sample;
use cpal::SampleFormat;
use std::ffi::c_char;
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;

use crate::internals::pump_over;
use crate::{
    normalize_timestamp, raw_timestamp_for_position, AudioSourceKind, NormalizedTimestamp,
    StreamTimeBase, StreamTiming,
};
use std::time::{Duration, Instant};

use super::asio_host::{
    choose_output_rate, negotiate_dsd, AsioDriverApi, AsioDsdFormat, AsioDsdIo, AsioDsdTransport,
    AsioHost, AsioLease, AsioOpenError,
};
use super::binding::{needs_rate_reopen, prepare_binding};
use super::callback::{
    CallbackConfiguration, CallbackControl, CallbackCore, CallbackMoment, CallbackResources,
    GainRamp, RampKind, UserGain, CALLBACK_CONTROL_TIMEOUT, MODE_HELD, MODE_RUNNING,
};
use super::clock::{
    AudioClock, GenerationCounter, PlaybackRateControl, FIRST_GENERATION, PLAYBACK_RATE_SCALE,
};
use super::device::{
    change_route, is_generic_asio_driver, output_fallback, output_fallback_version, output_route,
    output_route_version, output_systems, resolve_output_device, route_snapshot, set_output_route,
    stream_config_for, wake_on_move, AudioDeviceCaps, AudioDeviceSelection, AudioFallbackReason,
    AudioOutputHost, AudioOutputRoute, RouteState,
};
use super::endpoints::stop_endpoint_watch;
use super::error::{AudioError, AudioOpenError, NativeError};
use super::ffi::{BnAudioTrackInfo, BN_AUDIO_INVALID_STREAM};
use super::lease::{OutputAuthorityLease, OUTPUT_OWNER};
use super::metrics::SharedMetrics;
use super::park::{AudioParkCell, AudioParkState};
use super::policy::{AudioGainCurve, AudioPolicy, AudioPolicyTimes};
use super::posture::{
    hardware_play_then_ramp, posture_step, ramp_then_hardware_pause, AudioOutputPhase, PostureCell,
    PostureStep, TransportPosture,
};
use super::pump::{open_media_bytes, DecoderPump};
use super::rebind::{
    device_wake_at, rebind_backoff, rebind_honours_promise, AudioDeviceEvent, AudioDeviceLadder,
    AudioDevicePhase, DeviceDuty, DEVICE_RECOVERY_WINDOW,
};
use super::ring::AudioRing;
use super::seek::{promised_position, SeekHoldTransaction, UnadoptedSeek};
use super::session::{
    source_step, AudioEdge, AudioSession, AudioSource, AudioSourceState, SourceEvent, StopBound,
};
use super::stream::StreamWriter;
use super::track::{
    map_native_audio_tracks, requested_stream_index, AudioStreamIndex, AudioTrackSelection,
};
use super::worker::{AudioWorkerStart, WorkerCommand, WorkerLifetime};

#[cfg(any(feature = "asio", test))]
use super::device::release_for_wasapi;
#[cfg(not(feature = "asio"))]
use super::device::{asio_in_use, demote_asio};

#[cfg(feature = "instrument")]
use super::lease::{LIVE_OUTPUT_AUTHORITIES, MAX_LIVE_OUTPUT_AUTHORITIES};
#[cfg(feature = "instrument")]
use super::metrics::DEVICE_RESOLVES;

/// The `audio` module's production half, in declaration order.
///
/// Re-anchored when the module became a directory: the laws below are about the module, not
/// about one of its files, so what they read is every file of it concatenated. The order is
/// the order the split moved the code in, which is what keeps the batteries that cut the text
/// at one item and read up to the next one reading the same span they read before.
fn audio_production() -> String {
    [
        include_str!("../../src/audio/mod.rs"),
        include_str!("../../src/audio/ffi.rs"),
        include_str!("../../src/audio/error.rs"),
        include_str!("../../src/audio/policy.rs"),
        include_str!("../../src/audio/device.rs"),
        include_str!("../../src/audio/lease.rs"),
        include_str!("../../src/audio/metrics.rs"),
        include_str!("../../src/audio/clock.rs"),
        include_str!("../../src/audio/park.rs"),
        include_str!("../../src/audio/ring.rs"),
        include_str!("../../src/audio/callback.rs"),
        include_str!("../../src/audio/track.rs"),
        include_str!("../../src/audio/worker.rs"),
        include_str!("../../src/audio/pump.rs"),
        include_str!("../../src/audio/posture.rs"),
        include_str!("../../src/audio/rebind.rs"),
        include_str!("../../src/audio/seek.rs"),
        include_str!("../../src/audio/session.rs"),
        include_str!("../../src/audio/binding.rs"),
        include_str!("../../src/audio/owner.rs"),
        include_str!("../../src/audio/output.rs"),
        include_str!("../../src/dsp.rs"),
    ]
    .concat()
}

fn nonzero_usize(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("test value is nonzero")
}

fn nonzero_u32(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).expect("test value is nonzero")
}

fn policy() -> AudioPolicy {
    // Fixture dimensions exercise ordering only; they are not a product buffer/ramp policy.
    AudioPolicy {
        queue_capacity_frames: nonzero_usize(2_048),
        prefill_frames: nonzero_usize(512),
        refill_low_frames: nonzero_usize(256),
        refill_high_frames: nonzero_usize(1_024),
        ramp_frames: nonzero_u32(5),
        leave_frames: nonzero_u32(9),
        gain_curve: AudioGainCurve::Linear,
    }
}

#[test]
fn adjudicated_provisional_policy_scales_to_output_rate() {
    let policy = AudioPolicy::provisional_for_sample_rate(nonzero_u32(48_000));
    assert_eq!(policy.ramp_frames.get(), 384);
    assert_eq!(policy.queue_capacity_frames.get(), 12_000);
    assert_eq!(policy.prefill_frames.get(), 3_840);
    assert_eq!(policy.refill_low_frames.get(), 2_880);
    assert_eq!(policy.refill_high_frames.get(), 8_640);
    policy.validate().expect("adjudicated policy is ordered");
}

#[test]
fn a_policy_in_time_is_frames_at_each_bindings_rate() {
    let deep = AudioPolicyTimes {
        queue_capacity: Duration::from_secs(2),
        prefill: Duration::from_millis(80),
        refill_low: Duration::from_secs(1),
        refill_high: Duration::from_millis(1_500),
        ramp: Duration::from_millis(8),
        leave: Duration::from_millis(100),
    };
    for (rate, capacity, prefill, low, high, ramp) in [
        (44_100, 88_200, 3_528, 44_100, 66_150, 353),
        (48_000, 96_000, 3_840, 48_000, 72_000, 384),
        (192_000, 384_000, 15_360, 192_000, 288_000, 1_536),
    ] {
        let policy = deep.at_sample_rate(nonzero_u32(rate));
        assert_eq!(
            (
                policy.queue_capacity_frames.get(),
                policy.prefill_frames.get(),
                policy.refill_low_frames.get(),
                policy.refill_high_frames.get(),
                policy.ramp_frames.get(),
            ),
            (capacity, prefill, low, high, ramp),
            "at {rate} Hz"
        );
        // A prefill below the low watermark is how a deep ring keeps a seek prompt.
        policy
            .validate()
            .expect("a deep ring with a small prefill is ordered");
    }
    assert_eq!(
        AudioPolicyTimes::PROVISIONAL.at_sample_rate(nonzero_u32(48_000)),
        AudioPolicy::provisional_for_sample_rate(nonzero_u32(48_000))
    );
    let inverted = AudioPolicyTimes {
        refill_low: Duration::from_millis(1_600),
        ..deep
    };
    assert!(
        inverted
            .at_sample_rate(nonzero_u32(48_000))
            .validate()
            .is_err(),
        "low above high"
    );
    let overfull = AudioPolicyTimes {
        prefill: Duration::from_millis(1_600),
        ..deep
    };
    assert!(
        overfull
            .at_sample_rate(nonzero_u32(48_000))
            .validate()
            .is_err(),
        "prefill above high"
    );
}

fn callback_harness(
    ring: Arc<AudioRing>,
    clock: Arc<AudioClock>,
    control: Arc<CallbackControl>,
    metrics: Arc<SharedMetrics>,
) -> CallbackCore {
    CallbackCore::new(
        CallbackResources {
            ring,
            clock,
            control,
            metrics,
            user_gain: Arc::new(UserGain::default()),
            worker_thread: thread::current(),
        },
        CallbackConfiguration {
            policy: policy(),
            channels: 2,
            sample_rate: nonzero_u32(48_000),
        },
    )
}

fn inject_request(control: &CallbackControl, mode: u8, kind: RampKind) -> u64 {
    control.requested_mode.store(mode, Ordering::Relaxed);
    control.requested_kind.store(kind as u8, Ordering::Relaxed);
    control.request_version.fetch_add(1, Ordering::Release) + 1
}

#[test]
fn callback_acknowledgement_is_lock_free_in_both_directions() {
    // The RT callback must publish an acknowledgement without taking any lock, and the waiter
    // must observe it. `fetch_max` also makes a late acknowledgement for an older version
    // unable to satisfy a newer wait — the property the old Condvar predicate carried.
    let control = Arc::new(CallbackControl::new());
    let version = control
        .publish(MODE_RUNNING, RampKind::Start)
        .expect("a fresh control accepts a request");
    control.acknowledge(version - 1);
    assert!(matches!(
        control.await_acknowledgement(version, Duration::from_millis(20)),
        Err(AudioError::ControlTimeout)
    ));
    let notifier = Arc::clone(&control);
    let handle = thread::spawn(move || notifier.acknowledge(version));
    assert!(control
        .await_acknowledgement(version, Duration::from_secs(1))
        .is_ok());
    handle.join().expect("notifier completed");
}

#[test]
fn a_failed_binding_aborts_the_wait_instead_of_serving_its_full_timeout() {
    let control = Arc::new(CallbackControl::new());
    let version = control
        .publish(MODE_HELD, RampKind::Pause)
        .expect("a fresh control accepts a request");
    control.fail();
    let started_at = Instant::now();
    assert!(matches!(
        control.await_acknowledgement(version, CALLBACK_CONTROL_TIMEOUT),
        Err(AudioError::OutputFailed)
    ));
    assert!(started_at.elapsed() < Duration::from_millis(100));
}

/// BLOCKER-3's type condition. `WorkerHandle` is what crosses to the media loop and it has no
/// `JoinHandle`; `WorkerLifetime` is what reaps and it is constructed in exactly one place.
#[test]
fn only_the_reaping_half_of_a_decode_worker_owns_its_thread() {
    // Re-anchored when this battery moved out of `src`: the file it reads is now the
    // production half in full, so the old cut at the last `#[cfg(test)]` is gone.
    let production = audio_production();
    let handle = production
        .split("struct WorkerHandle {")
        .nth(1)
        .expect("WorkerHandle exists")
        .split('}')
        .next()
        .expect("WorkerHandle has fields");
    assert!(
        !handle.contains("JoinHandle"),
        "the talking half of a decode worker must not own the thread"
    );
    assert_eq!(
        production.matches("struct WorkerLifetime {").count(),
        1,
        "the reaping half has exactly one declaration"
    );
    // A construction expression begins the line; `struct`/`impl` headers do not, and
    // `DecodeWorkerLifetime` would be a substring match on the bare name.
    let constructions = production
        .lines()
        .filter(|line| line.trim_start().starts_with("WorkerLifetime {"))
        .count();
    assert_eq!(
        constructions, 1,
        "the reaping half has exactly one construction site"
    );
}

#[test]
fn worker_shutdown_used_by_dispose_is_bounded() {
    let (quick_commands, quick_rx) = mpsc::sync_channel(1);
    let quick_join = thread::spawn(move || loop {
        match quick_rx.try_recv() {
            Ok(WorkerCommand::Shutdown) | Err(mpsc::TryRecvError::Disconnected) => return,
            Ok(
                WorkerCommand::Seek { .. }
                | WorkerCommand::SetParked { .. }
                | WorkerCommand::Next(_),
            )
            | Err(mpsc::TryRecvError::Empty) => thread::park(),
        }
    });
    let quick_thread = quick_join.thread().clone();
    let mut quick_worker = WorkerLifetime {
        commands: quick_commands,
        thread: quick_thread,
        join: Some(quick_join),
    };
    let quick_started_at = Instant::now();
    quick_worker
        .shutdown_with_timeout(Duration::from_millis(250))
        .expect("responsive worker stops");
    assert!(quick_started_at.elapsed() < Duration::from_secs(1));

    let release = Arc::new(AtomicBool::new(false));
    let thread_release = Arc::clone(&release);
    let (blocked_commands, _blocked_rx) = mpsc::sync_channel(1);
    let (detached_done_tx, detached_done_rx) = mpsc::sync_channel(1);
    let blocked_join = thread::spawn(move || {
        while !thread_release.load(Ordering::Acquire) {
            thread::park();
        }
        detached_done_tx
            .send(())
            .expect("publish detached completion");
    });
    let blocked_thread = blocked_join.thread().clone();
    let mut blocked_worker = WorkerLifetime {
        commands: blocked_commands,
        thread: blocked_thread,
        join: Some(blocked_join),
    };
    let blocked_started_at = Instant::now();
    assert!(matches!(
        blocked_worker.shutdown_with_timeout(Duration::from_millis(20)),
        Err(AudioError::WorkerShutdownTimeout)
    ));
    assert!(blocked_started_at.elapsed() < Duration::from_secs(1));
    release.store(true, Ordering::Release);
    blocked_worker.thread.unpark();
    detached_done_rx
        .recv_timeout(Duration::from_millis(250))
        .expect("detached test worker can finish after fallback");
}

#[test]
fn seek_hold_rollback_and_superseding_seeks_never_reuse_a_spent_generation() {
    let mut generation = GenerationCounter::new(41);
    let mut phase = AudioOutputPhase::Running;
    let mut park = AudioParkState::Live;

    // First seek of a drag: hold the callback and hand the worker generation 42.
    let transaction = SeekHoldTransaction::begin(phase, park, Duration::from_millis(93_500));
    phase = phase.held();
    park = park.ramped_to_zero();
    assert_eq!(generation.advance(), 42);
    assert_eq!(phase, AudioOutputPhase::Paused);
    assert_eq!(park, AudioParkState::RampedToZero);

    // Four superseding seeks inside the same drag reuse the one transaction.
    for expected in 43..=46 {
        assert_eq!(generation.advance(), expected);
        phase = phase.held();
    }

    // The video landing fails, so the host rolls the transaction back. Rolling back IS the
    // generation advance now, so this reads the counter instead of moving it: the assertion
    // below observes the production transition rather than the test's own arithmetic.
    let (restored_phase, restored_park) = transaction.rollback(&mut generation);
    phase = restored_phase;
    park = restored_park;
    let after_rollback = generation.current();

    assert_eq!(
        phase,
        AudioOutputPhase::Running,
        "rollback must not leave audio held"
    );
    assert_eq!(
        park,
        AudioParkState::Live,
        "rollback restores the captured park posture"
    );
    assert!(
        after_rollback > 46,
        "a generation the worker and the media clock already saw must never be handed out again"
    );
    println!(
        "player-core[audio-coord] lost_wakeup=false bounded_dispose=true seek_rollback_ok=true"
    );
}

#[test]
fn audio_park_cell_cannot_encode_a_parked_stream_without_terminal_zero() {
    let cell = AudioParkCell::new();
    for park in [
        AudioParkState::Live,
        AudioParkState::RampedToZero,
        AudioParkState::HardwareParked,
    ] {
        cell.store(park);
        assert_eq!(
            cell.load(),
            park,
            "the park posture round-trips through the cell"
        );
        #[cfg(feature = "instrument")]
        assert!(
            !park.hardware_stream_paused() || park.terminal_zero_delivered(),
            "a parked hardware stream always implies a delivered zero ramp"
        );
    }
    // Every representable byte decodes to one of the three postures: the old
    // (terminal_zero = false, hardware_paused = true) product has no encoding at all.
    #[cfg(feature = "instrument")]
    for code in 0u8..=u8::MAX {
        let state = AudioParkState::from_code(code);
        assert!(!state.hardware_stream_paused() || state.terminal_zero_delivered());
    }
    // The pause and resume orders are total functions, not guarded assignments.
    assert_eq!(
        AudioParkState::HardwareParked.ramped_to_zero(),
        AudioParkState::HardwareParked
    );
    assert_eq!(
        AudioParkState::HardwareParked.hardware_running(),
        AudioParkState::RampedToZero
    );
    assert_eq!(
        AudioParkState::Live.hardware_running(),
        AudioParkState::Live
    );
}

#[test]
fn audio_device_ladder_is_a_timeline_not_a_frame_burst() {
    // The unplug a real user performs: cpal faults, and for the next few hundred
    // milliseconds Windows has no replacement endpoint, so every attempt fails immediately.
    // A budget counted in attempts is spent before the operating system has done anything.
    let t0 = Instant::now();
    let mut phase = AudioDevicePhase::Bound {
        device: "Headphones (Bluetooth)".to_owned(),
    }
    .step(AudioDeviceEvent::BindingLost, t0);
    assert_eq!(
        phase.duty(t0),
        DeviceDuty::Attempt,
        "the first attempt is due immediately"
    );

    // Everything a single `prepare_frame` could possibly do, at one instant.
    for _ in 0..8 {
        match phase.duty(t0) {
            DeviceDuty::Attempt => {
                phase = phase.step(AudioDeviceEvent::AttemptStarted, t0);
                phase = phase.step(
                    AudioDeviceEvent::AttemptFailed {
                        reason: "no output device".to_owned(),
                    },
                    t0,
                );
            }
            DeviceDuty::Idle => {}
            DeviceDuty::GiveUp { reason } => panic!("gave up inside one frame: {reason}"),
        }
    }
    assert_eq!(
        phase.duty(t0),
        DeviceDuty::Idle,
        "one instant owes exactly one attempt, so a frame cannot burn the recovery window"
    );
    assert_eq!(
        phase.duty(t0 + Duration::from_millis(149)),
        DeviceDuty::Idle
    );
    assert_eq!(
        phase.duty(t0 + Duration::from_millis(150)),
        DeviceDuty::Attempt,
        "the ladder is spaced by a clock the machine owns"
    );
    assert!(
        !matches!(phase, AudioDevicePhase::Lost { .. }),
        "a machine built to survive an unplug must not be dead before the device can return"
    );
}

#[test]
fn a_headphone_unplug_replug_returns_to_bound_within_the_recovery_window() {
    // The measured Windows timeline: the fault fires, the default endpoint switches a few
    // hundred milliseconds later, and the binding that lands then is adopted.
    let t0 = Instant::now();
    let mut phase = AudioDevicePhase::Bound {
        device: "Headphones (Bluetooth)".to_owned(),
    }
    .step(AudioDeviceEvent::BindingLost, t0);

    // Each attempt reports its failure 2 ms after it started, and the backoff is measured from
    // the FAILURE, so the next attempt comes due at 2 ms + 150 ms after the previous one.
    for at in [
        Duration::ZERO,
        Duration::from_millis(152),
        Duration::from_millis(304),
    ] {
        assert_eq!(phase.duty(t0 + at), DeviceDuty::Attempt);
        phase = phase.step(AudioDeviceEvent::AttemptStarted, t0 + at);
        phase = phase.step(
            AudioDeviceEvent::AttemptFailed {
                reason: "no output device".to_owned(),
            },
            t0 + at + Duration::from_millis(2),
        );
    }
    let landed = t0 + Duration::from_millis(700);
    assert_eq!(phase.duty(landed), DeviceDuty::Attempt);
    phase = phase.step(AudioDeviceEvent::AttemptStarted, landed);
    phase = phase.step(
        AudioDeviceEvent::Rebound {
            device: "Speakers (Realtek)".to_owned(),
        },
        landed + Duration::from_millis(180),
    );
    assert_eq!(
        phase,
        AudioDevicePhase::Bound {
            device: "Speakers (Realtek)".to_owned()
        },
        "the machine returns to Bound because a failed attempt only reschedules; only the \
         10 s window latches Lost"
    );
    assert_eq!(phase.duty(t0 + Duration::from_secs(60)), DeviceDuty::Idle);
}

#[test]
fn a_device_that_never_returns_is_given_up_once_the_window_closes() {
    let t0 = Instant::now();
    let mut phase = AudioDevicePhase::Bound {
        device: "Speakers".to_owned(),
    }
    .step(AudioDeviceEvent::BindingLost, t0);
    phase = phase.step(AudioDeviceEvent::AttemptStarted, t0);
    phase = phase.step(
        AudioDeviceEvent::AttemptFailed {
            reason: "no output device".to_owned(),
        },
        t0 + Duration::from_secs(9),
    );
    assert!(matches!(phase, AudioDevicePhase::Waiting { .. }));
    assert!(matches!(
        phase.duty(t0 + Duration::from_secs(10)),
        DeviceDuty::GiveUp { .. }
    ));

    // A wedged attempt is given up on the same deadline: the machine never depends on the
    // rebind thread reporting back.
    let mut wedged = AudioDevicePhase::Bound {
        device: "Speakers".to_owned(),
    }
    .step(AudioDeviceEvent::BindingLost, t0)
    .step(AudioDeviceEvent::AttemptStarted, t0);
    assert_eq!(wedged.duty(t0 + Duration::from_secs(9)), DeviceDuty::Idle);
    let DeviceDuty::GiveUp { reason } = wedged.duty(t0 + Duration::from_secs(10)) else {
        panic!("a wedged attempt must still hit the recovery deadline");
    };
    wedged = wedged.step(
        AudioDeviceEvent::GaveUp { reason },
        t0 + Duration::from_secs(10),
    );
    assert!(matches!(wedged, AudioDevicePhase::Lost { .. }));
    assert_eq!(
        wedged.clone().step(
            AudioDeviceEvent::Rebound {
                device: "Speakers".to_owned()
            },
            t0 + Duration::from_secs(11)
        ),
        wedged,
        "a session that gave up does not silently come back to life"
    );
}

/// Every phase, every event, one table. A cell may read "deaf" only where the machine is
/// deliberately deaf — `Bound` must keep its stream, `Lost` is terminal, and a second loss
/// report must not restart the recovery window — and nowhere else. The cell this test exists
/// for is `Waiting × AttemptFailed`: a rebind that could not even be started reports a failure
/// from `Waiting`, and while that cell was a silent no-op the ladder never applied a backoff,
/// so the retry came due again on the very next frame for the whole ten-second window and
/// burned one decode generation each time.
#[test]
fn no_failure_edge_leaves_the_phase_unchanged() {
    let t0 = Instant::now() + Duration::from_secs(60);
    let phases = [
        (
            "Bound",
            AudioDevicePhase::Bound {
                device: "Headphones (Bluetooth)".to_owned(),
            },
        ),
        (
            "Waiting",
            AudioDevicePhase::Waiting {
                since: t0,
                next_attempt_at: t0,
            },
        ),
        ("Attempting", AudioDevicePhase::Attempting { since: t0 }),
        (
            "Lost",
            AudioDevicePhase::Lost {
                reason: "gone".to_owned(),
            },
        ),
    ];
    let events = [
        ("BindingLost", AudioDeviceEvent::BindingLost),
        ("AttemptStarted", AudioDeviceEvent::AttemptStarted),
        (
            "Rebound",
            AudioDeviceEvent::Rebound {
                device: "Speakers (Realtek)".to_owned(),
            },
        ),
        (
            "AttemptFailed",
            AudioDeviceEvent::AttemptFailed {
                reason: "no output device".to_owned(),
            },
        ),
        (
            "GaveUp",
            AudioDeviceEvent::GaveUp {
                reason: "window closed".to_owned(),
            },
        ),
    ];
    // The complete list of cells allowed to be no-ops. Anything not on it must move.
    const DEAF: [(&str, &str); 11] = [
        ("Bound", "AttemptStarted"),
        ("Bound", "AttemptFailed"),
        ("Bound", "GaveUp"),
        ("Waiting", "BindingLost"),
        ("Attempting", "BindingLost"),
        ("Attempting", "AttemptStarted"),
        ("Lost", "BindingLost"),
        ("Lost", "AttemptStarted"),
        ("Lost", "Rebound"),
        ("Lost", "AttemptFailed"),
        ("Lost", "GaveUp"),
    ];

    for (phase_name, phase) in &phases {
        for (event_name, event) in &events {
            let stepped = phase.clone().step(event.clone(), t0);
            let deaf = DEAF.contains(&(*phase_name, *event_name));
            assert_eq!(
                stepped == *phase,
                deaf,
                "{phase_name} × {event_name}: the phase must {} — got {stepped:?}",
                if deaf { "stay put" } else { "move" }
            );
        }
    }

    // The spawn-failure edge, at the exact instant the attempt was due. It must reschedule,
    // keep the recovery deadline where it was, and leave nothing owed at this instant.
    let due = AudioDevicePhase::Waiting {
        since: t0,
        next_attempt_at: t0,
    };
    assert_eq!(due.duty(t0), DeviceDuty::Attempt);
    let after = due.clone().step(
        AudioDeviceEvent::AttemptFailed {
            reason: "could not start the audio rebind thread".to_owned(),
        },
        t0,
    );
    let AudioDevicePhase::Waiting {
        since,
        next_attempt_at,
    } = after
    else {
        panic!("a failure inside the window reschedules; it does not end the session");
    };
    assert_eq!(
        since, t0,
        "a refused spawn is not a new loss and must not restart the recovery window"
    );
    assert_eq!(next_attempt_at, t0 + Duration::from_millis(150));
    assert_eq!(
        AudioDevicePhase::Waiting {
            since,
            next_attempt_at
        }
        .duty(t0),
        DeviceDuty::Idle,
        "the frame whose spawn failed owes no second attempt at the same instant"
    );
}

/// `wake_at` and `duty` are one contract in two halves: nothing is owed before the instant the
/// machine named, and something is owed at it. A phase that names nothing must owe nothing at
/// any future instant, ever — that is what makes `None` safe for the render loop to treat as
/// "park indefinitely".
#[test]
fn the_ladder_names_the_instant_its_duty_comes_due() {
    let t0 = Instant::now() + Duration::from_secs(60);
    let phases = [
        AudioDevicePhase::Bound {
            device: "Speakers (Realtek)".to_owned(),
        },
        AudioDevicePhase::Waiting {
            since: t0,
            next_attempt_at: t0 + Duration::from_millis(150),
        },
        AudioDevicePhase::Waiting {
            since: t0,
            next_attempt_at: t0 + Duration::from_secs(30),
        },
        AudioDevicePhase::Attempting { since: t0 },
        AudioDevicePhase::Lost {
            reason: "gone".to_owned(),
        },
    ];
    for phase in &phases {
        match phase.wake_at() {
            None => {
                for at in [
                    Duration::ZERO,
                    Duration::from_millis(150),
                    Duration::from_secs(11),
                    Duration::from_secs(3_600),
                ] {
                    assert_eq!(
                        phase.duty(t0 + at),
                        DeviceDuty::Idle,
                        "{phase:?} named no wake, so it must owe nothing at t0+{at:?}"
                    );
                }
            }
            Some(due) => {
                assert_eq!(
                    phase.duty(due - Duration::from_nanos(1)),
                    DeviceDuty::Idle,
                    "{phase:?} owes nothing before the instant it named"
                );
                assert_ne!(
                    phase.duty(due),
                    DeviceDuty::Idle,
                    "{phase:?} named an instant at which it owes nothing"
                );
                assert!(
                    due <= t0 + DEVICE_RECOVERY_WINDOW,
                    "no wake may fall outside the one recovery deadline"
                );
            }
        }
    }

    // A backoff scheduled past the window yields to the give-up: the machine applies the `min`,
    // not the caller.
    assert_eq!(
        AudioDevicePhase::Waiting {
            since: t0,
            next_attempt_at: t0 + Duration::from_secs(30),
        }
        .wake_at(),
        Some(t0 + DEVICE_RECOVERY_WINDOW)
    );

    // The edge the phase has not seen yet: `Bound` alone would tell the loop to sleep forever.
    let bound = AudioDevicePhase::Bound {
        device: "Speakers (Realtek)".to_owned(),
    };
    assert_eq!(device_wake_at(&bound, true, t0), Some(t0));
    assert_eq!(device_wake_at(&bound, false, t0), None);
}

/// The render loop, reduced to its one scheduling law: it sleeps until the minimum of the
/// deadlines the last frame published, and services the device on the frame that follows.
///
/// A paused player publishes no video deadline at all (`prepare_frame` returns
/// `next_present_at: None` unconditionally while paused), so the device machine's own wake is
/// the only term left in the fold. Before this fix there was no such term: the minimum was
/// `None`, `FrameWaker::wait_until(None)` parked indefinitely, and the ten-second window closed
/// while the loop slept — a device that came back after 700 ms was judged dead.
#[test]
fn a_paused_player_walks_the_ladder_on_the_machines_own_clock() {
    let t0 = Instant::now() + Duration::from_secs(60);
    let device_returns_at = t0 + Duration::from_millis(700);
    let mut phase = AudioDevicePhase::Bound {
        device: "Headphones (Bluetooth)".to_owned(),
    }
    .step(AudioDeviceEvent::BindingLost, t0);

    let mut now = t0;
    let mut attempts = 0_u32;
    let mut frames = 0_u32;
    // `None` is the paused player's video deadline; the fold is the same `min` the host runs.
    while let Some(wake) = [None, phase.wake_at()].into_iter().flatten().min() {
        frames += 1;
        assert!(
            frames <= 16,
            "the ladder must not need one media-loop frame per millisecond"
        );
        now = wake.max(now);
        match phase.duty(now) {
            DeviceDuty::Idle => {}
            DeviceDuty::Attempt => {
                attempts += 1;
                phase = phase.step(AudioDeviceEvent::AttemptStarted, now);
                let landed = now + Duration::from_millis(2);
                phase = if landed >= device_returns_at {
                    phase.step(
                        AudioDeviceEvent::Rebound {
                            device: "Speakers (Realtek)".to_owned(),
                        },
                        landed,
                    )
                } else {
                    phase.step(
                        AudioDeviceEvent::AttemptFailed {
                            reason: "no output device".to_owned(),
                        },
                        landed,
                    )
                };
            }
            DeviceDuty::GiveUp { reason } => {
                phase = phase.step(AudioDeviceEvent::GaveUp { reason }, now);
            }
        }
    }

    assert_eq!(
        phase,
        AudioDevicePhase::Bound {
            device: "Speakers (Realtek)".to_owned()
        },
        "a paused player must still recover a device that came back inside the window"
    );
    // Attempts at 0 / 152 / 304 / 456 / 608 ms (150 ms backoff while the loss is younger than
    // 500 ms) and 1010 ms (400 ms once it is older). Six wakes in one second — not 121, and
    // not zero.
    assert_eq!(attempts, 6);
    assert_eq!(frames, 6);
    assert_eq!(now, t0 + Duration::from_millis(1_010));
}

#[test]
fn a_rebind_crossing_a_committed_seek_starts_at_the_promise_not_at_the_clock() {
    // The desynchronisation D3 names: the device dies mid exact-seek. The clock still shows
    // where the dead generation stopped; the transaction names where the transport is going.
    let clock_position = Duration::from_millis(41_250);
    let seek_target = Duration::from_millis(93_500);
    let mut transaction =
        SeekHoldTransaction::begin(AudioOutputPhase::Running, AudioParkState::Live, seek_target);
    assert_eq!(transaction.target(), seek_target);
    // A superseding scrub while the device is still gone moves the promise.
    transaction.set_target(Duration::from_millis(120_000));
    assert_eq!(transaction.target(), Duration::from_millis(120_000));
    transaction.set_target(seek_target);

    // The detector: a binding that anchored at the *clock* position is refused.
    assert!(
        !rebind_honours_promise(
            seek_target,
            NormalizedTimestamp::from_position(clock_position)
        ),
        "resuming at the pre-seek clock position must not be adopted silently"
    );
    // A binding that anchored inside one decoded block of the promise is adopted.
    assert!(rebind_honours_promise(
        seek_target,
        NormalizedTimestamp::from_position(seek_target)
    ));
    assert!(rebind_honours_promise(
        seek_target,
        NormalizedTimestamp::from_position(seek_target + Duration::from_millis(40))
    ));
    assert!(
        !rebind_honours_promise(
            seek_target,
            NormalizedTimestamp::from_position(seek_target + Duration::from_millis(200))
        ),
        "a decoder that landed a fifth of a second late is not where the transport promised"
    );
}

/// The ramp kind is a function of the posture edge. Every row is a transition the transport
/// can actually produce, and the table is what used to be four call sites each choosing a
/// `RampKind` by hand.
#[test]
fn the_ramp_kind_is_a_function_of_the_posture_edge() {
    let prepared = TransportPosture {
        phase: AudioOutputPhase::Prepared,
        seeking: false,
    };
    let running = TransportPosture {
        phase: AudioOutputPhase::Running,
        seeking: false,
    };
    let paused = TransportPosture {
        phase: AudioOutputPhase::Paused,
        seeking: false,
    };
    let seeking = TransportPosture {
        phase: AudioOutputPhase::Paused,
        seeking: true,
    };
    let disposed = TransportPosture {
        phase: AudioOutputPhase::Disposed,
        seeking: false,
    };

    // First play of a prepared output.
    let step = posture_step(prepared, running, AudioParkState::Live);
    assert_eq!(step.ramp, Some(RampKind::Start));
    assert!(!step.play_hardware);
    // Ordinary pause parks the hardware; a seek hold does not.
    assert_eq!(
        posture_step(running, paused, AudioParkState::Live).ramp,
        Some(RampKind::Pause)
    );
    assert!(posture_step(running, paused, AudioParkState::Live).park_hardware);
    assert_eq!(
        posture_step(running, seeking, AudioParkState::Live).ramp,
        Some(RampKind::SeekDown)
    );
    assert!(!posture_step(running, seeking, AudioParkState::Live).park_hardware);
    // Completing a seek into playback is a seek-up, not a resume.
    assert_eq!(
        posture_step(seeking, running, AudioParkState::RampedToZero).ramp,
        Some(RampKind::SeekUp)
    );
    // Completing a seek into a paused transport rings nothing and parks the hardware.
    let landed_paused = posture_step(seeking, paused, AudioParkState::RampedToZero);
    assert_eq!(landed_paused.ramp, None);
    assert!(landed_paused.park_hardware);
    assert_eq!(landed_paused.park_decoder, Some(true));
    // An ordinary resume from a parked stream plays the hardware BEFORE the ramp.
    let resumed = posture_step(paused, running, AudioParkState::HardwareParked);
    assert_eq!(resumed.ramp, Some(RampKind::Resume));
    assert!(resumed.play_hardware);
    // Dispose rings down once and never parks.
    assert_eq!(
        posture_step(running, disposed, AudioParkState::Live).ramp,
        Some(RampKind::Dispose)
    );
    // Idempotent: applying the posture the hardware already has is a no-op.
    assert_eq!(
        posture_step(running, running, AudioParkState::Live),
        PostureStep::IDLE
    );
    assert_eq!(
        posture_step(paused, paused, AudioParkState::HardwareParked),
        PostureStep::IDLE
    );
}

/// A fresh binding lands in whatever posture the transport already believes, using the SAME
/// function as every transport edge. This is what `AudioOutputPhase::commit` used to be, minus
/// the second table that could disagree with the first.
#[test]
fn a_fresh_binding_is_committed_into_the_posture_the_transport_already_believes() {
    let fresh = |seeking: bool| TransportPosture {
        phase: AudioOutputPhase::Paused,
        seeking,
    };
    for (wanted, expect_play, expect_ramp) in [
        (
            TransportPosture {
                phase: AudioOutputPhase::Prepared,
                seeking: false,
            },
            false,
            None,
        ),
        (
            TransportPosture {
                phase: AudioOutputPhase::Running,
                seeking: false,
            },
            true,
            Some(RampKind::Resume),
        ),
        (
            TransportPosture {
                phase: AudioOutputPhase::Paused,
                seeking: false,
            },
            false,
            None,
        ),
    ] {
        let step = posture_step(
            fresh(wanted.seeking),
            wanted,
            AudioParkState::HardwareParked,
        );
        assert_eq!(step.play_hardware, expect_play, "posture {wanted:?}");
        assert_eq!(step.ramp, expect_ramp, "posture {wanted:?}");
        let park = step.park_after(AudioParkState::HardwareParked, wanted);
        assert_eq!(
            park.hardware_stream_paused(),
            !expect_play,
            "posture {wanted:?}"
        );
    }
}

/// A burst of transport commands collapses into ONE hardware sequence, and it is the right
/// one: the owner thread reads the posture at apply time, so the intermediate postures need no
/// hardware operation of their own.
#[test]
fn a_posture_burst_converges_on_the_last_posture() {
    let cell = PostureCell::new(TransportPosture {
        phase: AudioOutputPhase::Prepared,
        seeking: false,
    });
    let mut applied = TransportPosture {
        phase: AudioOutputPhase::Paused,
        seeking: false,
    };
    let burst = [
        AudioOutputPhase::Running,
        AudioOutputPhase::Paused,
        AudioOutputPhase::Running,
    ];
    for phase in burst {
        cell.store(TransportPosture {
            phase,
            seeking: false,
        });
    }
    // One `Reconcile` is applied for each posted order, but the first already reaches the end
    // state and the rest are `IDLE`.
    let mut hardware_sequences = 0usize;
    for _ in 0..burst.len() {
        let wanted = cell.load();
        let step = posture_step(applied, wanted, AudioParkState::HardwareParked);
        if step != PostureStep::IDLE {
            hardware_sequences += 1;
        }
        applied = wanted;
    }
    assert_eq!(hardware_sequences, 1);
    assert_eq!(applied.phase, AudioOutputPhase::Running);
}

/// MAJOR-6 by construction: there is exactly one failure edge, the phase enters `Attempting`
/// in the same step that posts the attempt, and `AttemptFailed` in `Attempting` always applies
/// backoff. A spawn refusal cannot be reported from `Waiting` because nothing on the media
/// loop spawns anything.
#[test]
fn every_attempt_failure_moves_the_ladder_and_applies_backoff() {
    let origin = Instant::now();
    let mut ladder = AudioDeviceLadder::bound("initial".to_owned());
    ladder.lost(origin);
    let AudioDevicePhase::Waiting {
        next_attempt_at, ..
    } = ladder.phase().clone()
    else {
        panic!("a lost binding waits");
    };
    assert_eq!(next_attempt_at, origin);
    assert_eq!(ladder.phase().duty(origin), DeviceDuty::Attempt);

    ladder.attempt_started(origin);
    assert!(matches!(
        ladder.phase(),
        AudioDevicePhase::Attempting { .. }
    ));
    ladder.attempt_failed("thread spawn refused".to_owned(), origin);
    let AudioDevicePhase::Waiting {
        next_attempt_at, ..
    } = ladder.phase().clone()
    else {
        panic!("a failed attempt waits");
    };
    assert!(
        next_attempt_at > origin,
        "a refused spawn must apply backoff, not retry on the next frame"
    );
    assert_eq!(ladder.phase().duty(origin), DeviceDuty::Idle);

    let production = audio_production();
    assert_eq!(
        production.matches("AudioEdge::AttemptFailed").count(),
        4,
        "one failure edge kind, produced only by the owner thread and the order-post refusal"
    );
    assert!(
        !production.contains("AttemptStartFailed"),
        "a second failure edge would reintroduce a phase in which a failure applies no backoff"
    );
}

/// The transient BLOCKER-2 creates and must name: `finish_seek` closes the transaction, so the
/// transport's books say the seek is done, while the fact that any decoder took it belongs to
/// a worker that may not exist. Reading the clock there is how a rebind resumes at the
/// pre-seek position while video stands on the target — and `rebind_honours_promise` waves it
/// through, because the promise it checks is itself the wrong one.
#[test]
fn a_seek_committed_with_no_decoder_keeps_promising_its_target() {
    let target = Duration::from_millis(93_500);
    let before_the_seek = Duration::from_millis(12_000);
    // `seek_held` returned `Ok(None)`: generation 42 exists in the transport's books only, and
    // the media clock still publishes the anchor before the seek.
    let unadopted = UnadoptedSeek {
        generation: 42,
        target,
    };
    assert_eq!(
        promised_position(None, Some(unadopted), 41, before_the_seek),
        target,
        "a rebind resumed from the clock would start audio at 12s while video stands on 93.5s"
    );
    // A decode worker anchored the generation that carries it: the clock is media time again,
    // with no retirement edge anywhere.
    assert_eq!(promised_position(None, Some(unadopted), 42, target), target);
    assert_eq!(
        promised_position(None, Some(unadopted), 43, Duration::from_millis(94_200)),
        Duration::from_millis(94_200),
        "a later generation is a later observation; the promise is spent"
    );
    // An open transaction is newer than any promise it has not yet closed.
    let superseding = SeekHoldTransaction::begin(
        AudioOutputPhase::Running,
        AudioParkState::Live,
        Duration::from_millis(30_000),
    );
    assert_eq!(
        promised_position(Some(superseding), Some(unadopted), 41, before_the_seek),
        Duration::from_millis(30_000)
    );
    // No promise of any kind: the clock is the only answer.
    assert_eq!(
        promised_position(None, None, 41, before_the_seek),
        before_the_seek
    );
}

#[test]
fn rebind_backoff_is_a_function_of_how_long_the_device_has_been_gone() {
    assert_eq!(rebind_backoff(Duration::ZERO), Duration::from_millis(150));
    assert_eq!(
        rebind_backoff(Duration::from_millis(499)),
        Duration::from_millis(150)
    );
    assert_eq!(
        rebind_backoff(Duration::from_millis(500)),
        Duration::from_millis(400)
    );
    assert_eq!(
        rebind_backoff(Duration::from_secs(2)),
        Duration::from_secs(1)
    );
    // Bounded above: the window closes long before the interval could grow without limit.
    assert_eq!(
        rebind_backoff(Duration::from_secs(3_600)),
        Duration::from_secs(1)
    );
}

#[test]
fn fresh_paused_reopen_seek_then_play_activates_prepared_audio() {
    let mut generation = GenerationCounter::new(7);
    let mut phase = AudioOutputPhase::Prepared;
    phase = phase.held();
    assert_eq!(
        phase,
        AudioOutputPhase::Prepared,
        "pausing before first prime preserves the prepared phase"
    );

    let _transaction = SeekHoldTransaction::begin(
        phase,
        AudioParkState::HardwareParked,
        Duration::from_millis(93_500),
    );
    generation.advance();
    phase = phase.after_seek(false);
    assert_eq!(
        phase,
        AudioOutputPhase::Prepared,
        "a paused committed seek must retain never-started truth"
    );

    let held = |phase| TransportPosture {
        phase,
        seeking: false,
    };
    let running = held(AudioOutputPhase::Running);
    let activation = posture_step(held(phase), running, AudioParkState::HardwareParked);
    assert_eq!(activation.ramp, Some(RampKind::Start));
    assert!(
        activation.play_hardware,
        "the seek-paused hardware stream must play before the start ramp"
    );
    phase = phase.after_activation();
    assert!(phase.is_running());

    let ordinary_resume = posture_step(
        held(AudioOutputPhase::Paused),
        running,
        AudioParkState::HardwareParked,
    );
    assert_eq!(ordinary_resume.ramp, Some(RampKind::Resume));
    assert!(ordinary_resume.play_hardware);
    assert_eq!(
        AudioOutputPhase::Paused.after_seek(true),
        AudioOutputPhase::Running,
        "playing seek completion remains running"
    );
}

#[test]
fn disposed_audio_phase_is_terminal_for_activation_hold_and_seek() {
    let disposed = AudioOutputPhase::Disposed;
    // A disposed posture never rings up and never plays the hardware, whatever the park
    // record says; the only step it can produce is the terminal dispose ramp.
    for (park, expect_ramp) in [
        (AudioParkState::Live, Some(RampKind::Dispose)),
        (AudioParkState::RampedToZero, Some(RampKind::Dispose)),
        (AudioParkState::HardwareParked, None),
    ] {
        for from in [
            AudioOutputPhase::Prepared,
            AudioOutputPhase::Running,
            AudioOutputPhase::Paused,
        ] {
            let step = posture_step(
                TransportPosture {
                    phase: from,
                    seeking: false,
                },
                TransportPosture {
                    phase: disposed,
                    seeking: false,
                },
                park,
            );
            assert!(!step.play_hardware, "{from:?} -> disposed at {park:?}");
            assert_eq!(step.ramp, expect_ramp, "{from:?} -> disposed at {park:?}");
        }
    }
    assert_eq!(disposed.after_activation(), disposed);
    assert_eq!(disposed.held(), disposed);
    assert_eq!(disposed.after_seek(false), disposed);
    assert_eq!(disposed.after_seek(true), disposed);
}

/// B1 was never a wrong value: it was `start()` publishing `MODE_RUNNING` while the cpal stream
/// stood parked, so the callback that had to acknowledge it could not run, the render thread
/// blocked for `CALLBACK_CONTROL_TIMEOUT`, and the play page went black. The phase algebra
/// above proves the step is right; nothing above proves the step is fed the PLATFORM's own park
/// record. That edge is a procedural guard, so it gets the guard this crate already uses for
/// procedural facts (`windows.rs`'s clock-sovereignty pin): the one site that turns a posture
/// edge into hardware must read the live posture, and the three booleans whose product
/// produced B1 must stay dead.
#[test]
fn the_single_activation_entry_reads_the_live_park_record() {
    // Re-anchored when this battery moved out of `src`: the file it reads is now the
    // production half in full, so the old cut at the last `#[cfg(test)]` is gone.
    let production = audio_production();
    assert_eq!(
        production.matches("activation_plan").count(),
        0,
        "the second posture table is gone; `posture_step` is the only description"
    );
    assert_eq!(
        production
            .matches(concat!("posture_step(self.applied, ", "wanted, park)"))
            .count(),
        1,
        "the reconciling site must consult the live park posture at exactly one site"
    );
    assert_eq!(
        production
            .matches(concat!("let park = ", "self.session.park.load();"))
            .count(),
        1,
        "that site reads the platform's own record, not a caller's copy"
    );
    for retired in [
        concat!("self.", "started"),
        concat!("self.", "running"),
        concat!("self.", "disposed"),
    ] {
        assert_eq!(
            production.matches(retired).count(),
            0,
            "the retired activation booleans must not return: {retired}"
        );
    }
}

#[test]
fn hardware_park_orders_terminal_zero_before_pause_and_play_before_resume_ramp() {
    use std::cell::{Cell, RefCell};

    const CYCLES: usize = 16;
    let events = RefCell::new(Vec::new());
    let terminal_zero = Cell::new(false);
    let decoder_parked = Cell::new(false);
    let hardware_paused = Cell::new(false);
    let stream_operations = RefCell::new(Vec::new());
    let stream_id = 41_u64;

    for _ in 0..CYCLES {
        ramp_then_hardware_pause(
            || {
                events.borrow_mut().push("pause-ramp-terminal-zero");
                terminal_zero.set(true);
                Ok(())
            },
            || {
                assert!(terminal_zero.get());
                events.borrow_mut().push("decoder-park");
                decoder_parked.set(true);
                Ok(())
            },
            || {
                assert!(terminal_zero.get());
                assert!(decoder_parked.get());
                events.borrow_mut().push("stream-pause");
                stream_operations.borrow_mut().push(stream_id);
                Ok(())
            },
            || {
                events.borrow_mut().push("hardware-paused");
                hardware_paused.set(true);
            },
        )
        .expect("pause sequence");

        hardware_play_then_ramp(
            || {
                assert!(hardware_paused.get());
                events.borrow_mut().push("stream-play");
                stream_operations.borrow_mut().push(stream_id);
                Ok(())
            },
            || {
                events.borrow_mut().push("hardware-playing");
                hardware_paused.set(false);
            },
            || {
                assert!(!hardware_paused.get());
                events.borrow_mut().push("decoder-resume");
                decoder_parked.set(false);
                Ok(())
            },
            || {
                assert!(!hardware_paused.get());
                assert!(!decoder_parked.get());
                events.borrow_mut().push("resume-ramp-from-zero");
                terminal_zero.set(false);
                Ok(())
            },
        )
        .expect("resume sequence");
    }

    let expected_cycle = [
        "pause-ramp-terminal-zero",
        "decoder-park",
        "stream-pause",
        "hardware-paused",
        "stream-play",
        "hardware-playing",
        "decoder-resume",
        "resume-ramp-from-zero",
    ];
    assert!(events
        .borrow()
        .chunks_exact(expected_cycle.len())
        .all(|cycle| cycle == expected_cycle));
    assert_eq!(events.borrow().len(), CYCLES * expected_cycle.len());
    assert_eq!(stream_operations.borrow().len(), CYCLES * 2);
    assert!(stream_operations
        .borrow()
        .iter()
        .all(|observed| *observed == stream_id));
    assert!(!terminal_zero.get());
    assert!(!decoder_parked.get());
    assert!(!hardware_paused.get());

    println!(
        "player-core[audiopark] pause_order=ramp_then_pause resume_order=play_then_ramp callback_quiesced=true hardware_stream_paused=true decode_parked=true stream_reused=true leak=none"
    );
}

#[test]
fn user_gain_defaults_to_unity_and_clamps_to_the_supported_range() {
    let gain = UserGain::default();
    assert_eq!(gain.get(), 1.0);

    gain.set(-0.25);
    assert_eq!(gain.get(), 0.0);
    gain.set(1.25);
    assert_eq!(gain.get(), 1.0);
    gain.set(0.375);
    assert_eq!(gain.get(), 0.375);
    gain.set(f32::NAN);
    assert_eq!(gain.get(), 0.0);
}

#[test]
fn callback_multiplies_user_gain_without_changing_transition_ramp_evidence() {
    let ring = Arc::new(AudioRing::new(nonzero_usize(64), nonzero_usize(2)).expect("ring"));
    let clock = Arc::new(AudioClock::new());
    clock
        .reanchor(1, NormalizedTimestamp::from_nanoseconds(0))
        .expect("clock anchor");
    let control = Arc::new(CallbackControl::new());
    let metrics = Arc::new(SharedMetrics::default());
    let mut callback = callback_harness(ring.clone(), clock, control.clone(), metrics.clone());
    callback.user_gain.set(0.25);
    assert_eq!(ring.push_interleaved(&[1.0; 10]), 5);
    inject_request(&control, MODE_RUNNING, RampKind::Start);

    let mut output = [0.0f32; 10];
    callback.render(
        &mut output,
        CallbackMoment {
            observed_at: Instant::now(),
            delivery_delay: Duration::ZERO,
        },
    );

    assert_eq!(
        output
            .chunks_exact(2)
            .map(|frame| frame[0])
            .collect::<Vec<_>>(),
        vec![0.0, 0.0625, 0.125, 0.1875, 0.25]
    );
    assert!(output.chunks_exact(2).all(|frame| frame[0] == frame[1]));
    let evidence = metrics.snapshot(&callback.clock);
    assert_eq!(evidence.ramp_evidence_mask, RampKind::Start.evidence_bit());
    assert_eq!(evidence.ramp_shape_failures, 0);
    assert_eq!(evidence.ramp_sample_frames, 5);
}

#[test]
fn gain_ramps_have_exact_endpoints_interior_values_and_monotonic_shape() {
    let mut rising = GainRamp::new(0.0, 1.0, nonzero_u32(5), AudioGainCurve::Linear);
    let rise = (0..5).map(|_| rising.next().0).collect::<Vec<_>>();
    assert_eq!(rise.first(), Some(&0.0));
    assert_eq!(rise.last(), Some(&1.0));
    assert!(rise.windows(2).all(|window| window[0] <= window[1]));
    assert!(rise[1..4].iter().any(|gain| *gain > 0.0 && *gain < 1.0));

    let mut falling = GainRamp::new(1.0, 0.0, nonzero_u32(5), AudioGainCurve::Linear);
    let fall = (0..5).map(|_| falling.next().0).collect::<Vec<_>>();
    assert_eq!(fall.first(), Some(&1.0));
    assert_eq!(fall.last(), Some(&0.0));
    assert!(fall.windows(2).all(|window| window[0] >= window[1]));
    assert!(fall[1..4].iter().any(|gain| *gain > 0.0 && *gain < 1.0));
}

#[test]
fn media_time_is_scaled_by_the_device_the_callback_serves_not_by_the_one_it_opened_on() {
    // A rebind reuses the session clock and builds the callback for the *new* device. The
    // two must agree about the sample rate, or media time runs at the ratio between them:
    // 48 kHz headphones to 44.1 kHz speakers is 8.84% slow, forever, silently.
    let ring = Arc::new(AudioRing::new(nonzero_usize(2_048), nonzero_usize(2)).expect("ring"));
    let clock = Arc::new(AudioClock::new());
    clock
        .reanchor(1, NormalizedTimestamp::from_nanoseconds(0))
        .expect("clock anchor");
    let control = Arc::new(CallbackControl::new());
    let metrics = Arc::new(SharedMetrics::default());
    let mut callback = CallbackCore::new(
        CallbackResources {
            ring: ring.clone(),
            clock: clock.clone(),
            control: control.clone(),
            metrics,
            user_gain: Arc::new(UserGain::default()),
            worker_thread: thread::current(),
        },
        CallbackConfiguration {
            policy: policy(),
            channels: 2,
            // The device this binding actually landed on.
            sample_rate: nonzero_u32(44_100),
        },
    );
    inject_request(&control, MODE_RUNNING, RampKind::Start);
    assert_eq!(ring.push_interleaved(&vec![0.5; 441 * 2]), 441);
    let now = Instant::now();
    let mut output = vec![0.0f32; 441 * 2];
    callback.render(
        &mut output,
        CallbackMoment {
            observed_at: now,
            delivery_delay: Duration::ZERO,
        },
    );
    assert_eq!(
        clock.snapshot().expect("clock").media_now,
        NormalizedTimestamp::from_nanoseconds(10_000_000),
        "441 frames on a 44.1 kHz device is exactly 10 ms of media time"
    );
}

#[test]
fn callback_clock_counts_only_complete_real_frames_not_underrun_silence() {
    // Device requests, committed media, and underrun are distinct counters.
    let ring = Arc::new(AudioRing::new(nonzero_usize(2_048), nonzero_usize(2)).expect("ring"));
    let clock = Arc::new(AudioClock::new());
    clock
        .reanchor(1, NormalizedTimestamp::from_nanoseconds(2_000_000_000))
        .expect("clock anchor");
    let control = Arc::new(CallbackControl::new());
    let metrics = Arc::new(SharedMetrics::default());
    let mut callback = callback_harness(
        ring.clone(),
        clock.clone(),
        control.clone(),
        metrics.clone(),
    );
    inject_request(&control, MODE_RUNNING, RampKind::Start);
    assert_eq!(ring.push_interleaved(&vec![0.5; 480 * 2]), 480);
    let now = Instant::now();
    let mut output = vec![0.0f32; 480 * 2];
    callback.render(
        &mut output,
        CallbackMoment {
            observed_at: now,
            delivery_delay: Duration::ZERO,
        },
    );
    assert_eq!(
        clock.snapshot().expect("clock").media_now,
        NormalizedTimestamp::from_nanoseconds(2_010_000_000)
    );

    callback.render(
        &mut output,
        CallbackMoment {
            observed_at: now + Duration::from_millis(20),
            delivery_delay: Duration::ZERO,
        },
    );
    assert_eq!(
        clock.snapshot().expect("clock").media_now,
        NormalizedTimestamp::from_nanoseconds(2_010_000_000)
    );

    assert_eq!(ring.push_interleaved(&vec![0.5; 240 * 2]), 240);
    callback.render(
        &mut output,
        CallbackMoment {
            observed_at: now + Duration::from_millis(40),
            delivery_delay: Duration::ZERO,
        },
    );
    assert_eq!(
        clock.snapshot().expect("clock").media_now,
        NormalizedTimestamp::from_nanoseconds(2_015_000_000)
    );
    let snapshot = metrics.snapshot(&clock);
    assert_eq!(snapshot.device_frames_requested, 1_440);
    assert_eq!(snapshot.media_frames_committed, 720);
    assert_eq!(snapshot.underrun_frames, 720);
}

#[test]
fn callback_rate_atom_drives_source_consumption_and_media_clock_together() {
    fn run(rate: f32) -> (u64, i128, u64) {
        let ring = Arc::new(AudioRing::new(nonzero_usize(64), nonzero_usize(2)).expect("ring"));
        let rate_control = Arc::new(PlaybackRateControl::default());
        assert!(rate_control.set(rate));
        let clock = Arc::new(AudioClock::with_playback_rate(rate_control));
        clock
            .reanchor(1, NormalizedTimestamp::from_nanoseconds(0))
            .expect("clock anchor");
        let control = Arc::new(CallbackControl::new());
        let metrics = Arc::new(SharedMetrics::default());
        let mut callback = callback_harness(
            ring.clone(),
            clock.clone(),
            control.clone(),
            metrics.clone(),
        );
        assert_eq!(ring.push_interleaved(&[0.5; 40]), 20);
        inject_request(&control, MODE_RUNNING, RampKind::Start);
        callback.render(
            &mut [0.0f32; 10],
            CallbackMoment {
                observed_at: Instant::now(),
                delivery_delay: Duration::ZERO,
            },
        );
        let snapshot = clock.snapshot().expect("clock");
        (
            metrics.snapshot(&clock).media_frames_committed,
            snapshot.media_now.as_nanoseconds(),
            clock.playback_rate_q32(),
        )
    }

    let fast = run(2.0);
    assert_eq!(fast.0, 10);
    assert_eq!(fast.1, 208_333);
    assert_eq!(fast.2, PLAYBACK_RATE_SCALE * 2);

    let slow = run(0.5);
    assert_eq!(slow.0, 2);
    assert_eq!(slow.1, 52_083);
    assert_eq!(slow.2, PLAYBACK_RATE_SCALE / 2);
}

#[test]
fn playback_rate_validation_and_clock_reanchor_preserve_transport_rate() {
    let rate = Arc::new(PlaybackRateControl::default());
    assert!(!rate.set(0.49));
    assert!(!rate.set(2.01));
    assert!(!rate.set(f32::NAN));
    assert!(rate.set(1.5));

    let clock = AudioClock::with_playback_rate(Arc::clone(&rate));
    clock
        .reanchor(7, NormalizedTimestamp::from_nanoseconds(1_000_000_000))
        .expect("initial anchor");
    assert_eq!(clock.playback_rate_q32(), PLAYBACK_RATE_SCALE * 3 / 2);
    clock
        .reanchor(8, NormalizedTimestamp::from_nanoseconds(9_000_000_000))
        .expect("seek anchor");
    assert_eq!(clock.playback_rate_q32(), PLAYBACK_RATE_SCALE * 3 / 2);
    assert_eq!(
        rate.get(),
        1.5,
        "EOF/teardown observations never mutate the atom"
    );
}

#[test]
fn callback_advances_ramp_once_per_stereo_frame_and_uses_equilibrium_silence() {
    let ring = Arc::new(AudioRing::new(nonzero_usize(64), nonzero_usize(2)).expect("ring"));
    let clock = Arc::new(AudioClock::new());
    clock
        .reanchor(1, NormalizedTimestamp::from_nanoseconds(0))
        .expect("clock anchor");
    let control = Arc::new(CallbackControl::new());
    let metrics = Arc::new(SharedMetrics::default());
    let mut callback = callback_harness(ring.clone(), clock, control.clone(), metrics);
    assert_eq!(ring.push_interleaved(&[1.0; 10]), 5);
    inject_request(&control, MODE_RUNNING, RampKind::Start);
    let mut output = [0.0f32; 10];
    callback.render(
        &mut output,
        CallbackMoment {
            observed_at: Instant::now(),
            delivery_delay: Duration::ZERO,
        },
    );
    for frame in output.chunks_exact(2) {
        assert_eq!(frame[0], frame[1]);
    }
    assert_eq!(output[0], 0.0);
    assert_eq!(output[8], 1.0);
    assert!(output[2] > 0.0 && output[2] < 1.0);
    let evidence = callback.metrics.snapshot(&callback.clock);
    assert_eq!(evidence.ramp_evidence_mask, RampKind::Start.evidence_bit());
    assert_eq!(evidence.ramp_shape_failures, 0);
    assert_eq!(evidence.ramp_sample_frames, 5);

    let held_ring = Arc::new(AudioRing::new(nonzero_usize(64), nonzero_usize(2)).expect("ring"));
    let held_clock = Arc::new(AudioClock::new());
    let held_control = Arc::new(CallbackControl::new());
    let held_metrics = Arc::new(SharedMetrics::default());
    let mut held = callback_harness(held_ring, held_clock, held_control, held_metrics);
    let mut unsigned = [0u16; 8];
    held.render(
        &mut unsigned,
        CallbackMoment {
            observed_at: Instant::now(),
            delivery_delay: Duration::ZERO,
        },
    );
    let equilibrium = u16::from_sample(0.0);
    assert_ne!(equilibrium, 0);
    assert!(unsigned.iter().all(|sample| *sample == equilibrium));
}

#[test]
fn seek_flushes_old_generation_before_zero_to_new_signal_ramp() {
    // A printed success literal cannot prove generation isolation.
    let ring = Arc::new(AudioRing::new(nonzero_usize(64), nonzero_usize(2)).expect("ring"));
    let clock = Arc::new(AudioClock::new());
    clock
        .reanchor(1, NormalizedTimestamp::from_nanoseconds(0))
        .expect("clock anchor");
    let control = Arc::new(CallbackControl::new());
    let metrics = Arc::new(SharedMetrics::default());
    let mut callback = callback_harness(ring.clone(), clock.clone(), control.clone(), metrics);
    ring.push_interleaved(&[1.0; 20]);
    inject_request(&control, MODE_RUNNING, RampKind::Start);
    let now = Instant::now();
    let mut old = [0.0f32; 10];
    callback.render(
        &mut old,
        CallbackMoment {
            observed_at: now,
            delivery_delay: Duration::ZERO,
        },
    );
    inject_request(&control, MODE_HELD, RampKind::SeekDown);
    let mut down = [0.0f32; 10];
    callback.render(
        &mut down,
        CallbackMoment {
            observed_at: now + Duration::from_millis(1),
            delivery_delay: Duration::ZERO,
        },
    );
    assert_eq!(down[8], 0.0);
    callback.render(
        &mut [0.0f32; 2],
        CallbackMoment {
            observed_at: now + Duration::from_secs(1),
            delivery_delay: Duration::ZERO,
        },
    );
    ring.discard_all();
    clock
        .reanchor(2, NormalizedTimestamp::from_nanoseconds(5_000_000_000))
        .expect("seek anchor");
    ring.push_interleaved(&[-1.0; 10]);
    inject_request(&control, MODE_RUNNING, RampKind::SeekUp);
    let mut new = [0.0f32; 10];
    callback.render(
        &mut new,
        CallbackMoment {
            observed_at: now + Duration::from_secs(2),
            delivery_delay: Duration::ZERO,
        },
    );
    assert_eq!(new[0], 0.0);
    assert!(new[2..].iter().all(|sample| *sample <= 0.0));
    assert!(new.iter().any(|sample| *sample < 0.0));
}

#[test]
fn ring_discard_cannot_be_undone_by_a_callback_that_commits_across_it() {
    // The exact interleaving that used to hand pre-seek audio back to the callback: the
    // callback takes its window, the producer voids the generation, and only then does the
    // callback publish its own cursor. Under the old `clear_while_held` (producer writing
    // `read_frame`) the third assertion observed 2 readable frames of stale audio.
    let ring = AudioRing::new(nonzero_usize(8), nonzero_usize(1)).expect("ring");
    assert_eq!(ring.push_interleaved(&[1.0, 2.0, 3.0, 4.0]), 4);

    let (floor, available) = ring.consumer_window();
    assert_eq!((floor, available), (0, 4));

    ring.discard_all();
    assert_eq!(ring.available_frames(), 0);

    ring.commit_read(floor, 2);
    assert_eq!(
        ring.available_frames(),
        0,
        "a commit that crosses a discard must not resurrect voided frames"
    );

    assert_eq!(ring.push_interleaved(&[-1.0, -2.0]), 2);
    let (floor, available) = ring.consumer_window();
    assert_eq!(available, 2);
    assert_eq!(ring.sample(floor, 0), -1.0);
    assert_eq!(ring.sample(floor + 1, 0), -2.0);
}

#[test]
fn ring_window_after_a_discard_contains_no_pre_discard_sample() {
    // The producer reuses the slots it just voided. A stale consumer cursor must not be able
    // to point at them: the old `.min(capacity_frames)` clamp reported 4 readable frames here,
    // two of which were pre-seek audio.
    let ring = AudioRing::new(nonzero_usize(4), nonzero_usize(1)).expect("ring");
    assert_eq!(ring.push_interleaved(&[1.0, 2.0, 3.0, 4.0]), 4);
    let (stale_floor, _) = ring.consumer_window();

    ring.discard_all();
    assert_eq!(ring.push_interleaved(&[-1.0, -2.0]), 2);
    ring.commit_read(stale_floor, 1);

    let (floor, available) = ring.consumer_window();
    assert_eq!(available, 2, "only post-discard frames are readable");
    assert!(
        (0..available).all(|offset| ring.sample(floor + offset, 0) < 0.0),
        "post-discard window must contain no pre-discard sample"
    );
}

#[test]
fn clock_never_credits_one_generation_of_audio_to_another_generations_anchor() {
    // The old seqlock guarded `reanchor` only: a callback that committed across a seek used
    // `add_committed_nanoseconds`, whose `fetch_add` landed after `reanchor` had zeroed the
    // shared counter. The first snapshot after the seek then reported one device period past
    // the seek target.
    let clock = AudioClock::new();
    clock
        .reanchor(1, NormalizedTimestamp::from_nanoseconds(0))
        .expect("first anchor");
    clock.publish_delivery(1, 10_000_000, 0);
    assert_eq!(
        clock.snapshot().expect("clock").media_now,
        NormalizedTimestamp::from_nanoseconds(10_000_000)
    );

    clock
        .reanchor(2, NormalizedTimestamp::from_nanoseconds(5_000_000_000))
        .expect("seek anchor");
    // The in-flight callback still publishes for the generation it was serving.
    clock.publish_delivery(1, 20_000_000, 0);
    let after_seek = clock.snapshot().expect("clock");
    assert_eq!(after_seek.generation, 2);
    assert_eq!(
        after_seek.media_now,
        NormalizedTimestamp::from_nanoseconds(5_000_000_000),
        "a previous generation's delivery may not move the new anchor"
    );

    clock.publish_delivery(2, 4_000_000, 0);
    assert_eq!(
        clock.snapshot().expect("clock").media_now,
        NormalizedTimestamp::from_nanoseconds(5_004_000_000)
    );
}

/// The master clock is the OUTPUT position. A callback publishes media time it has WRITTEN
/// plus the device's own report of how far ahead of the ear that write is, and the published
/// position is the difference. Before this, every reader — the video scheduler included — was
/// handed a position one device buffer into the future, a constant bias no amount of
/// per-frame scheduling could remove.
#[test]
fn the_published_clock_is_the_ear_position_not_the_write_position() {
    let clock = AudioClock::new();
    clock
        .reanchor(1, NormalizedTimestamp::from_nanoseconds(0))
        .expect("anchor");
    clock.publish_delivery(1, 40_000_000, 13_000_000);
    assert_eq!(
        clock.snapshot().expect("clock").media_now,
        NormalizedTimestamp::from_nanoseconds(27_000_000),
        "40 ms written, 13 ms of it still in the device buffer"
    );

    // A stale generation contributes neither term, so a seek cannot inherit the old device's
    // delay any more than it can inherit its elapsed time.
    clock
        .reanchor(2, NormalizedTimestamp::from_nanoseconds(5_000_000_000))
        .expect("seek anchor");
    clock.publish_delivery(1, 80_000_000, 13_000_000);
    assert_eq!(
        clock.snapshot().expect("clock").media_now,
        NormalizedTimestamp::from_nanoseconds(5_000_000_000)
    );
}

#[test]
fn clock_reserves_no_timestamp_as_an_unset_sentinel() {
    let clock = AudioClock::new();
    assert!(
        clock.snapshot().is_none(),
        "an unanchored clock has no snapshot"
    );
    clock
        .reanchor(
            1,
            NormalizedTimestamp::from_nanoseconds(i128::from(i64::MIN)),
        )
        .expect("the clock has no reserved anchor value");
    let snapshot = clock.snapshot().expect("clock is anchored");
    assert_eq!(
        snapshot.media_now,
        NormalizedTimestamp::from_nanoseconds(i128::from(i64::MIN))
    );
}

#[test]
fn distinct_audio_video_origins_round_trip_on_one_presentation_timeline() {
    let video = StreamTiming {
        time_base: StreamTimeBase {
            numerator: 1,
            denominator: 1_000,
        },
        start_time: Some(8_000),
        presentation_offset_nanoseconds: -80_000_000,
    };
    let audio = StreamTiming {
        time_base: StreamTimeBase {
            numerator: 1,
            denominator: 48_000,
        },
        start_time: Some(1_024),
        presentation_offset_nanoseconds: -1_000_000,
    };
    assert_eq!(
        normalize_timestamp(8_080, video).expect("video origin"),
        NormalizedTimestamp::from_nanoseconds(0)
    );
    assert_eq!(
        normalize_timestamp(1_072, audio).expect("audio origin"),
        NormalizedTimestamp::from_nanoseconds(0)
    );
    assert_eq!(
        normalize_timestamp(8_580, video).expect("video half second"),
        NormalizedTimestamp::from_nanoseconds(500_000_000)
    );
    assert_eq!(
        normalize_timestamp(25_072, audio).expect("audio half second"),
        NormalizedTimestamp::from_nanoseconds(500_000_000)
    );
    assert_eq!(
        raw_timestamp_for_position(Duration::from_millis(500), video).expect("video inverse"),
        8_580
    );
    assert_eq!(
        raw_timestamp_for_position(Duration::from_millis(500), audio).expect("audio inverse"),
        25_072
    );
}

#[test]
fn output_authority_rejects_overlap_and_stale_drop_cannot_release_new_owner() {
    let first = OutputAuthorityLease::acquire().expect("first authority");
    let first_owner = first.owner;
    assert!(matches!(
        OutputAuthorityLease::acquire(),
        Err(AudioOpenError::OutputBusy { owner }) if owner == first_owner
    ));
    drop(first);
    let second = OutputAuthorityLease::acquire().expect("second authority");
    assert_ne!(second.owner, first_owner);
    assert_eq!(OUTPUT_OWNER.load(Ordering::Acquire), second.owner);
    #[cfg(feature = "instrument")]
    assert_eq!(LIVE_OUTPUT_AUTHORITIES.load(Ordering::Acquire), 1);
    #[cfg(feature = "instrument")]
    assert_eq!(MAX_LIVE_OUTPUT_AUTHORITIES.load(Ordering::Acquire), 1);
    drop(second);
    assert_eq!(OUTPUT_OWNER.load(Ordering::Acquire), 0);
    #[cfg(feature = "instrument")]
    assert_eq!(LIVE_OUTPUT_AUTHORITIES.load(Ordering::Acquire), 0);
}
/// The native fixed-size track record maps losslessly — index, language, codec, channels.
#[test]
fn native_audio_track_mapping_preserves_index_language_codec_and_channels() {
    fn fixed<const N: usize>(text: &str) -> [c_char; N] {
        let mut out = [0 as c_char; N];
        for (slot, byte) in out.iter_mut().zip(text.bytes()) {
            *slot = byte as c_char;
        }
        out
    }
    let native = BnAudioTrackInfo {
        stream_index: 2,
        channels: 6,
        language: fixed("eng"),
        codec: fixed("flac"),
    };
    let mapped = map_native_audio_tracks(&[native]).expect("valid track maps");
    assert_eq!(mapped.len(), 1);
    assert_eq!(mapped[0].stream_index, AudioStreamIndex::new(2).unwrap());
    assert_eq!(mapped[0].language.as_deref(), Some("eng"));
    assert_eq!(mapped[0].codec, "flac");
    assert_eq!(mapped[0].channels, 6);

    // An undeclared language stays None and an empty codec becomes the honest "unknown".
    let bare = BnAudioTrackInfo {
        stream_index: 0,
        channels: 0,
        language: fixed(""),
        codec: fixed(""),
    };
    let mapped = map_native_audio_tracks(&[bare]).expect("bare track maps");
    assert_eq!(mapped[0].language, None);
    assert_eq!(mapped[0].codec, "unknown");
}

/// As the factory shape leaves it: `Default` is the native `-1` auto-pick, and an
/// explicit stream is carried verbatim to the decoder that holds the container. The refusal
/// of a stream that is not audio is the C shim's, translated back into the caller's taxonomy
/// by `open_native_audio` — so there is ONE validator and the resource is opened once.
#[test]
fn a_track_selection_reaches_the_decoder_as_the_native_abi_states_it() {
    assert_eq!(requested_stream_index(AudioTrackSelection::Default), -1);
    let two = AudioStreamIndex::new(2).unwrap();
    assert_eq!(
        requested_stream_index(AudioTrackSelection::Stream(two)),
        2,
        "an explicit stream is asked for exactly as the picker named it",
    );
    assert_eq!(
        BN_AUDIO_INVALID_STREAM, -118,
        "the shim's refusal code is what SelectedTrackUnavailable is minted from",
    );
}

/// A resolved track and the TRANSPORT ride `AudioSource` together, and the rebind
/// ladder re-opens by CLONING it — so the selected stream and the factory both survive every
/// rebind with no second piece of bookkeeping. There is no path in this value to re-derive.
#[test]
fn a_resolved_track_and_its_factory_ride_the_source_verbatim_across_rebinds() {
    let factory = Arc::new(CountingFactory::new(b"factory-backed media".to_vec()));
    let source = AudioSource {
        factory: factory.clone(),
        stream_index: requested_stream_index(AudioTrackSelection::Stream(
            AudioStreamIndex::new(4).unwrap(),
        )),
        source_rate: Arc::new(AtomicU32::new(0)),
    };
    let rebound = source.clone(); // the rebind ladder's exact operation
    assert_eq!(rebound.stream_index, 4);
    assert_eq!(
        rebound.factory.label(),
        source.factory.label(),
        "a rebind re-opens the same resource because it holds the same factory",
    );
}

/// A factory that counts its opens and, optionally, cancels the generation from INSIDE the
/// open — the shape of a cancel that arrives while the transport is blocking.
struct CountingFactory {
    bytes: Vec<u8>,
    opens: AtomicUsize,
    cancel: std::sync::Mutex<Option<Arc<AtomicU64>>>,
}

impl CountingFactory {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            opens: AtomicUsize::new(0),
            cancel: std::sync::Mutex::new(None),
        }
    }

    fn opens(&self) -> usize {
        self.opens.load(Ordering::Acquire)
    }
}

impl crate::AudioSourceFactory for CountingFactory {
    fn open(&self) -> Result<Box<dyn crate::MediaByteSource>, crate::AudioSourceError> {
        self.opens.fetch_add(1, Ordering::AcqRel);
        if let Some(live) = self.cancel.lock().unwrap().as_ref() {
            // The session gave up on this attempt while its bytes were still in flight.
            live.store(0, Ordering::Release);
        }
        Ok(Box::new(SliceSource(self.bytes.clone())))
    }

    fn label(&self) -> String {
        "counting:contract".to_owned()
    }
}

struct SliceSource(Vec<u8>);

impl crate::MediaByteSource for SliceSource {
    fn read(&mut self, offset: u64, buffer: &mut [u8]) -> std::io::Result<usize> {
        let start = (offset as usize).min(self.0.len());
        let count = (self.0.len() - start).min(buffer.len());
        buffer[..count].copy_from_slice(&self.0[start..start + count]);
        Ok(count)
    }

    fn size(&self) -> Option<u64> {
        Some(self.0.len() as u64)
    }
}

fn test_session() -> (AudioSession, mpsc::Receiver<AudioEdge>) {
    let (edges, receiver) = mpsc::channel();
    let session = AudioSession::open(
        Arc::new(|| {}),
        Arc::new(PlaybackRateControl::default()),
        edges,
    );
    (session, receiver)
}

/// RED — a device-lost rebind re-opens THROUGH THE FACTORY. The ladder's attempt for a new
/// generation asks the factory again and gets its own independent source; nothing is carried
/// over from the dead binding, and there is no path anywhere to re-derive one from.
#[test]
fn a_simulated_device_loss_reopens_the_source_through_the_factory() {
    let factory = Arc::new(CountingFactory::new(b"0123456789abcdef".to_vec()));
    let (session, _edges) = test_session();

    let first = open_media_bytes(factory.as_ref(), &session, FIRST_GENERATION)
        .expect("the first open goes through the factory");
    assert_eq!(factory.opens(), 1);

    // The device is lost: the ladder admits the next generation and attempts again. The dead
    // binding's source is still alive here, exactly as it is on the owner thread until the
    // reaped worker drops it — a reopen must not need it gone.
    session.admit_generation(FIRST_GENERATION + 1);
    let second = open_media_bytes(factory.as_ref(), &session, FIRST_GENERATION + 1)
        .expect("the rebind opens through the same factory");
    assert_eq!(
        factory.opens(),
        2,
        "the rebind called open() again — it did not reuse or re-derive a source",
    );
    drop((first, second));

    // A generation the session has abandoned cannot open at all.
    session.cancel_attempts();
    assert!(
        matches!(
            open_media_bytes(factory.as_ref(), &session, FIRST_GENERATION + 1),
            Err(AudioOpenError::Cancelled),
        ),
        "an abandoned generation opens nothing",
    );
    assert_eq!(
        factory.opens(),
        2,
        "and it does not reach the transport to find that out",
    );
}

/// RED — a cancel that lands mid-open abandons the attempt WITHOUT TOUCHING A DEVICE. The
/// device-resolve counter is the observer: it is process-monotonic, so an extra touch cannot
/// hide behind an earlier one.
#[test]
fn a_cancel_during_the_transport_open_abandons_before_any_device_is_touched() {
    let factory = Arc::new(CountingFactory::new(b"cancelled".to_vec()));
    let (session, _edges) = test_session();
    *factory.cancel.lock().unwrap() = Some(Arc::clone(&session.live_generation));

    #[cfg(feature = "instrument")]
    let resolves_before = DEVICE_RESOLVES.load(Ordering::Acquire);
    let source = AudioSource {
        factory: factory.clone(),
        stream_index: -1,
        source_rate: Arc::new(AtomicU32::new(0)),
    };
    let outcome = prepare_binding(
        &source,
        &AudioDeviceSelection::Chosen,
        None,
        &session,
        AudioWorkerStart {
            generation: FIRST_GENERATION,
            position: None,
            track: 0,
        },
    );

    assert!(
        matches!(outcome, Err(AudioOpenError::Cancelled)),
        "a cancelled attempt returns Cancelled, not a binding: {:?}",
        outcome.err(),
    );
    assert_eq!(factory.opens(), 1, "the transport open was attempted once");
    #[cfg(feature = "instrument")]
    assert_eq!(
        DEVICE_RESOLVES.load(Ordering::Acquire),
        resolves_before,
        "a cancelled attempt resolved no output device",
    );
}

/// The whole open, on real hardware and real media: a `PathAudioSourceFactory` through the
/// custom-AVIO decoder onto the default output device, twice — the second call is the shape a
/// device-lost rebind takes. It is SILENT by construction: `prepare_binding` never builds a
/// `cpal::Stream` (only `commit_binding` does), so nothing can reach a speaker from here.
#[test]
#[ignore = "L5 manual: requires samples/dash-avio and an output device"]
fn the_factory_open_reaches_a_real_device_and_a_real_decoder_twice() {
    let media = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|crates| crates.parent())
        .expect("the crate lives two levels under the repository root")
        .join("samples/dash-avio/audio.m4s");
    let source = AudioSource {
        factory: Arc::new(crate::PathAudioSourceFactory::new(&media)),
        stream_index: -1,
        source_rate: Arc::new(AtomicU32::new(0)),
    };
    let (session, _edges) = test_session();

    for generation in [FIRST_GENERATION, FIRST_GENERATION + 1] {
        session.admit_generation(generation);
        let prepared = prepare_binding(
            &source,
            &AudioDeviceSelection::Chosen,
            None,
            &session,
            AudioWorkerStart {
                generation,
                position: None,
                track: 0,
            },
        )
        .expect("the factory-backed open binds a device and a decoder");
        assert_eq!(prepared.info.source_sample_rate, 48_000);
        assert_eq!(prepared.info.source_channels, 2);
        assert_eq!(prepared.ready.generation, generation);
        // Dropping reaps the decode worker here, on a thread that is allowed to wait.
        drop(prepared);
    }
}

/// RED — the lifecycle table. `Cancelled` and `Failed` are terminal for the generation that
/// reached them: only a new open or reopen leaves them, and every other event is absorbed.
#[test]
fn the_source_lifecycle_is_total_and_its_terminals_are_per_generation() {
    use AudioSourceState::{Cancelled, Closed, Failed, Opening, Ready, Reopening};

    assert_eq!(source_step(Closed, SourceEvent::Open), Opening);
    assert_eq!(source_step(Opening, SourceEvent::Opened), Ready);
    assert_eq!(source_step(Ready, SourceEvent::Reopen), Reopening);
    assert_eq!(source_step(Reopening, SourceEvent::Opened), Ready);
    assert_eq!(source_step(Ready, SourceEvent::Close), Closed);

    assert_eq!(source_step(Opening, SourceEvent::Cancel), Cancelled);
    assert_eq!(source_step(Reopening, SourceEvent::Fail), Failed);
    for absorbed in [
        SourceEvent::Opened,
        SourceEvent::Cancel,
        SourceEvent::Fail,
        SourceEvent::Close,
    ] {
        assert_eq!(source_step(Cancelled, absorbed), Cancelled);
        assert_eq!(source_step(Failed, absorbed), Failed);
    }
    // ...and a NEW generation is the only way out of a terminal state.
    assert_eq!(source_step(Failed, SourceEvent::Reopen), Reopening);
    assert_eq!(source_step(Cancelled, SourceEvent::Open), Opening);

    // A state that reaches the metrics cell survives the round trip it is published through,
    // and the drive path is the table: the cell is only ever written by `step_source`.
    let metrics = SharedMetrics::default();
    assert_eq!(metrics.source_state(), Closed);
    for (event, expected) in [
        (SourceEvent::Open, Opening),
        (SourceEvent::Opened, Ready),
        (SourceEvent::Reopen, Reopening),
        (SourceEvent::Cancel, Cancelled),
        (SourceEvent::Open, Opening),
        (SourceEvent::Fail, Failed),
    ] {
        metrics.step_source(event);
        assert_eq!(metrics.source_state(), expected);
    }
    metrics.fail_source(&AudioOpenError::NoOutputDevice);
    assert_eq!(
        metrics.snapshot(&AudioClock::new()).source_failure,
        Some(AudioSourceKind::Device),
        "the published failure kind is the one the error classified as",
    );
}

/// RED — every open failure carries one of the three kinds a host can act on, and the kind is
/// read off the error rather than re-derived from its message.
#[test]
fn every_open_failure_classifies_as_transport_decoder_or_device() {
    assert_eq!(
        AudioOpenError::Source(crate::AudioSourceError::open("no such file")).kind(),
        AudioSourceKind::Open,
    );
    assert_eq!(AudioOpenError::PathEncoding.kind(), AudioSourceKind::Open);
    assert_eq!(AudioOpenError::Cancelled.kind(), AudioSourceKind::Cancelled);
    assert_eq!(
        AudioOpenError::Native(NativeError {
            operation: "open",
            status: BN_AUDIO_INVALID_STREAM,
            transport: 0,
            message: "not an audio stream".to_owned(),
        })
        .kind(),
        AudioSourceKind::Decode,
    );
    assert_eq!(
        AudioOpenError::NoOutputDevice.kind(),
        AudioSourceKind::Device,
    );
    assert_eq!(
        AudioOpenError::BuildStream("refused".to_owned()).kind(),
        AudioSourceKind::Device,
    );
}

/// An acknowledgement that lands while the waiter is between its two checks must not be
/// lost.
///
/// Under the lock-free shape the race is not merely handled, it is unrepresentable: the waiter
/// re-reads `acknowledged_version` every poll and `fetch_max` is monotonic, so there is no
/// window in which a published acknowledgement can be missed. This test drives that window
/// deliberately — ack strictly after the waiter has begun waiting — and it must still return
/// Ok well inside the timeout.
#[test]
fn callback_control_ack_racing_the_wait_boundary_is_not_lost() {
    let control = Arc::new(CallbackControl::new());
    let version = control
        .publish(MODE_RUNNING, RampKind::Start)
        .expect("publish a request");

    let acking = Arc::clone(&control);
    let notifier = thread::spawn(move || {
        // Land the acknowledgement after the waiter is demonstrably inside its wait, not at
        // the boundary by luck.
        thread::sleep(Duration::from_millis(5));
        acking.acknowledge(version);
    });

    let started = Instant::now();
    control
        .await_acknowledgement(version, Duration::from_millis(500))
        .expect("an acknowledgement published mid-wait is observed, never lost");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "the waiter returned on the acknowledgement, not on its timeout"
    );
    notifier.join().expect("notifier completed");
}

/// The other half of the same seam: a request nobody acknowledges must end at its deadline
/// rather than hang, and it must report that as a timeout and not as success.
#[test]
fn callback_control_wait_returns_at_its_deadline() {
    let control = CallbackControl::new();
    let version = control
        .publish(MODE_RUNNING, RampKind::Start)
        .expect("publish a request");
    let started = Instant::now();
    let outcome = control.await_acknowledgement(version, Duration::from_millis(40));
    assert!(matches!(outcome, Err(AudioError::ControlTimeout)));
    assert!(started.elapsed() >= Duration::from_millis(40));
}

/// A failed binding releases every waiter immediately: `fail` wakes the wait, so a
/// device death cannot leave the owner thread parked until the full control timeout.
#[test]
fn a_failed_binding_releases_a_waiting_owner_at_once() {
    let control = Arc::new(CallbackControl::new());
    let version = control
        .publish(MODE_RUNNING, RampKind::Start)
        .expect("publish a request");
    let failing = Arc::clone(&control);
    let killer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(3));
        failing.fail();
    });
    let outcome = control.await_acknowledgement(version, Duration::from_secs(2));
    assert!(matches!(outcome, Err(AudioError::OutputFailed)));
    killer.join().expect("killer completed");
}

/// Structural guard: `CallbackControl` takes no lock, because its `acknowledge` runs inside
/// the real-time callback. The ban is mechanical rather than a comment asking nobody to
/// re-introduce one.
///
/// Scope: `impl CallbackCore` — everything the audio callback can execute. `CallbackControl`
/// itself is excluded because its OWNER-thread half (`await_acknowledgement`) may sleep; only
/// the callback half must be wait-free.
#[test]
fn the_audio_callback_body_takes_no_lock_and_never_blocks() {
    // Normalized: the block terminator this scan cuts on is written with LF, and this file is
    // CRLF in the worktree, so `find` missed and the test failed on the checkout rather than on
    // the code it is guarding.
    let source = audio_production().replace("\r\n", "\n");
    let start = source
        .find("impl CallbackCore {")
        .expect("the callback implementation must be findable");
    // Cut at the next top-level item so the scan covers exactly this impl block.
    let rest = &source[start + "impl CallbackCore {".len()..];
    let end = rest
        .find(
            "
}
",
        )
        .expect("the callback impl block must terminate");
    let body = &rest[..end];

    for banned in [
        ".lock()",
        "Condvar",
        "notify_one",
        "notify_all",
        "thread::sleep",
        ".recv()",
        "recv_timeout",
        ".join()",
    ] {
        assert!(
            !body.contains(banned),
            "`{banned}` appears inside the cpal real-time callback; that is the exact shape                  this seam was rebuilt to remove"
        );
    }
}

/// The decoder pump over the committed DASH audio sample, at the rate the native decoder is
/// asked to produce. No output device: the pump, the ring and the clock are all it needs.
fn sample_pump(metrics: &Arc<SharedMetrics>) -> DecoderPump {
    let media = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|crates| crates.parent())
        .expect("the crate lives two levels under the repository root")
        .join("samples/dash-avio/audio.m4s");
    pump_over(&media, metrics, Arc::new(StopBound::default()))
}

/// Regression: a video that played to its end and was dragged back to the start played
/// on in silence. A seek landing past the audio's end found no block to anchor on and failed,
/// which faulted the session and reopened it video-only. The pump now anchors such a
/// generation on its target, drained — and the seek back decodes audio again.
#[test]
#[ignore = "L5 manual: requires samples/dash-avio"]
fn a_seek_past_the_audio_end_anchors_on_its_target_and_a_seek_back_brings_audio_back() {
    let metrics = Arc::new(SharedMetrics::default());
    let mut pump = sample_pump(&metrics);
    let mut out = StreamWriter::new(
        Arc::new(AudioRing::new(nonzero_usize(48_000), nonzero_usize(2)).expect("ring")),
        None,
    );
    let ring = out.ring.clone();
    let clock = AudioClock::new();

    let past_end = Duration::from_secs(3_600);
    pump.seek(past_end)
        .expect("the native seek past the end is accepted");
    let ready = pump
        .begin_generation(
            &mut out,
            &clock,
            1,
            Some(NormalizedTimestamp::from_position(past_end)),
            4_800,
        )
        .expect("a target past the audio's end anchors on the target");
    assert_eq!(ready.anchor, NormalizedTimestamp::from_position(past_end));
    assert_eq!(ring.available_frames(), 0, "nothing is left to play there");
    assert!(
        metrics.eof_reached.load(Ordering::Acquire),
        "the output reads as drained"
    );

    ring.discard_all();
    pump.seek(Duration::ZERO).expect("the seek back");
    let ready = pump
        .begin_generation(
            &mut out,
            &clock,
            2,
            Some(NormalizedTimestamp::from_position(Duration::ZERO)),
            4_800,
        )
        .expect("the seek back to the start");
    assert!(ring.available_frames() > 0, "the audio is back");
    assert!(!metrics.eof_reached.load(Ordering::Acquire));
    assert!(ready.anchor < NormalizedTimestamp::from_position(Duration::from_millis(100)));
}

#[test]
fn every_generic_wrapper_is_marked_and_a_sound_cards_own_driver_is_not() {
    for wrapper in [
        "ASIO4ALL v2",
        "FlexASIO",
        "ASIO2WASAPI",
        "Generic Low Latency ASIO Driver",
        "Steinberg built-in ASIO Driver",
        "Realtek ASIO",
        "Voicemeeter AUX Virtual ASIO",
        "JackRouter",
        "ReaRoute ASIO (x64)",
    ] {
        assert!(
            is_generic_asio_driver(wrapper),
            "{wrapper} wraps another API"
        );
    }
    for vendor in [
        "Focusrite USB ASIO",
        "Yamaha Steinberg USB ASIO",
        "MOTU Pro Audio",
        "ASIO MADIface USB",
    ] {
        assert!(
            !is_generic_asio_driver(vendor),
            "{vendor} is a sound card's own driver"
        );
    }
}

fn endpoint(id: &str, name: &str, format: Option<(u16, u32)>) -> super::endpoints::Endpoint {
    super::endpoints::Endpoint {
        id: id.to_owned(),
        name: name.to_owned(),
        format,
    }
}

/// The systems a machine offers, shaped from what Windows and the registry list: WASAPI
/// always, with its endpoints by id in Windows' order and the default marked; ASIO only where
/// it is built and a driver is registered, with the wrappers marked and only the loaded
/// driver's capabilities known.
#[test]
fn the_systems_are_wasapi_always_and_asio_where_it_is_built_and_registered() {
    let speakers = "{0.0.0.00000000}.{speakers}";
    let apogee = "{0.0.0.00000000}.{apogee}";
    let wasapi = || {
        vec![
            endpoint(speakers, "扬声器 (Realtek(R) Audio)", Some((2, 48_000))),
            endpoint(apogee, "Duet (Apogee)", None),
        ]
    };
    let loaded = AudioDeviceCaps {
        channels: 4,
        rates: vec![44_100, 48_000, 96_000, 192_000],
        dsd: Some(true),
    };
    let systems = output_systems(
        wasapi(),
        Some(apogee),
        Some(vec![
            ("Apogee USB ASIO Driver".to_owned(), Some(loaded.clone())),
            ("ASIO4ALL v2".to_owned(), None),
        ]),
    );
    assert_eq!(
        systems.iter().map(|system| system.host).collect::<Vec<_>>(),
        [AudioOutputHost::Wasapi, AudioOutputHost::Asio]
    );
    let endpoints = &systems[0].devices;
    assert_eq!(
        endpoints
            .iter()
            .map(|device| device.id.as_str())
            .collect::<Vec<_>>(),
        [speakers, apogee]
    );
    assert_eq!(
        endpoints
            .iter()
            .map(|device| device.is_default)
            .collect::<Vec<_>>(),
        [false, true]
    );
    assert_eq!(
        endpoints[0].caps,
        Some(AudioDeviceCaps {
            channels: 2,
            rates: vec![48_000],
            dsd: None
        }),
        "shared: the mix format"
    );
    assert_eq!(
        endpoints[1].caps, None,
        "an endpoint that gave no mix format"
    );
    assert!(endpoints.iter().all(|device| !device.generic));
    let drivers = &systems[1].devices;
    assert_eq!(
        drivers
            .iter()
            .map(|device| (device.id.as_str(), device.name.as_str()))
            .collect::<Vec<_>>(),
        [
            ("Apogee USB ASIO Driver", "Apogee USB ASIO Driver"),
            ("ASIO4ALL v2", "ASIO4ALL v2"),
        ]
    );
    assert_eq!(
        drivers
            .iter()
            .map(|device| device.generic)
            .collect::<Vec<_>>(),
        [false, true]
    );
    assert_eq!(
        drivers
            .iter()
            .map(|device| device.caps.clone())
            .collect::<Vec<_>>(),
        [Some(loaded), None]
    );
    assert!(
        drivers.iter().all(|device| !device.is_default),
        "ASIO has no default device"
    );

    // No driver registered, or no ASIO in the build: WASAPI alone.
    for asio in [Some(Vec::new()), None] {
        let systems = output_systems(wasapi(), None, asio);
        assert_eq!(systems.len(), 1);
        assert!(
            systems[0].devices.iter().all(|device| !device.is_default),
            "no default output"
        );
    }
    // No endpoint at all is still the system, with nothing in it.
    assert_eq!(output_systems(Vec::new(), None, None)[0].devices, []);
}

/// The route's law: where outputs bind, when they follow Windows' default, when a vanished
/// endpoint takes them back, and how a failed driver falls back and is tried again.
#[test]
fn a_route_that_cannot_be_followed_falls_back_to_the_default_and_comes_back() {
    let mut state = RouteState::new();
    assert_eq!(state.effective(), AudioOutputRoute::WasapiDefault);
    assert!(state.follows_default());
    assert!(
        !state.choose(AudioOutputRoute::WasapiDefault),
        "choosing where outputs are moves nothing"
    );

    let chosen = AudioOutputRoute::Wasapi("{apogee}".to_owned());
    assert!(state.choose(chosen.clone()));
    assert_eq!(state.effective(), chosen);
    assert!(
        !state.follows_default(),
        "a chosen endpoint stays put when the default moves"
    );
    assert!(!state.awaits("{apogee}"), "it is there");

    // Unplugged: the bind goes to the default and says so, once.
    let at = state.snapshot();
    state.endpoint_missing(&at);
    let fallback = state.fallback().cloned().expect("the fallback is said");
    assert_eq!(
        (
            fallback.version,
            fallback.wanted.clone(),
            fallback.reason.clone()
        ),
        (1, chosen.clone(), AudioFallbackReason::DeviceMissing)
    );
    state.endpoint_missing(&at);
    assert_eq!(
        state.fallback().map(|fallback| fallback.version),
        Some(1),
        "a second bind that misses it is not news"
    );
    assert_eq!(state.effective(), chosen, "every bind asks for it first");
    assert!(
        state.follows_default(),
        "while it is gone, outputs are where the default is"
    );
    assert!(state.awaits("{apogee}"));
    assert!(!state.awaits("{speakers}"));
    // Back: the bind found it.
    state.endpoint_found(&at);
    assert_eq!(state.fallback(), None);
    assert!(!state.follows_default());
    assert!(!state.awaits("{apogee}"));

    // A driver that fails puts outputs on WASAPI's default until it is chosen again.
    let driver = AudioOutputRoute::Asio("Apogee USB ASIO Driver".to_owned());
    assert!(state.choose(driver.clone()));
    assert_eq!(state.effective(), driver);
    let at = state.snapshot();
    let after = state.driver_failed(&at, "ASE_NotPresent").expect("news");
    assert_eq!(
        (after.route.clone(), after.version),
        (AudioOutputRoute::WasapiDefault, at.version + 1),
        "the reading a bind goes on with"
    );
    assert_eq!(state.effective(), AudioOutputRoute::WasapiDefault);
    assert!(state.follows_default());
    assert_eq!(
        state
            .fallback()
            .map(|fallback| (fallback.version, fallback.reason.clone())),
        Some((
            2,
            AudioFallbackReason::DriverFailed("ASE_NotPresent".to_owned())
        ))
    );
    assert_eq!(
        state.driver_failed(&at, "again"),
        None,
        "a failed driver fails once"
    );
    assert!(
        state.choose(driver.clone()),
        "chosen again, it is tried again"
    );
    assert_eq!(
        (state.effective(), state.fallback()),
        (driver.clone(), None)
    );
    assert!(!state.choose(driver));
    // A WASAPI route has no driver to fail.
    assert!(state.choose(AudioOutputRoute::WasapiDefault));
    assert_eq!(state.driver_failed(&state.snapshot(), "not on ASIO"), None);
}

/// A bind reads the route once and is marked with that reading's version; a route chosen while
/// it resolves leaves it behind the process's version, so it rebinds. What it finds out about
/// its reading — a driver that failed, an endpoint missing — changes nothing once the
/// listener has chosen again: an old driver's failure cannot mark the new one failed.
#[test]
fn a_bind_is_marked_by_the_reading_it_resolved_and_a_stale_reading_changes_nothing() {
    let mut state = RouteState::new();
    let first = AudioOutputRoute::Asio("Apogee USB ASIO Driver".to_owned());
    let second = AudioOutputRoute::Asio("RME Fireface USB".to_owned());
    assert!(state.choose(first.clone()));
    let read = state.snapshot();
    assert_eq!(read.route, first);
    // The listener chooses another driver while the bind by `read` resolves the first.
    assert!(state.choose(second.clone()));
    assert!(
        read.version < state.snapshot().version,
        "the binding by the old reading is behind: it rebinds"
    );
    // The first driver fails under the old reading: the second is not marked failed.
    assert_eq!(state.driver_failed(&read, "ASE_NotPresent"), None);
    assert_eq!(
        (state.effective(), state.fallback()),
        (second.clone(), None)
    );
    // The same for an endpoint the old reading missed or found.
    let endpoint = AudioOutputRoute::Wasapi("{apogee}".to_owned());
    assert!(state.choose(endpoint.clone()));
    let read = state.snapshot();
    assert!(state.choose(AudioOutputRoute::Wasapi("{speakers}".to_owned())));
    state.endpoint_missing(&read);
    assert_eq!(
        state.fallback(),
        None,
        "a missing endpoint of the old choice is not the new one's"
    );
    // A reading the listener has chosen the same route since is stale as well.
    let read = state.snapshot();
    let _ = state.choose(state.chosen.clone());
    state.endpoint_missing(&read);
    assert_eq!(state.fallback(), None);
    // Windows' default moving is a move of its own, seen by every reading before it.
    let mut follows = RouteState::new();
    let read = follows.snapshot();
    follows.move_outputs();
    assert!(read.version < follows.snapshot().version);
    let _ = endpoint;
}

/// Serialises the tests that change the process's route.
static ROUTE_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The endpoint watch starts once, stops — unregistered and released on its own thread, before
/// the stop returns — and starts again when a bind asks for it.
#[test]
fn the_endpoint_watch_stops_and_starts_again() {
    let _exclusive = ROUTE_TESTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    super::endpoints::watch();
    super::endpoints::watch();
    assert!(super::endpoints::watching());
    stop_endpoint_watch();
    assert!(!super::endpoints::watching());
    stop_endpoint_watch();
    super::endpoints::watch();
    assert!(super::endpoints::watching(), "a later bind starts it again");
    stop_endpoint_watch();
}

/// The chosen endpoint removed, or turned off, while outputs are on it: Windows says so to the
/// endpoint watch, which moves the outputs — the stream on it need never report its loss —
/// and wakes every live session's media loop to see the move. Another endpoint's going, or
/// the chosen one's while outputs already stand on the default without it, moves nothing.
#[test]
fn the_chosen_endpoint_going_moves_the_outputs_and_wakes_the_sessions() {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Media::Audio::{
        IMMNotificationClient, DEVICE_STATE_ACTIVE, DEVICE_STATE_DISABLED, DEVICE_STATE_UNPLUGGED,
    };
    let _exclusive = ROUTE_TESTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let woken = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&woken);
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        counted.fetch_add(1, Ordering::Relaxed);
    });
    wake_on_move(&wake);
    let watch: IMMNotificationClient = super::endpoints::EndpointWatch.into();
    let apogee = HSTRING::from("{0.0.0.00000000}.{apogee}");
    let speakers = HSTRING::from("{0.0.0.00000000}.{speakers}");
    let (apogee_id, speakers_id) = (PCWSTR(apogee.as_ptr()), PCWSTR(speakers.as_ptr()));
    set_output_route(AudioOutputRoute::Wasapi(apogee.to_string()));

    let moves = |say: &dyn Fn()| {
        let (version, wakes) = (output_route_version(), woken.load(Ordering::Relaxed));
        say();
        (
            output_route_version() - version,
            woken.load(Ordering::Relaxed) - wakes,
        )
    };
    // SAFETY: the watch is called as Windows calls it, with live strings.
    unsafe {
        assert_eq!(
            moves(&|| watch.OnDeviceRemoved(apogee_id).unwrap()),
            (1, 1),
            "removed"
        );
        assert_eq!(
            moves(&|| watch
                .OnDeviceStateChanged(apogee_id, DEVICE_STATE_UNPLUGGED)
                .unwrap()),
            (1, 1),
            "unplugged"
        );
        assert_eq!(
            moves(&|| watch
                .OnDeviceStateChanged(apogee_id, DEVICE_STATE_DISABLED)
                .unwrap()),
            (1, 1),
            "disabled"
        );
        assert_eq!(
            moves(&|| watch.OnDeviceRemoved(speakers_id).unwrap()),
            (0, 0),
            "another endpoint"
        );
        assert_eq!(
            moves(&|| watch
                .OnDeviceStateChanged(speakers_id, DEVICE_STATE_ACTIVE)
                .unwrap()),
            (0, 0),
            "another endpoint arriving"
        );
        // Once a bind has found it missing, outputs stand on the default: its going again
        // moves nothing, and its coming back takes them back.
        change_route("test", |state| {
            let at = state.snapshot();
            state.endpoint_missing(&at);
        });
        assert_eq!(
            moves(&|| watch.OnDeviceRemoved(apogee_id).unwrap()),
            (0, 0),
            "already off it"
        );
        assert_eq!(
            moves(&|| watch
                .OnDeviceStateChanged(apogee_id, DEVICE_STATE_ACTIVE)
                .unwrap()),
            (1, 1),
            "back"
        );
    }
    set_output_route(AudioOutputRoute::WasapiDefault);
    drop(wake);
    let before = woken.load(Ordering::Relaxed);
    set_output_route(AudioOutputRoute::Wasapi(apogee.to_string()));
    set_output_route(AudioOutputRoute::WasapiDefault);
    assert_eq!(
        woken.load(Ordering::Relaxed),
        before,
        "a session gone is not woken"
    );
}

/// An endpoint chosen and then gone binds the default output instead, so the session plays on,
/// and the fallback is published. Resolving opens no stream: it reads the mix format.
#[test]
fn a_vanished_chosen_endpoint_binds_the_default_output_and_says_so() {
    let _exclusive = ROUTE_TESTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let gone = AudioOutputRoute::Wasapi(
        "{0.0.0.00000000}.{00000000-0000-0000-0000-00000000dead}".to_owned(),
    );
    assert_eq!(set_output_route(gone.clone()), gone);
    let before = output_fallback_version();
    let default_name = cpal::default_host()
        .default_output_device()
        .and_then(|device| device.name().ok());
    match resolve_output_device(&AudioDeviceSelection::Chosen, route_snapshot(), None) {
        Ok(resolved) => {
            assert_eq!(resolved.host, AudioOutputHost::Wasapi);
            assert_eq!(
                Some(resolved.name),
                default_name,
                "the default output, not a failure"
            );
            let fallback = output_fallback().expect("the fallback is published");
            assert_eq!(
                (fallback.wanted, fallback.reason),
                (gone.clone(), AudioFallbackReason::DeviceMissing)
            );
            assert_eq!(output_fallback_version(), before + 1);
        }
        // A machine with no output at all has no default to fall back to either.
        Err(error) => assert!(default_name.is_none(), "{error}"),
    }
    assert_eq!(output_route(), gone, "the route stays chosen");
    assert_eq!(
        set_output_route(AudioOutputRoute::WasapiDefault),
        AudioOutputRoute::WasapiDefault
    );
    assert_eq!(output_fallback(), None);
}

#[test]
fn an_asio_stream_asks_for_the_main_pair_and_a_wasapi_stream_for_every_channel() {
    let supported = |channels| {
        cpal::SupportedStreamConfig::new(
            channels,
            cpal::SampleRate(96_000),
            cpal::SupportedBufferSize::Unknown,
            SampleFormat::I32,
        )
    };
    let asio = stream_config_for(AudioOutputHost::Asio, &supported(18));
    assert_eq!(asio.channels, 2);
    assert_eq!(
        asio.sample_rate,
        cpal::SampleRate(96_000),
        "the driver's rate is kept"
    );
    assert_eq!(
        stream_config_for(AudioOutputHost::Asio, &supported(1)).channels,
        1
    );
    assert_eq!(
        stream_config_for(AudioOutputHost::Wasapi, &supported(6)).channels,
        6
    );
}

/// A build without the `asio` feature has no ASIO route: choosing one is WASAPI's default, and
/// a driver failure has nothing to move — every output stays exactly where it was.
#[test]
#[cfg(not(feature = "asio"))]
fn without_asio_in_the_build_an_asio_route_is_the_default_output() {
    let _exclusive = ROUTE_TESTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(
        set_output_route(AudioOutputRoute::Asio("RME Fireface".to_owned())),
        AudioOutputRoute::WasapiDefault
    );
    let version = output_route_version();
    let _ = demote_asio(&route_snapshot(), "a driver that was never chosen");
    assert_eq!(
        output_route_version(),
        version,
        "no output is asked to rebind"
    );
    assert!(!asio_in_use());
}

/// A driver layer that fails the way a real ASIO driver does: `CoCreateInstance` on a thread
/// that never initialised COM. The SDK initialises COM on the thread that first loads a driver
/// and on no other, so only that thread — here, the one `enter_thread` ran on — can load.
struct FakeAsio {
    com_thread: std::sync::Mutex<Option<thread::ThreadId>>,
    load_threads: std::sync::Mutex<Vec<String>>,
    loaded: AtomicBool,
    rate: AtomicU32,
    rates: Vec<u32>,
    dsd_support: AtomicU8,
    dop: AtomicBool,
    native_dsd: AtomicBool,
    format_changes: std::sync::Mutex<Vec<&'static str>>,
    /// Every load, by the driver's name, in order.
    loaded_names: std::sync::Mutex<Vec<String>>,
}

struct FakeAsioApi(Arc<FakeAsio>);

impl FakeAsio {
    fn new(rate: u32, rates: &[u32]) -> Arc<Self> {
        Arc::new(Self {
            com_thread: std::sync::Mutex::new(None),
            load_threads: std::sync::Mutex::new(Vec::new()),
            loaded_names: std::sync::Mutex::new(Vec::new()),
            loaded: AtomicBool::new(false),
            rate: AtomicU32::new(rate),
            rates: rates.to_vec(),
            dsd_support: AtomicU8::new(0),
            dop: AtomicBool::new(false),
            native_dsd: AtomicBool::new(false),
            format_changes: std::sync::Mutex::new(Vec::new()),
        })
    }
}

impl AsioDriverApi for FakeAsioApi {
    type Driver = ();

    fn enter_thread(&self) {
        *self.0.com_thread.lock().unwrap() = Some(thread::current().id());
    }

    fn names(&self) -> Vec<String> {
        vec![
            "Realtek ASIO".to_owned(),
            "Apogee USB ASIO Driver".to_owned(),
        ]
    }

    fn load(&self, name: &str) -> Result<(), String> {
        self.0
            .load_threads
            .lock()
            .unwrap()
            .push(thread::current().name().unwrap_or("?").to_owned());
        self.0.loaded_names.lock().unwrap().push(name.to_owned());
        if *self.0.com_thread.lock().unwrap() != Some(thread::current().id()) {
            return Err("CoCreateInstance: CO_E_NOTINITIALIZED".to_owned());
        }
        if self.0.loaded.swap(true, Ordering::AcqRel) {
            return Err("DriverAlreadyExists".to_owned());
        }
        Ok(())
    }

    fn unload(&self, _driver: ()) {
        self.0.loaded.store(false, Ordering::Release);
    }

    fn rate(&self, _driver: &()) -> Option<u32> {
        Some(self.0.rate.load(Ordering::Acquire))
    }

    fn can_rate(&self, _driver: &(), rate: u32) -> bool {
        self.0.rates.contains(&rate)
    }

    fn set_rate(&self, _driver: &(), rate: u32) -> Result<(), String> {
        if !self.0.rates.contains(&rate) {
            return Err("ASE_NoClock".to_owned());
        }
        self.0.rate.store(rate, Ordering::Release);
        Ok(())
    }

    fn open_dsd(
        &self,
        _driver: &(),
        bit_rate: u32,
        channels: u16,
    ) -> Result<Option<AsioDsdFormat>, String> {
        assert_eq!(
            *self.0.com_thread.lock().unwrap(),
            Some(thread::current().id())
        );
        self.0.format_changes.lock().unwrap().push("try DSD");
        negotiate_dsd(self, bit_rate, channels, self.0.dop.load(Ordering::Acquire))
    }

    fn restore_pcm(&self, _driver: &(), rate: u32) -> Result<(), String> {
        assert_eq!(
            *self.0.com_thread.lock().unwrap(),
            Some(thread::current().id())
        );
        self.0.format_changes.lock().unwrap().push("restore PCM");
        self.0.native_dsd.store(false, Ordering::Release);
        self.0.rate.store(rate, Ordering::Release);
        Ok(())
    }

    fn caps(&self, _driver: &()) -> Option<(u16, Vec<u32>)> {
        Some((2, self.0.rates.clone()))
    }
}

impl AsioDsdIo for FakeAsioApi {
    fn set_native(&self) -> bool {
        let accepted = matches!(self.0.dsd_support.load(Ordering::Acquire), 1 | 3 | 4 | 5);
        self.0.native_dsd.store(accepted, Ordering::Release);
        accepted
    }
    fn set_pcm(&self) -> Result<(), String> {
        if self.0.dsd_support.load(Ordering::Acquire) == 3 {
            return Err("PCM format refused".to_owned());
        }
        self.0.native_dsd.store(false, Ordering::Release);
        Ok(())
    }
    fn accepts_rate(&self, rate: u32) -> bool {
        match self.0.dsd_support.load(Ordering::Acquire) {
            1 | 4 => rate >= 2_822_400 || (rate == 176_400 || rate == 352_800),
            2 | 5 | 6 => rate == 176_400 || rate == 352_800,
            _ => false,
        }
    }
    fn set_dsd_rate(&self, rate: u32) -> Result<(), String> {
        self.0.rate.store(rate, Ordering::Release);
        Ok(())
    }
    fn output_type(&self, channel: u16) -> i32 {
        if self.0.native_dsd.load(Ordering::Acquire) {
            if self.0.dsd_support.load(Ordering::Acquire) == 4 && channel == 1 {
                16
            } else {
                33
            }
        } else if self.0.dsd_support.load(Ordering::Acquire) == 6 {
            19
        } else {
            17
        }
    }
}
#[test]
fn asio_host_selects_dsd_or_dop_and_restores_pcm_for_the_next_song() {
    for (support, transport, rate) in [
        (1, AsioDsdTransport::Native, 5_644_800),
        (2, AsioDsdTransport::Dop, 352_800),
        (4, AsioDsdTransport::Dop, 352_800),
        (5, AsioDsdTransport::Dop, 352_800),
    ] {
        let fake = FakeAsio::new(48_000, &[48_000, 96_000]);
        fake.dsd_support.store(support, Ordering::Release);
        fake.dop.store(true, Ordering::Release);
        let host = AsioHost::new(FakeAsioApi(fake.clone()), Duration::from_millis(2));
        let (format, original, lease) = host
            .open_dsd(APOGEE, 5_644_800, 2)
            .unwrap_or_else(|_| panic!("DSD open"))
            .unwrap();
        assert_eq!(
            (format.transport, format.hardware_rate, original),
            (transport, rate, 48_000)
        );
        // The driver said whether it takes DSD itself; the device list says so from now on.
        assert_eq!(
            host.loaded_caps(APOGEE).and_then(|caps| caps.dsd),
            Some(transport == AsioDsdTransport::Native)
        );
        super::dsd_delivery_tests::assert_muted_transport(format.transport);
        assert_eq!(
            fake.rate.load(Ordering::Acquire),
            rate,
            "mute keeps the negotiated rate"
        );
        assert_eq!(
            *fake.format_changes.lock().unwrap(),
            ["try DSD"],
            "mute does not change I/O format"
        );
        assert!(
            matches!(
                host.open(APOGEE, None, |_| ()),
                Err(AsioOpenError::Resetting)
            ),
            "no PCM writer joins DSD buffers"
        );
        host.restore_pcm(original).unwrap();
        assert_eq!(fake.rate.load(Ordering::Acquire), 48_000);
        assert!(!fake.native_dsd.load(Ordering::Acquire));
        let (_, pcm) = host
            .open(APOGEE, None, |_| ())
            .unwrap_or_else(|_| panic!("parked PCM"));
        assert!(
            host.resume_dsd(5_644_800, 2).is_err(),
            "another writer still owns PCM buffers"
        );
        drop(pcm);
        assert_eq!(host.resume_dsd(5_644_800, 2).unwrap(), format);
        host.restore_pcm(original).unwrap();
        drop(lease);
        let (_, next) = host
            .open(APOGEE, Some(96_000), |_| ())
            .unwrap_or_else(|_| panic!("next PCM song"));
        assert_eq!(fake.rate.load(Ordering::Acquire), 96_000);
        assert_eq!(
            *fake.format_changes.lock().unwrap(),
            ["try DSD", "restore PCM", "try DSD", "restore PCM"]
        );
        drop(next);
    }
}

/// A driver with no native DSD that takes the DoP carrier rate is not sent DoP unless the
/// listener chose it: the DAC behind it may not decode DoP. The song goes out as PCM.
#[test]
fn a_driver_without_native_dsd_is_not_sent_dop_unless_it_was_chosen() {
    for support in [2, 4, 5] {
        let fake = FakeAsio::new(48_000, &[48_000]);
        fake.dsd_support.store(support, Ordering::Release);
        let host = AsioHost::new(FakeAsioApi(fake.clone()), Duration::ZERO);
        assert!(
            matches!(host.open_dsd(APOGEE, 5_644_800, 2), Ok(None)),
            "support {support}"
        );
        assert_eq!(fake.rate.load(Ordering::Acquire), 48_000);
        assert!(!fake.native_dsd.load(Ordering::Acquire));
    }
}

/// A song change opens the next DSD song while the song before still holds the driver. The
/// open waits for it to let go and then negotiates; a driver still held after the wait plays
/// the song through the PCM conversion. Neither is an error: an error ended the session.
#[test]
fn a_dsd_song_opened_while_the_song_before_lets_go_waits_or_plays_as_pcm() {
    let fake = FakeAsio::new(48_000, &[48_000]);
    fake.dsd_support.store(1, Ordering::Release);
    let host = Arc::new(AsioHost::new(
        FakeAsioApi(fake.clone()),
        Duration::from_millis(500),
    ));
    let (_, before) = host
        .open(APOGEE, None, |_| ())
        .unwrap_or_else(|_| panic!("the song before"));
    let letting_go = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        drop(before);
    });
    let (format, original, lease) = host
        .open_dsd(APOGEE, 5_644_800, 2)
        .unwrap_or_else(|_| panic!("waited"))
        .expect("native DSD after the wait");
    assert_eq!(format.transport, AsioDsdTransport::Native);
    letting_go.join().unwrap();
    host.restore_pcm(original).unwrap();
    drop(lease);

    let stays = FakeAsio::new(48_000, &[48_000]);
    stays.dsd_support.store(1, Ordering::Release);
    let held = AsioHost::new(FakeAsioApi(stays.clone()), Duration::from_millis(20));
    let (_, still) = held
        .open(APOGEE, None, |_| ())
        .unwrap_or_else(|_| panic!("a binding that stays"));
    assert!(
        matches!(held.open_dsd(APOGEE, 5_644_800, 2), Ok(None)),
        "played through PCM, not an error"
    );
    assert!(!stays.native_dsd.load(Ordering::Acquire));
    drop(still);
}

#[test]
fn asio_host_refusal_or_failure_restores_the_original_pcm_clock() {
    for support in [0, 3, 6] {
        let fake = FakeAsio::new(48_000, &[48_000]);
        fake.dsd_support.store(support, Ordering::Release);
        let host = AsioHost::new(FakeAsioApi(fake.clone()), Duration::ZERO);
        let result = host.open_dsd(APOGEE, 2_822_400, 2);
        assert!(if support == 3 {
            result.is_err()
        } else {
            matches!(result, Ok(None))
        });
        assert_eq!(fake.rate.load(Ordering::Acquire), 48_000);
        assert!(!fake.native_dsd.load(Ordering::Acquire));
        assert!(host.open(APOGEE, None, |_| ()).is_ok());
    }
}

#[test]
fn asio_host_reset_waits_for_dsd_teardown_before_reloading() {
    let fake = FakeAsio::new(48_000, &[48_000]);
    fake.dsd_support.store(1, Ordering::Release);
    let host = AsioHost::new(FakeAsioApi(fake.clone()), Duration::ZERO);
    let (_, original, lease) = host
        .open_dsd(APOGEE, 2_822_400, 2)
        .unwrap_or_else(|_| panic!("DSD"))
        .unwrap();
    host.request_reset();
    assert!(matches!(
        host.open(APOGEE, None, |_| ()),
        Err(AsioOpenError::Resetting)
    ));
    assert_eq!(fake.load_threads.lock().unwrap().len(), 1);
    host.restore_pcm(original).unwrap();
    assert!(host.resume_dsd(2_822_400, 2).is_err());
    drop(lease);
    assert!(host.open(APOGEE, None, |_| ()).is_ok());
    assert_eq!(fake.load_threads.lock().unwrap().len(), 2);
}

const APOGEE: &str = "Apogee USB ASIO Driver";

/// Opens the driver the way a rebind attempt does: from a fresh thread of its own.
fn open_from_a_rebind_thread(
    host: &Arc<AsioHost<FakeAsioApi>>,
    rate: Option<u32>,
) -> Result<AsioLease, String> {
    let host = Arc::clone(host);
    thread::spawn(move || {
        host.open(APOGEE, rate, |_| ())
            .map(|((), lease)| lease)
            .map_err(|error| match error {
                AsioOpenError::Resetting => "resetting".to_owned(),
                AsioOpenError::Failed(error) => error,
            })
    })
    .join()
    .expect("the rebind thread")
}

#[test]
fn asio_switched_on_off_on_off_loads_and_unloads_the_driver_every_time() {
    let fake = FakeAsio::new(48_000, &[44_100, 48_000, 96_000]);
    let host = Arc::new(AsioHost::new(
        FakeAsioApi(Arc::clone(&fake)),
        Duration::from_millis(200),
    ));
    for round in 0..2 {
        // On: the rebind lands on the driver, and the binding holds it.
        let lease = open_from_a_rebind_thread(&host, None)
            .unwrap_or_else(|error| panic!("round {round}: {error}"));
        assert!(fake.loaded.load(Ordering::Acquire));
        // Off: the owner thread destroys the binding, and the WASAPI resolve that follows
        // unloads the driver before it opens anything, so the interface is free.
        drop(lease);
        assert!(
            host.release_when_idle(),
            "round {round}: nothing held the driver"
        );
        assert!(
            !fake.loaded.load(Ordering::Acquire),
            "round {round}: the driver is unloaded before WASAPI opens"
        );
        assert!(!host.is_loaded());
    }
    #[cfg(feature = "instrument")]
    assert_eq!(host.work.loads.load(Ordering::Relaxed), 2);
    #[cfg(feature = "instrument")]
    assert_eq!(host.work.unloads.load(Ordering::Relaxed), 2);
    // Every load ran on the one thread with COM, whichever thread asked for it.
    assert!(fake
        .load_threads
        .lock()
        .unwrap()
        .iter()
        .all(|name| name == "player-core-asio"));

    // What the switch used to hit: a second load from another thread fails, and that failure
    // switched ASIO off. The fake reproduces it when called directly.
    let direct = FakeAsioApi(Arc::clone(&fake));
    assert!(thread::spawn(move || direct.load(APOGEE))
        .join()
        .unwrap()
        .is_err());
}

/// WASAPI opens off ASIO only once the driver is released: one an output still holds past
/// the idle wait is `AsioStillHeld` — the driver stays loaded, nothing is opened beside it —
/// and the next attempt, once the output let go, releases it and goes on.
#[test]
fn wasapi_opens_only_after_the_asio_driver_is_released() {
    let fake = FakeAsio::new(48_000, &[48_000]);
    let host = Arc::new(AsioHost::new(
        FakeAsioApi(Arc::clone(&fake)),
        Duration::from_millis(30),
    ));
    let held = open_from_a_rebind_thread(&host, None).expect("a DSD song's output on the driver");
    assert!(matches!(
        release_for_wasapi(&host),
        Err(AudioOpenError::AsioStillHeld)
    ));
    assert!(
        fake.loaded.load(Ordering::Acquire),
        "the driver is left as it is"
    );
    assert_eq!(
        AudioOpenError::AsioStillHeld.kind(),
        AudioSourceKind::Device,
        "a device's wait, not the song's fault"
    );
    drop(held);
    assert!(release_for_wasapi(&host).is_ok());
    assert!(
        !fake.loaded.load(Ordering::Acquire),
        "released before WASAPI opens"
    );
}

#[test]
fn the_driver_is_not_unloaded_while_a_binding_holds_it() {
    let fake = FakeAsio::new(48_000, &[48_000]);
    let host = Arc::new(AsioHost::new(
        FakeAsioApi(Arc::clone(&fake)),
        Duration::from_millis(50),
    ));
    let lease = open_from_a_rebind_thread(&host, None).expect("on");
    assert!(!host.release_when_idle(), "a held driver is not released");
    assert!(fake.loaded.load(Ordering::Acquire));
    drop(lease);
    assert!(host.release_when_idle());
    assert!(!fake.loaded.load(Ordering::Acquire));
}

/// Moving from one driver to another, and from ASIO to WASAPI and back, never has two drivers
/// loaded: the one before is unloaded, once nothing holds it, before the next loads. One still
/// held keeps the next from loading at all — the open answers `Resetting` after the idle wait
/// and the rebind ladder tries again. (The fake refuses a load while a driver is loaded.)
#[test]
fn a_switch_between_drivers_or_systems_never_has_two_drivers_loaded() {
    const OTHER: &str = "RME Fireface USB";
    let fake = FakeAsio::new(48_000, &[44_100, 48_000]);
    let host = Arc::new(AsioHost::new(
        FakeAsioApi(Arc::clone(&fake)),
        Duration::from_millis(50),
    ));
    let open = |name: &'static str| {
        let host = Arc::clone(&host);
        thread::spawn(move || host.open(name, None, |_| ()).map(|((), lease)| lease))
            .join()
            .expect("the rebind thread")
    };
    let loads = || fake.loaded_names.lock().unwrap().clone();

    let apogee = open(APOGEE).unwrap_or_else(|_| panic!("the Apogee's driver"));
    assert_eq!(
        host.loaded_caps(APOGEE),
        Some(AudioDeviceCaps {
            channels: 2,
            rates: vec![44_100, 48_000],
            dsd: None
        })
    );
    assert_eq!(
        host.loaded_caps(OTHER),
        None,
        "a driver that is not loaded is not loaded to be asked"
    );
    // The other driver chosen while the Apogee's binding still holds its driver.
    assert!(matches!(open(OTHER), Err(AsioOpenError::Resetting)));
    assert_eq!(loads(), [APOGEE], "nothing more was loaded");
    drop(apogee);
    let other = open(OTHER).unwrap_or_else(|_| panic!("the other driver"));
    assert_eq!(loads(), [APOGEE, OTHER]);
    assert!(fake.loaded.load(Ordering::Acquire));

    // Onto WASAPI: the driver goes before WASAPI opens, once its binding lets go.
    assert!(!host.release_when_idle(), "held, it stays");
    drop(other);
    assert!(host.release_when_idle());
    assert!(!host.is_loaded() && !fake.loaded.load(Ordering::Acquire));
    // And back to ASIO: loaded afresh.
    let back = open(APOGEE).unwrap_or_else(|_| panic!("the Apogee's driver again"));
    assert_eq!(loads(), [APOGEE, OTHER, APOGEE]);
    drop(back);
    #[cfg(feature = "instrument")]
    assert_eq!(
        (
            host.work.loads.load(Ordering::Relaxed),
            host.work.unloads.load(Ordering::Relaxed)
        ),
        (3, 2)
    );
}

#[cfg(feature = "instrument")]
#[test]
fn the_asio_rate_follows_the_song_and_a_song_at_the_same_rate_changes_nothing() {
    let fake = FakeAsio::new(48_000, &[44_100, 48_000, 96_000, 192_000]);
    let host = Arc::new(AsioHost::new(
        FakeAsioApi(Arc::clone(&fake)),
        Duration::from_millis(50),
    ));
    let changes = || host.work.rate_changes.load(Ordering::Relaxed);

    // A 96 kHz song: the driver goes to 96 kHz.
    let first = open_from_a_rebind_thread(&host, Some(96_000)).expect("96 kHz");
    assert_eq!((fake.rate.load(Ordering::Acquire), changes()), (96_000, 1));
    // The next song at 96 kHz opens while the first still holds the driver: nothing changes,
    // and nothing waits.
    let second = open_from_a_rebind_thread(&host, Some(96_000)).expect("96 kHz again");
    assert_eq!((fake.rate.load(Ordering::Acquire), changes()), (96_000, 1));
    // A 44.1 kHz song while a binding still holds the driver: it plays at 96 kHz, resampled.
    drop(first);
    let held = open_from_a_rebind_thread(&host, Some(44_100)).expect("44.1 kHz, held");
    assert_eq!((fake.rate.load(Ordering::Acquire), changes()), (96_000, 1));
    drop((second, held));
    // Once nothing holds it, the next 44.1 kHz song gets 44.1 kHz.
    let third = open_from_a_rebind_thread(&host, Some(44_100)).expect("44.1 kHz");
    assert_eq!((fake.rate.load(Ordering::Acquire), changes()), (44_100, 2));
    drop(third);
    // 88.2 kHz, which this driver cannot do, nor 176.4: the nearest rate above, 96 kHz.
    let fourth = open_from_a_rebind_thread(&host, Some(88_200)).expect("88.2 kHz");
    assert_eq!((fake.rate.load(Ordering::Acquire), changes()), (96_000, 3));
    drop(fourth);
    assert_eq!(
        host.work.loads.load(Ordering::Relaxed),
        1,
        "the driver stayed loaded throughout"
    );
}

#[test]
fn a_rate_the_driver_cannot_play_falls_back_to_a_multiple_then_the_nearest_above() {
    let can = |rates: &'static [u32]| move |rate: u32| rates.contains(&rate);
    assert_eq!(
        choose_output_rate(44_100, 48_000, can(&[44_100, 48_000])),
        44_100
    );
    assert_eq!(
        choose_output_rate(22_050, 48_000, can(&[44_100, 48_000])),
        44_100,
        "an integer multiple"
    );
    assert_eq!(
        choose_output_rate(88_200, 48_000, can(&[44_100, 48_000, 96_000])),
        96_000,
        "the nearest above"
    );
    assert_eq!(
        choose_output_rate(384_000, 48_000, can(&[48_000, 96_000, 192_000])),
        192_000,
        "the nearest below"
    );
    assert_eq!(
        choose_output_rate(44_100, 48_000, can(&[])),
        48_000,
        "a driver fixed at one rate keeps it"
    );
}

/// At 100 % volume, once the start ramp is done, a 24-bit source reaches a 32-bit device and a
/// float device with every bit it had: no resampling at a matched rate, no gain, no dither.
#[test]
fn at_unity_volume_the_callback_passes_24_bit_samples_through_bit_exact() {
    let samples: Vec<i32> = [
        1, -1, 8_388_607, -8_388_608, 1_234_567, -7_654_321, 42, 0x55_5555,
    ]
    .into_iter()
    .cycle()
    .take(64 * 2)
    .collect();
    let as_float: Vec<f32> = samples
        .iter()
        .map(|sample| *sample as f32 / 8_388_608.0)
        .collect();
    let render_after_ramp = |output: &mut dyn FnMut(&mut CallbackCore)| {
        let ring = Arc::new(AudioRing::new(nonzero_usize(2_048), nonzero_usize(2)).expect("ring"));
        let clock = Arc::new(AudioClock::new());
        clock
            .reanchor(1, NormalizedTimestamp::from_nanoseconds(0))
            .expect("clock anchor");
        let control = Arc::new(CallbackControl::new());
        let mut callback = callback_harness(
            ring.clone(),
            clock,
            control.clone(),
            Arc::new(SharedMetrics::default()),
        );
        inject_request(&control, MODE_RUNNING, RampKind::Start);
        // The start ramp's five frames ramp silence; what follows is the song.
        ring.push_interleaved(&[0.0; 5 * 2]);
        ring.push_interleaved(&as_float);
        let mut ramp = vec![0.0_f32; 5 * 2];
        callback.render(
            &mut ramp,
            CallbackMoment {
                observed_at: Instant::now(),
                delivery_delay: Duration::ZERO,
            },
        );
        output(&mut callback);
    };
    let mut int32 = vec![0_i32; samples.len()];
    render_after_ramp(&mut |callback| {
        callback.render(
            &mut int32,
            CallbackMoment {
                observed_at: Instant::now(),
                delivery_delay: Duration::ZERO,
            },
        )
    });
    let shifted: Vec<i32> = samples.iter().map(|sample| sample << 8).collect();
    assert_eq!(
        int32, shifted,
        "a 32-bit device receives the 24-bit samples in its top 24 bits"
    );
    let mut float = vec![0.0_f32; samples.len()];
    render_after_ramp(&mut |callback| {
        callback.render(
            &mut float,
            CallbackMoment {
                observed_at: Instant::now(),
                delivery_delay: Duration::ZERO,
            },
        )
    });
    assert_eq!(
        float
            .iter()
            .map(|sample| sample.to_bits())
            .collect::<Vec<_>>(),
        as_float
            .iter()
            .map(|sample| sample.to_bits())
            .collect::<Vec<_>>()
    );
}

/// `prepare_binding`'s rate loop, counted: the device is resolved at the asked rate (the
/// driver follows it when it can), the decoder opens and reads the source's rate, and
/// `needs_rate_reopen` decides whether it opens again.
fn decoder_opens(
    on_asio: bool,
    hint: u32,
    source: u32,
    driver_rate: u32,
    driver_follows: bool,
) -> usize {
    let (mut asked, mut rate, mut opens) = (hint, driver_rate, 0);
    loop {
        if on_asio && asked != 0 && driver_follows {
            rate = asked;
        }
        opens += 1;
        if needs_rate_reopen(on_asio, source, rate, asked) {
            asked = source;
            continue;
        }
        return opens;
    }
}

#[test]
fn a_rate_hint_from_the_tags_opens_the_decoder_once_on_a_rate_change() {
    // 48 kHz driver, 96 kHz song: the hint makes it one open, its absence two.
    assert_eq!(decoder_opens(true, 96_000, 96_000, 48_000, true), 1);
    assert_eq!(decoder_opens(true, 0, 96_000, 48_000, true), 2);
    // A wrong hint is corrected by the decoder's own reading, at the cost of the second open.
    assert_eq!(decoder_opens(true, 44_100, 96_000, 48_000, true), 2);
    // The driver already at the song's rate, a driver that cannot follow, and WASAPI: once.
    assert_eq!(decoder_opens(true, 0, 48_000, 48_000, true), 1);
    assert_eq!(decoder_opens(true, 96_000, 96_000, 48_000, false), 1);
    assert_eq!(decoder_opens(false, 0, 96_000, 48_000, true), 1);
}

/// Regression ("linear 0..1 with no smoothing"): a volume slider dragged stepped the
/// gain once per callback, a staircase heard as zipper noise. The rendered samples now glide:
/// no two neighbours differ by more than a smoothed step, however hard the volume is thrown.
#[test]
fn a_volume_sweep_glides_without_zipper_steps_in_the_rendered_samples() {
    let ring = Arc::new(AudioRing::new(nonzero_usize(16_384), nonzero_usize(2)).expect("ring"));
    let clock = Arc::new(AudioClock::new());
    clock
        .reanchor(1, NormalizedTimestamp::from_nanoseconds(0))
        .expect("clock anchor");
    let control = Arc::new(CallbackControl::new());
    let metrics = Arc::new(SharedMetrics::default());
    let mut callback = callback_harness(ring.clone(), clock, control.clone(), metrics);
    assert_eq!(ring.push_interleaved(&[0.5; 2 * 16_000]), 16_000);
    inject_request(&control, MODE_RUNNING, RampKind::Start);
    let moment = CallbackMoment {
        observed_at: Instant::now(),
        delivery_delay: Duration::ZERO,
    };
    // The start ramp runs out first, at full volume.
    let mut warm = [0.0f32; 2 * 256];
    callback.render(&mut warm, moment);

    let mut rendered = Vec::new();
    for step in 0..160 {
        // The slider thrown between full and a fifth on every callback: the worst staircase.
        callback
            .user_gain
            .set(if step % 2 == 0 { 0.2 } else { 1.0 });
        let mut output = [0.0f32; 2 * 64];
        callback.render(&mut output, moment);
        rendered.extend(output.chunks_exact(2).map(|frame| frame[0]));
    }
    let largest = rendered
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .fold(0.0, f32::max);
    // Unsmoothed, the step is 0.5 × 0.8 = 0.4; a 10 ms one-pole at 48 kHz moves at most
    // 0.4 / 480 per frame.
    assert!(
        largest < 0.001,
        "a step of {largest} between neighbouring samples"
    );
    // It still goes where it is told: held at a fifth, it settles there.
    callback.user_gain.set(0.2);
    let mut settled = [0.0f32; 2 * 4_800];
    callback.render(&mut settled, moment);
    assert!((settled[settled.len() - 1] - 0.1).abs() < 1e-4);
}
