//! Text files in whatever code page their writer's machine had (GBK on a Chinese machine,
//! Shift-JIS on a Japanese one): the Windows player-core rules, with `encoding_rs` for
//! Windows' code-page tables. A BOM or valid UTF-8 says what the text is; else the CJK code
//! pages are tried and the one that reads most like words wins (GBK on a tie: the
//! "system" page of a Linux box has no meaning for files ripped on Windows).
use encoding_rs::{Encoding, BIG5, EUC_JP, GBK, SHIFT_JIS};

fn unicode(bytes: &[u8]) -> Option<String> {
    if let Some(b) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return Some(String::from_utf8_lossy(b).into_owned());
    }
    let utf16 = |b: &[u8], unit: fn([u8; 2]) -> u16| {
        let u: Vec<u16> = b.chunks_exact(2).map(|c| unit([c[0], c[1]])).collect();
        String::from_utf16_lossy(&u)
    };
    if let Some(b) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return Some(utf16(b, u16::from_le_bytes));
    }
    if let Some(b) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return Some(utf16(b, u16::from_be_bytes));
    }
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}

/// `bytes` in `enc`, or `None` when they are not valid in it.
fn strict(bytes: &[u8], enc: &'static Encoding) -> Option<String> {
    enc.decode_without_bom_handling_and_without_replacement(bytes)
        .map(|c| c.into_owned())
}

/// The same scoring as player-core's: kana count most, mojibake ranges count against.
fn plausibility(text: &str) -> i64 {
    text.chars()
        .map(|c| match u32::from(c) {
            0x20..=0x7E | 0x09 | 0x0A | 0x0D => 1,
            0x3041..=0x30FF => 3,
            0x3000..=0x3040 | 0x4E00..=0x9FFF | 0xFF01..=0xFF5E => 2,
            0xFF61..=0xFF9F => -4,
            0xE000..=0xF8FF | 0xFFFD => -4,
            0x3400..=0x4DBF | 0x2500..=0x257F => -2,
            _ => 0,
        })
        .sum()
}

/// Unicode when it says or is so, else the CJK page that reads it validly and most like words.
pub fn decode_guessing(bytes: &[u8]) -> String {
    if let Some(t) = unicode(bytes) {
        return t;
    }
    let mut best: Option<(i64, String)> = None;
    for enc in [GBK, SHIFT_JIS, BIG5, EUC_JP] {
        let Some(t) = strict(bytes, enc) else {
            continue;
        };
        let score = plausibility(&t);
        if best.as_ref().is_none_or(|(b, _)| score > *b) {
            best = Some((score, t));
        }
    }
    // GBK and EUC-JP share the kana rows; on a tie GBK (tried first) wins: a sheet whose
    // kanji read as common ideographs in both is more often a Chinese rip.
    best.map_or_else(|| String::from_utf8_lossy(bytes).into_owned(), |(_, t)| t)
}

/// An `.m3u`: Unicode, else guessed as a sheet is (Linux has no ANSI code page to defer to).
pub fn decode_ansi(bytes: &[u8]) -> String {
    decode_guessing(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn hex(t: &str) -> Vec<u8> {
        (0..t.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&t[i..i + 2], 16).unwrap())
            .collect()
    }
    #[test]
    fn legacy_sheets_read_in_their_code_page() {
        let gbk = "5449544c452022cceccbaeb5c4d4b4c8aa220a504552464f524d45522022c0d6badb220a5449544c452022b3bfd0c7c1c8cbde20b7e7d7a3c9f1cbcc220a";
        let sjis = "5449544c45202282b182ea82a982e78f6f89ef82a48b4d95fb82c995f982ae220a504552464f524d4552202288c39296220a5449544c4520228cf582cc97d689f4220a";
        assert_eq!(
            decode_guessing(&hex(gbk)),
            "TITLE \"天水的源泉\"
PERFORMER \"乐痕\"
TITLE \"晨星寥宿 风祝神颂\"
"
        );
        assert_eq!(
            decode_guessing(&hex(sjis)),
            "TITLE \"これから出会う貴方に捧ぐ\"
PERFORMER \"暗猪\"
TITLE \"光の輪廻\"
"
        );
        assert_eq!(
            decode_guessing("TITLE \"天水\"".as_bytes()),
            "TITLE \"天水\""
        );
    }
}
