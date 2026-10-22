//! Text files written in whatever code page their writer's machine had.
//!
//! A CUE sheet or an `.m3u` list is older than UTF-8's reach on Windows: EAC and foobar2000 wrote
//! them in the system's ANSI code page for years — GBK on a Chinese machine, Shift-JIS on a
//! Japanese one — and a sheet ripped in Tokyo is played in Shanghai. A BOM says what the text is;
//! bytes that are valid UTF-8 are UTF-8 (a legacy double-byte text is almost never valid UTF-8 by
//! accident); anything else is decoded the way Windows itself decodes a code page, and for a
//! sheet the code page is guessed.

use windows::Win32::Globalization::{GetACP, MultiByteToWideChar, MB_ERR_INVALID_CHARS};

/// Simplified Chinese (GBK), Japanese (Shift-JIS), Traditional Chinese (Big5).
const CJK_CODE_PAGES: [u32; 3] = [936, 932, 950];

/// The text a BOM or valid UTF-8 says, or `None` for bytes that are neither.
fn unicode(bytes: &[u8]) -> Option<String> {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return Some(String::from_utf8_lossy(rest).into_owned());
    }
    let utf16 = |rest: &[u8], unit: fn([u8; 2]) -> u16| {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|pair| unit([pair[0], pair[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return Some(utf16(rest, u16::from_le_bytes));
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return Some(utf16(rest, u16::from_be_bytes));
    }
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}

/// `bytes` in the code page `code_page`, or `None` when they are not valid in it.
fn in_code_page(bytes: &[u8], code_page: u32) -> Option<String> {
    if bytes.is_empty() {
        return Some(String::new());
    }
    // SAFETY: both slices are live for the calls; the first asks only for the length.
    let length = unsafe { MultiByteToWideChar(code_page, MB_ERR_INVALID_CHARS, bytes, None) };
    let mut wide = vec![0_u16; usize::try_from(length).ok().filter(|length| *length > 0)?];
    // SAFETY: as above; `wide` holds the length the first call reported.
    let written =
        unsafe { MultiByteToWideChar(code_page, MB_ERR_INVALID_CHARS, bytes, Some(&mut wide)) };
    wide.truncate(
        usize::try_from(written)
            .ok()
            .filter(|written| *written > 0)?,
    );
    Some(String::from_utf16_lossy(&wide))
}

fn system_code_page() -> u32 {
    // SAFETY: no arguments; it reads the process's ANSI code page.
    unsafe { GetACP() }
}

/// How much `text` reads like words: letters, kana and the common ideographs count for it;
/// half-width katakana, the private use area and the rest of the symbol planes — what one
/// double-byte code page's text becomes when read as another's — count against it. Kana count
/// the most: Shift-JIS read as GBK is all ideographs, and read as itself it is kana among them.
/// (A Japanese title of kanji alone reads as well either way, and goes to the system's page.)
fn plausibility(text: &str) -> i64 {
    text.chars()
        .map(|character| match u32::from(character) {
            0x20..=0x7E | 0x09 | 0x0A | 0x0D => 1,
            0x3041..=0x30FF => 3,
            // CJK punctuation, the common ideographs, full-width forms.
            0x3000..=0x3040 | 0x4E00..=0x9FFF | 0xFF01..=0xFF5E => 2,
            // Half-width katakana: Shift-JIS reading GBK's lead bytes.
            0xFF61..=0xFF9F => -4,
            0xE000..=0xF8FF | 0xFFFD => -4,
            // Rare ideographs (extension A) and box drawing: another code page's mojibake.
            0x3400..=0x4DBF | 0x2500..=0x257F => -2,
            _ => 0,
        })
        .sum()
}

/// A text file in the system's code page unless it says (or is) Unicode: how an `.m3u` is read,
/// the format's own rule.
pub fn decode_ansi(bytes: &[u8]) -> String {
    unicode(bytes)
        .or_else(|| in_code_page(bytes, system_code_page()))
        .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned())
}

/// A text file whose code page is guessed: Unicode when it says or is so, else whichever of the
/// system's code page and the three CJK ones reads it without an invalid sequence and most like
/// words — the system's winning a tie. How a CUE sheet is read.
pub fn decode_guessing(bytes: &[u8]) -> String {
    guess(bytes, system_code_page())
}

fn guess(bytes: &[u8], system: u32) -> String {
    if let Some(text) = unicode(bytes) {
        return text;
    }
    let mut best: Option<(i64, String)> = None;
    for code_page in
        std::iter::once(system).chain(CJK_CODE_PAGES.into_iter().filter(|page| *page != system))
    {
        let Some(text) = in_code_page(bytes, code_page) else {
            continue;
        };
        let score = plausibility(&text);
        if best.as_ref().is_none_or(|(best, _)| score > *best) {
            best = Some((score, text));
        }
    }
    best.map_or_else(
        || String::from_utf8_lossy(bytes).into_owned(),
        |(_, text)| text,
    )
}

#[cfg(test)]
mod tests;
