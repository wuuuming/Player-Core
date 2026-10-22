//! PulseAudio output, with fresh server timing and single-threaded operation ownership.
mod api;
mod recent;
mod stream;

pub use recent::Recent;
pub use stream::Stream;
pub const RATE: u32 = 48_000;
/// Stereo, the channels every stream has.
pub const CHANNELS: u8 = 2;
/// One frame as the stream takes it: `CHANNELS` little-endian `f32` samples.
pub const FRAME_BYTES: u32 = CHANNELS as u32 * 4;

/// One sample of the stream: 4 little-endian bytes of an `f32`.
pub fn sample(bytes: &[u8]) -> f32 {
    f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// The stream's bytes as samples.
pub fn samples(bytes: &[u8]) -> impl Iterator<Item = f32> + '_ {
    bytes.chunks_exact(4).map(sample)
}

/// The stream bytes `duration` takes, in whole frames.
pub fn duration_to_bytes(duration: std::time::Duration) -> u64 {
    let frames = duration.as_nanos() * u128::from(RATE) / 1_000_000_000;
    u64::try_from(frames).unwrap_or(u64::MAX / 8) * FRAME_BYTES as u64
}

/// `n` bytes cut down to whole frames.
pub fn whole_frames(n: usize) -> usize {
    n / FRAME_BYTES as usize * FRAME_BYTES as usize
}

/// How long `bytes` of stream frames play, in microseconds.
pub fn bytes_to_micros(bytes: u64) -> u64 {
    (u128::from(bytes) * 1_000_000 / u128::from(RATE * FRAME_BYTES)).min(u128::from(u64::MAX))
        as u64
}

/// A sound server answers (checked once, without writing any audio).
pub fn available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| Stream::open("probe", None, None).is_ok())
}
