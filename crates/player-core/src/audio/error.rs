//! The two typed failures an audio session reports: one for an open, one for a running
//! session; and the native shim's failure both carry.

use super::rebind::DEVICE_RECOVERY_WINDOW;
use super::track::AudioStreamIndex;
use crate::{AudioSourceKind, TimestampError};
use cpal::SampleFormat;
use std::sync::Arc;

/// A failure the native shim reported: the call's status, the transport error under it and the
/// shim's message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeError {
    /// What was being done: "open", "seek", "decode", "DSD open", "track listing", "tag reading".
    pub operation: &'static str,
    /// The shim's nonzero status; 0 when no shim call failed (it returned no decoder, say).
    pub status: i32,
    /// The transport AVERROR under the failure; 0 when there was none or it is unknown.
    pub transport: i32,
    pub message: String,
}

impl NativeError {
    /// True only when the transport under the failure is a transient network failure.
    pub fn is_network_failure(&self) -> bool {
        self.transport != 0 && crate::source::native_is_network_failure(self.transport)
    }
}

impl std::fmt::Display for NativeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "native audio {} failed with status {}: {}",
            self.operation, self.status, self.message
        )
    }
}

#[derive(Debug)]
pub enum AudioOpenError {
    OutputBusy {
        owner: u64,
    },
    NoOutputDevice,
    DefaultOutputConfig(String),
    InvalidOutputConfig(String),
    InvalidPolicy(String),
    PathEncoding,
    PathContainsNul,
    Native(NativeError),
    /// A typed running-session failure encountered while preparing the initial output.
    Prefill(Box<AudioError>),
    Timestamp(TimestampError),
    Worker(String),
    BuildStream(String),
    PlayStream(String),
    UnsupportedSampleFormat(SampleFormat),
    DeviceEnumeration(String),
    DeviceUnavailable(String),
    /// Off ASIO, the ASIO driver is still held by an output that is closing — a DSD song's, say —
    /// past the idle wait. WASAPI is not opened beside it (the driver may keep the interface):
    /// the open waits and asks again, a rebind takes its ladder's next rung.
    AsioStillHeld,
    SelectedTrackUnavailable {
        stream_index: AudioStreamIndex,
    },
    InvalidTrackInfo(String),
    /// The open's generation was superseded mid-flight: nothing failed.
    Cancelled,
    /// The source factory refused.
    Source(crate::AudioSourceError),
}

impl AudioOpenError {
    /// True only for typed transient transport failures. Unknown text, auth, media and device
    /// failures are not network failures. The host must suppress externally cancelled operations.
    pub fn is_network_failure(&self) -> bool {
        match self {
            Self::Source(error) => crate::source::source_is_network_failure(error),
            Self::Native(error) => error.is_network_failure(),
            Self::Prefill(error) => error.is_network_failure(),
            _ => false,
        }
    }

    /// The sound output refused: no device, one that went, or one that would not start. Opening
    /// again once a device is there may succeed; the song itself is not at fault.
    pub fn is_device_failure(&self) -> bool {
        match self {
            Self::Prefill(error) => error.is_device_failure(),
            _ => self.kind() == AudioSourceKind::Device,
        }
    }

    /// Which dependency refused, as published by the session. This is not a retry decision:
    /// an Open failure includes auth and bad paths. Use `is_network_failure` for transport retry.
    pub fn kind(&self) -> AudioSourceKind {
        match self {
            Self::Source(error) => error.kind,
            Self::PathEncoding | Self::PathContainsNul => AudioSourceKind::Open,
            Self::Cancelled => AudioSourceKind::Cancelled,
            Self::Native(_)
            | Self::Prefill(_)
            | Self::Timestamp(_)
            | Self::SelectedTrackUnavailable { .. }
            | Self::InvalidTrackInfo(_)
            | Self::Worker(_)
            | Self::InvalidPolicy(_) => AudioSourceKind::Decode,
            Self::OutputBusy { .. }
            | Self::NoOutputDevice
            | Self::DefaultOutputConfig(_)
            | Self::InvalidOutputConfig(_)
            | Self::BuildStream(_)
            | Self::PlayStream(_)
            | Self::UnsupportedSampleFormat(_)
            | Self::DeviceEnumeration(_)
            | Self::DeviceUnavailable(_)
            | Self::AsioStillHeld => AudioSourceKind::Device,
        }
    }
}

impl std::fmt::Display for AudioOpenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutputBusy { owner } => {
                write!(formatter, "audio output is owned by session {owner}")
            }
            Self::NoOutputDevice => {
                formatter.write_str("no default audio output device is available")
            }
            Self::DefaultOutputConfig(error) => write!(
                formatter,
                "could not read the default audio output config: {error}"
            ),
            Self::InvalidOutputConfig(error) | Self::InvalidPolicy(error) | Self::Worker(error) => {
                formatter.write_str(error)
            }
            Self::SelectedTrackUnavailable { stream_index } => write!(
                formatter,
                "audio stream index {stream_index} is not an enumerated audio track"
            ),
            Self::InvalidTrackInfo(error) => formatter.write_str(error),
            Self::PathEncoding => formatter.write_str("local media path is not valid UTF-8"),
            Self::PathContainsNul => formatter.write_str("local media path contains a NUL byte"),
            Self::Native(error) => std::fmt::Display::fmt(error, formatter),
            Self::Prefill(error) => std::fmt::Display::fmt(error, formatter),
            Self::Timestamp(error) => {
                write!(formatter, "audio presentation origin is invalid: {error}")
            }
            Self::BuildStream(error) => {
                write!(formatter, "could not build cpal output stream: {error}")
            }
            Self::PlayStream(error) => {
                write!(formatter, "could not start cpal output callback: {error}")
            }
            Self::UnsupportedSampleFormat(format) => {
                write!(formatter, "unsupported cpal output sample format {format}")
            }
            Self::DeviceEnumeration(error) => {
                write!(
                    formatter,
                    "could not enumerate audio output devices: {error}"
                )
            }
            Self::DeviceUnavailable(name) => {
                write!(formatter, "audio output device is unavailable: {name}")
            }
            Self::AsioStillHeld => {
                write!(
                    formatter,
                    "the ASIO driver is still held by an output that is closing"
                )
            }
            Self::Cancelled => {
                formatter.write_str("the audio open was cancelled before it bound a device")
            }
            Self::Source(error) => formatter.write_str(&error.to_string()),
        }
    }
}

impl std::error::Error for AudioOpenError {}

impl From<TimestampError> for AudioOpenError {
    fn from(value: TimestampError) -> Self {
        Self::Timestamp(value)
    }
}

#[derive(Clone, Debug)]
pub enum AudioError {
    Native(NativeError),
    ControlTimeout,
    OutputFailed,
    WorkerStopped,
    WorkerShutdownTimeout,
    OwnerShutdownTimeout,
    Worker(String),
    PauseStream(String),
    PlayStream(String),
    ClockRange(i128),
    Timestamp(TimestampError),
    SeekNotHeld,
    Disposed,
    DeviceLost(String),
    /// Recovery exhausted while reopening the source or binding the device. The ladder's
    /// public reason stays unchanged; the last attempt's typed cause survives beside it.
    RebindFailed {
        reason: String,
        cause: Arc<AudioOpenError>,
    },
}

impl AudioError {
    /// True only for a typed transient transport failure, not an output/control timeout.
    /// The host must suppress externally cancelled operations.
    pub fn is_network_failure(&self) -> bool {
        match self {
            Self::Native(error) => error.is_network_failure(),
            Self::RebindFailed { cause, .. } => cause.is_network_failure(),
            _ => false,
        }
    }

    /// The sound output failed under a running session: its device went and none took it.
    pub fn is_device_failure(&self) -> bool {
        match self {
            Self::OutputFailed
            | Self::DeviceLost(_)
            | Self::PlayStream(_)
            | Self::PauseStream(_) => true,
            Self::RebindFailed { cause, .. } => cause.is_device_failure(),
            _ => false,
        }
    }
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ControlTimeout => formatter
                .write_str("audio callback did not acknowledge the control transition in time"),
            Self::OutputFailed => formatter.write_str("cpal reported an output-device failure"),
            Self::WorkerStopped => formatter.write_str("audio decode worker stopped unexpectedly"),
            Self::WorkerShutdownTimeout => {
                formatter.write_str("audio decode worker did not stop before the teardown deadline")
            }
            Self::OwnerShutdownTimeout => formatter
                .write_str("the audio owner thread did not stop before the teardown deadline"),
            Self::Native(error) => std::fmt::Display::fmt(error, formatter),
            Self::Worker(error) => formatter.write_str(error),
            Self::PauseStream(error) => {
                write!(formatter, "could not pause cpal output stream: {error}")
            }
            Self::PlayStream(error) => {
                write!(formatter, "could not resume cpal output stream: {error}")
            }
            Self::ClockRange(value) => write!(
                formatter,
                "audio clock timestamp {value}ns exceeds the signed clock range"
            ),
            Self::Timestamp(error) => write!(formatter, "audio timestamp failed: {error}"),
            Self::SeekNotHeld => {
                formatter.write_str("audio seek work requires an active held transaction")
            }
            Self::Disposed => formatter.write_str("audio output is already disposed"),
            Self::DeviceLost(reason) | Self::RebindFailed { reason, .. } => write!(
                formatter,
                "audio output device could not be rebound within {}s: {reason}",
                DEVICE_RECOVERY_WINDOW.as_secs()
            ),
        }
    }
}

impl std::error::Error for AudioError {}

impl From<TimestampError> for AudioError {
    fn from(value: TimestampError) -> Self {
        Self::Timestamp(value)
    }
}
