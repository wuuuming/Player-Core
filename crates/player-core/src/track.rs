/// A concrete audio stream index inside the opened container. Non-negative by construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AudioStreamIndex(i32);

impl AudioStreamIndex {
    /// The stream the native decoder opened, validated: a request is an intention and `-1`
    /// means "pick one for me"; what the decoder selected must be a real index.
    #[cfg(windows)]
    pub(crate) fn opened(selected: i32) -> Result<Self, crate::AudioOpenError> {
        Self::new(selected).ok_or_else(|| {
            crate::AudioOpenError::InvalidTrackInfo(format!(
                "native decoder selected invalid audio stream index {selected}"
            ))
        })
    }

    pub const fn new(value: i32) -> Option<Self> {
        if value >= 0 {
            Some(Self(value))
        } else {
            None
        }
    }

    pub const fn get(self) -> i32 {
        self.0
    }
}

impl std::fmt::Display for AudioStreamIndex {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// WHICH stream of the media to decode — independent of the output device selection, which says
/// where to render it. `Default` lets FFmpeg pick the best stream, exactly as before this type
/// existed; an explicit `Stream` must name an enumerated audio stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AudioTrackSelection {
    #[default]
    Default,
    Stream(AudioStreamIndex),
}

pub(crate) fn requested_stream_index(selection: AudioTrackSelection) -> i32 {
    match selection {
        AudioTrackSelection::Default => -1,
        AudioTrackSelection::Stream(stream_index) => stream_index.get(),
    }
}
