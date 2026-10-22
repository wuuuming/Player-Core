//! Native ASIO output buffer writers for DSD, native or carried in PCM (DoP).
//!
//! The render side supplies MSB-first packed DSD bytes. ASIO owns one buffer per output channel,
//! so this module only changes their layout; it neither allocates in a callback nor changes gain.

// Negotiation (`asio_host::negotiate_dsd`) asks `for_sample_type` with ASIO switched off too.
#![cfg_attr(not(feature = "asio"), allow(dead_code))]

use super::asio_host::AsioDsdTransport;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DsdWireFormat {
    Lsb1,
    Msb1,
    Ner8,
}

/// The integer PCM sample a DoP carrier word is written as: 24 bits, or 32 with its valid bits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DopWireFormat {
    Int24Le,
    Int24Be,
    Int32Le { valid_bits: u8 },
    Int32Be { valid_bits: u8 },
}

impl DopWireFormat {
    pub(super) const fn bytes_per_sample(self) -> usize {
        match self {
            Self::Int24Le | Self::Int24Be => 3,
            Self::Int32Le { .. } | Self::Int32Be { .. } => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AsioWriteFormat {
    Dsd(DsdWireFormat),
    Dop(DopWireFormat),
}

/// Holds only callback-persistent state. The DoP marker changes once per carrier frame, not
/// once per callback, so pauses and uneven buffer sizes cannot restart it on the wrong marker.
impl AsioWriteFormat {
    /// How a DSD stream carried by `transport` is written to an ASIO channel of `sample_type`
    /// (the driver's `ASIOSampleType`): native DSD in one of its three layouts, or DoP in a 24- or
    /// 32-bit integer with at least 24 valid bits. `Err` for a type the transport cannot use, so
    /// a type negotiation accepts is one the writer has a layout for.
    pub(super) fn for_sample_type(
        transport: AsioDsdTransport,
        sample_type: i32,
    ) -> Result<Self, String> {
        if transport == AsioDsdTransport::Native {
            return match sample_type {
                32 => Ok(Self::Dsd(DsdWireFormat::Lsb1)),
                33 => Ok(Self::Dsd(DsdWireFormat::Msb1)),
                40 => Ok(Self::Dsd(DsdWireFormat::Ner8)),
                _ => Err(format!(
                    "ASIO output channel reports non-DSD sample type {sample_type}"
                )),
            };
        }
        Ok(Self::Dop(match sample_type {
            1 => DopWireFormat::Int24Be,
            17 => DopWireFormat::Int24Le,
            2 => DopWireFormat::Int32Be { valid_bits: 32 },
            18 => DopWireFormat::Int32Le { valid_bits: 32 },
            11 => DopWireFormat::Int32Be { valid_bits: 24 },
            27 => DopWireFormat::Int32Le { valid_bits: 24 },
            _ => {
                return Err(format!(
                "DoP needs a 24-bit or 32-bit integer ASIO output, not sample type {sample_type}"
            ))
            }
        }))
    }
}

pub(super) struct NativeAsioWriter {
    format: AsioWriteFormat,
    channels: usize,
    dop_second_marker: bool,
}

impl NativeAsioWriter {
    pub(super) fn new(format: AsioWriteFormat, channels: u16) -> Result<Self, String> {
        if channels == 0 {
            return Err("an ASIO writer needs an output channel".to_owned());
        }
        if let AsioWriteFormat::Dop(
            DopWireFormat::Int32Le { valid_bits } | DopWireFormat::Int32Be { valid_bits },
        ) = format
        {
            if !(24..=32).contains(&valid_bits) {
                return Err("DoP needs 24 to 32 valid bits in an Int32 ASIO output".to_owned());
            }
        }
        Ok(Self {
            format,
            channels: usize::from(channels),
            dop_second_marker: false,
        })
    }

    #[cfg(test)]
    pub(super) fn write_dsd(&mut self, input: &[u8], outputs: &mut [&mut [u8]]) {
        let initial_marker = self.dop_second_marker;
        for (channel, output) in outputs.iter_mut().enumerate() {
            self.dop_second_marker = initial_marker;
            let frames = match self.format {
                AsioWriteFormat::Dsd(DsdWireFormat::Ner8) => output.len(),
                AsioWriteFormat::Dsd(_) => output.len() * 8,
                AsioWriteFormat::Dop(format) => output.len() / format.bytes_per_sample(),
            };
            // SAFETY: `frames` is counted from the borrowed slice's length in this format.
            unsafe {
                self.write_dsd_raw_channel(input, output.as_mut_ptr(), frames, channel);
            }
        }
    }
    /// Writes one selected ASIO output.  A raw ASIO stream may expose a different sample type on
    /// each output, so the bridge owns one writer per output instead of assuming channel zero
    /// describes the whole device.
    ///
    /// # Safety
    /// `output` is valid for writes of `buffer_frames` frames of this writer's wire format (1-bit
    /// formats pack eight frames a byte) and aliases nothing else for the call: the ASIO buffer of
    /// that output for this callback.
    pub(super) unsafe fn write_dsd_raw_channel(
        &mut self,
        input: &[u8],
        output: *mut u8,
        buffer_frames: usize,
        source_channel: usize,
    ) {
        match self.format {
            AsioWriteFormat::Dsd(DsdWireFormat::Msb1)
            | AsioWriteFormat::Dsd(DsdWireFormat::Lsb1) => {
                // SAFETY: the function's contract: `buffer_frames` 1-bit frames, eight a byte.
                let output = unsafe { std::slice::from_raw_parts_mut(output, buffer_frames / 8) };
                let frames = input.len() / self.channels;
                for (frame, destination) in output.iter_mut().enumerate() {
                    let byte = dsd_byte(input, frames, self.channels, frame, source_channel);
                    *destination =
                        if matches!(self.format, AsioWriteFormat::Dsd(DsdWireFormat::Lsb1)) {
                            byte.reverse_bits()
                        } else {
                            byte
                        };
                }
            }
            AsioWriteFormat::Dsd(DsdWireFormat::Ner8) => {
                // SAFETY: the function's contract: `buffer_frames` one-byte frames.
                let output = unsafe { std::slice::from_raw_parts_mut(output, buffer_frames) };
                let frames = input.len() / self.channels;
                for (frame, destination) in output.iter_mut().enumerate() {
                    let byte = dsd_byte(input, frames, self.channels, frame / 8, source_channel);
                    *destination = if byte & (0x80 >> (frame & 7)) != 0 {
                        0xff
                    } else {
                        0
                    };
                }
            }
            AsioWriteFormat::Dop(format) => {
                let width = format.bytes_per_sample();
                // SAFETY: the function's contract: `buffer_frames` frames of `width` bytes.
                let output =
                    unsafe { std::slice::from_raw_parts_mut(output, buffer_frames * width) };
                let frames = input.len() / self.channels;
                for frame in 0..buffer_frames {
                    let marker = if self.dop_second_marker { 0xfa } else { 0x05 };
                    self.dop_second_marker = !self.dop_second_marker;
                    let first = dsd_byte(input, frames, self.channels, frame * 2, source_channel);
                    let second =
                        dsd_byte(input, frames, self.channels, frame * 2 + 1, source_channel);
                    write_dop_sample(
                        format,
                        first,
                        second,
                        marker,
                        &mut output[frame * width..(frame + 1) * width],
                    );
                }
            }
        }
    }
}

fn dsd_byte(input: &[u8], frames: usize, channels: usize, frame: usize, channel: usize) -> u8 {
    if channel < channels && frame < frames {
        input[frame * channels + channel]
    } else {
        0x69
    }
}

/// DoP transports sixteen consecutive DSD bits in each PCM frame. The marker follows those two
/// payload bytes in the numerical 24-bit word, and alternates for every carrier frame.
fn write_dop_sample(format: DopWireFormat, first: u8, second: u8, marker: u8, target: &mut [u8]) {
    match format {
        DopWireFormat::Int24Le => target.copy_from_slice(&[second, first, marker]),
        DopWireFormat::Int24Be => target.copy_from_slice(&[marker, first, second]),
        DopWireFormat::Int32Le { valid_bits } => {
            let shift = valid_bits.saturating_sub(24) / 8;
            let bytes = if shift == 0 {
                [
                    second,
                    first,
                    marker,
                    if marker & 0x80 != 0 { 0xff } else { 0 },
                ]
            } else {
                [0, second, first, marker]
            };
            target.copy_from_slice(&bytes);
        }
        DopWireFormat::Int32Be { valid_bits } => {
            let shift = valid_bits.saturating_sub(24) / 8;
            let bytes = if shift == 0 {
                [
                    if marker & 0x80 != 0 { 0xff } else { 0 },
                    marker,
                    first,
                    second,
                ]
            } else {
                [marker, first, second, 0]
            };
            target.copy_from_slice(&bytes);
        }
    }
}
