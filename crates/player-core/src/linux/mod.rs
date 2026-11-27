//! Linux playback: bundled FFmpeg decoding and dynamically loaded PulseAudio output.
//! Hosts supply PCM through media-io's `PcmSource`; this backend never depends on application
//! plugins.
//! Each submodule names what it uses from its siblings; this module only re-exports the API.

#[path = "../audio/ffi.rs"]
mod ffi;
pub mod legacy_text;
mod output;
mod playback;
mod player;
mod probe;
#[path = "../audio/tags.rs"]
mod tags;

mod pulse;
mod source;
mod types;

#[cfg(feature = "test-support")]
pub use output::{capture_null_audio_output, use_null_audio_output};
pub use output::{
    enumerate_output_systems, output_route, set_dop_output, set_output_route, watch_outputs,
    AudioDeviceCaps, AudioOutputDeviceInfo, AudioOutputHost, AudioOutputRoute, AudioOutputSystem,
    OutputEvent,
};
pub use player::{AudioNext, AudioOpenOptions, AudioPlayer};
// What the separately maintained tests reach (`crate::internals`): not API, so not in `linux::*`.
#[cfg(feature = "test-support")]
pub(crate) mod test_internals {
    pub use super::playback::{decode_pcm, Processing, CHAIN_CHANNELS, FRAME_BYTES};
    pub use super::probe::for_playback;
}
pub use probe::{probe_path, probe_path_cancellable, probe_stream_cancellable, probe_tags_only};
pub use tags::probe_media_tags;

pub use pulse::available as audio_output_available;

pub use crate::track::{AudioStreamIndex, AudioTrackSelection};
pub use types::AudioError;
