#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AudioErrorKind {
    Cancelled,
    Source,
    Decoder,
    Output,
    Transport,
}

#[derive(Clone, Debug)]
pub struct AudioError {
    kind: AudioErrorKind,
    message: String,
}

impl From<crate::source::NativeError> for AudioError {
    fn from(error: crate::source::NativeError) -> Self {
        Self::new(
            if error.is_network_failure() {
                AudioErrorKind::Transport
            } else {
                AudioErrorKind::Decoder
            },
            error.to_string(),
        )
    }
}

impl AudioError {
    pub(super) fn new(kind: AudioErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Only confirmed transport failures may trigger the engine's network recovery.
    /// An HTTP input alone says nothing about decoder, authentication or device failures.
    pub fn is_network_failure(&self) -> bool {
        self.kind == AudioErrorKind::Transport
    }

    /// The sound output refused (no sound server, a sink that would not start): opening again
    /// once there is one may succeed; the song itself is not at fault.
    pub fn is_device_failure(&self) -> bool {
        self.kind == AudioErrorKind::Output
    }
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
