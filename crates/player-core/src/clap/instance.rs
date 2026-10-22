//! One plugin instance: created, activated and shown on the host thread, processed on whichever
//! thread runs the chain (one at a time), destroyed on the host thread whoever lets go last.

use super::frame::{Frame, OnClose};
use super::host;
#[cfg(feature = "instrument")]
use super::library::text;
use super::library::{ClapDescription, Library};
use super::{Changes, EffectParam, MAX_CHOICES};
use crate::dsp::DSP_MAX_BLOCK_FRAMES;
use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::events::{
    clap_event_header, clap_event_param_value, clap_input_events, clap_output_events,
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_VALUE,
};
use clap_sys::ext::audio_ports::{
    clap_audio_port_info, clap_host_audio_ports, clap_plugin_audio_ports, CLAP_AUDIO_PORT_IS_MAIN,
    CLAP_EXT_AUDIO_PORTS,
};
use clap_sys::ext::gui::{
    clap_host_gui, clap_plugin_gui, clap_window, clap_window_handle, CLAP_EXT_GUI,
};
use clap_sys::ext::latency::{clap_host_latency, clap_plugin_latency, CLAP_EXT_LATENCY};
use clap_sys::ext::log::{clap_host_log, clap_log_severity, CLAP_EXT_LOG};
use clap_sys::ext::params::{
    clap_host_params, clap_param_clear_flags, clap_param_info, clap_param_rescan_flags,
    clap_plugin_params, CLAP_EXT_PARAMS, CLAP_PARAM_IS_BYPASS, CLAP_PARAM_IS_HIDDEN,
    CLAP_PARAM_IS_READONLY, CLAP_PARAM_IS_STEPPED,
};
#[cfg(target_os = "linux")]
use clap_sys::ext::posix_fd_support::{
    clap_host_posix_fd_support, clap_plugin_posix_fd_support, CLAP_EXT_POSIX_FD_SUPPORT,
};
use clap_sys::ext::state::{clap_host_state, clap_plugin_state, CLAP_EXT_STATE};
use clap_sys::ext::thread_check::{clap_host_thread_check, CLAP_EXT_THREAD_CHECK};
use clap_sys::ext::timer_support::{
    clap_host_timer_support, clap_plugin_timer_support, CLAP_EXT_TIMER_SUPPORT,
};
use clap_sys::host::clap_host;
use clap_sys::id::clap_id;
use clap_sys::plugin::clap_plugin;
use clap_sys::process::{clap_process, CLAP_PROCESS_ERROR};
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::version::CLAP_VERSION;
use std::ffi::{c_char, c_void, CStr, CString};
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::{Duration, Instant};

/// How long after its last block a plugin counts as playing: parameters set meanwhile go with its
/// next block, not through `params.flush` (the chain's thread fills a ring of at most 250 ms).
const PLAYING_GAP: Duration = Duration::from_millis(400);

/// What the plugin's callbacks into the host reach: set before the plugin exists, so its
/// `host_data` is valid from `create_plugin` on.
struct HostData {
    instance: OnceLock<Weak<Instance>>,
    /// The chain's change count, bumped when the plugin says its state or a parameter changed.
    changes: Arc<Changes>,
}

/// The plugin's extensions the host uses, null where it has none.
#[derive(Clone, Copy)]
struct Extensions {
    latency: *const clap_plugin_latency,
    state: *const clap_plugin_state,
    gui: *const clap_plugin_gui,
    params: *const clap_plugin_params,
    timer: *const clap_plugin_timer_support,
    #[cfg(target_os = "linux")]
    fd: *const clap_plugin_posix_fd_support,
}

/// The audio thread's side: buffers, and whether processing has started.
struct AudioSide {
    processing: bool,
    /// A reset owed since the plugin last processed (a seek while another thread held it).
    owe_reset: bool,
    steady_time: i64,
    input: [Vec<f32>; 2],
    output: [Vec<f32>; 2],
    /// The parameter events of the call being made (taken from `pending`).
    events: Vec<clap_event_param_value>,
}

/// The plugin's window, on the host thread.
#[derive(Default)]
struct Window {
    created: bool,
    /// The host's window the plugin's view is embedded in; none for a floating one.
    frame: Option<Frame>,
}

pub(super) struct Instance {
    plugin: *const clap_plugin,
    ext: Extensions,
    pub(super) desc: ClapDescription,
    /// The rate the plugin is active at; 0 inactive.
    active_rate: AtomicU32,
    latency: AtomicU32,
    audio: Mutex<AudioSide>,
    window: Mutex<Window>,
    failure: Mutex<Option<String>>,
    failed: AtomicBool,
    pub(super) bypass: AtomicBool,
    changes: Arc<Changes>,
    /// Parameter values set by the host and not yet handed to the plugin, one per parameter.
    pending: Mutex<Vec<(u32, f64)>>,
    /// When the plugin last processed a block ([`now_ms`]; 0: never).
    processed_at: AtomicU64,
    // Dropped after the plugin is destroyed (`Drop`), in this order.
    data: ManuallyDrop<Box<HostData>>,
    host: ManuallyDrop<Box<clap_host>>,
    library: Arc<Library>,
}

// SAFETY: the plugin is called on the host thread for main-thread functions and under `audio` for
// audio-thread ones, as CLAP requires; the extension tables are immutable once queried.
unsafe impl Send for Instance {}
// SAFETY: as for `Send`.
unsafe impl Sync for Instance {}

impl Instance {
    /// Creates and initialises `desc.id` from `library`, with `state` loaded when given. On the
    /// host thread.
    pub(super) fn create(
        library: Arc<Library>,
        desc: ClapDescription,
        state: Option<&[u8]>,
        changes: Arc<Changes>,
    ) -> Result<Arc<Instance>, String> {
        let id = CString::new(desc.id.as_str()).map_err(|_| "the plugin id has a NUL")?;
        // Built cyclic so that the host data names the instance before the plugin exists: a
        // plugin may register a timer from `init`.
        let instance = Arc::new_cyclic(|weak: &Weak<Instance>| {
            let data = Box::new(HostData {
                instance: OnceLock::from(weak.clone()),
                changes: changes.clone(),
            });
            let host = Box::new(clap_host {
                clap_version: CLAP_VERSION,
                host_data: ptr::from_ref::<HostData>(&data).cast_mut().cast(),
                name: c"NovaPlayer".as_ptr(),
                vendor: c"Nova".as_ptr(),
                url: c"".as_ptr(),
                version: c"0.0.1".as_ptr(),
                get_extension: Some(host_get_extension),
                request_restart: Some(host_request_restart),
                request_process: Some(host_request_process),
                request_callback: Some(host_request_callback),
            });
            // SAFETY: the host outlives the plugin (both boxes go after `destroy`); the id is
            // null-terminated.
            let created = unsafe { create_plugin(&library, &host, &id) };
            let (plugin, failure) = match created {
                Ok(plugin) => (plugin, None),
                Err(why) => (ptr::null(), Some(why)),
            };
            Instance {
                plugin,
                // SAFETY: an initialised plugin, or none.
                ext: unsafe { Extensions::of(plugin) },
                desc,
                active_rate: AtomicU32::new(0),
                latency: AtomicU32::new(0),
                audio: Mutex::new(AudioSide {
                    processing: false,
                    owe_reset: false,
                    steady_time: 0,
                    input: [
                        vec![0.0; DSP_MAX_BLOCK_FRAMES],
                        vec![0.0; DSP_MAX_BLOCK_FRAMES],
                    ],
                    output: [
                        vec![0.0; DSP_MAX_BLOCK_FRAMES],
                        vec![0.0; DSP_MAX_BLOCK_FRAMES],
                    ],
                    events: Vec::with_capacity(64),
                }),
                window: Mutex::new(Window::default()),
                failed: AtomicBool::new(failure.is_some()),
                failure: Mutex::new(failure),
                bypass: AtomicBool::new(false),
                changes,
                pending: Mutex::new(Vec::new()),
                processed_at: AtomicU64::new(0),
                data: ManuallyDrop::new(data),
                host: ManuallyDrop::new(host),
                library,
            }
        });
        if let Some(why) = instance.failure() {
            return Err(why);
        }
        // SAFETY: an initialised plugin, on the main thread.
        let ports =
            unsafe { extension::<clap_plugin_audio_ports>(instance.plugin, CLAP_EXT_AUDIO_PORTS) };
        // SAFETY: the plugin's own table, on the main thread.
        if !unsafe { stereo_main_ports(instance.plugin, ports) } {
            return Err("the plugin has no stereo input and output".into());
        }
        if let Some(state) = state {
            instance.load_state(state)?;
        }
        Ok(instance)
    }

    pub(super) fn path(&self) -> &std::path::Path {
        self.library.path()
    }

    fn functions(&self) -> &clap_plugin {
        // SAFETY: the plugin lives until `Drop`.
        unsafe { &*self.plugin }
    }

    /// Why the plugin was taken out of the chain, if it was.
    pub(super) fn failure(&self) -> Option<String> {
        self.failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(super) fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    /// Takes the plugin out of the chain for the rest of its life, saying why.
    pub(super) fn fail(&self, why: impl Into<String>) {
        let mut failure = self.failure.lock().unwrap_or_else(PoisonError::into_inner);
        if failure.is_none() {
            *failure = Some(why.into());
            self.failed.store(true, Ordering::Release);
            self.changes.bump();
        }
    }

    pub(super) fn active_rate(&self) -> u32 {
        self.active_rate.load(Ordering::Acquire)
    }

    pub(super) fn latency(&self) -> usize {
        self.latency.load(Ordering::Acquire) as usize
    }

    pub(super) fn has_window(&self) -> bool {
        !self.ext.gui.is_null()
    }

    pub(super) fn has_params(&self) -> bool {
        !self.ext.params.is_null()
    }

    /// Activates the plugin at `rate`, deactivating it first at another. On the host thread.
    pub(super) fn activate(&self, rate: u32) -> Result<(), String> {
        if self.failed() {
            return Ok(());
        }
        if self.active_rate() == rate {
            return Ok(());
        }
        self.deactivate();
        let activate = self
            .functions()
            .activate
            .ok_or("the plugin cannot activate")?;
        // SAFETY: the plugin's own function on the main thread while inactive.
        if !unsafe { activate(self.plugin, f64::from(rate), 1, DSP_MAX_BLOCK_FRAMES as u32) } {
            let why = format!("the plugin cannot run at {rate} Hz");
            self.fail(why.clone());
            return Err(why);
        }
        // SAFETY: the plugin's own table, read on the main thread after activation.
        let latency = unsafe { self.ext.latency.as_ref() }
            .and_then(|latency| latency.get)
            // SAFETY: as above.
            .map_or(0, |get| unsafe { get(self.plugin) });
        self.latency.store(latency, Ordering::Release);
        self.active_rate.store(rate, Ordering::Release);
        Ok(())
    }

    /// Stops processing (as the audio thread, holding it) and deactivates. On the host thread.
    fn deactivate(&self) {
        if self.active_rate() == 0 {
            return;
        }
        let mut audio = self.audio.lock().unwrap_or_else(PoisonError::into_inner);
        if audio.processing {
            if let Some(stop) = self.functions().stop_processing {
                // SAFETY: the plugin's own function; holding `audio` makes this the only thread
                // that processes it.
                host::as_audio_thread(|| unsafe { stop(self.plugin) });
            }
            audio.processing = false;
        }
        self.active_rate.store(0, Ordering::Release);
        drop(audio);
        if let Some(deactivate) = self.functions().deactivate {
            // SAFETY: the plugin's own function on the main thread, not processing.
            unsafe { deactivate(self.plugin) };
        }
    }

    /// Resets the plugin before its next block: at once when no other thread is processing it.
    pub(super) fn reset(&self) {
        if let Ok(mut audio) = self.audio.try_lock() {
            if audio.processing {
                if let Some(reset) = self.functions().reset {
                    // SAFETY: the plugin's own function as its audio thread.
                    host::as_audio_thread(|| unsafe { reset(self.plugin) });
                }
                audio.owe_reset = false;
            } else {
                audio.owe_reset = true;
            }
        }
    }

    /// Processes `input` (interleaved, `channels` per frame) on the plugin's stereo port into
    /// `output`'s first two channels (or its one, the mix of both): the first two channels, or
    /// the one doubled. Returns false when the plugin did not process (busy on another thread,
    /// or not active at `rate`), leaving `output` alone; a plugin that errs or answers with a NaN
    /// or an infinity fails.
    pub(super) fn process(
        &self,
        input: &[f32],
        output: &mut [f32],
        channels: usize,
        rate: u32,
    ) -> bool {
        if !self.runs_at(rate) {
            return false;
        }
        let Ok(mut audio) = self.audio.try_lock() else {
            return false;
        };
        // Deactivated while this thread waited for the lock.
        if self.active_rate() != rate {
            return false;
        }
        let audio = &mut *audio;
        let frames = input.len() / channels;
        host::as_audio_thread(|| {
            let functions = self.functions();
            if !audio.processing {
                // SAFETY: the plugin's own function, active, as its audio thread.
                let started = functions
                    .start_processing
                    .is_none_or(|start| unsafe { start(self.plugin) });
                if !started {
                    return false;
                }
                audio.processing = true;
                audio.owe_reset = false;
            } else if std::mem::take(&mut audio.owe_reset) {
                if let Some(reset) = functions.reset {
                    // SAFETY: the plugin's own function as its audio thread.
                    unsafe { reset(self.plugin) };
                }
            }
            for (frame, sample) in input.chunks_exact(channels).enumerate() {
                audio.input[0][frame] = sample[0];
                audio.input[1][frame] = sample[if channels > 1 { 1 } else { 0 }];
            }
            // What the host set since the last block; busy (being set now): with the next one.
            if let Ok(mut pending) = self.pending.try_lock() {
                audio.events.extend(param_events(&mut pending));
            }
            let in_events = input_events(&audio.events);
            let reported = AtomicBool::new(false);
            let out_events = output_events(&reported);
            let [in_left, in_right] = &mut audio.input;
            let [out_left, out_right] = &mut audio.output;
            let mut inputs = [in_left.as_mut_ptr(), in_right.as_mut_ptr()];
            let mut outputs = [out_left.as_mut_ptr(), out_right.as_mut_ptr()];
            let in_buffer = clap_audio_buffer {
                data32: inputs.as_mut_ptr(),
                data64: ptr::null_mut(),
                channel_count: 2,
                latency: 0,
                constant_mask: 0,
            };
            let mut out_buffer = clap_audio_buffer {
                data32: outputs.as_mut_ptr(),
                ..in_buffer
            };
            let process = clap_process {
                steady_time: audio.steady_time,
                frames_count: frames as u32,
                transport: ptr::null(),
                audio_inputs: &in_buffer,
                audio_outputs: &mut out_buffer,
                audio_inputs_count: 1,
                audio_outputs_count: 1,
                in_events: &in_events,
                out_events: &out_events,
            };
            let Some(run) = functions.process else {
                self.fail("the plugin cannot process");
                return false;
            };
            // SAFETY: the plugin's own function as its audio thread, processing, with buffers of
            // `frames` (at most the activation's maximum) per channel alive for the call.
            let status = unsafe { run(self.plugin, &process) };
            audio.steady_time += frames as i64;
            audio.events.clear();
            self.processed_at.store(now_ms(), Ordering::Release);
            self.note_reported(&reported);
            if status == CLAP_PROCESS_ERROR {
                self.fail("the plugin reported an error");
                return false;
            }
            let (left, right) = (&audio.output[0][..frames], &audio.output[1][..frames]);
            if !left.iter().chain(right).all(|sample| sample.is_finite()) {
                self.fail("the plugin's output was not a number");
                return false;
            }
            for (frame, sample) in output.chunks_exact_mut(channels).enumerate() {
                if channels == 1 {
                    sample[0] = 0.5 * (left[frame] + right[frame]);
                } else {
                    sample[0] = left[frame];
                    sample[1] = right[frame];
                }
            }
            true
        })
    }

    /// The plugin's state (`clap.state`). On the host thread.
    pub(super) fn save_state(&self) -> Result<Vec<u8>, String> {
        // SAFETY: the plugin's own table.
        let save = unsafe { self.ext.state.as_ref() }
            .and_then(|state| state.save)
            .ok_or("the plugin keeps no state")?;
        let mut bytes = Vec::new();
        let stream = clap_ostream {
            ctx: ptr::from_mut(&mut bytes).cast(),
            write: Some(write_bytes),
        };
        // SAFETY: the plugin's own function on the main thread; `stream` writes into `bytes`,
        // alive for the call.
        if unsafe { save(self.plugin, &stream) } {
            Ok(bytes)
        } else {
            Err("the plugin did not save its state".into())
        }
    }

    fn load_state(&self, bytes: &[u8]) -> Result<(), String> {
        // SAFETY: the plugin's own table.
        let Some(load) = unsafe { self.ext.state.as_ref() }.and_then(|state| state.load) else {
            return Ok(());
        };
        let mut reader = bytes;
        let stream = clap_istream {
            ctx: ptr::from_mut(&mut reader).cast(),
            read: Some(read_bytes),
        };
        // SAFETY: the plugin's own function on the main thread; `stream` reads `bytes`, alive
        // for the call.
        if unsafe { load(self.plugin, &stream) } {
            Ok(())
        } else {
            Err("the plugin did not take its saved state".into())
        }
    }

    /// Shows the plugin's window above `parent` (an HWND, an X11 window id; 0 for none): its
    /// own floating window when it has one, otherwise its view embedded in a window of the
    /// host's (`frame.rs`). On the host thread.
    pub(super) fn show_window(&self, parent: isize) -> Result<(), String> {
        self.prepare_window(parent)?;
        // SAFETY: the plugin's own table.
        let gui = unsafe { self.ext.gui.as_ref() }.ok_or("the plugin has no window")?;
        // SAFETY: the plugin's own function on the main thread, its window created.
        if !gui.show.is_some_and(|show| unsafe { show(self.plugin) }) {
            return Err("the plugin did not show its window".into());
        }
        self.with_frame(|frame| frame.show());
        Ok(())
    }

    /// Creates the plugin's window, hidden, unless it exists: what makes opening it slow (a
    /// plugin builds its whole interface here; Airwindows Consolidated takes 0.7 s), so that
    /// showing it later is at once. On the host thread.
    pub(super) fn prepare_window(&self, parent: isize) -> Result<(), String> {
        // SAFETY: the plugin's own table.
        let gui = unsafe { self.ext.gui.as_ref() }.ok_or("the plugin has no window")?;
        let mut window = self.window.lock().unwrap_or_else(PoisonError::into_inner);
        if !window.created && !self.failed() {
            window.frame = self.create_window(gui, parent)?;
            window.created = true;
        }
        Ok(())
    }

    /// Creates the plugin's window: floating when it can, embedded in a new frame otherwise.
    fn create_window(&self, gui: &clap_plugin_gui, parent: isize) -> Result<Option<Frame>, String> {
        #[cfg(windows)]
        let api = clap_sys::ext::gui::CLAP_WINDOW_API_WIN32;
        #[cfg(target_os = "linux")]
        let api = clap_sys::ext::gui::CLAP_WINDOW_API_X11;
        let supported = |floating| {
            // SAFETY: the plugin's own function on the main thread.
            gui.is_api_supported
                .is_some_and(|supported| unsafe { supported(self.plugin, api.as_ptr(), floating) })
        };
        let floating = supported(true);
        if !floating && !supported(false) {
            return Err("the plugin has no window for this system".into());
        }
        let create = gui.create.ok_or("the plugin has no window")?;
        // SAFETY: the plugin's own function on the main thread.
        if !unsafe { create(self.plugin, api.as_ptr(), floating) } {
            return Err("the plugin did not create its window".into());
        }
        let window = |handle: isize| clap_window {
            api: api.as_ptr(),
            #[cfg(windows)]
            specific: clap_window_handle {
                win32: handle as *mut c_void,
            },
            #[cfg(target_os = "linux")]
            specific: clap_window_handle {
                x11: handle as std::ffi::c_ulong,
            },
        };
        if floating {
            if let (Some(transient), true) = (gui.set_transient, parent != 0) {
                // SAFETY: as above, with a window description alive for the call.
                unsafe { transient(self.plugin, &window(parent)) };
            }
            if let (Some(title), Ok(name)) =
                (gui.suggest_title, CString::new(self.desc.name.as_str()))
            {
                // SAFETY: as above, with a null-terminated title.
                unsafe { title(self.plugin, name.as_ptr()) };
            }
            return Ok(None);
        }
        let owner = self.data.instance.get().cloned().unwrap_or_default();
        let on_close: OnClose = Arc::new(move || {
            if let Some(instance) = owner.upgrade() {
                instance.hide_window();
            }
        });
        let embedded = Frame::new(&self.desc.name, parent, on_close).and_then(|frame| {
            #[cfg(windows)]
            if let Some(set_scale) = gui.set_scale {
                // SAFETY: the plugin's own function on the main thread, its window created.
                unsafe { set_scale(self.plugin, frame.scale()) };
            }
            let (mut width, mut height) = (0, 0);
            let sized = gui.get_size.is_some_and(|get| {
                // SAFETY: as above, into two out-values.
                unsafe { get(self.plugin, &mut width, &mut height) }
            });
            if sized {
                frame.resize(width, height);
            }
            let parented = gui.set_parent.is_some_and(|set| {
                // SAFETY: as above, with a window description alive for the call.
                unsafe { set(self.plugin, &window(frame.handle())) }
            });
            if parented {
                Ok(frame)
            } else {
                Err("the plugin did not take its window".to_string())
            }
        });
        match embedded {
            Ok(frame) => Ok(Some(frame)),
            Err(why) => {
                if let Some(destroy) = gui.destroy {
                    // SAFETY: the plugin's own function on the main thread, its window created.
                    unsafe { destroy(self.plugin) };
                }
                Err(why)
            }
        }
    }

    /// Sizes the frame to the plugin's request (`gui.request_resize`). On the host thread.
    fn resize_frame(&self, width: u32, height: u32) {
        self.with_frame(|frame| frame.resize(width, height));
    }

    fn hide_window(&self) {
        // SAFETY: the plugin's own table and function on the main thread.
        if let Some(hide) = unsafe { self.ext.gui.as_ref() }.and_then(|gui| gui.hide) {
            if self
                .window
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .created
            {
                // SAFETY: as above, its window created.
                unsafe { hide(self.plugin) };
            }
        }
        self.with_frame(|frame| frame.hide());
    }

    /// Destroys the plugin's window. On the host thread.
    fn destroy_window(&self) {
        let mut window = self.window.lock().unwrap_or_else(PoisonError::into_inner);
        if std::mem::take(&mut window.created) {
            // SAFETY: the plugin's own table and function on the main thread.
            if let Some(destroy) = unsafe { self.ext.gui.as_ref() }.and_then(|gui| gui.destroy) {
                // SAFETY: as above, its window created.
                unsafe { destroy(self.plugin) };
            }
        }
        // After the plugin let go of it.
        window.frame = None;
    }

    pub(super) fn on_timer(&self, id: u32) {
        // SAFETY: the plugin's own table and function on the main thread.
        if let Some(on_timer) = unsafe { self.ext.timer.as_ref() }.and_then(|timer| timer.on_timer)
        {
            // SAFETY: as above.
            unsafe { on_timer(self.plugin, id) };
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn on_fd(&self, fd: i32, flags: u32) {
        // SAFETY: the plugin's own table and function on the main thread.
        if let Some(on_fd) = unsafe { self.ext.fd.as_ref() }.and_then(|fd| fd.on_fd) {
            // SAFETY: as above.
            unsafe { on_fd(self.plugin, fd, flags) };
        }
    }

    /// The parameters as they stand (hidden ones and the plugin's bypass left out), a value the
    /// host set and the plugin has not taken yet in place of the plugin's. On the host thread.
    pub(super) fn params(&self) -> Vec<EffectParam> {
        // SAFETY: the plugin's own table.
        let Some(params) = (unsafe { self.ext.params.as_ref() }) else {
            return Vec::new();
        };
        let (Some(count), Some(get_info)) = (params.count, params.get_info) else {
            return Vec::new();
        };
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        // SAFETY: the plugin's own function on the main thread.
        let count = unsafe { count(self.plugin) };
        (0..count)
            .filter_map(|index| {
                // SAFETY: a zeroed out-structure the plugin fills.
                let mut info: clap_param_info = unsafe { std::mem::zeroed() };
                // SAFETY: the plugin's own function on the main thread, into `info`.
                if !unsafe { get_info(self.plugin, index, &mut info) } {
                    return None;
                }
                if info.flags & (CLAP_PARAM_IS_HIDDEN | CLAP_PARAM_IS_BYPASS) != 0 {
                    return None;
                }
                let set = pending.iter().find(|(id, _)| *id == info.id);
                let value = set
                    .map(|(_, value)| *value)
                    .or_else(|| self.param_value(info.id))
                    .unwrap_or(info.default_value);
                let (min, max) = (info.min_value, info.max_value);
                let stepped = info.flags & CLAP_PARAM_IS_STEPPED != 0;
                let steps = (max - min).round();
                let choices =
                    if stepped && steps.is_finite() && (0.0..MAX_CHOICES as f64).contains(&steps) {
                        (0..=steps as i64)
                            .map(|step| self.param_text(info.id, min.round() + step as f64))
                            .collect()
                    } else {
                        Vec::new()
                    };
                Some(EffectParam {
                    id: info.id,
                    name: crate::fixed_c_text(&info.name),
                    module: crate::fixed_c_text(&info.module),
                    min,
                    max,
                    default: info.default_value,
                    value,
                    text: self.param_text(info.id, value),
                    stepped,
                    choices,
                    read_only: info.flags & CLAP_PARAM_IS_READONLY != 0,
                })
            })
            .collect()
    }

    /// The plugin's value of a parameter. On the host thread.
    fn param_value(&self, id: u32) -> Option<f64> {
        // SAFETY: the plugin's own table.
        let get = unsafe { self.ext.params.as_ref() }?.get_value?;
        let mut value = 0.0;
        // SAFETY: the plugin's own function on the main thread, into `value`.
        unsafe { get(self.plugin, id, &mut value) }.then_some(value)
    }

    /// `value` of a parameter as the plugin prints it (the number, when it does not). On the host
    /// thread.
    fn param_text(&self, id: u32, value: f64) -> String {
        // SAFETY: the plugin's own table.
        let to_text = unsafe { self.ext.params.as_ref() }.and_then(|params| params.value_to_text);
        let mut text: [c_char; 128] = [0; 128];
        let printed = to_text.is_some_and(|to_text| {
            // SAFETY: the plugin's own function on the main thread, into `text` of its length.
            unsafe { to_text(self.plugin, id, value, text.as_mut_ptr(), text.len() as u32) }
        });
        if printed {
            crate::fixed_c_text(&text)
        } else {
            format!("{value:.2}")
        }
    }

    /// Sets a parameter (any thread): kept until the plugin takes it, with its next block while
    /// it plays, through `params.flush` on the host thread otherwise.
    pub(super) fn set_param(self: &Arc<Self>, id: u32, value: f64) {
        {
            let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            match pending.iter_mut().find(|(known, _)| *known == id) {
                Some(set) => set.1 = value,
                None => pending.push((id, value)),
            }
        }
        self.changes.bump();
        let owner = Arc::downgrade(self);
        host::post(move || deliver(&owner));
    }

    /// Whether the plugin gets blocks: it processed one in the last [`PLAYING_GAP`] and is
    /// neither bypassed nor failed since.
    fn playing(&self) -> bool {
        let at = self.processed_at.load(Ordering::Acquire);
        at != 0
            && now_ms().saturating_sub(at) < PLAYING_GAP.as_millis() as u64
            && !self.failed()
            && !self.bypass.load(Ordering::Relaxed)
    }

    /// `params.flush`: the values the host set reach the plugin, and what its window changed
    /// reaches the host, while no block is processed. As its audio thread when it is active
    /// (holding `audio`, so no block runs meanwhile), on the host thread.
    fn flush_params(&self) {
        // SAFETY: the plugin's own table.
        let flush = unsafe { self.ext.params.as_ref() }.and_then(|params| params.flush);
        let (Some(flush), false) = (flush, self.failed()) else {
            self.pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clear();
            return;
        };
        let mut audio = self.audio.lock().unwrap_or_else(PoisonError::into_inner);
        let events: Vec<clap_event_param_value> =
            param_events(&mut self.pending.lock().unwrap_or_else(PoisonError::into_inner))
                .collect();
        audio.events.extend(events);
        let in_events = input_events(&audio.events);
        let reported = AtomicBool::new(false);
        let out_events = output_events(&reported);
        // SAFETY: the plugin's own function, with event lists alive for the call; holding `audio`
        // makes this the only thread that processes it.
        let call = || unsafe { flush(self.plugin, &in_events, &out_events) };
        if self.active_rate() != 0 {
            host::as_audio_thread(call);
        } else {
            call();
        }
        audio.events.clear();
        drop(audio);
        self.note_reported(&reported);
    }

    /// It runs, at `rate`: not failed, and active at that rate.
    pub(super) fn runs_at(&self, rate: u32) -> bool {
        !self.failed() && self.active_rate() == rate
    }

    /// The plugin reported a change of its own during a call: the chain's count moves.
    fn note_reported(&self, reported: &AtomicBool) {
        if reported.load(Ordering::Relaxed) {
            self.changes.bump();
        }
    }

    /// Runs `f` on the frame the plugin's window is in, when there is one.
    fn with_frame(&self, f: impl FnOnce(&Frame)) {
        if let Some(frame) = &self
            .window
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .frame
        {
            f(frame);
        }
    }

    /// Deactivates and activates again at the same rate (`request_restart`). On the host thread.
    fn restart(&self) {
        let rate = self.active_rate();
        if rate != 0 {
            self.deactivate();
            let _ = self.activate(rate);
        }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        // The last `Arc` may go on any thread; the plugin is destroyed on the host thread.
        struct Teardown {
            plugin: *const clap_plugin,
            processing: bool,
            active: bool,
            window: bool,
            frame: Option<Frame>,
            gui: *const clap_plugin_gui,
            owner: *const Instance,
            keep: (Box<HostData>, Box<clap_host>, Arc<Library>),
        }
        // SAFETY: only moved to the host thread, which alone calls the plugin from now on.
        unsafe impl Send for Teardown {}
        impl Teardown {
            fn run(self) {
                host::forget(self.owner);
                if self.plugin.is_null() {
                    return;
                }
                // SAFETY: the plugin, no longer reachable from any other thread, torn down in
                // CLAP's order on the main thread.
                unsafe {
                    let functions = &*self.plugin;
                    if self.window {
                        if let Some(destroy) = self.gui.as_ref().and_then(|gui| gui.destroy) {
                            destroy(self.plugin);
                        }
                    }
                    drop(self.frame);
                    if self.processing {
                        if let Some(stop) = functions.stop_processing {
                            host::as_audio_thread(|| stop(self.plugin));
                        }
                    }
                    if self.active {
                        if let Some(deactivate) = functions.deactivate {
                            deactivate(self.plugin);
                        }
                    }
                    if let Some(destroy) = functions.destroy {
                        destroy(self.plugin);
                    }
                }
                drop(self.keep);
            }
        }
        let teardown = Teardown {
            plugin: self.plugin,
            processing: self.audio.get_mut().map_or(true, |audio| audio.processing),
            active: self.active_rate() != 0,
            window: self.window.get_mut().map_or(true, |window| window.created),
            frame: self
                .window
                .get_mut()
                .ok()
                .and_then(|window| window.frame.take()),
            gui: self.ext.gui,
            owner: ptr::from_ref(self),
            // SAFETY: taken once, here; the fields are not used again.
            keep: unsafe {
                (
                    ManuallyDrop::take(&mut self.data),
                    ManuallyDrop::take(&mut self.host),
                    self.library.clone(),
                )
            },
        };
        if host::is_main_thread() {
            teardown.run();
        } else {
            host::post(move || teardown.run());
        }
    }
}

/// Hands what the host set to the plugin: left to its next block while it plays (looked at again
/// after [`PLAYING_GAP`], in case the sound stopped), flushed at once otherwise. On the host thread.
fn deliver(owner: &Weak<Instance>) {
    let Some(instance) = owner.upgrade() else {
        return;
    };
    let waiting = !instance
        .pending
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_empty();
    if !waiting {
        return;
    }
    if instance.playing() {
        let owner = owner.clone();
        host::post_after(PLAYING_GAP, move || deliver(&owner));
    } else {
        instance.flush_params();
    }
}

/// Milliseconds since the first call, never 0.
fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// A fixed-size C string of the plugin's (a name, a printed value), up to its first NUL.
/// A parameter's new value, for the plugin's next block or flush.
/// The values the host set since the last call, as the plugin's events.
fn param_events(
    pending: &mut Vec<(u32, f64)>,
) -> impl Iterator<Item = clap_event_param_value> + '_ {
    pending.drain(..).map(|(id, value)| param_event(id, value))
}

fn param_event(id: u32, value: f64) -> clap_event_param_value {
    clap_event_param_value {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_param_value>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_VALUE,
            flags: 0,
        },
        param_id: id,
        cookie: ptr::null_mut(),
        note_id: -1,
        port_index: -1,
        channel: -1,
        key: -1,
        value,
    }
}

/// The input event list over `events`, which outlives the call it is passed to.
fn input_events(events: &Vec<clap_event_param_value>) -> clap_input_events {
    clap_input_events {
        ctx: ptr::from_ref(events).cast_mut().cast(),
        size: Some(events_size),
        get: Some(events_get),
    }
}

/// An output event list that notes in `reported` whether the plugin changed a parameter itself
/// (its window), and forgets every event; `reported` outlives the call it is passed to.
fn output_events(reported: &AtomicBool) -> clap_output_events {
    clap_output_events {
        ctx: ptr::from_ref(reported).cast_mut().cast(),
        try_push: Some(events_push),
    }
}

/// Creates and initialises a plugin; destroys it again when `init` fails.
///
/// # Safety
/// `host` outlives the plugin; on the host thread.
unsafe fn create_plugin(
    library: &Library,
    host: &clap_host,
    id: &CStr,
) -> Result<*const clap_plugin, String> {
    // SAFETY: the library's factory, alive while `library` is.
    let factory = unsafe { &*library.factory };
    let create = factory
        .create_plugin
        .ok_or("the factory cannot create plugins")?;
    // SAFETY: the factory's own function, with a host that outlives the plugin.
    let plugin = unsafe { create(library.factory, host, id.as_ptr()) };
    if plugin.is_null() {
        return Err(format!(
            "the library has no plugin {}",
            id.to_string_lossy()
        ));
    }
    // SAFETY: a plugin just created.
    let functions = unsafe { &*plugin };
    // SAFETY: the plugin's own `init`, once, on the main thread.
    if functions.init.is_some_and(|init| unsafe { init(plugin) }) {
        return Ok(plugin);
    }
    if let Some(destroy) = functions.destroy {
        // SAFETY: a created plugin is destroyed even when `init` failed (CLAP `plugin.h`).
        unsafe { destroy(plugin) };
    }
    Err("the plugin did not start".into())
}

impl Extensions {
    /// # Safety
    /// `plugin` is initialised, or null for none.
    unsafe fn of(plugin: *const clap_plugin) -> Extensions {
        if plugin.is_null() {
            return Extensions {
                latency: ptr::null(),
                state: ptr::null(),
                gui: ptr::null(),
                params: ptr::null(),
                timer: ptr::null(),
                #[cfg(target_os = "linux")]
                fd: ptr::null(),
            };
        }
        // SAFETY: as the caller promises.
        unsafe {
            Extensions {
                latency: extension(plugin, CLAP_EXT_LATENCY),
                state: extension(plugin, CLAP_EXT_STATE),
                gui: extension(plugin, CLAP_EXT_GUI),
                params: extension(plugin, CLAP_EXT_PARAMS),
                timer: extension(plugin, CLAP_EXT_TIMER_SUPPORT),
                #[cfg(target_os = "linux")]
                fd: extension(plugin, CLAP_EXT_POSIX_FD_SUPPORT),
            }
        }
    }
}

/// # Safety
/// `plugin` is initialised; `id` names an extension whose table is `T`.
unsafe fn extension<T>(plugin: *const clap_plugin, id: &CStr) -> *const T {
    // SAFETY: as the caller promises.
    let get = unsafe { (*plugin).get_extension };
    get.map_or(ptr::null(), |get| {
        // SAFETY: the plugin's own function with a null-terminated id.
        unsafe { get(plugin, id.as_ptr()) }.cast()
    })
}

/// Whether the plugin's main ports take and give stereo.
///
/// # Safety
/// `plugin` is initialised; `ports` is its audio-ports table or null.
unsafe fn stereo_main_ports(
    plugin: *const clap_plugin,
    ports: *const clap_plugin_audio_ports,
) -> bool {
    // SAFETY: as the caller promises.
    let Some(ports) = (unsafe { ports.as_ref() }) else {
        return false;
    };
    let (Some(count), Some(get)) = (ports.count, ports.get) else {
        return false;
    };
    [true, false].into_iter().all(|is_input| {
        // SAFETY: the plugin's own function on the main thread.
        if unsafe { count(plugin, is_input) } == 0 {
            return false;
        }
        // SAFETY: a zeroed out-structure the plugin fills.
        let mut info: clap_audio_port_info = unsafe { std::mem::zeroed() };
        // SAFETY: the first port, into `info`.
        let got = unsafe { get(plugin, 0, is_input, &mut info) };
        got && info.channel_count == 2 && info.flags & CLAP_AUDIO_PORT_IS_MAIN != 0
    })
}

unsafe extern "C" fn write_bytes(
    stream: *const clap_ostream,
    buffer: *const c_void,
    size: u64,
) -> i64 {
    // SAFETY: `ctx` is the `Vec` `save_state` set; `buffer` holds `size` bytes (CLAP `stream.h`).
    unsafe {
        let bytes = &mut *(*stream).ctx.cast::<Vec<u8>>();
        bytes.extend_from_slice(std::slice::from_raw_parts(
            buffer.cast::<u8>(),
            size as usize,
        ));
    }
    size as i64
}

unsafe extern "C" fn read_bytes(
    stream: *const clap_istream,
    buffer: *mut c_void,
    size: u64,
) -> i64 {
    // SAFETY: `ctx` is the slice `load_state` set; `buffer` has room for `size` bytes.
    unsafe {
        let reader = &mut *(*stream).ctx.cast::<&[u8]>();
        let n = reader.len().min(size as usize);
        ptr::copy_nonoverlapping(reader.as_ptr(), buffer.cast::<u8>(), n);
        *reader = &reader[n..];
        n as i64
    }
}

/// The events behind a list of [`input_events`].
///
/// # Safety
/// `list` is one [`input_events`] made, its events alive.
unsafe fn listed<'a>(list: *const clap_input_events) -> &'a [clap_event_param_value] {
    // SAFETY: as the caller promises.
    unsafe { &*(*list).ctx.cast::<Vec<clap_event_param_value>>() }
}

unsafe extern "C" fn events_size(list: *const clap_input_events) -> u32 {
    // SAFETY: the list the host passed the plugin for this call.
    unsafe { listed(list) }.len() as u32
}

unsafe extern "C" fn events_get(
    list: *const clap_input_events,
    index: u32,
) -> *const clap_event_header {
    // SAFETY: as in `events_size`.
    unsafe { listed(list) }
        .get(index as usize)
        .map_or(ptr::null(), |event| &event.header)
}

unsafe extern "C" fn events_push(
    list: *const clap_output_events,
    event: *const clap_event_header,
) -> bool {
    // SAFETY: the plugin's event, valid for the call; `ctx` is the flag `output_events` set.
    unsafe {
        if let Some(header) = event.as_ref() {
            if header.space_id == CLAP_CORE_EVENT_SPACE_ID && header.type_ == CLAP_EVENT_PARAM_VALUE
            {
                (*(*list).ctx.cast::<AtomicBool>()).store(true, Ordering::Relaxed);
            }
        }
    }
    true
}

/// The host data behind a plugin's host pointer.
///
/// # Safety
/// `host` is the one an instance created the plugin with; it outlives the plugin.
unsafe fn data<'a>(host: *const clap_host) -> &'a HostData {
    // SAFETY: as the caller promises.
    unsafe { &*(*host).host_data.cast::<HostData>() }
}

/// The instance behind a plugin's host pointer, once it exists.
///
/// # Safety
/// As [`data`].
unsafe fn instance(host: *const clap_host) -> Option<Weak<Instance>> {
    // SAFETY: as the caller promises.
    unsafe { data(host) }.instance.get().cloned()
}

/// Runs `f` with the instance on the host thread, later.
///
/// # Safety
/// As [`data`].
unsafe fn later(host: *const clap_host, f: impl FnOnce(&Instance) + Send + 'static) {
    // SAFETY: as the caller promises.
    if let Some(owner) = unsafe { instance(host) } {
        host::post(move || {
            if let Some(instance) = owner.upgrade() {
                f(&instance);
            }
        });
    }
}

static LOG: clap_host_log = clap_host_log {
    log: Some(host_log),
};
static THREAD_CHECK: clap_host_thread_check = clap_host_thread_check {
    is_main_thread: Some(host_is_main_thread),
    is_audio_thread: Some(host_is_audio_thread),
};
static STATE: clap_host_state = clap_host_state {
    mark_dirty: Some(host_mark_dirty),
};
static LATENCY: clap_host_latency = clap_host_latency {
    changed: Some(host_latency_changed),
};
static AUDIO_PORTS: clap_host_audio_ports = clap_host_audio_ports {
    is_rescan_flag_supported: Some(host_ports_rescan_supported),
    rescan: Some(host_ports_rescan),
};
static PARAMS: clap_host_params = clap_host_params {
    rescan: Some(host_params_rescan),
    clear: Some(host_params_clear),
    request_flush: Some(host_params_request_flush),
};
static GUI: clap_host_gui = clap_host_gui {
    resize_hints_changed: Some(host_gui_hints_changed),
    request_resize: Some(host_gui_request_resize),
    request_show: Some(host_gui_request_show),
    request_hide: Some(host_gui_request_hide),
    closed: Some(host_gui_closed),
};
static TIMER: clap_host_timer_support = clap_host_timer_support {
    register_timer: Some(host_register_timer),
    unregister_timer: Some(host_unregister_timer),
};
#[cfg(target_os = "linux")]
static FD: clap_host_posix_fd_support = clap_host_posix_fd_support {
    register_fd: Some(host_register_fd),
    modify_fd: Some(host_modify_fd),
    unregister_fd: Some(host_unregister_fd),
};

unsafe extern "C" fn host_get_extension(
    _host: *const clap_host,
    id: *const c_char,
) -> *const c_void {
    if id.is_null() {
        return ptr::null();
    }
    // SAFETY: the plugin passes a null-terminated id.
    let id = unsafe { CStr::from_ptr(id) };
    let table: *const c_void = if id == CLAP_EXT_LOG {
        ptr::from_ref(&LOG).cast()
    } else if id == CLAP_EXT_THREAD_CHECK {
        ptr::from_ref(&THREAD_CHECK).cast()
    } else if id == CLAP_EXT_STATE {
        ptr::from_ref(&STATE).cast()
    } else if id == CLAP_EXT_LATENCY {
        ptr::from_ref(&LATENCY).cast()
    } else if id == CLAP_EXT_AUDIO_PORTS {
        ptr::from_ref(&AUDIO_PORTS).cast()
    } else if id == CLAP_EXT_PARAMS {
        ptr::from_ref(&PARAMS).cast()
    } else if id == CLAP_EXT_GUI {
        ptr::from_ref(&GUI).cast()
    } else if id == CLAP_EXT_TIMER_SUPPORT {
        ptr::from_ref(&TIMER).cast()
    } else {
        #[cfg(target_os = "linux")]
        if id == CLAP_EXT_POSIX_FD_SUPPORT {
            return ptr::from_ref(&FD).cast();
        }
        ptr::null()
    };
    table
}

unsafe extern "C" fn host_request_restart(host: *const clap_host) {
    // SAFETY: the plugin's own host pointer.
    unsafe { later(host, Instance::restart) };
}

unsafe extern "C" fn host_request_process(_host: *const clap_host) {}

unsafe extern "C" fn host_request_callback(host: *const clap_host) {
    // SAFETY: the plugin's own host pointer.
    unsafe {
        later(host, |instance| {
            if let Some(callback) = instance.functions().on_main_thread {
                // SAFETY: the plugin's own function on the main thread.
                callback(instance.plugin);
            }
        });
    }
}

unsafe extern "C" fn host_log(
    _host: *const clap_host,
    _severity: clap_log_severity,
    _msg: *const c_char,
) {
    #[cfg(feature = "instrument")]
    // SAFETY: the plugin passes a null-terminated message.
    eprintln!("[clap] {}", unsafe { text(_msg) });
}

unsafe extern "C" fn host_is_main_thread(_host: *const clap_host) -> bool {
    host::is_main_thread()
}

unsafe extern "C" fn host_is_audio_thread(_host: *const clap_host) -> bool {
    host::is_audio_thread()
}

unsafe extern "C" fn host_mark_dirty(host: *const clap_host) {
    // SAFETY: the plugin's own host pointer.
    unsafe { data(host) }.changes.bump();
}

unsafe extern "C" fn host_latency_changed(_host: *const clap_host) {}

unsafe extern "C" fn host_ports_rescan_supported(_host: *const clap_host, _flag: u32) -> bool {
    false
}

unsafe extern "C" fn host_ports_rescan(_host: *const clap_host, _flags: u32) {}

/// The plugin's parameters changed (their values, texts or list): the host reads them again.
unsafe extern "C" fn host_params_rescan(host: *const clap_host, _flags: clap_param_rescan_flags) {
    // SAFETY: the plugin's own host pointer.
    unsafe { data(host) }.changes.bump();
}

unsafe extern "C" fn host_params_clear(
    _host: *const clap_host,
    _id: clap_id,
    _flags: clap_param_clear_flags,
) {
}

/// The plugin's window changed a parameter while no block runs (while one does, the change comes
/// with it).
unsafe extern "C" fn host_params_request_flush(host: *const clap_host) {
    // SAFETY: the plugin's own host pointer.
    unsafe {
        later(host, |instance| {
            if !instance.playing() {
                instance.flush_params();
            }
        });
    }
}

unsafe extern "C" fn host_gui_hints_changed(_host: *const clap_host) {}

unsafe extern "C" fn host_gui_request_resize(
    host: *const clap_host,
    width: u32,
    height: u32,
) -> bool {
    // A floating window sizes itself; an embedded one's frame follows.
    // SAFETY: the plugin's own host pointer.
    unsafe { later(host, move |instance| instance.resize_frame(width, height)) };
    true
}

unsafe extern "C" fn host_gui_request_show(host: *const clap_host) -> bool {
    // SAFETY: the plugin's own host pointer.
    unsafe {
        later(host, |instance| {
            let _ = instance.show_window(0);
        });
    }
    true
}

unsafe extern "C" fn host_gui_request_hide(host: *const clap_host) -> bool {
    // SAFETY: the plugin's own host pointer.
    unsafe { later(host, Instance::hide_window) };
    true
}

unsafe extern "C" fn host_gui_closed(host: *const clap_host, was_destroyed: bool) {
    if was_destroyed {
        // SAFETY: the plugin's own host pointer.
        unsafe { later(host, Instance::destroy_window) };
    }
}

unsafe extern "C" fn host_register_timer(
    host: *const clap_host,
    period_ms: u32,
    id: *mut clap_id,
) -> bool {
    // SAFETY: the plugin's own host pointer.
    let Some(owner) = (unsafe { instance(host) }) else {
        return false;
    };
    match host::register_timer(owner, period_ms) {
        Some(timer) if !id.is_null() => {
            // SAFETY: the plugin's out-pointer.
            unsafe { *id = timer };
            true
        }
        _ => false,
    }
}

unsafe extern "C" fn host_unregister_timer(host: *const clap_host, id: clap_id) -> bool {
    // SAFETY: the plugin's own host pointer.
    unsafe { instance(host) }.is_some_and(|owner| host::unregister_timer(&owner, id))
}

#[cfg(target_os = "linux")]
unsafe extern "C" fn host_register_fd(host: *const clap_host, fd: i32, flags: u32) -> bool {
    // SAFETY: the plugin's own host pointer.
    unsafe { instance(host) }.is_some_and(|owner| host::register_fd(owner, fd, flags))
}

#[cfg(target_os = "linux")]
unsafe extern "C" fn host_modify_fd(host: *const clap_host, fd: i32, flags: u32) -> bool {
    // SAFETY: the plugin's own host pointer.
    unsafe { instance(host) }.is_some_and(|owner| host::modify_fd(&owner, fd, flags))
}

#[cfg(target_os = "linux")]
unsafe extern "C" fn host_unregister_fd(host: *const clap_host, fd: i32) -> bool {
    // SAFETY: the plugin's own host pointer.
    unsafe { instance(host) }.is_some_and(|owner| host::unregister_fd(&owner, fd))
}
