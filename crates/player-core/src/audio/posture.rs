//! The transport posture and the one table that turns a posture edge into hardware.

use super::callback::{CallbackControl, RampKind, MODE_HELD, MODE_RUNNING};
use super::device::OutputStream;
use super::error::AudioError;
use super::park::{AudioParkCell, AudioParkState};
use super::worker::WorkerHandle;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AudioOutputPhase {
    Prepared,
    Running,
    Paused,
    Disposed,
}

impl AudioOutputPhase {
    pub(super) fn is_running(self) -> bool {
        self == Self::Running
    }

    pub(super) fn is_disposed(self) -> bool {
        self == Self::Disposed
    }

    pub(super) fn held(self) -> Self {
        match self {
            Self::Running => Self::Paused,
            Self::Prepared | Self::Paused | Self::Disposed => self,
        }
    }

    pub(super) fn after_seek(self, resume: bool) -> Self {
        match (self, resume) {
            (Self::Disposed, _) => Self::Disposed,
            (_, true) => Self::Running,
            (_, false) => self.held(),
        }
    }

    pub(super) fn after_activation(self) -> Self {
        match self {
            Self::Disposed => Self::Disposed,
            Self::Prepared | Self::Running | Self::Paused => Self::Running,
        }
    }
}

/// What the transport wants, as one value. Written only by the media loop, read only by the audio
/// owner thread, and encoded in one word so a reader can never see half of it.
///
/// `seeking` is not decoration: it is what makes a hold a SEEK-DOWN rather than a PAUSE, and a
/// release a SEEK-UP rather than a RESUME. The ramp kind is a function of the posture edge, not
/// an instruction a call site chooses, so "a SeekUp ramp emitted while the transport is paused"
/// has no way to be spelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TransportPosture {
    pub(super) phase: AudioOutputPhase,
    pub(super) seeking: bool,
}

impl TransportPosture {
    pub(super) const fn code(self) -> u8 {
        let phase = match self.phase {
            AudioOutputPhase::Prepared => 0,
            AudioOutputPhase::Running => 1,
            AudioOutputPhase::Paused => 2,
            AudioOutputPhase::Disposed => 3,
        };
        phase | ((self.seeking as u8) << 2)
    }

    pub(super) fn from_code(code: u8) -> Self {
        let phase = match code & 0b11 {
            0 => AudioOutputPhase::Prepared,
            1 => AudioOutputPhase::Running,
            2 => AudioOutputPhase::Paused,
            _ => AudioOutputPhase::Disposed,
        };
        Self {
            phase,
            seeking: code & 0b100 != 0,
        }
    }

    /// Whether the callback should be delivering media at unity gain.
    pub(super) const fn audible(self) -> bool {
        matches!(self.phase, AudioOutputPhase::Running)
    }

    /// Whether the cpal stream should be running. A seek hold rings the gain down but leaves the
    /// hardware alive: parking and unparking the device on every seek would put a callback
    /// acknowledgement inside a seek storm. A transport that has never started rests like a
    /// paused one: a song opened and not played (a player that restored where it was) must not
    /// keep the device running silence, waking a hundred times a second, until it is.
    pub(super) const fn hardware_runs(self) -> bool {
        match self.phase {
            AudioOutputPhase::Running => true,
            AudioOutputPhase::Prepared | AudioOutputPhase::Paused => self.seeking,
            AudioOutputPhase::Disposed => false,
        }
    }
}

pub(super) struct PostureCell(AtomicU8);

impl PostureCell {
    pub(super) fn new(posture: TransportPosture) -> Self {
        Self(AtomicU8::new(posture.code()))
    }

    pub(super) fn store(&self, posture: TransportPosture) {
        self.0.store(posture.code(), Ordering::Release);
    }

    pub(super) fn load(&self) -> TransportPosture {
        TransportPosture::from_code(self.0.load(Ordering::Acquire))
    }
}

/// One hardware sequence. Total over (applied, wanted, park), so "a posture edge nobody described"
/// has no case, and it is the single description used by `open`, by every rebind commit, and by
/// every transport command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PostureStep {
    pub(super) play_hardware: bool,
    pub(super) ramp: Option<RampKind>,
    pub(super) ramp_mode: u8,
    pub(super) park_decoder: Option<bool>,
    pub(super) park_hardware: bool,
}

impl PostureStep {
    /// The step's decoder park, if it has one, applied to `worker`.
    fn park_decoder_on(self, worker: &WorkerHandle) -> Result<(), AudioError> {
        match self.park_decoder {
            Some(parked) => worker.set_parked(parked),
            None => Ok(()),
        }
    }

    /// The step's ramp, if it has one, asked of the callback.
    fn ramp_on(self, control: &CallbackControl) -> Result<(), AudioError> {
        match self.ramp {
            Some(kind) => control.request(self.ramp_mode, kind),
            None => Ok(()),
        }
    }

    pub(super) const IDLE: Self = Self {
        play_hardware: false,
        ramp: None,
        ramp_mode: MODE_HELD,
        park_decoder: None,
        park_hardware: false,
    };

    /// What the platform's own park record becomes once this step has run.
    pub(super) fn park_after(
        self,
        park: AudioParkState,
        wanted: TransportPosture,
    ) -> AudioParkState {
        if self.park_hardware {
            AudioParkState::HardwareParked
        } else if wanted.audible() && (self.ramp.is_some() || self.play_hardware) {
            AudioParkState::Live
        } else if self.ramp.is_some() {
            park.ramped_to_zero()
        } else if self.play_hardware {
            park.hardware_running()
        } else {
            park
        }
    }
}

/// The ramp kind is a function of the posture edge, never an argument.
pub(super) fn posture_step(
    applied: TransportPosture,
    wanted: TransportPosture,
    park: AudioParkState,
) -> PostureStep {
    if applied == wanted {
        return PostureStep::IDLE;
    }
    let play_hardware = wanted.hardware_runs() && park.hardware_stream_paused();
    let park_hardware = !wanted.hardware_runs() && !park.hardware_stream_paused();
    match (applied.audible(), wanted.audible(), wanted.phase) {
        (_, _, AudioOutputPhase::Disposed) => PostureStep {
            play_hardware: false,
            ramp: (!park.hardware_stream_paused()).then_some(RampKind::Dispose),
            ramp_mode: MODE_HELD,
            park_decoder: None,
            park_hardware: false,
        },
        (false, true, _) => PostureStep {
            play_hardware,
            ramp: Some(if applied.seeking {
                RampKind::SeekUp
            } else if applied.phase == AudioOutputPhase::Prepared {
                RampKind::Start
            } else {
                RampKind::Resume
            }),
            ramp_mode: MODE_RUNNING,
            park_decoder: Some(false),
            park_hardware: false,
        },
        (true, false, _) => PostureStep {
            play_hardware: false,
            ramp: Some(if wanted.seeking {
                RampKind::SeekDown
            } else {
                RampKind::Pause
            }),
            ramp_mode: MODE_HELD,
            park_decoder: (!wanted.seeking).then_some(true),
            park_hardware,
        },
        // Held to held: no gain moves. The hardware may still have to start (a fresh binding
        // committed into `Prepared`) or park (a seek that completed while the transport is paused).
        (false, false, _) => PostureStep {
            play_hardware,
            ramp: None,
            ramp_mode: MODE_HELD,
            park_decoder: if park_hardware { Some(true) } else { None },
            park_hardware,
        },
        (true, true, _) => PostureStep::IDLE,
    }
}

/// AUDIO-OWNER THREAD ONLY. The two ordering functions are unchanged; what changed is which
/// thread runs them.
pub(super) fn apply_posture_step(
    stream: &OutputStream,
    worker: &WorkerHandle,
    control: &CallbackControl,
    park: &AudioParkCell,
    step: PostureStep,
) -> Result<(), AudioError> {
    if step.play_hardware {
        hardware_play_then_ramp(
            || stream.play().map_err(AudioError::PlayStream),
            || park.store(park.load().hardware_running()),
            || step.park_decoder_on(worker),
            || step.ramp_on(control),
        )
    } else if step.park_hardware {
        ramp_then_hardware_pause(
            || {
                step.ramp_on(control)?;
                park.store(park.load().ramped_to_zero());
                Ok(())
            },
            || step.park_decoder_on(worker),
            || stream.pause().map_err(AudioError::PauseStream),
            || park.store(AudioParkState::HardwareParked),
        )
    } else {
        step.park_decoder_on(worker)?;
        step.ramp_on(control)
    }
}

pub(super) fn ramp_then_hardware_pause(
    ramp_to_terminal_zero: impl FnOnce() -> Result<(), AudioError>,
    park_decoder: impl FnOnce() -> Result<(), AudioError>,
    pause_stream: impl FnOnce() -> Result<(), AudioError>,
    publish_hardware_paused: impl FnOnce(),
) -> Result<(), AudioError> {
    ramp_to_terminal_zero()?;
    park_decoder()?;
    pause_stream()?;
    publish_hardware_paused();
    Ok(())
}

pub(super) fn hardware_play_then_ramp(
    play_stream: impl FnOnce() -> Result<(), AudioError>,
    publish_hardware_playing: impl FnOnce(),
    resume_decoder: impl FnOnce() -> Result<(), AudioError>,
    ramp_from_terminal_zero: impl FnOnce() -> Result<(), AudioError>,
) -> Result<(), AudioError> {
    play_stream()?;
    publish_hardware_playing();
    resume_decoder()?;
    ramp_from_terminal_zero()?;
    Ok(())
}
