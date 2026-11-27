//! Native decoding and the two processing chains before the sound server.
use super::ffi::*;
use super::output::{capturing, Capture, ROUTE, ROUTE_VERSION};
use super::player::{Following, Run, Terminal};
use super::pulse;
use super::types::{AudioError, AudioErrorKind};
use crate::dsp::{
    handover_at, song_chain, stream_chain, Crossfade, DspChain, ReplayGainSetting, UserGain,
};
use crate::dsp::{one_pole, one_pole_coefficient, USER_GAIN_SMOOTHING};
use crate::source::{NativeError, NativeMediaByteSource, NativeMessage};
use crate::{AudioSourceFactory, AudioTrackSelection};
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

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

/// What a run does to the sound between the decoded block and the stream: the song's own chain, the
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
        rate: NonZeroU32,
    ) -> Self {
        Self {
            song: RunChain::new(song),
            stream: stream.map(RunChain::new),
            volume,
            level: None,
            smoothing: one_pole_coefficient(USER_GAIN_SMOOTHING, rate),
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
        let heard = pulse::bytes_to_micros(written, out.rate().get()).saturating_sub(latency);
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
    let tail = Duration::from_micros(sample_output(run, out, written)?).min(Duration::from_secs(5));
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

pub(super) enum Sink {
    Pulse(pulse::Stream),
    Capture(Capture),
}

impl Sink {
    fn rate(&self) -> NonZeroU32 {
        match self {
            Self::Pulse(s) => s.rate(),
            Self::Capture(c) => c.rate(),
        }
    }

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

/// The next song's decoder, ready to take over the stream.
struct Ahead {
    song: Arc<Following>,
    reading: Reading,
}

/// One decoder's PCM over its selected span; a native chunk can straddle writer blocks.
struct Reading {
    decoder: NativeAudioDecoder,
    length: Option<Duration>,
    read: u64,
    limit: Option<u64>,
    pending: Vec<u8>,
    offset: usize,
    timing: crate::StreamTiming,
    target: Option<Duration>,
    rate: u32,
    _cancel: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Reading {
    fn start(
        cancel_read: impl FnOnce(&Arc<dyn Fn() + Send + Sync>),
        factory: &dyn AudioSourceFactory,
        span: (Duration, Option<Duration>, AudioTrackSelection),
        rate: NonZeroU32,
    ) -> Result<Self, AudioError> {
        let source = factory.open().map_err(|e| {
            AudioError::new(
                if crate::source::source_is_network_failure(&e) {
                    AudioErrorKind::Transport
                } else {
                    AudioErrorKind::Source
                },
                e.to_string(),
            )
        })?;
        let cancel = source.canceller();
        if let Some(c) = &cancel {
            cancel_read(c);
        }
        let (decoder, info, _) = open_native_audio(
            NativeMediaByteSource::new(source),
            crate::track::requested_stream_index(span.2),
            i32::try_from(rate.get())
                .map_err(|e| AudioError::new(AudioErrorKind::Output, e.to_string()))?,
            pulse::CHANNELS as u16,
        )?;
        let time_base = crate::StreamTimeBase {
            numerator: info.time_base_num,
            denominator: info.time_base_den,
        };
        let start_time = (info.has_start_time != 0).then_some(info.start_time);
        let timing = crate::StreamTiming {
            time_base,
            start_time,
            presentation_offset_nanoseconds: crate::presentation_offset_nanoseconds(
                time_base,
                start_time,
                (info.has_format_start_time != 0).then_some(info.format_start_time_us),
            )
            .map_err(|e| AudioError::new(AudioErrorKind::Decoder, e.to_string()))?,
        };
        let reading = Self {
            decoder,
            length: span.1,
            read: 0,
            limit: span.1.map(|d| pulse::duration_to_bytes(d, rate.get())),
            pending: Vec::new(),
            offset: 0,
            timing,
            target: (!span.0.is_zero()).then_some(span.0),
            rate: rate.get(),
            _cancel: cancel,
        };
        if let Some(at) = reading.target {
            let pts = crate::raw_timestamp_for_position(at, timing)
                .map_err(|e| AudioError::new(AudioErrorKind::Decoder, e.to_string()))?;
            let mut error = NativeMessage::new();
            // SAFETY: reading owns the decoder; the message buffer is live for the call.
            let status = unsafe {
                bn_audio_decoder_seek(
                    reading.decoder.raw.as_ptr(),
                    pts,
                    error.as_mut_ptr(),
                    error.capacity(),
                )
            };
            if status != 0 {
                return Err(reading.failure("seek", status, error));
            }
        }
        Ok(reading)
    }
    fn failure(&self, operation: &'static str, status: i32, error: NativeMessage) -> AudioError {
        // SAFETY: this reader owns the live decoder.
        let transport = unsafe { bn_audio_decoder_transport_error(self.decoder.raw.as_ptr()) };
        NativeError {
            operation,
            status,
            transport,
            message: error.message(),
        }
        .into()
    }
    /// None while the demuxer awaits input; zero at the end of this span.
    fn read(&mut self, to: &mut [u8]) -> Result<Option<usize>, AudioError> {
        let left = self.limit.map_or(usize::MAX, |n| {
            usize::try_from(n.saturating_sub(self.read)).unwrap_or(usize::MAX)
        });
        if left == 0 {
            return Ok(Some(0));
        }
        while self.offset == self.pending.len() {
            let mut chunk = BnAudioChunk::default();
            let mut error = NativeMessage::new();
            // SAFETY: decoder, out-parameter and message buffer live throughout the call.
            let status = unsafe {
                bn_audio_decoder_decode_next(
                    self.decoder.raw.as_ptr(),
                    &mut chunk,
                    error.as_mut_ptr(),
                    error.capacity(),
                )
            };
            match status {
                AUDIO_AGAIN => return Ok(None),
                AUDIO_EOF => return Ok(Some(0)),
                0 => {}
                _ => return Err(self.failure("decode", status, error)),
            }
            let frames = usize::try_from(chunk.frames)
                .map_err(|e| AudioError::new(AudioErrorKind::Decoder, e.to_string()))?;
            if frames == 0 || chunk.samples.is_null() {
                continue;
            }
            let skip = match self.target {
                Some(target) if chunk.timing.has_timestamp != 0 => {
                    let ts = crate::normalize_timestamp(chunk.timing.pts, self.timing)
                        .map_err(|e| AudioError::new(AudioErrorKind::Decoder, e.to_string()))?;
                    let ns = (target.as_nanos() as i128 - ts.as_nanoseconds()).max(0) as u128;
                    usize::try_from((ns * u128::from(self.rate)).div_ceil(1_000_000_000))
                        .unwrap_or(usize::MAX)
                        .min(frames)
                }
                _ => 0,
            };
            if skip == frames {
                continue;
            }
            self.target = None;
            // SAFETY: one interleaved stereo f32 block, borrowed only until the next decoder call.
            let bytes = unsafe {
                std::slice::from_raw_parts(chunk.samples.cast::<u8>(), frames * FRAME_BYTES)
            };
            self.pending.clear();
            self.pending.extend_from_slice(&bytes[skip * FRAME_BYTES..]);
            self.offset = 0;
        }
        let n = left.min(to.len()).min(self.pending.len() - self.offset);
        to[..n].copy_from_slice(&self.pending[self.offset..self.offset + n]);
        self.offset += n;
        self.read += n as u64;
        Ok(Some(n))
    }
}

/// The decoder gives sink-rate stereo PCM; `processing` takes it to the stream. This worker
/// owns the cancellable PulseAudio event loop; controllers interrupt source reads but never wait
/// for output cleanup.
///
/// The song the player wants next (`Run::next`) has its decoder started ahead; at this one's end
/// the writer goes on into it, in the same stream, its first frame right after this one's last.
pub(super) fn write_run(
    r: Arc<Run>,
    factory: &dyn AudioSourceFactory,
    span: (Duration, Option<Duration>, AudioTrackSelection),
    tags: crate::ReplayGainTags,
    gain: Arc<ReplayGainSetting>,
    effects: Option<crate::clap::Effects>,
    volume: Arc<UserGain>,
) {
    if let Some(mut writer) = Writer::start(r, factory, span, tags, gain, effects, volume) {
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
    gain: Arc<ReplayGainSetting>,
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
        factory: &dyn AudioSourceFactory,
        span: (Duration, Option<Duration>, AudioTrackSelection),
        tags: crate::ReplayGainTags,
        gain: Arc<ReplayGainSetting>,
        effects: Option<crate::clap::Effects>,
        volume: Arc<UserGain>,
    ) -> Option<Self> {
        if r.cancelled() {
            return None;
        }
        let what = factory.label();
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
        let rate = out.rate();
        let song = song_chain(gain.clone(), tags, rate, CHAIN_CHANNELS);
        let stream = stream_chain(effects.as_ref(), rate, CHAIN_CHANNELS);
        let processing = Processing::new(Arc::new(Mutex::new(song)), stream, volume, rate);
        let reading = match Reading::start(|c| r.reading(c), factory, span, rate) {
            Ok(started) => started,
            Err(error) => {
                r.fail(error);
                return None;
            }
        };
        Some(Self {
            r,
            gain,
            out,
            route_version,
            reading,
            ahead: None,
            going: None,
            followed: 0,
            refused: 0,
            processing,
            written: 0,
            buf: vec![0; rate.get() as usize / 50 * FRAME_BYTES],
            fill: 0,
        })
    }

    /// One pass: the song wanted next, the output route, then one block from the decoder.
    fn pass(&mut self) -> Result<(), Done> {
        if self.r.cancelled() {
            return Err(Done);
        }
        self.look_ahead()?;
        self.follow_route()?;
        if !self.processing.fading() {
            self.going = None;
        } else if let Some(going) = &mut self.going {
            let frames = self.processing.going_wanted(self.buf.len() / FRAME_BYTES);
            if frames > 0 {
                going.buf.resize(frames * FRAME_BYTES, 0);
                let cancelled = || self.r.cancelled();
                match going.reading.read(&mut going.buf) {
                    Ok(None) => {}
                    Ok(Some(0)) | Err(_) => {
                        // Only the outgoing song ended or failed; the incoming song keeps playing.
                        self.processing.going_ended(cancelled);
                        self.going = None;
                    }
                    Ok(Some(n)) => self.processing.going(&going.buf[..n], cancelled),
                }
                return Ok(());
            }
        }
        let end = self.read_end();
        if end == self.fill {
            return self.hand_over();
        }
        match self.reading.read(&mut self.buf[self.fill..end]) {
            Ok(Some(0)) => self.song_end(),
            Ok(Some(n)) => self.took(n),
            Ok(None) => {
                sample_output(&self.r, &mut self.out, self.written).map_err(|e| self.fail(e))?;
                Ok(())
            }
            Err(e) => Err(self.fail(e)),
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
        let (at, len) = song.span();
        match Reading::start(
            |c| self.r.reading(c),
            song.factory.as_ref(),
            (at, len, song.track),
            self.out.rate(),
        ) {
            Ok(reading) => self.ahead = Some(Ahead { song, reading }),
            // Not followed: this song ends as it would have.
            Err(_) => self.refused = song.serial,
        }
        Ok(())
    }

    /// A new route needs a new decoder and chains at its sink's rate. The controller
    /// starts the run again from the heard position.
    fn follow_route(&mut self) -> Result<(), Done> {
        if ROUTE_VERSION.load(Ordering::SeqCst) == self.route_version {
            return Ok(());
        }
        sample_output(&self.r, &mut self.out, self.written).map_err(|e| self.fail(e))?;
        self.r.finish(Terminal::Reroute);
        Err(Done)
    }

    /// `n` more bytes from the decoder: a full buffer goes through the processing to the stream.
    fn took(&mut self, n: usize) -> Result<(), Done> {
        self.fill += n;

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

    /// The decoder reached the end of its span: the song's last frames out, then on into the next song, or the
    /// stream drained and the run at its end.
    fn song_end(&mut self) -> Result<(), Done> {
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
            self.r.heard_us.store(
                pulse::bytes_to_micros(self.written, self.out.rate().get()),
                Ordering::Relaxed,
            );
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
            .map(|d| pulse::duration_to_bytes(d, self.out.rate().get()));
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
            .map_or(0, |d| pulse::duration_to_bytes(d, self.out.rate().get()))
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
        let chain = song_chain(
            self.gain.clone(),
            next.song.tags,
            self.out.rate(),
            CHAIN_CHANNELS,
        );
        let bytes = self.processing.follow(Arc::new(Mutex::new(chain)), overlap);
        if !bytes.is_empty() {
            if let Err(e) = write_counted(&mut self.out, &mut self.written, bytes) {
                self.r.fail(e);
                return Err(Done);
            }
        }
        let given = self.written + (self.processing.held_frames() * FRAME_BYTES) as u64;
        self.r.cross(
            next.song.clone(),
            pulse::bytes_to_micros(given, self.out.rate().get()),
        );
        self.followed = next.song.serial;
        self.fill = 0;
        let gone = std::mem::replace(&mut self.reading, next.reading);
        Ok(Going {
            reading: gone,
            buf: Vec::new(),
        })
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
        // Release decoders and their sources before any potentially blocking PulseAudio
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

/// The outgoing decoder and its reusable read buffer.
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

#[cfg(feature = "test-support")]
pub fn decode_pcm(
    path: &std::path::Path,
    from: Duration,
    rate: NonZeroU32,
) -> Result<Vec<u8>, AudioError> {
    let factory = crate::PathAudioSourceFactory::new(path);
    let mut reading = Reading::start(
        |_| {},
        &factory,
        (from, None, AudioTrackSelection::Default),
        rate,
    )?;
    let mut out = Vec::new();
    let mut buf = [0; 8192];
    loop {
        match reading.read(&mut buf)? {
            Some(0) => return Ok(out),
            Some(n) => out.extend_from_slice(&buf[..n]),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests;
