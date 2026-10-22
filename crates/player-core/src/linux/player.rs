use super::output::{capturing, NULL_OUTPUT};
use super::source::is_network;
use super::types::{AudioError, AudioErrorKind, TrackHint};
use super::{playback, probe, process};
use crate::dsp::{
    song_chain, song_end, stream_chain, within_end, DspChain, ReplayGainSetting, UserGain,
};
use crate::{AudioInput, AudioSourceFactory, ReplayGain, ReplayGainTags};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum Terminal {
    Pending,
    Eof,
    Failed,
}

/// Writer-owned resources can outlive a cancelled controller until cleanup completes.
/// Child cancellation and PCM feeder cancellation do not depend on that library returning.
pub(super) struct Run {
    pub(super) stop: Arc<AtomicBool>,
    /// Set before `stop` when the player is disposed: the writer fades the sound out over the
    /// leave, not a pause's declick.
    pub(super) leaving: AtomicBool,
    /// The cancellation tokens of the PCM feeders the run's decoders started, and the slots of
    /// their children: one decoder, and a second while the next song's is started ahead.
    feeds: Mutex<Vec<Arc<AtomicBool>>>,
    cancelled: Arc<AtomicBool>,
    children: Mutex<Vec<Arc<process::ChildSlot>>>,
    /// The song the player wants to follow this one (`AudioPlayer::set_next`), shared with it:
    /// the writer starts its decoder ahead and goes on into it at this one's end.
    pub(super) next: Arc<Mutex<Option<Arc<Following>>>>,
    /// The boundaries the writer has crossed: the song after each, and where in the run's heard
    /// time it becomes the one playing (its first frame, the start of an overlap with the song
    /// before), in microseconds. The player takes each up once it is heard.
    pub(super) boundaries: Mutex<VecDeque<(Arc<Following>, u64)>>,
    pub(super) heard_us: AtomicU64,
    terminal: AtomicU8,
    error: std::sync::Mutex<Option<AudioError>>,
    pub(super) started: Instant,
    pub(super) last_progress_us: AtomicU64,
    /// Nothing heard yet, or nothing new for `STALL_US`: set by the writer, which looks at it
    /// on every pass of its loop (`settle_buffering`).
    buffering: AtomicBool,
    /// The player's wake: called when `terminal` or `buffering` changes, so its host has nothing
    /// to poll.
    wake: Arc<dyn Fn() + Send + Sync>,
}

/// How long the heard position may stand still while playing before it reads as buffering.
const STALL_US: u64 = 300_000;

impl Run {
    fn new(
        cancelled: Arc<AtomicBool>,
        wake: Arc<dyn Fn() + Send + Sync>,
        next: Arc<Mutex<Option<Arc<Following>>>>,
    ) -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            leaving: AtomicBool::new(false),
            feeds: Mutex::default(),
            cancelled,
            children: Mutex::default(),
            next,
            boundaries: Mutex::default(),
            heard_us: AtomicU64::new(0),
            terminal: AtomicU8::new(Terminal::Pending as u8),
            error: std::sync::Mutex::new(None),
            started: Instant::now(),
            last_progress_us: AtomicU64::new(0),
            buffering: AtomicBool::new(true),
            wake,
        }
    }
    /// Whether what is heard has stalled, from the progress the writer recorded; announced when
    /// it changes. The writer calls it after every look at the output.
    pub(super) fn settle_buffering(&self) {
        let since = (self.started.elapsed().as_micros() as u64)
            .saturating_sub(self.last_progress_us.load(Ordering::Relaxed));
        let stalled = self.heard_us.load(Ordering::Relaxed) == 0 || since > STALL_US;
        if self.buffering.swap(stalled, Ordering::AcqRel) != stalled {
            self.announce();
        }
    }
    /// When the heard position, standing still from now on, would read as buffering; `None`
    /// while it already does.
    pub(super) fn stall_at(&self) -> Option<Instant> {
        if self.buffering.load(Ordering::Acquire) {
            return None;
        }
        let progress = self.last_progress_us.load(Ordering::Relaxed);
        Some(self.started + Duration::from_micros(progress + STALL_US + 1))
    }
    /// A slot for a decoder's child, killed with the run.
    pub(super) fn child_slot(&self) -> Arc<process::ChildSlot> {
        let slot = Arc::<process::ChildSlot>::default();
        self.children
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(slot.clone());
        slot
    }
    /// A PCM feeder's token, stopped with the run.
    pub(super) fn add_feed(&self, token: Arc<AtomicBool>) {
        self.feeds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(token);
    }
    /// Stops the run's feeders and kills its decoders. The writer sees `stop` and ends.
    fn terminate(&self) {
        self.stop.store(true, Ordering::Release);
        for feed in self
            .feeds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            feed.store(true, Ordering::Release);
        }
        for child in self
            .children
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            child.terminate();
        }
    }
    /// The writer went on into `song`, which starts at `at` of the heard time: the player is
    /// woken to take it up when it is heard.
    pub(super) fn cross(&self, song: Arc<Following>, at: u64) {
        self.boundaries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back((song, at));
        self.announce();
    }

    /// The player is woken to look at what changed (buffering, a boundary); a cancelled run
    /// wakes nobody.
    fn announce(&self) {
        if !self.cancelled() {
            (self.wake)();
        }
    }
    /// Whether a boundary the writer crossed is still to be taken up.
    fn boundary_ahead(&self) -> Option<u64> {
        self.boundaries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .front()
            .map(|(_, at)| *at)
    }
    pub(super) fn cancelled(&self) -> bool {
        self.stop.load(Ordering::Acquire) || self.cancelled.load(Ordering::Acquire)
    }
    pub(super) fn terminal(&self) -> Terminal {
        match self.terminal.load(Ordering::Acquire) {
            1 => Terminal::Eof,
            2 => Terminal::Failed,
            _ => Terminal::Pending,
        }
    }
    pub(super) fn fail(&self, error: AudioError) {
        *self.error.lock().unwrap_or_else(PoisonError::into_inner) = Some(error);
        self.finish(Terminal::Failed);
    }
    pub(super) fn finish(&self, terminal: Terminal) {
        if !self.cancelled() {
            self.terminal.store(terminal as u8, Ordering::Release);
            (self.wake)();
        }
    }
}

/// A decoder's span from `from` to the song's `end`: where it starts, and how long it reads.
fn span_to(from: Duration, end: Option<Duration>) -> (Duration, Option<Duration>) {
    (from, end.map(|end| end.saturating_sub(from)))
}

/// A factory's cancel flag, or a fresh one when it has none.
fn cancel_flag(factory: &dyn AudioSourceFactory) -> Arc<AtomicBool> {
    factory
        .cancellation()
        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)))
}

/// A song's own chain (its ReplayGain from `tags`) at the stream's rate and channels.
fn song_dsp(replay_gain: Arc<ReplayGainSetting>, tags: ReplayGainTags) -> Arc<Mutex<DspChain>> {
    Arc::new(Mutex::new(song_chain(
        replay_gain,
        tags,
        playback::CHAIN_RATE,
        playback::CHAIN_CHANNELS,
    )))
}

/// A song to go on into, ready: what its decoder reads, from where to where, and its own chain.
pub(super) struct Following {
    pub(super) serial: u64,
    pub(super) id: u64,
    pub(super) input: AudioInput,
    pub(super) start: Duration,
    pub(super) stop: Option<Duration>,
    pub(super) duration: Option<Duration>,
    pub(super) dsp: Arc<Mutex<DspChain>>,
    /// How long it overlaps the end of the song before it (`AudioNext::crossfade`).
    pub(super) crossfade: Duration,
}

impl Following {
    /// Where its decoder starts and for how long it reads, as a run's span.
    pub(super) fn span(&self) -> (Duration, Option<Duration>) {
        span_to(self.start, song_end(self.stop, self.duration))
    }
}

/// The song to go on into when this one ends, in the same stream: without a gap (gapless), or
/// overlapping this one's end (a crossfade). Its first sample follows this one's last, or the
/// first of their overlap; one that is not ready in time is not followed, and this one ends as it
/// would have.
#[derive(Clone)]
pub struct AudioNext {
    /// The host's own name for it, which [`AudioPlayer::followed`] reports once the sound has
    /// gone on into it.
    pub id: u64,
    pub factory: Arc<dyn AudioSourceFactory>,
    pub track: TrackHint,
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

struct PlaybackRun {
    state: Arc<Run>,
    worker: std::thread::JoinHandle<()>,
}

/// How a session opens: the Windows backend's options, less what this one has no use for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioOpenOptions {
    pub track: TrackHint,
    /// The container's length, when the host's tags probe read it.
    pub duration: Option<Duration>,
    /// The source's own rate; this backend resamples to the sound server's anyway.
    pub source_rate: Option<u32>,
    /// Which ReplayGain value the sound plays at.
    pub replay_gain: ReplayGain,
    /// The CLAP effects the sound goes through, after ReplayGain.
    pub effects: Option<crate::clap::Effects>,
}

pub struct AudioPlayer {
    input: AudioInput,
    /// Position at `since` (absolute in the file), and whether the clock runs. With sound,
    /// `at` is where the current run started and the run reports how much of it was heard.
    at: Duration,
    since: Instant,
    playing: bool,
    duration: Option<Duration>,
    stop: Option<Duration>,
    volume: Arc<UserGain>,
    replay_gain: Arc<ReplayGainSetting>,
    /// The song's own processing (`crate::dsp::song_chain`), which every run of it goes through,
    /// reset when a run starts; the run's writer holds it while it processes a block. The effects
    /// are the stream's: each run makes its own chain of them.
    dsp: Arc<Mutex<DspChain>>,
    run: Option<PlaybackRun>,
    cancelled: Arc<AtomicBool>,
    terminal: Terminal,
    wake: Arc<dyn Fn() + Send + Sync>,
    /// Explicit null output only; device failures must never enable a fake playback clock.
    silent: bool,
    /// The run's heard time at which the playing song started (the run went on into it): what
    /// the run has heard counts for this song from there.
    song_origin: Duration,
    /// The song to follow this one once ready, shared with every run; and the serial of the one
    /// wanted, which a probe that finishes late compares its own against.
    next: Arc<Mutex<Option<Arc<Following>>>>,
    next_wanted: Arc<AtomicU64>,
    /// The id of the song the run last went on into (`followed`).
    followed: Option<u64>,
    effects: Option<crate::clap::Effects>,
}

impl AudioPlayer {
    pub fn open(
        factory: Arc<dyn AudioSourceFactory>,
        track: TrackHint,
        wake: Arc<dyn Fn() + Send + Sync>,
        duration: Option<Duration>,
    ) -> Result<Self, AudioError> {
        let options = AudioOpenOptions {
            track,
            duration,
            ..AudioOpenOptions::default()
        };
        Self::open_with(factory, wake, options)
    }

    pub fn open_with(
        factory: Arc<dyn AudioSourceFactory>,
        wake: Arc<dyn Fn() + Send + Sync>,
        options: AudioOpenOptions,
    ) -> Result<Self, AudioError> {
        let duration = options.duration;
        let cancelled = cancel_flag(factory.as_ref());
        if cancelled.load(Ordering::Acquire) {
            return Err(AudioError::new(AudioErrorKind::Cancelled, "open cancelled"));
        }
        let input = factory.input().ok_or_else(|| {
            AudioError::new(
                AudioErrorKind::Source,
                format!("{}: a byte-only source cannot play here", factory.label()),
            )
        })?;
        match &input {
            AudioInput::Path(p) if !is_network(&input) && !p.exists() => {
                return Err(AudioError::new(
                    AudioErrorKind::Source,
                    format!("file not found: {}", p.display()),
                ));
            }
            AudioInput::Pcm(source) if !source.has_format() => {
                return Err(AudioError::new(
                    AudioErrorKind::Source,
                    "PCM sample rate and channel count must not be zero",
                ));
            }
            _ => {}
        }
        let (duration, tags) = probe_input(&input, duration, &cancelled);
        if cancelled.load(Ordering::Acquire) {
            return Err(AudioError::new(AudioErrorKind::Cancelled, "open cancelled"));
        }
        // PulseAudio connection can itself block. It belongs to the owned worker, never open().
        let silent = NULL_OUTPUT.load(Ordering::Relaxed) && !capturing();
        let replay_gain = Arc::new(ReplayGainSetting::default());
        replay_gain.set(options.replay_gain);
        let dsp = song_dsp(replay_gain.clone(), tags);
        Ok(AudioPlayer {
            input,
            at: Duration::ZERO,
            since: Instant::now(),
            playing: false,
            duration,
            stop: None,
            volume: Arc::new(UserGain::default()),
            replay_gain,
            dsp,
            run: None,
            cancelled,
            terminal: Terminal::Pending,
            wake,
            silent,
            song_origin: Duration::ZERO,
            next: Arc::default(),
            next_wanted: Arc::default(),
            followed: None,
            effects: options.effects,
        })
    }
    fn end(&self) -> Option<Duration> {
        song_end(self.stop, self.duration)
    }
    /// A position is never past the song's end, when it is known.
    fn within_end(&self, position: Duration) -> Duration {
        within_end(position, self.end())
    }
    /// Where the playing song is, from what `run` has heard.
    fn heard(&self, run: &Run) -> Duration {
        (self.at + Duration::from_micros(run.heard_us.load(Ordering::Relaxed)))
            .saturating_sub(self.song_origin)
    }
    /// The song ends by the clock alone: no output to report its end (silent), or a stop bound
    /// before the decoder's.
    fn ends_by_clock(&self) -> bool {
        self.silent || self.stop.is_some()
    }
    pub fn position(&self, now: Instant) -> Duration {
        let p = match (&self.run, self.playing) {
            (Some(run), true) => self.heard(&run.state),
            (None, true) if self.silent => self.at + now.saturating_duration_since(self.since),
            _ => self.at,
        };
        self.within_end(p)
    }
    /// Starts decoding and writing from `at`.
    fn start(&mut self) -> Result<(), AudioError> {
        self.halt();
        if self.cancelled.load(Ordering::Acquire) {
            return Err(AudioError::new(
                AudioErrorKind::Cancelled,
                "playback cancelled",
            ));
        }
        self.terminal = Terminal::Pending;
        if self.silent {
            return Ok(());
        }
        let run = Arc::new(Run::new(
            self.cancelled.clone(),
            self.wake.clone(),
            self.next.clone(),
        ));
        let (r, input, from) = (run.clone(), self.input.clone(), self.at);
        let (_, limit) = span_to(from, self.end());
        let (dsp, effects, volume) = (self.dsp.clone(), self.effects.clone(), self.volume.clone());
        let h = std::thread::Builder::new()
            .name("pulse-out".into())
            .spawn(move || {
                // The run's own stream chain, made on its thread: activating the effects may
                // wait on the plugin host thread.
                let stream = stream_chain(
                    effects.as_ref(),
                    playback::CHAIN_RATE,
                    playback::CHAIN_CHANNELS,
                );
                let processing = playback::Processing::new(dsp, stream, volume);
                playback::write_run(r, &input, (from, limit), processing)
            })
            .map_err(|e| {
                AudioError::new(
                    AudioErrorKind::Output,
                    format!("cannot start sound output: {e}"),
                )
            })?;
        self.run = Some(PlaybackRun {
            state: run,
            worker: h,
        });
        Ok(())
    }
    /// Stops the writer, keeping the position it reached.
    fn halt(&mut self) {
        self.take_up();
        if let Some(run) = self.run.take() {
            let r = &run.state;
            let position = self.heard(r);
            self.song_origin = Duration::ZERO;
            r.terminate();
            // The worker owns output and cleanup. Never join an unfinished operation from
            // the controller; cancellation prevents subsequent writes.
            if run.worker.is_finished() {
                let _ = run.worker.join();
            }
            if self.playing {
                self.at = self.within_end(position);
            }
        }
    }
    pub fn play(&mut self) -> Result<(), AudioError> {
        if !self.playing {
            self.since = Instant::now();
            self.start()?;
            self.playing = true;
        }
        Ok(())
    }
    /// Stops the transport where it is heard: the writer halted (keeping the position it
    /// reached) or, with none, the position as it stands; then not playing. The one way playing
    /// ends: `halt` keeps the position only while playing.
    fn stop_here(&mut self) {
        if self.run.is_some() {
            self.halt();
        } else {
            self.at = self.position(Instant::now());
        }
        self.playing = false;
    }
    pub fn pause(&mut self) -> Result<(), AudioError> {
        self.stop_here();
        Ok(())
    }
    pub fn seek(&mut self, to: Duration) -> Result<(), AudioError> {
        let was = self.playing;
        self.stop_here();
        self.at = self.within_end(to);
        self.terminal = Terminal::Pending;
        self.since = Instant::now();
        if was {
            self.start()?;
            self.playing = true;
        }
        Ok(())
    }
    pub fn set_stop(&mut self, s: Option<Duration>) {
        self.stop = s;
    }

    /// The song to go on into when this one ends; `None` takes back the one set before. It is
    /// probed and its decoder started ahead, and the sound goes on into it at the sample after
    /// this one's last: [`Self::followed`] names it, and from there on position, duration and
    /// stop are that song's, and the next one after it is set again.
    pub fn set_next(&mut self, next: Option<AudioNext>) {
        let serial = self.next_wanted.fetch_add(1, Ordering::AcqRel) + 1;
        *self.next.lock().unwrap_or_else(PoisonError::into_inner) = None;
        let Some(next) = next else {
            return;
        };
        let (slot, wanted) = (self.next.clone(), self.next_wanted.clone());
        let replay_gain = self.replay_gain.clone();
        // Probing reads the file: not on the host's thread. One that does not start is not
        // followed, and this song ends as it would have.
        let _ = std::thread::Builder::new()
            .name("audio-next".into())
            .spawn(move || {
                let cancelled = cancel_flag(next.factory.as_ref());
                // A byte-only source is not followed: this song ends as it would have.
                let Some(input) = next.factory.input() else {
                    return;
                };
                let (duration, tags) = probe_input(&input, next.duration, &cancelled);
                let dsp = song_dsp(replay_gain, tags);
                let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
                if wanted.load(Ordering::Acquire) == serial && !cancelled.load(Ordering::Acquire) {
                    *slot = Some(Arc::new(Following {
                        serial,
                        id: next.id,
                        input,
                        start: next.start,
                        stop: next.stop,
                        duration,
                        dsp,
                        crossfade: next.crossfade,
                    }));
                }
            });
    }

    /// The [`AudioNext::id`] of the song set with [`Self::set_next`] the sound last went on into;
    /// `None` until it has. A host that sees it change shows that song as the one playing.
    pub fn followed(&self) -> Option<u64> {
        self.followed
    }

    /// Takes up the boundaries the run has crossed and that are heard: from each on, the song
    /// after it is the one playing.
    fn take_up(&mut self) {
        let Some(run) = self.run.as_ref() else {
            return;
        };
        let state = run.state.clone();
        let heard = state.heard_us.load(Ordering::Relaxed);
        loop {
            let crossed = {
                let mut boundaries = state
                    .boundaries
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                match boundaries.front() {
                    Some((_, at)) if *at <= heard => boundaries.pop_front(),
                    _ => None,
                }
            };
            let Some((song, at)) = crossed else {
                return;
            };
            self.input = song.input.clone();
            self.duration = song.duration;
            self.stop = song.stop;
            self.dsp = song.dsp.clone();
            self.at = song.start;
            self.song_origin = Duration::from_micros(at);
            self.followed = Some(song.id);
            let mut slot = self.next.lock().unwrap_or_else(PoisonError::into_inner);
            if slot.as_ref().is_some_and(|next| next.serial == song.serial) {
                *slot = None;
            }
        }
    }

    /// The user's volume, a factor of the sound (0 to 1, as on Windows); the writer moves to it
    /// smoothly, frame by frame.
    pub fn set_volume(&self, v: f32) {
        self.volume.set(v);
    }
    pub fn volume(&self) -> f32 {
        self.volume.get()
    }
    /// The song's length: the host's, else what the probe read.
    pub fn duration(&self) -> Option<Duration> {
        self.duration
    }
    /// Which ReplayGain value the sound plays at; heard from the next block, moved to without a
    /// step.
    pub fn set_replay_gain(&self, mode: ReplayGain) {
        self.replay_gain.set(mode);
    }

    pub fn replay_gain(&self) -> ReplayGain {
        self.replay_gain.get()
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }
    pub fn buffering(&self) -> bool {
        self.playing
            && self.run.as_ref().is_some_and(|run| {
                let r = &run.state;
                r.terminal() == Terminal::Pending && r.buffering.load(Ordering::Acquire)
            })
    }
    pub fn ended(&self) -> bool {
        if self.cancelled.load(Ordering::Acquire) {
            return false;
        }
        let terminal = self
            .run
            .as_ref()
            .map_or(self.terminal, |run| run.state.terminal());
        // A boundary not taken up yet: `service` takes it up, and the song after it plays.
        if self
            .run
            .as_ref()
            .is_some_and(|run| run.state.boundary_ahead().is_some())
        {
            return false;
        }
        match terminal {
            Terminal::Eof => true,
            Terminal::Failed => false,
            Terminal::Pending => {
                // A decoder must report success; a duration hint cannot turn a failed or
                // truncated network stream into EOF. Clock-only output has no decoder.
                self.ends_by_clock()
                    && self
                        .end()
                        .is_some_and(|e| self.position(Instant::now()) >= e)
            }
        }
    }
    pub fn service(&mut self, now: Instant) -> Result<(), AudioError> {
        if self.cancelled.load(Ordering::Acquire) {
            self.stop_here();
            self.terminal = Terminal::Pending;
            return Err(AudioError::new(
                AudioErrorKind::Cancelled,
                "playback cancelled",
            ));
        }
        self.take_up();
        let failed = self
            .run
            .as_ref()
            .is_some_and(|run| run.state.terminal() == Terminal::Failed);
        if self.playing && (self.ended() || failed) {
            // Preserve delivered/heard samples, not a possibly wrong duration hint.
            let at = self.position(now);
            let error = self.run.as_ref().and_then(|run| {
                run.state
                    .error
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone()
            });
            self.stop_here();
            self.at = at;
            // EOF must survive removal of Run, including streams with unknown duration.
            self.terminal = if failed {
                Terminal::Failed
            } else {
                Terminal::Eof
            };
            if failed {
                return Err(error.unwrap_or_else(|| {
                    AudioError::new(AudioErrorKind::Decoder, "decoder worker stopped")
                }));
            }
        }
        Ok(())
    }
    /// When the song ends while it plays, where only the clock says so (`ended`): a null output,
    /// or a span's end. Otherwise the writer announces the end, buffering and failure itself.
    pub fn next_wake_at(&self, now: Instant) -> Option<Instant> {
        if !self.playing {
            return None;
        }
        // A boundary the writer crossed: heard when the run has heard up to it.
        if let Some(run) = self.run.as_ref() {
            if let Some(at) = run.state.boundary_ahead() {
                let heard = run.state.heard_us.load(Ordering::Relaxed);
                // What is heard moves on as the writer samples the output: not sooner than
                // that looks again.
                let left = Duration::from_micros(at.saturating_sub(heard));
                return Some(now + left.max(Duration::from_millis(5)));
            }
        }
        if !self.ends_by_clock() {
            return None;
        }
        let e = self.end()?;
        Some(now + e.saturating_sub(self.position(now)))
    }
    pub fn dispose(&mut self) -> Result<(), AudioError> {
        if let Some(run) = &self.run {
            run.state.leaving.store(true, Ordering::Release);
        }
        self.stop_here();
        Ok(())
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.halt();
    }
}

#[cfg(test)]
mod tests;

/// The length (the host's, else the input's own) and the ReplayGain tags of what `input` plays.
fn probe_input(
    input: &AudioInput,
    duration: Option<Duration>,
    cancelled: &AtomicBool,
) -> (Option<Duration>, ReplayGainTags) {
    match input {
        AudioInput::Pcm(source) => (
            duration.or_else(|| source.duration()),
            ReplayGainTags::default(),
        ),
        AudioInput::Path(p) => probe::for_playback(p, duration, cancelled),
        AudioInput::Url(u, _) => probe::for_playback(std::path::Path::new(u), duration, cancelled),
    }
}
