use super::*;

const RATE: NonZeroU32 = NonZeroU32::new(48_000).unwrap();

fn stage(track_db: f32, mode: ReplayGain) -> (ReplayGainStage, Arc<ReplayGainSetting>) {
    let setting = Arc::new(ReplayGainSetting::default());
    setting.set(mode);
    let tags = ReplayGainTags {
        track: Some(track_db),
        album: None,
    };
    (ReplayGainStage::new(setting.clone(), tags, RATE), setting)
}

/// Gain above 0 dB: a song lifted 20 dB past full scale is held at it by the limiter, and the
/// limiter lets go once the loud part has passed.
#[test]
fn gain_above_unity_is_limited_at_full_scale() {
    let (mut stage, _) = stage(20.0, ReplayGain::Track);
    let mut loud: Vec<f32> = (0..9_600)
        .map(|frame| 0.5 * (frame as f32 * 0.05).sin())
        .collect();
    stage.process(&mut loud, 1);
    assert!(
        loud.iter().all(|sample| sample.abs() <= 1.0),
        "nothing past full scale"
    );
    assert!(stage.limiter.engaged_frames > 0);
    // Quiet again: 0.01 × 10 is 0.1, far under the ceiling, and after the release it is
    // played at exactly the gain.
    let mut quiet = vec![0.01f32; 48_000];
    stage.process(&mut quiet, 1);
    assert!(
        (quiet[quiet.len() - 1] - 0.1).abs() < 1e-6,
        "{}",
        quiet[quiet.len() - 1]
    );
}

/// The setting changed while playing moves the gain without a step.
#[test]
fn a_setting_changed_while_playing_glides_to_the_new_gain() {
    let (mut stage, setting) = stage(-20.0, ReplayGain::Off);
    let mut before = vec![1.0f32; 480];
    stage.process(&mut before, 1);
    assert!(
        before.iter().all(|sample| *sample == 1.0),
        "off is untouched"
    );
    setting.set(ReplayGain::Track);
    let mut after = vec![1.0f32; 48_000];
    stage.process(&mut after, 1);
    let largest = after
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .fold(0.0, f32::max);
    assert!(largest < 0.001, "a step of {largest}");
    assert!(
        (after[after.len() - 1] - 0.1).abs() < 1e-6,
        "{}",
        after[after.len() - 1]
    );
    // A seek lands on the gain at once: the ring was discarded, nothing to glide from.
    setting.set(ReplayGain::Off);
    stage.reset();
    let mut seeked = vec![1.0f32; 4];
    stage.process(&mut seeked, 1);
    assert_eq!(seeked, vec![1.0; 4]);
}

/// A chain whose stage lags drops the lead-in, says where its output stands, and flushes
/// what it still holds — however the input is cut.
#[test]
fn a_chain_with_latency_drops_its_lead_in_and_flushes_its_tail() {
    struct Late(std::collections::VecDeque<f32>);
    impl DspStage for Late {
        fn process(&mut self, samples: &mut [f32], _channels: usize) {
            for sample in samples {
                self.0.push_back(*sample);
                *sample = self.0.pop_front().unwrap_or(0.0);
            }
        }
        fn reset(&mut self) {
            self.0 = vec![0.0; 3].into();
        }
        fn latency(&self) -> usize {
            3
        }
    }
    let mut chain = DspChain::new(vec![Box::new(Late(vec![0.0; 3].into()))], NonZeroUsize::MIN);
    let mut first = vec![1.0, 2.0];
    assert_eq!(chain.process(&mut first), 0);
    assert!(first.is_empty(), "all lead-in");
    let mut second = vec![3.0, 4.0, 5.0];
    assert_eq!(
        chain.process(&mut second),
        2,
        "starts two frames before this block"
    );
    assert_eq!(second, vec![1.0, 2.0]);
    assert_eq!(chain.flush(), vec![3.0, 4.0, 5.0]);
    assert!(chain.flush().is_empty());
    chain.reset();
    let mut short = vec![7.0];
    chain.process(&mut short);
    assert_eq!(
        chain.flush(),
        vec![7.0],
        "fewer frames than the latency still come out"
    );
}

#[test]
fn a_crossfade_holds_the_power_and_lets_the_coming_song_rise_alone() {
    let mut fade = Crossfade::new(4);
    for at in 0..4 {
        let (out, into) = fade.gains(at);
        assert!(
            (out * out + into * into - 1.0).abs() < 1e-6,
            "equal power at {at}"
        );
        assert!(out > into || at >= 2, "the going song leads the first half");
    }
    // Two frames of two channels: the going song's summed in under its falling gain.
    let mut coming = vec![1.0, 1.0, 1.0, 1.0];
    fade.mix(&mut coming, &[1.0, 1.0, 1.0, 1.0], 2);
    let (out, into) = fade.gains(0);
    assert!((coming[0] - (out + into)).abs() < 1e-6);
    assert_eq!(fade.left(), 2);
    // The going song has ended: the coming one goes on rising alone, and past the span it is
    // left as it is.
    let mut coming = vec![1.0, 1.0, 1.0, 1.0, 1.0, 1.0];
    fade.mix(&mut coming, &[], 2);
    assert!((coming[0] - fade.gains(2).1).abs() < 1e-6);
    assert!((coming[2] - fade.gains(3).1).abs() < 1e-6);
    assert_eq!(&coming[4..], &[1.0, 1.0]);
    assert_eq!(fade.left(), 0);
}

#[test]
fn a_song_ends_at_its_stop_else_its_length_and_nothing_is_past_that() {
    let s = Duration::from_secs;
    assert_eq!(song_end(Some(s(60)), Some(s(200))), Some(s(60)));
    assert_eq!(song_end(None, Some(s(200))), Some(s(200)));
    assert_eq!(song_end(None, None), None);
    assert_eq!(
        within_end(s(90), song_end(Some(s(60)), Some(s(200)))),
        s(60)
    );
    assert_eq!(within_end(s(30), Some(s(60))), s(30));
    assert_eq!(within_end(s(90), None), s(90));
}

#[test]
fn a_crossfade_starts_its_overlap_before_the_end_and_never_before_zero() {
    let s = Duration::from_secs;
    assert_eq!(handover_at(Some(s(200)), s(5)), Some(s(195)));
    assert_eq!(handover_at(Some(s(3)), s(5)), Some(Duration::ZERO));
    assert_eq!(handover_at(Some(s(200)), Duration::ZERO), None);
    assert_eq!(handover_at(None, s(5)), None);
}
