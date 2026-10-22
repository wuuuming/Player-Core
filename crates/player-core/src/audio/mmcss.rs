//! Scheduling for the threads a playing sound depends on.
//!
//! Left alone, every one of them runs at normal priority, level with a compiler or a game on every
//! core. cpal 0.15.3 means to raise its WASAPI thread to time-critical, but hands
//! `SetThreadPriority` the thread's ID where a handle belongs, so the call fails and the thread
//! stays normal; its shared-mode buffer is two 10 ms periods, and a thread kept off the CPU past
//! one of them is a gap the device fills with silence. The decode worker has the ring's depth to
//! be late by, no more.
//!
//! The Multimedia Class Scheduler is how Windows expects both to be scheduled: a thread that
//! hands samples to the device is a "Pro Audio" task, and one that decodes ahead of it an "Audio"
//! task. Both run above every normal-priority thread while they stay within the share of the CPU
//! the service reserves for them, which a decoder refilling a ring and a callback copying out of
//! it never approach. An ASIO driver's thread is the driver's to schedule and is left alone.

use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::HANDLE,
        System::Threading::{AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW},
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MmcssTask {
    /// A thread that hands samples to the device on its period.
    ProAudio,
    /// A thread that decodes ahead of the one that hands them over.
    Audio,
}

impl MmcssTask {
    fn name(self) -> PCWSTR {
        match self {
            Self::ProAudio => w!("Pro Audio"),
            Self::Audio => w!("Audio"),
        }
    }
}

/// The calling thread's membership of an MMCSS task, for as long as this lives. Dropped on the
/// thread that registered — each owner keeps it in that thread's own frame or closure.
pub(super) struct MmcssRegistration(HANDLE);

// SAFETY: the handle is a token the service hands the registering thread, touched only by `Drop`.
// A callback closure must be `Send` to reach the stream's thread; the registration is made there,
// inside the closure, and never leaves it.
unsafe impl Send for MmcssRegistration {}

impl MmcssRegistration {
    /// Registers the calling thread. `None` when the service refuses (it is stopped, or its
    /// tasks are all taken): the thread then runs as it did before, at normal priority.
    /// `current_thread`, and the metrics record whether it took (`cell`).
    pub(super) fn current_thread_recorded(
        task: MmcssTask,
        cell: &std::sync::atomic::AtomicU8,
    ) -> Option<Self> {
        let registration = Self::current_thread(task);
        super::metrics::SharedMetrics::record_mmcss(cell, registration.is_some());
        registration
    }

    pub(super) fn current_thread(task: MmcssTask) -> Option<Self> {
        let mut index = 0u32;
        // SAFETY: a static task name and a writable index; the handle is reverted by Drop.
        match unsafe { AvSetMmThreadCharacteristicsW(task.name(), &mut index) } {
            Ok(handle) if !handle.is_invalid() => Some(Self(handle)),
            _ => None,
        }
    }
}

impl Drop for MmcssRegistration {
    fn drop(&mut self) {
        // SAFETY: the handle came from `AvSetMmThreadCharacteristicsW` on this thread.
        let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
    }
}
