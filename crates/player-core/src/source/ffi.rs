//! The authored C shim's ABI: opaque handles, the `#[repr(C)]` structs that cross the
//! boundary, the FFmpeg status codes this crate compares against, and the `extern "C"`
//! declarations themselves.

use std::ffi::{c_char, c_void};

pub(super) const ERROR_CAPACITY: usize = 512;
pub(super) const AVERROR_EOF: i32 = -541_478_725;
pub(super) const AVERROR_EXIT: i32 = -1_414_092_869;
pub(super) const AVERROR_INVALID_ARGUMENT: i32 = -22;
pub(super) const AVERROR_NOT_SUPPORTED: i32 = -40;
pub(super) const AVSEEK_SIZE: i32 = 0x10000;
pub(super) const AVSEEK_FORCE: i32 = 0x20000;
pub(super) const SEEK_SET: i32 = 0;
pub(super) const SEEK_CUR: i32 = 1;
pub(super) const SEEK_END: i32 = 2;

pub(super) enum BnUrlSource {}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct BnByteSource {
    pub(super) opaque: *mut c_void,
    pub(super) read: unsafe extern "C" fn(*mut c_void, *mut u8, i32) -> i32,
    pub(super) seek: unsafe extern "C" fn(*mut c_void, i64, i32) -> i64,
    pub(super) cancelled: unsafe extern "C" fn(*mut c_void) -> i32,
    pub(super) read_timed: unsafe extern "C" fn(*mut c_void, *mut u8, i32, i64) -> i32,
    pub(super) local_path: *const u16,
}

#[repr(C)]
pub(super) struct BnHttpOpenOptions {
    pub(super) headers: *const c_char,
    pub(super) user_agent: *const c_char,
}

/// `BnBufferCensus`: measurement only.
#[cfg(feature = "instrument")]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NativeBufferCensus {
    /// Packet payload the sound's demux queues hold, how many packets carry it, and the bytes of
    /// the buffers those packets were read into.
    pub audio_queue_bytes: i64,
    pub audio_queue_packets: i64,
    pub audio_queue_buffer_bytes: i64,
}

#[cfg(feature = "instrument")]
extern "C" {
    pub(super) fn bn_buffer_census(out: *mut NativeBufferCensus);
}

/// What every demux queue in the process holds right now. Measurement only.
#[cfg(feature = "instrument")]
pub fn native_buffer_census() -> NativeBufferCensus {
    let mut census = NativeBufferCensus::default();
    // SAFETY: a live out-parameter; the shim reads its counters atomically.
    unsafe { bn_buffer_census(&mut census) };
    census
}

extern "C" {
    pub(super) fn bn_error_is_network_failure(code: i32) -> i32;
    pub(super) fn bn_io_error_code(kind: i32) -> i32;
    pub(super) fn bn_url_source_open_with_error(
        url: *const c_char,
        http: *const BnHttpOpenOptions,
        cancelled: unsafe extern "C" fn(*mut c_void) -> i32,
        cancel_opaque: *mut c_void,
        out_source: *mut *mut BnUrlSource,
        error: *mut c_char,
        error_capacity: usize,
        native_error: *mut i32,
    ) -> i32;
    pub(super) fn bn_url_source_destroy(source: *mut *mut BnUrlSource);
    pub(super) fn bn_url_source_size(source: *const BnUrlSource) -> i64;
    pub(super) fn bn_url_source_read(
        source: *mut BnUrlSource,
        offset: i64,
        buffer: *mut u8,
        size: i32,
        budget_us: i64,
    ) -> i32;
}
