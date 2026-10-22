//! The park posture cell: the platform's own record of whether the output is parked.

use std::sync::atomic::{AtomicU8, Ordering};

/// The park posture most recently published by ANY session, for `audio_runtime_counts` only.
/// Each session keeps its own cell (`AudioSession::park`): two sessions in one process — one
/// parked while another plays — publish different postures, and a reader that saw the other's
/// played a paused stream's ramp into a stopped callback.
#[cfg(feature = "instrument")]
pub(super) static LAST_PUBLISHED_PARK: AudioParkCell = AudioParkCell::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AudioParkState {
    /// Callback delivery is live and no zero ramp stands.
    Live,
    /// The terminal zero ramp reached the device; the hardware stream is still running.
    RampedToZero,
    /// The zero ramp reached the device and `Stream::pause()` then succeeded.
    HardwareParked,
}

impl AudioParkState {
    const LIVE: u8 = 0;
    const RAMPED_TO_ZERO: u8 = 1;
    const HARDWARE_PARKED: u8 = 2;

    pub(super) fn from_code(code: u8) -> Self {
        match code {
            Self::HARDWARE_PARKED => Self::HardwareParked,
            Self::RAMPED_TO_ZERO => Self::RampedToZero,
            _ => Self::Live,
        }
    }

    pub(super) fn code(self) -> u8 {
        match self {
            Self::Live => Self::LIVE,
            Self::RampedToZero => Self::RAMPED_TO_ZERO,
            Self::HardwareParked => Self::HARDWARE_PARKED,
        }
    }

    /// The terminal zero ramp has been delivered. A parked stream stays parked.
    pub(super) fn ramped_to_zero(self) -> Self {
        match self {
            Self::HardwareParked => Self::HardwareParked,
            Self::Live | Self::RampedToZero => Self::RampedToZero,
        }
    }

    /// `Stream::play()` succeeded: the hardware runs again while the zero ramp still stands.
    pub(super) fn hardware_running(self) -> Self {
        match self {
            Self::HardwareParked | Self::RampedToZero => Self::RampedToZero,
            Self::Live => Self::Live,
        }
    }

    #[cfg(feature = "instrument")]
    pub(super) fn terminal_zero_delivered(self) -> bool {
        !matches!(self, Self::Live)
    }

    pub(super) fn hardware_stream_paused(self) -> bool {
        matches!(self, Self::HardwareParked)
    }
}

pub(super) struct AudioParkCell(AtomicU8);

impl AudioParkCell {
    pub(super) const fn new() -> Self {
        Self(AtomicU8::new(AudioParkState::LIVE))
    }

    pub(super) fn load(&self) -> AudioParkState {
        AudioParkState::from_code(self.0.load(Ordering::Acquire))
    }

    pub(super) fn store(&self, state: AudioParkState) {
        self.0.store(state.code(), Ordering::Release);
        #[cfg(feature = "instrument")]
        LAST_PUBLISHED_PARK.0.store(state.code(), Ordering::Release);
    }
}
