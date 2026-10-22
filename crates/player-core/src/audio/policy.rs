//! The buffer and ramp dimensions one session runs on.

use super::error::AudioOpenError;
use std::num::{NonZeroU32, NonZeroUsize};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioGainCurve {
    Linear,
}

/// Values in this policy are supplied by the host: ramp shape and duration and buffer sizing
/// are a product's choice, so player-core has no numeric defaults of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioPolicy {
    pub queue_capacity_frames: NonZeroUsize,
    pub prefill_frames: NonZeroUsize,
    pub refill_low_frames: NonZeroUsize,
    pub refill_high_frames: NonZeroUsize,
    pub ramp_frames: NonZeroU32,
    /// How long a song that sounds fades out when the player is disposed (the host leaves it
    /// for another): at most this, as far as the sound decoded ahead reaches.
    pub leave_frames: NonZeroU32,
    pub gain_curve: AudioGainCurve,
}

impl AudioPolicy {
    /// The provisional release policy. These values are measurement defaults a host may
    /// override, not acceptance thresholds.
    pub fn provisional_for_sample_rate(sample_rate: NonZeroU32) -> Self {
        AudioPolicyTimes::PROVISIONAL.at_sample_rate(sample_rate)
    }

    /// The prefill may be smaller than the low watermark: a start or a seek then plays as soon as
    /// the prefill is in, and the worker, finding the ring below its low watermark, goes straight
    /// on to fill it to the high one. That is how a deep ring keeps a seek prompt.
    pub(super) fn validate(self) -> Result<Self, AudioOpenError> {
        let capacity = self.queue_capacity_frames.get();
        let low = self.refill_low_frames.get();
        let prefill = self.prefill_frames.get();
        let high = self.refill_high_frames.get();
        if self.ramp_frames.get() < 3 || self.leave_frames.get() < 3 {
            return Err(AudioOpenError::InvalidPolicy(
                "ramp_frames and leave_frames must contain both endpoints and an interior sample"
                    .to_owned(),
            ));
        }
        if !(low <= high && prefill <= high && high < capacity) {
            return Err(AudioOpenError::InvalidPolicy(format!(
                "audio watermarks must satisfy low <= high, prefill <= high and high < capacity; got low {low}, prefill {prefill}, high {high}, capacity {capacity}"
            )));
        }
        Ok(self)
    }
}

/// A policy stated in time, which is how a caller asks for one: it cannot know the rate its
/// device will open at, and that rate changes at a rebind — an ASIO driver follows each song's
/// own. Every binding turns it into frames at the rate it opened at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioPolicyTimes {
    pub queue_capacity: Duration,
    pub prefill: Duration,
    pub refill_low: Duration,
    pub refill_high: Duration,
    /// The fade of a play, pause, seek: a declick.
    pub ramp: Duration,
    /// The fade-out of a song that sounds as the player is disposed (`AudioPolicy::leave_frames`).
    pub leave: Duration,
}

impl AudioPolicy {
    /// The ring holds less than the refill mark: the decode worker is woken to fill it.
    pub(crate) fn ring_low(&self, available: usize) -> bool {
        available < self.refill_low_frames.get()
    }
}

impl AudioPolicyTimes {
    /// The provisional policy: a quarter of a second of sound decoded ahead, refilled below
    /// 60 ms; 8 ms declicks, a tenth of a second to leave a song. A session opened without a
    /// policy runs on it.
    pub const PROVISIONAL: Self = Self {
        queue_capacity: Duration::from_millis(250),
        prefill: Duration::from_millis(80),
        refill_low: Duration::from_millis(60),
        refill_high: Duration::from_millis(180),
        ramp: crate::dsp::DECLICK,
        leave: crate::dsp::LEAVE,
    };

    /// A binding's policy: the asked times, else the provisional ones, at the output's rate,
    /// validated.
    pub(super) fn for_output(
        requested: Option<Self>,
        sample_rate: NonZeroU32,
    ) -> Result<AudioPolicy, AudioOpenError> {
        requested
            .unwrap_or(Self::PROVISIONAL)
            .at_sample_rate(sample_rate)
            .validate()
    }

    /// These times in frames at `sample_rate`, each rounded up. A ramp (and the leave) keeps at
    /// least three frames: its two endpoints and one between.
    pub fn at_sample_rate(self, sample_rate: NonZeroU32) -> AudioPolicy {
        let frames = |time: Duration| {
            let frames = (u128::from(sample_rate.get()) * time.as_nanos()).div_ceil(1_000_000_000);
            NonZeroUsize::new(usize::try_from(frames).unwrap_or(usize::MAX))
                .unwrap_or(NonZeroUsize::MIN)
        };
        let ramp = |time: Duration| {
            NonZeroU32::MIN
                .saturating_add(u32::try_from(frames(time).get()).unwrap_or(u32::MAX).max(3) - 1)
        };
        AudioPolicy {
            queue_capacity_frames: frames(self.queue_capacity),
            prefill_frames: frames(self.prefill),
            refill_low_frames: frames(self.refill_low),
            refill_high_frames: frames(self.refill_high),
            ramp_frames: ramp(self.ramp),
            leave_frames: ramp(self.leave),
            gain_curve: AudioGainCurve::Linear,
        }
    }
}
