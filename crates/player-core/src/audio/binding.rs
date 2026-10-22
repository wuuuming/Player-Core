//! Binding a session to a device in two halves: prepare, which may fail, and commit, which
//! may not.

#[cfg(feature = "asio")]
use super::asio;
use super::callback::{CallbackConfiguration, CallbackControl, CallbackCore, CallbackResources};
use super::device::{
    build_output_stream, null_output_rate, resolve_output_device, route_snapshot,
    stream_config_for, AudioDeviceSelection, AudioOutputHost, NullStream, OutputDevice,
    OutputStream, RouteSnapshot, NULL_OUTPUT, NULL_OUTPUT_CHANNELS,
};
#[cfg(feature = "asio")]
use super::dsd::DsdCallback;
use super::endpoints;
use super::error::AudioOpenError;
use super::ffi::{BnAudioStreamInfo, NativeAudioDecoder};
use super::mix::{ChannelLayout, MixReport, MixStage};
use super::park::AudioParkState;
use super::policy::{AudioPolicy, AudioPolicyTimes};
use super::posture::{apply_posture_step, posture_step, AudioOutputPhase, TransportPosture};
use super::pump::{open_media_bytes, open_native_audio};
use super::rebind::StreamFaultSink;
use super::ring::AudioRing;
use super::session::{AudioSession, AudioSource, Landing};
use super::tags::replay_gain_tags;
use super::track::{dsd_sample_rate, AudioOutputEncoding, AudioStreamIndex, AudioStreamInfo};
use super::worker::{
    spawn_decode_worker, AudioWorkerConfiguration, AudioWorkerResources, AudioWorkerSource,
    AudioWorkerStart, WorkerHandle, WorkerLifetime, WorkerReady,
};
use crate::dsp::{song_chain, stream_chain, DspChain};
use crate::{presentation_offset_nanoseconds, StreamTimeBase, StreamTiming};
use cpal::SampleFormat;
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::atomic::Ordering;
use std::sync::Arc;
#[cfg(feature = "asio")]
use {
    super::asio_host::{AsioDsdTransport, AsioOpenError},
    super::device::{AudioOutputRoute, NULL_OUTPUT_RATE},
    super::error::NativeError,
    super::ffi::bn_audio_decoder_enable_dsd,
    crate::source::NativeMessage,
};

/// The null output's rate, for probing a DSD source before any device is chosen.
#[cfg(feature = "asio")]
const NULL_RATE: NonZeroU32 = match NonZeroU32::new(NULL_OUTPUT_RATE) {
    Some(rate) => rate,
    None => panic!("the null output has a rate"),
};

/// Everything scoped to one concrete device, and the audio-owner thread is the only place a value
/// of this type can exist: `stream` is `!Send` and `worker` is the reaping half.
pub(super) struct AudioBinding {
    pub(super) stream: OutputStream,
    pub(super) worker: WorkerLifetime,
    pub(super) handle: WorkerHandle,
    pub(super) control: Arc<CallbackControl>,
    #[cfg(feature = "asio")]
    pub(super) ring: Arc<AudioRing>,
}

/// A binding, built as far as it can be built away from the thread that will own it. Every field
/// is `Send`. The one part of a binding that is not — `cpal::Stream`, which carries
/// `platform::NotSendSyncAcrossAllPlatforms` on every platform — is absent by construction, so
/// this value can cross a thread boundary and a stream cannot.
/// The callback is carried as its RECIPE, not as a built `CallbackCore`: `build_output_stream`
/// consumes the closure, so a built core cannot survive a failed build, and a `commit_binding`
/// that cannot hand the prepared binding back intact is a `commit_binding` whose `?` silently
/// reaps a decode worker on whatever thread it happens to run on.
pub(super) struct PreparedBinding {
    pub(super) device: OutputDevice,
    pub(super) device_name: String,
    pub(super) config: cpal::StreamConfig,
    pub(super) sample_format: SampleFormat,
    pub(super) resources: CallbackResources,
    pub(super) configuration: CallbackConfiguration,
    pub(super) worker: WorkerLifetime,
    pub(super) handle: WorkerHandle,
    pub(super) control: Arc<CallbackControl>,
    pub(super) ring: Arc<AudioRing>,
    pub(super) info: AudioStreamInfo,
    pub(super) policy: AudioPolicy,
    pub(super) ready: WorkerReady,
    /// The reading of the route this binding was resolved by: its version marks the binding.
    pub(super) route: RouteSnapshot,
}

/// What a committed binding hands back: the owner thread keeps `binding`, the media loop is given
/// `landing`.
pub(super) struct CommittedBinding {
    pub(super) binding: AudioBinding,
    pub(super) landing: Landing,
}

/// Whether a binding must open its decoder again for the rate the decoder has just read: only on
/// ASIO, where the driver can follow the source, only when the device is not already at that rate,
/// and only when the driver was not already asked for it — a hint that was right, or the second
/// pass after a hint that was wrong or missing, opens once.
pub(super) fn needs_rate_reopen(
    on_asio: bool,
    read_rate: u32,
    output_rate: u32,
    asked_rate: u32,
) -> bool {
    on_asio && read_rate != 0 && read_rate != output_rate && read_rate != asked_rate
}

/// The one and only path that resolves a device, opens a decoder and starts a decode worker.
/// `open` and every rebind attempt call it, and every rebind calls it **off** the media loop.
///
/// It touches no global and builds no `cpal::Stream`: the stream, `Stream::play()` and the
/// `AudioSession::park` publication all belong to the owner thread and live in `commit_binding`. That is
/// what makes the standing claim on `AudioSession::park` — "published only by the stream-owner thread" —
/// true by construction once this function can run somewhere else.
pub(super) fn prepare_binding(
    source: &AudioSource,
    selection: &AudioDeviceSelection,
    requested_policy: Option<AudioPolicyTimes>,
    session: &AudioSession,
    start: AudioWorkerStart,
) -> Result<PreparedBinding, AudioOpenError> {
    // The transport comes FIRST and is checked at both of its boundaries, so a superseded or
    // abandoned attempt returns before any device is resolved. That ordering IS the cancel
    // guarantee: there is no window in which a dead attempt owns hardware.
    // On ASIO the device follows the source's own rate, which is known for certain only once the
    // decoder has opened the source. A source whose rate the binding did not know yet — the
    // session's first open, when the host gave no hint — is opened, its rate read, and opened
    // again once the driver has been asked for that rate. At most twice: the second pass asks for
    // the rate the first one read.
    let mut asked_rate = source.source_rate.load(Ordering::Acquire);
    #[cfg(feature = "asio")]
    let mut probed_dsd = false;
    let (
        device,
        device_name,
        sample_format,
        config,
        route,
        sample_rate,
        policy,
        channels,
        output_layout,
        decoder,
        native_info,
        codec,
    ) = loop {
        let bytes = open_media_bytes(source.factory.as_ref(), session, start.generation)?;
        // Where outputs should be, read once for this pass: the device is resolved by it and the
        // binding marked with its version, so a route that changes meanwhile rebinds it.
        let at = route_snapshot();
        #[cfg(feature = "asio")]
        let dsd_driver = match &at.route {
            AudioOutputRoute::Asio(name)
                if !probed_dsd
                    && !NULL_OUTPUT.load(Ordering::Acquire)
                    && *selection == AudioDeviceSelection::Chosen =>
            {
                Some(name.clone())
            }
            _ => None,
        };
        #[cfg(feature = "asio")]
        let bytes = if let Some(name) = dsd_driver.as_deref() {
            probed_dsd = true;
            // Probe before touching a PCM endpoint: a DSD driver's PCM formats need not
            // include the DSD byte rate. The same probe supplies the PCM fallback's rate.
            let (mut decoder, mut info, codec) =
                open_native_audio(bytes, source.stream_index, NULL_RATE, NULL_OUTPUT_CHANNELS)?;
            asked_rate = source.note_read_rate(info.source_sample_rate);
            session.ensure_live(start.generation)?;
            if matches!(codec.as_str(), "dsd_lsbf_planar" | "dsd_msbf")
                && session.clock.playback_rate.at_normal_speed()
            {
                let bit_rate = asked_rate.checked_mul(8).ok_or_else(|| {
                    AudioOpenError::InvalidOutputConfig("DSD sample rate overflowed".to_owned())
                })?;
                let source_channels = u16::try_from(info.source_channels).map_err(|_| {
                    AudioOpenError::InvalidOutputConfig("invalid DSD channels".to_owned())
                })?;
                let opened =
                    asio::open_dsd(name, bit_rate, source_channels).map_err(
                        |error| match error {
                            AsioOpenError::Resetting => {
                                AudioOpenError::DeviceUnavailable(name.to_owned())
                            }
                            AsioOpenError::Failed(reason) => AudioOpenError::BuildStream(reason),
                        },
                    )?;
                if let Some(device) = opened {
                    let mut error = NativeMessage::new();
                    // SAFETY: the decoder just opened; `info` and the message buffer (with its
                    // capacity) are live locals.
                    let status = unsafe {
                        bn_audio_decoder_enable_dsd(
                            decoder.raw.as_ptr(),
                            &mut info,
                            error.as_mut_ptr(),
                            error.capacity(),
                        )
                    };
                    if status != 0 {
                        return Err(AudioOpenError::Native(NativeError {
                            operation: "DSD open",
                            status,
                            transport: 0,
                            message: error.message(),
                        }));
                    }
                    decoder.dsd = true;
                    let sample_rate = NonZeroU32::new(asked_rate).ok_or_else(|| {
                        AudioOpenError::InvalidOutputConfig("DSD has no sample rate".to_owned())
                    })?;
                    let channels =
                        NonZeroUsize::new(usize::from(source_channels)).ok_or_else(|| {
                            AudioOpenError::InvalidOutputConfig("DSD has no channels".to_owned())
                        })?;
                    let policy = AudioPolicyTimes::for_output(requested_policy, sample_rate)?;
                    let format = if device.transport() == AsioDsdTransport::Native {
                        SampleFormat::U8
                    } else {
                        SampleFormat::I32
                    };
                    let config = cpal::StreamConfig {
                        channels: source_channels,
                        sample_rate: cpal::SampleRate(asked_rate),
                        buffer_size: cpal::BufferSize::Default,
                    };
                    break (
                        OutputDevice::NativeAsio(device),
                        name.to_owned(),
                        format,
                        config,
                        at,
                        sample_rate,
                        policy,
                        channels,
                        ChannelLayout::from_channels(source_channels)
                            .map_err(AudioOpenError::InvalidOutputConfig)?,
                        decoder,
                        info,
                        codec,
                    );
                }
            }
            drop(decoder);
            open_media_bytes(source.factory.as_ref(), session, start.generation)?
        } else {
            bytes
        };
        let (device, device_name, sample_format, mut config, route, endpoint) =
            if NULL_OUTPUT.load(Ordering::Acquire) {
                (
                    OutputDevice::Null,
                    "<null output>".to_owned(),
                    SampleFormat::F32,
                    cpal::StreamConfig {
                        channels: NULL_OUTPUT_CHANNELS,
                        sample_rate: cpal::SampleRate(null_output_rate()),
                        buffer_size: cpal::BufferSize::Default,
                    },
                    at,
                    None,
                )
            } else {
                // The format is the device's own — an ASIO driver's is fixed by the driver, often I32
                // or F32 — and the decoder converts into it, resampling only when the rates differ.
                // A resolve that fell back from a failed driver answers the reading after the
                // fallback, so a binding that fell back is not rebound again for the fallback it is.
                let resolved =
                    resolve_output_device(selection, at, (asked_rate != 0).then_some(asked_rate))?;
                let config = stream_config_for(resolved.host, &resolved.supported);
                (
                    OutputDevice::Cpal(resolved.device, resolved.host, resolved.lease),
                    resolved.name,
                    resolved.supported.sample_format(),
                    config,
                    resolved.route,
                    resolved.endpoint,
                )
            };
        let output_layout = if matches!(device, OutputDevice::Cpal(_, AudioOutputHost::Wasapi, _)) {
            endpoints::output_layout(selection, endpoint.as_deref(), config.channels)
                .map_err(AudioOpenError::InvalidOutputConfig)?
        } else {
            ChannelLayout::from_channels(config.channels)
                .map_err(AudioOpenError::InvalidOutputConfig)?
        };
        config.channels = output_layout.channels() as u16;
        let sample_rate = NonZeroU32::new(config.sample_rate.0).ok_or_else(|| {
            AudioOpenError::InvalidOutputConfig("cpal output sample rate was zero".to_owned())
        })?;
        let policy = AudioPolicyTimes::for_output(requested_policy, sample_rate)?;
        let channels = NonZeroUsize::new(usize::from(config.channels)).ok_or_else(|| {
            AudioOpenError::InvalidOutputConfig("cpal output channel count was zero".to_owned())
        })?;

        let (decoder, native_info, codec) = open_native_audio(
            bytes,
            source.stream_index,
            sample_rate,
            u16::try_from(channels.get()).map_err(|_| {
                AudioOpenError::InvalidOutputConfig(
                    "cpal output channel count exceeds the native ABI".to_owned(),
                )
            })?,
        )?;
        let read_rate = source.note_read_rate(native_info.source_sample_rate);
        let on_asio = matches!(device, OutputDevice::Cpal(_, AudioOutputHost::Asio, _));
        if needs_rate_reopen(on_asio, read_rate, sample_rate.get(), asked_rate) {
            // The device, and with it the lease, goes before the driver is asked again: a rate
            // change waits for every binding on the driver to let go, this one included.
            drop(decoder);
            drop(device);
            asked_rate = read_rate;
            session.ensure_live(start.generation)?;
            continue;
        }
        break (
            device,
            device_name,
            sample_format,
            config,
            route,
            sample_rate,
            policy,
            channels,
            output_layout,
            decoder,
            native_info,
            codec,
        );
    };
    let output_channels = config.channels;
    let DecodedTrack {
        timing,
        source_sample_rate,
        source_channels,
        source_layout,
        mixer,
        dsp,
        mix,
        stage_delay_frames,
    } = decoded_track(
        &decoder,
        &native_info,
        output_layout,
        (sample_rate, channels),
        session,
    )?;

    // The decoder open is the second blocking boundary — over a remote transport it reads and
    // probes — so an attempt that lost its generation there stops before a worker exists.
    session.ensure_live(start.generation)?;
    let ring = Arc::new(if decoder.dsd {
        AudioRing::with_encoding(policy.queue_capacity_frames, channels, true)?
    } else {
        AudioRing::new(policy.queue_capacity_frames, channels)?
    });
    // The stream's processing, which every song in this ring goes through; DSD bypasses it.
    let stream = if decoder.dsd {
        None
    } else {
        stream_chain(session.effects.as_ref(), sample_rate, channels)
    };
    let output_encoding = match &device {
        #[cfg(feature = "asio")]
        OutputDevice::NativeAsio(device) => match device.transport() {
            AsioDsdTransport::Native => AudioOutputEncoding::NativeDsd,
            AsioDsdTransport::Dop => AudioOutputEncoding::Dop,
        },
        _ => AudioOutputEncoding::Pcm,
    };
    let output_device_name = device_name.clone();
    let output_host = match &device {
        OutputDevice::Cpal(_, host, _) => Some(*host),
        #[cfg(feature = "asio")]
        OutputDevice::NativeAsio(_) => Some(AudioOutputHost::Asio),
        OutputDevice::Null => None,
    };
    let hardware_rate = match &device {
        #[cfg(feature = "asio")]
        OutputDevice::NativeAsio(device) => device.hardware_rate(),
        _ => sample_rate.get(),
    };
    let dsd_rate = dsd_sample_rate(&codec, source_sample_rate);
    // The callback control rides with the binding, so the sticky `failed` flag cpal raises dies
    // with the binding that raised it. The intent it publishes does not: that belongs to the
    // session, which no bind can construct.
    let control = Arc::new(CallbackControl::new());
    let (worker, handle, ready) = spawn_decode_worker(
        AudioWorkerSource {
            decoder,
            timing,
            mixer,
            dsp,
            stream,
        },
        AudioWorkerResources {
            ring: ring.clone(),
            clock: session.clock.clone(),
            metrics: session.metrics.clone(),
            control: control.clone(),
            edges: session.edges.clone(),
            wake_render: session.wake_render.clone(),
            stop: session.stop.clone(),
            next_wanted: session.next_wanted.clone(),
        },
        AudioWorkerConfiguration {
            policy,
            sample_rate,
            channels,
        },
        start,
    )?;
    // The binding dictates the generation; the worker no longer hands one back for the caller to
    // adopt. Pinning that here keeps the two from drifting apart silently.
    debug_assert_eq!(
        ready.generation, start.generation,
        "the decode worker must start on the generation this binding asked for"
    );
    let resources = CallbackResources {
        ring: ring.clone(),
        clock: session.clock.clone(),
        control: control.clone(),
        metrics: session.metrics.clone(),
        user_gain: session.user_gain.clone(),
        worker_thread: handle.thread.clone(),
    };
    let configuration = CallbackConfiguration {
        policy,
        channels: channels.get(),
        sample_rate,
    };

    Ok(PreparedBinding {
        device,
        device_name,
        config,
        sample_format,
        resources,
        configuration,
        worker,
        handle,
        control,
        ring,
        info: AudioStreamInfo {
            // From the stream the decoder ACTUALLY opened, not from the index that was requested:
            // a request is an intention and `-1` means "pick one for me". Validated here so an
            // invalid index cannot reach a consumer.
            stream_index: AudioStreamIndex::opened(native_info.stream_index)?,
            codec,
            timing,
            source_sample_rate,
            source_channels,
            source_layout,
            source_layout_assumed: source_layout.assumed(),
            output_layout,
            output_layout_assumed: output_layout.assumed(),
            mix,
            stage_delay_frames,
            output_sample_rate: hardware_rate,
            output_channels,
            output_sample_format: sample_format,
            output_encoding,
            dsd_sample_rate: dsd_rate,
            output_device: output_device_name,
            output_host,
        },
        policy,
        ready,
        route,
    })
}

/// What a song's decoder needs around it, read from what it opened: where its time stands, what
/// it is, and the mixer and processing between it and a ring of `sample_rate` and `channels`.
pub(super) struct DecodedTrack {
    pub(super) timing: StreamTiming,
    pub(super) source_sample_rate: u32,
    pub(super) source_channels: u16,
    pub(super) source_layout: ChannelLayout,
    pub(super) mixer: Option<MixStage>,
    pub(super) dsp: Option<DspChain>,
    pub(super) mix: Option<MixReport>,
    pub(super) stage_delay_frames: u32,
}

/// [`DecodedTrack`] for a decoder opened for an output of `output_layout` at `sample_rate` and
/// `channels`: one function for a binding's song and for the one that follows it.
pub(super) fn decoded_track(
    decoder: &NativeAudioDecoder,
    native_info: &BnAudioStreamInfo,
    output_layout: ChannelLayout,
    (sample_rate, channels): (NonZeroU32, NonZeroUsize),
    session: &AudioSession,
) -> Result<DecodedTrack, AudioOpenError> {
    let time_base = StreamTimeBase {
        numerator: native_info.time_base_num,
        denominator: native_info.time_base_den,
    };
    let start_time = (native_info.has_start_time != 0).then_some(native_info.start_time);
    let timing = StreamTiming {
        time_base,
        start_time,
        presentation_offset_nanoseconds: presentation_offset_nanoseconds(
            time_base,
            start_time,
            (native_info.has_format_start_time != 0).then_some(native_info.format_start_time_us),
        )?,
    };
    let source_sample_rate = u32::try_from(native_info.source_sample_rate)
        .ok()
        .filter(|rate| *rate != 0)
        .ok_or_else(|| {
            AudioOpenError::InvalidOutputConfig(
                "native audio source sample rate was invalid".to_owned(),
            )
        })?;
    let source_channels = u16::try_from(native_info.source_channels)
        .ok()
        .filter(|channels| *channels != 0)
        .ok_or_else(|| {
            AudioOpenError::InvalidOutputConfig(
                "native audio source channel count was invalid".to_owned(),
            )
        })?;
    let source_layout = ChannelLayout::new(
        native_info.source_layout_mask,
        source_channels,
        native_info.source_layout_assumed != 0 || native_info.source_layout_mask == 0,
    )
    .map_err(AudioOpenError::InvalidOutputConfig)?;
    let mixer = if !decoder.dsd && output_layout.channels() >= 4 {
        Some(
            MixStage::new(source_layout, output_layout, sample_rate.get())
                .map_err(AudioOpenError::InvalidOutputConfig)?,
        )
    } else {
        None
    };
    // DSD bypasses the processing; PCM runs through the song's own, after the mixer, at the
    // output's channels.
    let dsp = (!decoder.dsd).then(|| {
        song_chain(
            session.replay_gain.clone(),
            replay_gain_tags(decoder),
            sample_rate,
            channels,
        )
    });
    let mix = mixer.as_ref().map(MixStage::report);
    let stage_delay_frames = mixer.as_ref().map_or(0, MixStage::stage_delay_frames);
    let decoded_channels = mixer
        .as_ref()
        .map_or(channels.get(), MixStage::input_channels);
    if native_info.output_sample_rate != sample_rate.get() as i32
        || native_info.output_channels != decoded_channels as i32
    {
        return Err(AudioOpenError::InvalidOutputConfig(
            "native audio output format does not match cpal".to_owned(),
        ));
    }

    Ok(DecodedTrack {
        timing,
        source_sample_rate,
        source_channels,
        source_layout,
        mixer,
        dsp,
        mix,
        stage_delay_frames,
    })
}

/// Completes a prepared binding on the thread that will own it. This is the only place a
/// `cpal::Stream` is built or started and — with `reconcile` — the only place `AudioSession::park` is
/// published. Both now run on `player-core-audio-owner`, which is what finally makes the standing
/// claim on `AudioSession::park` ("published only by the stream-owner thread") true by construction.
pub(super) fn commit_binding(
    prepared: Box<PreparedBinding>,
    wanted: TransportPosture,
    session: &AudioSession,
) -> Result<CommittedBinding, (AudioOpenError, Box<PreparedBinding>)> {
    let callback = CallbackCore::new(prepared.resources.clone(), prepared.configuration);
    let fault = StreamFaultSink {
        control: prepared.control.clone(),
        metrics: session.metrics.clone(),
        device_faults: session.device_faults.clone(),
        wake_render: session.wake_render.clone(),
    };
    let built = match &prepared.device {
        OutputDevice::Cpal(device, host, lease) => build_output_stream(
            device,
            *host,
            &prepared.config,
            prepared.sample_format,
            callback,
            fault,
            lease.clone(),
            &prepared.route,
        ),
        OutputDevice::Null => NullStream::drive_callback(callback).map(OutputStream::Null),
        #[cfg(feature = "asio")]
        OutputDevice::NativeAsio(device) => {
            let mut callback = DsdCallback::new(prepared.resources.clone(), prepared.configuration);
            asio::build_native_asio_stream(
                device.clone(),
                Box::new(move |bytes, moment| callback.render(bytes, moment)),
                fault,
            )
            .map(OutputStream::NativeAsio)
            .map_err(AudioOpenError::BuildStream)
        }
    };
    let stream = match built {
        Ok(stream) => stream,
        Err(error) => return Err((error, prepared)),
    };
    // A stream that has never been played and has emitted nothing is exactly `Paused`'s posture,
    // with the terminal zero vacuous. That is the applied posture every fresh binding starts from,
    // and it is why one function — `posture_step` — describes `open`, every rebind and every
    // transport edge with no separate `BindingCommit` table.
    let applied = TransportPosture {
        phase: AudioOutputPhase::Paused,
        seeking: wanted.seeking,
    };
    let step = posture_step(applied, wanted, AudioParkState::HardwareParked);
    if let Err(error) = apply_posture_step(
        &stream,
        &prepared.handle,
        &prepared.control,
        &session.park,
        step,
    ) {
        // `stream` dies here — on the owner thread, which is allowed to wait — and `prepared`
        // goes back to the caller whole, so the decode worker is reaped where the caller decides.
        return Err((AudioOpenError::PlayStream(error.to_string()), prepared));
    }
    session
        .park
        .store(step.park_after(AudioParkState::HardwareParked, wanted));
    let PreparedBinding {
        device: _,
        device_name,
        config: _,
        sample_format: _,
        resources: _,
        configuration,
        worker,
        handle,
        control,
        ring,
        info,
        policy,
        ready: _,
        route,
    } = *prepared;
    Ok(CommittedBinding {
        binding: AudioBinding {
            stream,
            worker,
            handle: handle.clone(),
            control,
            #[cfg(feature = "asio")]
            ring: ring.clone(),
        },
        landing: Landing {
            device: device_name,
            info,
            policy,
            worker: handle,
            ring,
            ring_rate: configuration.sample_rate,
            route_version: route.version,
        },
    })
}
