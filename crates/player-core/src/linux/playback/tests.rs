use super::*;
use crate::dsp::{song_chain, ReplayGainSetting};
use crate::ReplayGainTags;

fn samples(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|sample| f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]))
        .collect()
}

fn full_volume() -> Arc<crate::dsp::UserGain> {
    Arc::new(crate::dsp::UserGain::default())
}

fn plain_bytes(samples: &[f32]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect()
}

/// The volume moves per frame, as on Windows. Thrown between full and a
/// fifth on every block, neighbouring samples still differ by less than a smoothed step (the old
/// per-buffer scale stepped by 0.4 here), and held, it settles on the factor it was given.
#[test]
fn a_volume_sweep_glides_without_zipper_steps() {
    let dsp = Arc::new(Mutex::new(song_chain(
        Arc::new(ReplayGainSetting::default()),
        ReplayGainTags::default(),
        CHAIN_RATE,
        CHAIN_CHANNELS,
    )));
    let volume = full_volume();
    let mut processing = Processing::new(dsp, None, volume.clone());
    let block = plain_bytes(&[0.5; 2 * 64]);
    let mut rendered = Vec::new();
    for step in 0..160 {
        let level: f32 = if step % 2 == 0 { 0.2 } else { 1.0 };
        volume.set(level);
        let out = samples(processing.block(&block, || false));
        rendered.extend(out.chunks_exact(2).map(|frame| frame[0]));
    }
    let largest = rendered
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .fold(0.0, f32::max);
    assert!(
        largest < 0.001,
        "a step of {largest} between neighbouring samples"
    );
    volume.set(0.2);
    let settled = samples(processing.block(&plain_bytes(&[0.5; 2 * 4_800]), || false));
    assert!((settled[settled.len() - 1] - 0.1).abs() < 1e-4);
}
