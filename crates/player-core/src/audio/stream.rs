//! The decode worker's side of a binding's ring: the stream's processing, which every song the
//! worker decodes goes through one after another (`crate::dsp::stream_chain`), then the ring.
//! Where one song overlaps the end of the one before (a crossfade), the two are summed first.

use super::ring::AudioRing;
use crate::dsp::{Crossfade, DspChain};
use std::num::NonZeroUsize;
use std::sync::Arc;

pub struct StreamWriter {
    pub ring: Arc<AudioRing>,
    /// The stream's chain; `None` without effects, and for DSD, which bypasses it.
    chain: Option<DspChain>,
    block: Vec<f32>,
    /// Frames given since the generation started (the binding's start, or a seek): where on its
    /// clock the next one given stands, counted from its anchor.
    given: usize,
    /// The crossfade under way: what is given is the coming song, summed with the going one.
    fade: Option<Box<Fade>>,
}

/// A crossfade under way. The going song's frames from the handover on wait in a ring of their
/// own, which the worker fills from its pump before the coming song is given (`going`), so the
/// sum never waits on a decoder: the coming song is taken only as far as the going one is there.
struct Fade {
    mix: Crossfade,
    going: StreamWriter,
    /// The going song has given all it has: the rest of the span is the coming one alone.
    going_ended: bool,
    taken: Vec<f32>,
}

impl Fade {
    /// How many frames of the coming song can be summed now: every one, once the going song's are
    /// there for the rest of the span, or it has ended.
    fn ready(&self) -> usize {
        let available = self.going.ring.available_frames();
        if self.going_ended || available >= self.mix.left() {
            usize::MAX
        } else {
            available
        }
    }

    /// Sums the going song's next frames into `coming`.
    fn mix(&mut self, coming: &mut [f32], channels: usize) {
        let ring = &self.going.ring;
        let (floor, available) = ring.consumer_window();
        let frames = (coming.len() / channels)
            .min(self.mix.left())
            .min(available);
        self.taken.clear();
        for frame in 0..frames {
            for channel in 0..channels {
                self.taken
                    .push(ring.sample(floor.wrapping_add(frame), channel));
            }
        }
        ring.commit_read(floor, frames);
        self.mix.mix(coming, &self.taken, channels);
    }
}

impl StreamWriter {
    pub fn new(ring: Arc<AudioRing>, chain: Option<DspChain>) -> Self {
        Self {
            ring,
            chain,
            block: Vec::new(),
            given: 0,
            fade: None,
        }
    }

    /// A new generation (a seek): the ring's frames are void, the chain forgets what it heard,
    /// a crossfade under way is over, and the count starts again.
    pub(super) fn restart(&mut self) {
        self.ring.discard_all();
        self.fade = None;
        if let Some(chain) = &mut self.chain {
            chain.reset();
        }
        self.given = 0;
    }

    /// Frames given since the generation started, those still inside the chain included.
    pub(super) fn given(&self) -> usize {
        self.given
    }

    /// Gives whole interleaved frames, as many as the ring has room for, and in a crossfade as
    /// the going song's are there to sum them with: how many it took. What the chain gives back
    /// for them is never more, so all of it goes into the ring.
    pub(super) fn push_pcm(&mut self, samples: &[f32]) -> usize {
        let channels = self.ring.channels;
        let mut frames = (samples.len() / channels).min(self.ring.free_frames());
        if let Some(fade) = &self.fade {
            frames = frames.min(fade.ready());
        }
        if self.chain.is_none() && self.fade.is_none() {
            let pushed = self.ring.push_interleaved(&samples[..frames * channels]);
            self.given += pushed;
            return pushed;
        }
        self.block.clear();
        self.block.extend_from_slice(&samples[..frames * channels]);
        if let Some(fade) = &mut self.fade {
            fade.mix(&mut self.block, channels);
            if fade.mix.left() == 0 {
                self.fade = None;
            }
        }
        if let Some(chain) = &mut self.chain {
            chain.process(&mut self.block);
        }
        let pushed = self.ring.push_interleaved(&self.block);
        debug_assert_eq!(pushed * channels, self.block.len());
        self.given += frames;
        frames
    }

    /// Gives DSD bytes, as many whole frames as the ring has room for: how many frames it took.
    pub(super) fn push_dsd(&mut self, bytes: &[u8]) -> usize {
        let pushed = self.ring.push_dsd(bytes);
        self.given += pushed;
        pushed
    }

    /// The sound's end: a crossfade still under way ends with it (the coming song was shorter
    /// than the span), and what the chain still holds goes into the ring, once the ring has room
    /// for all of it. Nothing to do a second time.
    pub(super) fn finish(&mut self) {
        self.fade = None;
        let Some(chain) = &mut self.chain else {
            return;
        };
        if chain.held() == 0 || self.ring.free_frames() < chain.held() {
            return;
        }
        let tail = chain.flush();
        self.ring.push_interleaved(&tail);
    }

    /// A song's boundary: the chain goes on into the next song, renewed first if its stages
    /// changed (`DspChain::renew`), the song before's last frames then flushed into the ring.
    /// `false` while the ring has no room for them yet: the boundary waits for it.
    pub(super) fn renew(&mut self) -> bool {
        let Some(chain) = &mut self.chain else {
            return true;
        };
        if !chain.stale() {
            return true;
        }
        if self.ring.free_frames() < chain.held() {
            return false;
        }
        let tail = chain.renew();
        self.ring.push_interleaved(&tail);
        true
    }
}

impl StreamWriter {
    /// The handover to a song that overlaps the end of the one before for `frames`: what is given
    /// from here is the coming song, summed with the going one's frames (`going`) under the span's
    /// gains. A crossfade still under way ends here: the coming song it brought in was shorter
    /// than its span.
    pub(super) fn begin_fade(&mut self, frames: usize) {
        self.fade = None;
        let (Some(capacity), Some(channels)) = (
            NonZeroUsize::new(self.ring.capacity_frames),
            NonZeroUsize::new(self.ring.channels),
        ) else {
            return;
        };
        // The main ring's own shape: the going song's ring never holds more than it.
        let Ok(ring) = AudioRing::new(capacity, channels) else {
            return;
        };
        if frames > 0 {
            self.fade = Some(Box::new(Fade {
                mix: Crossfade::new(frames),
                going: StreamWriter::new(Arc::new(ring), None),
                going_ended: false,
                taken: Vec::new(),
            }));
        }
    }

    /// Whether a crossfade is under way.
    pub(super) fn fading(&self) -> bool {
        self.fade.is_some()
    }

    /// The going song's side of the crossfade under way, for its pump to refill.
    pub(super) fn going(&mut self) -> Option<&mut StreamWriter> {
        self.fade.as_mut().map(|fade| &mut fade.going)
    }

    /// The going song has given all it has: the coming one finishes the span alone.
    pub(super) fn going_ended(&mut self) {
        if let Some(fade) = &mut self.fade {
            fade.going_ended = true;
        }
    }
}
