//! The transport seam: the protocol options one open carries, the URL-backed source FFmpeg's
//! own protocol layer reads, and the custom-AVIO adapter that carries a Rust
//! [`MediaByteSource`] into avformat.

use super::error::{io_error_code, NativeMessage, NativeReadError};
use super::ffi::{
    bn_url_source_destroy, bn_url_source_open_with_error, bn_url_source_read, bn_url_source_size,
    BnByteSource, BnHttpOpenOptions, BnUrlSource, AVERROR_EOF, AVERROR_EXIT,
    AVERROR_INVALID_ARGUMENT, AVERROR_NOT_SUPPORTED, AVSEEK_FORCE, AVSEEK_SIZE, SEEK_CUR, SEEK_END,
    SEEK_SET,
};
use crate::{
    AudioInput, AudioSourceError, AudioSourceFactory, HttpOpenOptions, MediaByteSource,
    SourceLocality,
};
use std::ffi::{c_void, CString};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Borrows `http`'s two C strings for exactly the duration of one open call.
fn with_native<T>(http: &HttpOpenOptions, apply: impl FnOnce(*const BnHttpOpenOptions) -> T) -> T {
    let headers = http
        .header_block()
        .and_then(|block| CString::new(block).ok());
    let user_agent = http
        .user_agent
        .as_ref()
        .and_then(|agent| CString::new(agent.as_str()).ok());
    if headers.is_none() && user_agent.is_none() {
        return apply(ptr::null());
    }
    let native = BnHttpOpenOptions {
        headers: headers.as_ref().map_or(ptr::null(), |value| value.as_ptr()),
        user_agent: user_agent
            .as_ref()
            .map_or(ptr::null(), |value| value.as_ptr()),
    };
    apply(&native)
}

/// A network resource read positionally through FFmpeg's own protocol layer.
///
/// The audio session's transport seam is a [`MediaByteSource`], so a DASH audio representation
/// reaches it as bytes, not as a URL. The bytes come from the HTTP client already linked into this
/// crate: the same `avio`, the same [`HttpOpenOptions`]. A second HTTP stack would be a second set of redirect, range, and timeout
/// manners to keep true to the first.
pub struct UrlMediaByteSource {
    pub(super) raw: NonNull<BnUrlSource>,
    pub(super) size: Option<u64>,
    // Native callbacks borrow this Arc's allocation until bn_url_source_destroy returns.
    cancellation: Arc<UrlCancellation>,
}

// SAFETY: the handle is owned exclusively by this value and every native entry point takes it by pointer;
// no other thread can observe it. `Send` is what lets the audio session hand the source to the
// decode worker that will read it.
unsafe impl Send for UrlMediaByteSource {}

impl Drop for UrlMediaByteSource {
    fn drop(&mut self) {
        let mut raw = self.raw.as_ptr();
        // SAFETY: the source this value owns, destroyed once; the cancellation state its callback
        // reads is a field, dropped after this body.
        unsafe { bn_url_source_destroy(&mut raw) };
    }
}

/// Cancellation owns no native pointer. A retained canceller is safe after source destruction,
/// including concurrent destruction. Each open has its own flag and shares only the factory flag.
struct UrlCancellation {
    source: AtomicBool,
    factory: Arc<AtomicBool>,
}

impl UrlCancellation {
    fn is_cancelled(&self) -> bool {
        self.source.load(Ordering::Acquire) || self.factory.load(Ordering::Acquire)
    }
}

unsafe extern "C" fn url_cancelled(opaque: *mut c_void) -> i32 {
    // SAFETY: `opaque` is the `UrlCancellation` given at open, kept alive by the source's `Arc`
    // for as long as the native source can call back.
    let state = unsafe { &*opaque.cast::<UrlCancellation>() };
    i32::from(state.is_cancelled())
}

impl MediaByteSource for UrlMediaByteSource {
    fn canceller(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let cancellation = self.cancellation.clone();
        Some(Arc::new(move || {
            cancellation.source.store(true, Ordering::Release)
        }))
    }

    fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    fn read(&mut self, offset: u64, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.read_with_timeout(offset, buffer, std::time::Duration::from_secs(10))
    }

    fn read_with_timeout(
        &mut self,
        offset: u64,
        buffer: &mut [u8],
        timeout: std::time::Duration,
    ) -> std::io::Result<usize> {
        if timeout.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "read budget expired",
            ));
        }
        let Ok(offset) = i64::try_from(offset) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "offset does not fit the transport's addressing",
            ));
        };
        let size = i32::try_from(buffer.len()).unwrap_or(i32::MAX);
        if size <= 0 {
            return Ok(0);
        }
        let budget_us = i64::try_from(timeout.as_micros())
            .unwrap_or(i64::MAX)
            .max(1);
        // SAFETY: the source this value owns; `buffer` is writable for `size` ≤ its length bytes.
        let read = unsafe {
            bn_url_source_read(
                self.raw.as_ptr(),
                offset,
                buffer.as_mut_ptr(),
                size,
                budget_us,
            )
        };
        match usize::try_from(read) {
            Ok(read) => Ok(read),
            Err(_) => Err(std::io::Error::new(
                if read == AVERROR_EXIT {
                    std::io::ErrorKind::Interrupted
                } else {
                    std::io::ErrorKind::Other
                },
                NativeReadError {
                    code: read,
                    message: format!(
                        "the media url refused a read of {size} bytes at {offset} ({read})"
                    ),
                },
            )),
        }
    }

    fn size(&self) -> Option<u64> {
        self.size
    }
}

/// The network factory: reopening is opening the same URL again, with the same admission headers.
///
/// A remote transport may not assume what a local file makes cheap — so every `open()` is its own
/// connection with its own cursor, which is exactly what one session's two decoders need.
pub struct UrlAudioSourceFactory {
    pub(super) url: String,
    pub(super) http: HttpOpenOptions,
    cancelled: media_io::CancelFlag,
}

impl UrlAudioSourceFactory {
    pub fn new(url: impl Into<String>, http: HttpOpenOptions) -> Self {
        Self {
            url: url.into(),
            http,
            cancelled: Default::default(),
        }
    }
}

impl AudioSourceFactory for UrlAudioSourceFactory {
    fn open(&self) -> Result<Box<dyn MediaByteSource>, AudioSourceError> {
        self.cancelled.refuse(self.label())?;
        let url = CString::new(self.url.as_str())
            .map_err(|_| AudioSourceError::open(format!("{}: url contains a nul", self.label())))?;
        let cancellation = Arc::new(UrlCancellation {
            source: AtomicBool::new(false),
            factory: self.cancelled.flag(),
        });
        let mut raw = ptr::null_mut();
        let mut error = NativeMessage::new();
        let mut native_error = 0;
        // SAFETY: `url` is NUL-terminated; `options` is live for the closure; the cancellation
        // state outlives the source (it moves into it below); out-parameters are live locals and
        // the message buffer is given with its capacity.
        let status = with_native(&self.http, |options| unsafe {
            bn_url_source_open_with_error(
                url.as_ptr(),
                options,
                url_cancelled,
                Arc::as_ptr(&cancellation).cast_mut().cast(),
                &mut raw,
                error.as_mut_ptr(),
                error.capacity(),
                &mut native_error,
            )
        });
        let raw = NonNull::new(raw).filter(|_| status == 0).ok_or_else(|| {
            AudioSourceError::open(format!("{}: {}", self.label(), error.message()))
                .with_cause(crate::AudioSourceCause::Native(native_error))
        })?;
        // SAFETY: the source just opened.
        let size = u64::try_from(unsafe { bn_url_source_size(raw.as_ptr()) }).ok();
        let source = UrlMediaByteSource {
            raw,
            size,
            cancellation,
        };
        if source.is_cancelled() {
            // Cancelled while it opened: as every factory says a cancelled open.
            return Err(AudioSourceError::cancelled(format!(
                "{}: cancelled",
                self.label()
            )));
        }
        Ok(Box::new(source))
    }

    fn input(&self) -> Option<AudioInput> {
        Some(AudioInput::Url(self.url.clone(), self.http.clone()))
    }

    fn cancel(&self) {
        self.cancelled.cancel();
    }

    fn cancellation(&self) -> Option<Arc<AtomicBool>> {
        Some(self.cancelled.flag())
    }

    fn label(&self) -> String {
        media_io::address_label(&self.url)
    }
}

pub(super) struct MediaByteSourceState {
    pub(super) source: Box<dyn MediaByteSource>,
    pub(super) position: u64,
}

/// The Rust side of the custom-AVIO trampoline. It is `pub(crate)` because every native open (the
/// decoder, the next song's, the tags probe) uses the SAME adapter: one byte-source vocabulary in
/// this crate, not two.
pub(crate) struct NativeMediaByteSource {
    // Only the native demux/probe owner may access this state. Owner-side cancel and locality
    // never borrow it while an FFI read holds &mut MediaByteSourceState on the demux thread.
    state: Box<std::cell::UnsafeCell<MediaByteSourceState>>,
    canceller: Option<Arc<dyn Fn() + Send + Sync>>,
    locality: SourceLocality,
    path: Option<Vec<u16>>,
}

impl NativeMediaByteSource {
    /// Aborts a read blocked on the transport. The reader thread is about to be told to stop.
    pub(crate) fn cancel(&self) {
        if let Some(cancel) = self.canceller.as_ref() {
            cancel();
        }
    }

    pub(crate) fn new(source: Box<dyn MediaByteSource>) -> Self {
        use std::os::windows::ffi::OsStrExt;
        let path = source
            .local_path()
            .map(|path| path.as_os_str().encode_wide().chain(Some(0)).collect());
        Self {
            path,
            canceller: source.canceller(),
            locality: source.locality(),
            state: Box::new(std::cell::UnsafeCell::new(MediaByteSourceState {
                source,
                position: 0,
            })),
        }
    }

    /// `1` for a local source, `0` for a network one: what `bn_*_decoder_set_source_local` takes.
    pub(crate) fn native_locality(&self) -> i32 {
        i32::from(self.locality == SourceLocality::Local)
    }

    pub(crate) fn descriptor(&mut self) -> BnByteSource {
        BnByteSource {
            opaque: self.state.get().cast(),
            read: media_byte_source_read,
            seek: media_byte_source_seek,
            cancelled: media_byte_source_cancelled,
            read_timed: media_byte_source_read_timed,
            local_path: self
                .path
                .as_ref()
                .map_or(std::ptr::null(), |path| path.as_ptr()),
        }
    }
}

pub(super) unsafe extern "C" fn media_byte_source_cancelled(opaque: *mut c_void) -> i32 {
    // SAFETY: `opaque` is the `MediaByteSourceState` its `NativeMediaByteSource` gave the shim,
    // alive until the decoder that calls back is destroyed (the decoder owns the source).
    let state = unsafe { &*opaque.cast::<MediaByteSourceState>() };
    i32::from(state.source.is_cancelled())
}

pub(super) unsafe extern "C" fn media_byte_source_read(
    opaque: *mut c_void,
    buffer: *mut u8,
    buffer_size: i32,
) -> i32 {
    // SAFETY: the caller's contract, passed on unchanged.
    unsafe { media_byte_source_read_timed(opaque, buffer, buffer_size, 0) }
}

pub(super) unsafe extern "C" fn media_byte_source_read_timed(
    opaque: *mut c_void,
    buffer: *mut u8,
    buffer_size: i32,
    budget_us: i64,
) -> i32 {
    if opaque.is_null() || buffer.is_null() || buffer_size <= 0 {
        return AVERROR_INVALID_ARGUMENT;
    }
    // SAFETY: as in `media_byte_source_cancelled`; the shim calls one callback at a time, so this
    // is the only reference to the state.
    let state = unsafe { &mut *opaque.cast::<MediaByteSourceState>() };
    // SAFETY: the shim's buffer, checked non-null, writable for `buffer_size` > 0 bytes.
    let buffer = unsafe {
        std::slice::from_raw_parts_mut(buffer, usize::try_from(buffer_size).unwrap_or_default())
    };
    let result = if budget_us > 0 {
        state.source.read_with_timeout(
            state.position,
            buffer,
            std::time::Duration::from_micros(budget_us as u64),
        )
    } else {
        state.source.read(state.position, buffer)
    };
    match result {
        Ok(0) => AVERROR_EOF,
        Ok(read) if read <= buffer.len() => {
            let Some(position) = state.position.checked_add(read as u64) else {
                return AVERROR_INVALID_ARGUMENT;
            };
            state.position = position;
            i32::try_from(read).unwrap_or(AVERROR_INVALID_ARGUMENT)
        }
        Err(error) => error
            .get_ref()
            .and_then(|error| error.downcast_ref::<NativeReadError>())
            .map_or_else(|| io_error_code(error.kind()), |error| error.code),
        Ok(_) => AVERROR_INVALID_ARGUMENT,
    }
}

pub(super) unsafe extern "C" fn media_byte_source_seek(
    opaque: *mut c_void,
    offset: i64,
    whence: i32,
) -> i64 {
    if opaque.is_null() {
        return i64::from(AVERROR_INVALID_ARGUMENT);
    }
    // SAFETY: as in `media_byte_source_read_timed`.
    let state = unsafe { &mut *opaque.cast::<MediaByteSourceState>() };
    if state.source.is_cancelled() {
        return i64::from(AVERROR_EXIT);
    }
    if whence & AVSEEK_SIZE != 0 {
        return state
            .source
            .size()
            .and_then(|size| i64::try_from(size).ok())
            .unwrap_or(i64::from(AVERROR_NOT_SUPPORTED));
    }

    let base = match whence & !AVSEEK_FORCE {
        SEEK_SET => 0_i128,
        SEEK_CUR => i128::from(state.position),
        SEEK_END => match state.source.size() {
            Some(size) => i128::from(size),
            None => return i64::from(AVERROR_NOT_SUPPORTED),
        },
        _ => return i64::from(AVERROR_INVALID_ARGUMENT),
    };
    let target = base + i128::from(offset);
    let Ok(target) = u64::try_from(target) else {
        return i64::from(AVERROR_INVALID_ARGUMENT);
    };
    let Ok(result) = i64::try_from(target) else {
        return i64::from(AVERROR_INVALID_ARGUMENT);
    };
    state.position = target;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cancelled address says so as every factory does: `Cancelled`, before any connection
    /// (it tried to connect, and a cancel during the open said `Open`).
    #[test]
    fn a_cancelled_address_refuses_to_open_as_cancelled() {
        let f =
            UrlAudioSourceFactory::new("http://127.0.0.1:9/song.flac", HttpOpenOptions::default());
        f.cancel();
        let Err(e) = f.open() else {
            panic!("a cancelled factory opened")
        };
        assert_eq!(e.kind, media_io::AudioSourceKind::Cancelled);
    }
}
