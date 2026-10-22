//! Fixed Auto mixing, owned exclusively by the decode worker, before the ring.
use super::mix_engine::UpmixEngine;
use crate::dsp::PeakLimiter;
use std::num::NonZeroU32;
use std::time::Duration;

/// The speakers' ceiling after mixing: -1 dBFS, room for the reconstruction of an upmix.
pub(super) const SPEAKER_CEILING: f32 = 0.891_250_9;
/// How far the speakers' limiter looks ahead, so it is already down when a peak arrives.
const SPEAKER_LOOKAHEAD: Duration = Duration::from_micros(1_500);
use std::collections::VecDeque;

pub(super) const FL: u64 = 1;
pub(super) const FR: u64 = 2;
pub(super) const FC: u64 = 4;
pub(super) const LFE: u64 = 8;
pub(super) const BL: u64 = 16;
pub(super) const BR: u64 = 32;
const FLC: u64 = 64;
const FRC: u64 = 128;
const BC: u64 = 256;
pub(super) const SL: u64 = 512;
pub(super) const SR: u64 = 1024;
const TC: u64 = 2048;
const TFL: u64 = 4096;
const TFC: u64 = 8192;
const TFR: u64 = 16384;
const TBL: u64 = 32768;
const TBC: u64 = 65536;
const TBR: u64 = 131072;
const TSL: u64 = 1 << 36;
const TSR: u64 = 1 << 37;
const LFE2: u64 = 1 << 35;
pub(super) const STEREO_MASK: u64 = FL | FR;
pub(super) const FIVE_ONE_MASK: u64 = FL | FR | FC | LFE | SL | SR;
pub(super) const SEVEN_ONE_MASK: u64 = FIVE_ONE_MASK | BL | BR;
pub(super) const HOP: usize = 2048;
const Q: f32 = std::f32::consts::FRAC_1_SQRT_2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChannelLayout {
    mask: u64,
    channels: u16,
    assumed: bool,
}
impl ChannelLayout {
    /// Its speakers' positions are known (a mask), so channels go by position, not by index.
    fn positions_known(self) -> bool {
        self.mask != 0
    }

    /// It carries both the side pair and the back pair.
    fn has_sides_and_backs(self) -> bool {
        self.mask & (SL | SR | BL | BR) == SL | SR | BL | BR
    }

    pub fn new(mask: u64, channels: u16, assumed: bool) -> Result<Self, String> {
        if channels == 0
            || channels > 64
            || (mask != 0 && mask.count_ones() != u32::from(channels))
            || (mask == 0 && !assumed)
        {
            return Err("invalid channel layout".into());
        }
        Ok(Self {
            mask,
            channels,
            assumed,
        })
    }
    /// Endpoint fallback only. Unknown source positions MUST remain mask=0.
    pub(super) fn from_channels(channels: u16) -> Result<Self, String> {
        let mask = match channels {
            1 => FC,
            2 => STEREO_MASK,
            3 => STEREO_MASK | FC,
            4 => FL | FR | BL | BR,
            5 => FL | FR | FC | BL | BR,
            6 => FL | FR | FC | LFE | BL | BR,
            7 => FIVE_ONE_MASK | BC,
            8 => SEVEN_ONE_MASK,
            _ => 0,
        };
        Self::new(mask, channels, true)
    }
    pub fn mask(self) -> u64 {
        self.mask
    }
    pub fn channels(self) -> usize {
        self.channels as usize
    }
    pub fn assumed(self) -> bool {
        self.assumed
    }
    pub(super) fn bits(self) -> Vec<u64> {
        (0..64)
            .map(|i| 1u64 << i)
            .filter(|b| self.mask & b != 0)
            .collect()
    }
    pub(super) fn index(self, bit: u64) -> Option<usize> {
        (self.mask & bit != 0).then(|| (self.mask & (bit - 1)).count_ones() as usize)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MixAction {
    Passthrough,
    Route,
    Downmix,
    Upmix,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MixReport {
    pub source: ChannelLayout,
    pub output: ChannelLayout,
    pub action: MixAction,
}

/// Exact surround output whitelist, intersected with the Auto policy (>=4 speakers).
pub(super) const UPMIX_OUTPUTS: &[u64] = &[
    FL | FR | FC | LFE,
    FL | FR | BL | BR,
    FL | FR | FC | BC,
    FL | FR | FC | LFE | BC,
    FL | FR | FC | SL | SR,
    FIVE_ONE_MASK,
    FL | FR | FC | BL | BR,
    FL | FR | FC | LFE | BL | BR,
    FL | FR | FC | BC | SL | SR,
    FL | FR | FC | LFE | BC | SL | SR,
    FL | FR | FC | BL | BR | SL | SR,
    SEVEN_ONE_MASK,
    FL | FR | FC | BL | BR | BC | SL | SR,
];

/// The upmix graph's first hop is skipped at the start: its latency. Nothing else is.
fn start_skip(action: MixAction) -> usize {
    if action == MixAction::Upmix {
        HOP
    } else {
        0
    }
}

pub(super) fn plan_mix(source: ChannelLayout, output: ChannelLayout) -> MixAction {
    if !source.positions_known() || !output.positions_known() {
        return MixAction::Route;
    }
    if source.mask == output.mask {
        return MixAction::Passthrough;
    }
    if output.channels() >= 4
        && [STEREO_MASK, STEREO_MASK | LFE].contains(&source.mask)
        && UPMIX_OUTPUTS.contains(&output.mask)
    {
        return MixAction::Upmix;
    }
    // A sole side/back pair is the same speaker role at a different azimuth.
    let canonical = |m: u64| {
        (m & !(BL | BR)) | if m & BL != 0 { SL } else { 0 } | if m & BR != 0 { SR } else { 0 }
    };
    if source.channels <= output.channels
        && canonical(source.mask) & !canonical(output.mask) == 0
        && !source.has_sides_and_backs()
    {
        return MixAction::Route;
    }
    if source.mask & !output.mask != 0 {
        MixAction::Downmix
    } else {
        MixAction::Route
    }
}

/// Sparse, precomputed ITU/Lo-Ro matrix v1. No per-sample role lookup or allocation.
pub(super) struct Matrix {
    rows: Vec<Vec<(usize, f32)>>,
    inputs: usize,
}
impl Matrix {
    pub(super) fn new(source: ChannelLayout, output: ChannelLayout, action: MixAction) -> Self {
        let mut rows = vec![Vec::new(); output.channels()];
        let mut put = |dest: u64, input: usize, gain: f32| {
            if let Some(i) = output.index(dest) {
                rows[i].push((input, gain));
            }
        };
        if !source.positions_known() || !output.positions_known() {
            for (i, row) in rows.iter_mut().enumerate().take(source.channels()) {
                row.push((i, 1.0));
            }
            return Self {
                rows,
                inputs: source.channels(),
            };
        }
        let has = |b| output.mask & b != 0;
        let left_surround = if has(SL) {
            SL
        } else if has(BL) {
            BL
        } else {
            FL
        };
        let right_surround = if has(SR) {
            SR
        } else if has(BR) {
            BR
        } else {
            FR
        };
        let fold_pairs = source.has_sides_and_backs() && !(has(SL) && has(BL));
        for (i, bit) in source.bits().into_iter().enumerate() {
            if source.mask == FC {
                put(FL, i, Q);
                put(FR, i, Q);
                continue;
            }
            if action != MixAction::Downmix {
                let dest = if has(bit) {
                    bit
                } else {
                    match bit {
                        BL => SL,
                        BR => SR,
                        SL => BL,
                        SR => BR,
                        _ => bit,
                    }
                };
                put(dest, i, 1.0);
                continue;
            }
            // The fixed downmix discards LFE; it never creates or redirects bass.
            if bit == LFE || bit == LFE2 {
                continue;
            }
            if output.mask == FC {
                put(
                    FC,
                    i,
                    match bit {
                        FC => 1.0,
                        FL | FR | SL | SR | BL | BR | TFL | TFR | TBL | TBR | TSL | TSR => 0.5,
                        _ => Q,
                    },
                );
                continue;
            }
            match bit {
                SL | BL | SR | BR => {
                    let dest = if bit == SL || bit == BL {
                        left_surround
                    } else {
                        right_surround
                    };
                    let gain = if has(SL) && has(BL) {
                        1.0
                    } else if fold_pairs {
                        Q * if dest == FL || dest == FR { Q } else { 1.0 }
                    } else if dest == FL || dest == FR {
                        Q
                    } else {
                        1.0
                    };
                    put(if has(SL) && has(BL) { bit } else { dest }, i, gain);
                }
                FC if !has(FC) => {
                    put(FL, i, Q);
                    put(FR, i, Q);
                }
                BC if !has(BC) => {
                    put(left_surround, i, Q);
                    put(right_surround, i, Q);
                }
                FLC if !has(FLC) => put(FL, i, Q),
                FRC if !has(FRC) => put(FR, i, Q),
                TFL | TFR | TBL | TBR | TSL | TSR | TFC | TC | TBC if !has(bit) => match bit {
                    TFL => put(FL, i, Q),
                    TFR => put(FR, i, Q),
                    TBL => put(if has(BL) { BL } else { left_surround }, i, Q),
                    TBR => put(if has(BR) { BR } else { right_surround }, i, Q),
                    TSL => put(left_surround, i, Q),
                    TSR => put(right_surround, i, Q),
                    TBC if left_surround != FL => {
                        put(left_surround, i, 0.5);
                        put(right_surround, i, 0.5);
                    }
                    _ if has(FC) => put(FC, i, Q),
                    _ => {
                        put(FL, i, Q);
                        put(FR, i, Q);
                    }
                },
                _ => put(bit, i, 1.0),
            }
        }
        Self {
            rows,
            inputs: source.channels(),
        }
    }
    pub(super) fn render(&self, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::with_capacity(input.len() / self.inputs * self.rows.len());
        for frame in input.chunks_exact(self.inputs) {
            for row in &self.rows {
                out.push(match row.as_slice() {
                    [] => 0.0,
                    &[(i, 1.0)] => frame[i],
                    _ => row.iter().map(|&(i, g)| frame[i] * g).sum(),
                });
            }
        }
        out
    }
}

fn speaker_limiter(rate: u32) -> PeakLimiter {
    let rate = NonZeroU32::new(rate).unwrap_or(NonZeroU32::MIN);
    PeakLimiter::new(SPEAKER_CEILING, SPEAKER_LOOKAHEAD, rate)
}

pub(super) struct MixStage {
    report: MixReport,
    rate: u32,
    matrix: Matrix,
    engine: Option<UpmixEngine>,
    limiter: PeakLimiter,
    /// Frames of the limiter's lead-in still to drop: the mixer hands back no silence for it.
    lead: usize,
    queued: VecDeque<f32>,
    lfe: VecDeque<f32>,
    received: usize,
    emitted: usize,
    skip: usize,
    ended: bool,
    pub(super) frames: u64,
    pub(super) hops: u64,
    pub(super) cycles: u64,
    pub(super) blocks: u64,
    /// Non-finite input samples (NaN, ±∞) played as silence instead.
    pub(super) zeroed: u64,
}
impl MixStage {
    pub(super) fn new(
        source: ChannelLayout,
        output: ChannelLayout,
        rate: u32,
    ) -> Result<Self, String> {
        if rate == 0 || output.channels() < 4 {
            return Err("mixer requires a multichannel PCM output".into());
        }
        let action = plan_mix(source, output);
        let engine = if action == MixAction::Upmix {
            Some(UpmixEngine::new(output, rate)?)
        } else {
            None
        };
        let limiter = speaker_limiter(rate);
        Ok(Self {
            report: MixReport {
                source,
                output,
                action,
            },
            rate,
            matrix: Matrix::new(source, output, action),
            engine,
            lead: limiter.latency(),
            limiter,
            queued: VecDeque::new(),
            lfe: VecDeque::new(),
            received: 0,
            emitted: 0,
            skip: start_skip(action),
            ended: false,
            frames: 0,
            hops: 0,
            cycles: 0,
            blocks: 0,
            zeroed: 0,
        })
    }
    pub(super) fn report(&self) -> MixReport {
        self.report
    }
    pub(super) fn stage_delay_frames(&self) -> u32 {
        (self.hold() + self.limiter.latency()) as u32
    }
    fn hold(&self) -> usize {
        if UPMIX_OUTPUTS.contains(&self.report.output.mask) {
            HOP
        } else {
            0
        }
    }
    pub(super) fn input_channels(&self) -> usize {
        self.report.source.channels()
    }
    pub(super) fn limiter_engaged(&self) -> u64 {
        self.limiter.engaged_frames
    }
    pub(super) fn reset(&mut self) {
        // Graph state cannot survive a seek. Recreate lazily on the worker's next block.
        self.engine = None;
        self.queued.clear();
        self.lfe.clear();
        self.received = 0;
        self.emitted = 0;
        self.skip = start_skip(self.report.action);
        self.ended = false;
        self.limiter.reset();
        self.lead = self.limiter.latency();
    }
    /// A block was mixed: its CPU cycles since `before`, and itself, are counted.
    fn count_block(&mut self, before: u64) {
        self.cycles += super::mix_engine::thread_cycles().saturating_sub(before);
        self.blocks += 1;
    }

    pub(super) fn process(&mut self, input: &[f32]) -> Result<Vec<f32>, String> {
        let before = super::mix_engine::thread_cycles();
        if self.ended || !input.len().is_multiple_of(self.input_channels()) {
            return Err("mixer input of a partial frame or after its end".into());
        }
        // A damaged stream can decode to NaN or infinity. Those samples play as silence and are
        // counted; the session goes on (principles B1).
        let zeroed = input.iter().filter(|x| !x.is_finite()).count();
        let cleaned: Vec<f32>;
        let input = if zeroed == 0 {
            input
        } else {
            self.zeroed += zeroed as u64;
            cleaned = input
                .iter()
                .map(|&x| if x.is_finite() { x } else { 0.0 })
                .collect();
            &cleaned
        };
        let frames = input.len() / self.input_channels();
        self.received += frames;
        self.frames += frames as u64;
        if self.report.action == MixAction::Upmix {
            let engine = match self.engine.take() {
                Some(engine) => engine,
                None => UpmixEngine::new(self.report.output, self.rate)?,
            };
            let mut stereo = Vec::with_capacity(frames * 2);
            for frame in input.chunks_exact(self.input_channels()) {
                stereo.extend_from_slice(&frame[..2]);
                self.lfe
                    .push_back(self.report.source.index(LFE).map_or(0.0, |i| frame[i]));
            }
            let raw = self.engine.insert(engine).push(&stereo)?;
            self.accept(raw);
        } else {
            self.queued.extend(self.matrix.render(input));
        }
        let out = self.drain(false);
        self.count_block(before);
        Ok(out)
    }
    fn accept(&mut self, raw: Vec<f32>) {
        let channels = self.report.output.channels();
        self.hops += (raw.len() / channels).div_ceil(HOP) as u64;
        for frame in raw.chunks_exact(channels) {
            if self.skip != 0 {
                self.skip -= 1;
                continue;
            }
            if self.emitted + self.queued.len() / channels >= self.received {
                break;
            }
            let lfe = self.lfe.pop_front().unwrap_or(0.0);
            for (i, &v) in frame.iter().enumerate() {
                self.queued
                    .push_back(if Some(i) == self.report.output.index(LFE) {
                        lfe
                    } else {
                        v
                    });
            }
        }
    }
    fn drain(&mut self, eof: bool) -> Vec<f32> {
        let channels = self.report.output.channels();
        let held = if eof || self.report.action == MixAction::Upmix {
            0
        } else {
            self.hold() * channels
        };
        let mut raw: Vec<_> = self
            .queued
            .drain(..self.queued.len().saturating_sub(held))
            .collect();
        self.emitted += raw.len() / channels;
        if eof {
            // Silence after the last frame pushes out what the limiter holds.
            raw.resize(raw.len() + self.limiter.latency() * channels, 0.0);
        }
        for frame in raw.chunks_exact_mut(channels) {
            self.limiter.frame(frame);
        }
        let lead = self.lead.min(raw.len() / channels);
        self.lead -= lead;
        raw.drain(..lead * channels);
        raw
    }
    pub(super) fn flush(&mut self) -> Result<Vec<f32>, String> {
        if self.ended {
            return Ok(Vec::new());
        }
        let before = super::mix_engine::thread_cycles();
        self.ended = true;
        if let Some(engine) = self.engine.as_mut() {
            // One hop restores the real tail. EOF also releases the final partial hop.
            let mut raw = engine.push(&vec![0.0; HOP * 2])?;
            raw.extend(engine.flush()?);
            self.accept(raw);
        }
        let out = self.drain(true);
        self.count_block(before);
        if self.emitted != self.received {
            return Err("mixer EOF did not preserve the frame count".into());
        }
        Ok(out)
    }
}
