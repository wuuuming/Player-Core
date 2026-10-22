//! NovaPlayer's playback core: sound only.
//!
//! FFmpeg demux and decode on a demux thread of their own, WASAPI and ASIO output with the device
//! ladder, the media clock and the held seek, tags, CUE spans, TAK and DSD. A session decodes from
//! a [`MediaByteSource`] (a file, a URL through FFmpeg's protocol layer, or any byte source a host
//! supplies) or plays PCM a host produces itself ([`PcmSource`]).

#[cfg(test)]
mod api_parity;
#[cfg(any(windows, target_os = "linux"))]
pub mod clap;
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod dsd_files;
#[cfg(any(windows, target_os = "linux"))]
mod dsp;
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod internals;

/// A C string from a library, copied with invalid sequences replaced; `None` for null.
///
/// # Safety
/// `text` is null or a NUL-terminated string alive for the call.
pub(crate) unsafe fn c_text(text: *const std::ffi::c_char) -> Option<String> {
    (!text.is_null()).then(|| {
        // SAFETY: not null, and as the caller promises.
        unsafe { std::ffi::CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned()
    })
}

/// A fixed-size C text field: the bytes before its first NUL (all of them if none), as UTF-8 with
/// invalid sequences replaced.
#[cfg(any(windows, target_os = "linux"))]
pub(crate) fn fixed_c_text(field: &[std::ffi::c_char]) -> String {
    let bytes: Vec<u8> = field
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}
#[cfg(any(windows, target_os = "linux"))]
mod media_tags;
mod replay_gain;
#[cfg(any(windows, target_os = "linux"))]
mod tag_rule;
#[cfg(any(windows, target_os = "linux"))]
pub use media_tags::{AudioResolution, MediaTags};
mod timing;

pub use replay_gain::ReplayGain;
#[cfg(any(windows, target_os = "linux"))]
pub(crate) use replay_gain::ReplayGainTags;

#[cfg(windows)]
pub mod legacy_text;

// The byte sources a session is opened over are media-io's, which builds without FFmpeg; the
// sessions here take them, so they are named here too.
pub use media_io::{
    AudioInput, AudioSourceCause, AudioSourceError, AudioSourceFactory, AudioSourceKind,
    FileMediaByteSource, HttpOpenOptions, MediaByteSource, PathAudioSourceFactory,
    PcmAudioSourceFactory, PcmSource, SourceLocality,
};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(windows)]
#[cfg(feature = "instrument")]
use std::sync::atomic::{AtomicU64, Ordering};

pub use timing::{
    normalize_timestamp, presentation_offset_nanoseconds, raw_timestamp_for_position,
    NormalizedTimestamp, StreamTimeBase, StreamTiming, TimestampError,
};

#[cfg(windows)]
mod audio;

#[cfg(windows)]
mod source;

#[cfg(windows)]
pub use audio::{
    dop_output, enumerate_audio_tracks, enumerate_output_systems, is_generic_asio_driver,
    output_fallback, output_fallback_version, output_route, probe_media_tags, set_dop_output,
    set_output_route, stop_endpoint_watch, watch_outputs, AudioCallbackWindow, AudioDeviceCaps,
    AudioDeviceSelection, AudioError, AudioFallbackReason, AudioGainCurve, AudioNext,
    AudioOpenError, AudioOpenOptions, AudioOutputDeviceInfo, AudioOutputEncoding,
    AudioOutputFallback, AudioOutputHost, AudioOutputRoute, AudioOutputSystem,
    AudioPlaybackMetrics, AudioPlayer, AudioPolicy, AudioPolicyTimes, AudioSourceState,
    AudioStreamIndex, AudioStreamInfo, AudioTrackInfo, AudioTrackSelection, ChannelLayout,
    MixAction, MixReport, OutputEvent, GENERIC_ASIO_DRIVERS,
};

#[cfg(windows)]
#[cfg(feature = "instrument")]
pub use audio::{audio_runtime_counts, AudioRuntimeCounts};
/// The null output (the clock, no device) and its knobs, for tests: never in a product build.
#[cfg(windows)]
#[cfg(feature = "test-support")]
pub use audio::{
    capture_null_audio_output, null_audio_output_late_periods, set_null_audio_output_period_frames,
    set_null_audio_output_rate, set_null_audio_output_uneven,
    take_null_audio_output_callback_times, use_null_audio_output,
};

#[cfg(windows)]
#[cfg(feature = "instrument")]
pub use source::{native_buffer_census, NativeBufferCensus};

#[cfg(windows)]
pub use source::{UrlAudioSourceFactory, UrlMediaByteSource};

/// Process-lifetime monotonic work counts, for measuring from outside (an idle player does no
/// work).
///
/// These counters deliberately have no reset operation. A measurement takes two snapshots and
/// compares their deltas, so opening the measurement window never needs to wake or mutate the
/// decoder or audio callback threads.
#[cfg(windows)]
#[cfg(feature = "instrument")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlayerActivityCounts {
    /// Wired at entry to the real CPAL output callback in `audio/callback.rs`.
    pub audio_callback_calls: u64,
    /// Wired at the audio decoder's real queue-refill attempt in `audio/pump.rs`.
    pub audio_decoder_refill_calls: u64,
    /// Wired at the real audio decode attempt in `audio/pump.rs`.
    pub audio_decode_calls: u64,
}

#[cfg(windows)]
#[cfg(feature = "instrument")]
static AUDIO_CALLBACK_CALLS: AtomicU64 = AtomicU64::new(0);
#[cfg(windows)]
#[cfg(feature = "instrument")]
static AUDIO_DECODER_REFILL_CALLS: AtomicU64 = AtomicU64::new(0);
#[cfg(windows)]
#[cfg(feature = "instrument")]
static AUDIO_DECODE_CALLS: AtomicU64 = AtomicU64::new(0);

#[cfg(windows)]
#[cfg(feature = "instrument")]
pub fn player_activity_counts() -> PlayerActivityCounts {
    PlayerActivityCounts {
        audio_callback_calls: AUDIO_CALLBACK_CALLS.load(Ordering::Acquire),
        audio_decoder_refill_calls: AUDIO_DECODER_REFILL_CALLS.load(Ordering::Acquire),
        audio_decode_calls: AUDIO_DECODE_CALLS.load(Ordering::Acquire),
    }
}

#[cfg(windows)]
#[cfg(feature = "instrument")]
pub(crate) fn record_audio_callback() -> u64 {
    AUDIO_CALLBACK_CALLS.fetch_add(1, Ordering::Relaxed) + 1
}

#[cfg(windows)]
#[cfg(feature = "instrument")]
pub(crate) fn record_audio_decoder_refill() {
    AUDIO_DECODER_REFILL_CALLS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(windows)]
#[cfg(feature = "instrument")]
pub(crate) fn record_audio_decode() {
    AUDIO_DECODE_CALLS.fetch_add(1, Ordering::Relaxed);
}
