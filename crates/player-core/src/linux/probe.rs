use super::legacy_text;
use super::source::is_http_input;
use crate::tag_rule::Said;
use crate::ReplayGainTags;
use crate::{AudioResolution, MediaTags};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

fn json_str<'a>(j: &'a str, key: &str) -> Option<&'a str> {
    // Minimal: `"key": "value"` (ffprobe's JSON is flat in what we read).
    let pat = format!("\"{key}\": \"");
    let i = j.find(&pat)? + pat.len();
    let rest = &j[i..];
    let mut end = 0;
    let b = rest.as_bytes();
    while end < b.len() {
        if b[end] == b'\\' {
            end += 2;
            continue;
        }
        if b[end] == b'"' {
            break;
        }
        end += 1;
    }
    rest.get(..end)
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('r') => {}
            Some('t') => out.push('\t'),
            Some('u') => {
                let h: String = it.by_ref().take(4).collect();
                if let Some(ch) = u32::from_str_radix(&h, 16).ok().and_then(char::from_u32) {
                    out.push(ch);
                }
            }
            Some(o) => out.push(o),
            None => {}
        }
    }
    out
}

/// A number ffprobe writes bare (`"bits_per_sample": 16`).
fn json_num(j: &str, key: &str) -> Option<i64> {
    let pat = format!("\"{key}\": ");
    let rest = &j[j.find(&pat)? + pat.len()..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '-'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// The `"tags": { "k": "v", … }` object in `j` (one stream's, or the format's), in order.
fn tags_object(j: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(at) = j.find("\"tags\": {") else {
        return out;
    };
    let mut rest = &j[at + 9..];
    // Each `"key": "value"` until the object's `}` (ffprobe's tag values are strings).
    while let Some(open) = rest.find(['"', '}']) {
        if rest.as_bytes()[open] == b'}' {
            break;
        }
        let Some(key) = json_str_at(&rest[open..]) else {
            break;
        };
        let after = &rest[open + key.len() + 2..];
        let Some(colon) = after.find('"') else {
            break;
        };
        let Some(value) = json_str_at(&after[colon..]) else {
            break;
        };
        out.push((unescape(key), unescape(value)));
        rest = &after[colon + value.len() + 2..];
    }
    out
}

/// The JSON string that starts at `s` (its opening quote), without its quotes, still escaped.
fn json_str_at(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    if b.first() != Some(&b'"') {
        return None;
    }
    let mut end = 1;
    while end < b.len() {
        match b[end] {
            b'\\' => end += 2,
            b'"' => return s.get(1..end),
            _ => end += 1,
        }
    }
    None
}

/// What ffprobe's `j` says the file holds: the format's tags, then the audio stream's.
fn said_of(j: &str, audio: Option<&str>) -> Said {
    let format = j.find("\"format\":").map_or("", |i| &j[i..]);
    let mut pairs = tags_object(format);
    pairs.extend(audio.map(tags_object).unwrap_or_default());
    Said(pairs)
}

/// The audio stream's object in ffprobe's `j`: from its index to the next stream, or to the
/// format's object after the last one.
fn audio_stream(j: &str) -> Option<&str> {
    let i = j.find("\"codec_type\": \"audio\"")?;
    let start = j[..i].rfind("\"index\"").unwrap_or(0);
    let end = j[i..].find("\"index\"").map_or(j.len(), |e| i + e);
    let end = j[i..end].find("\"format\":").map_or(end, |e| i + e);
    Some(&j[start..end])
}

/// Reads tags, cover and lyrics with the system's ffprobe/ffmpeg (blocking).
pub fn probe_path(path: &Path) -> Option<MediaTags> {
    probe_path_cancellable(path, &AtomicBool::new(false))
}

pub fn probe_path_cancellable(path: &Path, cancelled: &AtomicBool) -> Option<MediaTags> {
    probe_with(path, true, cancelled)
}

/// Stream information for a live selection: no artwork connection, canceled with the selection.
pub fn probe_stream_cancellable(path: &Path, cancelled: &AtomicBool) -> Option<MediaTags> {
    probe_with(path, false, cancelled)
}

/// Tags and length without extracting the picture (one ffprobe, no ffmpeg).
pub fn probe_tags_only(path: &Path) -> Option<MediaTags> {
    read_tags_native(path).or_else(|| probe_with(path, false, &AtomicBool::new(false)))
}

/// Tags and length read in-process (lofty): the tag blocks and the stream header only, no
/// picture, no child process. None for what lofty does not read (TAK, DSD, …): ffprobe then.
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
/// the song's ReplayGain values. One ffprobe at most: a local file's values are read in process
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
    let Some(json) = ffprobe(path, cancelled) else {
        return (duration, ReplayGainTags::default());
    };
    let said = said_of(&json, audio_stream(&json));
    let replay_gain = ReplayGainTags::read(|name| said.get(name));
    let duration = duration.or_else(|| from_json(path, &json, false, cancelled).duration);
    (duration, replay_gain)
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

/// ffprobe's JSON for the container and its streams; none when it fails or is cancelled.
fn ffprobe(path: &Path, cancelled: &AtomicBool) -> Option<String> {
    let mut cmd = std::process::Command::new("ffprobe");
    cmd.args([
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
    ])
    .arg(path);
    let out = super::process::capture(
        &mut cmd,
        || cancelled.load(Ordering::Acquire),
        super::process::IO_TIMEOUT,
        2 * 1024 * 1024,
    )
    .ok()?;
    Some(String::from_utf8_lossy(&out).into_owned())
}

pub(super) fn probe_with(
    path: &Path,
    with_cover: bool,
    cancelled: &AtomicBool,
) -> Option<MediaTags> {
    let json = ffprobe(path, cancelled)?;
    Some(from_json(path, &json, with_cover, cancelled))
}

/// The tags ffprobe's `j` states, and the cover from a second process when `with_cover`.
fn from_json(path: &Path, j: &str, with_cover: bool, cancelled: &AtomicBool) -> MediaTags {
    let a = audio_stream(j);
    let has_audio = a.is_some();
    let num = |s: Option<&str>| s.and_then(|v| v.parse::<f64>().ok());
    let codec = a
        .and_then(|a| json_str(a, "codec_name"))
        .unwrap_or("")
        .to_owned();
    let raw = a
        .and_then(|a| json_str(a, "bits_per_raw_sample"))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let coded = a.and_then(|a| json_num(a, "bits_per_sample")).unwrap_or(0);
    let words = said_of(j, a).tags();
    let resolution = a.and_then(|a| {
        AudioResolution::of(
            codec,
            num(json_str(a, "sample_rate")).unwrap_or(0.0) as u32,
            (raw, coded),
            num(json_str(a, "bit_rate"))
                .or(num(json_str(j, "bit_rate")))
                .map(|b| b as u32),
        )
    });
    let fmt_at = j.find("\"format\":").unwrap_or(0);
    let f = &j[fmt_at..];
    let duration = num(json_str(f, "duration")).and_then(|n| Duration::try_from_secs_f64(n).ok());
    let has_video = j.contains("\"codec_type\": \"video\"") && !j.contains("\"attached_pic\": 1");
    let mut t = MediaTags {
        duration,
        has_audio,
        has_video,
        resolution,
        container: json_str(f, "format_name").map(|s| s.split(',').next().unwrap_or(s).to_owned()),
        file_size: std::fs::metadata(path).ok().map(|m| m.len()),
        ..words
    };
    if with_cover && j.contains("\"attached_pic\": 1") {
        let mut cmd = std::process::Command::new("ffmpeg");
        cmd.args(["-v", "error", "-nostdin", "-i"]).arg(path).args([
            "-an",
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-c:v",
            "copy",
            "-f",
            "image2pipe",
            "-",
        ]);
        t.cover = super::process::capture(
            &mut cmd,
            || cancelled.load(Ordering::Acquire),
            super::process::IO_TIMEOUT,
            16 * 1024 * 1024,
        )
        .ok()
        .filter(|b| !b.is_empty());
    }
    t
}
