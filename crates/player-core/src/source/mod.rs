//! The byte-source half of the native shim: the custom-AVIO adapter that carries a Rust
//! [`MediaByteSource`] into avformat, the URL-backed source FFmpeg's own protocol layer reads, and
//! the error capture both share.
//!
//! Every submodule is a child of this one and names what it uses from its siblings; this module
//! only re-exports what the rest of the crate reads.

mod byte_source;
mod error;
mod ffi;

pub(crate) use self::byte_source::NativeMediaByteSource;
pub use self::byte_source::{UrlAudioSourceFactory, UrlMediaByteSource};
pub(crate) use self::error::{native_is_network_failure, source_is_network_failure, NativeMessage};
pub(crate) use self::ffi::BnByteSource;
#[cfg(feature = "instrument")]
pub use self::ffi::{native_buffer_census, NativeBufferCensus};
