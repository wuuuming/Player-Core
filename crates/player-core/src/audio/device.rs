//! Output devices: the audio systems this machine has and their devices, the one the listener
//! chose and where a bind actually lands, the null output the instrument gates run on, and
//! building the cpal stream itself.

#[cfg(feature = "asio")]
use super::asio;
use super::asio_host::AsioLease;
#[cfg(feature = "asio")]
use super::asio_host::AsioOpenError;
#[cfg(any(feature = "asio", test))]
use super::asio_host::{AsioDriverApi, AsioHost};
#[cfg(feature = "instrument")]
use super::callback::CallbackClosureLifetime;
use super::callback::{CallbackCore, CallbackMoment};
use super::error::AudioOpenError;
#[cfg(feature = "instrument")]
use super::metrics::DEVICE_RESOLVES;
use super::mmcss::{MmcssRegistration, MmcssTask};
use super::rebind::StreamFaultSink;
use cpal::traits::DeviceTrait;
use cpal::traits::HostTrait;
use cpal::traits::StreamTrait;
use cpal::{FromSample, OutputCallbackInfo, Sample, SampleFormat, SizedSample, Stream};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// Which output device a session wants. This is a rule, not a captured handle: every bind
/// re-reads the platform's own record and reports which device it actually got.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum AudioDeviceSelection {
    /// The output the listener chose for the process ([`set_output_route`]), read again at every
    /// bind: a session follows a change of it from where it is.
    #[default]
    Chosen,
    /// A WASAPI endpoint by the name it gives itself, whatever the listener chose.
    Named(String),
}

/// An audio system, in REAPER's sense: the platform API an output is opened through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AudioOutputHost {
    Wasapi,
    Asio,
}

/// Where the listener sends the sound, chosen as REAPER's Preferences → Device has it: an audio
/// system first, then one of its devices. WASAPI plays shared, at the engine's mix rate; an ASIO
/// driver plays at the song's own rate where it can (`asio_host::choose_output_rate`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum AudioOutputRoute {
    /// WASAPI on whatever Windows' default output is, following it when it changes.
    #[default]
    WasapiDefault,
    /// WASAPI on one endpoint, by its endpoint id (`IMMDevice::GetId`).
    Wasapi(String),
    /// The ASIO driver registered under this name: the key the SDK loads it by.
    Asio(String),
}

impl AudioOutputRoute {
    pub fn host(&self) -> AudioOutputHost {
        match self {
            Self::WasapiDefault | Self::Wasapi(_) => AudioOutputHost::Wasapi,
            Self::Asio(_) => AudioOutputHost::Asio,
        }
    }
}

/// What a device says it can do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioDeviceCaps {
    pub channels: u16,
    /// The rates it plays at: a shared WASAPI endpoint its mix rate alone, an ASIO driver every
    /// common rate it accepts.
    pub rates: Vec<u32>,
    /// Whether it takes DSD natively; `None` until a DSD song has asked it.
    pub dsd: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioOutputDeviceInfo {
    /// What a route names it by: the endpoint id, or the driver's registered name.
    pub id: String,
    pub name: String,
    /// Windows' default output. Only a WASAPI endpoint is ever one.
    pub is_default: bool,
    /// An ASIO driver that wraps another API rather than being a sound card's own
    /// ([`is_generic_asio_driver`]).
    pub generic: bool,
    /// `None` where learning it means loading it: an ASIO driver that is not the loaded one.
    pub caps: Option<AudioDeviceCaps>,
}

/// One audio system and its devices, in the platform's own order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioOutputSystem {
    pub host: AudioOutputHost,
    pub devices: Vec<AudioOutputDeviceInfo>,
}

/// The audio systems this machine has, and each one's devices, now: WASAPI's active render
/// endpoints, and — in a build with ASIO, when any driver is registered — the ASIO drivers. A
/// query, never cached, and it loads no driver: the registry lists the drivers, and only the one
/// already loaded is asked what it can do. Blocks for as long as Windows takes to answer; call it
/// off a window's thread.
pub fn enumerate_output_systems() -> Vec<AudioOutputSystem> {
    let default = super::endpoints::default_render_id();
    let wasapi = super::endpoints::render_endpoints().unwrap_or_default();
    #[cfg(feature = "asio")]
    let asio = Some(
        super::asio::driver_names()
            .into_iter()
            .map(|name| {
                let caps = super::asio::host().loaded_caps(&name);
                (name, caps)
            })
            .collect(),
    );
    #[cfg(not(feature = "asio"))]
    let asio = None;
    output_systems(wasapi, default.as_deref(), asio)
}

/// The systems, shaped: WASAPI always, its endpoints in Windows' order with the default marked;
/// ASIO only where it is built (`asio` is `Some`) and a driver is registered, in the registry's
/// order.
pub(super) fn output_systems(
    wasapi: Vec<super::endpoints::Endpoint>,
    default: Option<&str>,
    asio: Option<Vec<(String, Option<AudioDeviceCaps>)>>,
) -> Vec<AudioOutputSystem> {
    let mut systems = vec![AudioOutputSystem {
        host: AudioOutputHost::Wasapi,
        devices: wasapi
            .into_iter()
            .map(|endpoint| AudioOutputDeviceInfo {
                is_default: default == Some(endpoint.id.as_str()),
                generic: false,
                caps: endpoint.format.map(|(channels, rate)| AudioDeviceCaps {
                    channels,
                    rates: vec![rate],
                    dsd: None,
                }),
                id: endpoint.id,
                name: endpoint.name,
            })
            .collect(),
    }];
    if let Some(drivers) = asio.filter(|drivers| !drivers.is_empty()) {
        systems.push(AudioOutputSystem {
            host: AudioOutputHost::Asio,
            devices: drivers
                .into_iter()
                .map(|(name, caps)| AudioOutputDeviceInfo {
                    id: name.clone(),
                    is_default: false,
                    generic: is_generic_asio_driver(&name),
                    caps,
                    name,
                })
                .collect(),
        });
    }
    systems
}

/// ASIO drivers that are not a sound card's own: each wraps WASAPI, kernel streaming or another
/// program's mixer behind the ASIO interface, so playing through one buys nothing over WASAPI
/// and takes the endpoint exclusively besides. Matched case-insensitively as substrings of the
/// driver's registered name; this list is the whole of the host policy's judgement.
pub const GENERIC_ASIO_DRIVERS: &[&str] = &[
    "asio4all",
    "flexasio",
    "asio2wasapi",
    "asio2ks",
    "generic low latency asio",
    "steinberg built-in asio",
    "realtek asio",
    "voicemeeter",
    "jackrouter",
    "rearoute",
];

pub fn is_generic_asio_driver(name: &str) -> bool {
    let name = name.to_lowercase();
    GENERIC_ASIO_DRIVERS
        .iter()
        .any(|generic| name.contains(generic))
}

/// The stream shape asked of a device. An ASIO driver reports every output it has — eighteen on
/// a rack interface — and its first two are the main pair, so the sound goes there, downmixed by
/// the decoder like any other stereo endpoint's.
pub(super) fn stream_config_for(
    host: AudioOutputHost,
    supported: &cpal::SupportedStreamConfig,
) -> cpal::StreamConfig {
    let mut config = supported.config();
    if host == AudioOutputHost::Asio {
        config.channels = config.channels.min(2);
    }
    config
}

/// [`RouteState::version`], readable with a load: every bound output compares its own against it
/// once a frame. Written only under [`ROUTE`]'s lock, with the state it belongs to.
static OUTPUT_ROUTE_VERSION: AtomicU64 = AtomicU64::new(0);
/// [`RouteState::fallback_version`], readable with a load: a host looks for a new fallback every frame.
static FALLBACK_VERSION: AtomicU64 = AtomicU64::new(0);
static ROUTE: Mutex<RouteState> = Mutex::new(RouteState::new());

/// Why an output is not where the listener sent it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioFallbackReason {
    /// The chosen endpoint is not among the active ones: unplugged, disabled, or gone.
    DeviceMissing,
    /// The chosen ASIO driver did not load, or would not play; the driver's words for it.
    DriverFailed(String),
}

/// Outputs went to WASAPI's default output instead of the listener's route, and the sound played
/// on. The route stays chosen: an endpoint that comes back takes its outputs back, and a failed
/// driver is tried again when the listener chooses it again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioOutputFallback {
    /// Moves with each new fallback ([`output_fallback_version`]).
    pub version: u64,
    pub wanted: AudioOutputRoute,
    pub reason: AudioFallbackReason,
}

/// One reading of where outputs should be: the route a bind resolves by, the version that route
/// belongs to, and the choice it came from. A bind takes one and carries it to its commit, so a
/// binding is marked with the version of the route it was resolved by — never a later one — and a
/// route changed while it resolved leaves it behind the process's version, which rebinds it. What a
/// bind finds out (a driver that failed, an endpoint that is missing) is told against the same
/// reading, and changes nothing once the listener has chosen again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RouteSnapshot {
    pub(super) route: AudioOutputRoute,
    pub(super) version: u64,
    choice: u64,
}

/// The listener's route and what binding has made of it. Pure, so its law is tested on values;
/// the process keeps one ([`ROUTE`]).
#[derive(Debug)]
pub(super) struct RouteState {
    pub(super) chosen: AudioOutputRoute,
    /// Counts the listener's choices, the same route chosen again included.
    choice: u64,
    /// Moves whenever where outputs should be changes.
    version: u64,
    /// Why outputs are not on the chosen route, when they are not: the chosen ASIO driver failed
    /// (until the route is chosen again: a driver that refused once — another program holds it,
    /// the interface is gone — would refuse every rebind the same way), or the chosen endpoint
    /// was missing at the last bind (until it comes back). The one record of both.
    fallback: Option<AudioOutputFallback>,
    fallback_version: u64,
}

impl RouteState {
    pub(super) const fn new() -> Self {
        Self {
            chosen: AudioOutputRoute::WasapiDefault,
            choice: 0,
            version: 0,
            fallback: None,
            fallback_version: 0,
        }
    }

    pub(super) fn snapshot(&self) -> RouteSnapshot {
        RouteSnapshot {
            route: self.effective(),
            version: self.version,
            choice: self.choice,
        }
    }

    pub(super) fn move_outputs(&mut self) {
        self.version += 1;
    }

    /// The listener chose `route`. Answers whether bound outputs must move: a new route, or the
    /// route that failed chosen again — which is how a failed driver is tried again.
    pub(super) fn choose(&mut self, route: AudioOutputRoute) -> bool {
        let moved = self.chosen != route || self.fallback.is_some();
        self.chosen = route;
        self.choice += 1;
        self.fallback = None;
        if moved {
            self.move_outputs();
        }
        moved
    }

    /// The route outputs bind by: the chosen one, less a driver that failed. A missing endpoint
    /// is still asked for first at every bind: it may be back.
    pub(super) fn effective(&self) -> AudioOutputRoute {
        match &self.chosen {
            AudioOutputRoute::Asio(_) if self.asio_failed() => AudioOutputRoute::WasapiDefault,
            chosen => chosen.clone(),
        }
    }

    /// Whether outputs stand on Windows' default output, and so move when it does.
    pub(super) fn follows_default(&self) -> bool {
        match self.chosen {
            AudioOutputRoute::WasapiDefault => true,
            AudioOutputRoute::Wasapi(_) => self.is_endpoint_missing(),
            AudioOutputRoute::Asio(_) => self.asio_failed(),
        }
    }

    /// Whether the endpoint `id` arriving takes outputs back to it.
    pub(super) fn awaits(&self, id: &str) -> bool {
        self.is_endpoint_missing() && self.chose_endpoint(id)
    }

    /// The listener chose the endpoint `id`.
    fn chose_endpoint(&self, id: &str) -> bool {
        matches!(&self.chosen, AudioOutputRoute::Wasapi(chosen) if chosen == id)
    }

    /// A bind's finding counts only against the choice it was read under.
    fn is_current(&self, at: &RouteSnapshot) -> bool {
        at.choice == self.choice
    }

    /// Whether outputs are on the endpoint `id` by the listener's choice: its leaving is theirs
    /// to follow.
    pub(super) fn plays_on(&self, id: &str) -> bool {
        !self.is_endpoint_missing() && self.chose_endpoint(id)
    }

    /// The driver read at `at` failed. Answers the reading outputs bind by from now on when that
    /// is news — the outputs on it move — and `None` when it is not: already known, or told of a
    /// choice the listener has since replaced.
    pub(super) fn driver_failed(
        &mut self,
        at: &RouteSnapshot,
        reason: &str,
    ) -> Option<RouteSnapshot> {
        if !self.is_current(at) || self.asio_failed() || self.chosen.host() != AudioOutputHost::Asio
        {
            return None;
        }
        self.fall_back(AudioFallbackReason::DriverFailed(reason.to_owned()));
        self.move_outputs();
        Some(self.snapshot())
    }

    /// A bind by `at` did not find the chosen endpoint and went to the default instead.
    pub(super) fn endpoint_missing(&mut self, at: &RouteSnapshot) {
        if self.is_current(at) && !self.is_endpoint_missing() {
            self.fall_back(AudioFallbackReason::DeviceMissing);
        }
    }

    /// A bind by `at` found the chosen endpoint.
    pub(super) fn endpoint_found(&mut self, at: &RouteSnapshot) {
        if self.is_current(at) && self.is_endpoint_missing() {
            self.fallback = None;
        }
    }

    /// The chosen ASIO driver failed (`fallback`'s reason).
    fn asio_failed(&self) -> bool {
        matches!(
            self.fallback.as_ref().map(|f| &f.reason),
            Some(AudioFallbackReason::DriverFailed(_))
        )
    }

    /// The chosen endpoint was missing at the last bind (`fallback`'s reason).
    fn is_endpoint_missing(&self) -> bool {
        matches!(
            self.fallback.as_ref().map(|f| &f.reason),
            Some(AudioFallbackReason::DeviceMissing)
        )
    }

    pub(super) fn fallback(&self) -> Option<&AudioOutputFallback> {
        self.fallback.as_ref()
    }

    fn fall_back(&mut self, reason: AudioFallbackReason) {
        self.fallback_version += 1;
        self.fallback = Some(AudioOutputFallback {
            version: self.fallback_version,
            wanted: self.chosen.clone(),
            reason,
        });
    }
}

/// The route state, changed by `change` under its lock, its published numbers brought up to
/// date before the lock goes; then, when the outputs must move, every live session's render
/// thread is woken to see it.
pub(super) fn change_route<R>(why: &str, change: impl FnOnce(&mut RouteState) -> R) -> R {
    let mut state = ROUTE.lock().unwrap_or_else(PoisonError::into_inner);
    let version = state.version;
    let answer = change(&mut state);
    let moved = state.version != version;
    OUTPUT_ROUTE_VERSION.store(state.version, Ordering::Release);
    FALLBACK_VERSION.store(state.fallback_version, Ordering::Release);
    drop(state);
    if moved {
        #[cfg(feature = "instrument")]
        eprintln!("player-core[audio] outputs move: {why}");
        wake_sessions();
    }
    #[cfg(not(feature = "instrument"))]
    let _ = why;
    answer
}

fn route_state() -> MutexGuard<'static, RouteState> {
    ROUTE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The render-thread wakes of the live sessions: a move of the outputs is seen by a session's
/// media loop, which may be asleep — paused, or waiting on a device that went without a word.
static SESSION_WAKES: Mutex<Vec<std::sync::Weak<dyn Fn() + Send + Sync>>> = Mutex::new(Vec::new());

/// Registers a session's wake for as long as the session holds it.
pub(super) fn wake_on_move(wake: &Arc<dyn Fn() + Send + Sync>) {
    let mut wakes = SESSION_WAKES.lock().unwrap_or_else(PoisonError::into_inner);
    wakes.retain(|wake| wake.strong_count() > 0);
    wakes.push(Arc::downgrade(wake));
}

fn wake_sessions() {
    let live: Vec<Arc<dyn Fn() + Send + Sync>> = {
        let mut wakes = SESSION_WAKES.lock().unwrap_or_else(PoisonError::into_inner);
        wakes.retain(|wake| wake.strong_count() > 0);
        wakes.iter().filter_map(std::sync::Weak::upgrade).collect()
    };
    for wake in live {
        wake();
    }
}

/// Sends every output, from now on and from where each is, to `route`. Answers the route set:
/// in a build without ASIO an ASIO route is WASAPI's default. Choosing the route that fell back
/// tries it again. Nothing is opened here; the outputs rebind on their own threads.
pub fn set_output_route(route: AudioOutputRoute) -> AudioOutputRoute {
    #[cfg(not(feature = "asio"))]
    let route = if route.host() == AudioOutputHost::Asio {
        AudioOutputRoute::WasapiDefault
    } else {
        route
    };
    change_route(&format!("the listener chose {route:?}"), |state| {
        state.choose(route.clone())
    });
    route
}

/// The route the listener chose, whether or not outputs could follow it ([`output_fallback`]).
pub fn output_route() -> AudioOutputRoute {
    route_state().chosen.clone()
}

/// Why outputs are not on the chosen route, while they are not.
pub fn output_fallback() -> Option<AudioOutputFallback> {
    route_state().fallback().cloned()
}

/// Moves with every new fallback; zero before the first.
pub fn output_fallback_version() -> u64 {
    FALLBACK_VERSION.load(Ordering::Acquire)
}

pub(super) fn output_route_version() -> u64 {
    OUTPUT_ROUTE_VERSION.load(Ordering::Acquire)
}

/// Where outputs should be now, read once for one bind.
pub(super) fn route_snapshot() -> RouteSnapshot {
    route_state().snapshot()
}

/// Whether outputs currently go to an ASIO driver. Read around an open to tell whether a failed
/// build moved them off it, which is the one failure a second attempt can get past.
pub(super) fn asio_in_use() -> bool {
    route_snapshot().route.host() == AudioOutputHost::Asio
}

/// Moves every output off the driver read at `at`, onto WASAPI's default, after it failed.
/// Answers the reading a bind goes on with: the one after the fallback when this made it, else
/// `at` as it was — a stale reading's binding is rebound by its version.
#[cfg_attr(not(feature = "asio"), allow(dead_code))]
pub(super) fn demote_asio(at: &RouteSnapshot, reason: &str) -> RouteSnapshot {
    change_route(&format!("the ASIO driver failed: {reason}"), |state| {
        state.driver_failed(at, reason)
    })
    .unwrap_or_else(|| at.clone())
}

/// What the endpoint watch tells a host ([`watch_outputs`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputEvent {
    /// An endpoint came or went, or Windows' default output moved: a device list shown is stale.
    DevicesChanged,
    /// The endpoint outputs were last bound to went: unplugged, disabled, removed. Outputs move on
    /// by themselves (the route's fallback); whether the sound goes on is the host's to say.
    DeviceLost,
}

type OutputListener = Arc<dyn Fn(OutputEvent) + Send + Sync>;

static OUTPUT_LISTENERS: Mutex<Vec<OutputListener>> = Mutex::new(Vec::new());

/// The WASAPI endpoint binds landed on, by id: the last one, and the one before it with when it
/// was left. Windows may move the default (and outputs rebind onto it) before it says the old
/// endpoint went; the one just left still counts as the one played on.
#[derive(Default)]
struct Bound {
    now: Option<String>,
    before: Option<(String, Instant)>,
}

static BOUND_ENDPOINT: Mutex<Bound> = Mutex::new(Bound {
    now: None,
    before: None,
});

/// How long the endpoint left by a rebind still counts as the one played on.
const BOUND_GRACE: Duration = Duration::from_secs(3);

/// Calls `listener` with every [`OutputEvent`] from now on, on Windows' notification threads: it
/// must return at once (send a message, wake a thread). Starts the endpoint watch.
pub fn watch_outputs(listener: Arc<dyn Fn(OutputEvent) + Send + Sync>) {
    OUTPUT_LISTENERS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(listener);
    super::endpoints::watch();
}

fn tell(event: OutputEvent) {
    let listeners = OUTPUT_LISTENERS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    for listener in listeners {
        listener(event);
    }
}

fn bind_endpoint(id: Option<String>) {
    let mut bound = BOUND_ENDPOINT
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if bound.now != id {
        if let Some(old) = bound.now.take() {
            bound.before = Some((old, Instant::now()));
        }
        bound.now = id;
    }
}

/// Whether `id`, gone, is the endpoint played on (and forgets it).
fn bound_left(id: &str) -> bool {
    let mut bound = BOUND_ENDPOINT
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if bound.now.as_deref() == Some(id) {
        bound.now = None;
        return true;
    }
    match bound.before.take() {
        Some((old, at)) if old == id && at.elapsed() < BOUND_GRACE => true,
        other => {
            bound.before = other;
            false
        }
    }
}

/// Windows' default output changed (the endpoint watch). Outputs that stand on it move with it.
pub(super) fn default_endpoint_changed() {
    change_route("Windows' default output changed", |state| {
        if state.follows_default() {
            state.move_outputs();
        }
    });
    tell(OutputEvent::DevicesChanged);
}

/// An endpoint became active (the endpoint watch). The chosen one, back, takes its outputs back.
pub(super) fn endpoint_arrived(id: &str) {
    change_route("the chosen endpoint is back", |state| {
        if state.awaits(id) {
            state.move_outputs();
        }
    });
    tell(OutputEvent::DevicesChanged);
}

/// An endpoint went — removed, disabled, unplugged (the endpoint watch). The chosen one, while
/// outputs are on it, moves them: a WASAPI stream on an endpoint that went need not say so.
pub(super) fn endpoint_left(id: &str) {
    change_route("the chosen endpoint went", |state| {
        if state.plays_on(id) {
            state.move_outputs();
        }
    });
    if bound_left(id) {
        tell(OutputEvent::DeviceLost);
    }
    tell(OutputEvent::DevicesChanged);
}

/// Headless runs play into nothing: no WASAPI endpoint is opened and nothing is heard, but the
/// callback is driven at the wall clock's pace, so the ring drains, the media clock advances and
/// every transport edge ramps exactly as it does into a device. It exists so the playback laws can
/// be driven against a simulated network in a loop, on a machine with no speakers to disturb.
pub(super) static NULL_OUTPUT: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "test-support")]
/// Routes every audio output opened from now on into the null sink. Process-wide; for tests.
pub fn use_null_audio_output(enabled: bool) {
    NULL_OUTPUT.store(enabled, Ordering::Release);
}

/// What the null sink has played, interleaved at its own format, while a test listens; `None`
/// while nobody does.
static NULL_CAPTURE: std::sync::Mutex<Option<Vec<f32>>> = std::sync::Mutex::new(None);
/// How long each callback took while a test listens, with the frames it was asked for.
static NULL_CALLBACK_TIMES: std::sync::Mutex<Vec<(usize, Duration)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(feature = "test-support")]
/// The callbacks the null sink has timed since it was last asked: `(frames asked for, time it
/// took)`, kept only while `capture_null_audio_output` is on. Process-wide; for tests.
pub fn take_null_audio_output_callback_times() -> Vec<(usize, Duration)> {
    std::mem::take(
        &mut *NULL_CALLBACK_TIMES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    )
}
/// Periods the null sink's thread woke too late to hand over in time: a device would have played
/// silence of its own for each.
static NULL_LATE_PERIODS: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "test-support")]
/// Starts keeping what the null sink plays (`true`), or stops (`false`); either way it answers
/// what was kept since the last call. Process-wide; for tests.
pub fn capture_null_audio_output(capturing: bool) -> Vec<f32> {
    let mut capture = NULL_CAPTURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let kept = capture.take().unwrap_or_default();
    *capture = capturing.then(Vec::new);
    kept
}

#[cfg(feature = "test-support")]
/// How many periods the null sink's thread has been late for, since the process started: the
/// device-side glitches that its thread's scheduling alone would have caused.
pub fn null_audio_output_late_periods() -> u64 {
    NULL_LATE_PERIODS.load(Ordering::Acquire)
}

/// Whether a DSD song may go to an ASIO driver as DoP. A driver that takes the DoP carrier rate
/// cannot say whether the DAC behind it decodes DoP, and one that does not plays the carrier as
/// PCM: the music faint under loud noise. So DoP is the listener's choice, off until turned on, as
/// in foobar2000's SACD component; native DSD, which the driver itself declares, needs no choice.
pub(super) static DOP_OUTPUT: AtomicBool = AtomicBool::new(false);

/// Lets DSD songs opened from now on go to an ASIO driver as DoP when it has no native DSD.
pub fn set_dop_output(enabled: bool) {
    DOP_OUTPUT.store(enabled, Ordering::Release);
}

/// Whether DSD songs may go to an ASIO driver as DoP ([`set_dop_output`]).
pub fn dop_output() -> bool {
    DOP_OUTPUT.load(Ordering::Acquire)
}

/// The endpoint a binding plays into.
pub(super) enum OutputDevice {
    /// A cpal device, its host, and — on ASIO — the lease the binding holds from its resolve on.
    Cpal(cpal::Device, AudioOutputHost, Option<AsioLease>),
    #[cfg(feature = "asio")]
    NativeAsio(asio::NativeAsioDevice),
    Null,
}

/// The null sink's format: what a default Windows endpoint reports.
pub(super) const NULL_OUTPUT_RATE: u32 = 48_000;
static NULL_OUTPUT_RATE_NOW: AtomicU32 = AtomicU32::new(NULL_OUTPUT_RATE);

#[cfg(feature = "test-support")]
/// Sets the rate null outputs opened from now on run at — 48 kHz until changed. At a source's own
/// rate nothing is resampled, so what the sink plays can be compared with the decoded samples
/// frame for frame. Process-wide; for tests.
pub fn set_null_audio_output_rate(rate: u32) {
    NULL_OUTPUT_RATE_NOW.store(rate.max(1), Ordering::Release);
}

pub(super) fn null_output_rate() -> u32 {
    NULL_OUTPUT_RATE_NOW.load(Ordering::Acquire)
}
pub(super) const NULL_OUTPUT_CHANNELS: u16 = 2;
/// One null callback's worth of audio — WASAPI's default period.
pub(super) const NULL_OUTPUT_PERIOD: Duration = Duration::from_millis(10);

static NULL_OUTPUT_UNEVEN: AtomicBool = AtomicBool::new(false);
static NULL_OUTPUT_FRAMES: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "test-support")]
/// Makes null outputs opened from now on ask for `frames` a callback — an ASIO driver's 64, say —
/// at the pace those frames last; 0 goes back to a 10 ms period. Process-wide; for tests.
pub fn set_null_audio_output_period_frames(frames: usize) {
    NULL_OUTPUT_FRAMES.store(frames, Ordering::Release);
}
/// What a shared-mode endpoint hands a client per wake-up, in thousandths of a nominal period:
/// its own period's frame count, what the padding happens to leave (441 of 480, 448), a sliver,
/// a doubled buffer after a late wake, and the full buffer of the first fill.
const NULL_UNEVEN_PERIODS: [usize; 12] = [
    1_000, 919, 933, 2, 15, 2_000, 1_090, 500, 2_200, 27, 1_000, 314,
];

#[cfg(feature = "test-support")]
/// Makes null outputs opened from now on ask for uneven buffers, as a real shared-mode endpoint
/// does, at the same average pace. Process-wide; for tests.
pub fn set_null_audio_output_uneven(uneven: bool) {
    NULL_OUTPUT_UNEVEN.store(uneven, Ordering::Release);
}

/// A started-or-paused output, whichever endpoint is behind it.
pub(super) enum OutputStream {
    Cpal(Stream),
    /// A stream on the ASIO driver, played from the moment it is built, with the watch that
    /// turns the driver's reset request into a device fault and the lease that keeps the driver
    /// loaded under it. Fields drop in order: the stream, the watch, and the lease last.
    #[cfg(feature = "asio")]
    Asio {
        stream: Stream,
        _reset: super::asio::ResetWatch,
        _lease: AsioLease,
    },
    #[cfg(feature = "asio")]
    NativeAsio(asio::NativeAsioStream),
    Null(NullStream),
}

impl OutputStream {
    pub(super) fn play(&self) -> Result<(), String> {
        match self {
            Self::Cpal(stream) => stream.play().map_err(|error| error.to_string()),
            #[cfg(feature = "asio")]
            Self::Asio { stream, .. } => stream.play().map_err(|error| error.to_string()),
            #[cfg(feature = "asio")]
            Self::NativeAsio(stream) => stream.play(),
            Self::Null(stream) => {
                stream.playing.store(true, Ordering::Release);
                stream.thread.thread().unpark();
                Ok(())
            }
        }
    }

    pub(super) fn pause(&self) -> Result<(), String> {
        match self {
            Self::Cpal(stream) => stream.pause().map_err(|error| error.to_string()),
            // Deliberately not paused. cpal's ASIO pause only skips the callback, from the next
            // buffer on, so the ramp to zero asked for just before would never be rendered and
            // the sound would stop mid-sample. Left running, the callback renders the ramp and then
            // zeros. The driver runs either way (the driver thread's keeper stream silences its
            // buffers between songs), so this costs one zero-fill per ASIO period and no device
            // time.
            #[cfg(feature = "asio")]
            Self::Asio { .. } => Ok(()),
            #[cfg(feature = "asio")]
            Self::NativeAsio(stream) => stream.pause(),
            Self::Null(stream) => {
                stream.playing.store(false, Ordering::Release);
                Ok(())
            }
        }
    }
}

/// The null sink's "device thread": it calls the same callback a device would, one period of
/// frames per period of wall time, while playing.
pub(super) struct NullStream {
    pub(super) playing: Arc<AtomicBool>,
    pub(super) stop: Arc<AtomicBool>,
    pub(super) thread: thread::JoinHandle<()>,
}

impl NullStream {
    pub(super) fn drive_callback(mut callback: CallbackCore) -> Result<Self, AudioOpenError> {
        let playing = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (thread_playing, thread_stop) = (Arc::clone(&playing), Arc::clone(&stop));
        let rate = null_output_rate();
        let frames = match NULL_OUTPUT_FRAMES.load(Ordering::Acquire) {
            0 => (rate as usize * NULL_OUTPUT_PERIOD.as_millis() as usize) / 1_000,
            frames => frames,
        };
        let uneven = NULL_OUTPUT_UNEVEN.load(Ordering::Acquire);
        let thread = thread::Builder::new()
            .name("player-core-audio-null-output".to_owned())
            .spawn(move || {
                // Scheduled as a device's thread is (`mmcss`), so what it measures is the chain
                // behind it and not its own place in the queue.
                let _scheduling = MmcssRegistration::current_thread_recorded(
                    MmcssTask::ProAudio,
                    &callback.metrics.output_thread_mmcss,
                );
                let largest = NULL_UNEVEN_PERIODS.iter().max().copied().unwrap_or(1_000);
                let mut buffer = vec![
                    0.0_f32;
                    (frames * largest).div_ceil(1_000).max(frames)
                        * usize::from(NULL_OUTPUT_CHANNELS)
                ];
                let mut wake = 0usize;
                let mut due = Instant::now();
                while !thread_stop.load(Ordering::Acquire) {
                    if !thread_playing.load(Ordering::Acquire) {
                        thread::park_timeout(NULL_OUTPUT_PERIOD);
                        due = Instant::now();
                        continue;
                    }
                    let observed_at = Instant::now();
                    // A shared-mode endpoint holds about two periods: one handed over a period
                    // after it was due has already been played as silence.
                    if observed_at.saturating_duration_since(due) > NULL_OUTPUT_PERIOD {
                        NULL_LATE_PERIODS.fetch_add(1, Ordering::Relaxed);
                    }
                    let this = if uneven {
                        wake += 1;
                        (frames * NULL_UNEVEN_PERIODS[wake % NULL_UNEVEN_PERIODS.len()])
                            .div_ceil(1_000)
                            .max(1)
                    } else {
                        frames
                    };
                    let output = &mut buffer[..this * usize::from(NULL_OUTPUT_CHANNELS)];
                    callback.render(
                        output,
                        CallbackMoment {
                            observed_at,
                            delivery_delay: NULL_OUTPUT_PERIOD,
                        },
                    );
                    let took = observed_at.elapsed();
                    if let Some(capture) = NULL_CAPTURE
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .as_mut()
                    {
                        capture.extend_from_slice(output);
                        NULL_CALLBACK_TIMES
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push((this, took));
                    }
                    due += Duration::from_nanos(this as u64 * 1_000_000_000 / u64::from(rate));
                    let now = Instant::now();
                    if due > now {
                        thread::sleep(due - now);
                    } else if now - due > NULL_OUTPUT_PERIOD * 10 {
                        due = now; // the machine stalled; a device would have glitched, not caught up
                    }
                }
            })
            .map_err(|error| AudioOpenError::BuildStream(error.to_string()))?;
        Ok(Self {
            playing,
            stop,
            thread,
        })
    }
}

impl Drop for NullStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.thread().unpark();
        // The thread leaves within a period; the owner thread, which drops streams, may wait.
        while !self.thread.is_finished() {
            thread::sleep(Duration::from_millis(1));
        }
    }
}

/// A resolved endpoint: the device, the name it reports, the host it belongs to, and the format
/// it asks for.
pub(super) struct ResolvedOutput {
    pub(super) device: cpal::Device,
    pub(super) name: String,
    pub(super) host: AudioOutputHost,
    pub(super) supported: cpal::SupportedStreamConfig,
    /// The hold on the ASIO driver a binding on it keeps; `None` on WASAPI.
    pub(super) lease: Option<AsioLease>,
    /// The WASAPI endpoint the resolve landed on, when it went by route (the chosen one, or the
    /// default it fell back to); `None` for a named device and on ASIO. Its speaker layout is
    /// read from this one endpoint, not decided again.
    pub(super) endpoint: Option<String>,
    /// The reading of the route this was resolved by — after a fallback this resolve made, the
    /// reading after it. Its version is the one the binding is marked with.
    pub(super) route: RouteSnapshot,
}

/// Resolves a selection: the chosen route's device, read afresh. On ASIO the driver is asked for
/// `source_rate`, the rate of the sound about to play, when it can do it
/// (`asio_host::choose_output_rate`). An ASIO driver that does not load, or does not answer for
/// its format, and a chosen endpoint that is not there, fall back to WASAPI's default output in
/// this call, so the open or rebind that asked never sees the failure ([`output_fallback`]).
/// Off ASIO, the driver is unloaded before WASAPI is opened, so the interface it claimed is free.
pub(super) fn resolve_output_device(
    selection: &AudioDeviceSelection,
    at: RouteSnapshot,
    source_rate: Option<u32>,
) -> Result<ResolvedOutput, AudioOpenError> {
    #[cfg(feature = "instrument")]
    DEVICE_RESOLVES.fetch_add(1, Ordering::Relaxed);
    #[cfg_attr(not(feature = "asio"), allow(unused_mut))]
    let mut at = at;
    #[cfg(feature = "asio")]
    if let (AudioDeviceSelection::Chosen, AudioOutputRoute::Asio(driver)) = (selection, &at.route) {
        let driver = driver.clone();
        let opened = super::asio::host().open(&driver, source_rate, |loaded| {
            let device = loaded.device();
            let supported = device.default_output_config();
            (device, supported)
        });
        match opened {
            Ok(((device, Ok(supported)), lease)) => {
                bind_endpoint(None);
                return Ok(ResolvedOutput {
                    device,
                    name: driver,
                    host: AudioOutputHost::Asio,
                    supported,
                    lease: Some(lease),
                    endpoint: None,
                    route: at,
                });
            }
            Ok(((_, Err(error)), _)) => {
                at = demote_asio(&at, &format!("no output format: {error}"))
            }
            Err(AsioOpenError::Resetting) => return Err(AudioOpenError::DeviceUnavailable(driver)),
            Err(AsioOpenError::Failed(error)) => at = demote_asio(&at, &error),
        }
    }
    #[cfg(feature = "asio")]
    if at.route.host() != AudioOutputHost::Asio {
        release_for_wasapi(super::asio::host())?;
    }
    let _ = source_rate;
    super::endpoints::watch();
    let (device, name, endpoint) = resolve_wasapi_device(selection, &at)?;
    let supported = device
        .default_output_config()
        .map_err(|error| AudioOpenError::DefaultOutputConfig(error.to_string()))?;
    Ok(ResolvedOutput {
        device,
        name,
        host: AudioOutputHost::Wasapi,
        supported,
        lease: None,
        endpoint,
        route: at,
    })
}

/// Frees the interface an ASIO driver claims before WASAPI opens: the driver unloaded once no
/// output holds it, waiting up to the idle wait. One still held past it is
/// [`AudioOpenError::AsioStillHeld`], and WASAPI is not opened beside it.
#[cfg(any(feature = "asio", test))]
pub(super) fn release_for_wasapi<A: AsioDriverApi>(
    host: &AsioHost<A>,
) -> Result<(), AudioOpenError> {
    if host.release_when_idle() {
        Ok(())
    } else {
        #[cfg(feature = "instrument")]
        eprintln!("player-core[audio] the ASIO driver is still held; WASAPI waits for it");
        Err(AudioOpenError::AsioStillHeld)
    }
}

/// The WASAPI endpoint for `selection` under the route read at `at`: a named one by its name; the
/// chosen endpoint while it is active, else — the fallback noted, against `at` — the default, as
/// for every other route (a failed driver's included). With the device, its name and the endpoint
/// it is (`None` for a named one).
fn resolve_wasapi_device(
    selection: &AudioDeviceSelection,
    at: &RouteSnapshot,
) -> Result<(cpal::Device, String, Option<String>), AudioOpenError> {
    let host = cpal::default_host();
    let default = || {
        host.default_output_device()
            .ok_or(AudioOpenError::NoOutputDevice)
    };
    let (device, endpoint) = match (selection, &at.route) {
        (AudioDeviceSelection::Named(name), _) => (
            host.output_devices()
                .map_err(|error| AudioOpenError::DeviceEnumeration(error.to_string()))?
                .find(|device| device.name().is_ok_and(|actual| &actual == name))
                .ok_or_else(|| AudioOpenError::DeviceUnavailable(name.clone()))?,
            None,
        ),
        (AudioDeviceSelection::Chosen, AudioOutputRoute::Wasapi(id)) => {
            match super::endpoints::cpal_device_for(id)
                .map_err(AudioOpenError::DeviceEnumeration)?
            {
                Some(device) => {
                    change_route("the chosen endpoint was found", |state| {
                        state.endpoint_found(at)
                    });
                    (device, Some(id.clone()))
                }
                None => {
                    change_route("the chosen endpoint is missing", |state| {
                        state.endpoint_missing(at)
                    });
                    #[cfg(feature = "instrument")]
                    eprintln!("player-core[audio] the chosen endpoint {id} is not active; playing on the default output");
                    (default()?, super::endpoints::default_render_id())
                }
            }
        }
        (
            AudioDeviceSelection::Chosen,
            AudioOutputRoute::WasapiDefault | AudioOutputRoute::Asio(_),
        ) => (default()?, super::endpoints::default_render_id()),
    };
    bind_endpoint(endpoint.clone());
    let name = device
        .name()
        .unwrap_or_else(|_| "<unnamed output device>".to_owned());
    Ok((device, name, endpoint))
}

/// Builds the stream on `device`, resolved by the route read at `at`. A build that fails on the
/// ASIO driver demotes that driver, so the attempt after it — the open's retry, or the rebind
/// ladder's next rung — lands on WASAPI.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_output_stream(
    device: &cpal::Device,
    host: AudioOutputHost,
    config: &cpal::StreamConfig,
    sample_format: SampleFormat,
    callback: CallbackCore,
    fault: StreamFaultSink,
    lease: Option<AsioLease>,
    at: &RouteSnapshot,
) -> Result<OutputStream, AudioOpenError> {
    let _ = (&lease, at);
    match host {
        // cpal's own raise of its WASAPI thread does nothing (`mmcss`); the callback registers
        // the thread it runs on.
        AudioOutputHost::Wasapi => build_sample_stream(
            device,
            config,
            sample_format,
            callback,
            fault,
            Some(MmcssTask::ProAudio),
        )
        .map(OutputStream::Cpal),
        #[cfg(feature = "asio")]
        AudioOutputHost::Asio => {
            let watch = super::asio::ResetWatch::new(device, fault.clone());
            let built = build_sample_stream(device, config, sample_format, callback, fault, None)
                .and_then(|stream| {
                    stream
                        .play()
                        .map_err(|error| AudioOpenError::PlayStream(error.to_string()))?;
                    Ok(stream)
                });
            match (built, watch, lease) {
                (Ok(stream), Some(watch), Some(lease)) => Ok(OutputStream::Asio {
                    stream,
                    _reset: watch,
                    _lease: lease,
                }),
                (Ok(_), _, _) => {
                    let reason = "the ASIO device is not an ASIO driver".to_owned();
                    demote_asio(at, &reason);
                    Err(AudioOpenError::BuildStream(reason))
                }
                (Err(error), _, _) => {
                    demote_asio(at, &error.to_string());
                    Err(error)
                }
            }
        }
        #[cfg(not(feature = "asio"))]
        AudioOutputHost::Asio => Err(AudioOpenError::BuildStream(
            "this build has no ASIO support".to_owned(),
        )),
    }
}

fn build_sample_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_format: SampleFormat,
    callback: CallbackCore,
    fault: StreamFaultSink,
    scheduling: Option<MmcssTask>,
) -> Result<Stream, AudioOpenError> {
    macro_rules! build {
        ($sample:ty) => {
            build_typed_output_stream::<$sample>(device, config, callback, fault, scheduling)
        };
    }
    match sample_format {
        SampleFormat::I8 => build!(i8),
        SampleFormat::I16 => build!(i16),
        SampleFormat::I32 => build!(i32),
        SampleFormat::I64 => build!(i64),
        SampleFormat::U8 => build!(u8),
        SampleFormat::U16 => build!(u16),
        SampleFormat::U32 => build!(u32),
        SampleFormat::U64 => build!(u64),
        SampleFormat::F32 => build!(f32),
        SampleFormat::F64 => build!(f64),
        format => Err(AudioOpenError::UnsupportedSampleFormat(format)),
    }
}

pub(super) fn build_typed_output_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut callback: CallbackCore,
    fault: StreamFaultSink,
    mut scheduling: Option<MmcssTask>,
) -> Result<Stream, AudioOpenError>
where
    T: SizedSample + Sample + FromSample<f32>,
{
    #[cfg(feature = "instrument")]
    let callback_lifetime = CallbackClosureLifetime::new();
    // Held by the closure, which the stream's thread owns and drops on its way out.
    let mut registration = None;
    device
        .build_output_stream(
            config,
            move |output: &mut [T], info: &OutputCallbackInfo| {
                #[cfg(feature = "instrument")]
                let _lifetime = &callback_lifetime;
                // Once, on the first callback: the stream's thread is only reachable from here.
                if let Some(task) = scheduling.take() {
                    registration = MmcssRegistration::current_thread_recorded(
                        task,
                        &callback.metrics.output_thread_mmcss,
                    );
                }
                let _ = &registration;
                callback.render(output, CallbackMoment::from_cpal(info));
            },
            move |_error| fault.report(),
            None,
        )
        .map_err(|error| AudioOpenError::BuildStream(error.to_string()))
}
