//! The real-time output callback and the lock-free control seam it acknowledges on. Nothing
//! reachable from `CallbackCore` may block; the unit battery holds that as a law.

use super::clock::{AudioClock, PLAYBACK_RATE_SCALE};
use super::error::AudioError;
use super::metrics::{AudioCallbackWindow, SharedMetrics};
#[cfg(feature = "instrument")]
use super::metrics::{
    CALLBACKS_IN_FLIGHT, LAST_CALLBACK_PERIOD_NANOSECONDS, LIVE_CALLBACK_CLOSURES,
};
use super::policy::{AudioGainCurve, AudioPolicy};
use super::ring::AudioRing;
use crate::dsp::{one_pole, one_pole_coefficient, USER_GAIN_SMOOTHING};
use cpal::{FromSample, OutputCallbackInfo, Sample};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread::Thread;
use std::time::{Duration, Instant};
use windows::Win32::System::Threading::{WaitOnAddress, WakeByAddressAll};

pub(super) const MODE_HELD: u8 = 0;
pub(super) const MODE_RUNNING: u8 = 1;
pub(super) const CALLBACK_CONTROL_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum RampKind {
    Start = 1,
    Pause = 2,
    Resume = 3,
    SeekDown = 4,
    SeekUp = 5,
    Dispose = 6,
}

impl RampKind {
    pub(super) const fn evidence_bit(self) -> u32 {
        1 << (self as u8 - 1)
    }
}

/// The one seam between the audio-owner thread and the cpal callback.
///
/// Six atomics and no lock. The callback is a real-time thread: it must never take a mutex the
/// owner thread can hold, and the media loop must never take one the callback can hold.
///
/// The owner thread waits for an acknowledgement on an address (`WaitOnAddress` on `signal`),
/// not by polling: the callback's side is a store and `WakeByAddressAll`, which takes no lock the
/// waiter holds and returns at once.
pub(super) struct CallbackControl {
    /// Bumped by the owner thread AFTER `requested_mode` and `requested_kind` are in place, with
    /// Release ordering. The callback acquires on it, so it can never read half a request.
    pub(super) request_version: AtomicU64,
    /// Written only by the callback, and only upward.
    pub(super) acknowledged_version: AtomicU64,
    pub(super) requested_mode: AtomicU8,
    pub(super) requested_kind: AtomicU8,
    /// cpal reported this binding's device dead, or its decode worker failed. Sticky, and it dies
    /// with the binding that raised it because the binding owns the `Arc`.
    pub(super) failed: AtomicBool,
    /// Bumped, after the fact it announces, by every acknowledgement and by `fail`: the word the
    /// owner thread waits on, so neither can land between its check and its wait unseen.
    signal: AtomicU32,
}

impl CallbackControl {
    pub(super) fn new() -> Self {
        Self {
            request_version: AtomicU64::new(0),
            acknowledged_version: AtomicU64::new(0),
            requested_mode: AtomicU8::new(MODE_HELD),
            requested_kind: AtomicU8::new(RampKind::Start as u8),
            failed: AtomicBool::new(false),
            signal: AtomicU32::new(0),
        }
    }

    /// Publishes a ramp request without waiting for it. Callable from any thread; it is three
    /// stores and never blocks.
    pub(super) fn publish(&self, mode: u8, kind: RampKind) -> Result<u64, AudioError> {
        if self.failed.load(Ordering::Acquire) {
            return Err(AudioError::OutputFailed);
        }
        self.requested_mode.store(mode, Ordering::Relaxed);
        self.requested_kind.store(kind as u8, Ordering::Relaxed);
        Ok(self.request_version.fetch_add(1, Ordering::Release) + 1)
    }

    /// Publishes and then waits for the callback to acknowledge delivery.
    ///
    /// AUDIO-OWNER THREAD ONLY. It is the one thread in this file whose progress nothing else
    /// depends on; the media loop must never reach it.
    pub(super) fn request(&self, mode: u8, kind: RampKind) -> Result<(), AudioError> {
        self.request_with_timeout(mode, kind, CALLBACK_CONTROL_TIMEOUT)
    }

    pub(super) fn request_with_timeout(
        &self,
        mode: u8,
        kind: RampKind,
        timeout: Duration,
    ) -> Result<(), AudioError> {
        let version = self.publish(mode, kind)?;
        self.await_acknowledgement(version, timeout)
    }

    pub(super) fn await_acknowledgement(
        &self,
        version: u64,
        timeout: Duration,
    ) -> Result<(), AudioError> {
        let deadline = Instant::now() + timeout;
        loop {
            // Read before the facts: a change after this read moves `signal` off `seen`, and the
            // wait below then returns at once.
            let seen = self.signal.load(Ordering::Acquire);
            if self.failed.load(Ordering::Acquire) {
                return Err(AudioError::OutputFailed);
            }
            if self.acknowledged_version.load(Ordering::Acquire) >= version {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AudioError::ControlTimeout);
            }
            // Whole milliseconds, rounded up, so the wait does not end before the deadline.
            let millis = remaining
                .as_nanos()
                .div_ceil(1_000_000)
                .min(u128::from(u32::MAX - 1));
            // SAFETY: both pointers are to live `u32`s for the call; `size` matches them. A
            // timeout or a spurious return only loops back to the checks above.
            let _ = unsafe {
                WaitOnAddress(
                    self.signal.as_ptr().cast_const().cast(),
                    (&raw const seen).cast(),
                    size_of::<u32>(),
                    millis as u32,
                )
            };
        }
    }

    /// cpal callback only, wait-free.
    pub(super) fn acknowledge(&self, version: u64) {
        self.acknowledged_version
            .fetch_max(version, Ordering::AcqRel);
        self.wake();
    }

    pub(super) fn fail(&self) {
        self.failed.store(true, Ordering::Release);
        self.wake();
    }

    /// Moves `signal` and wakes the owner thread if it waits on it. Never blocks.
    fn wake(&self) {
        self.signal.fetch_add(1, Ordering::Release);
        // SAFETY: the address of a live `u32`; waking an address nobody waits on is a no-op.
        unsafe { WakeByAddressAll(self.signal.as_ptr().cast_const().cast()) };
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct GainRamp {
    pub(super) start: f32,
    pub(super) target: f32,
    pub(super) frame: u32,
    pub(super) frames: NonZeroU32,
    pub(super) curve: AudioGainCurve,
}

impl GainRamp {
    pub(super) fn new(start: f32, target: f32, frames: NonZeroU32, curve: AudioGainCurve) -> Self {
        Self {
            start,
            target,
            frame: 0,
            frames,
            curve,
        }
    }

    pub(super) fn next(&mut self) -> (f32, bool) {
        let denominator = self.frames.get() - 1;
        let position = self.frame.min(denominator) as f32 / denominator as f32;
        let shaped = match self.curve {
            AudioGainCurve::Linear => position,
        };
        let gain = self.start + (self.target - self.start) * shaped;
        self.frame = self.frame.saturating_add(1);
        (gain, self.frame >= self.frames.get())
    }
}

pub(super) use crate::dsp::UserGain;

#[cfg(feature = "instrument")]
pub(super) struct CallbackClosureLifetime;

#[cfg(feature = "instrument")]
impl CallbackClosureLifetime {
    pub(super) fn new() -> Self {
        LIVE_CALLBACK_CLOSURES.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

#[cfg(feature = "instrument")]
impl Drop for CallbackClosureLifetime {
    fn drop(&mut self) {
        LIVE_CALLBACK_CLOSURES.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(feature = "instrument")]
pub(super) struct CallbackInFlight;

#[cfg(feature = "instrument")]
impl CallbackInFlight {
    pub(super) fn enter() -> Self {
        CALLBACKS_IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

#[cfg(feature = "instrument")]
impl Drop for CallbackInFlight {
    fn drop(&mut self) {
        CALLBACKS_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy)]
pub(super) struct CallbackMoment {
    pub(super) observed_at: Instant,
    pub(super) delivery_delay: Duration,
}

impl CallbackMoment {
    /// When the `frames`-th frame given in this callback is heard (the device's delay after it
    /// was asked for, then the frames before it at `rate`).
    pub(super) fn heard_after(self, frames: usize, rate: NonZeroU32) -> Instant {
        self.observed_at
            .checked_add(self.delivery_delay)
            .and_then(|instant| instant.checked_add(frames_to_duration(frames, rate)))
            .unwrap_or(self.observed_at)
    }

    pub(super) fn from_cpal(info: &OutputCallbackInfo) -> Self {
        let timestamp = info.timestamp();
        Self {
            observed_at: Instant::now(),
            delivery_delay: timestamp
                .playback
                .duration_since(&timestamp.callback)
                .unwrap_or_default(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct RampCapture {
    pub(super) start: f32,
    pub(super) target: f32,
    pub(super) first: Option<f32>,
    pub(super) last: Option<f32>,
    pub(super) frames: u64,
    pub(super) monotonic: bool,
    pub(super) interior: bool,
}

impl RampCapture {
    pub(super) fn new(start: f32, target: f32) -> Self {
        Self {
            start,
            target,
            first: None,
            last: None,
            frames: 0,
            monotonic: true,
            interior: false,
        }
    }

    pub(super) fn observe(&mut self, gain: f32) {
        if self.first.is_none() {
            self.first = Some(gain);
        }
        if let Some(previous) = self.last {
            self.monotonic &= if self.target >= self.start {
                gain >= previous
            } else {
                gain <= previous
            };
        }
        self.interior |= if self.target > self.start {
            gain > self.start && gain < self.target
        } else if self.target < self.start {
            gain < self.start && gain > self.target
        } else {
            false
        };
        self.last = Some(gain);
        self.frames = self.frames.saturating_add(1);
    }

    pub(super) fn valid(self) -> bool {
        self.first == Some(self.start)
            && self.last == Some(self.target)
            && self.frames >= 3
            && self.monotonic
            && (self.start == self.target || self.interior)
    }
}

pub(super) struct ActiveRamp {
    pub(super) gain: GainRamp,
    pub(super) version: u64,
    pub(super) kind: RampKind,
    pub(super) capture: RampCapture,
    pub(super) target_mode: u8,
    pub(super) consume_media: bool,
}

#[derive(Clone)]
pub(super) struct CallbackResources {
    pub(super) ring: Arc<AudioRing>,
    pub(super) clock: Arc<AudioClock>,
    pub(super) control: Arc<CallbackControl>,
    pub(super) metrics: Arc<SharedMetrics>,
    pub(super) user_gain: Arc<UserGain>,
    pub(super) worker_thread: Thread,
}

#[derive(Clone, Copy)]
pub(super) struct CallbackConfiguration {
    pub(super) policy: AudioPolicy,
    pub(super) channels: usize,
    pub(super) sample_rate: NonZeroU32,
}

pub(super) struct CallbackCore {
    pub(super) ring: Arc<AudioRing>,
    pub(super) clock: Arc<AudioClock>,
    pub(super) control: Arc<CallbackControl>,
    pub(super) metrics: Arc<SharedMetrics>,
    pub(super) user_gain: Arc<UserGain>,
    pub(super) worker_thread: Thread,
    pub(super) channels: usize,
    pub(super) sample_rate: NonZeroU32,
    pub(super) refill_low_frames: usize,
    pub(super) ramp_frames: NonZeroU32,
    pub(super) leave_frames: NonZeroU32,
    pub(super) gain_curve: AudioGainCurve,
    pub(super) observed_version: u64,
    pub(super) mode: u8,
    pub(super) gain: f32,
    /// The user's volume as the samples have it: one-pole smoothed toward `user_gain`, so a
    /// slider dragged is a glide and not a staircase (zipper noise). `None` until the first
    /// callback, which starts at the volume set.
    pub(super) user_level: Option<f32>,
    pub(super) user_smoothing: f32,
    pub(super) ramp: Option<ActiveRamp>,
    pub(super) pending_delivery_ack: Option<(u64, Instant)>,
    pub(super) source_phase_frames: f64,
    pub(super) media_time_remainder: u128,
    /// Media time this callback has delivered for `observed_clock_generation`. Private to the
    /// callback: the clock no longer owns a shared elapsed counter that a second thread can add
    /// to across a reanchor.
    pub(super) delivered_nanoseconds: u64,
    pub(super) observed_clock_generation: u64,
    /// The second of device time being gathered for `SharedMetrics::publish_window`.
    pub(super) window: AudioCallbackWindow,
    pub(super) last_callback_at: Option<Instant>,
}

impl CallbackCore {
    const FRESH_WINDOW: AudioCallbackWindow = AudioCallbackWindow {
        sequence: 0,
        callbacks: 0,
        frames: 0,
        min_frames: u64::MAX,
        max_frames: 0,
        max_interval_ns: 0,
        max_execution_ns: 0,
        slow_callbacks: 0,
        ring_floor_frames: u64::MAX,
    };

    /// Adds one callback to the window, and publishes the window once it spans a second of
    /// device time.
    fn gather_window(&mut self, frames: usize, observed_at: Instant, left: usize) {
        let execution = observed_at.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        let period = frames_to_duration(frames, self.sample_rate)
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64;
        let window = &mut self.window;
        window.max_execution_ns = window.max_execution_ns.max(execution);
        window.slow_callbacks += u64::from(execution > period / 2);
        window.callbacks += 1;
        window.frames += frames as u64;
        window.min_frames = window.min_frames.min(frames as u64);
        window.max_frames = window.max_frames.max(frames as u64);
        if let Some(last) = self.last_callback_at {
            let interval = observed_at.saturating_duration_since(last).as_nanos();
            window.max_interval_ns = window
                .max_interval_ns
                .max(interval.min(u128::from(u64::MAX)) as u64);
        }
        self.last_callback_at = Some(observed_at);
        window.ring_floor_frames = window.ring_floor_frames.min(left as u64);
        if window.frames >= u64::from(self.sample_rate.get()) {
            self.metrics.publish_window(window);
            self.window = Self::FRESH_WINDOW;
        }
    }

    pub(super) fn new(resources: CallbackResources, configuration: CallbackConfiguration) -> Self {
        let observed_clock_generation = resources.clock.generation();
        Self {
            ring: resources.ring,
            clock: resources.clock,
            control: resources.control,
            metrics: resources.metrics,
            user_gain: resources.user_gain,
            worker_thread: resources.worker_thread,
            channels: configuration.channels,
            sample_rate: configuration.sample_rate,
            refill_low_frames: configuration.policy.refill_low_frames.get(),
            ramp_frames: configuration.policy.ramp_frames,
            leave_frames: configuration.policy.leave_frames,
            gain_curve: configuration.policy.gain_curve,
            observed_version: 0,
            mode: MODE_HELD,
            gain: 0.0,
            user_level: None,
            user_smoothing: one_pole_coefficient(USER_GAIN_SMOOTHING, configuration.sample_rate),
            ramp: None,
            pending_delivery_ack: None,
            source_phase_frames: 0.0,
            media_time_remainder: 0,
            delivered_nanoseconds: 0,
            observed_clock_generation,
            window: Self::FRESH_WINDOW,
            last_callback_at: None,
        }
    }

    pub(super) fn render<T>(&mut self, output: &mut [T], moment: CallbackMoment)
    where
        T: Sample + FromSample<f32>,
    {
        #[cfg(feature = "instrument")]
        let _in_flight = CallbackInFlight::enter();
        let requested_frames = output.len() / self.channels;
        #[cfg(feature = "instrument")]
        crate::record_audio_callback();
        #[cfg(feature = "instrument")]
        LAST_CALLBACK_PERIOD_NANOSECONDS.store(
            frames_to_duration(requested_frames, self.sample_rate)
                .as_nanos()
                .min(u128::from(u64::MAX)) as u64,
            Ordering::Release,
        );
        self.metrics.callback_calls.fetch_add(1, Ordering::Relaxed);
        let equilibrium = T::from_sample(0.0);
        output.fill(equilibrium);

        if let Some((version, delivered_at)) = self.pending_delivery_ack {
            if moment.observed_at >= delivered_at {
                self.pending_delivery_ack = None;
                self.metrics
                    .terminal_zero_delivery_acks
                    .fetch_add(1, Ordering::Relaxed);
                self.complete_transition(version);
            }
        }
        self.observe_request();

        self.metrics
            .device_frames_requested
            .fetch_add(requested_frames as u64, Ordering::Relaxed);
        let clock_generation = self.clock.generation();
        if clock_generation != self.observed_clock_generation {
            self.observed_clock_generation = clock_generation;
            self.source_phase_frames = 0.0;
            self.media_time_remainder = 0;
            self.delivered_nanoseconds = 0;
        }
        // The playback rate is how fast the ring is consumed: the clock follows it with no seek or
        // stream rebuild, but the pitch moves with it. Keeping the pitch takes a stage before the
        // ring (FFmpeg's atempo), not work here, where nothing may allocate or block.
        let rate_q32 = self.clock.playback_rate_q32();
        let playback_rate = rate_q32 as f64 / PLAYBACK_RATE_SCALE as f64;
        let (read_start, available) = self.ring.consumer_window();
        let user_target = self.user_gain.get();
        let mut user_level = self.user_level.unwrap_or(user_target);
        // A phase >= 1 represents source frames that a faster-than-1x callback still has to skip
        // once the decoder publishes them. Drain those frames before sampling the next output.
        let pending_skip = (self.source_phase_frames.floor() as usize).min(available);
        let mut consumed = pending_skip;
        self.source_phase_frames -= pending_skip as f64;
        let mut committed_output_frames = 0usize;
        let mut committed_rate_units = 0u128;
        let mut underrun = 0usize;
        let mut clipped = 0u64;

        for frame in 0..requested_frames {
            let consuming = self.mode == MODE_RUNNING
                || self.ramp.as_ref().is_some_and(|ramp| ramp.consume_media);
            let source_offset = self.source_phase_frames.floor() as usize;
            let has_media = consuming
                && consumed
                    .checked_add(source_offset)
                    .is_some_and(|source| source < available);
            let frame_gain = if let Some(active) = self.ramp.as_mut() {
                let (gain, complete) = active.gain.next();
                active.capture.observe(gain);
                self.gain = gain;
                if let Some(active) = complete.then(|| self.ramp.take()).flatten() {
                    self.metrics
                        .ramp_sample_frames
                        .fetch_add(active.capture.frames, Ordering::Relaxed);
                    if active.capture.valid() {
                        self.metrics
                            .ramp_evidence_mask
                            .fetch_or(active.kind.evidence_bit(), Ordering::Relaxed);
                    } else {
                        self.metrics
                            .ramp_shape_failures
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    self.gain = active.gain.target;
                    self.mode = active.target_mode;
                    if self.mode == MODE_HELD {
                        let delivered_at =
                            moment.heard_after(frame.saturating_add(1), self.sample_rate);
                        self.pending_delivery_ack = Some((active.version, delivered_at));
                    } else {
                        self.complete_transition(active.version);
                    }
                }
                gain
            } else {
                self.gain
            };
            user_level = one_pole(user_level, user_target, self.user_smoothing);

            if has_media {
                let absolute_frame = read_start
                    .wrapping_add(consumed)
                    .wrapping_add(source_offset);
                let remaining = available - consumed - source_offset;
                let next_frame = if remaining > 1 {
                    absolute_frame.wrapping_add(1)
                } else {
                    absolute_frame
                };
                let fraction = self.source_phase_frames.fract() as f32;
                let output_start = frame * self.channels;
                for channel in 0..self.channels {
                    let current = self.ring.sample(absolute_frame, channel);
                    let next = self.ring.sample(next_frame, channel);
                    let resampled = current + (next - current) * fraction;
                    let value = resampled * frame_gain * user_level;
                    clipped += u64::from(value.abs() > 1.0);
                    output[output_start + channel] = T::from_sample(value);
                }
                self.source_phase_frames += playback_rate;
                committed_output_frames += 1;
                committed_rate_units = committed_rate_units.saturating_add(u128::from(rate_q32));
            } else if consuming {
                underrun += 1;
            }
        }

        self.user_level = Some(user_level);
        let available_after_prefix = available.saturating_sub(consumed);
        let source_advance =
            (self.source_phase_frames.floor() as usize).min(available_after_prefix);
        consumed += source_advance;
        self.source_phase_frames -= source_advance as f64;
        if consumed != 0 {
            self.ring.commit_read(read_start, consumed);
            self.metrics
                .media_frames_committed
                .fetch_add(consumed as u64, Ordering::Relaxed);
        }
        if committed_output_frames != 0 {
            // The rate of the device this callback is actually serving: a rebind can change it, so
            // it is not read off the clock, which outlives every device.
            let denominator = u128::from(self.sample_rate.get()) * u128::from(PLAYBACK_RATE_SCALE);
            let numerator = self
                .media_time_remainder
                .saturating_add(committed_rate_units.saturating_mul(1_000_000_000));
            let committed_nanoseconds = numerator / denominator;
            self.media_time_remainder = numerator % denominator;
            self.delivered_nanoseconds = self
                .delivered_nanoseconds
                .saturating_add(committed_nanoseconds.min(u128::from(u64::MAX)) as u64);
        }
        let steady = self.mode == MODE_RUNNING && self.ramp.is_none();
        self.metrics
            .record_callback_supply(underrun, available.saturating_sub(consumed), steady);
        if clipped != 0 {
            self.metrics
                .clipped_samples
                .fetch_add(clipped, Ordering::Relaxed);
        }
        let left = available.saturating_sub(consumed);
        // One publication per callback, stamped with the generation this callback served. The
        // worker's version and this delivery are separate single-writer seqlocks, so no reader can
        // pair this elapsed time with a different generation's anchor. The device's own
        // playback-vs-callback delay rides along: only the callback is told it, and only a reader
        // holding both can name the position the listener is actually at.
        self.clock.publish_delivery(
            self.observed_clock_generation,
            self.delivered_nanoseconds,
            moment.delivery_delay.as_nanos().min(u128::from(u64::MAX)) as u64,
        );
        if self.ring.available_frames() < self.refill_low_frames {
            self.worker_thread.unpark();
        }
        // Last, so the time it measures is this callback's whole.
        self.gather_window(requested_frames, moment.observed_at, left);
    }

    pub(super) fn observe_request(&mut self) {
        let version = self.control.request_version.load(Ordering::Acquire);
        if version == self.observed_version {
            return;
        }
        self.observed_version = version;
        let target_mode = self.control.requested_mode.load(Ordering::Relaxed);
        let kind = match self.control.requested_kind.load(Ordering::Relaxed) {
            value if value == RampKind::Start as u8 => RampKind::Start,
            value if value == RampKind::Pause as u8 => RampKind::Pause,
            value if value == RampKind::Resume as u8 => RampKind::Resume,
            value if value == RampKind::SeekDown as u8 => RampKind::SeekDown,
            value if value == RampKind::SeekUp as u8 => RampKind::SeekUp,
            _ => RampKind::Dispose,
        };
        let target_gain = if target_mode == MODE_RUNNING {
            1.0
        } else {
            0.0
        };
        let frames = match kind {
            // A song that sounds is left with a fade over the leave, as far as the sound decoded
            // ahead reaches (never into an underrun); one held silent, at once.
            RampKind::Dispose if self.gain > 0.0 => {
                let held = u32::try_from(self.ring.available_frames()).unwrap_or(u32::MAX);
                NonZeroU32::new(self.leave_frames.get().min(held))
                    .map_or(self.ramp_frames, |frames| frames.max(self.ramp_frames))
            }
            _ => self.ramp_frames,
        };
        self.ramp = Some(ActiveRamp {
            gain: GainRamp::new(self.gain, target_gain, frames, self.gain_curve),
            version,
            kind,
            capture: RampCapture::new(self.gain, target_gain),
            target_mode,
            consume_media: target_mode == MODE_RUNNING
                || self.mode == MODE_RUNNING
                || self.gain > 0.0,
        });
        self.pending_delivery_ack = None;
        self.metrics
            .ramp_transitions_started
            .fetch_add(1, Ordering::Relaxed);
        match kind {
            RampKind::Start => &self.metrics.start_ramps,
            RampKind::Pause => &self.metrics.pause_ramps,
            RampKind::Resume => &self.metrics.resume_ramps,
            RampKind::SeekDown => &self.metrics.seek_down_ramps,
            RampKind::SeekUp => &self.metrics.seek_up_ramps,
            RampKind::Dispose => &self.metrics.dispose_ramps,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn complete_transition(&self, version: u64) {
        self.metrics
            .ramp_transitions_completed
            .fetch_add(1, Ordering::Relaxed);
        self.control.acknowledge(version);
    }
}

pub(super) fn frames_to_duration(frames: usize, sample_rate: NonZeroU32) -> Duration {
    let nanoseconds =
        (frames as u128).saturating_mul(1_000_000_000) / u128::from(sample_rate.get());
    Duration::from_nanos(nanoseconds.min(u128::from(u64::MAX)) as u64)
}
