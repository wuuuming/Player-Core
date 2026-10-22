use std::time::Duration;

const NANOS_PER_SECOND: i128 = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamTimeBase {
    pub numerator: i32,
    pub denominator: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamTiming {
    pub time_base: StreamTimeBase,
    pub start_time: Option<i64>,
    /// Maps the stream-local zero point onto the normalized presentation timeline.
    /// For an ordinary file this is `stream.start_time - format.start_time` after
    /// both values have been converted to nanoseconds.
    pub presentation_offset_nanoseconds: i128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NormalizedTimestamp(i128);

impl NormalizedTimestamp {
    pub const fn from_nanoseconds(nanoseconds: i128) -> Self {
        Self(nanoseconds)
    }

    pub fn from_position(position: Duration) -> Self {
        Self(position.as_nanos() as i128)
    }

    pub fn as_nanoseconds(self) -> i128 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimestampError {
    InvalidTimeBase,
    UnreconcilableOrigin,
    NormalizationOverflow,
    PositionOutOfRange,
}

impl std::fmt::Display for TimestampError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTimeBase => formatter.write_str("stream time base must be positive"),
            Self::UnreconcilableOrigin => formatter.write_str(
                "stream and container origins must both be observed before tracks can be reconciled",
            ),
            Self::NormalizationOverflow => {
                formatter.write_str("presentation timestamp normalization overflowed")
            }
            Self::PositionOutOfRange => {
                formatter.write_str("media position is outside the stream timestamp range")
            }
        }
    }
}

impl std::error::Error for TimestampError {}

pub fn presentation_offset_nanoseconds(
    time_base: StreamTimeBase,
    stream_start_time: Option<i64>,
    format_start_time_us: Option<i64>,
) -> Result<i128, TimestampError> {
    validate_time_base(time_base)?;
    match (stream_start_time, format_start_time_us) {
        (Some(stream_start), Some(format_start_us)) => {
            let stream_start_ns = ticks_to_nanoseconds(stream_start, time_base)?;
            let format_start_ns = i128::from(format_start_us)
                .checked_mul(1_000)
                .ok_or(TimestampError::NormalizationOverflow)?;
            stream_start_ns
                .checked_sub(format_start_ns)
                .ok_or(TimestampError::NormalizationOverflow)
        }
        // Neither states an origin (a WAV's header carries no timestamps): the container and its
        // stream are one timeline from zero, so there is nothing to reconcile. Half a pair is
        // still refused: one side's origin with the other's unknown cannot be placed.
        (None, None) => Ok(0),
        _ => Err(TimestampError::UnreconcilableOrigin),
    }
}

pub fn normalize_timestamp(
    raw_pts: i64,
    timing: StreamTiming,
) -> Result<NormalizedTimestamp, TimestampError> {
    validate_time_base(timing.time_base)?;

    let relative_ticks = raw_pts
        .checked_sub(timing.start_time.unwrap_or(0))
        .ok_or(TimestampError::NormalizationOverflow)?;
    let local_nanoseconds = ticks_to_nanoseconds(relative_ticks, timing.time_base)?;
    Ok(NormalizedTimestamp(
        local_nanoseconds
            .checked_add(timing.presentation_offset_nanoseconds)
            .ok_or(TimestampError::NormalizationOverflow)?,
    ))
}

pub fn raw_timestamp_for_position(
    position: Duration,
    timing: StreamTiming,
) -> Result<i64, TimestampError> {
    let StreamTimeBase {
        numerator,
        denominator,
    } = timing.time_base;
    validate_time_base(timing.time_base)?;

    let presentation_nanoseconds =
        i128::try_from(position.as_nanos()).map_err(|_| TimestampError::PositionOutOfRange)?;
    let local_nanoseconds = presentation_nanoseconds
        .checked_sub(timing.presentation_offset_nanoseconds)
        .ok_or(TimestampError::PositionOutOfRange)?;
    let ticks = local_nanoseconds
        .checked_mul(i128::from(denominator))
        .ok_or(TimestampError::PositionOutOfRange)?
        / i128::from(numerator)
            .checked_mul(NANOS_PER_SECOND)
            .ok_or(TimestampError::PositionOutOfRange)?;
    let raw = ticks
        .checked_add(i128::from(timing.start_time.unwrap_or(0)))
        .ok_or(TimestampError::PositionOutOfRange)?;
    i64::try_from(raw).map_err(|_| TimestampError::PositionOutOfRange)
}

fn validate_time_base(time_base: StreamTimeBase) -> Result<(), TimestampError> {
    if time_base.numerator <= 0 || time_base.denominator <= 0 {
        Err(TimestampError::InvalidTimeBase)
    } else {
        Ok(())
    }
}

fn ticks_to_nanoseconds(ticks: i64, time_base: StreamTimeBase) -> Result<i128, TimestampError> {
    validate_time_base(time_base)?;
    let numerator = i128::from(ticks)
        .checked_mul(i128::from(time_base.numerator))
        .and_then(|value| value.checked_mul(NANOS_PER_SECOND))
        .ok_or(TimestampError::NormalizationOverflow)?;
    Ok(numerator / i128::from(time_base.denominator))
}
