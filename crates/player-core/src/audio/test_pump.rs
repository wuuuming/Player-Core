//! A decoder pump over a file, for the tests (this crate's, and the separately maintained
//! ones through `crate::internals`): only with `test-support`.

use super::pump::{open_native_audio, DecoderPump};
use super::{metrics::SharedMetrics, session::StopBound};
use crate::source::NativeMediaByteSource;
use crate::{presentation_offset_nanoseconds, StreamTimeBase, StreamTiming};
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::Arc;

const RATE: NonZeroU32 = match NonZeroU32::new(48_000) {
    Some(rate) => rate,
    None => unreachable!(),
};
const STEREO: NonZeroUsize = match NonZeroUsize::new(2) {
    Some(channels) => channels,
    None => unreachable!(),
};

/// The decoder pump over `media`, at 48 kHz stereo, stopping where `stop` says.
pub fn pump_over(
    media: &std::path::Path,
    metrics: &Arc<SharedMetrics>,
    stop: Arc<StopBound>,
) -> DecoderPump {
    let bytes = NativeMediaByteSource::new(Box::new(
        crate::FileMediaByteSource::open(media)
            .unwrap_or_else(|e| panic!("{}: {e}", media.display())),
    ));
    let (decoder, info, _) = open_native_audio(bytes, -1, RATE, 2)
        .unwrap_or_else(|e| panic!("a native audio decoder: {e:?}"));
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
        .unwrap_or_else(|e| panic!("the presentation offset: {e:?}")),
    };
    DecoderPump::new(decoder, timing, RATE, STEREO, Arc::clone(metrics), stop)
}
