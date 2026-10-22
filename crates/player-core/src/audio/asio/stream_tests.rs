use super::*;
use crate::audio::asio_writer::{DopWireFormat, DsdWireFormat};
use crate::audio::callback::{
    CallbackConfiguration, CallbackControl, CallbackResources, RampKind, UserGain, MODE_RUNNING,
};
use crate::audio::clock::AudioClock;
use crate::audio::dsd::DsdCallback;
use crate::audio::ring::AudioRing;
use crate::{AudioPolicy, NormalizedTimestamp};
use std::num::NonZeroUsize;
use std::thread;

fn callback(
    render: Render,
    types: &[AsioWriteFormat],
    frames: usize,
    byte_frames: usize,
) -> Box<RawCallback> {
    let channels = types.len();
    Box::new(RawCallback {
        render: UnsafeCell::new(RenderState {
            render,
            writers: types
                .iter()
                .map(|format| NativeAsioWriter::new(*format, channels as u16).unwrap())
                .collect(),
            dsd: vec![0x69; byte_frames * channels],
            frames,
            channels,
            clock_rate: NonZeroU32::new(2_822_400).unwrap(),
            delivery_delay: Duration::ZERO,
        }),
        fault: StreamFaultSink {
            control: Arc::new(CallbackControl::new()),
            metrics: Arc::default(),
            device_faults: Arc::default(),
            wake_render: Arc::new(|| {}),
        },
    })
}

#[test]
fn driver_callback_maps_each_native_channel_using_its_own_wire_type() {
    let mut callback = callback(
        Box::new(|bytes, _| {
            bytes.copy_from_slice(&[0x80, 0x81, 0x23, 0x7e]);
        }),
        &[
            AsioWriteFormat::Dsd(DsdWireFormat::Lsb1),
            AsioWriteFormat::Dsd(DsdWireFormat::Ner8),
        ],
        16,
        2,
    );
    let mut left = [0; 2];
    let mut right = [0; 16];
    let mut outputs = [left.as_mut_ptr(), right.as_mut_ptr()];
    // SAFETY: `callback` outlives the call; `outputs` holds one buffer per writer, each sized for the
    // writer's format and the frames the state was built for.
    unsafe {
        buffer_switch(
            (&mut *callback as *mut RawCallback).cast(),
            outputs.as_mut_ptr(),
            16,
        );
    }
    assert_eq!(left, [0x01, 0xc4]);
    assert_eq!(
        right,
        [255, 0, 0, 0, 0, 0, 0, 255, 0, 255, 255, 255, 255, 255, 255, 0]
    );
}

#[test]
fn driver_callback_keeps_dop_markers_in_phase_on_mixed_integer_channels() {
    let mut callback = callback(
        Box::new(|bytes, _| {
            bytes.copy_from_slice(&[0x12, 0x34, 0x56, 0x78]);
        }),
        &[
            AsioWriteFormat::Dop(DopWireFormat::Int24Le),
            AsioWriteFormat::Dop(DopWireFormat::Int32Be { valid_bits: 24 }),
        ],
        1,
        2,
    );
    let mut left = [0; 3];
    let mut right = [0; 4];
    let mut outputs = [left.as_mut_ptr(), right.as_mut_ptr()];
    // SAFETY: `callback` outlives the call; `outputs` holds one buffer per writer, each sized for the
    // writer's format and the frames the state was built for.
    unsafe {
        buffer_switch(
            (&mut *callback as *mut RawCallback).cast(),
            outputs.as_mut_ptr(),
            1,
        );
    }
    assert_eq!(left, [0x56, 0x12, 0x05]);
    assert_eq!(right, [0, 0x05, 0x34, 0x78]);
    // SAFETY: `callback` outlives the call; `outputs` holds one buffer per writer, each sized for the
    // writer's format and the frames the state was built for.
    unsafe {
        buffer_switch(
            (&mut *callback as *mut RawCallback).cast(),
            outputs.as_mut_ptr(),
            1,
        );
    }
    assert_eq!(left, [0x56, 0x12, 0xfa]);
    assert_eq!(right, [0xff, 0xfa, 0x34, 0x78]);
}

#[test]
fn native_asio_driver_callback_keeps_dop_locked_across_mute() {
    let rate = NonZeroU32::new(352_800).unwrap();
    let ring = Arc::new(
        AudioRing::with_encoding(
            NonZeroUsize::new(6).unwrap(),
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
    let mut delivery = DsdCallback::new(
        CallbackResources {
            ring: ring.clone(),
            clock: clock.clone(),
            control: control.clone(),
            metrics: Arc::default(),
            user_gain: gain.clone(),
            worker_thread: thread::current(),
        },
        CallbackConfiguration {
            policy: AudioPolicy::provisional_for_sample_rate(rate),
            channels: 1,
            sample_rate: rate,
        },
    );
    let mut callback = callback(
        Box::new(move |bytes, moment| delivery.render(bytes, moment)),
        &[AsioWriteFormat::Dop(DopWireFormat::Int24Le)],
        1,
        2,
    );
    assert_eq!(ring.push_dsd(&[0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc]), 6);
    control.publish(MODE_RUNNING, RampKind::Start).unwrap();
    for (index, (volume, expected)) in [
        (0.5, [0x34, 0x12, 0x05]),
        (0.0, [0x69, 0x69, 0xfa]),
        (0.125, [0xbc, 0x9a, 0x05]),
    ]
    .into_iter()
    .enumerate()
    {
        gain.set(volume);
        let mut output = [0; 3];
        let mut outputs = [output.as_mut_ptr()];
        // SAFETY: `callback` outlives the call; `outputs` holds one buffer per writer, each sized for the
        // writer's format and the frames the state was built for.
        unsafe {
            buffer_switch(
                (&mut *callback as *mut RawCallback).cast(),
                outputs.as_mut_ptr(),
                1,
            );
        }
        assert_eq!(output, expected);
        assert_eq!(
            clock.delivery(1).0,
            (index as u64 + 1) * 2_000_000_000 / 352_800
        );
    }
    assert_eq!(ring.available_frames(), 0);
}

#[test]
fn a_driver_asking_again_and_again_to_be_reset_fails_its_binding_once() {
    let callback = callback(Box::new(|_, _| {}), &[], 16, 2);
    for _ in 0..3 {
        driver_reset(&callback.fault);
    }
    assert_eq!(callback.fault.device_faults.load(Ordering::Acquire), 1);
    assert_eq!(
        callback.fault.metrics.output_errors.load(Ordering::Relaxed),
        1
    );
}
