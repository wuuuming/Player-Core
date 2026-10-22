//! Where a song's sound comes from, in the three ways a host hands it over: a file, an address,
//! or PCM the host makes itself (a decode plugin). One [`crate::AudioSourceFactory`] carries any
//! of them, and each platform's player takes the part it plays: Windows reads every source as
//! bytes ([`crate::AudioSourceFactory::open`]), Linux gives ffmpeg the input itself
//! ([`crate::AudioSourceFactory::input`]).

use crate::{AudioSourceError, AudioSourceFactory, MediaByteSource, SourceLocality};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// A source as an input a player opens itself, rather than as bytes.
#[derive(Clone)]
pub enum AudioInput {
    /// A file on this machine.
    Path(std::path::PathBuf),
    /// An HTTP(S) address, verbatim (a signed query string included), and what its requests carry.
    Url(String, HttpOpenOptions),
    /// Seekable PCM the host makes.
    Pcm(Arc<dyn PcmSource>),
}

impl AudioInput {
    /// A name for it in a sound server's list or a log: the file's name, the address's last part
    /// without its query, or the PCM's label. Never carries a query string (it may be signed).
    pub fn name(&self) -> String {
        match self {
            AudioInput::Path(p) => p
                .file_name()
                .map_or(String::new(), |n| n.to_string_lossy().into_owned()),
            AudioInput::Url(u, _) => {
                let path = u.split(['?', '#']).next().unwrap_or("");
                path.rsplit('/').next().unwrap_or("").to_owned()
            }
            AudioInput::Pcm(s) => s.label(),
        }
    }
}

/// An address as logs and errors name it: scheme, host and path, never its userinfo, signed
/// query or fragment.
pub fn address_label(url: &str) -> String {
    let base = url.split(['?', '#']).next().unwrap_or_default();
    let Some((scheme, rest)) = base.split_once("://") else {
        return base.to_owned();
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let host = authority.rsplit('@').next().unwrap_or_default();
    format!("{scheme}://{host}{}", &rest[authority_end..])
}

/// The request options one open of an address carries, as FFmpeg's HTTP protocol takes them.
///
/// It is not a credential carrier: a cookie belongs to the signed HTTP client that resolved the
/// address, never to the demuxer that plays it, and nothing reads an environment variable to fill
/// it: the app decides. [`Default`] sends no custom headers or agent; the players' own finite
/// timeouts and reconnect limits apply either way.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HttpOpenOptions {
    /// Request headers in wire order, assembled into one block by [`Self::header_block`].
    pub headers: Vec<(String, String)>,
    /// Replaces FFmpeg's own `Lavf/…` agent. `None` leaves it alone.
    pub user_agent: Option<String>,
}

impl HttpOpenOptions {
    /// No custom headers or agent, as a const so a caller can borrow one without allocating.
    pub const fn none() -> Self {
        Self {
            headers: Vec::new(),
            user_agent: None,
        }
    }

    /// The exact string FFmpeg's `headers` option takes: every `Name: value` CRLF-terminated,
    /// including the last. `None` when there are no headers, so an empty list sets no key at all.
    pub fn header_block(&self) -> Option<String> {
        if self.headers.is_empty() {
            return None;
        }
        let mut block = String::new();
        for (name, value) in &self.headers {
            block.push_str(name);
            block.push_str(": ");
            block.push_str(value);
            block.push_str("\r\n");
        }
        Some(block)
    }
}

/// Seekable interleaved signed 16-bit little-endian PCM, independent of who makes it.
///
/// Each reader (a player's binding, a decoder started ahead) takes a fresh token: cancelling a
/// retired reader must not cancel a later seek or resume on the same source.
pub trait PcmSource: Send + Sync {
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
    /// It has a format to play: a sample rate and channels (neither 0).
    fn has_format(&self) -> bool {
        self.sample_rate() != 0 && self.channels() != 0
    }
    /// Its length, when the host knows it.
    fn duration(&self) -> Option<Duration>;
    /// Total frames, when the host knows them: what a reader that needs a size (a WAV header)
    /// says. Without it the stream is read until a read comes back empty.
    fn frames(&self) -> Option<u64> {
        self.duration()
            .map(|d| (d.as_secs_f64() * f64::from(self.sample_rate())).round() as u64)
    }
    /// A token for one reader, set when that reader is cancelled.
    fn reader_token(&self) -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }
    /// Up to `frames` frames from frame `at`; an empty block only at the end. Observe
    /// `cancelled` promptly while blocked.
    fn read_cancellable(
        &self,
        at: u64,
        frames: u64,
        cancelled: &AtomicBool,
    ) -> Result<Vec<u8>, String>;
    /// Bytes per frame.
    fn frame_bytes(&self) -> u64 {
        u64::from(self.channels()) * 2
    }
    /// Stable identity for logs and errors.
    fn label(&self) -> String;
}

/// The factory over host PCM: as bytes it is a WAV file (a 44-byte header, then the PCM), so a
/// player that reads bytes demuxes, seeks and resamples it as it would a file on disk; as an input
/// it is the PCM itself.
pub struct PcmAudioSourceFactory {
    source: Arc<dyn PcmSource>,
    cancelled: crate::CancelFlag,
}

impl PcmAudioSourceFactory {
    pub fn new(source: Arc<dyn PcmSource>) -> Self {
        Self {
            source,
            cancelled: Default::default(),
        }
    }
}

impl AudioSourceFactory for PcmAudioSourceFactory {
    fn open(&self) -> Result<Box<dyn MediaByteSource>, AudioSourceError> {
        self.cancelled.refuse(self.label())?;
        let (rate, channels) = (self.source.sample_rate(), self.source.channels());
        if !self.source.has_format() {
            return Err(AudioSourceError::open(format!(
                "{}: no sample rate or channels",
                self.label()
            )));
        }
        Ok(Box::new(PcmWav {
            source: self.source.clone(),
            header: wav_header(rate, channels, self.source.frames()),
            block: Block::default(),
            cancelled: self.cancelled.flag(),
        }))
    }
    fn input(&self) -> Option<AudioInput> {
        Some(AudioInput::Pcm(self.source.clone()))
    }
    fn cancel(&self) {
        self.cancelled.cancel();
    }
    fn cancellation(&self) -> Option<Arc<AtomicBool>> {
        Some(self.cancelled.flag())
    }
    fn label(&self) -> String {
        self.source.label()
    }
}

/// Frames per read of the host's PCM (~100 ms at 48 kHz).
const BLOCK: u64 = 4800;

/// The 44-byte header of a 16-bit PCM WAV; an unknown length is the largest a WAV can say.
fn wav_header(rate: u32, channels: u16, frames: Option<u64>) -> [u8; 44] {
    let fb = u32::from(channels) * 2;
    let data = frames.map_or(u32::MAX - 36, |f| {
        (f * u64::from(fb)).min(u64::from(u32::MAX - 36)) as u32
    });
    let mut h = [0u8; 44];
    let mut put = |at: usize, v: &[u8]| h[at..at + v.len()].copy_from_slice(v);
    put(0, b"RIFF");
    put(4, &(36 + data).to_le_bytes());
    put(8, b"WAVEfmt ");
    put(16, &16u32.to_le_bytes());
    put(20, &1u16.to_le_bytes());
    put(22, &channels.to_le_bytes());
    put(24, &rate.to_le_bytes());
    put(28, &(rate * fb).to_le_bytes());
    put(32, &(fb as u16).to_le_bytes());
    put(34, &16u16.to_le_bytes());
    put(36, b"data");
    put(40, &data.to_le_bytes());
    h
}

/// The block of PCM read last (reads go ahead `BLOCK` frames at a time).
#[derive(Default)]
struct Block {
    start: u64,
    bytes: Vec<u8>,
}

/// Host PCM read as a WAV file.
struct PcmWav {
    source: Arc<dyn PcmSource>,
    header: [u8; 44],
    block: Block,
    /// The factory's: a cancelled factory's sources stop reading.
    cancelled: Arc<AtomicBool>,
}

impl MediaByteSource for PcmWav {
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if offset < 44 {
            let n = (44 - offset as usize).min(buf.len());
            buf[..n].copy_from_slice(&self.header[offset as usize..offset as usize + n]);
            return Ok(n);
        }
        let fb = self.source.frame_bytes();
        let pcm = offset - 44;
        let frame = pcm / fb;
        let b = &mut self.block;
        if b.bytes.is_empty() || frame < b.start || frame >= b.start + BLOCK {
            b.start = frame / BLOCK * BLOCK;
            let mut bytes = self
                .source
                .read_cancellable(b.start, BLOCK, &self.cancelled)
                .map_err(std::io::Error::other)?;
            // Whole frames only.
            bytes.truncate((bytes.len() as u64 / fb * fb) as usize);
            b.bytes = bytes;
        }
        let at = (pcm - b.start * fb) as usize;
        if at >= b.bytes.len() {
            return Ok(0);
        }
        let n = (b.bytes.len() - at).min(buf.len());
        buf[..n].copy_from_slice(&b.bytes[at..at + n]);
        Ok(n)
    }
    fn size(&self) -> Option<u64> {
        self.source
            .frames()
            .map(|f| 44 + f * self.source.frame_bytes())
    }
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    fn locality(&self) -> SourceLocality {
        // Made on demand like a disk read: no long read-ahead.
        SourceLocality::Local
    }
}
