//! What a file says about itself, as a host reads it: one type on every platform, filled by one
//! rule from what each platform's reader found (`tag_rule`: FFmpeg's dictionaries on Windows,
//! ffprobe's JSON and lofty's tags on Linux).

use crate::tag_rule::{is_lossless, source_bits};
use std::time::Duration;

/// A container's own description. Every text is what the file says, trimmed, and absent when the
/// file says nothing: a host decides what to show in its place, never this crate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MediaTags {
    pub title: Option<String>,
    /// The artist, else the album artist, else the composer.
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    /// The date, else the year, else the container's creation time.
    pub date: Option<String>,
    pub duration: Option<Duration>,
    pub has_audio: bool,
    /// A picture that MOVES. A cover is a video stream to FFmpeg, and is not one here: a file
    /// that has only a cover is a sound file.
    pub has_video: bool,
    /// The attached cover as the container holds it — an encoded JPEG or PNG, not pixels.
    pub cover: Option<Vec<u8>>,
    /// The lyrics tag, whole and as written: plain words, or LRC lines with their timestamps.
    /// What they mean is the host's to parse; this crate only carries them out of the container.
    /// MP4's `lyrics`, ID3's USLT (`lyrics-<language>`), Vorbis' `LYRICS` or `UNSYNCEDLYRICS`.
    pub lyrics: Option<String>,
    /// The sound's own resolution, when the file has a sound.
    pub resolution: Option<AudioResolution>,
    pub genre: Option<String>,
    /// The track number as the file writes it: `3`, or `3/12` with the album's count.
    pub track: Option<String>,
    /// The container's format by FFmpeg's short name — `flac`, `mp3`, `ogg`, `wav`, and `mov` for
    /// every ISO MP4 (`.m4a`, `.mp4`).
    pub container: Option<String>,
    /// The file's size in bytes, when its transport knows it.
    pub file_size: Option<u64>,
}

impl MediaTags {
    /// Sound and no moving picture: what an audio-only session plays.
    pub fn is_audio_only(&self) -> bool {
        self.has_audio && !self.has_video
    }
}

/// What a listener reads under the album's name: the rate, the depth, the codec.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioResolution {
    /// FFmpeg's name for the codec: `alac`, `flac`, `aac`, `mp3`, `opus`, `pcm_s16le`…
    pub codec: String,
    pub sample_rate: u32,
    /// The SOURCE's depth. `Some` only for a lossless codec: a lossy one has no depth to state.
    pub bits: Option<u8>,
    pub lossless: bool,
    /// Bits per second: the stream's, else the container's.
    pub bit_rate: Option<u32>,
}

impl AudioResolution {
    /// The resolution of a sound stream as its reader found it: FFmpeg's codec name, the rate,
    /// the depth the decoder states (`raw`) and the one the samples are coded in (`coded`), the
    /// bit rate. `None` without a rate: there is nothing for a listener to read. A bit rate of
    /// zero is none.
    pub(crate) fn of(
        codec: String,
        sample_rate: u32,
        (raw, coded): (i64, i64),
        bit_rate: Option<u32>,
    ) -> Option<Self> {
        (sample_rate > 0).then(|| Self {
            bits: source_bits(&codec, raw, coded),
            lossless: is_lossless(&codec),
            codec,
            sample_rate,
            bit_rate: bit_rate.filter(|rate| *rate > 0),
        })
    }

    /// `96 kHz / 24-bit ALAC` for a lossless sound; `44.1 kHz AAC 256 kbps` for a lossy one,
    /// whose depth means nothing and whose rate says what its depth would have.
    pub fn label(&self) -> String {
        let khz = f64::from(self.sample_rate) / 1_000.0;
        // 44.1 and 88.2 keep their decimal; 48, 96 and 192 have none to keep.
        let rate = if self.sample_rate.is_multiple_of(1_000) {
            format!("{khz:.0} kHz")
        } else {
            format!("{khz:.1} kHz")
        };
        let codec = match self.codec.as_str() {
            codec if codec.starts_with("pcm_") => "PCM".to_owned(),
            "vorbis" => "Vorbis".to_owned(),
            "opus" => "Opus".to_owned(),
            codec => codec.to_ascii_uppercase(),
        };
        match (self.lossless, self.bits, self.bit_rate) {
            (true, Some(bits), _) => format!("{rate} / {bits}-bit {codec}"),
            (true, None, _) => format!("{rate} {codec}"),
            (false, _, Some(bit_rate)) => {
                format!("{rate} {codec} {} kbps", (bit_rate + 500) / 1_000)
            }
            (false, _, None) => format!("{rate} {codec}"),
        }
    }
}

#[cfg(test)]
mod tests;
