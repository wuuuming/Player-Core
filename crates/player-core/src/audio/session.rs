//! One audio session: the state it keeps across every binding, the orders the media loop
//! posts to it, the edges it posts back, and the source state machine those edges step.

use super::binding::PreparedBinding;
use super::callback::UserGain;
use super::clock::{AudioClock, PlaybackRateControl, FIRST_GENERATION};
use super::error::{AudioError, AudioOpenError};
use super::metrics::{Announce, SharedMetrics};
use super::park::AudioParkCell;
use super::policy::AudioPolicy;
use super::ring::AudioRing;
use super::track::AudioStreamInfo;
use super::worker::WorkerHandle;
use crate::dsp::{song_end, ReplayGainSetting};
use crate::{AudioSourceFactory, NormalizedTimestamp};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// Everything that outlives every binding: the media clock, the user's volume, the host's wake,
/// the metrics, the worker error slot.
///
/// It is created exactly once, by `AudioOutput::open`, **before any device is resolved**, and
/// from then on it is only ever cloned into a bind. `prepare_binding` borrows one and has no
/// parameter from which a clock could be built, so a rebind keeps the position the host reads.
#[derive(Clone)]
pub(super) struct AudioSession {
    pub(super) clock: Arc<AudioClock>,
    pub(super) metrics: Arc<SharedMetrics>,
    /// This output's park posture. One cell, three values: "hardware parked while the terminal
    /// zero ramp was never delivered" is not a representable posture, so no call site can publish
    /// it. Published only by the stream-owner thread after the matching CPAL operation succeeds.
    pub(super) park: Arc<AudioParkCell>,
    pub(super) user_gain: Arc<UserGain>,
    /// The host's ReplayGain setting, read by every binding's stage.
    pub(super) replay_gain: Arc<ReplayGainSetting>,
    /// The host's CLAP effects, after ReplayGain in every binding's chain.
    pub(super) effects: Option<crate::clap::Effects>,
    /// Bumped by the cpal error callback. It stays an atomic because a real-time callback must not
    /// allocate; every other producer of an edge uses `edges`.
    pub(super) device_faults: Arc<AtomicU64>,
    pub(super) edges: mpsc::Sender<AudioEdge>,
    pub(super) wake_render: Arc<dyn Fn() + Send + Sync>,
    /// The one generation an in-flight open may still land. An attempt owns a number; anything
    /// that supersedes or abandons it publishes a different one here, and the attempt sees that at
    /// its next boundary. No generation owns 0, so `cancel_attempts` names no successor.
    pub(super) live_generation: Arc<AtomicU64>,
    /// Where the sound stops short of its container's end. Every binding's decode worker reads
    /// the same one, so a rebind stops where the binding it replaces would have.
    pub(super) stop: Arc<StopBound>,
    /// The generation of the last seek posted to a decode worker. Until the clock has taken it
    /// up — the worker anchored there, with the target's first frames in the ring — the ring holds
    /// the old position's frames, then nothing: sound must not be ramped up over either
    /// (`AudioOwner::reconcile`). 0 before any seek.
    pub(super) awaited_generation: Arc<AtomicU64>,
    /// The serial of the song the host wants to follow this one (`AudioOutput::set_next`), 0 for
    /// none. A decode worker takes a song opened ahead only while it is still this one, and lets
    /// go of one it holds once it is not.
    pub(super) next_wanted: Arc<AtomicU64>,
}

/// Where a song ends. Its stop is a media position the decoder hands the ring nothing past: a
/// stretch of a file played as the whole of one (a CUE sheet's track) ends there, exactly, and
/// reads as ended. Unset, the sound runs to its container's end, which is where the host's probe
/// put it (`length`), when it knew. The stop is written by the media loop and read by the decode
/// worker between blocks; nanoseconds of normalized media time, `i64::MAX` for none.
pub struct StopBound {
    stop: AtomicI64,
    length: Option<Duration>,
}

impl Default for StopBound {
    fn default() -> Self {
        Self::new(None, None)
    }
}

impl StopBound {
    /// A song's stop and its container's length, as the host gave them.
    pub(super) fn new(stop: Option<Duration>, length: Option<Duration>) -> Self {
        let bound = Self {
            stop: AtomicI64::new(i64::MAX),
            length,
        };
        bound.set(stop);
        bound
    }

    pub(super) fn get(&self) -> Option<NormalizedTimestamp> {
        self.stop().map(NormalizedTimestamp::from_position)
    }

    fn stop(&self) -> Option<Duration> {
        match self.stop.load(Ordering::Acquire) {
            i64::MAX => None,
            nanoseconds => Some(Duration::from_nanos(
                u64::try_from(nanoseconds).unwrap_or(0),
            )),
        }
    }

    pub fn set(&self, stop: Option<Duration>) {
        let nanoseconds = stop.map_or(i64::MAX, |stop| {
            i64::try_from(stop.as_nanos()).unwrap_or(i64::MAX - 1)
        });
        self.stop.store(nanoseconds, Ordering::Release);
    }

    /// Where the song's sound ends, as far as is known before it gets there: its stop, else its
    /// container's length.
    pub(super) fn end(&self) -> Option<Duration> {
        song_end(self.stop(), self.length)
    }
}

impl AudioSession {
    pub(super) fn open(
        wake_render: Arc<dyn Fn() + Send + Sync>,
        playback_rate: Arc<PlaybackRateControl>,
        edges: mpsc::Sender<AudioEdge>,
    ) -> Self {
        Self {
            live_generation: Arc::new(AtomicU64::new(FIRST_GENERATION)),
            clock: Arc::new(AudioClock::with_playback_rate(playback_rate)),
            metrics: Arc::new(SharedMetrics {
                announce: Announce(Arc::clone(&wake_render)),
                ..SharedMetrics::default()
            }),
            park: Arc::new(AudioParkCell::new()),
            user_gain: Arc::new(UserGain::default()),
            replay_gain: Arc::new(ReplayGainSetting::default()),
            effects: None,
            device_faults: Arc::new(AtomicU64::new(0)),
            edges,
            wake_render,
            stop: Arc::new(StopBound::default()),
            awaited_generation: Arc::new(AtomicU64::new(0)),
            next_wanted: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Whether a seek has been posted that no decode worker has anchored yet.
    pub(super) fn seek_unlanded(&self) -> bool {
        self.clock.generation() < self.awaited_generation.load(Ordering::Acquire)
    }

    /// Checked at every boundary of an open that can block. Nothing else stops an attempt the
    /// ladder has already given up on from finishing and touching a device behind its back.
    pub(super) fn generation_live(&self, generation: u64) -> bool {
        self.live_generation.load(Ordering::Acquire) == generation
    }

    /// An open of a generation no longer live stops here, as `Cancelled`.
    pub(super) fn ensure_live(&self, generation: u64) -> Result<(), AudioOpenError> {
        if self.generation_live(generation) {
            Ok(())
        } else {
            Err(AudioOpenError::Cancelled)
        }
    }

    /// Tells the media loop: posts `edge`, then wakes it.
    pub(super) fn post_edge(&self, edge: AudioEdge) {
        let _ = self.edges.send(edge);
        (self.wake_render)();
    }

    pub(super) fn admit_generation(&self, generation: u64) {
        self.live_generation.store(generation, Ordering::Release);
    }

    pub(super) fn cancel_attempts(&self) {
        self.live_generation.store(0, Ordering::Release);
    }
}

/// Orders the media loop and the rebind threads post to the audio-owner thread.
///
/// The channel is UNBOUNDED on purpose: `mpsc::Sender::send` never blocks, so posting an order
/// cannot become a wait, and "the order slot was full so the transport's intent was dropped" is
/// not a state that exists. `SyncSender`, whose `send` blocks when full, must never appear here.
pub(super) enum AudioOrder {
    /// The transport published a new posture. What to do is read from the posture cell at APPLY
    /// time, so a burst collapses into one convergent hardware sequence.
    Reconcile,
    /// Start exactly one rebind attempt.
    Attempt { generation: u64, promised: Duration },
    /// A rebind thread finished and has already placed its result in `AudioOwner::results`.
    ///
    /// It is a SIGNAL, not a payload. A `RebindResult` owns a `WorkerLifetime`, whose destructor
    /// polls for up to `WORKER_SHUTDOWN_TIMEOUT`; if it travelled in this enum then the media
    /// loop — which constructs `Attempt`, `Abandon` and `Dispose`, and drops whatever a refused
    /// `send` hands back — would hold a type that can reap a decode worker. Splitting the signal
    /// from the payload puts the result's whole lifetime on the owner thread, where waiting is
    /// allowed, so the media loop reaches no blocking wait and no device build.
    Landed,
    /// The recovery window closed. Destroy what is left; expect no more attempts.
    Abandon,
    /// Send the decode worker to `position` under `generation`. It travels through this thread,
    /// not straight to the worker, because the worker's first act is to void the ring: a sound
    /// still audible is taken to its terminal zero here first, so what it plays is never cut off
    /// under full gain — and its return waits for the target's first frames (`reconcile`).
    Seek { position: Duration, generation: u64 },
    /// The sound went on into the song after it, and that song is now what a rebind opens again
    /// and where a seek goes: its source, its stop bound and its serial.
    Follow {
        source: Box<AudioSource>,
        stop: Arc<StopBound>,
        track: u64,
    },
    /// Teardown. The owner thread performs the dispose ramp, destroys the binding, and exits.
    Dispose,
}

/// A finished attempt, delivered on a channel whose receiver never leaves the owner thread.
/// `Box` keeps the failure arm cheap and the enum small.
pub(super) enum RebindResult {
    Prepared(Box<PreparedBinding>),
    Failed(Arc<AudioOpenError>),
}

/// Everything about a live binding the media loop is allowed to learn. No `cpal::Stream`, no
/// `WorkerLifetime`, no `CallbackControl`: the three values whose use or destruction can wait.
pub(super) struct Landing {
    pub(super) device: String,
    pub(super) info: AudioStreamInfo,
    pub(super) policy: AudioPolicy,
    pub(super) worker: WorkerHandle,
    pub(super) ring: Arc<AudioRing>,
    /// The ring frames the device plays a second at playback rate 1 (the callback's rate).
    pub(super) ring_rate: NonZeroU32,
    /// The output-host version the device was resolved under.
    pub(super) route_version: u64,
}

/// What the media loop folds. Every edge that is not real-time-callback-sourced travels here, so
/// there is one place to look for "what happened to the device since the last frame".
pub(super) enum AudioEdge {
    Landed(Box<Landing>),
    /// Every attempt failure — a refused thread spawn, a failed device resolve, a failed stream
    /// build, a broken position promise — arrives here and nowhere else.
    AttemptFailed(Arc<AudioOpenError>),
    WorkerFailed(AudioError),
    /// The decode worker went on from one song into the next in the same ring: where, on the
    /// generation's clock, the next one's first frame plays.
    Followed(Box<Followed>),
}

/// A boundary the decode worker crossed: the song after it starts at `at` of `generation`'s
/// clock, at its own media time `start`. The media loop takes it up once the clock gets there.
pub(super) struct Followed {
    pub(super) generation: u64,
    pub(super) at: NormalizedTimestamp,
    pub(super) start: NormalizedTimestamp,
    /// The serial `AudioOutput::set_next` gave the song.
    pub(super) track: u64,
    /// The host's name for it (`AudioNext::id`).
    pub(super) id: u64,
    pub(super) duration: Option<Duration>,
    pub(super) info: AudioStreamInfo,
    pub(super) source: AudioSource,
    pub(super) stop: Arc<StopBound>,
}

/// What the media loop holds about the live binding: two `Send + Sync` handles and nothing else.
pub(super) struct BoundView {
    pub(super) worker: WorkerHandle,
    pub(super) ring: Arc<AudioRing>,
    /// As [`Landing::ring_rate`].
    pub(super) ring_rate: NonZeroU32,
}

/// Where a binding's bytes come from: the FACTORY and the resolved stream index, nothing else.
/// There is no path here, so a rebind cannot re-derive one — it calls `open()` again. The
/// device-rebind ladder clones this value, so the track and the transport both survive a rebind.
#[derive(Clone)]
pub(super) struct AudioSource {
    pub(super) factory: Arc<dyn AudioSourceFactory>,
    pub(super) stream_index: i32,
    /// The source's own sample rate as the last binding's decoder read it; 0 until one has.
    /// Shared by every clone, so a rebind asks an ASIO driver for it without opening twice.
    pub(super) source_rate: Arc<AtomicU32>,
}

impl AudioSource {
    /// The rate a decoder of this source read is remembered, so that the next bind asks the
    /// driver for it; returned too (0 when it does not fit).
    pub(super) fn note_read_rate(&self, source_sample_rate: i32) -> u32 {
        let rate = u32::try_from(source_sample_rate).unwrap_or(0);
        self.source_rate.store(rate, Ordering::Release);
        rate
    }
}

/// The source lifecycle, per generation. `Cancelled` and `Failed` are terminal *for the generation
/// that reached them*: only a new open or a new reopen — each carrying a new generation — leaves
/// them, so "terminal" is a property of the table rather than a rule someone has to remember.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum AudioSourceState {
    #[default]
    Closed = 0,
    Opening = 1,
    Ready = 2,
    Reopening = 3,
    Cancelled = 4,
    Failed = 5,
}

/// Discriminant order, so publishing is a cast and reading back is an index — one encoding.
pub(super) const SOURCE_STATES: [AudioSourceState; 6] = [
    AudioSourceState::Closed,
    AudioSourceState::Opening,
    AudioSourceState::Ready,
    AudioSourceState::Reopening,
    AudioSourceState::Cancelled,
    AudioSourceState::Failed,
];

/// What happens to a source. One total step function describes the first open, every rebind, and
/// every teardown; there is no second table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SourceEvent {
    Open,
    Reopen,
    Opened,
    Cancel,
    Fail,
    Close,
}

pub(super) fn source_step(state: AudioSourceState, event: SourceEvent) -> AudioSourceState {
    use AudioSourceState::{Cancelled, Closed, Failed, Opening, Ready, Reopening};
    match (state, event) {
        (_, SourceEvent::Open) => Opening,
        (_, SourceEvent::Reopen) => Reopening,
        (Opening | Reopening, SourceEvent::Opened) => Ready,
        (Opening | Reopening, SourceEvent::Cancel) => Cancelled,
        (Opening | Reopening, SourceEvent::Fail) => Failed,
        (Ready, SourceEvent::Close) => Closed,
        (state, _) => state,
    }
}
