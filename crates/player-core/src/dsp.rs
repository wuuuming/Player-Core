//! Sound processing between the decoder and the output, the same chains on every platform: on
//! Windows the decode worker runs them between the pump's output and the ring, on Linux the
//! Pulse writer between FFmpeg's pipe and the stream.
//!
//! Two chains, by what they belong to. A song's own ([`song_chain`]: its ReplayGain) runs on that
//! song alone and ends with it. The stream's ([`stream_chain`]: the host's CLAP effects) runs on
//! every song that plays in the stream, one after another, and goes on across their boundaries as
//! it would within one song: at a boundary it is only renewed when its stages changed
//! ([`DspChain::renew`]).
//!
//! A [`DspStage`] works on interleaved `f32` PCM at the output's rate and channels. Neither thread
//! is the device's real-time one, but each is what keeps the output fed: a stage does not block
//! and does not wait. The chain hands each stage at most [`DSP_MAX_BLOCK_FRAMES`] frames at a
//! time, whatever size the decoder's blocks come in; DSD never reaches it.
//!
//! A stage whose output lags its input says so (`latency`); the chain drops that many frames of
//! lead-in after every reset, and flushes the frames still inside it at the decoder's end, so what
//! leaves the chain stands at the media time of what went in.

use crate::{ReplayGain, ReplayGainTags};
use std::collections::VecDeque;
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// A declick: how long a sound stopped while it sounds (a pause, a seek) ramps, on both
/// platforms (Windows: the provisional policy's `ramp`).
pub(crate) const DECLICK: Duration = Duration::from_millis(8);
/// How long the song a player leaves for another fades out (Windows: the provisional policy's
/// `leave`).
pub(crate) const LEAVE: Duration = Duration::from_millis(100);

/// The user's volume, a factor of the sound (0 to 1; NaN is 0), full at first; set from any
/// thread, read where the sound is made (both platforms).
#[derive(Debug)]
pub struct UserGain {
    bits: std::sync::atomic::AtomicU32,
}

impl Default for UserGain {
    fn default() -> Self {
        Self {
            bits: std::sync::atomic::AtomicU32::new(1.0f32.to_bits()),
        }
    }
}

impl UserGain {
    pub fn set(&self, volume: f32) {
        let volume = if volume.is_nan() {
            0.0
        } else {
            volume.clamp(0.0, 1.0)
        };
        self.bits.store(volume.to_bits(), Ordering::Relaxed);
    }

    pub fn get(&self) -> f32 {
        f32::from_bits(self.bits.load(Ordering::Relaxed))
    }
}

/// The largest block a stage is handed.
pub const DSP_MAX_BLOCK_FRAMES: usize = 1_024;

pub trait DspStage: Send {
    /// Processes whole interleaved frames of `channels` samples in place; at most
    /// [`DSP_MAX_BLOCK_FRAMES`] of them.
    fn process(&mut self, samples: &mut [f32], channels: usize);
    /// Forgets what it has heard: the next block does not follow the last (a seek).
    fn reset(&mut self);
    /// How many frames its output lags its input; read when the chain is made and after each
    /// reset (a stage may take a new latency only there).
    fn latency(&self) -> usize {
        0
    }
    /// Whether what it was made from has changed since it was made or last reset (an effect
    /// added, removed or moved): a stream's chain is then renewed at the next song's boundary.
    fn stale(&self) -> bool {
        false
    }
}

/// The stages a song or a stream runs, in order, with the latency they add up to.
pub struct DspChain {
    stages: Vec<Box<dyn DspStage>>,
    channels: usize,
    latency: usize,
    /// Frames given to the chain since the last reset, up to `latency`: how many of its input
    /// frames have not come out yet.
    held: usize,
}

/// A chain's latency: its stages' together.
fn latency_of(stages: &[Box<dyn DspStage>]) -> usize {
    stages.iter().map(|stage| stage.latency()).sum()
}

impl DspChain {
    pub fn new(stages: Vec<Box<dyn DspStage>>, channels: NonZeroUsize) -> Self {
        let latency = latency_of(&stages);
        Self {
            stages,
            channels: channels.get(),
            latency,
            held: 0,
        }
    }

    pub fn reset(&mut self) {
        for stage in &mut self.stages {
            stage.reset();
        }
        self.latency = latency_of(&self.stages);
        self.held = 0;
    }

    fn run(&mut self, samples: &mut [f32]) {
        for block in samples.chunks_mut(DSP_MAX_BLOCK_FRAMES * self.channels) {
            for stage in &mut self.stages {
                stage.process(block, self.channels);
            }
        }
    }

    /// Runs `samples` through every stage and drops the lead-in the latency put before the sound.
    /// Returns how many frames before the input's first the output's first stands: the frames the
    /// chain held from earlier blocks.
    pub fn process(&mut self, samples: &mut Vec<f32>) -> usize {
        self.run(samples);
        let earlier = self.held;
        let frames = samples.len() / self.channels;
        let lead_in = (self.latency - earlier).min(frames);
        samples.drain(..lead_in * self.channels);
        self.held += lead_in;
        earlier
    }

    /// How many of the frames given have not come out yet.
    pub fn held(&self) -> usize {
        self.held
    }

    /// Whether a stage changed since it was made or last reset.
    pub fn stale(&self) -> bool {
        self.stages.iter().any(|stage| stage.stale())
    }

    /// At a song's boundary in a stream: the chain goes on into the next song as it is, unless a
    /// stage changed since it was made; then the frames still inside it — the end of the song
    /// before — are flushed out and returned, and it is reset, taking up the change.
    pub fn renew(&mut self) -> Vec<f32> {
        if !self.stale() {
            return Vec::new();
        }
        let tail = self.flush();
        self.reset();
        tail
    }

    /// The frames still inside the chain, pushed out with silence after the decoder's last block;
    /// empty when there are none. They stand right before the end of the last block given.
    pub fn flush(&mut self) -> Vec<f32> {
        let held = std::mem::take(&mut self.held);
        if held == 0 {
            return Vec::new();
        }
        let mut samples = vec![0.0; self.latency * self.channels];
        self.run(&mut samples);
        // Fewer than `latency` frames went in: the first ones out are still the lead-in.
        let lead_in = self.latency - held;
        samples.drain(..lead_in * self.channels);
        samples.truncate(held * self.channels);
        samples
    }
}

/// A song's own chain: its ReplayGain, at the session's `setting`.
pub fn song_chain(
    setting: Arc<ReplayGainSetting>,
    tags: ReplayGainTags,
    rate: NonZeroU32,
    channels: NonZeroUsize,
) -> DspChain {
    let replay_gain = ReplayGainStage::new(setting, tags, rate);
    DspChain::new(vec![Box::new(replay_gain)], channels)
}

/// The stream's chain, which every song in it goes through: the host's CLAP `effects`, activated
/// at `rate` (blocking while the plugin host thread does it). `None` without effects.
pub fn stream_chain(
    effects: Option<&crate::clap::Effects>,
    rate: NonZeroU32,
    channels: NonZeroUsize,
) -> Option<DspChain> {
    let effects = effects?;
    let stage: Box<dyn DspStage> = Box::new(effects.stage(rate, channels.get()));
    Some(DspChain::new(vec![stage], channels))
}

/// How one song gives way to the next over a span of the stream (a crossfade): the going song's
/// sound and the coming one's summed under equal-power gains, the going one's falling from full to
/// nothing as the coming one's rises, so the loudness holds through the middle. Both come already
/// through their own chains ([`song_chain`]); the stream's chain runs on the sum. With no span the
/// coming song simply follows the going one's last frame (gapless), and there is nothing to sum.
pub(crate) struct Crossfade {
    frames: usize,
    done: usize,
}

impl Crossfade {
    pub fn new(frames: usize) -> Self {
        Self { frames, done: 0 }
    }

    /// Frames of the span still to sum.
    pub fn left(&self) -> usize {
        self.frames - self.done
    }

    /// The gains of the span's frame `at`: the going song's and the coming one's. Their squares
    /// add up to one.
    pub fn gains(&self, at: usize) -> (f32, f32) {
        let angle = (at as f64 + 0.5) / self.frames as f64 * std::f64::consts::FRAC_PI_2;
        (angle.cos() as f32, angle.sin() as f32)
    }

    /// Sums `going` into `coming`, whole interleaved frames of `channels`, from where the span
    /// stands. Where `going` runs out before the span does, the going song has ended: the coming
    /// one goes on rising alone. Frames of `coming` past the span's end are left as they are.
    pub fn mix(&mut self, coming: &mut [f32], going: &[f32], channels: usize) {
        for (index, frame) in coming.chunks_exact_mut(channels).enumerate() {
            if self.done == self.frames {
                return;
            }
            let (out, into) = self.gains(self.done);
            let going = going.get(index * channels..(index + 1) * channels);
            for (channel, sample) in frame.iter_mut().enumerate() {
                *sample = *sample * into + going.map_or(0.0, |going| going[channel] * out);
            }
            self.done += 1;
        }
    }
}

/// A one-pole step toward `target`: `coefficient` of the way there per frame, landing on it when
/// the rest is inaudible (a thousandth of the target). No step is a jump, so a change of gain is
/// never a click. The landing is also what ends it: in `f32` a small enough remainder times the
/// coefficient rounds to nothing, and the step would stall short of its target for good.
pub fn one_pole(current: f32, target: f32, coefficient: f32) -> f32 {
    let next = current + (target - current) * coefficient;
    if (target - next).abs() <= target.abs() * 1e-3 + 1e-7 {
        target
    } else {
        next
    }
}

/// The coefficient of a one-pole step whose time constant is `time` at `sample_rate`.
pub fn one_pole_coefficient(time: Duration, sample_rate: NonZeroU32) -> f32 {
    let frames = time.as_secs_f32() * sample_rate.get() as f32;
    if frames <= 1.0 {
        1.0
    } else {
        1.0 - (-1.0 / frames).exp()
    }
}

/// How quickly the user's volume follows its slider.
pub(crate) const USER_GAIN_SMOOTHING: Duration = Duration::from_millis(10);
/// How quickly a gain moves to a new value (a setting changed while playing).
const GAIN_SMOOTHING: Duration = Duration::from_millis(50);
/// How quickly the limiter lets go once the peak that engaged it has passed.
const LIMITER_RELEASE: Duration = Duration::from_millis(100);
/// Full scale: the ceiling after ReplayGain's boost.
pub(crate) const FULL_SCALE: f32 = 1.0;

/// A linked peak limiter: every frame is scaled so that no sample passes `ceiling`, by the
/// smallest of the gains the frames still ahead of it within `lookahead` will need, and the
/// reduction relaxes back over [`LIMITER_RELEASE`]. The channels keep their ratios. Looking ahead
/// delays the sound by as many frames ([`Self::latency`]), and the reduction starts before the
/// peak instead of on it; with none it costs no delay. Idle (nothing above the ceiling, nothing to
/// relax) it leaves every sample as it is.
pub(crate) struct PeakLimiter {
    ceiling: f32,
    lookahead: usize,
    release: f32,
    reduction: f32,
    /// The frames given and not yet handed back, interleaved.
    line: VecDeque<f32>,
    /// The frames' peaks, decreasing from the front: `(frame index, peak)`.
    peaks: VecDeque<(u64, f32)>,
    next: u64,
    /// Frames handed back below unity.
    pub(crate) engaged_frames: u64,
}

impl PeakLimiter {
    pub fn new(ceiling: f32, lookahead: Duration, sample_rate: NonZeroU32) -> Self {
        Self {
            ceiling,
            lookahead: (u128::from(sample_rate.get()) * lookahead.as_micros() / 1_000_000) as usize,
            release: one_pole_coefficient(LIMITER_RELEASE, sample_rate),
            reduction: 1.0,
            line: VecDeque::new(),
            peaks: VecDeque::new(),
            next: 0,
            engaged_frames: 0,
        }
    }

    /// How many frames the output lags the input.
    pub fn latency(&self) -> usize {
        self.lookahead
    }

    /// Nothing held and nothing to relax: a frame given now comes back as it is, unless it is
    /// itself past the ceiling.
    pub fn idle(&self) -> bool {
        self.reduction == 1.0 && self.line.is_empty()
    }

    pub fn reset(&mut self) {
        self.reduction = 1.0;
        self.line.clear();
        self.peaks.clear();
        self.next = 0;
    }

    /// Takes `frame` and puts in its place the frame [`Self::latency`] frames before it, limited:
    /// silence until that many have been given.
    pub fn frame(&mut self, frame: &mut [f32]) {
        let channels = frame.len();
        let peak = frame
            .iter()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        let peak = if peak.is_finite() { peak } else { 0.0 };
        while self.peaks.back().is_some_and(|&(_, held)| held <= peak) {
            self.peaks.pop_back();
        }
        self.peaks.push_back((self.next, peak));
        self.next += 1;
        self.line.extend(frame.iter().copied());
        if self.line.len() / channels <= self.lookahead {
            frame.fill(0.0);
            return;
        }
        let out = self.next - 1 - self.lookahead as u64;
        while self.peaks.front().is_some_and(|&(index, _)| index < out) {
            self.peaks.pop_front();
        }
        let ahead = self.peaks.front().map_or(0.0, |&(_, peak)| peak);
        let needed = if ahead > self.ceiling {
            self.ceiling / ahead
        } else {
            1.0
        };
        self.reduction = needed.min(one_pole(self.reduction, 1.0, self.release));
        if self.reduction < 1.0 {
            self.engaged_frames += 1;
        }
        for sample in frame.iter_mut() {
            // Whole frames only are held, so one is there.
            *sample = self.line.pop_front().unwrap_or(0.0) * self.reduction;
        }
    }
}

/// The session's ReplayGain setting, shared by every binding's stage and set by the host at any
/// time.
#[derive(Debug, Default)]
pub struct ReplayGainSetting(AtomicU8);

impl ReplayGainSetting {
    pub fn set(&self, mode: ReplayGain) {
        self.0.store(mode.to_bits(), Ordering::Relaxed);
    }

    pub fn get(&self) -> ReplayGain {
        ReplayGain::from_bits(self.0.load(Ordering::Relaxed))
    }
}

/// ReplayGain: the song's gain for the session's setting, above 0 dB as readily as below it,
/// moved to over [`GAIN_SMOOTHING`] when the setting changes, and limited at full scale. At unity
/// with the limiter idle — the setting off, or a song with no values — it does not touch the sound.
pub struct ReplayGainStage {
    setting: Arc<ReplayGainSetting>,
    tags: ReplayGainTags,
    gain: f32,
    smoothing: f32,
    pub(crate) limiter: PeakLimiter,
}

impl ReplayGainStage {
    pub fn new(
        setting: Arc<ReplayGainSetting>,
        tags: ReplayGainTags,
        sample_rate: NonZeroU32,
    ) -> Self {
        let gain = tags.factor(setting.get());
        Self {
            setting,
            tags,
            gain,
            smoothing: one_pole_coefficient(GAIN_SMOOTHING, sample_rate),
            limiter: PeakLimiter::new(FULL_SCALE, Duration::ZERO, sample_rate),
        }
    }
}

impl DspStage for ReplayGainStage {
    fn process(&mut self, samples: &mut [f32], channels: usize) {
        let target = self.tags.factor(self.setting.get());
        if self.gain == 1.0 && target == 1.0 && self.limiter.idle() {
            return;
        }
        for frame in samples.chunks_exact_mut(channels) {
            self.gain = one_pole(self.gain, target, self.smoothing);
            if self.gain != 1.0 {
                for sample in frame.iter_mut() {
                    *sample *= self.gain;
                }
            }
            self.limiter.frame(frame);
        }
    }

    /// A seek starts the sound afresh: at the gain the setting asks for, nothing to relax.
    fn reset(&mut self) {
        self.gain = self.tags.factor(self.setting.get());
        self.limiter.reset();
    }

    fn latency(&self) -> usize {
        self.limiter.latency()
    }
}

/// Where a song's sound ends, as known before it gets there: its stop, else its length. The one
/// rule on both platforms.
pub fn song_end(stop: Option<Duration>, length: Option<Duration>) -> Option<Duration> {
    stop.or(length)
}

/// A position is never past the song's end (`song_end`), when that is known: what is reported
/// and where a seek goes alike, on both platforms.
pub fn within_end(position: Duration, end: Option<Duration>) -> Duration {
    end.map_or(position, |end| position.min(end))
}

/// Where a crossfade of `overlap` into the next song starts: the song's end as known ahead
/// (`song_end`) less the overlap, not before zero; none without an overlap or a known end. Worked
/// out in time on both platforms, then taken into each one's unit.
pub fn handover_at(end: Option<Duration>, overlap: Duration) -> Option<Duration> {
    if overlap.is_zero() {
        return None;
    }
    end.map(|end| end.saturating_sub(overlap))
}

#[cfg(test)]
mod tests;
