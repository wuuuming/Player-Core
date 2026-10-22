use super::{process, pulse};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::PoisonError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AudioOutputHost {
    Wasapi,
    Asio,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum AudioOutputRoute {
    #[default]
    WasapiDefault,
    Wasapi(String),
    Asio(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioDeviceCaps {
    pub channels: u16,
    pub rates: Vec<u32>,
    pub dsd: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioOutputDeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub generic: bool,
    pub caps: Option<AudioDeviceCaps>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioOutputSystem {
    pub host: AudioOutputHost,
    pub devices: Vec<AudioOutputDeviceInfo>,
}

/// The sound server's sinks (`pactl list sinks`, C locale: its field names are translated
/// otherwise), the server's default marked. The shim keeps Windows' host names: `Wasapi` is the
/// sound server, a device id is a sink name.
pub fn enumerate_output_systems() -> Vec<AudioOutputSystem> {
    if !pulse::available() {
        return vec![AudioOutputSystem {
            host: AudioOutputHost::Wasapi,
            devices: vec![AudioOutputDeviceInfo {
                id: "default".into(),
                name: "No sound server".into(),
                is_default: true,
                generic: false,
                caps: None,
            }],
        }];
    }
    let pactl = |args: &[&str]| {
        let mut cmd = std::process::Command::new("pactl");
        cmd.args(args).env("LC_ALL", "C");
        process::capture(&mut cmd, || false, process::IO_TIMEOUT, 2 * 1024 * 1024)
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    };
    let default = pactl(&["get-default-sink"])
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();
    let mut devices = Vec::new();
    let mut name: Option<String> = None;
    for line in pactl(&["list", "sinks"]).unwrap_or_default().lines() {
        let t = line.trim();
        if let Some(n) = t.strip_prefix("Name: ") {
            name = Some(n.to_owned());
        } else if let (Some(d), Some(n)) = (t.strip_prefix("Description: "), name.take()) {
            devices.push(AudioOutputDeviceInfo {
                is_default: n == default,
                id: n,
                name: d.to_owned(),
                generic: false,
                caps: None,
            });
        }
    }
    vec![AudioOutputSystem {
        host: AudioOutputHost::Wasapi,
        devices,
    }]
}

/// The sink the next stream opens on (None: the server's default) and a count of changes, which
/// a playing stream watches to move to the new sink.
pub(super) static ROUTE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
pub(super) static ROUTE_VERSION: AtomicU32 = AtomicU32::new(0);

pub fn output_route() -> AudioOutputRoute {
    ROUTE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .map_or(AudioOutputRoute::WasapiDefault, AudioOutputRoute::Wasapi)
}
pub fn set_output_route(r: AudioOutputRoute) -> AudioOutputRoute {
    let sink = match &r {
        AudioOutputRoute::Wasapi(id) => Some(id.clone()),
        _ => None,
    };
    let mut cur = ROUTE.lock().unwrap_or_else(PoisonError::into_inner);
    if *cur != sink {
        *cur = sink;
        ROUTE_VERSION.fetch_add(1, Ordering::SeqCst);
    }
    drop(cur);
    output_route()
}
pub fn set_dop_output(_on: bool) {}

/// What a device watch tells a host. Windows' shape; the sound server keeps its streams on a
/// sink that goes (it moves them), so nothing is told here: a stream that fails says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputEvent {
    DevicesChanged,
    DeviceLost,
}

/// Windows' device watch; none here (the sink list is read afresh each time it is asked for).
pub fn watch_outputs(_listener: std::sync::Arc<dyn Fn(OutputEvent) + Send + Sync>) {}
pub(super) static NULL_OUTPUT: AtomicBool = AtomicBool::new(false);
/// Tests: no sound server, the clock only.
#[cfg(feature = "test-support")]
pub fn use_null_audio_output(on: bool) {
    NULL_OUTPUT.store(on, Ordering::Relaxed);
}

/// What the output has played, interleaved stereo `f32` at 48 kHz, while a test listens; `None`
/// while nobody does.
static CAPTURE: std::sync::Mutex<Option<Vec<f32>>> = std::sync::Mutex::new(None);

/// Tests: while `capturing`, a run plays into a capture paced at the real-time rate, instead of
/// the sound server (and the null output); answers what it captured since it was last asked.
#[cfg(feature = "test-support")]
pub fn capture_null_audio_output(capturing: bool) -> Vec<f32> {
    let mut capture = CAPTURE.lock().unwrap_or_else(PoisonError::into_inner);
    let kept = capture.take().unwrap_or_default();
    *capture = capturing.then(Vec::new);
    kept
}

/// Whether a test listens (`capture_null_audio_output`).
pub(super) fn capturing() -> bool {
    CAPTURE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_some()
}

/// The output a test listens to: it takes the sound as fast as it plays, holding a tenth of a
/// second as the sound server's stream does.
pub(super) struct Capture {
    started: std::time::Instant,
    written: pulse::Recent,
}

impl Capture {
    const HELD_US: u64 = 100_000;

    pub(super) fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
            written: pulse::Recent::new(),
        }
    }

    pub(super) fn write(&mut self, bytes: &[u8]) {
        if let Some(capture) = CAPTURE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
        {
            capture.extend(pulse::samples(bytes));
        }
        self.written.push(bytes);
        let held = self.latency_us();
        if held > Self::HELD_US {
            std::thread::sleep(std::time::Duration::from_micros(held - Self::HELD_US));
        }
    }

    pub(super) fn latency_us(&self) -> u64 {
        let played = self.started.elapsed().as_micros() as u64;
        pulse::bytes_to_micros(self.written.written()).saturating_sub(played)
    }

    pub(super) fn drain(&self) {
        std::thread::sleep(std::time::Duration::from_micros(self.latency_us()));
    }

    /// `pulse::Stream::fade_out`: what it holds from what plays now on is captured again faded,
    /// and played to its end.
    pub(super) fn fade_out(&mut self, fade: std::time::Duration) -> bool {
        let from = pulse::duration_to_bytes(self.started.elapsed());
        let Some(faded) = self.written.faded_from(from, fade) else {
            return false;
        };
        if let Some(capture) = CAPTURE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
        {
            // The capture's end is the output's: what it has of the faded part, aligned there.
            let samples = faded.chunks_exact(4);
            let n = samples.len().min(capture.len());
            let start = capture.len() - n;
            for (kept, sample) in capture[start..]
                .iter_mut()
                .zip(samples.skip(faded.len() / 4 - n))
            {
                *kept = pulse::sample(sample);
            }
        }
        self.drain();
        true
    }
}
