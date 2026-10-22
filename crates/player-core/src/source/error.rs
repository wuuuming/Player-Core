//! Native status and message capture for the byte-source half of the shim.

use super::ffi::{bn_error_is_network_failure, bn_io_error_code, ERROR_CAPACITY};
use crate::AudioSourceError;
use std::ffi::{c_char, CStr};
use std::mem::MaybeUninit;

pub(crate) fn native_is_network_failure(code: i32) -> bool {
    // SAFETY: a plain value; the shim only compares it.
    unsafe { bn_error_is_network_failure(code) != 0 }
}

pub(crate) fn io_error_code(kind: std::io::ErrorKind) -> i32 {
    use std::io::ErrorKind::*;
    let kind = match kind {
        TimedOut => 1,
        ConnectionReset => 2,
        ConnectionAborted => 3,
        ConnectionRefused => 4,
        NotConnected => 5,
        BrokenPipe => 6,
        NetworkUnreachable => 7,
        HostUnreachable => 8,
        PermissionDenied => 9,
        InvalidInput => 10,
        InvalidData => 11,
        Interrupted => 12,
        _ => 0,
    };
    // SAFETY: a plain value; the shim only maps it.
    unsafe { bn_io_error_code(kind) }
}

pub(crate) fn source_is_network_failure(error: &AudioSourceError) -> bool {
    if error.kind != crate::AudioSourceKind::Open {
        return false;
    }
    match error.cause {
        Some(crate::AudioSourceCause::Native(code)) => native_is_network_failure(code),
        Some(crate::AudioSourceCause::Io(kind)) => {
            // Interrupted alone does not prove a deadline; native AVERROR_EXIT does.
            kind != std::io::ErrorKind::Interrupted
                && native_is_network_failure(io_error_code(kind))
        }
        None => false,
    }
}

/// An io::Error payload, preserving the native code across the Rust byte-source seam.
#[derive(Debug)]
pub(super) struct NativeReadError {
    pub(super) code: i32,
    pub(super) message: String,
}

impl std::fmt::Display for NativeReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for NativeReadError {}

/// The buffer a shim call writes its failure message into, NUL-terminated.
pub(crate) struct NativeMessage {
    pub(super) bytes: MaybeUninit<[c_char; ERROR_CAPACITY]>,
}

impl NativeMessage {
    pub(crate) fn new() -> Self {
        let mut bytes = MaybeUninit::<[c_char; ERROR_CAPACITY]>::uninit();
        // The native ABI writes and terminates every error result. Initializing only the sentinel
        // preserves an empty fallback if that contract is ever violated without clearing 512 bytes
        // before every successful decode/render call.
        // SAFETY: the first byte of the buffer just made, written before anything reads it.
        unsafe { bytes.as_mut_ptr().cast::<c_char>().write(0) };
        Self { bytes }
    }

    pub(crate) fn as_mut_ptr(&mut self) -> *mut c_char {
        self.bytes.as_mut_ptr().cast()
    }

    pub(crate) fn capacity(&self) -> usize {
        ERROR_CAPACITY
    }

    pub(crate) fn message(&self) -> String {
        // SAFETY: the buffer starts NUL-terminated (`new`) and the shim writes only terminated
        // strings within its capacity, so the read stops inside it.
        unsafe { CStr::from_ptr(self.bytes.as_ptr().cast()) }
            .to_string_lossy()
            .into_owned()
    }
}
