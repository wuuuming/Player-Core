//! The seek hold transaction: what one seek promises and what an unadopted one leaves behind.

use super::clock::GenerationCounter;
use super::park::AudioParkState;
use super::posture::AudioOutputPhase;
use std::time::Duration;

/// The pre-seek posture and the position this transaction will leave media time at.
///
/// It carries no generation on purpose: a generation the worker has already seen must never come
/// back. It does carry the target, because the target is the transport's *promise* while the
/// media clock still shows the old anchor's *observation* — and a rebind that starts from the
/// observation puts the sound before the seek while the transport shows its target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SeekHoldTransaction {
    pub(super) before_phase: AudioOutputPhase,
    pub(super) before_park: AudioParkState,
    pub(super) target: Duration,
}

impl SeekHoldTransaction {
    pub(super) fn begin(phase: AudioOutputPhase, park: AudioParkState, target: Duration) -> Self {
        Self {
            before_phase: phase,
            before_park: park,
            target,
        }
    }

    /// The transport intent this seek must land in.
    pub(super) fn resumes_running(self) -> bool {
        self.before_phase.is_running()
    }

    /// Refines the promise: the requested position at `begin`, the exact position handed to the
    /// decode worker at `seek_held`, and a superseding seek's position after that.
    pub(super) fn set_target(&mut self, target: Duration) {
        self.target = target;
    }

    pub(super) fn target(self) -> Duration {
        self.target
    }

    /// Rolling back ABANDONS the generation the worker and the media clock have already seen, so
    /// spending a fresh one is part of the same transition rather than a separate statement the
    /// caller must remember: entering a new generation is what makes the next callback reset its
    /// resampler phase. There is no signature here that restores the posture without advancing,
    /// so "a generation handed out twice" cannot be produced by forgetting a line.
    pub(super) fn rollback(
        self,
        generation: &mut GenerationCounter,
    ) -> (AudioOutputPhase, AudioParkState) {
        generation.advance();
        (self.before_phase, self.before_park)
    }
}

/// A media position the transport committed to and no decode worker has adopted.
///
/// `finish_seek` closes the seek transaction, and after it the transport's books say the seek is
/// done — but "done" is a statement about the TRANSPORT, not about any decoder. When the seek was
/// committed while the device was gone, `seek_held` had nobody to tell and returned `Ok(None)`, so
/// the media clock still describes the generation BEFORE the seek. Dropping the promise there is
/// how a rebind would resume at the pre-seek position while the transport shows the target: the
/// divergence `rebind_honours_promise` exists to catch, made invisible because the promise it
/// checks against is itself the wrong one.
///
/// It carries the generation that must carry it, and nothing retires it. `generation` is compared
/// against the media clock's own anchor, and that anchor has one writer — a decode worker inside
/// `DecoderPump::begin_generation` — whose write IS the event "a worker adopted this target".
/// There is no retirement edge for anyone to forget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct UnadoptedSeek {
    pub(super) generation: u64,
    pub(super) target: Duration,
}

impl UnadoptedSeek {
    /// No decode worker has anchored on it yet: the clock, at `clock_generation`, is still
    /// behind it.
    pub(super) fn pending(self, clock_generation: u64) -> bool {
        clock_generation < self.generation
    }
}

/// Where the transport promises audio is. Total and pure over the three facts that can answer it,
/// so "answered from a clock that describes the generation before the seek" is a branch that does
/// not exist rather than a case somebody remembered to handle.
///
/// Newest first: an open transaction is a promise the transport has not finished making; an
/// unadopted seek is one it finished making and no decoder signed for; only when neither stands is
/// the clock an answer.
pub(super) fn promised_position(
    transaction: Option<SeekHoldTransaction>,
    unadopted: Option<UnadoptedSeek>,
    clock_generation: u64,
    clock_position: Duration,
) -> Duration {
    match (transaction, unadopted) {
        (Some(transaction), _) => transaction.target(),
        (None, Some(unadopted)) if unadopted.pending(clock_generation) => unadopted.target,
        (None, _) => clock_position,
    }
}
