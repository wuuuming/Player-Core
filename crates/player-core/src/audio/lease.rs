//! The output authority: exactly one live session may own the device, and the lease is what
//! says so.

use super::error::AudioOpenError;
#[cfg(feature = "instrument")]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub(super) static NEXT_OUTPUT_OWNER: AtomicU64 = AtomicU64::new(1);
pub(super) static OUTPUT_OWNER: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "instrument")]
pub(super) static LIVE_OUTPUT_AUTHORITIES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "instrument")]
pub(super) static MAX_LIVE_OUTPUT_AUTHORITIES: AtomicUsize = AtomicUsize::new(0);

/// How long an opening session waits for the previous one to let go of the audio output.
pub(super) const OUTPUT_AUTHORITY_WAIT: Duration = Duration::from_secs(3);

pub(super) struct OutputAuthorityLease {
    pub(super) owner: u64,
}

impl OutputAuthorityLease {
    pub(super) fn acquire() -> Result<Self, AudioOpenError> {
        let owner = NEXT_OUTPUT_OWNER.fetch_add(1, Ordering::Relaxed);
        match OUTPUT_OWNER.compare_exchange(0, owner, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                #[cfg(feature = "instrument")]
                let live = LIVE_OUTPUT_AUTHORITIES.fetch_add(1, Ordering::AcqRel) + 1;
                #[cfg(feature = "instrument")]
                MAX_LIVE_OUTPUT_AUTHORITIES.fetch_max(live, Ordering::AcqRel);
                Ok(Self { owner })
            }
            Err(current) => Err(AudioOpenError::OutputBusy { owner: current }),
        }
    }
}

impl Drop for OutputAuthorityLease {
    fn drop(&mut self) {
        if OUTPUT_OWNER
            .compare_exchange(self.owner, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            #[cfg(feature = "instrument")]
            LIVE_OUTPUT_AUTHORITIES.fetch_sub(1, Ordering::AcqRel);
        }
    }
}
