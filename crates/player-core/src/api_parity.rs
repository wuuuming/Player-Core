//! The Windows and Linux `AudioPlayer` offer the same API (principles B5): every public method
//! and every `AudioOpenOptions` field of one is on the other with the same signature, except
//! what one host cannot do, named below with why (and in `docs/map.md`, "Linux"). Read
//! from the two sources, so the check runs on either host.

use std::collections::BTreeMap;

/// Types one host names apart, as (that host's name, the name both are compared in).
const WINDOWS_TYPES: &[(&str, &str)] = &[
    // Linux reports an open's failure as any other playback error.
    ("AudioOpenError", "AudioError"),
];
const LINUX_TYPES: &[(&str, &str)] = &[
    // Windows picks one of a file's audio streams; Linux plays FFmpeg's default one.
    ("TrackHint", "AudioTrackSelection"),
];

/// What only the Windows player has.
const WINDOWS_ONLY: &[&str] = &[
    // The playback rate: the Windows output resamples on the fly; the Linux writer plays at the
    // sound server's rate.
    "fn set_playback_rate",
    "fn playback_rate",
    // The decoder's stream description, the callback metrics and the WASAPI timing policy belong
    // to the Windows output path.
    "fn stream_info",
    "fn metrics",
    "fn policy",
    "field policy",
];

fn source(path: &str) -> String {
    let file = format!("{}/src/{path}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{file}: {e}"))
}

/// Splits `text` at top-level commas (outside `()`, `<>`, `[]`).
fn split_top(text: &str) -> Vec<String> {
    let (mut parts, mut cur, mut depth) = (Vec::new(), String::new(), 0i32);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '-' if chars.peek() == Some(&'>') => {
                cur.push(c);
                cur.push(chars.next().unwrap_or('>'));
                continue;
            }
            '(' | '<' | '[' => depth += 1,
            ')' | '>' | ']' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    parts.push(cur);
    parts
        .into_iter()
        .map(|p| p.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|p| !p.is_empty())
        .collect()
}

/// A type with the host's own names put in the shared words.
fn shared_words(ty: &str, linux: bool) -> String {
    let mut ty = ty.to_owned();
    for (own, shared) in if linux { LINUX_TYPES } else { WINDOWS_TYPES } {
        ty = ty.replace(own, shared);
    }
    ty
}

/// `impl AudioPlayer`'s public methods (`fn name` → receiver and argument types → result) and
/// `AudioOpenOptions`'s fields (`field name` → type).
fn api(path: &str, linux: bool) -> BTreeMap<String, String> {
    let text = source(path);
    let mut out = BTreeMap::new();
    let imp = text
        .find("\nimpl AudioPlayer {")
        .unwrap_or_else(|| panic!("{path}: no impl AudioPlayer"));
    let mut rest = &text[imp..];
    while let Some(at) = rest.find("\n    pub fn ") {
        rest = &rest[at + "\n    pub fn ".len()..];
        let (Some(name_end), Some(open)) = (rest.find(['(', '<']), rest.find('(')) else {
            panic!("{path}: a method without arguments");
        };
        let name = &rest[..name_end];
        let mut depth = 0;
        let Some(close) = rest[open..].char_indices().find_map(|(i, c)| {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            (depth == 0).then_some(open + i)
        }) else {
            panic!("{path}: {name}'s arguments do not close");
        };
        let args: Vec<String> = split_top(&rest[open + 1..close])
            .into_iter()
            .map(|a| {
                if a.contains("self") {
                    a
                } else {
                    a.split_once(':')
                        .map_or(a.clone(), |(_, t)| t.trim().to_owned())
                }
            })
            .map(|a| shared_words(&a, linux))
            .collect();
        let body = rest[close + 1..]
            .find('{')
            .map_or(close + 1, |b| close + 1 + b);
        let result = rest[close + 1..body]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        out.insert(
            format!("fn {name}"),
            format!("({}) {}", args.join(", "), shared_words(&result, linux)),
        );
        // The next item at this depth ends the impl.
        if rest
            .find("\n}")
            .is_some_and(|end| rest.find("\n    pub fn ").is_none_or(|next| end < next))
        {
            break;
        }
    }
    let opts = text
        .find("pub struct AudioOpenOptions {")
        .unwrap_or_else(|| panic!("{path}: no AudioOpenOptions"));
    let body = &text[opts..];
    let body = &body[body.find('{').unwrap_or(0) + 1..body.find("\n}").unwrap_or(body.len())];
    for line in body
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("pub "))
    {
        let Some((name, ty)) = line["pub ".len()..].split_once(':') else {
            panic!("{path}: a field without a type: {line}");
        };
        out.insert(
            format!("field {}", name.trim()),
            shared_words(ty.trim().trim_end_matches(','), linux),
        );
    }
    out
}

#[test]
fn audio_player_is_the_same_on_windows_and_linux() {
    let windows = api("audio/player.rs", false);
    let linux = api("linux/player.rs", true);
    let mut differences = Vec::new();
    for (item, sig) in &windows {
        match linux.get(item) {
            None if WINDOWS_ONLY.contains(&item.as_str()) => {}
            None => differences.push(format!("{item}: only on Windows")),
            Some(_) if WINDOWS_ONLY.contains(&item.as_str()) => {
                differences.push(format!("{item}: on Linux too (drop it from WINDOWS_ONLY)"))
            }
            Some(other) if other != sig => {
                differences.push(format!("{item}: Windows {sig}, Linux {other}"))
            }
            Some(_) => {}
        }
    }
    for item in linux.keys().filter(|i| !windows.contains_key(*i)) {
        differences.push(format!("{item}: only on Linux"));
    }
    assert!(
        differences.is_empty(),
        "the two players differ (B5):\n{}",
        differences.join("\n")
    );
    assert!(
        windows.contains_key("fn open_with") && windows.contains_key("field effects"),
        "the reader found the API"
    );
}
