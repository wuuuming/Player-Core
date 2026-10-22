//! Generated one-bit files (DSD64, 2 822 400 Hz, stereo) for the tests that need DSF or DFF:
//! the payload is `pattern`, so a reader's bytes can be checked without FFmpeg's DSD decoder.
//! For this crate's unit tests and, with `test-support`, the tests of its users.

use std::{fs, path::Path};

/// DSD64: one-bit samples per second, and bytes (eight samples) per channel per second.
pub const DSD64: u32 = 2_822_400;
pub const BYTES_PER_SECOND: usize = DSD64 as usize / 8;

/// The byte (MSB first) of `channel` at byte frame `frame`.
pub fn pattern(frame: usize, channel: usize) -> u8 {
    ((frame * 2 + channel) as u8)
        .wrapping_mul(37)
        .wrapping_add((frame / 251) as u8)
}

/// A DSF of `frames` byte frames: LSB first, planar in 4096-byte blocks per channel, the last
/// block padded (the sample count says where the media ends).
pub fn write_dsf(path: &Path, frames: usize) {
    const BLOCK: usize = 4096;
    let blocks = frames.div_ceil(BLOCK);
    let mut payload = vec![0; blocks * BLOCK * 2];
    for frame in 0..frames {
        for channel in 0..2 {
            payload[(frame / BLOCK) * BLOCK * 2 + channel * BLOCK + frame % BLOCK] =
                pattern(frame, channel).reverse_bits();
        }
    }
    let mut bytes = Vec::new();
    bytes.extend(b"DSD ");
    bytes.extend(28u64.to_le_bytes());
    bytes.extend((92u64 + payload.len() as u64).to_le_bytes());
    bytes.extend(0u64.to_le_bytes());
    bytes.extend(b"fmt ");
    bytes.extend(52u64.to_le_bytes());
    // Version 1, raw DSD, stereo channel type, 2 channels, the rate, 1 bit per sample (LSB first).
    for word in [1u32, 0, 2, 2, DSD64, 1] {
        bytes.extend(word.to_le_bytes());
    }
    bytes.extend(((frames * 8) as u64).to_le_bytes());
    bytes.extend((BLOCK as u32).to_le_bytes());
    bytes.extend(0u32.to_le_bytes());
    bytes.extend(b"data");
    bytes.extend((12u64 + payload.len() as u64).to_le_bytes());
    bytes.extend(payload);
    fs::write(path, bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
}

/// A DSDIFF (DFF) of `frames` byte frames: MSB first, channels interleaved byte by byte.
pub fn write_dff(path: &Path, frames: usize) {
    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        out.extend((body.len() as u64).to_be_bytes());
        out.extend(body);
        if body.len() % 2 == 1 {
            out.push(0);
        }
        out
    }
    let mut channels = 2u16.to_be_bytes().to_vec();
    channels.extend(b"SLFTSRGT");
    // The compression name is a Pascal string padded to an even length.
    let mut compression = b"DSD ".to_vec();
    compression.push(14);
    compression.extend(b"not compressed");
    compression.push(0);
    let mut sound = b"SND ".to_vec();
    sound.extend(chunk(b"FS  ", &DSD64.to_be_bytes()));
    sound.extend(chunk(b"CHNL", &channels));
    sound.extend(chunk(b"CMPR", &compression));
    let payload: Vec<u8> = (0..frames)
        .flat_map(|frame| [pattern(frame, 0), pattern(frame, 1)])
        .collect();
    let mut form = b"DSD ".to_vec();
    form.extend(chunk(b"FVER", &0x0105_0000u32.to_be_bytes()));
    form.extend(chunk(b"PROP", &sound));
    form.extend(chunk(b"DSD ", &payload));
    fs::write(path, chunk(b"FRM8", &form)).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
}
