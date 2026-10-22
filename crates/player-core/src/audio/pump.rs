//! The decoder pump: opening the native decoder over a byte source and refilling the ring
//! from it.

use super::callback::frames_to_duration;
use super::clock::AudioClock;
use super::error::{AudioError, AudioOpenError, NativeError};
use super::ffi::{
    bn_audio_decoder_codec_name, bn_audio_decoder_decode_next, bn_audio_decoder_input_state,
    bn_audio_decoder_open_source_stream_with_error, bn_audio_decoder_passed_over,
    bn_audio_decoder_read_dsd, bn_audio_decoder_resampler_input_frames, bn_audio_decoder_seek,
    bn_audio_decoder_set_source_local, bn_audio_decoder_transport_error, BnAudioChunk,
    BnAudioStreamInfo, BnDsdChunk, NativeAudioDecoder, AUDIO_AGAIN, AUDIO_EOF,
    BN_AUDIO_INVALID_STREAM,
};
use super::metrics::SharedMetrics;
use super::mix::MixStage;
use super::session::{AudioSession, StopBound};
use super::stream::StreamWriter;
use super::track::AudioStreamIndex;
use super::worker::WorkerReady;
use crate::dsp::DspChain;
use crate::source::{NativeMediaByteSource, NativeMessage};
use crate::{
    normalize_timestamp, raw_timestamp_for_position, AudioSourceFactory, NormalizedTimestamp,
    StreamTiming,
};
use std::collections::VecDeque;
use std::num::{NonZeroU32, NonZeroUsize};
use std::ptr::{self, NonNull};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

pub(super) enum PendingAudio {
    Pcm(Vec<f32>),
    Dsd(Vec<u8>),
}

impl PendingAudio {
    fn len(&self) -> usize {
        match self {
            Self::Pcm(samples) => samples.len(),
            Self::Dsd(bytes) => bytes.len(),
        }
    }
    fn clear(&mut self) {
        match self {
            Self::Pcm(samples) => samples.clear(),
            Self::Dsd(bytes) => bytes.clear(),
        }
    }
    fn push_to(&self, out: &mut StreamWriter, range: std::ops::Range<usize>) -> usize {
        match self {
            Self::Pcm(samples) => out.push_pcm(&samples[range]),
            Self::Dsd(bytes) => out.push_dsd(&bytes[range]),
        }
    }
}

/// A decoded block's samples, borrowed from the decoder until its next call.
pub enum BlockData<'a> {
    Pcm(&'a [f32]),
    Dsd(&'a [u8]),
}

impl BlockData<'_> {
    fn is_empty(&self) -> bool {
        match self {
            Self::Pcm(samples) => samples.is_empty(),
            Self::Dsd(bytes) => bytes.is_empty(),
        }
    }
}

pub struct DecoderPump {
    pub(super) mixer: Option<MixStage>,
    /// The song's own processing (`crate::dsp::song_chain`), before the stream's; `None` for
    /// DSD, which bypasses both.
    pub dsp: Option<DspChain>,
    mixed: VecDeque<f32>,
    timeline: VecDeque<(NormalizedTimestamp, usize, usize)>,
    decoder_ended: bool,
    mix_counted: (u64, u64, u64, u64, u64, u64),
    pub decoder: NativeAudioDecoder,
    pub timing: StreamTiming,
    pub(super) sample_rate: NonZeroU32,
    pub(super) channels: usize,
    pub(super) metrics: Arc<SharedMetrics>,
    pub(super) pending: PendingAudio,
    pub(super) pending_frame: usize,
    pub(super) pending_start: Option<NormalizedTimestamp>,
    pub(super) generation_anchor: Option<NormalizedTimestamp>,
    pub(super) target: Option<NormalizedTimestamp>,
    pub(super) preroll_frames_discarded: u64,
    pub(super) last_chunk_timestamp: Option<NormalizedTimestamp>,
    /// Where the last block placed ends: a block whose time runs back is placed here.
    last_chunk_end: Option<NormalizedTimestamp>,
    /// Where this song's sound has ended, if it has: everything it has to give is in the ring.
    pub end: Option<SongEnd>,
    /// The last decode found no packet queued: the refill stopped short, for now.
    pub(super) starved: bool,
    /// The session's stop bound, read afresh at every refill: a bound moved on plays on.
    pub(super) stop: Arc<StopBound>,
    /// Where `pending`'s first frame stands in media time — the block's, before any skip.
    pub block_start: Option<NormalizedTimestamp>,
    /// The decoder's own count of frames handed to its resampler, as last read.
    pub(super) resampler_input_seen: i64,
    /// The decoder's own count of frames it passed over, as last read.
    pub(super) passed_over_seen: i64,
    /// A song follows this one in the same ring (the worker holds it ready): its end is where
    /// that one starts, not the end of the sound, and is not published as such.
    pub(super) end_held: bool,
    /// Where the stream goes on into the song that follows, when that one overlaps this one's end
    /// (a crossfade): the ring is handed nothing past it, as at the stop, and what comes after
    /// goes to the overlap once the worker has handed over.
    handover: Option<NormalizedTimestamp>,
}

/// How a song's sound ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SongEnd {
    /// The container's end: nothing more will be decoded until a seek.
    File,
    /// The stop bound, or the handover to an overlapping next song: every frame before it is in
    /// the ring and the block held reaches past it. The block is kept, so a bound moved on later
    /// plays on from the very frame it stopped at.
    StopBound,
}

impl DecoderPump {
    /// A decoder call that failed, with the decoder's own transport error.
    fn native_failure(&self, operation: &'static str, status: i32, message: String) -> AudioError {
        AudioError::Native(NativeError {
            operation,
            status,
            // SAFETY: the decoder this pump owns, live until the pump drops.
            transport: unsafe { bn_audio_decoder_transport_error(self.decoder.raw.as_ptr()) },
            message,
        })
    }

    pub(super) fn new(
        decoder: NativeAudioDecoder,
        timing: StreamTiming,
        sample_rate: NonZeroU32,
        channels: NonZeroUsize,
        metrics: Arc<SharedMetrics>,
        stop: Arc<StopBound>,
    ) -> Self {
        let pending = if decoder.dsd {
            PendingAudio::Dsd(Vec::new())
        } else {
            PendingAudio::Pcm(Vec::new())
        };
        Self {
            mixer: None,
            dsp: None,
            mixed: VecDeque::new(),
            timeline: VecDeque::new(),
            decoder_ended: false,
            mix_counted: (0, 0, 0, 0, 0, 0),
            decoder,
            timing,
            sample_rate,
            channels: channels.get(),
            metrics,
            pending,
            pending_frame: 0,
            pending_start: None,
            generation_anchor: None,
            target: None,
            preroll_frames_discarded: 0,
            last_chunk_timestamp: None,
            last_chunk_end: None,
            end: None,
            starved: false,
            stop,
            block_start: None,
            resampler_input_seen: 0,
            passed_over_seen: 0,
            end_held: false,
            handover: None,
        }
    }

    /// Whether everything this song has to give is in the ring: its end, or its stop bound.
    pub(super) fn at_end(&self) -> bool {
        self.end.is_some()
    }

    /// Whether this song's end is the end of the sound: it has ended and no song follows.
    pub(super) fn sound_ended(&self) -> bool {
        self.at_end() && !self.end_held
    }

    /// The one writer of the song's end, and of the end the host reads from it.
    fn set_end(&mut self, end: Option<SongEnd>) {
        self.end = end;
        self.metrics
            .flag(&self.metrics.eof_reached, self.sound_ended());
    }

    /// Whether a song follows this one. One that stops following at its end publishes the end it
    /// held back.
    pub(super) fn hold_end(&mut self, held: bool) {
        if self.end_held == held {
            return;
        }
        self.end_held = held;
        self.set_end(self.end);
        self.publish_input_state();
    }

    /// Where the stream goes on into the next song, if it overlaps this one's end; `None` runs to
    /// the end (gapless, or nothing follows), and lets a pump handed over play on past the point.
    pub(super) fn hand_over_at(&mut self, at: Option<NormalizedTimestamp>) {
        if self.handover != at {
            self.handover = at;
            self.settle_stop();
        }
    }

    /// Whether the ring has everything before the handover: the next song is to come in now.
    pub(super) fn at_handover(&self) -> bool {
        self.handover.is_some() && self.end == Some(SongEnd::StopBound)
    }

    /// Where the song's sound ends, as known ahead (`StopBound::end`): where an overlap with the
    /// next one would end. `None` for DSD, which nothing overlaps.
    pub(super) fn known_end(&self) -> Option<NormalizedTimestamp> {
        self.known_end_time()
            .map(NormalizedTimestamp::from_position)
    }

    /// [`Self::known_end`] as a position.
    pub(super) fn known_end_time(&self) -> Option<Duration> {
        if self.decoder.dsd {
            return None;
        }
        self.stop.end()
    }

    /// Frames from the next one to hand on to the song's end as known ahead: none if that is not
    /// known, or already passed.
    pub(super) fn frames_to_known_end(&self) -> usize {
        let (Some(end), Some(start)) = (self.known_end(), self.block_start) else {
            return 0;
        };
        let rate = i128::from(self.sample_rate.get());
        let end = (end.as_nanoseconds() - start.as_nanoseconds()) * rate / 1_000_000_000;
        usize::try_from(end)
            .unwrap_or(0)
            .saturating_sub(self.pending_frame)
    }

    /// A fresh pump's start: the frames before `target` are passed over, to the sample.
    pub(super) fn aim(&mut self, target: Option<NormalizedTimestamp>) {
        self.target = target;
    }

    /// The container's end, reached.
    fn reach_end(&mut self) {
        self.set_end(Some(SongEnd::File));
    }

    /// How many of the pending block's frames stand before the stop bound, or the handover: all
    /// of them with neither. A frame at the bound's own instant is past it.
    fn frames_before_stop(&self, pending_frames: usize) -> usize {
        let bound = [self.stop.get(), self.handover].into_iter().flatten().min();
        match (bound, self.block_start) {
            (Some(bound), Some(start)) => self.frames_before(start, bound, pending_frames),
            _ => pending_frames,
        }
    }

    fn frames_before(
        &self,
        start: NormalizedTimestamp,
        target: NormalizedTimestamp,
        frames: usize,
    ) -> usize {
        if self.decoder.dsd || self.mixer.is_some() {
            // DSD and mixer timestamps identify whole frames. Recover that integer before
            // subtracting: subtracting two truncated nanosecond times can discard an extra
            // byte at a span boundary that lies exactly on a sample.
            let rate = i128::from(self.sample_rate.get());
            let first = (start.as_nanoseconds() * rate + 500_000_000).div_euclid(1_000_000_000);
            let target = (target.as_nanoseconds() * rate + 999_999_999).div_euclid(1_000_000_000);
            usize::try_from(target.saturating_sub(first))
                .unwrap_or(0)
                .min(frames)
        } else {
            frames_before_target(start, target, self.sample_rate, frames)
        }
    }

    /// Whether the refill stands at the stop bound: every frame before it handed over, and the
    /// block held reaching past it. Published as the end of the stream while it holds.
    fn settle_stop(&mut self) {
        let pending_frames = self.pending.len() / self.channels;
        let playable = self.frames_before_stop(pending_frames);
        if self.end == Some(SongEnd::File) {
            return;
        }
        let at_stop = playable < pending_frames && self.pending_frame >= playable;
        let end = at_stop.then_some(SongEnd::StopBound);
        if end != self.end {
            self.set_end(end);
        }
    }

    /// Publishes what the demux thread holds, for the transport's buffering decision.
    pub fn publish_input_state(&self) {
        let (mut queued_us, mut ended) = (0_i64, 0_i32);
        // SAFETY: the decoder this pump owns, live until the pump drops; two live out-parameters.
        unsafe {
            bn_audio_decoder_input_state(self.decoder.raw.as_ptr(), &mut queued_us, &mut ended)
        };
        self.metrics.input_queued_ms.store(
            u64::try_from(queued_us / 1_000).unwrap_or(0),
            Ordering::Release,
        );
        // At the stop bound the sound is over as far as the transport is concerned: nothing it
        // is waiting for is coming.
        let over = self.sound_ended();
        self.metrics
            .flag(&self.metrics.input_ended, ended != 0 || over);
        self.metrics.flag(
            &self.metrics.input_starved,
            self.starved && !over && ended == 0 && queued_us <= 0,
        );
    }

    pub fn seek(&mut self, position: Duration) -> Result<(), AudioError> {
        let target_pts = raw_timestamp_for_position(position, self.timing)?;
        let mut error = NativeMessage::new();
        // SAFETY: the decoder this pump owns, live until the pump drops; the message buffer is given with its capacity.
        let status = unsafe {
            bn_audio_decoder_seek(
                self.decoder.raw.as_ptr(),
                target_pts,
                error.as_mut_ptr(),
                error.capacity(),
            )
        };
        if status != 0 {
            return Err(self.native_failure("seek", status, error.message()));
        }
        Ok(())
    }

    /// Starts a generation: everything decoded for the last one is forgotten.
    pub(super) fn reset_generation(&mut self, target: Option<NormalizedTimestamp>) {
        if let Some(mixer) = &mut self.mixer {
            mixer.reset();
        }
        if let Some(dsp) = &mut self.dsp {
            dsp.reset();
        }
        self.mixed.clear();
        self.timeline.clear();
        self.decoder_ended = false;
        self.pending.clear();
        self.pending_frame = 0;
        self.pending_start = None;
        self.generation_anchor = None;
        self.target = target;
        self.preroll_frames_discarded = 0;
        self.last_chunk_timestamp = None;
        self.last_chunk_end = None;
        self.starved = false;
        self.block_start = None;
        self.set_end(None);
    }

    /// Prefills the generation and anchors the clock on its first sound. `None` while that sound
    /// has not arrived: over a network that is a wait, and the worker comes back to it between
    /// looks at its commands rather than sitting in it.
    pub(super) fn try_anchor(
        &mut self,
        out: &mut StreamWriter,
        clock: &AudioClock,
        generation: u64,
        prefill_frames: usize,
    ) -> Result<Option<WorkerReady>, AudioError> {
        self.refill_to(out, prefill_frames)?;
        // A target at or past the sound's own end, or its stop bound, decodes nothing: the sound
        // is simply over there. The generation anchors at the target with an empty ring, so the
        // output reads as drained, instead of failing the open or the seek: a later seek back
        // still has the sound to bring.
        let anchor = match (self.generation_anchor, self.target.or(self.pending_start)) {
            (Some(anchor), _) => anchor,
            (None, Some(target)) if self.at_end() => {
                self.generation_anchor = Some(target);
                target
            }
            (None, _) if self.at_end() => {
                return Err(AudioError::Worker(
                    "audio stream ended before a presentation anchor was decoded".to_owned(),
                ))
            }
            (None, _) => return Ok(None),
        };
        clock.reanchor(generation, anchor)?;
        self.metrics.clock_reanchors.fetch_add(1, Ordering::Relaxed);
        Ok(Some(WorkerReady { generation, anchor }))
    }

    /// `reset_generation` + `try_anchor` until it lands. For callers with nothing else to do.
    #[cfg(any(test, feature = "test-support"))]
    pub fn begin_generation(
        &mut self,
        out: &mut StreamWriter,
        clock: &AudioClock,
        generation: u64,
        target: Option<NormalizedTimestamp>,
        prefill_frames: usize,
    ) -> Result<WorkerReady, AudioError> {
        self.reset_generation(target);
        loop {
            if let Some(ready) = self.try_anchor(out, clock, generation, prefill_frames)? {
                return Ok(ready);
            }
        }
    }

    pub fn refill_to(
        &mut self,
        out: &mut StreamWriter,
        target_frames: usize,
    ) -> Result<(), AudioError> {
        #[cfg(feature = "instrument")]
        crate::record_audio_decoder_refill();
        self.starved = false;
        while out.ring.available_frames() < target_frames
            && self.end != Some(SongEnd::File)
            && !self.starved
        {
            let pending_frames = self.pending.len() / self.channels;
            // The stop bound: what of the block stands before it is handed over, and the rest is
            // held where it is, with everything after it, until the bound moves on.
            let playable = self.frames_before_stop(pending_frames);
            if self.pending_frame < playable {
                if self.generation_anchor.is_none() {
                    self.generation_anchor = self.pending_start;
                }
                let pushed = self.pending.push_to(
                    out,
                    self.pending_frame * self.channels..playable * self.channels,
                );
                self.pending_frame += pushed;
                if pushed == 0 {
                    break;
                }
                continue;
            }
            if playable < pending_frames {
                break;
            }
            self.decode_block()?;
        }
        self.settle_stop();
        if self.sound_ended() {
            out.finish();
        }
        Ok(())
    }

    /// Frames the decoder passed over since it was last asked, into the session's count. The first
    /// of a session is said once — FFmpeg says every one on its own — with where the sound was.
    fn count_passed_over(&mut self) {
        // SAFETY: the decoder this pump owns, live until the pump drops.
        let passed = unsafe { bn_audio_decoder_passed_over(self.decoder.raw.as_ptr()) };
        let fresh = u64::try_from(passed - self.passed_over_seen).unwrap_or(0);
        if fresh == 0 {
            return;
        }
        self.passed_over_seen = passed;
        let before = self
            .metrics
            .frames_passed_over
            .fetch_add(fresh, Ordering::Relaxed);
        #[cfg(feature = "instrument")]
        if before == 0 {
            let after = self
                .last_chunk_timestamp
                .map_or(0.0, |at| at.as_nanoseconds() as f64 / 1e9);
            eprintln!(
                "player-core[audio] passed over {fresh} frame(s) that did not decode, after {after:.3} s: \
                 their span plays as silence (said once per session)"
            );
        }
        #[cfg(not(feature = "instrument"))]
        let _ = before;
    }

    pub(super) fn decode_block(&mut self) -> Result<(), AudioError> {
        self.pending.clear();
        self.pending_frame = 0;
        self.pending_start = None;
        if self.mixer.is_some() && self.take_mixed()? {
            return Ok(());
        }
        if self.decoder_ended {
            if !self.flush_dsp()? {
                self.reach_end();
            }
            return Ok(());
        }
        let mut chunk = BnAudioChunk::default();
        let mut dsd_chunk = BnDsdChunk::default();
        let mut error = NativeMessage::new();
        #[cfg(feature = "instrument")]
        crate::record_audio_decode();
        // SAFETY: the decoder this pump owns, live until the pump drops; the chunk and the message buffer (with its capacity) are live locals.
        let status = unsafe {
            if self.decoder.dsd {
                let status = bn_audio_decoder_read_dsd(
                    self.decoder.raw.as_ptr(),
                    &mut dsd_chunk,
                    error.as_mut_ptr(),
                    error.capacity(),
                );
                chunk.frames = dsd_chunk.frames;
                chunk.timing = dsd_chunk.timing;
                status
            } else {
                bn_audio_decoder_decode_next(
                    self.decoder.raw.as_ptr(),
                    &mut chunk,
                    error.as_mut_ptr(),
                    error.capacity(),
                )
            }
        };
        if !self.decoder.dsd {
            self.count_passed_over();
        }
        if status == AUDIO_EOF {
            self.decoder_ended = true;
            if let Some(mixer) = &mut self.mixer {
                self.mixed
                    .extend(mixer.flush().map_err(AudioError::Worker)?);
                self.count_mix();
                if self.take_mixed()? {
                    return Ok(());
                }
            }
            if self.flush_dsp()? {
                return Ok(());
            }
            self.reach_end();
            return Ok(());
        }
        if status == AUDIO_AGAIN {
            self.starved = true;
            return Ok(());
        }
        if status != 0 {
            return Err(self.native_failure("decode", status, error.message()));
        }
        let frames = usize::try_from(chunk.frames).map_err(|_| {
            AudioError::Worker("audio block exposed a negative frame count".to_owned())
        })?;
        let samples = frames
            .checked_mul(
                self.mixer
                    .as_ref()
                    .map_or(self.channels, MixStage::input_channels),
            )
            .ok_or_else(|| AudioError::Worker("audio block size overflowed".to_owned()))?;
        let pts = (chunk.timing.has_timestamp != 0).then_some(chunk.timing.pts);
        // The decoder owns the block's buffer until its next call, and `samples` is its length
        // in elements; a null buffer is an empty block.
        let data = if self.decoder.dsd {
            BlockData::Dsd(if dsd_chunk.bytes.is_null() {
                &[]
            } else {
                // SAFETY: as said above: a live buffer of `samples` bytes until the next call.
                unsafe { std::slice::from_raw_parts(dsd_chunk.bytes, samples) }
            })
        } else {
            BlockData::Pcm(if chunk.samples.is_null() {
                &[]
            } else {
                // SAFETY: as said above: a live buffer of `samples` floats until the next call.
                unsafe { std::slice::from_raw_parts(chunk.samples, samples) }
            })
        };
        self.accept_block(pts, frames, data)
    }

    /// Takes one decoded block: places it in media time and hands it on, to the mixer or to
    /// `pending`. What a damaged or resynced stream hands over is repaired here and counted, not
    /// turned into a failure (principles B1): a block with no frames is skipped, and one whose
    /// presentation time runs back before the block ahead of it (or is missing after one) is
    /// placed at that block's end.
    pub fn accept_block(
        &mut self,
        pts: Option<i64>,
        frames: usize,
        data: BlockData<'_>,
    ) -> Result<(), AudioError> {
        if frames == 0 || data.is_empty() {
            self.metrics.empty_blocks.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let decoded = pts
            .map(|pts| normalize_timestamp(pts, self.timing))
            .transpose()?;
        let placed = self.last_chunk_timestamp.zip(self.last_chunk_end);
        let timestamp = match (decoded, placed) {
            (Some(at), None) => at,
            (Some(at), Some((previous, _))) if at >= previous => at,
            (_, Some((_, end))) => {
                self.metrics
                    .timestamps_clamped
                    .fetch_add(1, Ordering::Relaxed);
                end
            }
            (None, None) => {
                return Err(AudioError::Worker(
                    "the first decoded audio block had no presentation timestamp".to_owned(),
                ))
            }
        };
        self.last_chunk_timestamp = Some(timestamp);
        self.last_chunk_end = Some(NormalizedTimestamp::from_nanoseconds(
            timestamp
                .as_nanoseconds()
                .saturating_add(frames_to_duration(frames, self.sample_rate).as_nanos() as i128),
        ));
        self.block_start = Some(timestamp);
        match (&mut self.pending, data) {
            (PendingAudio::Pcm(pending), BlockData::Pcm(input)) => {
                if let Some(mixer) = &mut self.mixer {
                    self.timeline.push_back((timestamp, frames, 0));
                    self.mixed
                        .extend(mixer.process(input).map_err(AudioError::Worker)?);
                } else {
                    pending.extend_from_slice(input);
                }
            }
            (PendingAudio::Dsd(pending), BlockData::Dsd(input)) => pending.extend_from_slice(input),
            _ => {
                return Err(AudioError::Worker(
                    "a decoded block of the other kind than the session's".to_owned(),
                ))
            }
        }
        self.metrics
            .decoded_media_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
        self.metrics.decoded_chunks.fetch_add(1, Ordering::Relaxed);
        if !self.decoder.dsd {
            // The session's count spans every binding; this decoder's own starts at zero.
            // SAFETY: the decoder this pump owns, live until the pump drops.
            let resampled =
                unsafe { bn_audio_decoder_resampler_input_frames(self.decoder.raw.as_ptr()) };
            let fresh = u64::try_from(resampled - self.resampler_input_seen).unwrap_or(0);
            self.resampler_input_seen = resampled;
            self.metrics
                .resampler_input_frames
                .fetch_add(fresh, Ordering::Relaxed);
        }

        if self.mixer.is_some() {
            self.count_mix();
            self.take_mixed()?;
            return Ok(());
        }
        let (timestamp, frames) = self.run_dsp(timestamp, frames);
        self.place_pending(timestamp, frames)
    }

    /// Runs `pending` — a PCM block placed at `timestamp`, `frames` long — through the DSP chain:
    /// where what comes out stands, and how many frames it is.
    fn run_dsp(
        &mut self,
        timestamp: NormalizedTimestamp,
        frames: usize,
    ) -> (NormalizedTimestamp, usize) {
        let (Some(dsp), PendingAudio::Pcm(samples)) = (&mut self.dsp, &mut self.pending) else {
            return (timestamp, frames);
        };
        let earlier = dsp.process(samples);
        let at = NormalizedTimestamp::from_nanoseconds(
            timestamp.as_nanoseconds()
                - frames_to_duration(earlier, self.sample_rate).as_nanos() as i128,
        );
        self.block_start = Some(at);
        (at, samples.len() / self.channels)
    }

    /// After the decoder's last block: the frames still inside the DSP chain, as one more block
    /// ending where the last one did. `false` when it holds none.
    fn flush_dsp(&mut self) -> Result<bool, AudioError> {
        let Some(dsp) = &mut self.dsp else {
            return Ok(false);
        };
        let samples = dsp.flush();
        let (Some(end), false) = (self.last_chunk_end, samples.is_empty()) else {
            return Ok(false);
        };
        let frames = samples.len() / self.channels;
        let at = NormalizedTimestamp::from_nanoseconds(
            end.as_nanoseconds() - frames_to_duration(frames, self.sample_rate).as_nanos() as i128,
        );
        self.pending = PendingAudio::Pcm(samples);
        self.block_start = Some(at);
        self.place_pending(at, frames)?;
        Ok(true)
    }

    fn count_mix(&mut self) {
        if let Some(mixer) = &self.mixer {
            let next = (
                mixer.frames,
                mixer.hops,
                mixer.limiter_engaged(),
                mixer.cycles,
                mixer.blocks,
                mixer.zeroed,
            );
            self.metrics
                .mix_frames
                .fetch_add(next.0 - self.mix_counted.0, Ordering::Relaxed);
            self.metrics
                .upmix_hops
                .fetch_add(next.1 - self.mix_counted.1, Ordering::Relaxed);
            self.metrics
                .limiter_engaged_frames
                .fetch_add(next.2 - self.mix_counted.2, Ordering::Relaxed);
            self.metrics
                .mix_cycles
                .fetch_add(next.3 - self.mix_counted.3, Ordering::Relaxed);
            self.metrics
                .mix_blocks
                .fetch_add(next.4 - self.mix_counted.4, Ordering::Relaxed);
            self.metrics
                .non_finite_samples
                .fetch_add(next.5 - self.mix_counted.5, Ordering::Relaxed);
            self.mix_counted = next;
        }
    }

    // Keep each decoded block's original media timestamp even when the FFT emits
    // several blocks together. Neither spectral nor limiter delay enters the clock.
    fn take_mixed(&mut self) -> Result<bool, AudioError> {
        let Some(&(timestamp, frames, consumed)) = self.timeline.front() else {
            return Ok(false);
        };
        let take = (frames - consumed).min(self.mixed.len() / self.channels);
        if take == 0 {
            return Ok(false);
        }
        let at = NormalizedTimestamp::from_nanoseconds(
            timestamp.as_nanoseconds()
                + frames_to_duration(consumed, self.sample_rate).as_nanos() as i128,
        );
        self.pending = PendingAudio::Pcm(self.mixed.drain(..take * self.channels).collect());
        if consumed + take == frames {
            self.timeline.pop_front();
        } else {
            if let Some(front) = self.timeline.front_mut() {
                front.2 += take;
            }
        }
        self.block_start = Some(at);
        let (at, take) = self.run_dsp(at, take);
        self.place_pending(at, take)?;
        Ok(true)
    }

    fn place_pending(
        &mut self,
        timestamp: NormalizedTimestamp,
        frames: usize,
    ) -> Result<(), AudioError> {
        let skip = self
            .target
            .map(|target| self.frames_before(timestamp, target, frames))
            .unwrap_or(0);
        if skip == frames {
            self.preroll_frames_discarded =
                self.preroll_frames_discarded.saturating_add(frames as u64);
            self.pending.clear();
            return Ok(());
        }
        self.pending_frame = skip;
        self.preroll_frames_discarded = self.preroll_frames_discarded.saturating_add(skip as u64);
        let start_offset = frames_to_duration(skip, self.sample_rate);
        self.pending_start = Some(NormalizedTimestamp::from_nanoseconds(
            timestamp
                .as_nanoseconds()
                .saturating_add(start_offset.as_nanos() as i128),
        ));
        self.target = None;
        Ok(())
    }
}

pub(super) fn frames_before_target(
    block_start: NormalizedTimestamp,
    target: NormalizedTimestamp,
    sample_rate: NonZeroU32,
    block_frames: usize,
) -> usize {
    let delta = target
        .as_nanoseconds()
        .saturating_sub(block_start.as_nanoseconds());
    if delta <= 0 {
        return 0;
    }
    let numerator = (delta as u128).saturating_mul(u128::from(sample_rate.get()));
    let frames = numerator.div_ceil(1_000_000_000);
    usize::try_from(frames)
        .unwrap_or(usize::MAX)
        .min(block_frames)
}

/// The transport half of an open. Both of its boundaries are generation-checked, so a cancelled
/// attempt costs the bytes already in flight and nothing else — no device, no decoder, no worker.
pub(super) fn open_media_bytes(
    factory: &dyn AudioSourceFactory,
    session: &AudioSession,
    generation: u64,
) -> Result<NativeMediaByteSource, AudioOpenError> {
    session.ensure_live(generation)?;
    let source = factory.open().map_err(AudioOpenError::Source)?;
    session.ensure_live(generation)?;
    Ok(NativeMediaByteSource::new(source))
}

pub(super) fn open_native_audio(
    mut source: NativeMediaByteSource,
    audio_stream_index: i32,
    sample_rate: NonZeroU32,
    channels: u16,
) -> Result<(NativeAudioDecoder, BnAudioStreamInfo, String), AudioOpenError> {
    let descriptor = source.descriptor();
    let mut raw = ptr::null_mut();
    let mut info = BnAudioStreamInfo::default();
    let mut error = NativeMessage::new();
    let mut native_code = 0;
    // SAFETY: the descriptor's source lives in `source` until the decoder owns it; every
    // out-parameter is a live local and the message buffer is given with its capacity.
    let status = unsafe {
        bn_audio_decoder_open_source_stream_with_error(
            &descriptor,
            audio_stream_index,
            i32::try_from(sample_rate.get()).map_err(|_| {
                AudioOpenError::InvalidOutputConfig(
                    "cpal output sample rate exceeds the native ABI".to_owned(),
                )
            })?,
            i32::from(channels),
            &mut raw,
            &mut info,
            error.as_mut_ptr(),
            error.capacity(),
            &mut native_code,
        )
    };
    if status != 0 {
        // The C shim is the ONE validator of "is this index an audio stream" — it holds the
        // container — so the Rust side does not open the resource a second time to pre-check.
        // Its refusal is translated back into the taxonomy the caller asked in.
        if status == BN_AUDIO_INVALID_STREAM {
            if let Some(stream_index) = AudioStreamIndex::new(audio_stream_index) {
                return Err(AudioOpenError::SelectedTrackUnavailable { stream_index });
            }
        }
        return Err(AudioOpenError::Native(NativeError {
            operation: "open",
            status,
            transport: native_code,
            message: error.message(),
        }));
    }
    let decoder = NativeAudioDecoder {
        raw: NonNull::new(raw).ok_or_else(|| {
            AudioOpenError::Native(NativeError {
                operation: "open",
                status: 0,
                transport: 0,
                message: "the shim returned no decoder".to_owned(),
            })
        })?,
        _source: source,
        dsd: false,
    };
    // Before the first read: it sizes how far ahead the sound's input is read.
    // SAFETY: the decoder just opened, owned by `decoder`.
    unsafe {
        bn_audio_decoder_set_source_local(decoder.raw.as_ptr(), decoder._source.native_locality())
    };
    // SAFETY: the decoder just opened; the name, when not null, is a NUL-terminated string the
    // decoder owns, copied here before anything else touches the decoder.
    let codec = unsafe { crate::c_text(bn_audio_decoder_codec_name(decoder.raw.as_ptr())) }
        .unwrap_or_else(|| "unknown".to_owned());
    Ok((decoder, info, codec))
}
