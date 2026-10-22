//! What a running session publishes: the process-lifetime runtime counts and the per-session
//! playback metrics the callback and the pump both write.

use super::clock::AudioClock;
use super::error::AudioOpenError;
#[cfg(feature = "instrument")]
use super::lease::{LIVE_OUTPUT_AUTHORITIES, MAX_LIVE_OUTPUT_AUTHORITIES};
#[cfg(feature = "instrument")]
use super::park::LAST_PUBLISHED_PARK;
use super::session::{source_step, AudioSourceState, SourceEvent, SOURCE_STATES};
use crate::AudioSourceKind;
#[cfg(feature = "instrument")]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

#[cfg(feature = "instrument")]
pub(super) static LIVE_DECODE_WORKERS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "instrument")]
pub(super) static LIVE_CALLBACK_CLOSURES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "instrument")]
pub(super) static CALLBACKS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "instrument")]
pub(super) static LAST_CALLBACK_PERIOD_NANOSECONDS: AtomicU64 = AtomicU64::new(0);
/// Every device resolve this process has performed — the external observer of "a cancelled open
/// touched no device". A count, not a flag, so an extra touch cannot hide behind an earlier one.
#[cfg(feature = "instrument")]
pub(super) static DEVICE_RESOLVES: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "instrument")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioRuntimeCounts {
    pub output_authorities: usize,
    pub decode_workers: usize,
    pub callback_closures: usize,
    pub callbacks_in_flight: usize,
    pub max_output_authorities: usize,
    /// Process-lifetime callback invocation count. Unlike `callback_closures`, this is activity,
    /// so an unchanged delta between two snapshots means no callback ran.
    pub callback_calls: u64,
    /// Duration represented by the most recently requested device buffer. The external observer
    /// must see a stable callback count for at least this period before opening its fixed window.
    pub last_callback_period_nanoseconds: u64,
    /// The pause/down-ramp callback has acknowledged delivery of terminal zero.
    pub terminal_zero_delivered: bool,
    /// Truth published by the owner-thread `Stream::pause()` step, never inferred from the ramp.
    pub hardware_stream_paused: bool,
}

#[cfg(feature = "instrument")]
pub fn audio_runtime_counts() -> AudioRuntimeCounts {
    AudioRuntimeCounts {
        output_authorities: LIVE_OUTPUT_AUTHORITIES.load(Ordering::Acquire),
        decode_workers: LIVE_DECODE_WORKERS.load(Ordering::Acquire),
        callback_closures: LIVE_CALLBACK_CLOSURES.load(Ordering::Acquire),
        callbacks_in_flight: CALLBACKS_IN_FLIGHT.load(Ordering::Acquire),
        max_output_authorities: MAX_LIVE_OUTPUT_AUTHORITIES.load(Ordering::Acquire),
        callback_calls: crate::player_activity_counts().audio_callback_calls,
        last_callback_period_nanoseconds: LAST_CALLBACK_PERIOD_NANOSECONDS.load(Ordering::Acquire),
        terminal_zero_delivered: LAST_PUBLISHED_PARK.load().terminal_zero_delivered(),
        hardware_stream_paused: LAST_PUBLISHED_PARK.load().hardware_stream_paused(),
    }
}

/// What the device asked of the callback over one second of its own time: a diagnostic of how a
/// real endpoint schedules its buffers, which no headless run reproduces.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioCallbackWindow {
    /// Counts completed windows; a reader that sees the same number twice has seen one window.
    pub sequence: u64,
    pub callbacks: u64,
    pub frames: u64,
    pub min_frames: u64,
    pub max_frames: u64,
    /// The longest time between two callbacks' starts.
    pub max_interval_ns: u64,
    /// The longest a callback took, from its start to its return.
    pub max_execution_ns: u64,
    /// Callbacks that took more than half the time their own buffer lasts.
    pub slow_callbacks: u64,
    /// The fewest frames a callback left in the ring.
    pub ring_floor_frames: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
/// Frame counters use PCM frames for PCM output and packed byte frames (eight one-bit samples
/// per channel) for native DSD and DoP. Clock positions always use nanoseconds.
pub struct AudioPlaybackMetrics {
    pub mix_frames: u64,
    pub mix_cycles: u64,
    pub mix_blocks: u64,
    pub upmix_hops: u64,
    pub limiter_engaged_frames: u64,
    pub callback_calls: u64,
    pub device_frames_requested: u64,
    pub media_frames_committed: u64,
    pub underrun_frames: u64,
    /// Callbacks that played silence for part of their buffer because the ring ran dry while the
    /// sound had more to give: each one is an audible gap. The end of the stream is not one.
    pub underrun_events: u64,
    /// Of `underrun_events`, those that found the decoder waiting on its input: the bytes had not
    /// arrived. The rest found input queued, so decoding, or waking the decoder, was late.
    pub underrun_events_awaiting_input: u64,
    /// The fewest frames a playing callback has left in the ring; `None` before one has played.
    /// How close the decoder came to being late.
    pub ring_floor_frames: Option<u64>,
    /// The frames the last callback left in the ring: how deep it is now.
    pub ring_frames: u64,
    /// PCM samples the callback handed the device beyond full scale (|x| > 1, after the gain): a
    /// float endpoint passes them to the audio engine, which clips them; an integer one is clipped
    /// in the conversion.
    pub clipped_samples: u64,
    /// Source frames the decoder handed its resampler; `decoded_media_frames` is what came out.
    pub resampler_input_frames: u64,
    /// Seeks the host asked for. Every re-anchor after a session's first is one of these or a
    /// rebind; a log that shows one without either has found something else.
    pub seeks_requested: u64,
    /// Whether the thread that hands the device its samples was registered as an MMCSS "Pro
    /// Audio" task: `None` until it has run, or where the driver schedules it (ASIO).
    pub output_thread_mmcss: Option<bool>,
    /// Whether the decode worker was registered as an MMCSS "Audio" task.
    pub decoder_thread_mmcss: Option<bool>,
    /// The callbacks of the last second of device time; `None` before the first one completed.
    pub callback_window: Option<AudioCallbackWindow>,
    pub decoded_media_frames: u64,
    pub decoded_chunks: u64,
    /// Codec frames that did not decode — a damaged frame, bytes after the last one — and were
    /// passed over. Each played as the silence of its span; the clock ran on through it.
    pub frames_passed_over: u64,
    /// Decoded samples that were not finite (NaN, ±∞) and played as silence.
    pub non_finite_samples: u64,
    /// Blocks the decoder handed over with no frames in them, skipped.
    pub empty_blocks: u64,
    /// Blocks whose presentation time went back before the block ahead of them, or was missing
    /// after one: each was placed at the end of the block ahead, so the clock never runs back.
    pub timestamps_clamped: u64,
    pub ramp_transitions_started: u64,
    pub ramp_transitions_completed: u64,
    pub ramp_evidence_mask: u32,
    pub ramp_shape_failures: u64,
    pub ramp_sample_frames: u64,
    pub terminal_zero_delivery_acks: u64,
    pub start_ramps: u64,
    pub pause_ramps: u64,
    pub resume_ramps: u64,
    pub seek_down_ramps: u64,
    pub seek_up_ramps: u64,
    pub dispose_ramps: u64,
    pub clock_generation: u64,
    pub clock_reanchors: u64,
    pub last_media_now_ns: Option<i64>,
    pub output_errors: u64,
    pub eof_reached: bool,
    pub device_rebinds: u64,
    /// Where the source lifecycle stands. The external observer reads the state machine here
    /// rather than inferring it from counters that only say something moved.
    pub source_state: AudioSourceState,
    /// The kind of the last failed open, if one failed. `None` until one does.
    pub source_failure: Option<AudioSourceKind>,
}

fn mmcss_state(cell: &AtomicU8) -> Option<bool> {
    match cell.load(Ordering::Acquire) {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

/// The session's wake, as the metrics hold it: who to tell when a flag the host reads to decide
/// its next step flips (`SharedMetrics::announce`). A no-op until a session gives it one.
pub(super) struct Announce(pub(super) Arc<dyn Fn() + Send + Sync>);

impl Default for Announce {
    fn default() -> Self {
        Self(Arc::new(|| {}))
    }
}

#[derive(Default)]
pub struct SharedMetrics {
    /// Called when `eof_reached`, `input_starved` or `input_ended` changes, from the thread that
    /// changed it: the end, buffering and failure are announced, not polled. Never from the
    /// real-time callback, which writes none of the three.
    pub(super) announce: Announce,
    pub(super) mix_frames: AtomicU64,
    pub(super) mix_cycles: AtomicU64,
    pub(super) mix_blocks: AtomicU64,
    pub(super) upmix_hops: AtomicU64,
    pub(super) limiter_engaged_frames: AtomicU64,
    pub(super) callback_calls: AtomicU64,
    pub(super) device_frames_requested: AtomicU64,
    pub(super) media_frames_committed: AtomicU64,
    pub(super) underrun_frames: AtomicU64,
    pub(super) underrun_events: AtomicU64,
    pub(super) underrun_events_awaiting_input: AtomicU64,
    /// `u64::MAX` less the ring floor, raised with `fetch_max`: zero, the derived default, is
    /// "no playing callback yet", and no ring is large enough to be mistaken for it.
    pub(super) ring_floor_complement: AtomicU64,
    pub(super) ring_frames: AtomicU64,
    pub(super) clipped_samples: AtomicU64,
    pub(super) seeks_requested: AtomicU64,
    pub(super) resampler_input_frames: AtomicU64,
    /// 0: not yet known; 1: registered; 2: refused.
    pub(super) output_thread_mmcss: AtomicU8,
    pub(super) decoder_thread_mmcss: AtomicU8,
    /// The last completed `AudioCallbackWindow`, published by the callback: its fields first,
    /// then `window_sequence` with Release, so a reader that sees the sequence unchanged across
    /// its reads has read one window.
    pub(super) window_sequence: AtomicU64,
    pub(super) window_callbacks: AtomicU64,
    pub(super) window_frames: AtomicU64,
    pub(super) window_min_frames: AtomicU64,
    pub(super) window_max_frames: AtomicU64,
    pub(super) window_max_interval_ns: AtomicU64,
    pub(super) window_max_execution_ns: AtomicU64,
    pub(super) window_slow_callbacks: AtomicU64,
    pub(super) window_ring_floor: AtomicU64,
    pub(super) decoded_media_frames: AtomicU64,
    pub(super) decoded_chunks: AtomicU64,
    pub(super) frames_passed_over: AtomicU64,
    pub(super) non_finite_samples: AtomicU64,
    pub empty_blocks: AtomicU64,
    pub timestamps_clamped: AtomicU64,
    pub(super) ramp_transitions_started: AtomicU64,
    pub(super) ramp_transitions_completed: AtomicU64,
    pub(super) ramp_evidence_mask: AtomicU32,
    pub(super) ramp_shape_failures: AtomicU64,
    pub(super) ramp_sample_frames: AtomicU64,
    pub(super) terminal_zero_delivery_acks: AtomicU64,
    pub(super) start_ramps: AtomicU64,
    pub(super) pause_ramps: AtomicU64,
    pub(super) resume_ramps: AtomicU64,
    pub(super) seek_down_ramps: AtomicU64,
    pub(super) seek_up_ramps: AtomicU64,
    pub(super) dispose_ramps: AtomicU64,
    pub(super) clock_reanchors: AtomicU64,
    pub(super) output_errors: AtomicU64,
    pub eof_reached: AtomicBool,
    /// The decode worker wanted a packet and the network had not delivered one.
    pub input_starved: AtomicBool,
    /// How much compressed sound is queued ahead of the decoder, in milliseconds.
    pub(super) input_queued_ms: AtomicU64,
    pub input_ended: AtomicBool,
    pub(super) device_rebinds: AtomicU64,
    pub(super) source_state: AtomicU8,
    pub(super) source_failure: AtomicU8,
}

impl SharedMetrics {
    /// Sets one of the three flags the host steps by (`announce`), and wakes it when the value
    /// changed. Not for the real-time callback: the wake may take a lock.
    pub(super) fn flag(&self, cell: &AtomicBool, value: bool) {
        if cell.swap(value, Ordering::AcqRel) != value {
            (self.announce.0)();
        }
    }

    /// The only writer of the lifecycle cell, and it writes only what the table returned — so a
    /// state the machine cannot reach cannot be published either.
    pub(super) fn step_source(&self, event: SourceEvent) {
        let next = source_step(self.source_state(), event);
        self.source_state.store(next as u8, Ordering::Release);
    }

    /// What one callback found: `silent` frames it wanted media for and the ring did not have,
    /// and the `left` frames still in the ring after it — which count toward the floor when it
    /// played `steady`ily throughout. Wait-free: callback threads call it.
    pub(super) fn record_callback_supply(&self, silent: usize, left: usize, steady: bool) {
        self.ring_frames.store(left as u64, Ordering::Relaxed);
        if steady {
            self.ring_floor_complement
                .fetch_max(u64::MAX - left as u64, Ordering::Relaxed);
        }
        if silent == 0 {
            return;
        }
        self.underrun_frames
            .fetch_add(silent as u64, Ordering::Relaxed);
        if self.eof_reached.load(Ordering::Acquire) {
            return;
        }
        self.underrun_events.fetch_add(1, Ordering::Relaxed);
        if self.input_starved.load(Ordering::Acquire) {
            self.underrun_events_awaiting_input
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Whether a thread's MMCSS registration took. Written once per thread; wait-free.
    pub(super) fn record_mmcss(cell: &AtomicU8, registered: bool) {
        cell.store(if registered { 1 } else { 2 }, Ordering::Relaxed);
    }

    /// A completed window, from the callback that completed it. Wait-free.
    pub(super) fn publish_window(&self, window: &AudioCallbackWindow) {
        self.window_callbacks
            .store(window.callbacks, Ordering::Relaxed);
        self.window_frames.store(window.frames, Ordering::Relaxed);
        self.window_min_frames
            .store(window.min_frames, Ordering::Relaxed);
        self.window_max_frames
            .store(window.max_frames, Ordering::Relaxed);
        self.window_max_interval_ns
            .store(window.max_interval_ns, Ordering::Relaxed);
        self.window_max_execution_ns
            .store(window.max_execution_ns, Ordering::Relaxed);
        self.window_slow_callbacks
            .store(window.slow_callbacks, Ordering::Relaxed);
        self.window_ring_floor
            .store(window.ring_floor_frames, Ordering::Relaxed);
        self.window_sequence.fetch_add(1, Ordering::Release);
    }

    fn window(&self) -> Option<AudioCallbackWindow> {
        for _ in 0..4 {
            let sequence = self.window_sequence.load(Ordering::Acquire);
            if sequence == 0 {
                return None;
            }
            let window = AudioCallbackWindow {
                sequence,
                callbacks: self.window_callbacks.load(Ordering::Relaxed),
                frames: self.window_frames.load(Ordering::Relaxed),
                min_frames: self.window_min_frames.load(Ordering::Relaxed),
                max_frames: self.window_max_frames.load(Ordering::Relaxed),
                max_interval_ns: self.window_max_interval_ns.load(Ordering::Relaxed),
                max_execution_ns: self.window_max_execution_ns.load(Ordering::Relaxed),
                slow_callbacks: self.window_slow_callbacks.load(Ordering::Relaxed),
                ring_floor_frames: self.window_ring_floor.load(Ordering::Relaxed),
            };
            std::sync::atomic::fence(Ordering::Acquire);
            if self.window_sequence.load(Ordering::Relaxed) == sequence {
                return Some(window);
            }
        }
        None
    }

    pub(super) fn source_state(&self) -> AudioSourceState {
        SOURCE_STATES[usize::from(self.source_state.load(Ordering::Acquire)).min(5)]
    }

    /// A failed open, classified and recorded together: the state says a generation failed, the
    /// kind says which of the three dependencies refused.
    pub(super) fn fail_source(&self, error: &AudioOpenError) {
        self.source_failure
            .store(error.kind() as u8 + 1, Ordering::Release);
        self.step_source(if matches!(error, AudioOpenError::Cancelled) {
            SourceEvent::Cancel
        } else {
            SourceEvent::Fail
        });
    }

    pub(super) fn snapshot(&self, clock: &AudioClock) -> AudioPlaybackMetrics {
        let clock_snapshot = clock.snapshot();
        AudioPlaybackMetrics {
            mix_frames: self.mix_frames.load(Ordering::Relaxed),
            mix_cycles: self.mix_cycles.load(Ordering::Relaxed),
            mix_blocks: self.mix_blocks.load(Ordering::Relaxed),
            upmix_hops: self.upmix_hops.load(Ordering::Relaxed),
            limiter_engaged_frames: self.limiter_engaged_frames.load(Ordering::Relaxed),
            callback_calls: self.callback_calls.load(Ordering::Acquire),
            device_frames_requested: self.device_frames_requested.load(Ordering::Acquire),
            media_frames_committed: self.media_frames_committed.load(Ordering::Acquire),
            underrun_frames: self.underrun_frames.load(Ordering::Acquire),
            underrun_events: self.underrun_events.load(Ordering::Acquire),
            underrun_events_awaiting_input: self
                .underrun_events_awaiting_input
                .load(Ordering::Acquire),
            ring_floor_frames: match self.ring_floor_complement.load(Ordering::Acquire) {
                0 => None,
                complement => Some(u64::MAX - complement),
            },
            ring_frames: self.ring_frames.load(Ordering::Acquire),
            clipped_samples: self.clipped_samples.load(Ordering::Acquire),
            seeks_requested: self.seeks_requested.load(Ordering::Acquire),
            resampler_input_frames: self.resampler_input_frames.load(Ordering::Acquire),
            output_thread_mmcss: mmcss_state(&self.output_thread_mmcss),
            decoder_thread_mmcss: mmcss_state(&self.decoder_thread_mmcss),
            callback_window: self.window(),
            decoded_media_frames: self.decoded_media_frames.load(Ordering::Acquire),
            decoded_chunks: self.decoded_chunks.load(Ordering::Acquire),
            frames_passed_over: self.frames_passed_over.load(Ordering::Acquire),
            non_finite_samples: self.non_finite_samples.load(Ordering::Relaxed),
            empty_blocks: self.empty_blocks.load(Ordering::Relaxed),
            timestamps_clamped: self.timestamps_clamped.load(Ordering::Relaxed),
            ramp_transitions_started: self.ramp_transitions_started.load(Ordering::Acquire),
            ramp_transitions_completed: self.ramp_transitions_completed.load(Ordering::Acquire),
            ramp_evidence_mask: self.ramp_evidence_mask.load(Ordering::Acquire),
            ramp_shape_failures: self.ramp_shape_failures.load(Ordering::Acquire),
            ramp_sample_frames: self.ramp_sample_frames.load(Ordering::Acquire),
            terminal_zero_delivery_acks: self.terminal_zero_delivery_acks.load(Ordering::Acquire),
            start_ramps: self.start_ramps.load(Ordering::Acquire),
            pause_ramps: self.pause_ramps.load(Ordering::Acquire),
            resume_ramps: self.resume_ramps.load(Ordering::Acquire),
            seek_down_ramps: self.seek_down_ramps.load(Ordering::Acquire),
            seek_up_ramps: self.seek_up_ramps.load(Ordering::Acquire),
            dispose_ramps: self.dispose_ramps.load(Ordering::Acquire),
            clock_generation: clock_snapshot.map_or(0, |snapshot| snapshot.generation),
            clock_reanchors: self.clock_reanchors.load(Ordering::Acquire),
            last_media_now_ns: clock_snapshot
                .and_then(|snapshot| i64::try_from(snapshot.media_now.as_nanoseconds()).ok()),
            output_errors: self.output_errors.load(Ordering::Acquire),
            eof_reached: self.eof_reached.load(Ordering::Acquire),
            device_rebinds: self.device_rebinds.load(Ordering::Acquire),
            source_state: self.source_state(),
            source_failure: match self.source_failure.load(Ordering::Acquire) {
                1 => Some(AudioSourceKind::Open),
                2 => Some(AudioSourceKind::Decode),
                3 => Some(AudioSourceKind::Device),
                _ => None,
            },
        }
    }
}
