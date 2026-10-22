// Oracles: the multichannel design's matrix (v1);
// shipped FFmpeg surround; generated integer PCM has an exact external identity.
use crate::source::NativeMediaByteSource;
use crate::StreamTimeBase;
use crate::{NormalizedTimestamp, StreamTiming};
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(feature = "test-support")]
use super::device::use_null_audio_output;
use super::mix::SPEAKER_CEILING as CEILING;
use super::mix::{
    plan_mix, ChannelLayout, Matrix, MixAction, MixStage, BL, BR, FC, FIVE_ONE_MASK, FL, FR, LFE,
    SEVEN_ONE_MASK, STEREO_MASK, UPMIX_OUTPUTS,
};
use super::pump::{open_native_audio, DecoderPump, SongEnd};
use super::ring::AudioRing;
use super::stream::StreamWriter;
use crate::dsp::PeakLimiter;
use std::path::PathBuf;

fn layout(mask: u64) -> ChannelLayout {
    ChannelLayout::new(mask, mask.count_ones() as u16, false).unwrap()
}
fn stereo() -> ChannelLayout {
    layout(STEREO_MASK)
}
fn run(
    source: ChannelLayout,
    output: ChannelLayout,
    input: &[f32],
    block: usize,
) -> (Vec<f32>, MixStage) {
    let mut stage = MixStage::new(source, output, 48_000).unwrap();
    let mut result = Vec::new();
    for chunk in input.chunks(block * source.channels()) {
        result.extend(stage.process(chunk).unwrap());
    }
    result.extend(stage.flush().unwrap());
    (result, stage)
}

#[test]
fn fixed_decision_table_and_layout_validation() {
    assert!(ChannelLayout::new(FL, 2, false).is_err());
    assert!(ChannelLayout::new(0, 6, false).is_err());
    assert!(MixStage::new(stereo(), stereo(), 48_000).is_err());
    for &(src, dst, action) in &[
        (STEREO_MASK, FIVE_ONE_MASK, MixAction::Upmix),
        (STEREO_MASK | LFE, FL | FR | BL | BR, MixAction::Upmix),
        (FIVE_ONE_MASK, SEVEN_ONE_MASK, MixAction::Route),
        (SEVEN_ONE_MASK, FIVE_ONE_MASK, MixAction::Downmix),
        (FIVE_ONE_MASK, FIVE_ONE_MASK, MixAction::Passthrough),
        (
            FL | FR | FC | LFE | BL | BR,
            FIVE_ONE_MASK,
            MixAction::Route,
        ),
        (FL | FR | FC, FIVE_ONE_MASK, MixAction::Route),
        (FL | FR | BL | BR, FIVE_ONE_MASK, MixAction::Route),
        (FC, FIVE_ONE_MASK, MixAction::Route),
        (
            STEREO_MASK,
            SEVEN_ONE_MASK | (1 << 12) | (1 << 14),
            MixAction::Route,
        ),
    ] {
        assert_eq!(
            plan_mix(layout(src), layout(dst)),
            action,
            "{src:x}->{dst:x}"
        );
    }
    assert_eq!(
        plan_mix(
            ChannelLayout::new(0, 6, true).unwrap(),
            layout(SEVEN_ONE_MASK)
        ),
        MixAction::Route
    );
}

#[test]
fn wasapi_masks_preserve_side_back_and_physical_stereo() {
    let pick = super::endpoints::layout_from_masks;
    assert_eq!(pick(6, FIVE_ONE_MASK, 0).unwrap(), layout(FIVE_ONE_MASK));
    assert_eq!(
        pick(6, FL | FR | FC | LFE | BL | BR, 0).unwrap(),
        layout(FL | FR | FC | LFE | BL | BR)
    );
    assert_eq!(pick(8, SEVEN_ONE_MASK, STEREO_MASK).unwrap(), stereo());
    assert!(pick(6, STEREO_MASK, 0).unwrap().assumed());
    assert!(pick(0, 0, 0).is_err());
}

#[test]
fn shipped_engine_accepts_every_fixed_whitelist_layout() {
    for &mask in UPMIX_OUTPUTS {
        let (out, _) = run(stereo(), layout(mask), &vec![0.0; 4097 * 2], 317);
        assert_eq!(
            out.len(),
            4097 * mask.count_ones() as usize,
            "layout {mask:x}"
        );
    }
}

#[test]
fn itu_impulses_and_seven_to_stereo_product_matrix() {
    let source = layout(SEVEN_ONE_MASK);
    let five = layout(FIVE_ONE_MASK);
    let q = std::f32::consts::FRAC_1_SQRT_2;
    let a = Matrix::new(source, five, MixAction::Downmix);
    let b = Matrix::new(five, stereo(), MixAction::Downmix);
    let direct = Matrix::new(source, stereo(), MixAction::Downmix);
    for i in 0..8 {
        let mut x = vec![0.0; 8];
        x[i] = 1.0;
        let y = a.render(&x);
        let expected = match i {
            0 => vec![1., 0., 0., 0., 0., 0.],
            1 => vec![0., 1., 0., 0., 0., 0.],
            2 => vec![0., 0., 1., 0., 0., 0.],
            3 => vec![0., 0., 0., 0., 0., 0.],
            4 | 6 => vec![0., 0., 0., 0., q, 0.],
            _ => vec![0., 0., 0., 0., 0., q],
        };
        assert_eq!(y, expected);
        for (a, b) in direct.render(&x).iter().zip(b.render(&y)) {
            assert!((a - b).abs() < 1e-6);
        }
    }
    assert_eq!(
        b.render(&[0., 0., 0., 1., 0., 0.]),
        vec![0., 0.],
        "no LFE in mains"
    );
}

#[test]
fn routing_does_not_duplicate_center_or_surrounds() {
    let a = layout(FL | FR | FC | LFE | BL | BR);
    let b = layout(FIVE_ONE_MASK);
    let input = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
    assert_eq!(Matrix::new(a, b, plan_mix(a, b)).render(&input), input);
    let (out, _) = run(b, b, &input, 1);
    assert_eq!(out, input);
    let unknown = ChannelLayout::new(0, 6, true).unwrap();
    assert_eq!(
        Matrix::new(unknown, b, MixAction::Route).render(&input),
        input
    );
}

#[test]
fn mono_and_height_follow_fixed_matrix() {
    let q = std::f32::consts::FRAC_1_SQRT_2;
    assert_eq!(
        Matrix::new(layout(FC), layout(FIVE_ONE_MASK), MixAction::Downmix).render(&[1.]),
        [q, q, 0., 0., 0., 0.]
    );
    let source = layout(SEVEN_ONE_MASK | (1 << 12) | (1 << 14) | (1 << 15) | (1 << 17));
    let mut impulse = vec![0.; 12];
    impulse[8] = 1.;
    assert_eq!(
        Matrix::new(source, layout(SEVEN_ONE_MASK), MixAction::Downmix).render(&impulse),
        [q, 0., 0., 0., 0., 0., 0., 0.]
    );
}

/// The speakers' limiter, as the mixer runs it: the lead-in dropped, the tail pushed out.
fn speakers() -> PeakLimiter {
    PeakLimiter::new(
        CEILING,
        Duration::from_micros(1_500),
        NonZeroU32::new(48_000).unwrap(),
    )
}

fn limit(limiter: &mut PeakLimiter, input: &[f32], channels: usize) -> Vec<f32> {
    let mut out = input.to_vec();
    out.resize(out.len() + limiter.latency() * channels, 0.0);
    for frame in out.chunks_exact_mut(channels) {
        limiter.frame(frame);
    }
    out.drain(..limiter.latency() * channels);
    out
}

#[test]
fn linked_lookahead_bounds_peaks_and_preserves_ratios() {
    let mut input = Vec::new();
    for i in 0..6000 {
        let v = if i % 127 == 0 { 4.0 } else { 0.1 };
        input.extend([v, v * 0.25, -v * 0.5, 0.0]);
    }
    let mut stage = speakers();
    assert_eq!(stage.latency(), 72);
    let out = limit(&mut stage, &input, 4);
    assert_eq!(out.len(), input.len());
    assert!(out.iter().all(|x| x.abs() <= CEILING + 1e-7));
    for frame in out.chunks_exact(4) {
        assert_eq!(frame[1], frame[0] * 0.25);
        assert_eq!(frame[2], -frame[0] * 0.5);
    }
    assert!(stage.engaged_frames > 0);
    let mut quiet = speakers();
    assert_eq!(limit(&mut quiet, &[0.1; 400], 4), vec![0.1; 400]);
    assert_eq!(quiet.engaged_frames, 0);
}

#[test]
fn lookahead_attenuates_before_a_peak_and_never_releases_above_it() {
    let mut stage = speakers();
    let mut input = vec![0.1; 100 * 4];
    input.extend([4.; 4]);
    input.extend(vec![0.1; 200 * 4]);
    let out = limit(&mut stage, &input, 4);
    assert!(out[50 * 4] < 0.1, "future peak must already affect gain");
    assert!(out[100 * 4] <= CEILING);
}

#[test]
fn eof_lengths_lfe_identity_and_idempotent_flush() {
    for frames in [0, 1, 71, 72, 2047, 2048, 2049, 4095, 4096, 4097, 12031] {
        let input: Vec<_> = (0..frames)
            .flat_map(|i| [0., 0., (i as f32 * 0.01).sin() * 0.1])
            .collect();
        let (out, mut stage) = run(layout(STEREO_MASK | LFE), layout(FIVE_ONE_MASK), &input, 73);
        assert_eq!(stage.stage_delay_frames(), 2120);
        assert_eq!(out.len(), frames * 6, "{frames}");
        for i in 0..frames {
            assert_eq!(
                out[i * 6 + 3].to_bits(),
                input[i * 3 + 2].to_bits(),
                "LFE frame {i}/{frames}"
            );
        }
        assert!(stage.flush().unwrap().is_empty());
    }
}

#[test]
fn non_finite_input_plays_as_silence_and_is_counted() {
    let clean: Vec<f32> = (0..4096)
        .flat_map(|i| {
            let x = (i as f32 * 0.02).sin() * 0.2;
            [x, -x]
        })
        .collect();
    let mut broken = clean.clone();
    broken[10] = f32::NAN;
    broken[2001] = f32::INFINITY;
    broken[5000] = f32::NEG_INFINITY;
    let mut zeroed = clean.clone();
    zeroed[10] = 0.;
    zeroed[2001] = 0.;
    zeroed[5000] = 0.;
    let (a, sa) = run(stereo(), layout(FIVE_ONE_MASK), &broken, 512);
    let (b, sb) = run(stereo(), layout(FIVE_ONE_MASK), &zeroed, 512);
    assert_eq!(a, b);
    assert_eq!((sa.zeroed, sb.zeroed), (3, 0));
}

#[test]
fn seek_recreates_graph_without_previous_song_tail() {
    let mut stage = MixStage::new(stereo(), layout(FIVE_ONE_MASK), 48_000).unwrap();
    stage.process(&vec![0.4; 16000]).unwrap();
    stage.reset();
    let mut out = stage.process(&vec![0.; 13000]).unwrap();
    out.extend(stage.flush().unwrap());
    assert_eq!(out, vec![0.; 6500 * 6]);
}

#[test]
fn block_boundaries_do_not_change_pcm_or_hops() {
    let input: Vec<_> = (0..16001)
        .flat_map(|i| {
            let x = (i as f32 * 0.03).sin() * 0.1;
            [x, x * 0.4]
        })
        .collect();
    let (a, sa) = run(stereo(), layout(FIVE_ONE_MASK), &input, 37);
    let (b, sb) = run(stereo(), layout(FIVE_ONE_MASK), &input, 4096);
    assert_eq!(a, b);
    assert_eq!(sa.hops, sb.hops);
}

#[test]
fn impulse_latency_is_absorbed_at_every_hop_phase_including_eof() {
    for at in [0, 1, 72, 1023, 2047, 2048, 4095, 9999] {
        let mut input = vec![0.0; 10000 * 2];
        input[at * 2] = 0.1;
        input[at * 2 + 1] = 0.1;
        let (out, _) = run(stereo(), layout(FIVE_ONE_MASK), &input, 317);
        let peak = out
            .chunks_exact(6)
            .enumerate()
            .max_by(|(_, a), (_, b)| a[2].abs().total_cmp(&b[2].abs()))
            .unwrap()
            .0;
        assert_eq!(peak, at, "compensated impulse");
    }
}

#[test]
fn unknown_multichannel_source_is_discrete_and_swr_does_not_remix() {
    let fixture = Fixture::new(10, 5003);
    let output = layout(SEVEN_ONE_MASK);
    let mut pump = fixture.pump(output, true);
    assert_eq!(pump.mixer.as_ref().unwrap().report().source.mask(), 0);
    let ring = Arc::new(
        AudioRing::new(
            NonZeroUsize::new(1024).unwrap(),
            NonZeroUsize::new(8).unwrap(),
        )
        .unwrap(),
    );
    let out = drain(&mut pump, &ring);
    let expected: Vec<_> = (0..5003)
        .flat_map(|i| (0..8).map(move |ch| Fixture::sample(i, ch) as f32 / 32768.0))
        .collect();
    assert_eq!(out, expected);
    assert_eq!(pump.metrics.mix_frames.load(Ordering::Relaxed), 5003);
}

#[test]
fn surround_pcm_matches_shipped_ffmpeg_console_oracle() {
    let frames = 24031;
    let fixture = Fixture::new(2, frames);
    // The oracle is FFmpeg's own console tool (the audio-only build ships none): from the SDK the
    // build linked when it has one, or NOVA_FFMPEG_EXE; else the test says so and skips.
    let exe = std::env::var_os("NOVA_FFMPEG_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("PLAYER_CORE_FFMPEG_DIR")).join("bin/ffmpeg.exe"));
    if !exe.is_file() {
        eprintln!(
            "skipped: no ffmpeg.exe oracle at {} (set NOVA_FFMPEG_EXE)",
            exe.display()
        );
        return;
    }
    let result=std::process::Command::new(exe)
        .args(["-v","error","-threads","1","-filter_threads","1","-i"]).arg(&fixture.0)
        .args(["-af","apad=pad_len=2048,surround=chl_in=stereo:chl_out=5.1(side):lfe=0:win_size=4096:overlap=0.5:win_func=hann:fc_out=0.7071:angle=90:focus=0:smooth=0.3,atrim=start_sample=2048:end_sample=26079",
            "-f","f32le","pipe:1"]).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let oracle: Vec<_> = result
        .stdout
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    let input: Vec<_> = (0..frames)
        .flat_map(|i| (0..2).map(move |ch| Fixture::sample(i, ch) as f32 / 32768.))
        .collect();
    let (actual, stage) = run(stereo(), layout(FIVE_ONE_MASK), &input, 1024);
    assert_eq!(actual.len(), oracle.len());
    assert!(actual.iter().zip(oracle).all(|(a, b)| (a - b).abs() < 1e-6));
    eprintln!("mch[cost] frames={} blocks={} hops={} thread_cycles={} stage_delay_frames={} limiter_frames={}",
        stage.frames,stage.blocks,stage.hops,stage.cycles,stage.stage_delay_frames(),stage.limiter_engaged());
}

struct Fixture(PathBuf);
impl Fixture {
    fn new(channels: u16, frames: usize) -> Self {
        static ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "nova-mch-{}-{}.wav",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        let size = (frames * channels as usize * 2) as u32;
        let mut bytes = Vec::new();
        bytes.extend(b"RIFF");
        bytes.extend((36 + size).to_le_bytes());
        bytes.extend(b"WAVEfmt ");
        bytes.extend(16u32.to_le_bytes());
        bytes.extend(1u16.to_le_bytes());
        bytes.extend(channels.to_le_bytes());
        bytes.extend(48000u32.to_le_bytes());
        bytes.extend((48000u32 * channels as u32 * 2).to_le_bytes());
        bytes.extend((channels * 2).to_le_bytes());
        bytes.extend(16u16.to_le_bytes());
        bytes.extend(b"data");
        bytes.extend(size.to_le_bytes());
        for frame in 0..frames {
            for channel in 0..channels {
                bytes.extend(Self::sample(frame, channel).to_le_bytes());
            }
        }
        std::fs::write(&path, bytes).unwrap();
        Self(path)
    }
    fn sample(frame: usize, channel: u16) -> i16 {
        ((frame * 17 + channel as usize * 101) % 4096) as i16 - 2048
    }
    fn pump(&self, output: ChannelLayout, with_mix: bool) -> DecoderPump {
        use_null_audio_output(true);
        let bytes = NativeMediaByteSource::new(Box::new(
            crate::FileMediaByteSource::open(&self.0).unwrap(),
        ));
        let rate = NonZeroU32::new(48_000).unwrap();
        let (decoder, info, _) =
            open_native_audio(bytes, -1, rate, output.channels() as u16).unwrap();
        let timing = StreamTiming {
            time_base: StreamTimeBase {
                numerator: info.time_base_num,
                denominator: info.time_base_den,
            },
            start_time: None,
            presentation_offset_nanoseconds: 0,
        };
        let mut pump = DecoderPump::new(
            decoder,
            timing,
            rate,
            NonZeroUsize::new(output.channels()).unwrap(),
            Arc::default(),
            Arc::default(),
        );
        if with_mix {
            let source = ChannelLayout::new(
                info.source_layout_mask,
                info.source_channels as u16,
                info.source_layout_mask == 0 || info.source_layout_assumed != 0,
            )
            .unwrap();
            assert_eq!(
                info.output_channels, info.source_channels,
                "swr preserves source layout"
            );
            pump.mixer = Some(MixStage::new(source, output, 48_000).unwrap());
        }
        pump
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
fn drain(pump: &mut DecoderPump, ring: &Arc<AudioRing>) -> Vec<f32> {
    let mut writer = StreamWriter::new(ring.clone(), None);
    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump.refill_to(&mut writer, 1024).unwrap();
        let (read, n) = ring.consumer_window();
        for frame in 0..n {
            for ch in 0..pump.channels {
                out.push(ring.sample(read.wrapping_add(frame), ch));
            }
        }
        ring.commit_read(read, n);
        if pump.at_end() {
            break;
        }
        assert!(Instant::now() < deadline);
    }
    out
}
#[test]
fn stereo_decoder_and_ring_remain_bit_identical_without_mixer() {
    let fixture = Fixture::new(2, 12031);
    let mut pump = fixture.pump(stereo(), false);
    let ring = Arc::new(
        AudioRing::new(
            NonZeroUsize::new(1024).unwrap(),
            NonZeroUsize::new(2).unwrap(),
        )
        .unwrap(),
    );
    let out = drain(&mut pump, &ring);
    let expected: Vec<_> = (0..12031)
        .flat_map(|i| (0..2).map(move |ch| Fixture::sample(i, ch) as f32 / 32768.0))
        .collect();
    assert_eq!(out, expected);
    assert!(pump.mixer.is_none());
    assert_eq!(pump.metrics.mix_frames.load(Ordering::Relaxed), 0);
}
#[test]
fn processed_pump_preserves_stop_resume_eof_and_clock_anchor() {
    let fixture = Fixture::new(2, 12031);
    let output = layout(FIVE_ONE_MASK);
    let mut pump = fixture.pump(output, true);
    let ring = Arc::new(
        AudioRing::new(
            NonZeroUsize::new(1024).unwrap(),
            NonZeroUsize::new(6).unwrap(),
        )
        .unwrap(),
    );
    pump.stop.set(Some(Duration::from_millis(125)));
    let first = drain(&mut pump, &ring);
    assert_eq!(first.len(), 6000 * 6);
    assert_eq!(pump.end, Some(SongEnd::StopBound));
    assert_eq!(pump.generation_anchor.unwrap().as_nanoseconds(), 0);
    pump.stop.set(None);
    let tail = drain(&mut pump, &ring);
    assert_eq!(first.len() + tail.len(), 12031 * 6);
    let mut whole = fixture.pump(output, true);
    let uninterrupted = drain(&mut whole, &ring);
    assert_eq!(
        [first, tail].concat(),
        uninterrupted,
        "CUE change adds no step or flush"
    );
    pump.seek(Duration::from_millis(50)).unwrap();
    pump.reset_generation(Some(NormalizedTimestamp::from_position(
        Duration::from_millis(50),
    )));
    let sought = drain(&mut pump, &ring);
    assert_eq!(sought.len(), (12031 - 2400) * 6);
    assert_eq!(pump.generation_anchor.unwrap().as_nanoseconds(), 50_000_000);
}
