use super::*;
use crate::linux::pulse;
use crate::{enumerate_output_systems, set_output_route, AudioOutputRoute, PathAudioSourceFactory};

/// A real file through ffmpeg and the sound server: the position follows what was heard,
/// pause holds it, seek moves it, the end is reported. Needs `ffmpeg` and a pulse server;
/// `NOVA_PULSE_TEST=1` turns it on.
#[test]
fn pulse_playback_follows_heard_audio() {
    if std::env::var("NOVA_PULSE_TEST").is_err() {
        eprintln!("skipped: NOVA_PULSE_TEST not set");
        return;
    }
    assert!(pulse::available(), "no pulse server");
    let dir = std::env::temp_dir().join(format!("nova-pulse-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let wav = dir.join("tone.wav");
    let ok = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=3",
        ])
        .arg(&wav)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let f: Arc<dyn AudioSourceFactory> = Arc::new(PathAudioSourceFactory::new(&wav));
    let mut p = AudioPlayer::open(f, TrackHint, Arc::new(|| {}), None).unwrap();
    assert!(!p.silent);
    let d = p.duration.expect("duration");
    assert!((d.as_secs_f64() - 3.0).abs() < 0.05, "{d:?}");
    p.set_volume(0.05);
    // play() now queues startup instead of waiting on PulseAudio/ffmpeg. Bound startup
    // separately, then measure the heard clock, not time spent opening the output.
    let wait_for_audio = |player: &mut AudioPlayer| {
        let started = Instant::now();
        loop {
            player.service(Instant::now()).unwrap();
            if player
                .run
                .as_ref()
                .is_some_and(|run| run.state.heard_us.load(Ordering::Relaxed) > 0)
            {
                let position = player.position(Instant::now());
                eprintln!(
                    "Pulse first audio after {:?}, position={position:?}",
                    started.elapsed()
                );
                return position;
            }
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "Pulse output did not deliver audio"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    p.play().unwrap();
    let first = wait_for_audio(&mut p);
    std::thread::sleep(Duration::from_millis(1000));
    p.service(Instant::now()).unwrap();
    let a = p
        .position(Instant::now())
        .saturating_sub(first)
        .as_secs_f64();
    assert!((0.6..1.1).contains(&a), "after 1 s heard an additional {a}");
    p.pause().unwrap();
    let held = p.position(Instant::now());
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(p.position(Instant::now()), held, "pause holds");
    p.seek(Duration::from_millis(2200)).unwrap();
    p.play().unwrap();
    let first = wait_for_audio(&mut p);
    assert!(
        (2.2..2.3).contains(&first.as_secs_f64()),
        "seek starts at {first:?}"
    );
    std::thread::sleep(Duration::from_millis(400));
    p.service(Instant::now()).unwrap();
    let b = p
        .position(Instant::now())
        .saturating_sub(first)
        .as_secs_f64();
    assert!(
        (0.2..0.5).contains(&b),
        "after seek heard an additional {b}"
    );
    let t0 = Instant::now();
    while !p.ended() && t0.elapsed() < Duration::from_secs(3) {
        p.service(Instant::now()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(p.ended(), "end reported");
    p.service(Instant::now()).unwrap();
    assert!(!p.is_playing());
    // -ss/-t are expressed in milliseconds; 44.1 -> 48 kHz resampling can
    // round the final sample count. Never replace the actual position by the hint.
    assert!(p.position(Instant::now()).abs_diff(d) <= Duration::from_millis(2));
    p.dispose().unwrap();

    // Core-private EOF assertions moved from the app's HTTP integration test. Reuse this
    // existing fixture/case: no test hooks or mutable internals are exposed to the host.
    let factory = Arc::new(PathAudioSourceFactory::new(&wav));
    let mut player = AudioPlayer::open(
        factory,
        TrackHint,
        Arc::new(|| {}),
        Some(Duration::from_secs(3)),
    )
    .unwrap();
    player.duration = None;
    player.play().unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while player.is_playing() && Instant::now() < deadline {
        player.service(Instant::now()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(player.ended() && !player.is_playing());
    assert!(player.run.is_none());
    assert!(
        player
            .position(Instant::now())
            .abs_diff(Duration::from_secs(3))
            <= Duration::from_micros(21)
    );

    let factory = Arc::new(PathAudioSourceFactory::new(&wav));
    let mut player = AudioPlayer::open(
        factory,
        TrackHint,
        Arc::new(|| {}),
        Some(Duration::from_secs(60)),
    )
    .unwrap();
    player.play().unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while player.is_playing() && Instant::now() < deadline {
        player.service(Instant::now()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(player.ended() && !player.is_playing());
    assert!(player.run.is_none());
    assert!(
        player
            .position(Instant::now())
            .abs_diff(Duration::from_secs(3))
            <= Duration::from_micros(21)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The sinks are listed, and switching mid-song moves the stream to the chosen sink while
/// the position keeps going.
#[test]
fn output_switch_moves_the_stream() {
    if std::env::var("NOVA_PULSE_TEST").is_err() {
        eprintln!("skipped: NOVA_PULSE_TEST not set");
        return;
    }
    let sys = enumerate_output_systems();
    let sinks = &sys[0].devices;
    assert!(sinks.len() >= 2, "need two sinks: {sinks:?}");
    assert_eq!(
        sinks.iter().filter(|d| d.is_default).count(),
        1,
        "{sinks:?}"
    );
    let other = sinks.iter().find(|d| !d.is_default).unwrap().id.clone();
    let dir = std::env::temp_dir().join(format!("nova-route-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let wav = dir.join("tone.wav");
    assert!(std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=3"
        ])
        .arg(&wav)
        .status()
        .unwrap()
        .success());
    let on_sink = || {
        let o = std::process::Command::new("pactl")
            .args(["list", "sink-inputs"])
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        let t = String::from_utf8_lossy(&o.stdout).into_owned();
        let idx = t
            .lines()
            .find_map(|l| l.trim().strip_prefix("Sink: ").map(str::to_owned))?;
        let o = std::process::Command::new("pactl")
            .args(["list", "short", "sinks"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .find(|l| l.split(char::is_whitespace).next() == Some(idx.as_str()))
            .and_then(|l| l.split(char::is_whitespace).nth(1).map(str::to_owned))
    };
    set_output_route(AudioOutputRoute::WasapiDefault);
    let f: Arc<dyn AudioSourceFactory> = Arc::new(PathAudioSourceFactory::new(&wav));
    let mut p = AudioPlayer::open(f, TrackHint, Arc::new(|| {}), None).unwrap();
    p.set_volume(0.05);
    p.play().unwrap();
    std::thread::sleep(Duration::from_millis(600));
    let def = sinks.iter().find(|d| d.is_default).unwrap().id.clone();
    assert_eq!(
        on_sink().as_deref(),
        Some(def.as_str()),
        "starts on the default"
    );
    let before = p.position(Instant::now()).as_secs_f64();
    assert_eq!(
        set_output_route(AudioOutputRoute::Wasapi(other.clone())),
        AudioOutputRoute::Wasapi(other.clone())
    );
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        on_sink().as_deref(),
        Some(other.as_str()),
        "moved to the chosen sink"
    );
    let after = p.position(Instant::now()).as_secs_f64();
    assert!(after > before + 0.3, "keeps going: {before} -> {after}");
    p.dispose().unwrap();
    set_output_route(AudioOutputRoute::WasapiDefault);
    let _ = std::fs::remove_dir_all(&dir);
}
