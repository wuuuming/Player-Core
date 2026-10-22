//! The handle the media loop holds: every call on it posts an order or reads a published
//! value, and none of them waits on the device.

use super::binding::prepare_binding;
use super::clock::{
    GenerationCounter, MediaClockSnapshot, PlaybackRateControl, FIRST_GENERATION,
    PLAYBACK_RATE_SCALE,
};
use super::device::{asio_in_use, output_route_version, wake_on_move, AudioDeviceSelection};
use super::error::{AudioError, AudioOpenError};
use super::follow::{spawn_next, RingShape};
use super::lease::{OutputAuthorityLease, OUTPUT_AUTHORITY_WAIT};
use super::metrics::AudioPlaybackMetrics;
use super::owner::AudioOwner;
use super::player::{AudioNext, AudioOpenOptions};
use super::policy::AudioPolicy;
use super::posture::{AudioOutputPhase, PostureCell, TransportPosture};
use super::rebind::{
    device_wake_at, AudioDeviceLadder, AudioDevicePhase, DeviceDuty, DEVICE_RECOVERY_WINDOW,
};
use super::seek::{promised_position, SeekHoldTransaction, UnadoptedSeek};
use super::session::{
    AudioEdge, AudioOrder, AudioSession, AudioSource, BoundView, Followed, SourceEvent, StopBound,
};
use super::track::{requested_stream_index, AudioStreamInfo};
use super::worker::AudioWorkerStart;
use super::{AudioEdges, AudioOrders};
use crate::{AudioSourceFactory, ReplayGain};
use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The least [`AudioOutput::played_out_at`] looks ahead. The device takes the ring a period at a
/// time, so a look at the computed end can find part of a period still there; this bounds how
/// often it looks again until the device has taken it.
pub(super) const TAIL_FLOOR: Duration = Duration::from_millis(5);

pub(crate) struct AudioOutput {
    pub(super) session: AudioSession,
    /// The media loop's post-only order sender and take-only edge receiver. Between them they are
    /// the whole of what the media loop can do to, or learn about, an audio device.
    pub(super) orders: AudioOrders,
    pub(super) edges: AudioEdges,
    pub(super) posture: Arc<PostureCell>,
    pub(super) owner: Option<JoinHandle<()>>,
    /// Shared with the audio-owner thread. Whoever drops last releases the process-wide output
    /// authority, so "the authority was released while a stream still lives" is unconstructible
    /// even when the owner thread is wedged inside a WASAPI call — except by `yield_output`,
    /// which is a parked session handing the device on with its stream paused.
    pub(super) lease: Option<Arc<OutputAuthorityLease>>,
    pub(super) info: AudioStreamInfo,
    pub(super) policy: AudioPolicy,
    pub(super) device: AudioDeviceLadder,
    pub(super) bound: Option<BoundView>,
    pub(super) observed_device_faults: u64,
    /// The output-host version the live binding was resolved under. Behind the process's version, the
    /// binding is on the host ASIO was just switched away from (or onto) and rebinds.
    pub(super) observed_route_version: u64,
    pub(super) worker_failure: Option<AudioError>,
    pub(super) rebind_failure: Option<Arc<AudioOpenError>>,
    pub(super) generation: GenerationCounter,
    pub(super) phase: AudioOutputPhase,
    pub(super) seek_transaction: Option<SeekHoldTransaction>,
    /// A target `finish_seek` committed that no decode worker has anchored on yet. Never cleared:
    /// `promised_position` retires it by comparing it against the media clock's own anchor.
    pub(super) unadopted_seek: Option<UnadoptedSeek>,
    /// The song the host wants to follow this one, with the serial it was given: kept until the
    /// sound has gone on into it, and opened again for a binding that lands meanwhile.
    pub(super) next: Option<(u64, AudioNext)>,
    /// The last serial given to a next song.
    pub(super) next_serial: u64,
    /// Boundaries the decode worker crossed that the clock has not reached yet, in order.
    pub(super) boundaries: VecDeque<Followed>,
    /// The id of the song the sound last went on into (`AudioPlayer::followed`).
    pub(super) followed: Option<u64>,
    /// The playing song's length, as the host's tags probe read it: the opened song's, then each
    /// followed song's from its boundary on.
    pub(super) duration: Option<Duration>,
    /// Where the playing song's own time is zero on the clock of a generation: the clock stays on
    /// the time of the generation's first song across a boundary. `(generation, nanoseconds)`;
    /// `None`, or another generation, is zero.
    pub(super) song_origin: Option<(u64, i128)>,
}

impl AudioOutput {
    /// [`Self::open`], told the source's own sample rate by a host that has read it already (its
    /// tag probe). An ASIO driver is asked for that rate before the decoder opens, so a song at a
    /// new rate opens its decoder once; a wrong hint is corrected by the decoder's own reading.
    pub(crate) fn open_hinted(
        factory: Arc<dyn AudioSourceFactory>,
        device_selection: AudioDeviceSelection,
        options: AudioOpenOptions,
        wake_render: Arc<dyn Fn() + Send + Sync>,
        playback_rate: Arc<PlaybackRateControl>,
    ) -> Result<Self, AudioOpenError> {
        let attempt = || {
            Self::open_once(
                Arc::clone(&factory),
                device_selection.clone(),
                options.clone(),
                Arc::clone(&wake_render),
                Arc::clone(&playback_rate),
            )
        };
        let asked_at = Instant::now();
        loop {
            let on_asio = asio_in_use();
            match attempt() {
                // The song before still lets go of the ASIO driver: each attempt has waited the
                // idle wait for it; asked again for as long as a lost device is waited for.
                Err(AudioOpenError::AsioStillHeld)
                    if asked_at.elapsed() < DEVICE_RECOVERY_WINDOW =>
                {
                    continue
                }
                Err(error) if on_asio && !asio_in_use() => {
                    #[cfg(feature = "instrument")]
                    eprintln!(
                        "player-core[audio] reopening on WASAPI after the ASIO open failed: {error}"
                    );
                    #[cfg(not(feature = "instrument"))]
                    let _ = error;
                    return attempt();
                }
                opened => return opened,
            }
        }
    }

    fn open_once(
        factory: Arc<dyn AudioSourceFactory>,
        device_selection: AudioDeviceSelection,
        options: AudioOpenOptions,
        wake_render: Arc<dyn Fn() + Send + Sync>,
        playback_rate: Arc<PlaybackRateControl>,
    ) -> Result<Self, AudioOpenError> {
        let AudioOpenOptions {
            track,
            policy,
            source_rate,
            replay_gain,
            effects,
            duration,
        } = options;
        let source = AudioSource {
            factory,
            // `Default` is the native `-1` auto-pick; an explicit stream is carried as asked and
            // validated by the decoder that opens it. Resolved ONCE, here, so every later rebind
            // re-opens the same track by cloning this value.
            stream_index: requested_stream_index(track),
            source_rate: Arc::new(AtomicU32::new(source_rate.unwrap_or(0))),
        };
        let (edge_tx, edge_rx) = mpsc::channel();
        // A move of the outputs — the listener's, or Windows' — wakes this session's media loop
        // to see it, whether or not anything else would.
        wake_on_move(&wake_render);
        // The session, and with it the clock, exists before any device is touched, so no bind can
        // create a second one.
        let mut session = AudioSession::open(wake_render, playback_rate, edge_tx);
        session.effects = effects;
        // The song's end, as the host knows it ahead: where a crossfade into the next one starts.
        session.stop = Arc::new(StopBound::new(None, duration));
        // Before the first binding: its prefill is already processed at the setting.
        session.replay_gain.set(replay_gain);
        let start = AudioWorkerStart {
            generation: FIRST_GENERATION,
            position: None,
            track: 0,
        };
        session.metrics.step_source(SourceEvent::Open);
        // Preparing the first binding on the caller's thread is what lets `open` fail fast: a song
        // that cannot play fails its open, not a later service. It is not the media loop's steady
        // state.
        let prepared = prepare_binding(&source, &device_selection, policy, &session, start)
            .inspect_err(|error| session.metrics.fail_source(error))?;
        // The output authority guards the DEVICE, so it is taken only now that the network half of
        // the open is done: an open abandoned mid-download (a song skipped before its sound arrived)
        // must not sit on the output while it finishes. The previous session's owner thread may
        // still be stopping its stream — closing does not wait for it — so it is asked for a few
        // times over.
        let asked_at = Instant::now();
        let lease = loop {
            match OutputAuthorityLease::acquire() {
                Ok(lease) => break Arc::new(lease),
                Err(error) if asked_at.elapsed() >= OUTPUT_AUTHORITY_WAIT => return Err(error),
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        };
        let posture = Arc::new(PostureCell::new(TransportPosture {
            phase: AudioOutputPhase::Prepared,
            seeking: false,
        }));
        let (order_tx, order_rx) = mpsc::channel();
        // The stream must be built on the thread that will own it, so `open` hands the prepared
        // binding across and waits once for the commit. One `IAudioClient::Initialize`, at page
        // open, on the page-open thread.
        let (first_tx, first_rx) = mpsc::sync_channel(1);
        let owner_session = session.clone();
        let owner_posture = Arc::clone(&posture);
        let owner_orders = order_tx.clone();
        let owner_lease = Arc::clone(&lease);
        let owner = thread::Builder::new()
            .name("player-core-audio-owner".to_owned())
            .spawn(move || {
                let wanted = owner_posture.load();
                let (result_sender, results) = mpsc::channel();
                let mut owner = AudioOwner {
                    source,
                    track: 0,
                    selection: device_selection,
                    requested_policy: policy,
                    session: owner_session,
                    posture: owner_posture,
                    orders: owner_orders,
                    results,
                    result_sender,
                    binding: None,
                    applied: TransportPosture {
                        phase: AudioOutputPhase::Paused,
                        seeking: false,
                    },
                    lease: Some(owner_lease),
                    awaiting_landing: None,
                };
                match owner.commit(Box::new(prepared), wanted) {
                    Ok(landing) => {
                        let _ = first_tx.send(Ok(landing));
                    }
                    Err(error) => {
                        let _ = first_tx.send(Err(error));
                        return;
                    }
                }
                owner.run(order_rx);
            })
            .map_err(|error| {
                AudioOpenError::Worker(format!("could not start the audio owner thread: {error}"))
            })?;
        let landing = match first_rx.recv() {
            Ok(Ok(landing)) => {
                session.metrics.step_source(SourceEvent::Opened);
                landing
            }
            Ok(Err(error)) => {
                session.metrics.fail_source(&error);
                return Err(error);
            }
            Err(_) => {
                return Err(AudioOpenError::Worker(
                    "the audio owner thread stopped before it committed a binding".to_owned(),
                ))
            }
        };
        Ok(Self {
            session,
            orders: AudioOrders(order_tx),
            edges: AudioEdges(edge_rx),
            posture,
            owner: Some(owner),
            lease: Some(lease),
            info: landing.info,
            policy: landing.policy,
            device: AudioDeviceLadder::bound(landing.device),
            bound: Some(BoundView {
                worker: landing.worker,
                ring: landing.ring,
                ring_rate: landing.ring_rate,
            }),
            observed_device_faults: 0,
            observed_route_version: landing.route_version,
            worker_failure: None,
            rebind_failure: None,
            generation: GenerationCounter::new(start.generation),
            phase: AudioOutputPhase::Prepared,
            seek_transaction: None,
            unadopted_seek: None,
            next: None,
            next_serial: 0,
            boundaries: VecDeque::new(),
            followed: None,
            duration,
            song_origin: None,
        })
    }

    /// The transport's posture, published. Every transport method ends here, and this is the only
    /// writer of the posture cell.
    pub(super) fn publish_posture(&self) {
        let posture = TransportPosture {
            phase: self.phase,
            seeking: self.seek_transaction.is_some(),
        };
        self.posture.store(posture);
        self.orders.post(AudioOrder::Reconcile);
    }

    pub(super) fn media_position(&self) -> Duration {
        self.session
            .clock
            .snapshot()
            .map_or(Duration::ZERO, |snapshot| self.song_position(&snapshot))
    }

    /// Where in the playing song the clock stands, as a position: zero before its start, and
    /// `Duration::MAX` past what a `Duration` of nanoseconds holds, for every reader alike.
    fn song_position(&self, snapshot: &MediaClockSnapshot) -> Duration {
        u64::try_from(self.song_time(snapshot).max(0)).map_or(Duration::MAX, Duration::from_nanos)
    }

    /// Where in the playing song's own time the clock stands, in nanoseconds: the clock stays on
    /// the time of the generation's first song, and each boundary taken up moves the song's on.
    fn song_time(&self, snapshot: &MediaClockSnapshot) -> i128 {
        let origin = match self.song_origin {
            Some((generation, origin)) if generation == snapshot.generation => origin,
            _ => 0,
        };
        snapshot.media_now.as_nanoseconds() - origin
    }

    /// Where the transport promises audio is. The media clock is an *observation* of where audio
    /// got to; a held seek transaction is a *promise* about where it is going, and while one is
    /// open the clock still shows the old anchor. This is the only answer, and it is asked at the
    /// instant an attempt is issued rather than snapshotted at the loss edge, so a seek that
    /// arrives while the device is gone retargets the rebind with no extra state.
    pub(super) fn resume_position(&self) -> Duration {
        promised_position(
            self.seek_transaction,
            self.unadopted_seek,
            self.session.clock.generation(),
            self.media_position(),
        )
    }

    /// The position the host shows: the seek's target while one is promised, else the clock.
    /// `None` only when no seek is promised and the clock has never been anchored.
    pub(crate) fn promised_position(&self) -> Option<Duration> {
        let clock = self
            .session
            .clock
            .snapshot()
            .map(|snapshot| self.song_position(&snapshot));
        let promised = self.seek_unadopted();
        if clock.is_none() && !promised {
            return None;
        }
        Some(promised_position(
            self.seek_transaction,
            self.unadopted_seek,
            self.session.clock.generation(),
            clock.unwrap_or(Duration::ZERO),
        ))
    }

    /// The device ladder's only driver, and the whole of what a device costs the media loop:
    /// one atomic load, a drain of already-published edges, one pure `duty`, and at most one
    /// unbounded `send`. Nothing here can wait, and nothing here holds a `cpal::Stream`, a
    /// `WorkerLifetime`, or a `CallbackControl`.
    pub(crate) fn service_device(&mut self, now: Instant) -> Result<(), AudioError> {
        if self.phase.is_disposed() {
            return Ok(());
        }
        // A fault not yet folded, or a host switch: the device loss the listener asked for, the
        // same ladder, the same promised position. Only a bound output owes a switch; one
        // already rebinding takes the version its landing was resolved under, and a stale one is
        // caught here on the frame after.
        if let Some(faults) = self.loss_pending() {
            self.observed_device_faults = faults;
            self.bound = None;
            self.device.lost(now);
        }
        while let Some(edge) = self.edges.take() {
            match edge {
                AudioEdge::Landed(landing) => {
                    self.rebind_failure = None;
                    let landing = *landing;
                    self.info = landing.info;
                    self.policy = landing.policy;
                    self.bound = Some(BoundView {
                        worker: landing.worker,
                        ring: landing.ring,
                        ring_rate: landing.ring_rate,
                    });
                    self.observed_route_version = landing.route_version;
                    self.device.rebound(landing.device, now);
                    // The new binding starts in the song the old one had reached; the one after
                    // it is opened again for this ring.
                    self.boundaries.clear();
                    self.open_next();
                }
                AudioEdge::Followed(followed) => self.boundaries.push_back(*followed),
                AudioEdge::AttemptFailed(error) => {
                    self.device.attempt_failed(error.to_string(), now);
                    self.rebind_failure = Some(error);
                }
                AudioEdge::WorkerFailed(error) => {
                    // Follow-on owner/control failures must not overwrite the terminal cause.
                    self.worker_failure.get_or_insert(error);
                }
            }
        }
        self.take_up_boundaries();
        match self.device.phase().duty(now) {
            DeviceDuty::Idle => {}
            DeviceDuty::Attempt => {
                let promised = self.resume_position();
                let generation = self.generation.advance();
                // The phase enters `Attempting` in the same step that posts the order, so every
                // failure — including a refused thread spawn on the owner thread — is reported
                // from `Attempting` and applies backoff. There is no edge that reports a failure
                // while the phase is still `Waiting`.
                self.device.attempt_started(now);
                if !self.orders.post(AudioOrder::Attempt {
                    generation,
                    promised,
                }) {
                    self.rebind_failure = None;
                    self.device
                        .attempt_failed("the audio owner thread has stopped".to_owned(), now);
                }
            }
            DeviceDuty::GiveUp { reason } => {
                self.device.gave_up(reason, now);
                self.orders.post(AudioOrder::Abandon);
            }
        }
        match self.device.phase() {
            AudioDevicePhase::Lost { reason } => Err(match &self.rebind_failure {
                Some(cause) => AudioError::RebindFailed {
                    reason: reason.clone(),
                    cause: cause.clone(),
                },
                None => AudioError::DeviceLost(reason.clone()),
            }),
            AudioDevicePhase::Bound { .. }
            | AudioDevicePhase::Waiting { .. }
            | AudioDevicePhase::Attempting { .. } => self.check_health(),
        }
    }

    /// The device ladder's own deadline, for the host's next wake.
    ///
    /// `service_device` is this ladder's only driver, and the host calls it only when woken: a
    /// timeline whose next step happens only when someone else has an unrelated reason to call
    /// would never step. So the machine names the instant it needs the loop, and the host's
    /// deadline fold parks until exactly then.
    ///
    /// A disposed output and a bound device both answer `None`, so this costs the steady state
    /// nothing: while audio is healthy the render loop's fold is bit-for-bit what it was.
    pub(crate) fn next_device_wake_at(&self, now: Instant) -> Option<Instant> {
        if self.phase.is_disposed() {
            return None;
        }
        // The ladder no longer holds the binding, so the unseen loss edge is the session's fault
        // counter running ahead of what `service_device` last folded.
        device_wake_at(self.device.phase(), self.loss_pending().is_some(), now)
    }

    /// A device loss `service_device` has still to fold: a fault the session counted since, or a
    /// host switch the bound output owes. `Some`: the fault count to fold with it.
    fn loss_pending(&self) -> Option<u64> {
        let faults = self.session.device_faults.load(Ordering::Acquire);
        (faults != self.observed_device_faults || self.host_switch_pending()).then_some(faults)
    }

    fn host_switch_pending(&self) -> bool {
        matches!(self.device.phase(), AudioDevicePhase::Bound { .. })
            && (output_route_version() != self.observed_route_version
                || (self.info.volume_bypassed()
                    && !self.session.clock.playback_rate.at_normal_speed()))
    }

    pub(crate) fn stream_info(&self) -> &AudioStreamInfo {
        &self.info
    }

    pub(crate) fn policy(&self) -> AudioPolicy {
        self.policy
    }

    pub(crate) fn set_volume(&self, volume: f32) {
        self.session.user_gain.set(volume);
    }

    /// Takes effect at the next block the decoder processes, moved to without a step.
    pub(crate) fn set_replay_gain(&self, mode: ReplayGain) {
        self.session.replay_gain.set(mode);
    }

    pub(crate) fn replay_gain(&self) -> ReplayGain {
        self.session.replay_gain.get()
    }

    pub(crate) fn volume(&self) -> f32 {
        self.session.user_gain.get()
    }

    pub(crate) fn resume(&mut self) -> Result<(), AudioError> {
        self.activate()
    }

    pub(crate) fn pause(&mut self) -> Result<(), AudioError> {
        self.ensure_live()?;
        if self.phase.is_running() {
            self.phase = self.phase.held();
            self.publish_posture();
        }
        Ok(())
    }

    /// The single entry into audible playback. The ramp, the hardware play and the decoder unpark
    /// are no longer chosen here: they are `posture_step`'s answer to this posture edge, applied
    /// on the audio-owner thread. What this function does is publish, which cannot fail and
    /// cannot wait.
    pub(super) fn activate(&mut self) -> Result<(), AudioError> {
        self.ensure_live()?;
        if !self.phase.is_running() {
            self.phase = self.phase.after_activation();
            self.publish_posture();
        }
        Ok(())
    }

    /// Holds callback delivery once and preserves that transaction across superseding seeks.
    ///
    /// A newer seek can therefore replace an in-flight exact preroll without briefly
    /// ramping old audio back up. Each replacement still receives a new decoder/clock generation,
    /// and each moves the promise, which is what a pending rebind will start from.
    pub(crate) fn begin_seek_hold(&mut self, position: Duration) -> Result<bool, AudioError> {
        self.ensure_live()?;
        if let Some(transaction) = self.seek_transaction.as_mut() {
            transaction.set_target(position);
            let resumes_running = transaction.resumes_running();
            self.generation.advance();
            self.phase = self.phase.held();
            self.publish_posture();
            return Ok(resumes_running);
        }
        let transaction =
            SeekHoldTransaction::begin(self.phase, self.session.park.load(), position);
        self.phase = self.phase.held();
        self.generation.advance();
        self.seek_transaction = Some(transaction);
        self.publish_posture();
        Ok(transaction.resumes_running())
    }

    /// `seek_held` for the media loop: never waited for. The seek goes to the owner thread, which
    /// fades an audible sound out before it hands the target to the worker (`AudioOwner::seek`).
    pub(crate) fn seek_held_posted(&mut self, position: Duration) -> Result<(), AudioError> {
        self.ensure_live()?;
        self.session
            .metrics
            .seeks_requested
            .fetch_add(1, Ordering::Relaxed);
        let transaction = self
            .seek_transaction
            .as_mut()
            .ok_or(AudioError::SeekNotHeld)?;
        transaction.set_target(position);
        let generation = self.generation.current();
        if self.bound.is_none() {
            return Ok(());
        }
        // Before the posture that ends the hold is published, so the owner thread that reads that
        // posture already knows what the sound is waiting for.
        self.session
            .awaited_generation
            .store(generation, Ordering::Release);
        if self.orders.post(AudioOrder::Seek {
            position,
            generation,
        }) {
            Ok(())
        } else {
            Err(AudioError::WorkerStopped)
        }
    }

    pub(crate) fn finish_seek(&mut self, resume: bool) -> Result<(), AudioError> {
        self.ensure_live()?;
        let Some(transaction) = self.seek_transaction else {
            return Err(AudioError::SeekNotHeld);
        };
        self.phase = self.phase.after_seek(resume);
        // Removing the snapshot commits the held-state transaction. The target does not go with
        // it: committing is the TRANSPORT's statement, and until a decode worker anchors this
        // generation the media clock still describes the anchor before the seek.
        self.unadopted_seek = Some(UnadoptedSeek {
            generation: self.generation.current(),
            target: transaction.target(),
        });
        self.seek_transaction = None;
        self.publish_posture();
        Ok(())
    }

    pub(crate) fn rollback_seek(&mut self) -> Result<(), AudioError> {
        self.ensure_live()?;
        let Some(transaction) = self.seek_transaction else {
            return Ok(());
        };
        let (phase, _park) = transaction.rollback(&mut self.generation);
        self.phase = phase;
        self.seek_transaction = None;
        self.publish_posture();
        Ok(())
    }

    /// The decode worker is waiting on the network for sound it needs now.
    pub(crate) fn input_starved(&self) -> bool {
        self.session.metrics.input_starved.load(Ordering::Acquire)
    }

    /// The song to go on into when this one ends; `None` takes back the one set before. It is
    /// opened ahead for the live binding's ring.
    pub(crate) fn set_next(&mut self, next: Option<AudioNext>) {
        self.next = next.map(|next| {
            self.next_serial += 1;
            (self.next_serial, next)
        });
        self.open_next();
    }

    pub(crate) fn followed(&self) -> Option<u64> {
        self.followed
    }

    /// The playing song's length, when the host's probe knew it.
    pub(crate) fn duration(&self) -> Option<Duration> {
        self.duration
    }

    /// Where the playing song's sound ends, as known ahead: its stop, else its length.
    pub(crate) fn end(&self) -> Option<Duration> {
        self.session.stop.end()
    }

    /// Opens the next song ahead for the live binding's ring, or tells the decode worker there is
    /// none to follow.
    fn open_next(&self) {
        self.publish_next_wanted();
        let Some(bound) = self.bound.as_ref() else {
            return;
        };
        bound.worker.thread.unpark();
        let Some((serial, next)) = self.next.clone() else {
            return;
        };
        let Some(channels) = NonZeroUsize::new(usize::from(self.info.output_channels)) else {
            return;
        };
        let shape = RingShape {
            info: self.info.clone(),
            sample_rate: bound.ring_rate,
            channels,
        };
        spawn_next(
            next,
            serial,
            shape,
            self.session.clone(),
            bound.worker.clone(),
        );
    }

    /// Takes up the boundaries the clock has reached: from there on, the song after each is the
    /// one playing. Boundaries of a generation the clock has left are dropped.
    fn take_up_boundaries(&mut self) {
        let Some(snapshot) = self.session.clock.snapshot() else {
            return;
        };
        while let Some(boundary) = self.boundaries.front() {
            if boundary.generation != snapshot.generation {
                if boundary.generation < snapshot.generation {
                    self.boundaries.pop_front();
                    continue;
                }
                return;
            }
            if snapshot.media_now.as_nanoseconds() < boundary.at.as_nanoseconds() {
                return;
            }
            let Some(boundary) = self.boundaries.pop_front() else {
                return;
            };
            self.song_origin = Some((
                boundary.generation,
                boundary.at.as_nanoseconds() - boundary.start.as_nanoseconds(),
            ));
            self.info = boundary.info;
            self.session.stop = boundary.stop.clone();
            self.followed = Some(boundary.id);
            self.duration = boundary.duration;
            if self
                .next
                .as_ref()
                .is_some_and(|(serial, _)| *serial == boundary.track)
            {
                self.next = None;
                self.publish_next_wanted();
            }
            self.orders.post(AudioOrder::Follow {
                source: Box::new(boundary.source),
                stop: boundary.stop,
                track: boundary.track,
            });
        }
    }

    /// When the clock reaches the next boundary the decode worker crossed, for a host to take it
    /// up then (`service_device`).
    pub(crate) fn boundary_at(&self, now: Instant) -> Option<Instant> {
        let boundary = self.boundaries.front()?;
        let snapshot = self.session.clock.snapshot()?;
        if boundary.generation != snapshot.generation {
            return None;
        }
        let left = (boundary.at.as_nanoseconds() - snapshot.media_now.as_nanoseconds()).max(0);
        let rate = u128::from(self.session.clock.playback_rate.q32()).max(1);
        let nanos = (left as u128 * u128::from(PLAYBACK_RATE_SCALE)).div_ceil(rate);
        Some(now + Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX)))
    }

    /// Stops the sound at `stop` of its container instead of at its end, or lets it run to the
    /// end again. The decode worker is woken to look: a bound moved on past where it stood plays
    /// on from the frame it stopped at.
    pub(crate) fn set_stop(&self, stop: Option<Duration>) {
        self.session.stop.set(stop);
        if let Some(bound) = self.bound.as_ref() {
            bound.worker.thread.unpark();
        }
    }

    /// The sound's container has nothing more to deliver.
    pub(crate) fn input_ended(&self) -> bool {
        self.session.metrics.input_ended.load(Ordering::Acquire)
    }

    /// The decode worker learns which next song is wanted (`self.next`'s serial, 0 for none).
    fn publish_next_wanted(&self) {
        let serial = self.next.as_ref().map_or(0, |(serial, _)| *serial);
        self.session.next_wanted.store(serial, Ordering::Release);
    }

    /// A seek is held, or was committed and no decode worker has anchored on it yet: the clock
    /// and the end-of-stream record still describe the anchor BEFORE it.
    pub(crate) fn seek_unadopted(&self) -> bool {
        self.seek_transaction.is_some()
            || self
                .unadopted_seek
                .is_some_and(|unadopted| unadopted.pending(self.session.clock.generation()))
    }

    /// The decoder's end is published and counts: no seek is still landing (whose end record
    /// would be the place it left).
    pub(crate) fn end_decoded(&self) -> bool {
        !self.seek_unadopted() && self.session.metrics.eof_reached.load(Ordering::Acquire)
    }

    /// The sound has ended and the device has played all of it. The decoder's end alone is not
    /// enough: the ring can still hold frames, and the clock advances while it drains. Both facts
    /// come from the platform's own records (the end flag and the ring's two frame counters).
    pub(crate) fn eof_drained(&self) -> bool {
        self.session.metrics.eof_reached.load(Ordering::Acquire)
            && self
                .bound
                .as_ref()
                .is_none_or(|bound| bound.ring.available_frames() == 0)
    }

    /// While the end is decoded and the device has not played it out yet (the one stretch where
    /// [`Self::eof_drained`] turns true with nothing to announce it): when it will have, from the
    /// frames left in the ring at the device's rate and the playback rate, and never sooner than
    /// [`TAIL_FLOOR`] from `now`. `None` otherwise, and while a seek is landing, whose end record
    /// still describes the place it left.
    pub(crate) fn played_out_at(&self, now: Instant) -> Option<Instant> {
        if !self.end_decoded() {
            return None;
        }
        let bound = self.bound.as_ref()?;
        let frames = bound.ring.available_frames();
        if frames == 0 {
            return None;
        }
        // frames / (rate * playback rate), the playback rate in Q32.
        let per_second =
            u128::from(bound.ring_rate.get()) * u128::from(self.session.clock.playback_rate.q32());
        let nanos = (frames as u128 * 1_000_000_000 * u128::from(PLAYBACK_RATE_SCALE))
            .div_ceil(per_second.max(1));
        let left = Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX));
        Some(now + left.max(TAIL_FLOOR))
    }

    /// The binding's own health, read from records the media loop already folded. No lock, no
    /// handle to a `CallbackControl`, and no conflation of "the device went away" (the ladder's
    /// business) with "the decoder died" (the session's).
    pub(super) fn check_health(&self) -> Result<(), AudioError> {
        match self.worker_failure.as_ref() {
            Some(reason) => Err(reason.clone()),
            None => Ok(()),
        }
    }

    pub(crate) fn metrics(&self) -> AudioPlaybackMetrics {
        self.session.metrics.snapshot(&self.session.clock)
    }

    /// Teardown. The dispose ramp, `drop(cpal::Stream)` and the decode worker's reap all happen on
    /// the audio-owner thread; this posts the order and returns without waiting for them.
    pub(crate) fn dispose(&mut self) -> Result<(), AudioError> {
        if self.phase.is_disposed() {
            return Ok(());
        }
        self.phase = AudioOutputPhase::Disposed;
        self.seek_transaction = None;
        self.generation.advance();
        self.publish_posture();
        self.orders.post(AudioOrder::Dispose);
        self.bound = None;
        self.device.disposed();
        // The caller is the media loop: it posts the order and leaves. The owner thread stops
        // the stream, hands the output authority back and reaps the worker on its own time.
        self.lease.take();
        drop(self.owner.take());
        Ok(())
    }

    pub(super) fn ensure_live(&self) -> Result<(), AudioError> {
        if self.phase.is_disposed() {
            Err(AudioError::Disposed)
        } else {
            Ok(())
        }
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        let _ = self.dispose();
    }
}
