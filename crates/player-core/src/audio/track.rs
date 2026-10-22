//! Audio tracks: the stream index, the selection a caller makes, and the enumeration it
//! makes it from.

use super::device::AudioOutputHost;
use super::error::{AudioOpenError, NativeError};
use super::ffi::{bn_audio_tracks_enumerate, BnAudioTrackInfo};
use super::mix::{ChannelLayout, MixReport};
use crate::source::NativeMessage;
use crate::StreamTiming;
use cpal::SampleFormat;
use std::ffi::CString;
use std::path::Path;
use std::ptr;

/// The bytes delivered to the endpoint. DSD and DoP bypass digital gain and PCM processing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AudioOutputEncoding {
    #[default]
    Pcm,
    NativeDsd,
    Dop,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioStreamInfo {
    /// Which stream this description came from, so a consumer that opened a specific stream can
    /// tell which one it got back.
    pub stream_index: AudioStreamIndex,
    pub codec: String,
    pub timing: StreamTiming,
    pub source_sample_rate: u32,
    pub source_channels: u16,
    pub source_layout: ChannelLayout,
    pub source_layout_assumed: bool,
    pub output_layout: ChannelLayout,
    pub output_layout_assumed: bool,
    /// None means neither mixer nor limiter is in the path (stereo and DSD/DoP).
    pub mix: Option<MixReport>,
    /// Decode advance only; never added to the device clock's delivery delay.
    pub stage_delay_frames: u32,
    /// Hardware rate: one-bit samples for native DSD, carrier frames for DoP, PCM frames otherwise.
    pub output_sample_rate: u32,
    pub output_channels: u16,
    pub output_sample_format: SampleFormat,
    pub output_encoding: AudioOutputEncoding,
    /// One-bit source rate, including when the endpoint requires conversion to PCM.
    pub dsd_sample_rate: Option<u32>,
    /// The endpoint the sound plays into, as it names itself.
    pub output_device: String,
    /// Which host it is on; `None` for the null sink.
    pub output_host: Option<AudioOutputHost>,
}

impl AudioStreamInfo {
    /// Native ASIO output preserves DSD and DoP at unity for nonzero volume. Zero volume mutes
    /// with DSD idle bytes while playback advances; fractional digital gain is bypassed.
    pub fn volume_bypassed(&self) -> bool {
        self.output_encoding != AudioOutputEncoding::Pcm
    }
}

/// The codec carries DSD (raw DSD or DST-compressed).
pub(super) fn is_dsd_codec(codec: &str) -> bool {
    codec.starts_with("dsd_") || codec == "dst"
}

/// A DSD codec's bit rate from the rate its decoder reports in bytes (8 bits each); none for PCM.
pub(super) fn dsd_sample_rate(codec: &str, source_rate: u32) -> Option<u32> {
    is_dsd_codec(codec)
        .then(|| source_rate.checked_mul(8))
        .flatten()
}

/// A concrete audio stream index inside the opened container. Non-negative by construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AudioStreamIndex(i32);

impl AudioStreamIndex {
    /// The stream the native decoder opened, validated: a request is an intention and `-1`
    /// means "pick one for me"; what the decoder selected must be a real index.
    pub(super) fn opened(selected: i32) -> Result<Self, AudioOpenError> {
        Self::new(selected).ok_or_else(|| {
            AudioOpenError::InvalidTrackInfo(format!(
                "native decoder selected invalid audio stream index {selected}"
            ))
        })
    }

    pub const fn new(value: i32) -> Option<Self> {
        if value >= 0 {
            Some(Self(value))
        } else {
            None
        }
    }

    pub const fn get(self) -> i32 {
        self.0
    }
}

impl std::fmt::Display for AudioStreamIndex {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// WHICH stream of the media to decode — independent of [`AudioDeviceSelection`], which says
/// where to render it. `Default` lets FFmpeg pick the best stream, exactly as before this type
/// existed; an explicit `Stream` must name an enumerated audio stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AudioTrackSelection {
    #[default]
    Default,
    Stream(AudioStreamIndex),
}

/// One enumerated audio track, as the picker displays it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioTrackInfo {
    pub stream_index: AudioStreamIndex,
    pub language: Option<String>,
    pub codec: String,
    /// Source channel count. Zero means the container did not declare it.
    pub channels: u16,
}

/// Enumerate the audio tracks of a local file, for a picker. Two native calls: one to size, one
/// to fill — the C side owns the truth of how many audio streams the container has.
pub fn enumerate_audio_tracks(
    local_path: impl AsRef<Path>,
) -> Result<Vec<AudioTrackInfo>, AudioOpenError> {
    let path = local_path
        .as_ref()
        .to_str()
        .ok_or(AudioOpenError::PathEncoding)?;
    let path = CString::new(path).map_err(|_| AudioOpenError::PathContainsNul)?;
    let mut track_count = 0usize;
    let mut error = NativeMessage::new();
    // SAFETY: `path` is NUL-terminated; no track buffer, so only the count is written; the
    // message buffer is given with its capacity. All outlive the call.
    let status = unsafe {
        bn_audio_tracks_enumerate(
            path.as_ptr(),
            ptr::null_mut(),
            0,
            &mut track_count,
            error.as_mut_ptr(),
            error.capacity(),
        )
    };
    if status != 0 {
        return Err(AudioOpenError::Native(NativeError {
            operation: "track listing",
            status,
            transport: 0,
            message: error.message(),
        }));
    }
    if track_count == 0 {
        return Ok(Vec::new());
    }

    let mut native_tracks = vec![BnAudioTrackInfo::default(); track_count];
    let mut reported_count = track_count;
    let mut error = NativeMessage::new();
    // SAFETY: as above; `native_tracks` holds `track_count` entries, the capacity given, and
    // `reported_count` takes the number written.
    let status = unsafe {
        bn_audio_tracks_enumerate(
            path.as_ptr(),
            native_tracks.as_mut_ptr(),
            native_tracks.len(),
            &mut reported_count,
            error.as_mut_ptr(),
            error.capacity(),
        )
    };
    if status != 0 {
        return Err(AudioOpenError::Native(NativeError {
            operation: "track listing",
            status,
            transport: 0,
            message: error.message(),
        }));
    }
    native_tracks.truncate(reported_count);
    map_native_audio_tracks(&native_tracks)
}

pub(super) fn map_native_audio_tracks(
    native_tracks: &[BnAudioTrackInfo],
) -> Result<Vec<AudioTrackInfo>, AudioOpenError> {
    native_tracks
        .iter()
        .map(|track| {
            let stream_index = AudioStreamIndex::new(track.stream_index).ok_or_else(|| {
                AudioOpenError::InvalidTrackInfo(format!(
                    "native audio track exposed invalid stream index {}",
                    track.stream_index
                ))
            })?;
            let channels = u16::try_from(track.channels).map_err(|_| {
                AudioOpenError::InvalidTrackInfo(format!(
                    "audio stream index {stream_index} exposed invalid channel count {}",
                    track.channels
                ))
            })?;
            let language = crate::fixed_c_text(&track.language);
            let codec = crate::fixed_c_text(&track.codec);
            Ok(AudioTrackInfo {
                stream_index,
                language: (!language.is_empty()).then_some(language),
                codec: if codec.is_empty() {
                    "unknown".to_owned()
                } else {
                    codec
                },
                channels,
            })
        })
        .collect()
}

/// A selection as the native ABI wants it: `-1` is "pick one for me". Whether an explicit index
/// names an audio stream is decided by the decoder that opens it — the one holder of the
/// container — so a factory-backed session never opens the resource twice to pre-check.
pub(super) fn requested_stream_index(selection: AudioTrackSelection) -> i32 {
    match selection {
        AudioTrackSelection::Default => -1,
        AudioTrackSelection::Stream(stream_index) => stream_index.get(),
    }
}
