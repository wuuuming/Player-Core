//! What a file says, read by one rule on every platform: the names each field is read under, where
//! the lyrics are, which codecs are lossless and what the source's depth is. The readers (FFmpeg's
//! dictionaries on Windows; ffprobe's JSON and lofty's tags on Linux) hand over what the file
//! holds, as [`Said`]; this decides what it means.

/// The tags a file holds, as its reader found them, by FFmpeg's names (`title`, `album_artist`,
/// `lyrics-eng`, any case): the container's first, then the audio stream's. Ogg and Opus keep
/// their comments on the stream, everything else on the container.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Said(pub(crate) Vec<(String, String)>);

/// Each field's names, in the order they are tried. FFmpeg converts ID3's TCON/TRCK and Vorbis'
/// GENRE/TRACKNUMBER to `genre`/`track`; a Vorbis comment it has no conversion for keeps its own
/// name, which is the second.
const TITLE: &[&str] = &["title"];
const ARTIST: &[&str] = &["artist", "album_artist", "composer"];
const ALBUM: &[&str] = &["album"];
const ALBUM_ARTIST: &[&str] = &["album_artist"];
const DATE: &[&str] = &["date", "year", "creation_time"];
const GENRE: &[&str] = &["genre"];
const TRACK: &[&str] = &["track", "tracknumber"];

impl Said {
    /// The first non-empty value under `name` (any case), trimmed.
    pub(crate) fn get(&self, name: &str) -> Option<String> {
        self.find(|k| k.eq_ignore_ascii_case(name))
            .map(|v| v.trim().to_owned())
    }

    /// The first value under a key `key` takes that is not blank, as written.
    fn find(&self, key: impl Fn(&str) -> bool) -> Option<&str> {
        self.0
            .iter()
            .filter(|(k, _)| key(k))
            .map(|(_, v)| v.as_str())
            .find(|v| !v.trim().is_empty())
    }

    fn first(&self, names: &[&str]) -> Option<String> {
        names.iter().find_map(|n| self.get(n))
    }

    /// What it says, field by field. The lyrics come whole, as written (LRC lines keep their
    /// line ends).
    /// The tags whose words the file gives, each the first of its names that is not blank,
    /// trimmed (the lyrics whole); what the words do not say (the length, the streams, the
    /// cover) is the reader's to add.
    pub(crate) fn tags(&self) -> crate::MediaTags {
        let lyrics = self
            .find(|k| {
                k.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("lyrics"))
                    && (k.len() == 6 || k.as_bytes()[6] == b'-')
            })
            .or_else(|| self.find(|k| k.eq_ignore_ascii_case("unsyncedlyrics")))
            .map(str::to_owned);
        crate::MediaTags {
            title: self.first(TITLE),
            artist: self.first(ARTIST),
            album: self.first(ALBUM),
            album_artist: self.first(ALBUM_ARTIST),
            date: self.first(DATE),
            genre: self.first(GENRE),
            track: self.first(TRACK),
            lyrics,
            ..Default::default()
        }
    }
}

/// The codecs whose sound is the source's, by FFmpeg's names: those FFmpeg marks lossless and not
/// lossy (`ffprobe -codecs`, FFmpeg 5.1 and 7), and WavPack, which FFmpeg marks both for its rare
/// hybrid mode. DSD is not among them: FFmpeg marks its decoding to PCM lossy, and DST (DSD's own
/// lossless packing) is.
const LOSSLESS: &[&str] = &[
    "alac",
    "ape",
    "atrac3al",
    "atrac3pal",
    "dst",
    "flac",
    "mlp",
    "mp4als",
    "ralf",
    "s302m",
    "shorten",
    "tak",
    "truehd",
    "tta",
    "wavpack",
    "wmalossless",
];

/// PCM is lossless but for the companded and the Archimedes kinds.
const LOSSY_PCM: &[&str] = &["pcm_alaw", "pcm_mulaw", "pcm_vidc"];

pub(crate) fn is_lossless(codec: &str) -> bool {
    LOSSLESS.contains(&codec) || (codec.starts_with("pcm_") && !LOSSY_PCM.contains(&codec))
}

/// The source's depth, which only a lossless codec has: the depth the decoder states, else the
/// one the container codes the samples in.
pub(crate) fn source_bits(codec: &str, raw: i64, coded: i64) -> Option<u8> {
    if !is_lossless(codec) {
        return None;
    }
    [raw, coded]
        .into_iter()
        .find(|b| *b > 0)
        .and_then(|b| u8::try_from(b).ok())
}
