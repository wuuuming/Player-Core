//! The decode worker thread: the commands it takes, the two halves of its handle - the one
//! that talks and the one that reaps - and the spawn itself.

use super::callback::CallbackControl;
use super::clock::AudioClock;
use super::error::{AudioError, AudioOpenError};
use super::ffi::NativeAudioDecoder;
use super::follow::{NextMeta, NextTrack};
use super::metrics::SharedMetrics;
#[cfg(feature = "instrument")]
use super::metrics::LIVE_DECODE_WORKERS;
use super::mix::MixStage;
use super::mmcss::{MmcssRegistration, MmcssTask};
use super::policy::AudioPolicy;
use super::pump::DecoderPump;
use super::ring::AudioRing;
use super::session::{AudioEdge, Followed, StopBound};
use super::stream::StreamWriter;
use crate::dsp::{handover_at, DspChain};
use crate::{NormalizedTimestamp, StreamTiming};
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

pub(super) const WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
pub(super) const WORKER_SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(1);

#[derive(Clone, Copy, Debug)]
pub struct WorkerReady {
    pub generation: u64,
    pub anchor: NormalizedTimestamp,
}

pub(super) enum WorkerCommand {
    Seek {
        position: Duration,
        generation: u64,
        /// The song `position` is in: the one the worker decodes, or the one before a boundary
        /// the media loop's clock has not reached yet.
        track: u64,
        response: mpsc::SyncSender<Result<WorkerReady, AudioError>>,
    },
    SetParked {
        parked: bool,
        response: mpsc::SyncSender<()>,
    },
    /// A song opened ahead, to follow the one decoding.
    Next(Box<NextTrack>),
    Shutdown,
}

/// How to talk to a decode worker: a command channel and a thread to unpark.
///
/// Every field is `Send + Sync`, nothing here owns the worker thread, and the destructor is O(1).
/// This is the only worker surface a media-loop value is allowed to hold.
#[derive(Clone)]
pub(super) struct WorkerHandle {
    pub(super) commands: mpsc::SyncSender<WorkerCommand>,
    pub(super) thread: Thread,
}

/// The duty to reap one decode worker, and the only value in this file whose destructor waits.
///
/// `spawn_decode_worker` is its only constructor, and `prepare_binding` — which runs on a rebind
/// thread or the audio-owner thread — is that function's only caller. "The media loop reaped a
/// decode worker" is therefore not a rule anyone has to remember: no type a media-loop value can
/// hold has this field.
pub(super) struct WorkerLifetime {
    pub(super) commands: mpsc::SyncSender<WorkerCommand>,
    pub(super) thread: Thread,
    pub(super) join: Option<JoinHandle<()>>,
}

pub(super) struct AudioWorkerSource {
    pub(super) mixer: Option<MixStage>,
    /// The song's own processing.
    pub(super) dsp: Option<DspChain>,
    /// The stream's, which every song in the ring goes through (`crate::dsp::stream_chain`).
    pub(super) stream: Option<DspChain>,
    pub(super) decoder: NativeAudioDecoder,
    pub(super) timing: StreamTiming,
}

pub(super) struct AudioWorkerResources {
    pub(super) ring: Arc<AudioRing>,
    pub(super) clock: Arc<AudioClock>,
    pub(super) metrics: Arc<SharedMetrics>,
    pub(super) control: Arc<CallbackControl>,
    /// Where every off-media-loop thread publishes an edge the media loop must fold. It replaces
    /// `worker_error: Arc<Mutex<Option<String>>>`, whose reader was the media loop — which had to
    /// take a lock the decode worker could hold.
    pub(super) edges: mpsc::Sender<AudioEdge>,
    pub(super) wake_render: Arc<dyn Fn() + Send + Sync>,
    /// The session's stop bound: the pump hands the ring nothing past it.
    pub(super) stop: Arc<StopBound>,
    /// The serial of the song the host wants next (`AudioSession::next_wanted`).
    pub(super) next_wanted: Arc<AtomicU64>,
}

/// Where a fresh binding's decoder starts. `open` passes generation 1 with no position; a rebind
/// passes the next generation and the media position to resume at.
#[derive(Clone, Copy, Debug)]
pub(super) struct AudioWorkerStart {
    pub(super) generation: u64,
    pub(super) position: Option<Duration>,
    /// Which song the source is (`AudioOwner::track`).
    pub(super) track: u64,
}

#[derive(Clone, Copy)]
pub(super) struct AudioWorkerConfiguration {
    pub(super) policy: AudioPolicy,
    pub(super) sample_rate: NonZeroU32,
    pub(super) channels: NonZeroUsize,
}

/// The one and only decode-worker constructor. It hands back the reaping duty and the talking
/// surface as two separate values, so a caller that must not reap simply never receives the one
/// that reaps.
pub(super) fn spawn_decode_worker(
    source: AudioWorkerSource,
    resources: AudioWorkerResources,
    configuration: AudioWorkerConfiguration,
    start: AudioWorkerStart,
) -> Result<(WorkerLifetime, WorkerHandle, WorkerReady), AudioOpenError> {
    let (command_tx, command_rx) = mpsc::sync_channel(1);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let join = thread::Builder::new()
        .name("player-core-audio-decode".to_owned())
        .spawn(move || {
            #[cfg(feature = "instrument")]
            let _lifetime = DecodeWorkerLifetime::enter();
            // The device waits on this thread for as long as the ring lasts, and no longer.
            let _scheduling = MmcssRegistration::current_thread_recorded(
                MmcssTask::Audio,
                &resources.metrics.decoder_thread_mmcss,
            );
            if let Some(worker) = DecodeWorker::start(
                source,
                resources,
                configuration,
                start,
                command_rx,
                ready_tx,
            ) {
                worker.run();
            }
        })
        .map_err(|error| {
            AudioOpenError::Worker(format!("could not start audio decode worker: {error}"))
        })?;
    let worker_thread = join.thread().clone();
    let ready = ready_rx
        .recv()
        .map_err(|_| {
            AudioOpenError::Worker("audio decode worker stopped during prefill".to_owned())
        })?
        .map_err(|error| AudioOpenError::Prefill(Box::new(error)))?;
    Ok((
        WorkerLifetime {
            commands: command_tx.clone(),
            thread: worker_thread.clone(),
            join: Some(join),
        },
        WorkerHandle {
            commands: command_tx,
            thread: worker_thread,
        },
        ready,
    ))
}

/// The worker is done: shut down, or failed (the failure published).
struct Stopped;

/// A generation whose first sound has not arrived yet: who is waiting for it, if anyone. The
/// worker returns to it on every pass, so a command — a newer seek, a park, the shutdown — is
/// never stuck behind a network read.
struct PendingGeneration {
    generation: u64,
    response: Option<mpsc::SyncSender<Result<WorkerReady, AudioError>>>,
}

/// The decode worker's state, on its thread: the songs in line, the stream they go into, and the
/// generation the clock is on.
struct DecodeWorker {
    commands: mpsc::Receiver<WorkerCommand>,
    /// The song decoding.
    pump: DecoderPump,
    out: StreamWriter,
    pending: Option<PendingGeneration>,
    /// The serial of the song `pump` decodes (0: the one the session opened), and what it is
    /// when it followed another.
    track: u64,
    meta: Option<NextMeta>,
    /// The song opened ahead to follow it.
    next: Option<Box<NextTrack>>,
    /// The song it followed, kept until the clock is well past their boundary so a seek back
    /// over it can still go there.
    previous: Option<Previous>,
    /// The generation the clock is on and where it is anchored: a boundary's place on that clock
    /// is counted in frames given to the stream from there.
    generation: u64,
    anchor: i128,
    /// A boundary crossed whose next song's first frame is not placed yet: where on the clock it
    /// stands (with an overlap, its start), and the song.
    unposted: Option<(i128, NextMeta)>,
    /// The next pass refills whatever the ring holds: a song has just begun in it.
    fill_now: bool,
    parked: bool,
    clock: Arc<AudioClock>,
    control: Arc<CallbackControl>,
    edges: mpsc::Sender<AudioEdge>,
    wake_render: Arc<dyn Fn() + Send + Sync>,
    next_wanted: Arc<AtomicU64>,
    policy: AudioPolicy,
    sample_rate: NonZeroU32,
}

impl DecodeWorker {
    /// The worker at its start position, its first generation pending; `None`, the failure sent
    /// to `ready`, if the decoder could not get there.
    fn start(
        source: AudioWorkerSource,
        resources: AudioWorkerResources,
        configuration: AudioWorkerConfiguration,
        start: AudioWorkerStart,
        commands: mpsc::Receiver<WorkerCommand>,
        ready: mpsc::SyncSender<Result<WorkerReady, AudioError>>,
    ) -> Option<Self> {
        let mut pump = DecoderPump::new(
            source.decoder,
            source.timing,
            configuration.sample_rate,
            configuration.channels,
            resources.metrics,
            resources.stop,
        );
        pump.mixer = source.mixer;
        pump.dsp = source.dsp;
        let target = start.position.map(NormalizedTimestamp::from_position);
        if let Some(position) = start.position {
            if let Err(error) = pump.seek(position) {
                let _ = ready.send(Err(error));
                return None;
            }
        }
        pump.reset_generation(target);
        Some(Self {
            commands,
            pump,
            out: StreamWriter::new(resources.ring, source.stream),
            pending: Some(PendingGeneration {
                generation: start.generation,
                response: Some(ready),
            }),
            track: start.track,
            meta: None,
            next: None,
            previous: None,
            generation: start.generation,
            anchor: 0,
            unposted: None,
            fill_now: false,
            parked: false,
            clock: resources.clock,
            control: resources.control,
            edges: resources.edges,
            wake_render: resources.wake_render,
            next_wanted: resources.next_wanted,
            policy: configuration.policy,
            sample_rate: configuration.sample_rate,
        })
    }

    fn run(mut self) {
        while self.pass().is_ok() {}
    }

    /// One pass: the commands, the pending generation, a boundary, the refill, then the wait for
    /// whatever comes next.
    fn pass(&mut self) -> Result<(), Stopped> {
        let commanded = self.take_commands()?;
        self.forget_unwanted_next();
        // A pending generation anchors whether or not the transport is parked: a seek made while
        // paused still has to land. Its sound not arrived, the read waited its slice already.
        if !self.anchor_pending()? {
            return Ok(());
        }
        self.cross_boundary();
        self.release_previous();
        // Parked, nothing is asked of the decoder until an order comes.
        if !self.parked && !self.refill()? {
            // Starved: the read already waited its slice; go round and ask again.
            return Ok(());
        }
        if !commanded {
            // The callback unparks this thread whenever it leaves the ring low, and every order
            // unparks it too (`post_seek`, `set_parked`, `set_stop`, the shutdown).
            thread::park();
            self.pump.publish_input_state();
        }
        Ok(())
    }

    /// Every command waiting: whether there was one.
    fn take_commands(&mut self) -> Result<bool, Stopped> {
        let mut commanded = false;
        while let Ok(command) = self.commands.try_recv() {
            commanded = true;
            match command {
                WorkerCommand::Seek {
                    position,
                    generation,
                    track,
                    response,
                } => self.seek(position, generation, track, response)?,
                WorkerCommand::Next(opened) => {
                    // Too late once this song's end is published: the host is switching the
                    // ordinary way.
                    if opened.meta.serial == self.next_wanted.load(Ordering::Acquire)
                        && !self.pump.sound_ended()
                    {
                        self.next = Some(opened);
                    }
                }
                WorkerCommand::SetParked { parked, response } => {
                    self.parked = parked;
                    let _ = response.send(());
                }
                WorkerCommand::Shutdown => return Err(Stopped),
            }
        }
        Ok(commanded)
    }

    fn seek(
        &mut self,
        position: Duration,
        generation: u64,
        track: u64,
        response: mpsc::SyncSender<Result<WorkerReady, AudioError>>,
    ) -> Result<(), Stopped> {
        self.out.restart();
        // A generation still pending is superseded: whoever waited for it hears so by its sender
        // going away.
        drop(self.pending.take());
        self.unposted = None;
        if track != self.track {
            self.back_over_boundary(track);
        }
        self.previous = None;
        match self.pump.seek(position) {
            Ok(()) => {
                self.pump
                    .reset_generation(Some(NormalizedTimestamp::from_position(position)));
                self.pending = Some(PendingGeneration {
                    generation,
                    response: Some(response),
                });
                Ok(())
            }
            Err(error) => {
                // Posted seeks have no waiting receiver. The terminal edge must be independent of
                // whether this send succeeds.
                self.fail(&error);
                let _ = response.send(Err(error));
                Err(Stopped)
            }
        }
    }

    /// A seek back over a boundary the clock has not taken up, into `track`: the song it left
    /// plays again, and the one after it waits at its start.
    fn back_over_boundary(&mut self, track: u64) {
        let Some(back) = self.previous.take().filter(|back| back.track == track) else {
            return;
        };
        let mut after = std::mem::replace(&mut self.pump, back.pump);
        if let Some(after_meta) = self.meta.take() {
            let target = after_meta.target();
            if after.seek(after_meta.start).is_ok() {
                after.reset_generation(target);
                self.next = Some(Box::new(NextTrack {
                    pump: after,
                    meta: after_meta,
                }));
            }
        }
        self.meta = back.meta;
        self.track = back.track;
    }

    /// The song opened ahead goes if the host wants another now; the song decoding holds its end
    /// while one is there to follow it, and hands on to it where their overlap starts, if they
    /// overlap. Not while a generation is pending: its first sound may already be past that
    /// point, and it has to arrive before anything is handed on.
    fn forget_unwanted_next(&mut self) {
        let wanted = self.next_wanted.load(Ordering::Acquire);
        if self.next.as_ref().is_some_and(|n| n.meta.serial != wanted) {
            self.next = None;
        }
        self.pump.hold_end(self.next.is_some());
        let handover = self
            .next
            .as_ref()
            .filter(|_| self.pending.is_none())
            .and_then(|next| handover_at(self.pump.known_end_time(), next.meta.crossfade))
            .map(NormalizedTimestamp::from_position);
        self.pump.hand_over_at(handover);
    }

    /// Anchors the pending generation once its sound has arrived: `false` while it has not.
    fn anchor_pending(&mut self) -> Result<bool, Stopped> {
        let Some(waiting) = self.pending.as_mut() else {
            return Ok(true);
        };
        match self.pump.try_anchor(
            &mut self.out,
            &self.clock,
            waiting.generation,
            self.policy.prefill_frames.get(),
        ) {
            Ok(Some(ready)) => {
                self.generation = ready.generation;
                self.anchor = ready.anchor.as_nanoseconds();
                if let Some(response) = waiting.response.take() {
                    let _ = response.send(Ok(ready));
                }
                self.pending = None;
                self.pump.publish_input_state();
                (self.wake_render)();
                Ok(true)
            }
            Ok(None) => {
                self.pump.publish_input_state();
                Ok(false)
            }
            Err(error) => {
                let waiter = waiting.response.take();
                self.fail(&error);
                if let Some(response) = waiter {
                    let _ = response.send(Err(error));
                }
                Err(Stopped)
            }
        }
    }

    /// The song has given all it has, or all before the overlap with the next one, and the next
    /// one is ready: it goes on into the same stream, its first frame right after this one's
    /// last, or summed with the rest of it (`StreamWriter::begin_fade`). The stream's processing
    /// goes on with it, renewed first if its effects changed. The boundary the host hears of is
    /// the overlap's start: the coming song's first frame, as without one.
    fn cross_boundary(&mut self) {
        if self.pending.is_some()
            || !self.pump.end_held
            || !self.pump.at_end()
            || self.next.is_none()
            || !self.out.renew()
        {
            return;
        }
        let Some(opened) = self.next.take() else {
            return;
        };
        let NextTrack {
            pump: after,
            meta: after_meta,
        } = *opened;
        let overlap = if self.pump.at_handover() {
            self.pump.frames_to_known_end()
        } else {
            0
        };
        // The song going plays on to its end, into the overlap.
        self.pump.hand_over_at(None);
        self.out.begin_fade(overlap);
        let rate = i128::from(self.sample_rate.get());
        let at = self.anchor + self.out.given() as i128 * 1_000_000_000 / rate;
        let before = std::mem::replace(&mut self.pump, after);
        self.previous = Some(Previous {
            pump: before,
            track: self.track,
            meta: self.meta.take(),
            at,
        });
        self.track = after_meta.serial;
        self.unposted = Some((at, after_meta.clone()));
        self.meta = Some(after_meta);
        self.fill_now = true;
    }

    /// The song before the boundary goes once the clock is well past it, and their overlap is
    /// over.
    fn release_previous(&mut self) {
        let generation = self.generation;
        if self.out.fading() {
            return;
        }
        if self.previous.as_ref().is_some_and(|back| {
            self.clock.snapshot().is_some_and(|now| {
                now.generation == generation
                    && now.media_now.as_nanoseconds() >= back.at + PREVIOUS_KEPT_NS
            })
        }) {
            self.previous = None;
        }
    }

    /// Tops the ring up once it runs low (or a song has just begun in it), the going song's side
    /// of an overlap first: `false` if a decoder is starved.
    fn refill(&mut self) -> Result<bool, Stopped> {
        let going_starved = self.refill_going();
        let available = self.out.ring.available_frames();
        if !self.fill_now && !self.policy.ring_low(available) {
            return Ok(!going_starved);
        }
        self.fill_now = false;
        if let Err(error) = self
            .pump
            .refill_to(&mut self.out, self.policy.refill_high_frames.get())
        {
            self.fail(&error);
            return Err(Stopped);
        }
        if available == 0 && self.out.ring.available_frames() != 0 {
            (self.wake_render)();
        }
        self.pump.publish_input_state();
        self.post_boundary();
        Ok(!self.pump.starved && !going_starved)
    }

    /// Tops up the going song's side of the overlap under way, from the pump it kept: whether that
    /// decoder is starved. A song that ends, or fails, there leaves the coming one to finish
    /// rising alone; its failure is not the stream's, which goes on.
    fn refill_going(&mut self) -> bool {
        let target = self.policy.refill_high_frames.get();
        let (Some(back), Some(going)) = (self.previous.as_mut(), self.out.going()) else {
            self.out.going_ended();
            return false;
        };
        let failed = back.pump.refill_to(going, target).is_err();
        let starved = back.pump.starved;
        if failed || back.pump.at_end() {
            self.out.going_ended();
            return false;
        }
        starved
    }

    /// Tells the media loop of the boundary crossed last, once the next song's first frame is
    /// placed (or it has none to place): where on the generation's clock it plays, and what the
    /// song is.
    fn post_boundary(&mut self) {
        let Some((_, meta)) = self.unposted.as_ref() else {
            return;
        };
        let start = match self.pump.generation_anchor {
            Some(start) => start,
            None if self.pump.at_end() => meta
                .target()
                .unwrap_or(NormalizedTimestamp::from_nanoseconds(0)),
            None => return,
        };
        let Some((at, meta)) = self.unposted.take() else {
            return;
        };
        let _ = self.edges.send(AudioEdge::Followed(Box::new(Followed {
            generation: self.generation,
            at: NormalizedTimestamp::from_nanoseconds(at),
            start,
            track: meta.serial,
            id: meta.id,
            duration: meta.duration,
            info: meta.info,
            source: meta.source,
            stop: meta.stop,
        })));
        (self.wake_render)();
    }

    fn fail(&self, error: &AudioError) {
        publish_worker_failure(&self.edges, &self.control, self.wake_render.as_ref(), error);
    }
}

impl WorkerHandle {
    /// Posts a seek and does not wait for it: the media loop's form. The worker discards the ring,
    /// repositions and re-anchors the clock when it gets there — which, over a network, can be a
    /// while — and the transport's promise (`UnadoptedSeek`) stands in for the clock until then.
    pub(super) fn post_seek(
        &self,
        position: Duration,
        generation: u64,
        track: u64,
    ) -> Result<(), AudioError> {
        let (response, _nobody_waits) = mpsc::sync_channel(1);
        self.commands
            .send(WorkerCommand::Seek {
                position,
                generation,
                track,
                response,
            })
            .map_err(|_| AudioError::WorkerStopped)?;
        self.thread.unpark();
        Ok(())
    }

    /// Hands the worker a song opened ahead. Waits while the one command slot is taken, so only
    /// a thread that may wait calls it (the next song's opener); a worker gone drops it.
    pub(super) fn post_next(&self, track: Box<NextTrack>) {
        if self.commands.send(WorkerCommand::Next(track)).is_ok() {
            self.thread.unpark();
        }
    }

    pub(super) fn set_parked(&self, parked: bool) -> Result<(), AudioError> {
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        self.commands
            .send(WorkerCommand::SetParked {
                parked,
                response: response_tx,
            })
            .map_err(|_| AudioError::WorkerStopped)?;
        self.thread.unpark();
        worker_reply(&response_rx)
    }
}

/// How long past a boundary, on the clock, the song before it stays open for a seek back over it.
const PREVIOUS_KEPT_NS: i128 = 2_000_000_000;

/// The song a boundary left: its pump, which song it is, and where on the clock the boundary is.
struct Previous {
    pump: DecoderPump,
    track: u64,
    meta: Option<NextMeta>,
    at: i128,
}

/// How long the media loop waits for the decode worker to answer a command. The loop is the render
/// thread: a worker blocked on a stalled network read never answered, and the application froze
/// with it. Past this the command fails and the session faults, which reopens
/// the stream; the worker's late answer finds nobody listening.
pub(super) const WORKER_REPLY_DEADLINE: Duration = Duration::from_secs(2);
/// How long a thread that may wait gives a seek to land: the sound's first packets at the new
/// position have to cross the network.
pub(super) const SEEK_LANDING_DEADLINE: Duration = Duration::from_secs(30);

pub(super) fn worker_reply<T>(response: &mpsc::Receiver<T>) -> Result<T, AudioError> {
    response
        .recv_timeout(WORKER_REPLY_DEADLINE)
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => AudioError::Worker(
                "the audio decode worker did not answer within its deadline".to_owned(),
            ),
            mpsc::RecvTimeoutError::Disconnected => AudioError::WorkerStopped,
        })
}

impl WorkerLifetime {
    pub(super) fn shutdown(&mut self) -> Result<(), AudioError> {
        self.shutdown_with_timeout(WORKER_SHUTDOWN_TIMEOUT)
    }

    pub(super) fn shutdown_with_timeout(&mut self, timeout: Duration) -> Result<(), AudioError> {
        let Some(join) = self.join.take() else {
            return Ok(());
        };
        let started_at = Instant::now();
        let mut shutdown_sent = false;
        loop {
            if !shutdown_sent {
                shutdown_sent = match self.commands.try_send(WorkerCommand::Shutdown) {
                    Ok(()) | Err(mpsc::TrySendError::Disconnected(_)) => true,
                    Err(mpsc::TrySendError::Full(_)) => false,
                };
            }
            self.thread.unpark();
            if join.is_finished() {
                let _ = join.join();
                return Ok(());
            }
            let Some(remaining) = timeout.checked_sub(started_at.elapsed()) else {
                return Err(AudioError::WorkerShutdownTimeout);
            };
            thread::sleep(remaining.min(WORKER_SHUTDOWN_POLL_INTERVAL));
        }
    }
}

impl Drop for WorkerLifetime {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// The decode worker's terminal report. It runs on the worker thread, so allocating a message and
/// posting it is legitimate here; the cpal error callback is a real-time thread and must not, so
/// device faults travel on an atomic instead (`AudioSession::device_faults`).
pub(super) fn publish_worker_failure(
    edges: &mpsc::Sender<AudioEdge>,
    control: &CallbackControl,
    wake_render: &(dyn Fn() + Send + Sync),
    error: &AudioError,
) {
    let _ = edges.send(AudioEdge::WorkerFailed(error.clone()));
    control.fail();
    wake_render();
}

#[cfg(feature = "instrument")]
pub(super) struct DecodeWorkerLifetime;

#[cfg(feature = "instrument")]
impl DecodeWorkerLifetime {
    pub(super) fn enter() -> Self {
        LIVE_DECODE_WORKERS.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

#[cfg(feature = "instrument")]
impl Drop for DecodeWorkerLifetime {
    fn drop(&mut self) {
        LIVE_DECODE_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}
