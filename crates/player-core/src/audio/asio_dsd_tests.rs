use crate::audio::asio_writer::{AsioWriteFormat, DopWireFormat, DsdWireFormat, NativeAsioWriter};

#[test]
fn native_asio_writer_uses_the_dsd_idle_pattern_in_every_wire_order() {
    for (wire, expected) in [
        (DsdWireFormat::Msb1, vec![0x69, 0x69]),
        (DsdWireFormat::Lsb1, vec![0x96, 0x96]),
        (
            DsdWireFormat::Ner8,
            vec![
                0, 255, 255, 0, 255, 0, 0, 255, 0, 255, 255, 0, 255, 0, 0, 255,
            ],
        ),
    ] {
        let mut left = vec![0; expected.len()];
        let mut right = vec![0; expected.len()];
        NativeAsioWriter::new(AsioWriteFormat::Dsd(wire), 2)
            .unwrap()
            .write_dsd(&[0x69; 4], &mut [&mut left, &mut right]);
        assert_eq!(left, expected, "{wire:?}");
        assert_eq!(right, expected, "{wire:?}");
    }
}

#[test]
fn native_asio_dop_mute_keeps_markers_in_every_integer_wire_format() {
    for (format, muted, resumed) in [
        (
            DopWireFormat::Int24Le,
            vec![0x69, 0x69, 0xfa],
            vec![0x34, 0x12, 0x05],
        ),
        (
            DopWireFormat::Int24Be,
            vec![0xfa, 0x69, 0x69],
            vec![0x05, 0x12, 0x34],
        ),
        (
            DopWireFormat::Int32Le { valid_bits: 32 },
            vec![0, 0x69, 0x69, 0xfa],
            vec![0, 0x34, 0x12, 0x05],
        ),
        (
            DopWireFormat::Int32Be { valid_bits: 32 },
            vec![0xfa, 0x69, 0x69, 0],
            vec![0x05, 0x12, 0x34, 0],
        ),
        (
            DopWireFormat::Int32Le { valid_bits: 24 },
            vec![0x69, 0x69, 0xfa, 0xff],
            vec![0x34, 0x12, 0x05, 0],
        ),
        (
            DopWireFormat::Int32Be { valid_bits: 24 },
            vec![0xff, 0xfa, 0x69, 0x69],
            vec![0, 0x05, 0x12, 0x34],
        ),
    ] {
        let mut writer = NativeAsioWriter::new(AsioWriteFormat::Dop(format), 1).unwrap();
        let mut output = vec![0; muted.len()];
        writer.write_dsd(&[0x12, 0x34], &mut [&mut output]);
        assert_eq!(output, resumed);
        writer.write_dsd(&[0x69, 0x69], &mut [&mut output]);
        assert_eq!(output, muted);
        writer.write_dsd(&[0x12, 0x34], &mut [&mut output]);
        assert_eq!(output, resumed);
    }
}

#[test]
fn asio_dsd_writer_reverses_dsf_bytes_for_lsb_and_keeps_dff_msb() {
    let source = [0x80, 0x01, 0xa5, 0x5a];
    let mut left = [0_u8; 2];
    let mut right = [0_u8; 2];
    let mut outputs: [&mut [u8]; 2] = [&mut left, &mut right];
    NativeAsioWriter::new(AsioWriteFormat::Dsd(DsdWireFormat::Msb1), 2)
        .expect("MSB writer")
        .write_dsd(&source, &mut outputs);
    assert_eq!((left, right), ([0x80, 0xa5], [0x01, 0x5a]));

    let mut left = [0_u8; 2];
    let mut right = [0_u8; 2];
    let mut outputs: [&mut [u8]; 2] = [&mut left, &mut right];
    NativeAsioWriter::new(AsioWriteFormat::Dsd(DsdWireFormat::Lsb1), 2)
        .expect("LSB writer")
        .write_dsd(&source, &mut outputs);
    assert_eq!((left, right), ([0x01, 0xa5], [0x80, 0x5a]));
}

#[test]
fn asio_dsd_writer_expands_ner8_and_silences_missing_channels() {
    let mut first = [0_u8; 16];
    let mut second = [0xff_u8; 16];
    let mut outputs: [&mut [u8]; 2] = [&mut first, &mut second];
    NativeAsioWriter::new(AsioWriteFormat::Dsd(DsdWireFormat::Ner8), 1)
        .expect("NER8 writer")
        .write_dsd(&[0x81, 0x7e], &mut outputs);
    assert_eq!(&first[..8], &[0xff, 0, 0, 0, 0, 0, 0, 0xff]);
    assert_eq!(&first[8..], &[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
    assert_eq!(
        second,
        [0, 0xff, 0xff, 0, 0xff, 0, 0, 0xff, 0, 0xff, 0xff, 0, 0xff, 0, 0, 0xff]
    );
}

#[test]
fn asio_dop_markers_continue_across_callbacks_and_interleave_channels() {
    let mut writer =
        NativeAsioWriter::new(AsioWriteFormat::Dop(DopWireFormat::Int24Le), 2).expect("DoP writer");
    let mut left = [0_u8; 3];
    let mut right = [0_u8; 3];
    writer.write_dsd(&[0x12, 0x34, 0x56, 0x78], &mut [&mut left, &mut right]);
    assert_eq!(left, [0x56, 0x12, 0x05]);
    assert_eq!(right, [0x78, 0x34, 0x05]);

    let mut left = [0_u8; 3];
    let mut right = [0_u8; 3];
    writer.write_dsd(&[0x9a, 0xbc, 0xde, 0xf0], &mut [&mut left, &mut right]);
    assert_eq!(left, [0xde, 0x9a, 0xfa]);
    assert_eq!(right, [0xf0, 0xbc, 0xfa]);
}

#[test]
fn asio_dop_refuses_an_output_without_twenty_four_valid_bits() {
    assert!(NativeAsioWriter::new(
        AsioWriteFormat::Dop(DopWireFormat::Int32Le { valid_bits: 20 }),
        2
    )
    .is_err());
}

#[test]
fn asio_dop_int32_carrier_alignment_and_sign_extension_match_the_wire() {
    for (format, expected) in [
        (
            DopWireFormat::Int32Le { valid_bits: 32 },
            [0, 0x34, 0x12, 5, 0, 0x78, 0x56, 0xfa],
        ),
        (
            DopWireFormat::Int32Be { valid_bits: 32 },
            [5, 0x12, 0x34, 0, 0xfa, 0x56, 0x78, 0],
        ),
        (
            DopWireFormat::Int32Le { valid_bits: 24 },
            [0x34, 0x12, 5, 0, 0x78, 0x56, 0xfa, 0xff],
        ),
        (
            DopWireFormat::Int32Be { valid_bits: 24 },
            [0, 5, 0x12, 0x34, 0xff, 0xfa, 0x56, 0x78],
        ),
    ] {
        let mut output = [0; 8];
        NativeAsioWriter::new(AsioWriteFormat::Dop(format), 1)
            .unwrap()
            .write_dsd(&[0x12, 0x34, 0x56, 0x78], &mut [&mut output]);
        assert_eq!(output, expected, "{format:?}");
    }
}
#[test]
fn asio_raw_writer_keeps_planar_channels() {
    let mut writer = NativeAsioWriter::new(AsioWriteFormat::Dsd(DsdWireFormat::Msb1), 2)
        .expect("raw DSD writer");
    let source = [0x12, 0x34, 0x56, 0x78];
    let mut left = [0_u8; 2];
    let mut right = [0_u8; 2];

    // SAFETY: the two pointers address the two 16-bit-sample ASIO channel buffers.
    unsafe {
        writer.write_dsd_raw_channel(&source, left.as_mut_ptr(), 16, 0);
        writer.write_dsd_raw_channel(&source, right.as_mut_ptr(), 16, 1);
    };
    assert_eq!(left, [0x12, 0x56]);
    assert_eq!(right, [0x34, 0x78]);
}
