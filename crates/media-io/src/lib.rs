//! The bytes a media session reads, apart from any decoder: a seekable byte source, where it
//! comes from, and the factory an audio session reopens one through. A transport (a file, HTTP,
//! SFTP) implements these and nothing else, so it builds without FFmpeg. A song's source is a file,
//! an address or host PCM (`input`); one factory carries any of them, on every platform.

mod input;
pub use input::{address_label, AudioInput, HttpOpenOptions, PcmAudioSourceFactory, PcmSource};

/// A seekable byte-addressed media input independent of its transport.
///
/// FFmpeg owns the sequential cursor in the custom-AVIO adapter. Implementations only need to
/// satisfy positional reads and report the stable resource size, which keeps filesystem, cache,
/// and future network-backed sources behind the same safe Rust boundary.
pub trait MediaByteSource: Send {
    /// Reads bytes beginning at `offset`, returning `0` only when the offset is at end of input.
    fn read(&mut self, offset: u64, buffer: &mut [u8]) -> std::io::Result<usize>;

    /// Reads within the caller's remaining operation budget. Blocking network transports should
    /// override this and cap their usual read/seek/reconnect budget, never extend it. The default
    /// preserves local/nonblocking implementations; it cannot preempt an arbitrary blocking read.
    fn read_with_timeout(
        &mut self,
        offset: u64,
        buffer: &mut [u8],
        _timeout: std::time::Duration,
    ) -> std::io::Result<usize> {
        self.read(offset, buffer)
    }

    /// Returns the stable total byte size when the transport has discovered it.
    fn size(&self) -> Option<u64>;

    /// The file this source reads, when a decoder needs the operating system's wide path API.
    /// Byte-only transports leave this absent and are read through their callbacks.
    fn local_path(&self) -> Option<&std::path::Path> {
        None
    }

    /// A way to abort a `read` that is blocked on the transport, from another thread. The player
    /// takes it once, before any read, and calls it when the session closes: the blocked read
    /// should return an error promptly, and every later read fail. A transport whose reads cannot
    /// block for long (a local file) has none. Without one, a closed session's reader thread lives
    /// on until its read returns by itself.
    fn canceller(&self) -> Option<std::sync::Arc<dyn Fn() + Send + Sync>> {
        None
    }

    /// Terminal cancellation state, polled by decoder interrupt callbacks even while probing
    /// buffered bytes. Must be nonblocking. Local sources normally leave this false.
    fn is_cancelled(&self) -> bool {
        false
    }

    /// Where the bytes come from, which decides how far ahead the player reads them: a network
    /// source about 30 s (up to 64 MiB), so a stall is ridden out and a seek into what already
    /// arrived costs no request; a local one about 2 s, since the disk refills it at once and
    /// whatever is queued is resident memory. A transport that does not say is read as a network.
    fn locality(&self) -> SourceLocality {
        SourceLocality::Network
    }
}

/// See [`MediaByteSource::locality`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceLocality {
    /// A file on a local volume: reads return in milliseconds and never stall.
    Local,
    /// Anything that can stall: HTTP, SFTP, a share on another machine.
    Network,
}

impl SourceLocality {
    /// The locality of what `avformat_open_input` opens for `url`: a URL with a protocol other
    /// than `file` is a network source; a path (or `file:` URL) is local unless it lies on a
    /// network share. A single letter before the colon is a drive, not a protocol.
    pub fn of_url(url: &str) -> Self {
        let scheme = url
            .split_once(':')
            .map(|(scheme, _)| scheme)
            .filter(|scheme| {
                scheme.len() > 1
                    && scheme
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            });
        match scheme {
            Some(scheme) if !scheme.eq_ignore_ascii_case("file") => Self::Network,
            Some(_) => Self::of_path(std::path::Path::new(&url[5..])),
            None => Self::of_path(std::path::Path::new(url)),
        }
    }

    /// A path on a network share — `\\server\share\…`, or a drive letter mapped to one — is a
    /// network source: it can stall like one. Anything else is local.
    pub fn of_path(path: &std::path::Path) -> Self {
        if path_is_on_a_share(path) {
            Self::Network
        } else {
            Self::Local
        }
    }
}

#[cfg(windows)]
fn path_is_on_a_share(path: &std::path::Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Prefix};

    #[link(name = "kernel32")]
    extern "system" {
        fn GetDriveTypeW(root: *const u16) -> u32;
    }
    const DRIVE_REMOTE: u32 = 4;
    match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::UNC(..) | Prefix::VerbatimUNC(..) => true,
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                let root: Vec<u16> = std::ffi::OsString::from(format!("{}:\\", letter as char))
                    .encode_wide()
                    .chain(Some(0))
                    .collect();
                // SAFETY: `root` is a NUL-terminated wide string that outlives the call.
                unsafe { GetDriveTypeW(root.as_ptr()) == DRIVE_REMOTE }
            }
            Prefix::DeviceNS(_) | Prefix::Verbatim(_) => false,
        },
        _ => false,
    }
}

#[cfg(not(windows))]
fn path_is_on_a_share(_path: &std::path::Path) -> bool {
    false
}

/// Local-file implementation used by the deterministic custom-AVIO gate and offline playback.
pub struct FileMediaByteSource {
    file: std::fs::File,
    path: std::path::PathBuf,
    size: u64,
    locality: SourceLocality,
}

impl FileMediaByteSource {
    pub fn open(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        let file = std::fs::File::open(path.as_ref())?;
        let size = file.metadata()?.len();
        // A relative path is judged by where it resolves; one that does not resolve is judged as
        // given (a relative path without a drive is on the current drive, which is local).
        let absolute = std::path::absolute(path.as_ref());
        let locality = SourceLocality::of_path(absolute.as_deref().unwrap_or(path.as_ref()));
        let path = absolute.unwrap_or_else(|_| path.as_ref().to_path_buf());
        Ok(Self {
            file,
            path,
            size,
            locality,
        })
    }
}

impl MediaByteSource for FileMediaByteSource {
    fn local_path(&self) -> Option<&std::path::Path> {
        Some(&self.path)
    }

    fn read(&mut self, offset: u64, buffer: &mut [u8]) -> std::io::Result<usize> {
        use std::io::{Read, Seek};

        self.file.seek(std::io::SeekFrom::Start(offset))?;
        self.file.read(buffer)
    }

    fn size(&self) -> Option<u64> {
        Some(self.size)
    }

    fn locality(&self) -> SourceLocality {
        self.locality
    }
}

/// Which of the three things an audio session depends on refused, or `Cancelled` when none did.
/// This dependency domain is not a retry policy: transport auth and bad paths are Open failures
/// too. `Open`: the transport could not produce a source — no such file, refused connection,
/// denied auth. `Decode`: bytes arrived and the decoder refused them. `Device`: the output device
/// refused. `Cancelled`: the work was superseded before it finished; nothing refused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum AudioSourceKind {
    #[default]
    Open = 0,
    Decode = 1,
    Device = 2,
    Cancelled = 3,
}

/// A typed audio-source failure. The message is for a human; kind and optional cause are data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioSourceError {
    pub kind: AudioSourceKind,
    pub message: String,
    /// Optional machine-readable provenance. Absent for untyped third-party failures.
    pub cause: Option<AudioSourceCause>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioSourceCause {
    Io(std::io::ErrorKind),
    /// FFmpeg AVERROR status, not a player-core operation status.
    Native(i32),
}

impl AudioSourceError {
    pub fn open(message: impl Into<String>) -> Self {
        Self {
            kind: AudioSourceKind::Open,
            message: message.into(),
            cause: None,
        }
    }

    /// The work was superseded: nothing refused.
    pub fn cancelled(message: impl Into<String>) -> Self {
        Self {
            kind: AudioSourceKind::Cancelled,
            message: message.into(),
            cause: None,
        }
    }

    pub fn with_cause(mut self, cause: AudioSourceCause) -> Self {
        self.cause = Some(cause);
        self
    }
}

impl std::fmt::Display for AudioSourceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.kind {
            AudioSourceKind::Open => "audio source",
            AudioSourceKind::Decode => "audio decoder",
            AudioSourceKind::Device => "audio device",
            AudioSourceKind::Cancelled => "cancelled",
        };
        write!(formatter, "{kind}: {}", self.message)
    }
}

impl std::error::Error for AudioSourceError {}

/// The audio transport seam, in the shape the B2 ruling fixed: a session holds a FACTORY, never an
/// already-open source, so a reopen — a device-lost rebind, a part switch — is `open()` called
/// again rather than a path the session was supposed to have forgotten being re-derived. `&self`
/// and `Send + Sync` are the binding parts: a factory is shareable, reusable, and carries
/// everything an open needs. The opened value is a [`MediaByteSource`], because here the
/// byte-addressed transport IS the audio source; a second trait for it would be a duplicate owner.
///
/// A player that opens its input itself (Linux's ffmpeg child) takes [`Self::input`] instead: a
/// file, an address or host PCM. A factory without one (a byte-only transport such as SFTP) plays
/// only where the player reads bytes.
pub trait AudioSourceFactory: Send + Sync {
    /// Opens a fresh, independent source. Every call produces its own cursor, because every
    /// decoder of a song (the one playing, the next one opened ahead, a rebind's) holds one and
    /// none may move another's.
    fn open(&self) -> Result<Box<dyn MediaByteSource>, AudioSourceError>;

    /// The source as an input a player opens itself; `None` for a byte-only transport.
    fn input(&self) -> Option<AudioInput> {
        None
    }

    /// Nonblocking, terminal cancellation of this factory's in-flight opens and live sources.
    /// Later opens must fail. Cancelling an individual source must not cancel its siblings or
    /// this factory. Local factories whose I/O cannot be interrupted may leave this a no-op.
    fn cancel(&self) {}

    /// [`Self::cancel`] as a flag a player that opens its input itself watches (it stops the
    /// child reading it). `None` when the factory has no such flag.
    fn cancellation(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        None
    }

    /// Stable identity for logs and errors. Never carries credential material.
    fn label(&self) -> String;
}

/// The local-file factory: reopening is opening the same path again — what a local file makes
/// cheap and what no other transport is allowed to assume.
/// A source factory's cancel flag: `cancel` sets it, `flag` hands it to the readers and the
/// player, and `refuse` stops an open of a cancelled factory (as `Cancelled`).
#[derive(Clone, Debug, Default)]
pub struct CancelFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl CancelFlag {
    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
    pub fn is_set(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
    /// The flag itself (`AudioSourceFactory::cancellation`).
    pub fn flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.0.clone()
    }
    /// `Err(Cancelled)` naming `label` once the flag is set.
    pub fn refuse(&self, label: impl std::fmt::Display) -> Result<(), AudioSourceError> {
        if self.is_set() {
            return Err(AudioSourceError::cancelled(format!("{label}: cancelled")));
        }
        Ok(())
    }
}

pub struct PathAudioSourceFactory {
    path: std::path::PathBuf,
    cancelled: CancelFlag,
}

impl PathAudioSourceFactory {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            path: path.into(),
            cancelled: Default::default(),
        }
    }
}

impl AudioSourceFactory for PathAudioSourceFactory {
    fn open(&self) -> Result<Box<dyn MediaByteSource>, AudioSourceError> {
        self.cancelled.refuse(self.path.display())?;
        FileMediaByteSource::open(&self.path)
            .map(|source| Box::new(source) as Box<dyn MediaByteSource>)
            .map_err(|error| {
                AudioSourceError::open(format!("{}: {error}", self.path.display()))
                    .with_cause(AudioSourceCause::Io(error.kind()))
            })
    }

    fn input(&self) -> Option<AudioInput> {
        Some(AudioInput::Path(self.path.clone()))
    }

    fn cancel(&self) {
        self.cancelled.cancel();
    }

    fn cancellation(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        Some(self.cancelled.flag())
    }

    fn label(&self) -> String {
        self.path.display().to_string()
    }
}
