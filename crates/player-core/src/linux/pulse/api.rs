//! Public libpulse ABI; no access to pa_simple's private layout.
use std::ffi::{c_char, c_int, c_void};
use std::sync::OnceLock;

pub(super) type Ptr = *mut c_void;
pub(super) type Success = Option<unsafe extern "C" fn(Ptr, c_int, Ptr)>;
pub(super) type OperationFn = unsafe extern "C" fn(Ptr, Success, Ptr) -> Ptr;

#[repr(C)]
pub(super) struct SampleSpec {
    pub format: c_int,
    pub rate: u32,
    pub channels: u8,
}
#[repr(C)]
pub(super) struct BufferAttr {
    pub maxlength: u32,
    pub tlength: u32,
    pub prebuf: u32,
    pub minreq: u32,
    pub fragsize: u32,
}
/// Stable public pa_timing_info prefix through since_underrun (since PulseAudio 0.9.11).
/// libpulse may append fields; we only borrow this prefix, never allocate/copy its object.
#[repr(C)]
pub(super) struct Timing {
    pub timestamp: libc::timeval,
    pub synchronized_clocks: c_int,
    pub sink_usec: u64,
    pub source_usec: u64,
    pub transport_usec: u64,
    pub playing: c_int,
    pub write_index_corrupt: c_int,
    pub write_index: i64,
    pub read_index_corrupt: c_int,
    pub read_index: i64,
    pub configured_sink_usec: u64,
    pub configured_source_usec: u64,
    pub since_underrun: i64,
}

macro_rules! api {
    ($($name:ident: $signature:ty),+ $(,)?) => {
        pub(super) struct Lib { $(pub $name: $signature),+ }
        impl Lib {
            unsafe fn load(handle: Ptr) -> Option<Self> {
                $(let symbol = libc::dlsym(handle, concat!(stringify!($name), "\0").as_ptr().cast());
                if symbol.is_null() { return None; }
                let $name = std::mem::transmute::<Ptr, $signature>(symbol);)+
                Some(Self { $($name),+ })
            }
        }
    };
}

api! {
    pa_mainloop_new: unsafe extern "C" fn() -> Ptr,
    pa_mainloop_get_api: unsafe extern "C" fn(Ptr) -> Ptr,
    pa_mainloop_prepare: unsafe extern "C" fn(Ptr, c_int) -> c_int,
    pa_mainloop_poll: unsafe extern "C" fn(Ptr) -> c_int,
    pa_mainloop_dispatch: unsafe extern "C" fn(Ptr) -> c_int,
    pa_mainloop_free: unsafe extern "C" fn(Ptr),
    pa_context_new: unsafe extern "C" fn(Ptr, *const c_char) -> Ptr,
    pa_context_connect: unsafe extern "C" fn(Ptr, *const c_char, c_int, *const c_void) -> c_int,
    pa_context_get_state: unsafe extern "C" fn(*const c_void) -> c_int,
    pa_context_errno: unsafe extern "C" fn(*const c_void) -> c_int,
    pa_context_disconnect: unsafe extern "C" fn(Ptr),
    pa_context_unref: unsafe extern "C" fn(Ptr),
    pa_stream_new: unsafe extern "C" fn(Ptr, *const c_char, *const SampleSpec, *const c_void) -> Ptr,
    pa_stream_connect_playback: unsafe extern "C" fn(Ptr, *const c_char, *const BufferAttr, c_int, *const c_void, Ptr) -> c_int,
    pa_stream_get_state: unsafe extern "C" fn(*const c_void) -> c_int,
    pa_stream_writable_size: unsafe extern "C" fn(*const c_void) -> usize,
    pa_stream_write: unsafe extern "C" fn(Ptr, *const c_void, usize, Option<unsafe extern "C" fn(Ptr)>, i64, c_int) -> c_int,
    pa_stream_update_timing_info: OperationFn,
    pa_stream_get_timing_info: unsafe extern "C" fn(Ptr) -> *const Timing,
    pa_stream_get_latency: unsafe extern "C" fn(Ptr, *mut u64, *mut c_int) -> c_int,
    pa_stream_drain: OperationFn,
    pa_stream_flush: OperationFn,
    pa_stream_disconnect: unsafe extern "C" fn(Ptr) -> c_int,
    pa_stream_unref: unsafe extern "C" fn(Ptr),
    pa_operation_get_state: unsafe extern "C" fn(*const c_void) -> c_int,
    pa_operation_cancel: unsafe extern "C" fn(Ptr),
    pa_operation_unref: unsafe extern "C" fn(Ptr),
    pa_strerror: unsafe extern "C" fn(c_int) -> *const c_char,
}

pub(super) fn lib() -> Option<&'static Lib> {
    static LIB: OnceLock<Option<Lib>> = OnceLock::new();
    // SAFETY: dlopen/dlsym with static NUL-terminated names; the library stays loaded for the
    // process once its functions are kept (closed only when they were not).
    LIB.get_or_init(|| unsafe {
        let handle = libc::dlopen(c"libpulse.so.0".as_ptr(), libc::RTLD_NOW);
        if handle.is_null() {
            return None;
        }
        let loaded = Lib::load(handle);
        if loaded.is_none() {
            libc::dlclose(handle);
        }
        // Successful load stays resident for the process lifetime: function pointers use it.
        loaded
    })
    .as_ref()
}
