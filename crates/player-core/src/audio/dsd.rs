//! DSD byte delivery for native ASIO output. One queue frame is eight one-bit samples per channel.

use super::callback::{
    frames_to_duration, CallbackConfiguration, CallbackMoment, CallbackResources, MODE_HELD,
    MODE_RUNNING,
};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

pub(super) struct DsdCallback {
    resources: CallbackResources,
    configuration: CallbackConfiguration,
    version: u64,
    mode: u8,
    delivery_ack: Option<(u64, Instant)>,
    generation: u64,
    delivered_frames: u64,
    last_media_write: Option<(Instant, Duration)>,
}

impl DsdCallback {
    pub(super) fn new(resources: CallbackResources, configuration: CallbackConfiguration) -> Self {
        let generation = resources.clock.generation();
        Self {
            resources,
            configuration,
            version: 0,
            mode: MODE_HELD,
            delivery_ack: None,
            generation,
            delivered_frames: 0,
            last_media_write: None,
        }
    }

    pub(super) fn render(&mut self, output: &mut [u8], moment: CallbackMoment) {
        // DSD silence has equal numbers of ones and zeroes. PCM zero would be full scale DC.
        output.fill(0x69);
        let resources = &self.resources;
        let channels = self.configuration.channels;
        let requested = output.len() / channels;
        resources
            .metrics
            .callback_calls
            .fetch_add(1, Ordering::Relaxed);
        resources
            .metrics
            .device_frames_requested
            .fetch_add(requested as u64, Ordering::Relaxed);
        if let Some((version, due)) = self.delivery_ack {
            if moment.observed_at >= due {
                resources.control.acknowledge(version);
                resources
                    .metrics
                    .terminal_zero_delivery_acks
                    .fetch_add(1, Ordering::Relaxed);
                self.delivery_ack = None;
            }
        }
        let version = resources.control.request_version.load(Ordering::Acquire);
        if version != self.version {
            self.version = version;
            self.mode = resources.control.requested_mode.load(Ordering::Relaxed);
            self.delivery_ack = if self.mode == MODE_HELD {
                // Seek may reuse ring slots only after both previously queued device buffers
                // have been replaced and their final byte has reached the output.
                Some((
                    version,
                    moment.heard_after(requested, self.configuration.sample_rate),
                ))
            } else {
                resources.control.acknowledge(version);
                None
            };
        }
        let generation = resources.clock.generation();
        if generation != self.generation {
            self.generation = generation;
            self.delivered_frames = 0;
            self.last_media_write = None;
        }
        let (read, available) = resources.ring.consumer_window();
        if self.mode == MODE_RUNNING && !resources.control.failed.load(Ordering::Acquire) {
            let consumed = requested.min(available);
            // Zero volume substitutes DSD idle bytes without stopping source consumption. Every
            // nonzero volume preserves the bitstream at unity; the writer keeps its wire format
            // and DoP marker phase through both transitions.
            if resources.user_gain.get() != 0.0 {
                for frame in 0..consumed {
                    for channel in 0..channels {
                        output[frame * channels + channel] =
                            resources.ring.dsd_sample(read.wrapping_add(frame), channel);
                    }
                }
            }
            resources.ring.commit_read(read, consumed);
            self.delivered_frames = self.delivered_frames.saturating_add(consumed as u64);
            if consumed != 0 {
                self.last_media_write = Some((moment.observed_at, moment.delivery_delay));
            }
            resources
                .metrics
                .media_frames_committed
                .fetch_add(consumed as u64, Ordering::Relaxed);
            let silent = if resources.metrics.eof_reached.load(Ordering::Acquire) {
                0
            } else {
                requested - consumed
            };
            resources
                .metrics
                .record_callback_supply(silent, available - consumed, true);
        }
        let elapsed = frames_to_duration(
            self.delivered_frames as usize,
            self.configuration.sample_rate,
        )
        .as_nanos() as u64;
        // Underrun or held silence consumes no media time, but the last media in the driver reaches
        // the output. Let its outstanding delay expire instead of freezing short of the span end.
        let delay = self
            .last_media_write
            .map_or(Duration::ZERO, |(written_at, delay)| {
                delay.saturating_sub(moment.observed_at.saturating_duration_since(written_at))
            });
        resources.clock.publish_delivery(
            self.generation,
            elapsed,
            delay.as_nanos().min(u128::from(u64::MAX)) as u64,
        );
        if self
            .configuration
            .policy
            .ring_low(resources.ring.available_frames())
        {
            resources.worker_thread.unpark();
        }
    }
}
