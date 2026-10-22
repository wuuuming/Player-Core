use super::*;

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("hex"))
        .collect()
}

/// `TITLE "天水的源泉"` and two more lines, as a Chinese machine's EAC writes them: GBK.
const GBK: &str = "5449544c452022cceccbaeb5c4d4b4c8aa220a504552464f524d45522022c0d6badb220a5449544c452022b3bfd0c7c1c8cbde20b7e7d7a3c9f1cbcc220a";
/// `TITLE "これから出会う貴方に捧ぐ"` and two more, as a Japanese machine's: Shift-JIS.
const SHIFT_JIS: &str = "5449544c45202282b182ea82a982e78f6f89ef82a48b4d95fb82c995f982ae220a504552464f524d4552202288c39296220a5449544c4520228cf582cc97d689f4220a";

#[test]
fn a_bom_or_valid_utf8_says_what_the_text_is() {
    let text = "TITLE \"天水的源泉\"\n";
    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend_from_slice(text.as_bytes());
    assert_eq!(decode_guessing(&bom), text);
    assert_eq!(decode_guessing(text.as_bytes()), text);
    assert_eq!(decode_ansi(&bom), text);
    let mut utf16 = vec![0xFF, 0xFE];
    utf16.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    assert_eq!(decode_guessing(&utf16), text);
    let mut utf16_be = vec![0xFE, 0xFF];
    utf16_be.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
    assert_eq!(decode_guessing(&utf16_be), text);
}

#[test]
fn a_legacy_sheet_is_read_in_the_code_page_it_was_written_in() {
    assert_eq!(
        decode_guessing(&hex(GBK)),
        "TITLE \"天水的源泉\"\nPERFORMER \"乐痕\"\nTITLE \"晨星寥宿 风祝神颂\"\n",
    );
    assert_eq!(
        decode_guessing(&hex(SHIFT_JIS)),
        "TITLE \"これから出会う貴方に捧ぐ\"\nPERFORMER \"暗猪\"\nTITLE \"光の輪廻\"\n",
    );
}

#[test]
fn mojibake_reads_worse_than_the_words() {
    assert!(plausibility("これから出会う") > plausibility("偙傟偐傜弌夛偆"));
    assert!(plausibility("天水的源泉") > plausibility("ﾌﾋﾋｮｵﾔｴﾈ"));
    assert!(plausibility("\u{E000}\u{FFFD}") < 0);
}

#[test]
fn an_invalid_sequence_is_no_reading() {
    // A lead byte with nothing after it is invalid in every double-byte page.
    assert_eq!(in_code_page(&[0x81], 936), None);
    assert_eq!(in_code_page(b"ascii", 936).as_deref(), Some("ascii"));
    assert_eq!(in_code_page(&[], 932).as_deref(), Some(""));
}

#[test]
fn the_system_page_wins_a_tie_and_unicode_wins_before_guessing() {
    // B0 A1 names different common ideographs in GBK and Big5. They score equally,
    // so the selected system page decides the reading.
    let bytes = b"\xb0\xa1";
    assert_eq!(
        plausibility(&in_code_page(bytes, 936).unwrap()),
        plausibility(&in_code_page(bytes, 950).unwrap())
    );
    assert_eq!(guess(bytes, 936), in_code_page(bytes, 936).unwrap());
    assert_eq!(guess(bytes, 950), in_code_page(bytes, 950).unwrap());
    assert_eq!(guess("届かない恋 🎵".as_bytes(), 950), "届かない恋 🎵");
}
