//! What a container says about itself: its tags, its length, whether it has a picture that
//! moves, and the cover attached to it. A probe — it opens the container, reads, and closes it.

use super::error::{AudioOpenError, NativeError};
use super::ffi::{
    bn_audio_decoder_tag, bn_media_tags_free, bn_media_tags_probe_source, BnMediaTags,
    NativeAudioDecoder,
};
use crate::source::{NativeMediaByteSource, NativeMessage};
use crate::tag_rule::Said;
use crate::ReplayGainTags;
use crate::{AudioResolution, MediaTags};
use std::ffi::{c_char, CString};
use std::time::Duration;

/// A fixed native name (codec, container): ASCII, NUL-terminated.
fn name_text(bytes: &[c_char]) -> Option<String> {
    let text = crate::fixed_c_text(bytes);
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// The native `key NUL value NUL` pairs, each value decoded as UTF-8 or else by code-page guess
/// (an ID3v1 or RIFF tag written in Shift-JIS, GBK…).
fn said(blob: &[u8]) -> Said {
    let mut parts = blob.split(|b| *b == 0);
    let mut pairs = Vec::new();
    while let (Some(key), Some(value)) = (parts.next(), parts.next()) {
        if key.is_empty() {
            break;
        }
        pairs.push((
            String::from_utf8_lossy(key).into_owned(),
            crate::legacy_text::decode_guessing(value),
        ));
    }
    Said(pairs)
}

/// Reads `source`'s tags. The source is consumed: the probe owns its cursor, and a session that
/// plays the same media opens its own.
pub fn probe_media_tags(
    source: Box<dyn crate::MediaByteSource>,
) -> Result<MediaTags, AudioOpenError> {
    // The size is the transport's to know, before the container is opened over it.
    let file_size = source.size();
    let mut source = NativeMediaByteSource::new(source);
    let descriptor = source.descriptor();
    let mut native = BnMediaTags::default();
    let mut error = NativeMessage::new();
    // SAFETY: the descriptor's source lives in `source` until after the call; `native` and the
    // message buffer (with its capacity) are live locals.
    let status = unsafe {
        bn_media_tags_probe_source(
            &descriptor,
            &mut native,
            error.as_mut_ptr(),
            error.capacity(),
        )
    };
    if status != 0 {
        return Err(AudioOpenError::Native(NativeError {
            operation: "tag reading",
            status,
            transport: 0,
            message: error.message(),
        }));
    }
    let cover = (!native.picture.is_null() && native.picture_size > 0).then(|| {
        // SAFETY: the shim's picture, `picture_size` bytes, alive until `bn_media_tags_free`.
        unsafe { std::slice::from_raw_parts(native.picture, native.picture_size) }.to_vec()
    });
    let said = if native.metadata.is_null() || native.metadata_size == 0 {
        Said::default()
    } else {
        // SAFETY: the shim's tags, `metadata_size` bytes, alive until `bn_media_tags_free`.
        said(unsafe {
            std::slice::from_raw_parts(native.metadata.cast::<u8>(), native.metadata_size)
        })
    };
    let words = said.tags();
    // SAFETY: the tags the probe filled, freed once; nothing borrowed from them is kept (copied above).
    unsafe { bn_media_tags_free(&mut native) };
    let codec = name_text(&native.audio_codec).unwrap_or_default();
    Ok(MediaTags {
        duration: (native.has_duration != 0)
            .then(|| Duration::from_micros(u64::try_from(native.duration_us).unwrap_or(0))),
        has_audio: native.has_audio != 0,
        has_video: native.has_video != 0,
        cover,
        resolution: (native.has_audio != 0)
            .then(|| {
                AudioResolution::of(
                    codec,
                    u32::try_from(native.audio_sample_rate).unwrap_or(0),
                    (native.audio_bits_raw.into(), native.audio_bits_coded.into()),
                    u32::try_from(native.audio_bit_rate).ok(),
                )
            })
            .flatten(),
        container: name_text(&native.container),
        file_size,
        ..words
    })
}

/// The song's ReplayGain values, from the tags of the file `decoder` decodes. Read on the opening
/// thread, before the decoder moves to its worker.
pub fn replay_gain_tags(decoder: &NativeAudioDecoder) -> ReplayGainTags {
    ReplayGainTags::read(|name| {
        let name = CString::new(name).ok()?;
        // SAFETY: the decoder `decoder` owns, live for the call; a NUL-terminated name. The value,
        // when not null, is a NUL-terminated string the decoder owns, copied before returning.
        unsafe { crate::c_text(bn_audio_decoder_tag(decoder.raw.as_ptr(), name.as_ptr())) }
    })
}
