//! The media clock: what the listener hears now, from the decode worker's anchor and the
//! callback's delivery; the playback rate it advances at; the generation counter.

use super::error::AudioError;
use crate::NormalizedTimestamp;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

pub(super) const PLAYBACK_RATE_SCALE: u64 = 1u64 << 32;
pub(super) const MIN_PLAYBACK_RATE: f32 = 0.5;
pub(super) const MAX_PLAYBACK_RATE: f32 = 2.0;

/// The generation `AudioOutput::open` binds on. Rebinds advance from here.
pub(super) const FIRST_GENERATION: u64 = 1;

/// The decoder/clock generation. `advance` is the only mutator and there is no assignment
/// interface, so a number the worker or the media clock has already seen can never be handed
/// out a second time — the `c <= g` invariant holds by construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct GenerationCounter(u64);

impl GenerationCounter {
    pub(super) fn new(initial: u64) -> Self {
        Self(initial)
    }

    pub(super) fn current(self) -> u64 {
        self.0
    }

    pub(super) fn advance(&mut self) -> u64 {
        self.0 = self.0.saturating_add(1);
        self.0
    }
}

#[derive(Debug)]
pub(crate) struct PlaybackRateControl {
    pub(super) q32: AtomicU64,
}

impl Default for PlaybackRateControl {
    fn default() -> Self {
        Self {
            q32: AtomicU64::new(PLAYBACK_RATE_SCALE),
        }
    }
}

impl PlaybackRateControl {
    pub(crate) fn set(&self, rate: f32) -> bool {
        if !rate.is_finite() || !(MIN_PLAYBACK_RATE..=MAX_PLAYBACK_RATE).contains(&rate) {
            return false;
        }
        let q32 = (f64::from(rate) * PLAYBACK_RATE_SCALE as f64).round() as u64;
        self.q32.store(q32, Ordering::Release);
        true
    }

    /// Playback runs at normal speed (1x): the only speed the bit-perfect paths (native DSD, DoP,
    /// the volume bypassed) play at.
    pub(crate) fn at_normal_speed(&self) -> bool {
        self.q32() == PLAYBACK_RATE_SCALE
    }

    pub(crate) fn get(&self) -> f32 {
        (self.q32.load(Ordering::Acquire) as f64 / PLAYBACK_RATE_SCALE as f64) as f32
    }

    pub(super) fn q32(&self) -> u64 {
        self.q32.load(Ordering::Acquire)
    }
}

/// Where the clock stands at one instant: the generation and the media time the listener hears.
#[derive(Clone, Copy, Debug)]
pub(super) struct MediaClockSnapshot {
    pub(super) generation: u64,
    pub(super) media_now: NormalizedTimestamp,
}

/// The media clock: two independent single-writer publications.
///
/// * The **anchor** (`generation`, `anchor_ns`) is written only by the decode worker, inside the
///   odd `anchor_sequence` window. `anchor_sequence == 0` is the "never anchored" state, so no
///   timestamp value is reserved as a sentinel.
/// * The **delivery** (`generation`, `elapsed_ns`, `playback_delay_ns`) is written only by the
///   cpal callback, inside the odd `delivery_sequence` window, and carries the generation it was
///   accumulated for. `elapsed_ns` is media time the callback has WRITTEN into the device buffer;
///   `playback_delay_ns` is how far in the future the device says that write becomes audible
///   (cpal's `playback - callback`). The master clock is what the listener HEARS, so the published
///   position is `anchor + elapsed - playback_delay`; without the subtraction it would run a
///   device buffer ahead of the ear.
///
/// Every field therefore has exactly one writer, and every write advances the sequence that
/// guards it. A reader adds elapsed time to an anchor only when the two publications name the
/// same generation, which makes "one generation's audio credited to another generation's anchor"
/// unrepresentable rather than merely unlikely.
///
/// Nothing here is device-scoped. The output sample rate belongs to the callback, the only code
/// that knows which device it is serving: a rebind from 48 kHz headphones to 44.1 kHz speakers
/// changes it, and a copy here would run the clock at the wrong speed. What is left is exactly
/// what survives a device change.
pub struct AudioClock {
    pub(super) anchor_sequence: AtomicU64,
    pub(super) anchor_generation: AtomicU64,
    pub(super) anchor_ns: AtomicI64,
    pub(super) delivery_sequence: AtomicU64,
    pub(super) delivery_generation: AtomicU64,
    pub(super) delivery_elapsed_ns: AtomicU64,
    pub(super) delivery_playback_delay_ns: AtomicU64,
    pub(super) playback_rate: Arc<PlaybackRateControl>,
}

impl AudioClock {
    /// A clock at the normal rate, for the tests (the product builds one only through
    /// `with_playback_rate`).
    #[cfg(any(test, feature = "test-support"))]
    #[allow(clippy::new_without_default)] // a test's; the product's clock always takes its rate control
    pub fn new() -> Self {
        Self::with_playback_rate(Arc::new(PlaybackRateControl::default()))
    }

    pub(super) fn with_playback_rate(playback_rate: Arc<PlaybackRateControl>) -> Self {
        Self {
            anchor_sequence: AtomicU64::new(0),
            anchor_generation: AtomicU64::new(0),
            anchor_ns: AtomicI64::new(0),
            delivery_sequence: AtomicU64::new(0),
            delivery_generation: AtomicU64::new(0),
            delivery_elapsed_ns: AtomicU64::new(0),
            delivery_playback_delay_ns: AtomicU64::new(0),
            playback_rate,
        }
    }

    /// Decode worker only. Publishes a new generation and its presentation anchor.
    pub(super) fn reanchor(
        &self,
        generation: u64,
        anchor: NormalizedTimestamp,
    ) -> Result<(), AudioError> {
        let anchor_ns = i64::try_from(anchor.as_nanoseconds())
            .map_err(|_| AudioError::ClockRange(anchor.as_nanoseconds()))?;
        self.anchor_sequence.fetch_add(1, Ordering::AcqRel);
        self.anchor_generation.store(generation, Ordering::Relaxed);
        self.anchor_ns.store(anchor_ns, Ordering::Relaxed);
        self.anchor_sequence.fetch_add(1, Ordering::Release);
        Ok(())
    }

    /// cpal callback only, wait-free. Publishes how much media time this callback has delivered
    /// *for the generation it observed*, together with the device's own report of how far ahead of
    /// the ear that delivery is. A generation it no longer serves can never be credited.
    pub(super) fn publish_delivery(
        &self,
        generation: u64,
        elapsed_nanoseconds: u64,
        playback_delay_nanoseconds: u64,
    ) {
        self.delivery_sequence.fetch_add(1, Ordering::AcqRel);
        self.delivery_generation
            .store(generation, Ordering::Relaxed);
        self.delivery_elapsed_ns
            .store(elapsed_nanoseconds, Ordering::Relaxed);
        self.delivery_playback_delay_ns
            .store(playback_delay_nanoseconds, Ordering::Relaxed);
        self.delivery_sequence.fetch_add(1, Ordering::Release);
    }

    pub(super) fn anchor(&self) -> Option<(u64, i64)> {
        loop {
            let before = self.anchor_sequence.load(Ordering::Acquire);
            if before == 0 {
                // The sequence counter's own initial value is the "never anchored" state; no
                // timestamp is reserved, so `i64::MIN` is an ordinary anchor.
                return None;
            }
            if before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let generation = self.anchor_generation.load(Ordering::Relaxed);
            let anchor_ns = self.anchor_ns.load(Ordering::Relaxed);
            if self.anchor_sequence.load(Ordering::Acquire) != before {
                continue;
            }
            return Some((generation, anchor_ns));
        }
    }

    /// Media time written for `generation`, and the device's playback delay on that write. A
    /// delivery stamped with another generation contributes neither.
    pub(super) fn delivery(&self, generation: u64) -> (u64, u64) {
        loop {
            let before = self.delivery_sequence.load(Ordering::Acquire);
            if before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let delivered_generation = self.delivery_generation.load(Ordering::Relaxed);
            let elapsed_nanoseconds = self.delivery_elapsed_ns.load(Ordering::Relaxed);
            let playback_delay_nanoseconds =
                self.delivery_playback_delay_ns.load(Ordering::Relaxed);
            if self.delivery_sequence.load(Ordering::Acquire) != before {
                continue;
            }
            return if delivered_generation == generation {
                (elapsed_nanoseconds, playback_delay_nanoseconds)
            } else {
                (0, 0)
            };
        }
    }

    pub(super) fn snapshot(&self) -> Option<MediaClockSnapshot> {
        let (generation, anchor_ns) = self.anchor()?;
        let (delivered_ns, playback_delay_ns) = self.delivery(generation);
        // The OUTPUT position: what the device is playing now, not what the callback has queued
        // for it to play later.
        let media_now = i128::from(anchor_ns)
            .saturating_add(i128::from(delivered_ns))
            .saturating_sub(i128::from(playback_delay_ns));
        Some(MediaClockSnapshot {
            generation,
            media_now: NormalizedTimestamp::from_nanoseconds(media_now),
        })
    }

    pub(super) fn generation(&self) -> u64 {
        self.anchor_generation.load(Ordering::Acquire)
    }

    pub(super) fn playback_rate_q32(&self) -> u64 {
        self.playback_rate.q32()
    }
}
