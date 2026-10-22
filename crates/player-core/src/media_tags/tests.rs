use super::*;

/// One rule for every reader (it was two: Linux gave a stream without a rate a resolution,
/// `0 kHz`, and a bit rate of zero `0 kbps`).
#[test]
fn a_resolution_needs_a_rate_and_a_bit_rate_of_zero_is_none() {
    assert_eq!(
        AudioResolution::of("flac".into(), 0, (24, 0), Some(900_000)),
        None
    );
    let aac = AudioResolution::of("aac".into(), 44_100, (0, 0), Some(0)).unwrap();
    assert_eq!(
        (aac.bit_rate, aac.label()),
        (None, "44.1 kHz AAC".to_owned())
    );
    let flac = AudioResolution::of("flac".into(), 96_000, (0, 24), Some(2_000_000)).unwrap();
    assert!(flac.lossless);
    assert_eq!(flac.label(), "96 kHz / 24-bit FLAC");
}
