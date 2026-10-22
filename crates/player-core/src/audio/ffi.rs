//! The authored C shim's audio ABI: the opaque decoder, the `#[repr(C)]` structs that cross
//! the boundary, the status codes, the `extern "C"` declarations, and the native handle that
//! wraps them.

use crate::source::{BnByteSource, NativeMediaByteSource};
use std::ffi::c_char;
use std::ptr::{self, NonNull};

pub(super) const AUDIO_EOF: i32 = 1;
/// `BN_AUDIO_AGAIN`: no packet arrived within the wait slice. The network is still delivering.
pub(super) const AUDIO_AGAIN: i32 = 2;
/// `BN_AUDIO_INVALID_STREAM`, the C shim's refusal of a stream index that is not audio.
pub(super) const BN_AUDIO_INVALID_STREAM: i32 = -118;

pub(super) enum BnAudioDecoder {}

#[repr(C)]
#[derive(Default)]
pub(super) struct BnFrameTiming {
    pub(super) pts: i64,
    pub(super) has_timestamp: i32,
    pub(super) used_best_effort_timestamp: i32,
}

#[repr(C)]
#[derive(Default)]
pub(super) struct BnAudioStreamInfo {
    pub(super) time_base_num: i32,
    pub(super) time_base_den: i32,
    pub(super) start_time: i64,
    pub(super) has_start_time: i32,
    pub(super) format_start_time_us: i64,
    pub(super) has_format_start_time: i32,
    pub(super) source_sample_rate: i32,
    pub(super) source_channels: i32,
    pub(super) output_sample_rate: i32,
    pub(super) output_channels: i32,
    pub(super) stream_index: i32,
    pub(super) source_layout_mask: u64,
    pub(super) source_layout_assumed: i32,
}

pub(super) const AUDIO_TRACK_LANGUAGE_CAPACITY: usize = 64;
pub(super) const AUDIO_TRACK_CODEC_CAPACITY: usize = 64;

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct BnAudioTrackInfo {
    pub(super) stream_index: i32,
    pub(super) channels: i32,
    pub(super) language: [c_char; AUDIO_TRACK_LANGUAGE_CAPACITY],
    pub(super) codec: [c_char; AUDIO_TRACK_CODEC_CAPACITY],
}

impl Default for BnAudioTrackInfo {
    fn default() -> Self {
        Self {
            stream_index: 0,
            channels: 0,
            language: [0; AUDIO_TRACK_LANGUAGE_CAPACITY],
            codec: [0; AUDIO_TRACK_CODEC_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Default)]
pub(super) struct BnAudioChunk {
    pub(super) samples: *const f32,
    pub(super) frames: i32,
    pub(super) timing: BnFrameTiming,
}

#[repr(C)]
#[derive(Default)]
pub(super) struct BnDsdChunk {
    pub(super) bytes: *const u8,
    pub(super) frames: i32,
    pub(super) timing: BnFrameTiming,
}

/// `BnMediaTags` of `player_core.h`. `metadata` and `picture` are native memory until
/// `bn_media_tags_free`.
#[repr(C)]
pub(super) struct BnMediaTags {
    pub(super) metadata: *mut c_char,
    pub(super) metadata_size: usize,
    pub(super) duration_us: i64,
    pub(super) has_duration: i32,
    pub(super) has_audio: i32,
    pub(super) has_video: i32,
    pub(super) picture: *mut u8,
    pub(super) picture_size: usize,
    pub(super) audio_codec: [c_char; 32],
    pub(super) audio_sample_rate: i32,
    pub(super) audio_bits_raw: i32,
    pub(super) audio_bits_coded: i32,
    pub(super) audio_bit_rate: i64,
    pub(super) container: [c_char; 32],
}

impl Default for BnMediaTags {
    fn default() -> Self {
        Self {
            metadata: ptr::null_mut(),
            metadata_size: 0,
            duration_us: 0,
            has_duration: 0,
            has_audio: 0,
            has_video: 0,
            picture: ptr::null_mut(),
            picture_size: 0,
            audio_codec: [0; 32],
            audio_sample_rate: 0,
            audio_bits_raw: 0,
            audio_bits_coded: 0,
            audio_bit_rate: 0,
            container: [0; 32],
        }
    }
}

extern "C" {
    #[cfg(any(feature = "asio", test))]
    pub(super) fn bn_audio_decoder_enable_dsd(
        decoder: *mut BnAudioDecoder,
        info: *mut BnAudioStreamInfo,
        error: *mut c_char,
        error_capacity: usize,
    ) -> i32;
    pub(super) fn bn_audio_decoder_read_dsd(
        decoder: *mut BnAudioDecoder,
        chunk: *mut BnDsdChunk,
        error: *mut c_char,
        error_capacity: usize,
    ) -> i32;
    pub(super) fn bn_media_tags_probe_source(
        source: *const BnByteSource,
        out_tags: *mut BnMediaTags,
        error: *mut c_char,
        error_capacity: usize,
    ) -> i32;
    pub(super) fn bn_media_tags_free(tags: *mut BnMediaTags);
    pub(super) fn bn_audio_decoder_transport_error(decoder: *const BnAudioDecoder) -> i32;
    pub(super) fn bn_audio_decoder_open_source_stream_with_error(
        source: *const BnByteSource,
        audio_stream_index: i32,
        output_sample_rate: i32,
        output_channels: i32,
        out_decoder: *mut *mut BnAudioDecoder,
        out_info: *mut BnAudioStreamInfo,
        error: *mut c_char,
        error_capacity: usize,
        native_error: *mut i32,
    ) -> i32;
    pub(super) fn bn_audio_tracks_enumerate(
        local_path: *const c_char,
        out_tracks: *mut BnAudioTrackInfo,
        track_capacity: usize,
        out_track_count: *mut usize,
        error: *mut c_char,
        error_capacity: usize,
    ) -> i32;
    pub(super) fn bn_audio_decoder_destroy(decoder: *mut *mut BnAudioDecoder);
    pub(super) fn bn_audio_decoder_cancel(decoder: *mut BnAudioDecoder);
    pub(super) fn bn_audio_decoder_set_source_local(decoder: *mut BnAudioDecoder, local: i32);
    pub(super) fn bn_audio_decoder_input_state(
        decoder: *mut BnAudioDecoder,
        out_queued_us: *mut i64,
        out_ended: *mut i32,
    ) -> i32;
    pub(super) fn bn_audio_decoder_codec_name(decoder: *const BnAudioDecoder) -> *const c_char;
    pub(super) fn bn_audio_decoder_tag(
        decoder: *const BnAudioDecoder,
        name: *const c_char,
    ) -> *const c_char;
    pub(super) fn bn_audio_decoder_resampler_input_frames(decoder: *const BnAudioDecoder) -> i64;
    pub(super) fn bn_audio_decoder_passed_over(decoder: *const BnAudioDecoder) -> i64;
    pub(super) fn bn_audio_decoder_seek(
        decoder: *mut BnAudioDecoder,
        target_pts: i64,
        error: *mut c_char,
        error_capacity: usize,
    ) -> i32;
    pub(super) fn bn_audio_decoder_decode_next(
        decoder: *mut BnAudioDecoder,
        out_chunk: *mut BnAudioChunk,
        error: *mut c_char,
        error_capacity: usize,
    ) -> i32;
}

pub struct NativeAudioDecoder {
    pub(super) raw: NonNull<BnAudioDecoder>,
    pub(super) dsd: bool,
    /// FFmpeg holds this source's address in its AVIO opaque for the decoder's entire life, so the
    /// source is owned BY the decoder and dies with it — never beside it, never before it.
    pub(super) _source: NativeMediaByteSource,
}

// SAFETY: the decoder and its source are reached only through this handle, so by one thread at
// a time (it moves, it is never shared); the shim keeps no thread-affine state in them.
unsafe impl Send for NativeAudioDecoder {}

impl Drop for NativeAudioDecoder {
    fn drop(&mut self) {
        // SAFETY: the live decoder this handle owns.
        unsafe { bn_audio_decoder_cancel(self.raw.as_ptr()) };
        self._source.cancel();
        let mut raw = self.raw.as_ptr();
        // SAFETY: the decoder this handle owns, destroyed once (the handle is being dropped);
        // its source outlives the call (a field, dropped after this body).
        unsafe { bn_audio_decoder_destroy(&mut raw) };
    }
}
