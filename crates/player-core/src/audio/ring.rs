//! The lock-free sample ring the decode worker fills and the real-time callback drains.

use super::error::AudioOpenError;
use std::cell::UnsafeCell;
use std::num::NonZeroUsize;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Which of two monotonically advancing frame cursors is further along. Wrap-safe: the cursors
/// stay within one `usize` half-range of each other, so their signed difference orders them even
/// across a counter wrap.
pub(super) fn later_cursor(left: usize, right: usize) -> usize {
    if (left.wrapping_sub(right) as isize) > 0 {
        left
    } else {
        right
    }
}

pub struct AudioRing {
    samples: RingSamples,
    pub(super) capacity_frames: usize,
    pub(super) channels: usize,
    /// Producer cursor. Written only by the decoder worker; never decreases.
    pub(super) write_frame: AtomicUsize,
    /// Consumer cursor. Written only by the cpal callback; never decreases.
    pub(super) read_frame: AtomicUsize,
    /// Producer-owned discard barrier: every frame before it is void. Written only by the decoder
    /// worker, always to that worker's own `write_frame`, so it can neither pass the producer
    /// cursor nor decrease.
    pub(super) discard_frame: AtomicUsize,
}

enum RingSamples {
    Pcm(Box<[UnsafeCell<f32>]>),
    Dsd(Box<[UnsafeCell<u8>]>),
}

// SAFETY: the ring is shared between exactly the threads below, and its slots are reached only
// through the protocol described here. Every counter has exactly one writer: the decoder worker owns `write_frame` and `discard_frame`,
// the cpal callback owns `read_frame`. `later_cursor(read, discard) <= write` therefore holds with
// no external protocol, and publication of the sample slots occurs by Release/Acquire on
// `write_frame`. The slots themselves still require that the producer only overwrite frames the
// callback is not sampling: steady state is guaranteed by the free-space computation in
// `push_interleaved`, and a discard is guaranteed by `CallbackControl`'s terminal-zero handshake.
// The owner thread additionally reads *only* those counters, through `AudioOutput::eof_drained`,
// to decide when the sound has played out.
unsafe impl Send for AudioRing {}
// SAFETY: as for `Send`, above.
unsafe impl Sync for AudioRing {}

impl AudioRing {
    pub fn new(
        capacity_frames: NonZeroUsize,
        channels: NonZeroUsize,
    ) -> Result<Self, AudioOpenError> {
        Self::with_encoding(capacity_frames, channels, false)
    }

    pub(super) fn with_encoding(
        capacity_frames: NonZeroUsize,
        channels: NonZeroUsize,
        dsd: bool,
    ) -> Result<Self, AudioOpenError> {
        let sample_capacity = capacity_frames
            .get()
            .checked_mul(channels.get())
            .ok_or_else(|| {
                AudioOpenError::InvalidPolicy("audio queue size overflowed".to_owned())
            })?;
        Ok(Self {
            samples: if dsd {
                RingSamples::Dsd(
                    (0..sample_capacity)
                        .map(|_| UnsafeCell::new(0x69))
                        .collect(),
                )
            } else {
                RingSamples::Pcm((0..sample_capacity).map(|_| UnsafeCell::new(0.0)).collect())
            },
            capacity_frames: capacity_frames.get(),
            channels: channels.get(),
            write_frame: AtomicUsize::new(0),
            read_frame: AtomicUsize::new(0),
            discard_frame: AtomicUsize::new(0),
        })
    }

    /// First frame the consumer may still play: its own cursor, or the producer's discard barrier
    /// when the producer has voided everything the consumer had left.
    pub(super) fn consumer_floor(&self) -> usize {
        later_cursor(
            self.read_frame.load(Ordering::Acquire),
            self.discard_frame.load(Ordering::Acquire),
        )
    }

    /// `(floor, frames)`: where the consumer may start reading and how many frames follow it. The
    /// producer cursor is loaded first, so a discard landing mid-call can only shrink this window
    /// — it can never manufacture readable frames.
    pub fn consumer_window(&self) -> (usize, usize) {
        let write = self.write_frame.load(Ordering::Acquire);
        let floor = self.consumer_floor();
        let behind = write.wrapping_sub(floor) as isize;
        debug_assert!(
            behind <= self.capacity_frames as isize,
            "audio ring producer ran more than one capacity ahead of the consumer floor"
        );
        (floor, behind.max(0) as usize)
    }

    pub fn available_frames(&self) -> usize {
        self.consumer_window().1
    }

    /// How many frames the producer may push now. Producer-only, for the reason
    /// `push_interleaved` gives: the consumer only ever makes it larger.
    pub(super) fn free_frames(&self) -> usize {
        let filled = self
            .write_frame
            .load(Ordering::Relaxed)
            .wrapping_sub(self.consumer_floor());
        self.capacity_frames.saturating_sub(filled)
    }

    pub(super) fn push_interleaved(&self, samples: &[f32]) -> usize {
        debug_assert_eq!(samples.len() % self.channels, 0);
        let requested_frames = samples.len() / self.channels;
        let write = self.write_frame.load(Ordering::Relaxed);
        // The producer owns `write_frame` and `discard_frame`, and `read_frame` can only move
        // toward a write cursor the producer has already published, so this difference is exact:
        // it can be neither negative nor larger than the capacity.
        let filled = write.wrapping_sub(self.consumer_floor());
        debug_assert!(
            filled <= self.capacity_frames,
            "audio ring holds more than its capacity"
        );
        let frames = requested_frames.min(self.capacity_frames.saturating_sub(filled));
        let RingSamples::Pcm(storage) = &self.samples else {
            unreachable!("PCM producer requires a PCM ring")
        };
        for frame in 0..frames {
            let destination_frame = write.wrapping_add(frame) % self.capacity_frames;
            let destination = destination_frame * self.channels;
            let source = frame * self.channels;
            // Only free slots are written. The allocation and its occupied slots are shared
            // with the consumer, so no mutable reference covers the whole allocation.
            // SAFETY: `destination` starts a whole frame inside the ring (`destination_frame` <
            // capacity) and `source` one inside `samples` (`frame` < `frames` ≤ its frame count);
            // the frame is free, so the consumer reads none of it (the ring's protocol).
            unsafe {
                ptr::copy_nonoverlapping(
                    samples.as_ptr().add(source),
                    storage[destination].get(),
                    self.channels,
                );
            }
        }
        self.write_frame
            .store(write.wrapping_add(frames), Ordering::Release);
        frames
    }

    pub fn sample(&self, absolute_frame: usize, channel: usize) -> f32 {
        debug_assert!(channel < self.channels);
        let frame = absolute_frame % self.capacity_frames;
        let RingSamples::Pcm(storage) = &self.samples else {
            unreachable!("PCM consumer requires a PCM ring")
        };
        // SAFETY: an occupied slot (the caller samples only published frames), which the producer
        // does not write until the consumer releases it (the ring's protocol).
        unsafe { *storage[frame * self.channels + channel].get() }
    }

    pub(super) fn push_dsd(&self, bytes: &[u8]) -> usize {
        let write = self.write_frame.load(Ordering::Relaxed);
        // Exact for the reason `push_interleaved` gives; saturating as there all the same.
        let filled = write.wrapping_sub(self.consumer_floor());
        debug_assert!(
            filled <= self.capacity_frames,
            "audio ring holds more than its capacity"
        );
        let frames = (bytes.len() / self.channels).min(self.capacity_frames.saturating_sub(filled));
        let RingSamples::Dsd(storage) = &self.samples else {
            unreachable!("DSD producer requires a DSD ring")
        };
        for frame in 0..frames {
            let destination = (write.wrapping_add(frame) % self.capacity_frames) * self.channels;
            let source = frame * self.channels;
            // SAFETY: as for PCM: a whole free frame of the ring, a whole frame of `bytes`.
            unsafe {
                ptr::copy_nonoverlapping(
                    bytes.as_ptr().add(source),
                    storage[destination].get(),
                    self.channels,
                );
            }
        }
        self.write_frame
            .store(write.wrapping_add(frames), Ordering::Release);
        frames
    }

    #[cfg(any(feature = "asio", test))]
    pub(super) fn dsd_sample(&self, absolute_frame: usize, channel: usize) -> u8 {
        let RingSamples::Dsd(storage) = &self.samples else {
            unreachable!("DSD consumer requires a DSD ring")
        };
        // SAFETY: as for `sample`: an occupied slot, which the producer does not write until
        // the consumer releases it.
        unsafe { *storage[(absolute_frame % self.capacity_frames) * self.channels + channel].get() }
    }

    pub(super) fn commit_read(&self, read: usize, frames: usize) {
        self.read_frame
            .store(read.wrapping_add(frames), Ordering::Release);
    }

    /// Voids every frame the ring currently holds. Producer-only: it advances the producer's own
    /// barrier to the producer's own cursor and never touches `read_frame`, so a callback that
    /// commits across it can only move `read_frame` forward into ground the barrier already
    /// covers. Slot-overwrite safety after a discard still comes from the terminal-zero handshake.
    pub fn discard_all(&self) {
        self.discard_frame
            .store(self.write_frame.load(Ordering::Relaxed), Ordering::Release);
    }
}
