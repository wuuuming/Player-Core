use super::legacy_text;
use super::source::is_http_input;
use crate::tag_rule::Said;
use crate::ReplayGainTags;
use crate::{AudioSourceFactory, FileMediaByteSource, MediaTags, UrlAudioSourceFactory};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub fn probe_path(path: &Path) -> Option<MediaTags> {
    probe_path_cancellable(path, &AtomicBool::new(false))
}

pub fn probe_path_cancellable(path: &Path, cancelled: &AtomicBool) -> Option<MediaTags> {
    probe_native(path, cancelled).map(|(tags, _)| tags)
}

/// Stream information for a live selection: no artwork connection, canceled with the selection.
pub fn probe_stream_cancellable(path: &Path, cancelled: &AtomicBool) -> Option<MediaTags> {
    probe_native(path, cancelled).map(|(mut tags, _)| {
        tags.cover = None;
        tags
    })
}

/// Tags and length without extracting the picture.
pub fn probe_tags_only(path: &Path) -> Option<MediaTags> {
    read_tags_native(path).or_else(|| probe_stream_cancellable(path, &AtomicBool::new(false)))
}

/// Tags and length read in-process (lofty): the tag blocks and the stream header only, no
/// picture, no child process. None for what lofty does not read (TAK, DSD, …): the native probe then.
fn read_tags_native(path: &Path) -> Option<MediaTags> {
    use lofty::prelude::*;
    let f = lofty_file(path)?;
    let p = f.properties();
    let tag = f.primary_tag().or_else(|| f.first_tag());
    // lofty's items by FFmpeg's names, so the one rule reads them (`tag_rule`).
    let names = [
        (ItemKey::TrackTitle, "title"),
        (ItemKey::TrackArtist, "artist"),
        (ItemKey::AlbumArtist, "album_artist"),
        (ItemKey::Composer, "composer"),
        (ItemKey::AlbumTitle, "album"),
        (ItemKey::RecordingDate, "date"),
        (ItemKey::Year, "year"),
        (ItemKey::Genre, "genre"),
        (ItemKey::TrackNumber, "track"),
    ];
    let said = Said(
        names
            .into_iter()
            .filter_map(|(k, name)| Some((name.to_owned(), tag?.get_string(k)?.to_owned())))
            .collect(),
    );
    let words = said.tags();
    let duration = Some(p.duration()).filter(|d| !d.is_zero());
    // RIFF INFO has no charset: Japanese rippers wrote Shift-JIS or EUC-JP, which lofty reads as
    // Latin-1. Its raw bytes go through the same code-page guess as a CUE sheet.
    let info = if f.file_type() == lofty::file::FileType::Wav {
        riff_info(path)
    } else {
        Default::default()
    };
    Some(MediaTags {
        title: info.title.or(words.title),
        artist: info.artist.or(words.artist),
        album: info.album.or(words.album),
        duration,
        has_audio: true,
        file_size: std::fs::metadata(path).ok().map(|m| m.len()),
        ..words
    })
}

#[derive(Default)]
struct RiffInfo {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
}

/// INAM / IART / IPRD of a WAV's `LIST INFO`, decoded by code-page guess.
fn riff_info(path: &Path) -> RiffInfo {
    use std::io::{Read, Seek, SeekFrom};
    let mut out = RiffInfo::default();
    let Ok(mut f) = std::fs::File::open(path) else {
        return out;
    };
    let mut h = [0u8; 12];
    if f.read_exact(&mut h).is_err() || &h[..4] != b"RIFF" || &h[8..] != b"WAVE" {
        return out;
    }
    let mut c = [0u8; 8];
    while f.read_exact(&mut c).is_ok() {
        let n = u32::from_le_bytes([c[4], c[5], c[6], c[7]]) as u64;
        if &c[..4] == b"LIST" && (4..1 << 20).contains(&n) {
            let mut b = vec![0u8; n as usize];
            if f.read_exact(&mut b).is_err() || &b[..4] != b"INFO" {
                break;
            }
            let mut i = 4;
            while i + 8 <= b.len() {
                let id = &b[i..i + 4];
                let len = u32::from_le_bytes([b[i + 4], b[i + 5], b[i + 6], b[i + 7]]) as usize;
                let v = &b[(i + 8).min(b.len())..(i + 8 + len).min(b.len())];
                let v = v.split(|&x| x == 0).next().unwrap_or(&[]);
                let t = legacy_text::decode_guessing(v).trim().to_owned();
                let t = (!t.is_empty()).then_some(t);
                match id {
                    b"INAM" => out.title = t,
                    b"IART" => out.artist = t,
                    b"IPRD" => out.album = t,
                    _ => {}
                }
                i += 8 + len + (len & 1);
            }
            break;
        }
        if f.seek(SeekFrom::Current((n + (n & 1)) as i64)).is_err() {
            break;
        }
    }
    out
}

/// What playback needs to know before it starts: the length when `duration` is not known yet, and
/// the song's ReplayGain values. One native probe at most: a local file's values are read in process
/// (lofty) unless the length has to be probed anyway or lofty does not know the format.
pub fn for_playback(
    path: &Path,
    duration: Option<Duration>,
    cancelled: &AtomicBool,
) -> (Option<Duration>, ReplayGainTags) {
    if duration.is_some() && !is_http_input(path) {
        if let Some(tags) = replay_gain_native(path) {
            return (duration, tags);
        }
    }
    let Some((tags, gain)) = probe_native(path, cancelled) else {
        return (duration, ReplayGainTags::default());
    };
    (duration.or(tags.duration), gain)
}

/// A file lofty reads, without its picture; `None` for a format it does not know.
fn lofty_file(path: &Path) -> Option<lofty::file::TaggedFile> {
    let opts = lofty::config::ParseOptions::new().read_cover_art(false);
    let file = lofty::probe::Probe::open(path)
        .ok()?
        .options(opts)
        .guess_file_type()
        .ok()?;
    file.file_type()?;
    file.read().ok()
}

/// The ReplayGain values of a file lofty reads; none for a format it does not know.
fn replay_gain_native(path: &Path) -> Option<ReplayGainTags> {
    use lofty::prelude::*;
    let f = lofty_file(path)?;
    let tag = f.primary_tag().or_else(|| f.first_tag());
    Some(ReplayGainTags::read(|name| {
        let key = match name {
            "REPLAYGAIN_TRACK_GAIN" => ItemKey::ReplayGainTrackGain,
            "REPLAYGAIN_ALBUM_GAIN" => ItemKey::ReplayGainAlbumGain,
            "R128_TRACK_GAIN" => ItemKey::R128TrackGain,
            "R128_ALBUM_GAIN" => ItemKey::R128AlbumGain,
            _ => return None,
        };
        tag?.get_string(key).map(str::to_owned)
    }))
}

fn probe_native(path: &Path, cancelled: &AtomicBool) -> Option<(MediaTags, ReplayGainTags)> {
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    let source: Box<dyn crate::MediaByteSource> = if is_http_input(path) {
        UrlAudioSourceFactory::new(path.to_str()?, Default::default())
            .open()
            .ok()?
    } else {
        Box::new(FileMediaByteSource::open(path).ok()?)
    };
    let tags = super::tags::probe(source).ok()?;
    (!cancelled.load(Ordering::Acquire)).then_some(tags)
}
