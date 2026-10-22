//! The song after this one: opened, read and placed ahead, on a thread of its own, so the decode
//! worker can go on into it in the same ring at the sample after the last one (gapless).
//!
//! The worker is told it through [`WorkerHandle::post_next`] and takes it only while it is still
//! the one the host wants (`AudioSession::next_wanted`); the media loop takes up the boundary once
//! its clock reaches it (`AudioEdge::Followed`).

use super::binding::{decoded_track, DecodedTrack};
use super::device::AudioOutputHost;
use super::error::AudioOpenError;
use super::player::AudioNext;
use super::pump::{open_native_audio, DecoderPump};
use super::session::{AudioSession, AudioSource, StopBound};
use super::track::{
    dsd_sample_rate, is_dsd_codec, requested_stream_index, AudioOutputEncoding, AudioStreamIndex,
    AudioStreamInfo,
};
use super::worker::WorkerHandle;
use crate::NormalizedTimestamp;
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// What a song that follows is, beside its decoder: kept by the worker while it plays, to put it
/// back in line when a seek goes back over the boundary.
#[derive(Clone)]
pub(super) struct NextMeta {
    pub(super) serial: u64,
    /// Where in its container the song starts (a CUE track's), zero for its beginning.
    pub(super) start: Duration,
    /// The host's name for it (`AudioNext::id`).
    pub(super) id: u64,
    /// Its length, as the host read it.
    pub(super) duration: Option<Duration>,
    pub(super) info: AudioStreamInfo,
    pub(super) source: AudioSource,
    pub(super) stop: Arc<StopBound>,
    /// How long it overlaps the end of the song before it (`AudioNext::crossfade`).
    pub(super) crossfade: Duration,
}

impl NextMeta {
    /// The first frame a song starting at `start` plays: every frame before it is passed over.
    pub(super) fn target(&self) -> Option<NormalizedTimestamp> {
        (!self.start.is_zero()).then(|| NormalizedTimestamp::from_position(self.start))
    }
}

/// A song opened ahead, waiting for the one before it to end.
pub(super) struct NextTrack {
    pub(super) pump: DecoderPump,
    pub(super) meta: NextMeta,
}

/// What the ring the next song goes into is: the binding's, as the media loop knows it.
pub(super) struct RingShape {
    pub(super) info: AudioStreamInfo,
    pub(super) sample_rate: NonZeroU32,
    pub(super) channels: NonZeroUsize,
}

/// Whether a song read at `rate` with `codec` can follow in this ring, or needs the ordinary
/// switch: a DSD output (a ring of DSD bytes or DoP); on ASIO, where the device follows the
/// source's rate, a song at another rate, or a DSD one (the ordinary switch plays it natively).
pub(super) fn can_follow(current: &AudioStreamInfo, rate: u32, codec: &str) -> bool {
    let dsd = is_dsd_codec(codec);
    let on_asio = current.output_host == Some(AudioOutputHost::Asio);
    current.output_encoding == AudioOutputEncoding::Pcm
        && !(on_asio && (dsd || rate != current.source_sample_rate))
}

/// Opens `factory` for the ring `shape` describes and places it at `start`: the song, ready to
/// follow, or why it cannot.
pub(super) fn open_next(
    next: AudioNext,
    serial: u64,
    shape: &RingShape,
    session: &AudioSession,
) -> Result<NextTrack, AudioOpenError> {
    let AudioNext {
        factory,
        track,
        id,
        duration,
        start,
        stop,
        crossfade,
    } = next;
    let source = AudioSource {
        factory,
        stream_index: requested_stream_index(track),
        source_rate: Arc::new(AtomicU32::new(0)),
    };
    // No generation guards this open: it holds no device, and a song no longer wanted is let
    // go by the worker, which looks at `next_wanted` before it takes one.
    let bytes = source.factory.open().map_err(AudioOpenError::Source)?;
    let bytes = crate::source::NativeMediaByteSource::new(bytes);
    let output_layout = shape.info.output_layout;
    let (decoder, native_info, codec) = open_native_audio(
        bytes,
        source.stream_index,
        shape.sample_rate,
        u16::try_from(shape.channels.get()).map_err(|_| {
            AudioOpenError::InvalidOutputConfig("the ring's channels exceed the native ABI".into())
        })?,
    )?;
    let rate = source.note_read_rate(native_info.source_sample_rate);
    if !can_follow(&shape.info, rate, &codec) {
        return Err(AudioOpenError::InvalidOutputConfig(format!(
            "{codec} at {rate} Hz cannot follow in this output"
        )));
    }
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
        (shape.sample_rate, shape.channels),
        session,
    )?;
    let stop = Arc::new(StopBound::new(stop, duration));
    let info = AudioStreamInfo {
        stream_index: AudioStreamIndex::opened(native_info.stream_index)?,
        codec: codec.clone(),
        timing,
        source_sample_rate,
        source_channels,
        source_layout,
        source_layout_assumed: source_layout.assumed(),
        mix,
        stage_delay_frames,
        dsd_sample_rate: dsd_sample_rate(&codec, source_sample_rate),
        ..shape.info.clone()
    };
    let mut pump = DecoderPump::new(
        decoder,
        timing,
        shape.sample_rate,
        shape.channels,
        session.metrics.clone(),
        stop.clone(),
    );
    pump.mixer = mixer;
    pump.dsp = dsp;
    let meta = NextMeta {
        serial,
        start,
        id,
        duration,
        info,
        source,
        stop,
        crossfade,
    };
    if !start.is_zero() {
        pump.seek(start).map_err(|error| {
            AudioOpenError::Worker(format!("the next song did not seek to its start: {error}"))
        })?;
    }
    pump.aim(meta.target());
    Ok(NextTrack { pump, meta })
}

/// Opens the next song on a thread of its own and hands it to `worker`. A song that does not
/// open, or cannot follow, is simply not handed over: the one before it ends as it would have,
/// and the host switches the ordinary way.
pub(super) fn spawn_next(
    next: AudioNext,
    serial: u64,
    shape: RingShape,
    session: AudioSession,
    worker: WorkerHandle,
) {
    let spawned = thread::Builder::new()
        .name("player-core-audio-next".to_owned())
        .spawn(move || {
            if session.next_wanted.load(Ordering::Acquire) != serial {
                return;
            }
            match open_next(next, serial, &shape, &session) {
                Ok(track) => worker.post_next(Box::new(track)),
                Err(error) => {
                    #[cfg(feature = "instrument")]
                    eprintln!(
                        "player-core[audio] the next song will not follow gaplessly: {error}"
                    );
                    #[cfg(not(feature = "instrument"))]
                    let _ = error;
                }
            }
        });
    // A process that cannot start a thread switches the ordinary way.
    drop(spawned);
}
