//! Single-owner, cancellable libpulse client. No PulseAudio call waits on the UI thread.
//!
//! What every `unsafe` block below relies on unless its comment says more: `lib` is the loaded
//! libpulse, kept for the process; a `Stream` is used through `&mut self` by its one owner (it is
//! not `Send`), and its mainloop, context and stream are created here, checked non-null before
//! use (or null and skipped), and freed once, in `Drop`, children first; the mainloop is
//! non-threaded, so a callback runs only inside `pa_mainloop_dispatch` on this thread.
use super::api::{self, BufferAttr, Lib, OperationFn, Ptr, SampleSpec};
use super::{bytes_to_micros, Recent, CHANNELS, FRAME_BYTES, RATE};
use crate::linux::types::{AudioError, AudioErrorKind};
use std::cell::Cell;
use std::ffi::{c_int, CString};
use std::ptr;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(super) type Cancel = Arc<dyn Fn() -> bool + Send + Sync>;
const TIMEOUT: Duration = Duration::from_secs(5);
const CONTEXT_READY: c_int = 4;
const CONTEXT_FAILED: c_int = 5;
const STREAM_READY: c_int = 2;
const STREAM_FAILED: c_int = 3;
const OPERATION_RUNNING: c_int = 0;
const ADJUST_LATENCY: c_int = 0x2000;
const NOT_MONOTONIC: c_int = 0x0004;
/// `PA_SAMPLE_FLOAT32LE`.
const SAMPLE_FLOAT32LE: c_int = 5;
/// `PA_SEEK_ABSOLUTE`: a write at a given index of the stream.
const SEEK_ABSOLUTE: c_int = 1;

pub struct Stream {
    lib: &'static Lib,
    mainloop: Ptr,
    context: Ptr,
    stream: Ptr,
    cancel: Option<Cancel>,
    /// What was written, the last of it kept for a fade-out (`fade_out`).
    written: Recent,
    started: bool,
}
// Raw handles intentionally remain !Send: their event loop and callbacks have one owner.

struct Operation {
    handle: Ptr,
    lib: &'static Lib,
}
impl Drop for Operation {
    fn drop(&mut self) {
        // SAFETY: the module contract; the operation is this stream's, unreferenced once here.
        unsafe {
            // The public API guarantees cancellation disables further callbacks, including
            // when the server-side request itself cannot be cancelled.
            (self.lib.pa_operation_cancel)(self.handle);
            (self.lib.pa_operation_unref)(self.handle);
        }
    }
}
unsafe extern "C" fn success(_stream: Ptr, success: c_int, userdata: Ptr) {
    // Dispatched only by this stream's non-threaded mainloop. Operation is cancelled before
    // the caller's Cell goes out of scope; Cell permits callback writes during the wait.
    (*userdata.cast::<Cell<c_int>>()).set(success);
}

impl Stream {
    pub fn open(
        what: &str,
        sink: Option<&str>,
        cancel: Option<Cancel>,
    ) -> Result<Self, AudioError> {
        let lib = api::lib()
            .ok_or_else(|| AudioError::new(AudioErrorKind::Output, "libpulse unavailable"))?;
        #[allow(clippy::expect_used, reason = "every NUL was just removed")]
        let name = CString::new(what.replace('\0', "")).expect("NUL removed");
        let sink = sink
            .map(CString::new)
            .transpose()
            .map_err(|e| AudioError::new(AudioErrorKind::Output, e.to_string()))?;
        let mut out = Self {
            lib,
            // SAFETY: no arguments; the result is checked below.
            mainloop: unsafe { (lib.pa_mainloop_new)() },
            context: ptr::null_mut(),
            stream: ptr::null_mut(),
            cancel,
            written: Recent::new(),
            started: false,
        };
        if out.mainloop.is_null() {
            return Err(AudioError::new(
                AudioErrorKind::Output,
                "cannot create PulseAudio mainloop",
            ));
        }
        let until = Instant::now() + TIMEOUT;
        // SAFETY: the module contract; `name`, `sink`, `spec` and `attr` are live locals the calls
        // copy; on an early return `out`'s Drop frees what was made.
        unsafe {
            out.context = (lib.pa_context_new)(
                (lib.pa_mainloop_get_api)(out.mainloop),
                c"NovaPlayer".as_ptr(),
            );
            if out.context.is_null() {
                return Err(AudioError::new(
                    AudioErrorKind::Output,
                    "cannot create PulseAudio context",
                ));
            }
            if (lib.pa_context_connect)(out.context, ptr::null(), 0, ptr::null()) < 0 {
                return Err(out.error("connect"));
            }
            while (lib.pa_context_get_state)(out.context) != CONTEXT_READY {
                out.wait(until, "connect")?;
            }
            let spec = SampleSpec {
                format: SAMPLE_FLOAT32LE,
                rate: RATE,
                channels: CHANNELS,
            };
            out.stream = (lib.pa_stream_new)(out.context, name.as_ptr(), &spec, ptr::null());
            if out.stream.is_null() {
                return Err(out.error("stream"));
            }
            let attr = BufferAttr {
                maxlength: u32::MAX,
                tlength: RATE * FRAME_BYTES / 10,
                prebuf: u32::MAX,
                minreq: u32::MAX,
                fragsize: u32::MAX,
            };
            // Request ~100 ms, not a hard bound. Explicit timing requests below replace the
            // interpolator: it favours smoothness and can report audio before actual start.
            if (lib.pa_stream_connect_playback)(
                out.stream,
                sink.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
                &attr,
                ADJUST_LATENCY | NOT_MONOTONIC,
                ptr::null(),
                ptr::null_mut(),
            ) < 0
            {
                return Err(out.error("open"));
            }
            while (lib.pa_stream_get_state)(out.stream) != STREAM_READY {
                out.wait(until, "open")?;
            }
        }
        Ok(out)
    }

    fn error(&self, what: &str) -> AudioError {
        // SAFETY: the module contract; the context is the one `open` made.
        let code = unsafe { (self.lib.pa_context_errno)(self.context) };
        // SAFETY: a plain value; the result is a static string or null.
        let text = unsafe { (self.lib.pa_strerror)(code) };
        // SAFETY: null or a static NUL-terminated string from libpulse.
        let text = unsafe { crate::c_text(text) }.unwrap_or_default();
        AudioError::new(
            AudioErrorKind::Output,
            format!("PulseAudio {what}: {text} ({code})"),
        )
    }

    /// Runs the callbacks for what has already arrived, without waiting.
    fn poll(&mut self, until: Instant, what: &str) -> Result<(), AudioError> {
        self.step(until, what, false)
    }

    /// Waits for the server's next event (or `until`) and runs its callbacks: the stream's
    /// state, its write requests and the operations' replies all arrive as events, so a caller
    /// looking for one of them waits here instead of polling.
    fn wait(&mut self, until: Instant, what: &str) -> Result<(), AudioError> {
        self.step(until, what, true)
    }

    fn step(&mut self, until: Instant, what: &str, block: bool) -> Result<(), AudioError> {
        if self.cancel.as_ref().is_some_and(|cancel| cancel()) {
            return Err(AudioError::new(
                AudioErrorKind::Cancelled,
                "operation cancelled",
            ));
        }
        if Instant::now() >= until {
            return Err(AudioError::new(
                AudioErrorKind::Output,
                format!("PulseAudio {what} timed out"),
            ));
        }
        let timeout = if block {
            // Microseconds, rounded up so the wait does not end before `until`.
            let left = until.saturating_duration_since(Instant::now());
            c_int::try_from(left.as_nanos().div_ceil(1_000)).unwrap_or(c_int::MAX)
        } else {
            0
        };
        // SAFETY: the module contract; callbacks run inside `dispatch`, on this thread.
        unsafe {
            if (self.lib.pa_mainloop_prepare)(self.mainloop, timeout) < 0
                || (self.lib.pa_mainloop_poll)(self.mainloop) < 0
                || (self.lib.pa_mainloop_dispatch)(self.mainloop) < 0
                || (self.lib.pa_context_get_state)(self.context) >= CONTEXT_FAILED
                || (!self.stream.is_null()
                    && (self.lib.pa_stream_get_state)(self.stream) >= STREAM_FAILED)
            {
                return Err(self.error(what));
            }
        }
        Ok(())
    }

    fn operation(&mut self, call: OperationFn, what: &str) -> Result<(), AudioError> {
        let result = Cell::new(-1);
        // SAFETY: the module contract; `result` outlives every callback: the operation that
        // carries it is cancelled (`pending`'s Drop) before `result` goes out of scope.
        let handle = unsafe {
            call(
                self.stream,
                Some(success),
                ptr::from_ref(&result).cast_mut().cast(),
            )
        };
        if handle.is_null() {
            return Err(self.error(what));
        }
        // Declared after result, so callbacks are cancelled before its userdata is dropped.
        let pending = Operation {
            handle,
            lib: self.lib,
        };
        let until = Instant::now() + TIMEOUT;
        // SAFETY: the operation just made, referenced by `pending` until it drops.
        while unsafe { (self.lib.pa_operation_get_state)(pending.handle) } == OPERATION_RUNNING {
            self.wait(until, what)?;
        }
        if result.get() == 1 {
            Ok(())
        } else {
            Err(self.error(what))
        }
    }

    pub fn write(&mut self, mut bytes: &[u8]) -> Result<(), AudioError> {
        let until = Instant::now() + TIMEOUT;
        while !bytes.is_empty() {
            self.poll(until, "write")?;
            // SAFETY: the module contract.
            let available = unsafe { (self.lib.pa_stream_writable_size)(self.stream) };
            if available == usize::MAX {
                return Err(self.error("write"));
            }
            let count = super::whole_frames(available.min(bytes.len()));
            if count == 0 {
                // Until the server asks for more.
                self.wait(until, "write")?;
                continue;
            }
            // NULL free callback requests a copy; no Rust slice outlives this call.
            // SAFETY: the module contract; `count` <= `bytes.len()` bytes are read, during the call.
            if unsafe {
                (self.lib.pa_stream_write)(self.stream, bytes.as_ptr().cast(), count, None, 0, 0)
            } < 0
            {
                return Err(self.error("write"));
            }
            self.written.push(&bytes[..count]);
            bytes = &bytes[count..];
        }
        Ok(())
    }

    /// Fresh server timing, never interpolated startup data or negative latency hidden by
    /// pa_simple. `playing` means submitted to the sink, not yet necessarily audible.
    pub fn latency_us(&mut self) -> Result<u64, AudioError> {
        self.operation(self.lib.pa_stream_update_timing_info, "timing")?;
        // SAFETY: the module contract; the timing record is the stream's, read before the next
        // libpulse call can change it.
        let timing = unsafe { (self.lib.pa_stream_get_timing_info)(self.stream).as_ref() }
            .ok_or_else(|| self.error("timing"))?;
        if !self.started {
            self.started = timing.playing != 0
                && timing.since_underrun >= 0
                && bytes_to_micros(timing.since_underrun as u64) > timing.sink_usec;
            if !self.started {
                return Ok(bytes_to_micros(self.written.written()));
            }
        }
        let (mut latency, mut negative) = (0, 0);
        // SAFETY: the module contract; two live out-parameters.
        if unsafe { (self.lib.pa_stream_get_latency)(self.stream, &mut latency, &mut negative) } < 0
        {
            return Err(self.error("latency"));
        }
        Ok(if negative != 0 { 0 } else { latency })
    }

    pub fn drain(&mut self) -> Result<(), AudioError> {
        self.operation(self.lib.pa_stream_drain, "drain")
    }
    pub fn flush(&mut self) -> Result<(), AudioError> {
        self.operation(self.lib.pa_stream_flush, "flush")
    }

    /// The writer's last act on a stream it stops while it sounds: the sound fades out over
    /// `fade` (as far as what is written reaches) from just past where the server reads, and
    /// plays to its end. What the server holds is written again at its own index, faded, so the
    /// sink needs no rewind. False when nothing sounds to fade: the caller flushes.
    pub fn fade_out(&mut self, fade: Duration) -> bool {
        // The run is over; these last operations run to their end (each within `TIMEOUT`).
        self.cancel = None;
        self.rewrite_faded(fade).unwrap_or(false)
    }

    fn rewrite_faded(&mut self, fade: Duration) -> Result<bool, AudioError> {
        self.operation(self.lib.pa_stream_update_timing_info, "timing")?;
        // SAFETY: the module contract; the timing record is the stream's, read before the next
        // libpulse call can change it.
        let timing = unsafe { (self.lib.pa_stream_get_timing_info)(self.stream).as_ref() }
            .ok_or_else(|| self.error("timing"))?;
        if timing.playing == 0 || timing.read_index_corrupt != 0 {
            return Ok(false);
        }
        let Ok(read) = u64::try_from(timing.read_index) else {
            return Ok(false);
        };
        // The server reads on while the rewrite travels to it: 10 ms past where it read.
        let ahead = u64::from(RATE * FRAME_BYTES) / 100;
        let from = (read + ahead) / u64::from(FRAME_BYTES) * u64::from(FRAME_BYTES);
        let Some(faded) = self.written.faded_from(from, fade) else {
            return Ok(false);
        };
        let Ok(offset) = i64::try_from(from) else {
            return Ok(false);
        };
        // NULL free callback requests a copy; no Rust slice outlives this call.
        // SAFETY: the module contract; `faded.len()` bytes are read, during the call.
        if unsafe {
            (self.lib.pa_stream_write)(
                self.stream,
                faded.as_ptr().cast(),
                faded.len(),
                None,
                offset,
                SEEK_ABSOLUTE,
            )
        } < 0
        {
            return Err(self.error("write"));
        }
        self.drain()?;
        Ok(true)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: the module contract: each handle freed once, stream and context before the
        // mainloop they run on.
        unsafe {
            if !self.stream.is_null() {
                (self.lib.pa_stream_disconnect)(self.stream);
                (self.lib.pa_stream_unref)(self.stream);
            }
            if !self.context.is_null() {
                (self.lib.pa_context_disconnect)(self.context);
                (self.lib.pa_context_unref)(self.context);
            }
            if !self.mainloop.is_null() {
                (self.lib.pa_mainloop_free)(self.mainloop);
            }
        }
    }
}
