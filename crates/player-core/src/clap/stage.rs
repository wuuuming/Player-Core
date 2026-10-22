//! The chain's CLAP effects as one [`DspStage`]: each effect in order, its latency kept even when
//! it is bypassed, has failed, or is busy on another thread (its input then comes out as late as
//! its output would have).

use super::instance::Instance;
use super::Effects;
use crate::dsp::DspStage;
use std::collections::VecDeque;
use std::sync::Arc;

struct Slot {
    instance: Arc<Instance>,
    /// The plugin was active at the stage's rate when the stage took it; otherwise its input
    /// passes through until the next reset.
    usable: bool,
    latency: usize,
    /// The last `latency` frames of input, every channel.
    line: VecDeque<f32>,
    bypassed: bool,
}

impl Slot {
    /// Delays `samples` by `latency` frames in place.
    fn delay(&mut self, samples: &mut [f32]) {
        if self.latency == 0 {
            return;
        }
        for sample in samples.iter_mut() {
            self.line.push_back(*sample);
            *sample = self.line.pop_front().unwrap_or(0.0);
        }
    }
}

pub struct EffectsStage {
    effects: Effects,
    rate: u32,
    channels: usize,
    /// The structure the slots were taken at.
    seen: u64,
    slots: Vec<Slot>,
    input: Vec<f32>,
}

impl EffectsStage {
    pub(super) fn new(effects: Effects, rate: u32, channels: usize) -> Self {
        let mut stage = Self {
            effects,
            rate,
            channels,
            seen: u64::MAX,
            slots: Vec::new(),
            input: Vec::new(),
        };
        stage.take_chain();
        stage
    }

    /// Takes the chain as it is now: its effects, their latencies, empty delay lines.
    fn take_chain(&mut self) {
        let (seen, instances) = self.effects.snapshot();
        self.seen = seen;
        self.slots = instances
            .into_iter()
            .map(|instance| {
                let usable = instance.runs_at(self.rate);
                let latency = if usable { instance.latency() } else { 0 };
                Slot {
                    bypassed: instance.bypass.load(std::sync::atomic::Ordering::Relaxed),
                    instance,
                    usable,
                    latency,
                    line: vec![0.0; latency * self.channels].into(),
                }
            })
            .collect();
    }
}

impl DspStage for EffectsStage {
    fn process(&mut self, samples: &mut [f32], channels: usize) {
        for slot in &mut self.slots {
            let bypassed = slot
                .instance
                .bypass
                .load(std::sync::atomic::Ordering::Relaxed);
            if slot.bypassed && !bypassed {
                // What it heard before the bypass is not what comes next.
                slot.instance.reset();
            }
            slot.bypassed = bypassed;
            if !slot.usable || bypassed || slot.instance.failed() {
                slot.delay(samples);
                continue;
            }
            self.input.clear();
            self.input.extend_from_slice(samples);
            slot.delay(samples);
            slot.instance
                .process(&self.input, samples, channels, self.rate);
        }
    }

    /// A seek: every effect forgets what it heard; a chain changed since is taken up now, with
    /// the latency it has (the chain reads it after this).
    fn reset(&mut self) {
        if self.stale() {
            self.take_chain();
        } else {
            for slot in &mut self.slots {
                slot.line.iter_mut().for_each(|sample| *sample = 0.0);
            }
        }
        for slot in &self.slots {
            slot.instance.reset();
        }
    }

    fn latency(&self) -> usize {
        self.slots.iter().map(|slot| slot.latency).sum()
    }

    /// The chain was changed since its slots were taken: an effect added, removed or moved.
    fn stale(&self) -> bool {
        self.effects.structure() != self.seen
    }
}
