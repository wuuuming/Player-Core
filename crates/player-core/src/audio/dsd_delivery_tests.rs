use crate::source::{NativeMediaByteSource, NativeMessage};
use crate::{presentation_offset_nanoseconds, NormalizedTimestamp, StreamTimeBase, StreamTiming};
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use super::asio_host::AsioDsdTransport;
use super::callback::{
    CallbackConfiguration, CallbackControl, CallbackMoment, CallbackResources, RampKind, UserGain,
    MODE_HELD, MODE_RUNNING,
};
use super::clock::AudioClock;
#[cfg(feature = "test-support")]
use super::device::use_null_audio_output;
#[cfg(any(feature = "asio", test))]
use super::dsd::DsdCallback;
use super::ffi::bn_audio_decoder_enable_dsd;
use super::metrics::SharedMetrics;
use super::policy::AudioPolicy;
use super::pump::{open_native_audio, DecoderPump, SongEnd};
use super::ring::AudioRing;
use super::session::StopBound;
use super::stream::StreamWriter;

#[test]
fn raw_dff_pump_obeys_span_boundaries_and_resume_without_skipping_bytes() {
    use std::io::{Read, Seek, SeekFrom};
    // A generated DFF (`crate::dsd_files`), or the real one `PLAYER_CORE_FORMAT_DFF` names (L5).
    use crate::dsd_files;
    let generated =
        std::env::temp_dir().join(format!("player-core-dff-pump-{}.dff", std::process::id()));
    let path = match std::env::var_os("PLAYER_CORE_FORMAT_DFF") {
        Some(path) => std::path::PathBuf::from(path),
        None => {
            dsd_files::write_dff(&generated, dsd_files::BYTES_PER_SECOND * 3);
            generated.clone()
        }
    };
    use_null_audio_output(true);
    let mut oracle = std::fs::File::open(&path).unwrap();
    oracle.seek(SeekFrom::Start(16)).unwrap();
    let payload = loop {
        let mut header = [0; 12];
        oracle.read_exact(&mut header).unwrap();
        let size = u64::from_be_bytes(header[4..].try_into().unwrap());
        if &header[..4] == b"DSD " {
            break oracle.stream_position().unwrap();
        }
        oracle
            .seek(SeekFrom::Current((size + (size & 1)) as i64))
            .unwrap();
    };
    let source =
        NativeMediaByteSource::new(Box::new(crate::FileMediaByteSource::open(&path).unwrap()));
    let (mut decoder, mut info, _) =
        open_native_audio(source, -1, NonZeroU32::new(48_000).unwrap(), 2).unwrap();
    let mut error = NativeMessage::new();
    assert_eq!(
        // SAFETY: the decoder just opened; `info` and the message buffer are live locals.
        unsafe {
            bn_audio_decoder_enable_dsd(
                decoder.raw.as_ptr(),
                &mut info,
                error.as_mut_ptr(),
                error.capacity(),
            )
        },
        0
    );
    decoder.dsd = true;
    let rate = NonZeroU32::new(info.output_sample_rate as u32).unwrap();
    let channels = NonZeroUsize::new(info.output_channels as usize).unwrap();
    let time_base = StreamTimeBase {
        numerator: info.time_base_num,
        denominator: info.time_base_den,
    };
    let start_time = (info.has_start_time != 0).then_some(info.start_time);
    let timing = StreamTiming {
        time_base,
        start_time,
        presentation_offset_nanoseconds: presentation_offset_nanoseconds(
            time_base,
            start_time,
            (info.has_format_start_time != 0).then_some(info.format_start_time_us),
        )
        .unwrap(),
    };
    let stop = Arc::new(StopBound::default());
    let metrics = Arc::new(SharedMetrics::default());
    let mut pump = DecoderPump::new(decoder, timing, rate, channels, metrics, stop.clone());
    let mut out = StreamWriter::new(
        Arc::new(
            AudioRing::with_encoding(NonZeroUsize::new(1024).unwrap(), channels, true).unwrap(),
        ),
        None,
    );
    let ring = out.ring.clone();
    let clock = AudioClock::with_playback_rate(Arc::default());
    let start = Duration::from_millis(1123);
    let split = Duration::from_millis(1150);
    let end = Duration::from_millis(1167);
    let frame_at =
        |time: Duration| (time.as_nanos() * u128::from(rate.get())).div_ceil(1_000_000_000) as u64;
    stop.set(Some(split));
    pump.seek(start).unwrap();
    pump.begin_generation(
        &mut out,
        &clock,
        1,
        Some(NormalizedTimestamp::from_position(start)),
        1024,
    )
    .unwrap();
    let mut cursor = frame_at(start);
    oracle
        .seek(SeekFrom::Start(payload + cursor * channels.get() as u64))
        .unwrap();
    for bound in [split, end] {
        stop.set(Some(bound));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            pump.refill_to(&mut out, 1024).unwrap();
            let (read, available) = ring.consumer_window();
            let mut expected = vec![0; available * channels.get()];
            oracle.read_exact(&mut expected).unwrap();
            for frame in 0..available {
                for channel in 0..channels.get() {
                    assert_eq!(
                        ring.dsd_sample(read.wrapping_add(frame), channel),
                        expected[frame * channels.get() + channel],
                        "raw DFF byte frame {}, channel {}",
                        cursor + frame as u64,
                        channel
                    );
                }
            }
            ring.commit_read(read, available);
            cursor += available as u64;
            if pump.end == Some(SongEnd::StopBound) {
                break;
            }
            assert!(Instant::now() < deadline);
            if available == 0 {
                thread::sleep(Duration::from_millis(1));
            }
        }
        assert_eq!(
            cursor,
            frame_at(bound),
            "span end excludes the next byte frame"
        );
    }
    drop(pump);
    let _ = std::fs::remove_file(&generated);
}

#[test]
fn native_asio_dsd_mute_keeps_the_clock_and_resumes_at_unity() {
    let rate = NonZeroU32::new(352_800).unwrap();
    let ring = Arc::new(
        AudioRing::with_encoding(
            NonZeroUsize::new(32).unwrap(),
            NonZeroUsize::new(2).unwrap(),
            true,
        )
        .unwrap(),
    );
    let clock = Arc::new(AudioClock::with_playback_rate(Arc::default()));
    clock
        .reanchor(1, NormalizedTimestamp::from_nanoseconds(0))
        .unwrap();
    let control = Arc::new(CallbackControl::new());
    let gain = Arc::new(UserGain::default());
    gain.set(0.25);
    let metrics = Arc::new(SharedMetrics::default());
    let mut callback = DsdCallback::new(
        CallbackResources {
            ring: ring.clone(),
            clock: clock.clone(),
            control: control.clone(),
            metrics: metrics.clone(),
            user_gain: gain.clone(),
            worker_thread: thread::current(),
        },
        CallbackConfiguration {
            policy: AudioPolicy::provisional_for_sample_rate(rate),
            channels: 2,
            sample_rate: rate,
        },
    );
    let bytes = [0x80, 0x01, 0x23, 0xfe, 0x45, 0xdc, 0x67, 0xba];
    assert_eq!(ring.push_dsd(&bytes), 4);
    control.publish(MODE_RUNNING, RampKind::Start).unwrap();
    let now = Instant::now();
    let mut out = [0; 4];
    callback.render(
        &mut out,
        CallbackMoment {
            observed_at: now,
            delivery_delay: Duration::ZERO,
        },
    );
    assert_eq!(out, bytes[..4]);
    gain.set(0.0);
    callback.render(
        &mut out,
        CallbackMoment {
            observed_at: now,
            delivery_delay: Duration::ZERO,
        },
    );
    assert_eq!(out, [0x69; 4]);
    assert_eq!(
        ring.available_frames(),
        0,
        "muting consumes the source instead of pausing it"
    );
    assert_eq!(clock.delivery(1).0, 4_000_000_000 / 352_800);
    assert_eq!(metrics.media_frames_committed.load(Ordering::Acquire), 4);

    let held = control.publish(MODE_HELD, RampKind::SeekDown).unwrap();
    callback.render(
        &mut out,
        CallbackMoment {
            observed_at: now,
            delivery_delay: Duration::from_millis(2),
        },
    );
    assert_eq!(out, [0x69; 4]);
    assert!(control.acknowledged_version.load(Ordering::Acquire) < held);
    callback.render(
        &mut out,
        CallbackMoment {
            observed_at: now + Duration::from_millis(3),
            delivery_delay: Duration::ZERO,
        },
    );
    assert_eq!(control.acknowledged_version.load(Ordering::Acquire), held);
    ring.discard_all();
    clock
        .reanchor(2, NormalizedTimestamp::from_nanoseconds(1_000_000_000))
        .unwrap();
    assert_eq!(ring.push_dsd(&[0x12, 0x34]), 1);
    gain.set(0.125);
    control.publish(MODE_RUNNING, RampKind::SeekUp).unwrap();
    callback.render(
        &mut out,
        CallbackMoment {
            observed_at: now + Duration::from_millis(4),
            delivery_delay: Duration::from_millis(2),
        },
    );
    assert_eq!(out, [0x12, 0x34, 0x69, 0x69]);
    assert_eq!(clock.delivery(2).0, 1_000_000_000 / 352_800);
    assert_eq!(clock.delivery(2).1, 2_000_000);
    callback.render(
        &mut out,
        CallbackMoment {
            observed_at: now + Duration::from_millis(5),
            delivery_delay: Duration::from_millis(2),
        },
    );
    assert_eq!(clock.delivery(2).1, 1_000_000);
    callback.render(
        &mut out,
        CallbackMoment {
            observed_at: now + Duration::from_millis(6),
            delivery_delay: Duration::from_millis(2),
        },
    );
    assert_eq!(clock.delivery(2), (1_000_000_000 / 352_800, 0));
}

/// Drives the byte callback and writer under the host's negotiated transport. Expected
/// bytes are hand-computed, including each muted carrier's still-alternating DoP marker.
pub(super) fn assert_muted_transport(transport: AsioDsdTransport) {
    use crate::audio::asio_writer::{
        AsioWriteFormat, DopWireFormat, DsdWireFormat, NativeAsioWriter,
    };
    let rate = NonZeroU32::new(352_800).unwrap();
    let ring = Arc::new(
        AudioRing::with_encoding(
            NonZeroUsize::new(8).unwrap(),
            NonZeroUsize::new(1).unwrap(),
            true,
        )
        .unwrap(),
    );
    let clock = Arc::new(AudioClock::with_playback_rate(Arc::default()));
    clock
        .reanchor(1, NormalizedTimestamp::from_nanoseconds(0))
        .unwrap();
    let control = Arc::new(CallbackControl::new());
    let gain = Arc::new(UserGain::default());
    let metrics = Arc::new(SharedMetrics::default());
    let mut callback = DsdCallback::new(
        CallbackResources {
            ring: ring.clone(),
            clock: clock.clone(),
            control: control.clone(),
            metrics: metrics.clone(),
            user_gain: gain.clone(),
            worker_thread: thread::current(),
        },
        CallbackConfiguration {
            policy: AudioPolicy::provisional_for_sample_rate(rate),
            channels: 1,
            sample_rate: rate,
        },
    );
    let (format, expected): (_, [&[u8]; 4]) = match transport {
        AsioDsdTransport::Native => (
            AsioWriteFormat::Dsd(DsdWireFormat::Msb1),
            [&[0x12, 0x34], &[0x69, 0x69], &[0x69, 0x69], &[0xde, 0xf0]],
        ),
        AsioDsdTransport::Dop => (
            AsioWriteFormat::Dop(DopWireFormat::Int24Le),
            [
                &[0x34, 0x12, 0x05],
                &[0x69, 0x69, 0xfa],
                &[0x69, 0x69, 0x05],
                &[0xf0, 0xde, 0xfa],
            ],
        ),
    };
    let mut writer = NativeAsioWriter::new(format, 1).unwrap();
    assert_eq!(
        ring.push_dsd(&[0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0]),
        8
    );
    let version = control.publish(MODE_RUNNING, RampKind::Start).unwrap();
    for (index, volume) in [0.5, 0.0, -0.0, 0.125].into_iter().enumerate() {
        gain.set(volume);
        let mut bytes = [0; 2];
        callback.render(
            &mut bytes,
            CallbackMoment {
                observed_at: Instant::now(),
                delivery_delay: Duration::ZERO,
            },
        );
        let mut output = vec![0; expected[index].len()];
        writer.write_dsd(&bytes, &mut [&mut output]);
        assert_eq!(output, expected[index]);
        let consumed = (index + 1) as u64 * 2;
        assert_eq!(
            clock.delivery(1).0,
            consumed * 1_000_000_000 / u64::from(rate.get())
        );
        assert_eq!(
            metrics.media_frames_committed.load(Ordering::Acquire),
            consumed
        );
        assert_eq!(
            control.request_version.load(Ordering::Acquire),
            version,
            "mute does not restart the stream"
        );
    }
    assert_eq!(ring.available_frames(), 0);
}
