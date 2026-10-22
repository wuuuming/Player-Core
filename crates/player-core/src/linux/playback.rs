//! Decoder and PCM feeder lifetime, separate from the player's persistent state, and the
//! processing between FFmpeg's pipe and the stream (the chains Windows runs).
use super::output::{capturing, Capture, ROUTE, ROUTE_VERSION};
use super::player::{Following, Run, Terminal};
use super::source::{ffmpeg_input, is_network};
use super::types::{AudioError, AudioErrorKind};
use super::{process, pulse};
use crate::dsp::{handover_at, Crossfade, DspChain, UserGain};
use crate::dsp::{one_pole, one_pole_coefficient, USER_GAIN_SMOOTHING};
use crate::{AudioInput, PcmSource};
use std::io::{Read, Write};
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

struct Decoder {
    child: process::OwnedChild,
    /// The PCM feeder's own token: stopped when this decoder finishes, and with the run.
    feed_stop: Arc<AtomicBool>,
    feeder: Option<std::thread::JoinHandle<()>>,
    feed_error: Arc<Mutex<Option<AudioError>>>,
    stderr: Option<process::StderrTail>,
}

impl Decoder {
    fn finish_feeder(&mut self) {
        self.feed_stop.store(true, Ordering::Release);
        // read() uses the feeder token, pipe writes are nonblocking. No orphan plugin readers.
        if let Some(feeder) = self.feeder.take() {
            if feeder.join().is_err() {
                let mut error = self
                    .feed_error
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                error.get_or_insert_with(|| {
                    AudioError::new(AudioErrorKind::Source, "PCM reader thread stopped")
                });
            }
        }
    }

    fn finish(&mut self) -> (Option<AudioError>, String) {
        self.child.terminate();
        self.finish_feeder();
        let log = self
            .stderr
            .take()
            .map_or_else(String::new, |mut stderr| stderr.finish());
        (
            self.feed_error
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
            log,
        )
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.finish();
    }
}

fn feed(
    source: Arc<dyn PcmSource>,
    mut stdin: std::process::ChildStdin,
    (run, feed_stop): (Arc<Run>, Arc<AtomicBool>),
    error: Arc<Mutex<Option<AudioError>>>,
    from: Duration,
) {
    let mut at = (from.as_secs_f64() * f64::from(source.sample_rate())) as u64;
    let cancelled = || run.cancelled() || feed_stop.load(Ordering::Acquire);
    while !cancelled() {
        let bytes = match source.read_cancellable(at, 4800, &feed_stop) {
            Ok(bytes) if bytes.is_empty() => return,
            Ok(bytes) => bytes,
            Err(message) => {
                if !cancelled() {
                    // A host source returns text, not a typed network status. Preserve it,
                    // but never guess that arbitrary source/decode failures are transport.
                    *error.lock().unwrap_or_else(PoisonError::into_inner) =
                        Some(AudioError::new(AudioErrorKind::Source, message));
                }
                return;
            }
        };
        at += bytes.len() as u64 / source.frame_bytes();
        let mut written = 0;
        while written < bytes.len() && !cancelled() {
            match stdin.write(&bytes[written..]) {
                Ok(0) => return,
                Ok(n) => written += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                // Until ffmpeg takes more (or exits: a hung-up pipe is ready too). The deadline
                // only bounds how late a stop asked without killing ffmpeg is seen.
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    let until = Instant::now() + process::IO_TIMEOUT;
                    if process::ready_one(&stdin, libc::POLLOUT, Some(until)).is_err() {
                        return;
                    }
                }
                // ffmpeg may have reached a span's -t limit. Its exit status decides success.
                Err(_) => return,
            }
        }
    }
}

/// The ffmpeg CLI exposes no typed AVERROR. Match only explicit transport diagnostics in
/// its C-locale stderr, never "HTTP input => network error" or generic "Input/output error".
/// Ambiguous failures stay decoder errors; auth and invalid media always veto retries.
fn transport_log(log: &str) -> bool {
    let log = log.to_ascii_lowercase();
    !non_transport_log(&log)
        && [
            "connection reset by peer",
            "connection timed out",
            "connection refused",
            "network is unreachable",
            "network is down",
            "no route to host",
            "temporary failure in name resolution",
            "failed to resolve hostname",
            "stream ends prematurely",
        ]
        .iter()
        .any(|text| log.contains(text))
}

fn non_transport_log(log: &str) -> bool {
    [
        "401 unauthorized",
        "403 forbidden",
        "404 not found",
        "http error 4",
        "server returned 4",
        "invalid data found when processing input",
        "error while decoding",
        "invalid pcm packet",
        "error initializing output stream",
        "error initializing output file",
        "error opening output",
        "decoder not found",
        "unknown decoder",
        "permission denied",
        "certificate verification failed",
    ]
    .iter()
    .any(|text| log.contains(text))
}

pub(super) fn decoder_error(
    log: String,
    http: bool,
    transport_watchdog: bool,
    fallback: String,
) -> AudioError {
    // A generic stdout/PCM stall is not a transport watchdog: decoding/probing can stall
    // too. Only an independently confirmed transport timeout may use the no-log fallback.
    // Nonempty, unrecognized diagnostics never become transport merely because of a timer.
    let network = http && (transport_log(&log) || (transport_watchdog && log.trim().is_empty()));
    let message = if !log.trim().is_empty() {
        log
    } else if network && transport_watchdog {
        "network stream lost".into()
    } else {
        fallback
    };
    AudioError::new(
        if network {
            AudioErrorKind::Transport
        } else {
            AudioErrorKind::Decoder
        },
        message,
    )
}

/// The stream's rate and channels, as the chain takes them.
pub const CHAIN_RATE: NonZeroU32 = match NonZeroU32::new(pulse::RATE) {
    Some(rate) => rate,
    None => panic!("the stream has a rate"),
};
pub const CHAIN_CHANNELS: NonZeroUsize = match NonZeroUsize::new(pulse::CHANNELS as usize) {
    Some(channels) => channels,
    None => panic!("the stream has channels"),
};
/// One frame of FFmpeg's output and the stream's input.
pub const FRAME_BYTES: usize = pulse::FRAME_BYTES as usize;

// How the sound of a run stopped while it sounds ends (`Writer::close`): a pause or a seek
// with a declick; a song the player is disposed of (its host leaves it for another) with a
// short fade. The Windows backend's provisional policy's `ramp` and `leave` are the same.
use crate::dsp::{DECLICK, LEAVE};

/// What a run does to the sound between FFmpeg's pipe and the stream: the song's own chain, the
/// stream's (which every song of the run goes through, one after another), then the user's
/// volume, moved to frame by frame over [`USER_GAIN_SMOOTHING`] as on Windows. Where a song
/// overlaps the end of the one before (a crossfade), the two are summed before the stream's chain,
/// as on Windows.
pub struct Processing {
    /// The playing song's chain: the player's, kept across its runs.
    song: RunChain<Arc<Mutex<DspChain>>>,
    /// The stream's chain: the run's own.
    stream: Option<RunChain<DspChain>>,
    volume: Arc<UserGain>,
    /// The volume the samples have; the target itself at the run's first block.
    level: Option<f32>,
    smoothing: f32,
    samples: Vec<f32>,
    bytes: Vec<u8>,
    /// The crossfade under way: what `block` is given is the coming song.
    fade: Option<Fading>,
}

/// A chain as one run uses it: reset before the first block the run gives it, and only then
/// holding anything of the run's to flush or renew. A chain swapped in starts unreset.
struct RunChain<C> {
    chain: C,
    started: bool,
}

impl<C> RunChain<C> {
    fn new(chain: C) -> Self {
        Self {
            chain,
            started: false,
        }
    }

    /// `chain` from now, not yet reset; the one before given back, with whether it had been.
    fn replace(&mut self, chain: C) -> (C, bool) {
        (
            std::mem::replace(&mut self.chain, chain),
            std::mem::replace(&mut self.started, false),
        )
    }
}

impl RunChain<DspChain> {
    fn process(&mut self, samples: &mut Vec<f32>) {
        reset_first(&mut self.chain, &mut self.started);
        self.chain.process(samples);
    }

    /// What the chain holds of the run's (nothing before its first block), flushed out.
    fn flush(&mut self) -> Vec<f32> {
        if self.started {
            self.chain.flush()
        } else {
            Vec::new()
        }
    }

    /// The chain renewed if its effects changed; what it held of the run's then flushed out.
    fn renew(&mut self) -> Vec<f32> {
        if self.started {
            self.chain.renew()
        } else {
            Vec::new()
        }
    }
}

/// A song's chain is shared with the player: a cancelled run leaves it alone (checked with it
/// held), since its writer may still be here when the next run has started and the chain is
/// that run's now.
impl RunChain<Arc<Mutex<DspChain>>> {
    fn process(&mut self, samples: &mut Vec<f32>, cancelled: impl Fn() -> bool) {
        let mut chain = self.chain.lock().unwrap_or_else(PoisonError::into_inner);
        if cancelled() {
            samples.clear();
            return;
        }
        reset_first(&mut chain, &mut self.started);
        chain.process(samples);
    }

    fn flush(&mut self, cancelled: impl Fn() -> bool) -> Vec<f32> {
        let mut chain = self.chain.lock().unwrap_or_else(PoisonError::into_inner);
        if self.started && !cancelled() {
            chain.flush()
        } else {
            Vec::new()
        }
    }
}

fn reset_first(chain: &mut DspChain, started: &mut bool) {
    if !*started {
        chain.reset();
        *started = true;
    }
}

/// A crossfade under way: the going song's chain, and its frames through it, not yet summed.
struct Fading {
    mix: Crossfade,
    chain: Arc<Mutex<DspChain>>,
    frames: Vec<f32>,
    /// The going song has given all it has: the rest of the span is the coming one alone.
    ended: bool,
}

impl Processing {
    pub fn new(
        song: Arc<Mutex<DspChain>>,
        stream: Option<DspChain>,
        volume: Arc<UserGain>,
    ) -> Self {
        Self {
            song: RunChain::new(song),
            stream: stream.map(RunChain::new),
            volume,
            level: None,
            smoothing: one_pole_coefficient(USER_GAIN_SMOOTHING, CHAIN_RATE),
            samples: Vec::new(),
            bytes: Vec::new(),
            fade: None,
        }
    }

    /// Whole frames of FFmpeg's little-endian `f32` through the chains and the volume: the bytes
    /// for the stream, fewer than given while the chains fill their latency. A cancelled run gets
    /// nothing back and leaves the song's chain alone: its writer may still be here when the next
    /// run has started, and the chain is that run's now.
    pub fn block(&mut self, pcm: &[u8], cancelled: impl Fn() -> bool) -> &[u8] {
        self.samples.clear();
        self.samples.extend(pulse::samples(pcm));
        self.song.process(&mut self.samples, cancelled);
        if let Some(fade) = &mut self.fade {
            let channels = CHAIN_CHANNELS.get();
            let summed = (self.samples.len() / channels)
                .min(fade.mix.left())
                .min(fade.frames.len() / channels);
            fade.mix.mix(
                &mut self.samples,
                &fade.frames[..summed * channels],
                channels,
            );
            fade.frames.drain(..summed * channels);
            if fade.mix.left() == 0 {
                self.fade = None;
            }
        }
        self.through_stream();
        self.finish()
    }

    /// Whole frames of the going song in a crossfade, through its own chain, to be summed with the
    /// coming one's next.
    pub(super) fn going(&mut self, pcm: &[u8], cancelled: impl Fn() -> bool) {
        let Some(fade) = &mut self.fade else {
            return;
        };
        let start = fade.frames.len();
        fade.frames.extend(pulse::samples(pcm));
        let mut chain = fade.chain.lock().unwrap_or_else(PoisonError::into_inner);
        if cancelled() {
            fade.frames.truncate(start);
            return;
        }
        let mut block = fade.frames.split_off(start);
        chain.process(&mut block);
        fade.frames.extend(block);
    }

    /// The going song has given all it has: what its chain still holds is the last of it, and the
    /// coming one finishes the span alone.
    pub(super) fn going_ended(&mut self, cancelled: impl Fn() -> bool) {
        let Some(fade) = &mut self.fade else {
            return;
        };
        let mut chain = fade.chain.lock().unwrap_or_else(PoisonError::into_inner);
        if !cancelled() {
            fade.frames.extend(chain.flush());
        }
        fade.ended = true;
    }

    /// Frames of the going song still wanted before `frames` more of the coming one can be summed:
    /// none outside a crossfade, or once the going song has ended.
    pub(super) fn going_wanted(&self, frames: usize) -> usize {
        match &self.fade {
            Some(fade) if !fade.ended => frames
                .min(fade.mix.left())
                .saturating_sub(fade.frames.len() / CHAIN_CHANNELS.get()),
            _ => 0,
        }
    }

    /// Whether a crossfade is under way.
    pub(super) fn fading(&self) -> bool {
        self.fade.is_some()
    }

    /// The song's end: the frames still inside its own chain, on through the stream's. A
    /// crossfade it was coming in under ends with it: it was shorter than the span.
    pub fn song_end(&mut self, cancelled: impl Fn() -> bool) -> &[u8] {
        self.fade = None;
        self.samples = self.song.flush(cancelled);
        self.through_stream();
        self.finish()
    }

    /// The sound's end, after the last song's: the frames still inside the stream's chain.
    pub fn sound_end(&mut self) -> &[u8] {
        self.samples = self
            .stream
            .as_mut()
            .map_or_else(Vec::new, |stream| stream.flush());
        self.finish()
    }

    /// The song after this one in the same run, its own chain `song`, overlapping this one's last
    /// `overlap` frames (a crossfade; none follows gaplessly): the stream's chain goes on into
    /// it, renewed first if its effects changed (the song before's last frames then flushed out
    /// of it, returned here).
    pub(super) fn follow(&mut self, song: Arc<Mutex<DspChain>>, overlap: usize) -> &[u8] {
        let (going, started) = self.song.replace(song);
        self.fade = (overlap > 0 && started).then(|| Fading {
            mix: Crossfade::new(overlap),
            chain: going,
            frames: Vec::new(),
            ended: false,
        });
        self.samples = self
            .stream
            .as_mut()
            .map_or_else(Vec::new, |stream| stream.renew());
        self.finish()
    }

    /// The frames given that the stream's chain still holds: written to the stream once more
    /// sound pushes them out.
    pub(super) fn held_frames(&self) -> usize {
        self.stream.as_ref().map_or(0, |stream| stream.chain.held())
    }

    fn through_stream(&mut self) {
        if let Some(stream) = &mut self.stream {
            stream.process(&mut self.samples);
        }
    }

    fn finish(&mut self) -> &[u8] {
        self.apply_volume();
        self.bytes.clear();
        self.bytes
            .extend(self.samples.iter().flat_map(|sample| sample.to_le_bytes()));
        &self.bytes
    }

    /// The user's volume: a factor, as on Windows; at full volume and settled the sound is left
    /// as it is.
    fn apply_volume(&mut self) {
        let target = self.volume.get();
        let mut level = self.level.unwrap_or(target);
        if level != 1.0 || target != 1.0 {
            for frame in self.samples.chunks_exact_mut(CHAIN_CHANNELS.get()) {
                level = one_pole(level, target, self.smoothing);
                if level != 1.0 {
                    for sample in frame {
                        *sample *= level;
                    }
                }
            }
        }
        self.level = Some(level);
    }
}

/// Sample consumption even when the decoder has no new bytes. Never advance beyond PCM
/// actually submitted, and never query PulseAudio from the controller/UI thread.
fn sample_output(run: &Run, out: &mut Sink, written: u64) -> Result<u64, AudioError> {
    if written == 0 || run.cancelled() {
        return Ok(0);
    }
    let latency = out.latency_us()?;
    if !run.cancelled() {
        let heard = pulse::bytes_to_micros(written).saturating_sub(latency);
        if heard > run.heard_us.fetch_max(heard, Ordering::Relaxed) {
            run.last_progress_us
                .store(run.started.elapsed().as_micros() as u64, Ordering::Relaxed);
        }
        run.settle_buffering();
    }
    Ok(latency)
}

/// The output latency at which what the stream still holds counts as played: its estimate keeps
/// a small floor after the last samples.
const DRAIN_AT: Duration = Duration::from_millis(20);

fn drain_output(run: &Run, out: &mut Sink, written: u64) -> Result<(), AudioError> {
    // Latency is an estimate: after the last samples it can retain a small nonzero floor.
    // Look again when the estimate says the tail is down to `DRAIN_AT`, and at most until the
    // initial estimated tail (capped) has passed, never renewing that deadline on tiny timing
    // corrections. Only drain's acknowledgement confirms the actual EOF.
    let tail = Duration::from_micros(sample_output(run, out, written)?).min(process::IO_TIMEOUT);
    let until = Instant::now() + tail;
    while !run.cancelled() {
        let remaining = Duration::from_micros(sample_output(run, out, written)?);
        if run.cancelled() {
            break;
        }
        let now = Instant::now();
        if remaining <= DRAIN_AT || now >= until {
            return out.drain();
        }
        std::thread::sleep((remaining - DRAIN_AT).min(until - now));
    }
    Ok(())
}

/// The decoder: `ffmpeg` reading `input` (the host's PCM on its stdin) from `from` for at most
/// `limit`, writing 48 kHz stereo `f32le` to its stdout. An address's requests carry its headers
/// and agent.
pub fn decoder_command(
    input: &AudioInput,
    (from, limit): (Duration, Option<Duration>),
) -> std::process::Command {
    let mut cmd = std::process::Command::new("ffmpeg");
    // Decode errors must not be silently accepted as successful (possibly premature) EOF.
    cmd.env("LC_ALL", "C").arg("-xerror");
    match input {
        AudioInput::Pcm(s) => {
            cmd.args([
                "-v",
                "error",
                "-f",
                "s16le",
                "-ar",
                &s.sample_rate().to_string(),
                "-ac",
                &s.channels().to_string(),
                "-i",
                "pipe:0",
            ]);
        }
        _ => {
            if is_network(input) {
                cmd.args(["-rw_timeout", "5000000"]);
            }
            if let AudioInput::Url(_, http) = input {
                if let Some(agent) = &http.user_agent {
                    cmd.arg("-user_agent").arg(agent);
                }
                if let Some(block) = http.header_block() {
                    cmd.arg("-headers").arg(block);
                }
            }
            cmd.args([
                "-v",
                "error",
                "-nostdin",
                "-ss",
                &format!("{:.6}", from.as_secs_f64()),
                "-i",
            ]);
            cmd.args(ffmpeg_input(input));
        }
    }
    if let Some(limit) = limit {
        cmd.args(["-t", &format!("{:.6}", limit.as_secs_f64())]);
    }
    cmd.args([
        "-vn",
        "-ac",
        &pulse::CHANNELS.to_string(),
        "-ar",
        &pulse::RATE.to_string(),
        "-f",
        "f32le",
        "-",
    ])
    .stdin(if matches!(input, AudioInput::Pcm(_)) {
        std::process::Stdio::piped()
    } else {
        std::process::Stdio::null()
    })
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped());
    cmd
}

/// Where a run's sound goes: the sound server's stream, or the capture a test listens to.
pub(super) enum Sink {
    Pulse(pulse::Stream),
    Capture(Capture),
}

impl Sink {
    fn write(&mut self, bytes: &[u8]) -> Result<(), AudioError> {
        match self {
            Self::Pulse(stream) => stream.write(bytes),
            Self::Capture(capture) => {
                capture.write(bytes);
                Ok(())
            }
        }
    }

    fn latency_us(&mut self) -> Result<u64, AudioError> {
        match self {
            Self::Pulse(stream) => stream.latency_us(),
            Self::Capture(capture) => Ok(capture.latency_us()),
        }
    }

    fn drain(&mut self) -> Result<(), AudioError> {
        match self {
            Self::Pulse(stream) => stream.drain(),
            Self::Capture(capture) => {
                capture.drain();
                Ok(())
            }
        }
    }

    fn flush(&mut self) -> Result<(), AudioError> {
        match self {
            Self::Pulse(stream) => stream.flush(),
            Self::Capture(_) => Ok(()),
        }
    }

    /// The sound fades out over `fade` and plays to its end; false when none sounds.
    fn fade_out(&mut self, fade: Duration) -> bool {
        match self {
            Self::Pulse(stream) => stream.fade_out(fade),
            Self::Capture(capture) => capture.fade_out(fade),
        }
    }
}

/// A decoder of `path` (or the host's `pcm`) over `span`, its child and feeder stopped with `r`,
/// and its nonblocking stdout. `None` when the run was cancelled meanwhile.
fn start_decoder(
    r: &Arc<Run>,
    input: &AudioInput,
    span: (Duration, Option<Duration>),
) -> Result<Option<(Decoder, std::process::ChildStdout)>, AudioError> {
    let decoder_io = |e| AudioError::new(AudioErrorKind::Decoder, format!("ffmpeg: {e}"));
    let mut cmd = decoder_command(input, span);
    let pcm: Option<Arc<dyn PcmSource>> = match input {
        AudioInput::Pcm(s) => Some(s.clone()),
        _ => None,
    };
    let mut child = cmd.spawn().map_err(decoder_io)?;
    let stdin = child.stdin.take();
    #[allow(
        clippy::expect_used,
        reason = "the child was spawned with piped stdout"
    )]
    let pipe = child.stdout.take().expect("piped stdout");
    #[allow(
        clippy::expect_used,
        reason = "the child was spawned with piped stderr"
    )]
    let stderr = child.stderr.take().expect("piped stderr");
    let feed_stop = pcm.as_ref().map_or_else(
        || Arc::new(AtomicBool::new(false)),
        |source| source.reader_token(),
    );
    r.add_feed(feed_stop.clone());
    let mut decoder = Decoder {
        child: process::OwnedChild::install(child, r.child_slot()),
        feed_stop,
        feeder: None,
        feed_error: Arc::new(Mutex::new(None)),
        stderr: None,
    };
    if r.cancelled() {
        return Ok(None);
    }
    decoder.stderr = Some(process::StderrTail::start(stderr).map_err(decoder_io)?);
    process::nonblocking(&pipe).map_err(decoder_io)?;
    if let (Some(source), Some(stdin)) = (pcm, stdin) {
        process::nonblocking(&stdin).map_err(decoder_io)?;
        let tokens = (r.clone(), decoder.feed_stop.clone());
        let error = decoder.feed_error.clone();
        let feeder = std::thread::Builder::new()
            .name("pcm-feed".into())
            .spawn(move || feed(source, stdin, tokens, error, span.0))
            .map_err(|e| AudioError::new(AudioErrorKind::Source, e.to_string()))?;
        decoder.feeder = Some(feeder);
    }
    Ok(Some((decoder, pipe)))
}

/// The next song's decoder, started ahead: it waits on its full pipe until this song's ends.
struct Ahead {
    song: Arc<Following>,
    reading: Reading,
}

/// A decoder being read: its child and nonblocking stdout, whether it reads from the network,
/// the bytes its span is to give (as known ahead) and those read of them, and when it last gave
/// any. Started together, replaced together.
struct Reading {
    /// Dropped, it reaps the child and its feeder.
    decoder: Decoder,
    pipe: std::process::ChildStdout,
    http: bool,
    length: Option<Duration>,
    read: u64,
    last_read: Instant,
}

impl Reading {
    /// A decoder of `input` over `span` started; `None` when the run was cancelled meanwhile.
    fn start(
        r: &Arc<Run>,
        input: &AudioInput,
        span: (Duration, Option<Duration>),
    ) -> Result<Option<Self>, AudioError> {
        Ok(start_decoder(r, input, span)?.map(|(decoder, pipe)| Self {
            decoder,
            pipe,
            http: is_network(input),
            length: span.1,
            read: 0,
            last_read: Instant::now(),
        }))
    }
}

/// `ffmpeg` decodes to 48 kHz stereo `f32le`; `processing` takes it to the stream. This worker
/// owns the cancellable PulseAudio event loop; controllers cancel children/feeds but never wait
/// for output cleanup.
///
/// The song the player wants next (`Run::next`) has its decoder started ahead; at this one's end
/// the writer goes on into it, in the same stream, its first frame right after this one's last.
pub(super) fn write_run(
    r: Arc<Run>,
    input: &AudioInput,
    span: (Duration, Option<Duration>),
    processing: Processing,
) {
    if let Some(mut writer) = Writer::start(r, input, span, processing) {
        while writer.pass().is_ok() {}
        writer.close();
    }
}

/// The writer is done: the run cancelled, failed (the failure reported) or played to its end.
struct Done;

/// A run's writer, on its thread: the decoder it reads, the one started ahead, and the stream it
/// writes.
struct Writer {
    r: Arc<Run>,
    /// What the stream is named for the sound server.
    what: String,
    /// The run's cancellation, for a stream being opened to give up on.
    cancel: Arc<dyn Fn() -> bool + Send + Sync>,
    out: Sink,
    /// The output route `out` was opened on.
    route_version: u32,
    /// The song decoding: its `length` and `read` say where a crossfade into the next one
    /// starts.
    reading: Reading,
    ahead: Option<Ahead>,
    /// The song going out under a crossfade, its decoder read alongside the coming one's.
    going: Option<Going>,
    /// The serial of the song this run went on into last, and of one whose decoder did not
    /// start: neither is started (again).
    followed: u64,
    refused: u64,
    processing: Processing,
    /// Bytes written to the stream.
    written: u64,
    buf: Vec<u8>,
    /// Bytes of `buf` read and not yet written.
    fill: usize,
}

impl Writer {
    /// The stream open and the decoder started; `None` if the run was cancelled meanwhile or
    /// either failed (the failure reported).
    fn start(
        r: Arc<Run>,
        input: &AudioInput,
        span: (Duration, Option<Duration>),
        processing: Processing,
    ) -> Option<Self> {
        if r.cancelled() {
            return None;
        }
        let what = input.name();
        let cancel: Arc<dyn Fn() -> bool + Send + Sync> = {
            let run = r.clone();
            Arc::new(move || run.cancelled())
        };
        let (out, route_version) = open_sink(&r, &what, &cancel);
        if r.cancelled() {
            return None;
        }
        let out = match out {
            Ok(out) => out,
            Err(error) => {
                r.fail(error);
                return None;
            }
        };
        let reading = match Reading::start(&r, input, span) {
            Ok(Some(started)) => started,
            Ok(None) => return None,
            Err(error) => {
                r.fail(error);
                return None;
            }
        };
        Some(Self {
            r,
            what,
            cancel,
            out,
            route_version,
            reading,
            ahead: None,
            going: None,
            followed: 0,
            refused: 0,
            processing,
            written: 0,
            buf: vec![0; pulse::RATE as usize / 50 * FRAME_BYTES],
            fill: 0,
        })
    }

    /// One pass: the song wanted next, the output route, then one read of the decoder's pipe.
    fn pass(&mut self) -> Result<(), Done> {
        if self.r.cancelled() {
            return Err(Done);
        }
        self.look_ahead()?;
        self.follow_route()?;
        if self.going.is_some() {
            if !self.processing.fading() {
                // The crossfade is over: what the going song has left is not heard.
                self.going = None;
            } else if self.processing.going_wanted(self.buf.len() / FRAME_BYTES) > 0 {
                return self.read_going();
            }
        }
        let end = self.read_end();
        if end == self.fill {
            return self.hand_over();
        }
        match self.reading.pipe.read(&mut self.buf[self.fill..end]) {
            Ok(0) => self.song_end(),
            Ok(n) => self.took(n),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => self.wait(),
            Err(e) => Err(self.decoder_failed(format!("ffmpeg: {e}"))),
        }
    }

    /// The song wanted next: its decoder started ahead, once; one no longer wanted stopped.
    fn look_ahead(&mut self) -> Result<(), Done> {
        let (followed, refused) = (self.followed, self.refused);
        let wanted = self
            .r
            .next
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .filter(|song| song.serial != followed && song.serial != refused);
        let ahead = self.ahead.as_ref().map(|next| next.song.serial);
        if ahead == wanted.as_ref().map(|song| song.serial) {
            return Ok(());
        }
        self.ahead = None;
        let Some(song) = wanted else {
            return Ok(());
        };
        match Reading::start(&self.r, &song.input, song.span()) {
            Ok(Some(reading)) => self.ahead = Some(Ahead { song, reading }),
            Ok(None) => return Err(Done),
            // Not followed: this song ends as it would have.
            Err(_) => self.refused = song.serial,
        }
        Ok(())
    }

    /// The output route changed: the stream reopened on it, what the old one still held counted
    /// as never written.
    fn follow_route(&mut self) -> Result<(), Done> {
        if ROUTE_VERSION.load(Ordering::SeqCst) == self.route_version {
            return Ok(());
        }
        let lost = self.out.latency_us().map_err(|e| self.fail(e))?;
        if self.r.cancelled() {
            return Err(Done);
        }
        self.out.flush().map_err(|e| self.fail(e))?;
        if self.r.cancelled() {
            return Err(Done);
        }
        let (next, version) = open_sink(&self.r, &self.what, &self.cancel);
        self.route_version = version;
        if self.r.cancelled() {
            return Err(Done);
        }
        self.out = next.map_err(|e| self.fail(e))?;
        let bytes_per_us = f64::from(pulse::RATE * pulse::FRAME_BYTES) / 1e6;
        self.written = self
            .written
            .saturating_sub((lost as f64 * bytes_per_us) as u64 & !(pulse::FRAME_BYTES as u64 - 1));
        Ok(())
    }

    /// `n` more bytes from the decoder: a full buffer goes through the processing to the stream.
    fn took(&mut self, n: usize) -> Result<(), Done> {
        self.reading.last_read = Instant::now();
        self.fill += n;
        self.reading.read += n as u64;
        if self.fill < self.buf.len() {
            return Ok(());
        }
        if self.r.cancelled() {
            return Err(Done);
        }
        let r = &self.r;
        let bytes = self.processing.block(&self.buf, || r.cancelled());
        if let Err(e) = self.out.write(bytes) {
            return Err(self.fail(e));
        }
        if self.r.cancelled() {
            return Err(Done);
        }
        self.written += bytes.len() as u64;
        self.fill = 0;
        sample_output(&self.r, &mut self.out, self.written).map_err(|e| self.fail(e))?;
        Ok(())
    }

    /// The decoder stopped: the run fails with its feeder's error, else with what its log says
    /// (`fallback` when it says nothing).
    fn decoder_failed(&mut self, fallback: String) -> Done {
        let (feed_error, log) = self.reading.decoder.finish();
        self.fail(
            feed_error.unwrap_or_else(|| decoder_error(log, self.reading.http, false, fallback)),
        )
    }

    /// The decoder has nothing yet: wait for it, or for the next thing due without it.
    fn wait(&mut self) -> Result<(), Done> {
        let latency = sample_output(&self.r, &mut self.out, self.written)
            .map(Duration::from_micros)
            .map_err(|e| self.fail(e))?;
        if self.reading.last_read.elapsed() >= output_timeout(self.reading.http) {
            return Err(self.decoder_failed("ffmpeg PCM output timed out".into()));
        }
        // Until ffmpeg writes (or exits), or the next thing due without it: the output has played
        // out what it holds (the heard position stops there), the stall reads as buffering, the
        // output guard, a look for the song wanted next.
        let now = Instant::now();
        let mut until =
            (self.reading.last_read + output_timeout(self.reading.http)).min(now + NEXT_LOOK);
        if latency > DRAIN_AT {
            until = until.min(now + latency);
        }
        if let Some(stall) = self.r.stall_at() {
            until = until.min(stall);
        }
        let until = until.max(now + Duration::from_millis(1));
        process::ready_one(&self.reading.pipe, libc::POLLIN, Some(until)).map_err(|e| {
            self.fail(AudioError::new(
                AudioErrorKind::Decoder,
                format!("ffmpeg: {e}"),
            ))
        })?;
        Ok(())
    }

    /// The decoder's pipe closed: the song's last frames out, then on into the next song, or the
    /// stream drained and the run at its end.
    fn song_end(&mut self) -> Result<(), Done> {
        self.settle_decoder()?;
        self.write_song_tail()?;
        // The next song, ready: on into it, its first frame right after this one's last (still
        // inside the stream's chain, the frames it holds), and the player told where that is
        // heard.
        if let Some(next) = self.ahead.take() {
            self.go_on_into(next, 0)?;
            return Ok(());
        }
        drain_output(&self.r, &mut self.out, self.written).map_err(|e| self.fail(e))?;
        if !self.r.cancelled() {
            self.r
                .heard_us
                .store(pulse::bytes_to_micros(self.written), Ordering::Relaxed);
            self.r.finish(Terminal::Eof);
        }
        Err(Done)
    }

    /// Where the read of the song decoding stops in `buf`: at the crossfade into the next one, if
    /// there is one to fade into and the song's length is known.
    fn read_end(&self) -> usize {
        let handover = self
            .ahead
            .as_ref()
            .and_then(|next| handover_at(self.reading.length, next.song.crossfade))
            .map(pulse::duration_to_bytes);
        match handover {
            Some(at) => {
                let before =
                    usize::try_from(at.saturating_sub(self.reading.read)).unwrap_or(usize::MAX);
                self.fill.saturating_add(before).min(self.buf.len())
            }
            None => self.buf.len(),
        }
    }

    /// The crossfade's start: what was read of the going song so far goes out, then the coming
    /// one is summed with the rest of it, its decoder read alongside.
    fn hand_over(&mut self) -> Result<(), Done> {
        let Some(next) = self.ahead.take() else {
            return Ok(());
        };
        let r = &self.r;
        let bytes = self
            .processing
            .block(&self.buf[..self.fill], || r.cancelled());
        if let Err(e) = write_counted(&mut self.out, &mut self.written, bytes) {
            return Err(self.fail(e));
        }
        let left = self
            .reading
            .length
            .map_or(0, pulse::duration_to_bytes)
            .saturating_sub(self.reading.read);
        let overlap = usize::try_from(left).unwrap_or(usize::MAX) / FRAME_BYTES;
        let going = self.go_on_into(next, overlap)?;
        if overlap > 0 {
            self.going = Some(going);
        }
        Ok(())
    }

    /// On into `next`, over the going song's last `overlap` frames: its decoder is the one read
    /// from now, and the player is told where it is heard (the overlap's start).
    /// The going song's decoder, given back.
    fn go_on_into(&mut self, next: Ahead, overlap: usize) -> Result<Going, Done> {
        let bytes = self.processing.follow(next.song.dsp.clone(), overlap);
        if !bytes.is_empty() {
            if let Err(e) = write_counted(&mut self.out, &mut self.written, bytes) {
                self.r.fail(e);
                return Err(Done);
            }
        }
        let given = self.written + (self.processing.held_frames() * FRAME_BYTES) as u64;
        self.r
            .cross(next.song.clone(), pulse::bytes_to_micros(given));
        self.followed = next.song.serial;
        self.fill = 0;
        let mut gone = std::mem::replace(&mut self.reading, next.reading);
        let now = Instant::now();
        self.reading.last_read = now;
        gone.last_read = now;
        Ok(Going {
            reading: gone,
            buf: Vec::new(),
        })
    }

    /// Reads the going song's decoder for the crossfade; its end, its failure or its silence past
    /// the output guard leave the coming one to finish rising alone: they are not the stream's.
    fn read_going(&mut self) -> Result<(), Done> {
        let Some(going) = self.going.as_mut() else {
            return Ok(());
        };
        let wanted = self.processing.going_wanted(self.buf.len() / FRAME_BYTES) * FRAME_BYTES;
        let start = going.buf.len();
        going.buf.resize(start + wanted, 0);
        let read = going.reading.pipe.read(&mut going.buf[start..]);
        going.buf.truncate(start + *read.as_ref().unwrap_or(&0));
        let r = &self.r;
        let ended = match read {
            Ok(0) => true,
            Ok(_) => {
                going.reading.last_read = Instant::now();
                let whole = pulse::whole_frames(going.buf.len());
                self.processing.going(&going.buf[..whole], || r.cancelled());
                going.buf.drain(..whole);
                false
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => false,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if going.reading.last_read.elapsed() >= output_timeout(going.reading.http) {
                    true
                } else {
                    let until = Instant::now() + Duration::from_millis(20);
                    process::ready_one(&going.reading.pipe, libc::POLLIN, Some(until)).is_err()
                }
            }
            Err(_) => true,
        };
        if ended {
            self.processing.going_ended(|| r.cancelled());
            self.going = None;
        }
        if self.r.cancelled() {
            return Err(Done);
        }
        Ok(())
    }

    /// The decoder that closed its pipe, reaped: a failure it reports, or one its log tells of,
    /// ends the run.
    fn settle_decoder(&mut self) -> Result<(), Done> {
        // Collect final stderr before publishing failure; the reader drains while wait() runs, so
        // stderr cannot fill its pipe and deadlock child exit.
        let r = &self.r;
        let status = self.reading.decoder.child.wait(|| r.cancelled());
        // A -t span may close ffmpeg before plugin EOF. Reclaim that feeder before draining
        // PulseAudio, which might never return.
        let (feed_error, log) = self.reading.decoder.finish();
        if self.r.cancelled() {
            return Err(Done);
        }
        if let Some(error) = feed_error {
            return Err(self.fail(error));
        }
        let failure = match status {
            Ok(status) if status.success() => None,
            Ok(status) => Some(format!("ffmpeg: {status}")),
            Err(e) => Some(format!("ffmpeg: {e}")),
        };
        // ffmpeg can return success after an interrupted HTTP read or a decode error. Do not let
        // explicit invalid-media/auth diagnostics become an EOF that the engine might mistake
        // for a retryable truncated network stream.
        if failure.is_some()
            || (self.reading.http && transport_log(&log))
            || non_transport_log(&log.to_ascii_lowercase())
        {
            return Err(self.fail(decoder_error(
                log,
                self.reading.http,
                false,
                failure.unwrap_or_else(|| "ffmpeg decoding stopped".into()),
            )));
        }
        Ok(())
    }

    /// The last whole frames, then what the song's chain still holds; then, unless a song
    /// follows (`go_on_into`), what the stream's chain still holds: the sound's end.
    fn write_song_tail(&mut self) -> Result<(), Done> {
        let tail = pulse::whole_frames(self.fill);
        let r = &self.r;
        let cancelled = || r.cancelled();
        let parts = if self.ahead.is_some() { 2 } else { 3 };
        for part in 0..parts {
            let bytes = match part {
                0 => self.processing.block(&self.buf[..tail], cancelled),
                1 => self.processing.song_end(cancelled),
                _ => self.processing.sound_end(),
            };
            if bytes.is_empty() {
                continue;
            }
            if let Err(e) = write_counted(&mut self.out, &mut self.written, bytes) {
                r.fail(e);
                return Err(Done);
            }
        }
        if self.r.cancelled() {
            return Err(Done);
        }
        Ok(())
    }

    fn fail(&self, error: AudioError) -> Done {
        self.r.fail(error);
        Done
    }

    /// The writer is done: whatever still feeds or decodes for it stops. A run stopped while
    /// it sounds fades out (`DECLICK`, or `LEAVE` when the player is disposed); one that failed
    /// is flushed; one that played to its end is left to play out.
    fn close(self) {
        let stopped = self.r.cancelled();
        let flush = stopped || self.r.terminal() != Terminal::Eof;
        let fade = if self.r.leaving.load(Ordering::Acquire) {
            LEAVE
        } else {
            DECLICK
        };
        self.r.stop.store(true, Ordering::Release);
        // Reap decoders, stderr readers and feeders before any potentially blocking PulseAudio
        // cleanup.
        let Self {
            ahead,
            reading,
            mut out,
            ..
        } = self;
        drop(ahead);
        drop(reading);
        if stopped && out.fade_out(fade) {
            return;
        }
        if flush {
            let _ = out.flush();
        }
    }
}

/// The song going out under a crossfade: its decoder, read alongside the coming one's, and what of
/// its output is not yet a whole frame.
struct Going {
    reading: Reading,
    buf: Vec<u8>,
}

/// Writes `bytes` to the stream and counts them as written (what the heard time is read
/// against).
fn write_counted(out: &mut Sink, written: &mut u64, bytes: &[u8]) -> Result<(), AudioError> {
    out.write(bytes)?;
    *written += bytes.len() as u64;
    Ok(())
}

/// The run's stream, on the route chosen now (the default sink if the chosen one will not open),
/// or the capture a test listens to; and the route's version.
fn open_sink(
    r: &Run,
    what: &str,
    cancel: &Arc<dyn Fn() -> bool + Send + Sync>,
) -> (Result<Sink, AudioError>, u32) {
    let version = ROUTE_VERSION.load(Ordering::SeqCst);
    if capturing() {
        return (Ok(Sink::Capture(Capture::new())), version);
    }
    let sink = ROUTE.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let stream = pulse::Stream::open(what, sink.as_deref(), Some(cancel.clone())).or_else(|e| {
        if r.cancelled() || sink.is_none() {
            Err(e)
        } else {
            pulse::Stream::open(what, None, Some(cancel.clone()))
        }
    });
    (stream.map(Sink::Pulse), version)
}

/// How long the writer waits on a silent decoder. Let ffmpeg's five-second -rw_timeout report its
/// real transport diagnostic first: this is only a decoder-output guard, not proof of a network
/// failure.
fn output_timeout(http: bool) -> Duration {
    process::IO_TIMEOUT
        + if http {
            Duration::from_secs(1)
        } else {
            Duration::ZERO
        }
}

/// How long a writer waiting on its decoder goes without looking for the song wanted next.
const NEXT_LOOK: Duration = Duration::from_millis(100);

#[cfg(test)]
mod tests;
