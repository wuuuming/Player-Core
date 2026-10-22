//! Where a run's sound comes from: the factory's input (`media_io::AudioInput`), which ffmpeg
//! opens itself. The address factory is this platform's: the address goes to ffmpeg as it is.

use crate::{AudioInput, AudioSourceError, AudioSourceFactory, HttpOpenOptions, MediaByteSource};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// `path` is an HTTP(S) address rather than a file.
pub(super) fn is_http_input(path: &Path) -> bool {
    path.to_str()
        .and_then(|s| s.split_once("://"))
        .is_some_and(|(scheme, _)| {
            scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
        })
}

/// An address, played by ffmpeg's own HTTP client with the options' headers and agent. Its bytes
/// are not read here (`open` refuses): ffmpeg takes the address itself.
pub struct UrlAudioSourceFactory {
    url: String,
    http: HttpOpenOptions,
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
        Err(AudioSourceError::open(format!(
            "{}: an address is read by ffmpeg here, not as bytes",
            self.label()
        )))
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

/// What ffmpeg reads for `input`: the file or the address (`None` for host PCM, which is fed on
/// its stdin).
pub(super) fn ffmpeg_input(input: &AudioInput) -> Option<&std::ffi::OsStr> {
    match input {
        AudioInput::Path(p) => Some(p.as_os_str()),
        AudioInput::Url(u, _) => Some(std::ffi::OsStr::new(u.as_str())),
        AudioInput::Pcm(_) => None,
    }
}

/// The input reads from the network (ffmpeg's transport timeouts and errors apply).
pub(super) fn is_network(input: &AudioInput) -> bool {
    match input {
        AudioInput::Url(..) => true,
        AudioInput::Path(p) => is_http_input(p),
        AudioInput::Pcm(_) => false,
    }
}
