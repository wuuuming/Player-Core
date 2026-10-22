//! The owner thread: the one thread that builds, rebinds and tears down the output stream,
//! so that no other thread ever has to wait on the device.

use super::binding::{commit_binding, prepare_binding, AudioBinding, PreparedBinding};
use super::device::AudioDeviceSelection;
use super::error::AudioOpenError;
use super::lease::OutputAuthorityLease;
use super::park::AudioParkState;
use super::policy::AudioPolicyTimes;
use super::posture::{
    apply_posture_step, posture_step, AudioOutputPhase, PostureCell, TransportPosture,
};
use super::rebind::rebind_honours_promise;
use super::session::{
    AudioEdge, AudioOrder, AudioSession, AudioSource, Landing, RebindResult, SourceEvent,
};
use super::worker::{AudioWorkerStart, SEEK_LANDING_DEADLINE};
use crate::NormalizedTimestamp;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};
#[cfg(feature = "asio")]
use {
    super::callback::{RampKind, MODE_HELD},
    super::device::OutputStream,
    super::error::AudioError,
};

/// Everything the audio-owner thread owns. Nothing here is reachable from the media loop.
pub(super) struct AudioOwner {
    pub(super) source: AudioSource,
    /// Which song `source` is: 0 for the one the session opened, then the serial of each song it
    /// went on into (`AudioOrder::Follow`). A seek and a rebind name it to the decode worker.
    pub(super) track: u64,
    pub(super) selection: AudioDeviceSelection,
    pub(super) requested_policy: Option<AudioPolicyTimes>,
    pub(super) session: AudioSession,
    pub(super) posture: Arc<PostureCell>,
    pub(super) orders: mpsc::Sender<AudioOrder>,
    /// Where a rebind thread leaves its finished attempt. The receiver is owned here and nowhere
    /// else, so a queued `PreparedBinding` is reaped on this thread even when nobody adopts it.
    pub(super) results: mpsc::Receiver<RebindResult>,
    pub(super) result_sender: mpsc::Sender<RebindResult>,
    pub(super) binding: Option<AudioBinding>,
    /// The posture the hardware currently reflects. It is the owner thread's own record, so a
    /// posture the media loop published but the hardware has not reached yet cannot be mistaken
    /// for one it has.
    pub(super) applied: TransportPosture,
    /// The process's audio output authority. It guards the DEVICE, so it goes the moment the
    /// stream does — before the decode worker is reaped, which can take as long as a network read
    /// that will not return. Held until then, a session closed mid-read would keep the output from
    /// every session after it.
    pub(super) lease: Option<Arc<OutputAuthorityLease>>,
    /// Since when an audible posture has waited for a posted seek to land (`reconcile`); `None`
    /// while nothing waits.
    pub(super) awaiting_landing: Option<Instant>,
}

/// How often the owner thread looks again for the seek an audible posture waits on. The decode
/// worker anchors on a thread of its own and posts no order here; the wait lasts as long as the
/// target's first frames take to decode, or their bytes to arrive.
const LANDING_POLL_INTERVAL: Duration = Duration::from_millis(1);

impl AudioOwner {
    /// Orders block this owner alone. A raw DSD stream also checks for its drained tail so the
    /// driver returns to PCM when the song ends without waiting for another transport order.
    pub(super) fn run(mut self, orders: mpsc::Receiver<AudioOrder>) {
        loop {
            let received = match self.idle_look() {
                Some((wait, look)) => match orders.recv_timeout(wait) {
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        look(&mut self);
                        continue;
                    }
                    other => other.map_err(|_| mpsc::RecvError),
                },
                None => orders.recv(),
            };
            let Ok(order) = received else {
                break;
            };
            match order {
                AudioOrder::Reconcile => self.reconcile(),
                AudioOrder::Seek {
                    position,
                    generation,
                } => self.seek(position, generation),
                AudioOrder::Attempt {
                    generation,
                    promised,
                } => self.start_attempt(generation, promised),
                AudioOrder::Landed => {
                    if let Ok(result) = self.results.try_recv() {
                        self.land(result);
                    }
                }
                AudioOrder::Follow {
                    source,
                    stop,
                    track,
                } => {
                    self.source = *source;
                    self.session.stop = stop;
                    self.track = track;
                }
                AudioOrder::Abandon => {
                    // Whatever is still opening loses its generation HERE, before the binding is
                    // destroyed, so an attempt in flight abandons itself at its next boundary
                    // instead of landing a device on a session that gave up on it.
                    self.session.cancel_attempts();
                    self.session.metrics.step_source(SourceEvent::Cancel);
                    self.destroy_binding();
                }
                AudioOrder::Dispose => break,
            }
        }
        self.reconcile();
        self.session.cancel_attempts();
        if let Some(AudioBinding { stream, worker, .. }) = self.binding.take() {
            drop(stream);
            self.session.park.store(AudioParkState::HardwareParked);
            self.lease = None;
            drop(worker);
        }
        self.lease = None;
        self.session.metrics.step_source(SourceEvent::Close);
    }

    /// What the owner looks at by itself when no order comes, and how long it waits first: a
    /// seek's landing an audible posture waits on (`LANDING_POLL_INTERVAL`); a native ASIO
    /// stream's drained end (20 ms). `None`: it waits for an order.
    #[allow(clippy::type_complexity)]
    fn idle_look(&self) -> Option<(Duration, fn(&mut Self))> {
        if self.awaiting_landing.is_some() {
            return Some((LANDING_POLL_INTERVAL, Self::reconcile));
        }
        #[cfg(feature = "asio")]
        if self
            .binding
            .as_ref()
            .is_some_and(|binding| matches!(binding.stream, OutputStream::NativeAsio(_)))
        {
            return Some((Duration::from_millis(20), Self::settle_native_asio_end));
        }
        None
    }

    /// A prepared binding becomes this owner's: committed, kept, the hardware now reflecting
    /// `wanted`. On a failure the prepared binding is dropped here, its decode worker reaped on
    /// this thread.
    pub(super) fn commit(
        &mut self,
        prepared: Box<PreparedBinding>,
        wanted: TransportPosture,
    ) -> Result<Landing, AudioOpenError> {
        match commit_binding(prepared, wanted, &self.session) {
            Ok(committed) => {
                self.binding = Some(committed.binding);
                self.applied = wanted;
                Ok(committed.landing)
            }
            Err((error, prepared)) => {
                drop(prepared);
                Err(error)
            }
        }
    }

    #[cfg(feature = "asio")]
    fn settle_native_asio_end(&mut self) {
        let Some(binding) = self.binding.as_ref() else {
            return;
        };
        if self.posture.load().seeking || binding.control.failed.load(Ordering::Acquire) {
            return;
        }
        let ended = self.session.metrics.eof_reached.load(Ordering::Acquire)
            && binding.ring.available_frames() == 0;
        if ended && !self.session.park.load().hardware_stream_paused() {
            // The callback first delivers DSD silence through the outstanding hardware buffers.
            // The driver can then return to PCM without cutting off the final media bytes.
            let result = binding
                .control
                .request(MODE_HELD, RampKind::Pause)
                .and_then(|()| binding.stream.pause().map_err(AudioError::PauseStream));
            match result {
                Ok(()) => {
                    self.session.park.store(AudioParkState::HardwareParked);
                    self.applied.phase = AudioOutputPhase::Paused;
                }
                Err(error) => {
                    binding.control.fail();
                    self.session.post_edge(AudioEdge::WorkerFailed(error));
                }
            }
        } else if !ended
            && self.session.park.load().hardware_stream_paused()
            && self.posture.load().hardware_runs()
        {
            // Extending a CUE span can supply more bytes without a transport command.
            self.reconcile();
        }
    }

    pub(super) fn reconcile(&mut self) {
        let wanted = self.posture.load();
        if self.binding.is_none() {
            self.awaiting_landing = None;
            return;
        }
        // A seek posted and not yet landed: the ring holds the old position's frames until the
        // worker voids them, and then nothing until the target's first frames are decoded. A
        // sound ramped up now plays a snippet of the old place, then silence under a fade-in that
        // is over by the time the target arrives at full gain — the clicks a CUE track's start
        // made (it is sent to its track and told to play in the same breath). So the sound comes
        // up on the target's first frames. The wait is bounded as a seek's landing is: past it,
        // what there is plays.
        if wanted.audible() && !self.applied.audible() && self.session.seek_unlanded() {
            let since = *self.awaiting_landing.get_or_insert_with(Instant::now);
            if since.elapsed() < SEEK_LANDING_DEADLINE {
                return;
            }
        }
        self.awaiting_landing = None;
        self.apply(wanted);
        (self.session.wake_render)();
    }

    /// Takes the hardware from the posture it reflects to `wanted`, through the one table.
    fn apply(&mut self, wanted: TransportPosture) {
        let Some(binding) = self.binding.as_ref() else {
            return;
        };
        let park = self.session.park.load();
        let step = posture_step(self.applied, wanted, park);
        match apply_posture_step(
            &binding.stream,
            &binding.handle,
            &binding.control,
            &self.session.park,
            step,
        ) {
            Ok(()) => {
                self.session.park.store(step.park_after(park, wanted));
                self.applied = wanted;
            }
            Err(error) => self.session.post_edge(AudioEdge::WorkerFailed(error)),
        }
    }

    /// A seek, in the only order that cannot be heard: a sound still audible fades to its
    /// terminal zero where it is; then the worker voids the ring and goes to `position`; then the
    /// sound comes back up — as `reconcile` has it — on the target's first frames.
    ///
    /// The transport's own postures cannot carry this. A seek holds and commits in one call on
    /// the media loop; by the time this thread reads the posture cell the hold is gone and the
    /// playing posture it finds is the one it already applied, so it did nothing — while the
    /// worker, told directly, voided the ring under a sound at full gain.
    fn seek(&mut self, position: Duration, generation: u64) {
        if self.binding.is_none() {
            // A rebind is under way; the binding that lands starts at the transport's promise.
            return;
        }
        if self.applied.audible() {
            self.apply(TransportPosture {
                phase: self.applied.phase.held(),
                seeking: true,
            });
        }
        let Some(binding) = self.binding.as_ref() else {
            return;
        };
        if let Err(error) = binding.handle.post_seek(position, generation, self.track) {
            self.session.post_edge(AudioEdge::WorkerFailed(error));
        }
        self.reconcile();
    }

    /// Destroys the current binding. `drop(cpal::Stream)` may terminate and join cpal's WASAPI
    /// callback thread, and `WorkerLifetime::drop` polls for up to `WORKER_SHUTDOWN_TIMEOUT`.
    /// Both happen HERE, on `player-core-audio-owner`, and nowhere else in this file.
    pub(super) fn destroy_binding(&mut self) {
        if let Some(binding) = self.binding.take() {
            let AudioBinding { stream, worker, .. } = binding;
            drop(stream);
            drop(worker);
            self.applied = TransportPosture {
                phase: AudioOutputPhase::Paused,
                seeking: self.posture.load().seeking,
            };
            self.session.park.store(AudioParkState::HardwareParked);
        }
    }

    pub(super) fn start_attempt(&mut self, generation: u64, promised: Duration) {
        // Destroy first: the dead worker still holds the session's metrics, so it must be reaped
        // before a new one can publish into them.
        self.destroy_binding();
        // The attempt owns this generation from here until something else publishes another one.
        self.session.admit_generation(generation);
        self.session.metrics.step_source(SourceEvent::Reopen);
        let start = AudioWorkerStart {
            generation,
            position: Some(promised),
            track: self.track,
        };
        let source = self.source.clone();
        let selection = self.selection.clone();
        let requested_policy = self.requested_policy;
        let session = self.session.clone();
        let orders = self.orders.clone();
        let results = self.result_sender.clone();
        let spawned = thread::Builder::new()
            .name("player-core-audio-rebind".to_owned())
            .spawn(move || {
                let result =
                    match prepare_binding(&source, &selection, requested_policy, &session, start) {
                        Ok(prepared) => {
                            if rebind_honours_promise(promised, prepared.ready.anchor) {
                                RebindResult::Prepared(Box::new(prepared))
                            } else {
                                RebindResult::Failed(Arc::new(AudioOpenError::Worker(format!(
                                    "rebound decoder anchored at {}ns, not at the promised {}ns",
                                    prepared.ready.anchor.as_nanoseconds(),
                                    NormalizedTimestamp::from_position(promised).as_nanoseconds()
                                ))))
                            }
                        }
                        Err(error) => {
                            session.metrics.fail_source(&error);
                            RebindResult::Failed(Arc::new(error))
                        }
                    };
                // The payload goes first and the signal second, both on this thread and both FIFO,
                // so an owner that observes `Landed` always finds the result waiting. A closed
                // channel means the session is gone; dropping the result reaps the prepared
                // decode worker HERE, on a thread that is allowed to wait.
                let _ = results.send(result);
                let _ = orders.send(AudioOrder::Landed);
                (session.wake_render)();
            });
        if let Err(error) = spawned {
            // A refused spawn is an attempt that failed, reported through the one failure edge
            // every other failure uses. There is no second kind of failure edge, so there is no
            // phase in which a failure can fail to apply backoff.
            self.session
                .post_edge(AudioEdge::AttemptFailed(Arc::new(AudioOpenError::Worker(
                    format!("could not start the audio rebind thread: {error}"),
                ))));
        }
    }

    pub(super) fn land(&mut self, result: RebindResult) {
        match result {
            RebindResult::Failed(reason) => {
                let _ = self.session.edges.send(AudioEdge::AttemptFailed(reason));
            }
            RebindResult::Prepared(prepared) => {
                let wanted = self.posture.load();
                if matches!(wanted.phase, AudioOutputPhase::Disposed) {
                    // Dropping `prepared` reaps its decode worker on this thread.
                    drop(prepared);
                    return;
                }
                match self.commit(prepared, wanted) {
                    Ok(landing) => {
                        self.session.metrics.step_source(SourceEvent::Opened);
                        self.session
                            .metrics
                            .device_rebinds
                            .fetch_add(1, Ordering::Relaxed);
                        let _ = self
                            .session
                            .edges
                            .send(AudioEdge::Landed(Box::new(landing)));
                    }
                    Err(error) => {
                        let _ = self
                            .session
                            .edges
                            .send(AudioEdge::AttemptFailed(Arc::new(error)));
                    }
                }
            }
        }
        (self.session.wake_render)();
    }
}
