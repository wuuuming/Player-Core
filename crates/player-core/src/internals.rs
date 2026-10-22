//! What the separately maintained tests reach that is not this crate's API: the tests that need
//! a fixture file or the test CLAP effect live beside their fixtures. Hidden, and only with
//! `test-support`; nothing here is a promise.

#[cfg(windows)]
pub use crate::audio::{
    pump_over, replay_gain_tags, AudioClock, AudioRing, BlockData, DecoderPump, SharedMetrics,
    SongEnd, StopBound, StreamWriter,
};
#[cfg(any(windows, target_os = "linux"))]
pub use crate::clap::{EffectsStage, Library as ClapLibrary};
#[cfg(any(windows, target_os = "linux"))]
pub use crate::dsp::{
    song_chain, DspChain, DspStage, ReplayGainSetting, ReplayGainStage, UserGain,
    DSP_MAX_BLOCK_FRAMES,
};
#[cfg(target_os = "linux")]
pub use crate::linux::test_internals::*;
pub use crate::replay_gain::ReplayGainTags;
