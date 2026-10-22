//! The only FFI owner of a surround graph. Never reachable from the device callback.
use super::mix::ChannelLayout;
use std::{
    ffi::c_void,
    ptr::{self, NonNull},
};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThread() -> *mut c_void;
    fn QueryThreadCycleTime(thread: *mut c_void, cycles: *mut u64) -> i32;
}
pub(super) fn thread_cycles() -> u64 {
    let mut cycles = 0;
    // Current-thread pseudo handle, no allocation and no handle to close.
    // SAFETY: that handle and a live local.
    unsafe {
        QueryThreadCycleTime(GetCurrentThread(), &mut cycles);
    }
    cycles
}

unsafe extern "C" {
    fn bn_upmix_open(mask: u64, rate: i32, out: *mut *mut c_void) -> i32;
    fn bn_upmix_push(value: *mut c_void, input: *const f32, frames: i32) -> i32;
    fn bn_upmix_pull(
        value: *mut c_void,
        planes: *mut *const *const f32,
        frames: *mut i32,
        channels: *mut i32,
    ) -> i32;
    fn bn_upmix_free(value: *mut *mut c_void);
}
pub(super) struct UpmixEngine {
    raw: NonNull<c_void>,
    channels: usize,
}
// SAFETY: exclusive graph ownership moves to the decode thread; no reference is shared.
unsafe impl Send for UpmixEngine {}
impl UpmixEngine {
    pub(super) fn new(output: ChannelLayout, rate: u32) -> Result<Self, String> {
        let mut raw = ptr::null_mut();
        let rate = i32::try_from(rate).map_err(|_| "invalid upmix rate")?;
        // SAFETY: plain values and a live out-parameter.
        let status = unsafe { bn_upmix_open(output.mask(), rate, &mut raw) };
        if status < 0 {
            return Err(format!("surround graph open failed: {status}"));
        }
        Ok(Self {
            raw: NonNull::new(raw).ok_or("surround returned no graph")?,
            channels: output.channels(),
        })
    }
    pub(super) fn push(&mut self, input: &[f32]) -> Result<Vec<f32>, String> {
        if input.is_empty() {
            return Ok(Vec::new());
        }
        let frames = i32::try_from(input.len() / 2).map_err(|_| "upmix block too large")?;
        // SAFETY: the graph this engine owns; `input` holds `frames` stereo frames.
        let status = unsafe { bn_upmix_push(self.raw.as_ptr(), input.as_ptr(), frames) };
        if status < 0 {
            return Err(format!("surround push failed: {status}"));
        }
        self.pull()
    }
    pub(super) fn flush(&mut self) -> Result<Vec<f32>, String> {
        // SAFETY: the graph this engine owns; no input (a flush).
        let status = unsafe { bn_upmix_push(self.raw.as_ptr(), ptr::null(), 0) };
        if status < 0 {
            return Err(format!("surround EOF failed: {status}"));
        }
        self.pull()
    }
    fn pull(&mut self) -> Result<Vec<f32>, String> {
        let mut output = Vec::new();
        loop {
            let (mut planes, mut frames, mut channels) = (ptr::null(), 0, 0);
            // SAFETY: the graph this engine owns; three live out-parameters.
            let status = unsafe {
                bn_upmix_pull(self.raw.as_ptr(), &mut planes, &mut frames, &mut channels)
            };
            if status == 1 {
                break;
            }
            if status < 0 || frames < 0 || channels as usize != self.channels || planes.is_null() {
                return Err(format!("invalid surround output: {status}"));
            }
            // C checked FLTP, and owns all planes until the next graph call.
            // SAFETY: `planes` is not null and holds `channels` == `self.channels` plane pointers
            // (checked above), alive until the next graph call.
            let planes = unsafe { std::slice::from_raw_parts(planes, self.channels) };
            output.reserve(frames as usize * self.channels);
            for frame in 0..frames as usize {
                for &plane in planes {
                    // SAFETY: each plane holds `frames` samples (the shim's pull contract).
                    output.push(unsafe { *plane.add(frame) });
                }
            }
        }
        Ok(output)
    }
}
impl Drop for UpmixEngine {
    fn drop(&mut self) {
        let mut raw = self.raw.as_ptr();
        // SAFETY: the graph this engine owns, freed once (the engine is being dropped).
        unsafe { bn_upmix_free(&mut raw) };
    }
}
