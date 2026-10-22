//! The device rebind ladder: what a lost device does, how long it waits before each attempt,
//! and the promise a rebind has to honour.

use super::callback::CallbackControl;
use super::metrics::SharedMetrics;
use crate::NormalizedTimestamp;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Everything the cpal error callback needs. It runs on a real-time thread, so every field here
/// must be writable without allocating or taking a lock.
#[derive(Clone)]
pub(super) struct StreamFaultSink {
    pub(super) control: Arc<CallbackControl>,
    pub(super) metrics: Arc<SharedMetrics>,
    pub(super) device_faults: Arc<AtomicU64>,
    pub(super) wake_render: Arc<dyn Fn() + Send + Sync>,
}

impl StreamFaultSink {
    /// The binding failed, not the session. Publishes the edge and wakes the host: the media clock
    /// is about to freeze, so nothing else would wake it.
    pub(super) fn report(&self) {
        self.metrics.output_errors.fetch_add(1, Ordering::Relaxed);
        self.device_faults.fetch_add(1, Ordering::AcqRel);
        self.control.fail();
        (self.wake_render)();
    }
}

/// How long a lost device may stay lost before the session gives up. The budget is wall-clock,
/// not attempts, because what we are waiting for is the operating system publishing a
/// replacement endpoint — an event that has a duration, not a count. Ten seconds covers every
/// measured case: a jack unplug switches the default endpoint in well under a second, a Bluetooth
/// reconnection in a few, an exclusive-mode holder releasing the device in a few more.
pub(super) const DEVICE_RECOVERY_WINDOW: Duration = Duration::from_secs(10);

/// The largest gap tolerated between the media position the transport promised and the position
/// the rebound decoder actually anchored at. `DecoderPump::begin_generation` discards every frame
/// before its target, so a binding can only anchor at or after the promise, and only part of one
/// decoded block late.
pub(super) const REBIND_ANCHOR_TOLERANCE: Duration = Duration::from_millis(120);

/// How long the machine waits before the next attempt, as a function of how long the device has
/// already been gone. The first attempt is immediate: by the time cpal reports the fault, Windows
/// has often already published the replacement endpoint.
pub(super) fn rebind_backoff(elapsed: Duration) -> Duration {
    if elapsed < Duration::from_millis(500) {
        Duration::from_millis(150)
    } else if elapsed < Duration::from_secs(2) {
        Duration::from_millis(400)
    } else {
        Duration::from_secs(1)
    }
}

/// The postcondition every rebind must satisfy, and the detector for the invariant the device
/// machine shares with the seek transaction: the new decoder anchored where the transport
/// promised. A late anchor means the promised position is not where this binding will play;
/// adopting it silently would leave a device lost during a committed seek playing from before the
/// seek while the transport shows its target.
pub(super) fn rebind_honours_promise(promised: Duration, anchor: NormalizedTimestamp) -> bool {
    let promised = NormalizedTimestamp::from_position(promised).as_nanoseconds();
    let anchor = anchor.as_nanoseconds();
    anchor >= promised
        && anchor.saturating_sub(promised) <= REBIND_ANCHOR_TOLERANCE.as_nanos() as i128
}

/// The lifecycle of the binding between this output and one concrete device.
///
/// The ladder is a *timeline*, not a counter. `since` is when the device was lost; every attempt
/// is scheduled at an `Instant`; the whole recovery has one deadline, `since +
/// DEVICE_RECOVERY_WINDOW`. "Three attempts inside one frame" is therefore unrepresentable, and
/// so is "retry forever" — the previous shape made the second impossible by making the first
/// inevitable, which is why a real unplug always ended the session.
///
/// No variant carries a media position. Where a new binding resumes is asked of the transport at
/// the instant the attempt is issued, so the device machine and an in-flight seek transaction
/// cannot hold two different answers about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum AudioDevicePhase {
    /// A live cpal stream exists on `device`.
    Bound { device: String },
    /// No stream exists and no attempt is running. The next one comes due at `next_attempt_at`.
    Waiting {
        since: Instant,
        next_attempt_at: Instant,
    },
    /// An attempt is running off the media loop. The loop owes it nothing but a poll.
    Attempting { since: Instant },
    /// The recovery window closed with no device. The only phase that fails the session.
    Lost { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum AudioDeviceEvent {
    /// The stream error callback fired: unplug, default-device teardown, exclusive-mode seizure.
    /// A future `IMMNotificationClient` watcher adds one more producer and nothing else.
    BindingLost,
    /// One attempt was handed to a rebind thread.
    AttemptStarted,
    /// An attempt produced a live stream on `device`.
    Rebound { device: String },
    /// An attempt failed, could not be started, or was refused because it did not honour the
    /// transport's promised position.
    AttemptFailed { reason: String },
    /// The recovery window closed.
    GaveUp { reason: String },
}

/// What one instant owes the device machine. Total over the phase, so there is no "nothing
/// described this state" branch, and no duty can be produced twice without an intervening edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DeviceDuty {
    /// A live binding, or a wait that has not come due.
    Idle,
    /// Issue exactly one rebind attempt.
    Attempt,
    /// The recovery window closed. The only producer of `Lost` outside teardown.
    GiveUp { reason: String },
}

impl AudioDevicePhase {
    pub(super) fn step(self, event: AudioDeviceEvent, now: Instant) -> Self {
        match event {
            AudioDeviceEvent::BindingLost => match self {
                Self::Bound { .. } => Self::Waiting {
                    since: now,
                    next_attempt_at: now,
                },
                // A second loss report for a binding that is already gone is not a new failure,
                // and must not restart the recovery window.
                Self::Waiting { .. } | Self::Attempting { .. } | Self::Lost { .. } => self,
            },
            AudioDeviceEvent::AttemptStarted => match self {
                Self::Waiting { since, .. } => Self::Attempting { since },
                Self::Bound { .. } | Self::Attempting { .. } | Self::Lost { .. } => self,
            },
            AudioDeviceEvent::Rebound { device } => match self {
                // A binding that lands after the session gave up is not adopted; dropping it
                // shuts its worker down.
                Self::Lost { .. } => self,
                Self::Bound { .. } | Self::Waiting { .. } | Self::Attempting { .. } => {
                    Self::Bound { device }
                }
            },
            AudioDeviceEvent::AttemptFailed { reason } => match self {
                // One arm for both recovering phases. An attempt that could not even be started —
                // a refused `thread::Builder::spawn`, a closed order channel — is still a failure
                // of *this* recovery, and it costs the same backoff as one that ran and came back
                // empty. Which phase the failure is reported from is not this machine's business;
                // that it was reported while a recovery is in progress is. While `Waiting` had its
                // own silent arm here, such a failure left `next_attempt_at` in the past, so
                // `duty` returned `Attempt` again on the very next frame and the loop retried at
                // display cadence for the whole ten-second window, advancing a decode generation
                // each time.
                //
                // `since` is the fixed point: a failed start is not a new loss, so the recovery
                // deadline does not move. An attempt only issues when `now >= next_attempt_at`, so
                // `now + rebind_backoff(..)` is strictly later than the schedule it replaces —
                // this arm can only push the ladder forward, never back.
                Self::Waiting { since, .. } | Self::Attempting { since } => {
                    if now >= recovery_deadline(since) {
                        Self::Lost { reason }
                    } else {
                        Self::Waiting {
                            since,
                            next_attempt_at: now
                                + rebind_backoff(now.saturating_duration_since(since)),
                        }
                    }
                }
                // Never touches the binding: an attempt only exists while there is none, so a
                // `Bound` phase keeps both its phase and its stream. `Lost` is terminal.
                Self::Bound { .. } | Self::Lost { .. } => self,
            },
            AudioDeviceEvent::GaveUp { reason } => match self {
                Self::Waiting { .. } | Self::Attempting { .. } => Self::Lost { reason },
                Self::Bound { .. } | Self::Lost { .. } => self,
            },
        }
    }

    /// Pure: the same phase and the same instant always produce the same duty.
    pub(super) fn duty(&self, now: Instant) -> DeviceDuty {
        match self {
            Self::Bound { .. } | Self::Lost { .. } => DeviceDuty::Idle,
            Self::Attempting { since } | Self::Waiting { since, .. }
                if now >= recovery_deadline(*since) =>
            {
                DeviceDuty::GiveUp {
                    reason: "no audio output device accepted a binding within the recovery window"
                        .to_owned(),
                }
            }
            Self::Attempting { .. } => DeviceDuty::Idle,
            Self::Waiting {
                next_attempt_at, ..
            } => {
                if now >= *next_attempt_at {
                    DeviceDuty::Attempt
                } else {
                    DeviceDuty::Idle
                }
            }
        }
    }

    /// The instant at which this phase next owes the media loop a duty — the dual of `duty`.
    ///
    /// `duty(t)` is `Idle` for every `t` strictly before this instant and is not `Idle` at it, so
    /// the pair is the whole of the ladder's clock: the machine says when it needs the loop, and
    /// the host's deadline fold parks until then. Without it a paused player, which has no other
    /// reason to wake, would let a ten-second recovery window close while it slept.
    ///
    /// The recovery deadline is part of the answer, not just the attempt schedule: a `Waiting`
    /// whose backoff falls past `since + DEVICE_RECOVERY_WINDOW` owes the give-up first, and a
    /// wedged `Attempting` owes nothing *but* the give-up. `min` belongs to the machine, not to
    /// the caller.
    ///
    /// `None` is the steady state: a bound device publishes no deadline at all, so ordinary
    /// playback folds exactly the terms it folded before this term existed.
    pub(super) fn wake_at(&self) -> Option<Instant> {
        match self {
            Self::Bound { .. } | Self::Lost { .. } => None,
            Self::Attempting { since } => Some(recovery_deadline(*since)),
            Self::Waiting {
                since,
                next_attempt_at,
            } => Some((*next_attempt_at).min(recovery_deadline(*since))),
        }
    }
}

/// The whole machine's wake: the phase's own, plus the edge the phase has not seen yet.
///
/// `loss_pending` is published by cpal's error callback on its own thread, as a bump of
/// `AudioSession::device_faults`. Until a `service_device` observes it, the phase still reads
/// `Bound`, and `Bound` answers "never" — so the phase alone would tell a paused render loop to
/// sleep through a device it has already lost. A loss already on the wire is owed *now*, which
/// makes the one frame that turns it into `Waiting` independent of whether the callback's wake
/// notification survived.
///
/// It is a free function so it can be tested against the phase alone, with no `AudioOutput` and
/// therefore no audio device.
pub(super) fn device_wake_at(
    phase: &AudioDevicePhase,
    loss_pending: bool,
    now: Instant,
) -> Option<Instant> {
    if loss_pending {
        return Some(now);
    }
    phase.wake_at()
}

/// The device phase, and nothing else.
///
/// The binding lives on the audio-owner thread; this ladder is a FOLD over records that thread
/// published, plus a clock, so it cannot write a stream into a phase that has already given up.
pub(super) struct AudioDeviceLadder {
    pub(super) phase: AudioDevicePhase,
}

impl AudioDeviceLadder {
    pub(super) fn bound(device: String) -> Self {
        Self {
            phase: AudioDevicePhase::Bound { device },
        }
    }

    pub(super) fn phase(&self) -> &AudioDevicePhase {
        &self.phase
    }

    pub(super) fn step(&mut self, event: AudioDeviceEvent, now: Instant) {
        self.phase = self.phase.clone().step(event, now);
    }

    pub(super) fn lost(&mut self, now: Instant) {
        self.step(AudioDeviceEvent::BindingLost, now);
    }

    pub(super) fn attempt_started(&mut self, now: Instant) {
        self.step(AudioDeviceEvent::AttemptStarted, now);
    }

    pub(super) fn rebound(&mut self, device: String, now: Instant) {
        self.step(AudioDeviceEvent::Rebound { device }, now);
    }

    pub(super) fn attempt_failed(&mut self, reason: String, now: Instant) {
        self.step(AudioDeviceEvent::AttemptFailed { reason }, now);
    }

    pub(super) fn gave_up(&mut self, reason: String, now: Instant) {
        self.step(AudioDeviceEvent::GaveUp { reason }, now);
    }

    pub(super) fn disposed(&mut self) {
        self.phase = AudioDevicePhase::Lost {
            reason: "audio output disposed".to_owned(),
        };
    }
}

/// When a device lost at `since` is given up on: the recovery window closes.
fn recovery_deadline(since: Instant) -> Instant {
    since + DEVICE_RECOVERY_WINDOW
}
