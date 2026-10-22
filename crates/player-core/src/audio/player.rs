//! The player of one song: [`AudioOutput`] (the device ladder, the media clock, the held-seek
//! transaction, the posture ramp) with nothing beside it. The clock it publishes IS the song's
//! clock, and a seek is finished the moment it is posted.
//!
//! Like the output it wraps, nothing here waits on the device except [`AudioPlayer::open`], which
//! is the one call a host runs where it may block.

use super::clock::PlaybackRateControl;
use super::device::AudioDeviceSelection;
use super::error::{AudioError, AudioOpenError};
use super::metrics::AudioPlaybackMetrics;
use super::output::AudioOutput;
use super::policy::{AudioPolicy, AudioPolicyTimes};
use super::track::{AudioStreamInfo, AudioTrackSelection};
use crate::dsp::within_end;
use crate::{AudioSourceFactory, ReplayGain};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How a session opens. `Default` is FFmpeg's pick of track, no hints, the provisional policy and
/// no ReplayGain.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioOpenOptions {
    pub track: AudioTrackSelection,
    /// The container's length, as the host's tags probe read it. The output decodes a stream and
    /// does not know where it ends until it gets there.
    pub duration: Option<Duration>,
    /// The source's own sample rate, when the host has read it (its tags' resolution): an ASIO
    /// driver is set to it before the decoder opens.
    pub source_rate: Option<u32>,
    /// How much sound to decode ahead of the device; `None` is [`AudioPolicyTimes::PROVISIONAL`].
    pub policy: Option<AudioPolicyTimes>,
    /// Which ReplayGain value the sound plays at, from its first frame.
    pub replay_gain: ReplayGain,
    /// The CLAP effects the sound goes through, after ReplayGain.
    pub effects: Option<crate::clap::Effects>,
}

/// The song to go on into when this one ends, in the same stream: without a gap (gapless), or
/// overlapping this one's end (a crossfade). Its first sample follows this one's last, or the
/// first of their overlap; a song that cannot (one that does not open in time, a DSD one, one at
/// another rate on ASIO) is not followed, and this one ends as it would have.
#[derive(Clone)]
pub struct AudioNext {
    /// The host's own name for it, which [`AudioPlayer::followed`] reports once the sound has
    /// gone on into it.
    pub id: u64,
    pub factory: Arc<dyn AudioSourceFactory>,
    pub track: AudioTrackSelection,
    /// Its length, as the host's tags probe read it: [`AudioPlayer::duration`] once it plays.
    pub duration: Option<Duration>,
    /// Where in its container it starts (a CUE track), zero for its beginning.
    pub start: Duration,
    /// Where it stops, as [`AudioPlayer::set_stop`] would set it.
    pub stop: Option<Duration>,
    /// How long it overlaps the end of the song before it, the one fading out as it fades in:
    /// zero follows gaplessly. The overlap ends where that song ends, its stop or its length; one
    /// whose end is not known is followed gaplessly, and one that ends before the overlap does
    /// leaves this one to finish fading in alone.
    pub crossfade: Duration,
}

pub struct AudioPlayer {
    output: AudioOutput,
    rate: Arc<PlaybackRateControl>,
    playing: bool,
}

impl AudioPlayer {
    /// Opens `factory`'s sound on the system's default output, paused at zero. Blocks for the
    /// source's open and the device's first bind, so a host that must not block runs it on a
    /// thread of its own; `wake` is called, from any thread, whenever the session has news.
    pub fn open(
        factory: Arc<dyn AudioSourceFactory>,
        track: AudioTrackSelection,
        wake: Arc<dyn Fn() + Send + Sync>,
        duration: Option<Duration>,
    ) -> Result<Self, AudioOpenError> {
        let options = AudioOpenOptions {
            track,
            duration,
            ..AudioOpenOptions::default()
        };
        Self::open_with(factory, wake, options)
    }

    /// [`Self::open`] with everything a host may say about the source and the session.
    pub fn open_with(
        factory: Arc<dyn AudioSourceFactory>,
        wake: Arc<dyn Fn() + Send + Sync>,
        options: AudioOpenOptions,
    ) -> Result<Self, AudioOpenError> {
        let rate = Arc::new(PlaybackRateControl::default());
        let mut output = AudioOutput::open_hinted(
            factory,
            AudioDeviceSelection::Chosen,
            options,
            wake,
            Arc::clone(&rate),
        )?;
        output
            .pause()
            .map_err(|error| AudioOpenError::Prefill(Box::new(error)))?;
        Ok(Self {
            output,
            rate,
            playing: false,
        })
    }

    pub fn play(&mut self) -> Result<(), AudioError> {
        self.output.resume()?;
        self.playing = true;
        Ok(())
    }

    pub fn pause(&mut self) -> Result<(), AudioError> {
        self.output.pause()?;
        self.playing = false;
        Ok(())
    }

    /// What the host asked for, not what the device is doing: a playing session that is
    /// buffering, or rebinding a lost device, is still playing.
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Moves to `position`, clamped to the song's end (its stop, else its length) when it is
    /// known. The decoder is told and not
    /// waited for; until it lands, [`Self::position`] answers the promise rather than the clock.
    pub fn seek(&mut self, position: Duration) -> Result<(), AudioError> {
        let position = within_end(position, self.output.end());
        self.output.begin_seek_hold(position)?;
        match self.output.seek_held_posted(position) {
            Ok(()) => self.output.finish_seek(self.playing),
            Err(error) => {
                self.output.rollback_seek()?;
                Err(error)
            }
        }
    }

    /// Where the sound is: the promised position while a seek is landing, else the media clock.
    /// `now` serves hosts whose clock is the wall's (Linux's null output); this one is the
    /// device's.
    pub fn position(&self, _now: Instant) -> Duration {
        within_end(
            self.output.promised_position().unwrap_or(Duration::ZERO),
            self.output.end(),
        )
    }

    /// The playing song's length, as the host's tags probe read it. The output decodes a stream
    /// and does not know where it ends until it gets there.
    pub fn duration(&self) -> Option<Duration> {
        self.output.duration()
    }

    /// Ends the sound at `stop` of its container, to the sample, instead of at the container's
    /// end: [`Self::ended`] reads true once what comes before it has played. `None` lets it run to
    /// the end again. Moving the bound on before the sound reaches it keeps the sound running
    /// without a break — how one CUE track runs into the next of the same file.
    pub fn set_stop(&mut self, stop: Option<Duration>) {
        self.output.set_stop(stop);
    }

    /// The song to go on into when this one ends; `None` takes back the one set before. It is
    /// opened ahead, and the sound goes on into it at the sample after this one's last, or at
    /// their overlap's start: [`Self::followed`] names it once that is heard, and from there on position, duration, stop and stream info
    /// are that song's, and the next one after it is set again.
    pub fn set_next(&mut self, next: Option<AudioNext>) {
        self.output.set_next(next);
    }

    /// The [`AudioNext::id`] of the song set with [`Self::set_next`] the sound last went on into;
    /// `None` until it has. A host that sees it change shows that song as the one playing.
    pub fn followed(&self) -> Option<u64> {
        self.output.followed()
    }

    pub fn set_volume(&self, volume: f32) {
        self.output.set_volume(volume);
    }

    pub fn volume(&self) -> f32 {
        self.output.volume()
    }

    /// Which ReplayGain value the sound plays at from here on. The decoder works ahead of the
    /// device, so it is heard a ring's length later, moved to without a step.
    pub fn set_replay_gain(&self, mode: ReplayGain) {
        self.output.set_replay_gain(mode);
    }

    pub fn replay_gain(&self) -> ReplayGain {
        self.output.replay_gain()
    }

    /// `false` when `rate` is outside what the output can play.
    pub fn set_playback_rate(&self, rate: f32) -> bool {
        self.rate.set(rate)
    }

    pub fn playback_rate(&self) -> f32 {
        self.rate.get()
    }

    /// The device ladder's one driver: a lost device is rebound from here, at the promised
    /// position. A host calls it whenever it is woken and at [`Self::next_wake_at`], and reads
    /// [`Self::ended`], [`Self::buffering`] and the failure this returns after it: `wake` is
    /// called whenever one of them may have changed, so a host has nothing to poll.
    pub fn service(&mut self, now: Instant) -> Result<(), AudioError> {
        self.output.service_device(now)
    }

    /// When [`Self::service`] is next needed without a wake: the device ladder's deadline while
    /// the device is lost, and the tail. `None` otherwise.
    ///
    /// The tail is the one change nothing announces: once the decoder has reached the end, the
    /// sound is over when the device has played out the ring, and only the real-time callback
    /// sees the ring run dry, and it must not wake anyone. So a playing session with the end
    /// decoded is looked at when the frames still in the ring will have played.
    pub fn next_wake_at(&self, now: Instant) -> Option<Instant> {
        let device = self.output.next_device_wake_at(now);
        let (tail, boundary) = if self.playing {
            (self.output.played_out_at(now), self.output.boundary_at(now))
        } else {
            (None, None)
        };
        [device, tail, boundary].into_iter().flatten().min()
    }

    /// The container has ended and the device has played out what was decoded. Never while a
    /// seek is still landing: the end a host would read then is the end it just seeked AWAY from,
    /// and a host that restarts at the end would restart twice.
    pub fn ended(&self) -> bool {
        !self.output.seek_unadopted() && self.output.eof_drained()
    }

    /// Playing, and waiting on the source for sound it needs now.
    pub fn buffering(&self) -> bool {
        self.playing && self.output.input_starved() && !self.output.input_ended()
    }

    pub fn stream_info(&self) -> &AudioStreamInfo {
        self.output.stream_info()
    }

    pub fn metrics(&self) -> AudioPlaybackMetrics {
        self.output.metrics()
    }

    pub fn policy(&self) -> AudioPolicy {
        self.output.policy()
    }

    /// Gives the device back now rather than at drop. Posted, not waited for; every later call
    /// answers `AudioError::Disposed`, and a second dispose is a no-op.
    pub fn dispose(&mut self) -> Result<(), AudioError> {
        self.playing = false;
        self.output.dispose()
    }
}
