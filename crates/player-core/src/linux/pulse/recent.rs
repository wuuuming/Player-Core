//! The sound last written to an output, kept so its end can be written again faded out.

use super::{FRAME_BYTES, RATE};
use std::collections::VecDeque;
use std::time::Duration;

/// The bytes last written to an output (interleaved `f32le` frames), as far back as the output
/// can hold them ahead of what is heard, and how many were written in all: a byte's index is
/// its place in everything written.
pub struct Recent {
    bytes: VecDeque<u8>,
    written: u64,
}

impl Recent {
    /// Half a second: more than an output holds ahead of what is heard.
    const KEPT: usize = (RATE * FRAME_BYTES / 2) as usize;

    pub fn new() -> Self {
        Self {
            bytes: VecDeque::with_capacity(Self::KEPT),
            written: 0,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend(bytes);
        self.written = self.written.saturating_add(bytes.len() as u64);
        let over = self.bytes.len().saturating_sub(Self::KEPT);
        self.bytes.drain(..over);
    }

    /// Bytes written in all.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// What was written from byte `from` (a frame's start) to the end, faded out: from full level
    /// to silence over `fade`, or over what there is when that is less, then silence. `None` when
    /// nothing kept lies there.
    pub fn faded_from(&self, from: u64, fade: Duration) -> Option<Vec<u8>> {
        let oldest = self.written - self.bytes.len() as u64;
        if from < oldest || from >= self.written {
            return None;
        }
        let frame = FRAME_BYTES as usize;
        let skip = super::whole_frames(usize::try_from(from - oldest).ok()?);
        let mut out: Vec<u8> = self.bytes.range(skip..).copied().collect();
        let frames = out.len() / frame;
        let fade = (u128::from(RATE) * fade.as_nanos()).div_ceil(1_000_000_000);
        let fade = usize::try_from(fade).unwrap_or(usize::MAX).min(frames);
        for (i, frame) in out.chunks_exact_mut(frame).enumerate() {
            // Linear, its first frame at full level and its last silent (as the declick's ramp).
            let gain = if i + 1 < fade {
                (fade - 1 - i) as f32 / (fade - 1) as f32
            } else {
                0.0
            };
            for sample in frame.chunks_exact_mut(4) {
                let value = super::sample(sample);
                sample.copy_from_slice(&(value * gain).to_le_bytes());
            }
        }
        Some(out)
    }
}
