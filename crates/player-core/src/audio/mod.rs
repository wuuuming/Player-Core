//! The audio session, split by responsibility.
//!
//! Every submodule is a child of this one and names what it uses from its siblings
//! (`use super::ring::AudioRing;`); this module only re-exports the crate's API. The two channel
//! newtypes stay here because the owner thread and the media-loop handle each construct one, and
//! a tuple field is private to the module that declares it.

#[cfg(feature = "asio")]
mod asio;
mod asio_host;
mod asio_writer;
mod binding;
mod callback;
mod clock;
mod device;
#[cfg(any(feature = "asio", test))]
mod dsd;
mod endpoints;
mod error;
mod ffi;
mod follow;
mod lease;
mod metrics;
mod mix;
mod mix_engine;
mod mmcss;
mod output;
mod owner;
mod park;
mod player;
mod policy;
mod posture;
mod pump;
mod rebind;
mod ring;
mod seek;
mod session;
mod stream;
mod tags;
#[cfg(feature = "test-support")]
mod test_pump;
mod track;
mod worker;

// The crate's API (`lib.rs` re-exports these).
#[cfg(feature = "test-support")]
pub use self::device::{
    capture_null_audio_output, null_audio_output_late_periods, set_null_audio_output_period_frames,
    set_null_audio_output_rate, set_null_audio_output_uneven,
    take_null_audio_output_callback_times, use_null_audio_output,
};
// What the separately maintained tests reach (`crate::internals`): not API.
#[cfg(feature = "instrument")]
pub use self::metrics::{audio_runtime_counts, AudioRuntimeCounts};
use self::session::{AudioEdge, AudioOrder};
#[cfg(feature = "test-support")]
pub use self::{
    clock::AudioClock,
    metrics::SharedMetrics,
    pump::{BlockData, DecoderPump, SongEnd},
    ring::AudioRing,
    session::StopBound,
    stream::StreamWriter,
    tags::replay_gain_tags,
    test_pump::pump_over,
};
pub use self::{
    device::{
        dop_output, enumerate_output_systems, is_generic_asio_driver, output_fallback,
        output_fallback_version, output_route, set_dop_output, set_output_route, watch_outputs,
        AudioDeviceCaps, AudioDeviceSelection, AudioFallbackReason, AudioOutputDeviceInfo,
        AudioOutputFallback, AudioOutputHost, AudioOutputRoute, AudioOutputSystem, OutputEvent,
        GENERIC_ASIO_DRIVERS,
    },
    endpoints::stop_endpoint_watch,
    error::{AudioError, AudioOpenError},
    metrics::{AudioCallbackWindow, AudioPlaybackMetrics},
    mix::{ChannelLayout, MixAction, MixReport},
    player::{AudioNext, AudioOpenOptions, AudioPlayer},
    policy::{AudioGainCurve, AudioPolicy, AudioPolicyTimes},
    session::AudioSourceState,
    tags::probe_media_tags,
    track::{
        enumerate_audio_tracks, AudioOutputEncoding, AudioStreamIndex, AudioStreamInfo,
        AudioTrackInfo, AudioTrackSelection,
    },
};
use std::sync::mpsc;

/// The media loop's post-only view of the order channel. One method, and no way to reach the
/// inner sender, so no call site can turn a post into a wait.
struct AudioOrders(mpsc::Sender<AudioOrder>);

impl AudioOrders {
    fn post(&self, order: AudioOrder) -> bool {
        self.0.send(order).is_ok()
    }
}

/// The media loop's take-only view of the edge channel. `recv` is not in scope.
struct AudioEdges(mpsc::Receiver<AudioEdge>);

impl AudioEdges {
    fn take(&self) -> Option<AudioEdge> {
        self.0.try_recv().ok()
    }
}

#[cfg(test)]
mod asio_dsd_tests;
#[cfg(test)]
mod dsd_delivery_tests;
#[cfg(test)]
mod multichannel_tests;
#[cfg(test)]
mod tests;
