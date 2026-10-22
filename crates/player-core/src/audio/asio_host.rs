//! The ASIO driver's life: loaded, re-rated, reset and unloaded on one thread of its own, and
//! shared by every binding that plays through it.
//!
//! One thread, because the SDK's driver list initialises COM on the thread that first loads a
//! driver and on no other. A driver loaded from a thread without COM — any thread after the first
//! one, when loads came from whichever rebind thread happened to ask — fails in
//! `CoCreateInstance`, and the failure switched ASIO off: switched off once, ASIO could not be
//! switched on again. Every load, rate change and unload now runs on `player-core-asio`, which
//! initialises COM once and lives as long as the process.
//!
//! The driver stays loaded between songs, so consecutive songs at one rate reuse its running
//! buffers rather than stopping, disposing and recreating them. It is unloaded — on its thread,
//! once no binding holds it — when ASIO is switched off or the driver asks for a reset, so the
//! interface is free for WASAPI before WASAPI is opened on it.
//!
//! The driver layer is a trait, so the life cycle is tested against a fake driver that fails the
//! way a real one does when it is loaded from a thread without COM.

// Without the `asio` feature only the tests drive the life cycle.
#![cfg_attr(not(feature = "asio"), allow(dead_code))]

use super::asio_writer::AsioWriteFormat;
use super::device::AudioDeviceCaps;
#[cfg(feature = "instrument")]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// How long an open waits for the bindings still on the driver to close before it gives up on
/// what needed them gone (a rate change, an unload). A song ending hands its device over within
/// milliseconds; this bounds a binding that is wedged.
pub(super) const ASIO_IDLE_WAIT: Duration = Duration::from_secs(2);

/// Rates a driver is asked for when it cannot play a source's own, in order of preference after
/// the source's integer multiples.
pub(super) const COMMON_RATES: [u32; 10] = [
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000,
];

/// The driver calls the life cycle needs. The real one is asio-sys's (`asio.rs`); the tests' is a
/// fake that counts.
pub(super) trait AsioDriverApi: Send + Sync + 'static {
    type Driver: Send + Sync + 'static;
    /// Runs once, first, on the driver thread: COM for the real driver.
    fn enter_thread(&self);
    fn names(&self) -> Vec<String>;
    fn load(&self, name: &str) -> Result<Self::Driver, String>;
    /// Stops the driver and exits it. Only called once no binding holds it.
    fn unload(&self, driver: Self::Driver);
    fn rate(&self, driver: &Self::Driver) -> Option<u32>;
    fn can_rate(&self, driver: &Self::Driver, rate: u32) -> bool;
    /// Stops the driver, disposes its buffers and sets `rate`. Only called while no binding
    /// holds the driver: the buffers are freed under whoever still writes into them.
    fn set_rate(&self, driver: &Self::Driver, rate: u32) -> Result<(), String>;
    /// Stops and disposes the custom stream, restores the driver's PCM I/O format and recreates
    /// its PCM keeper. A PCM open calls this after every DSD owner has released its lease.
    fn restore_pcm(&self, _driver: &Self::Driver, _original_rate: u32) -> Result<(), String> {
        Ok(())
    }
    /// Attempts native DSD first and then DoP. `None` means that PCM remains the only safe path;
    /// the driver is restored before returning it. This is called only on the ASIO host thread.
    fn open_dsd(
        &self,
        _driver: &Self::Driver,
        _bit_rate: u32,
        _channels: u16,
    ) -> Result<Option<AsioDsdFormat>, String> {
        Ok(None)
    }
    /// The loaded driver's output count and the common rates it accepts: queries that touch no
    /// buffer, so a driver that is playing is asked as it plays.
    fn caps(&self, _driver: &Self::Driver) -> Option<(u16, Vec<u32>)> {
        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AsioDsdTransport {
    Native,
    Dop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AsioDsdFormat {
    pub(super) transport: AsioDsdTransport,
    /// The rate passed to ASIO: DSD bit samples for native DSD, PCM frames for DoP.
    pub(super) hardware_rate: u32,
    pub(super) channels: u16,
}

/// Format negotiation uses the same driver operations in production and in the host tests.
/// The caller has stopped and disposed the previous buffers before entering this protocol.
pub(super) trait AsioDsdIo {
    fn set_native(&self) -> bool;
    fn set_pcm(&self) -> Result<(), String>;
    fn accepts_rate(&self, rate: u32) -> bool;
    fn set_dsd_rate(&self, rate: u32) -> Result<(), String>;
    fn output_type(&self, channel: u16) -> i32;
}

/// Native DSD when the driver declares it; DoP only when `dop` is chosen (see `set_dop_output`).
pub(super) fn negotiate_dsd(
    io: &impl AsioDsdIo,
    bit_rate: u32,
    channels: u16,
    dop: bool,
) -> Result<Option<AsioDsdFormat>, String> {
    if io.set_native() {
        if io.accepts_rate(bit_rate)
            && io.set_dsd_rate(bit_rate).is_ok()
            && (0..channels).all(|channel| {
                AsioWriteFormat::for_sample_type(AsioDsdTransport::Native, io.output_type(channel))
                    .is_ok()
            })
        {
            return Ok(Some(AsioDsdFormat {
                transport: AsioDsdTransport::Native,
                hardware_rate: bit_rate,
                channels,
            }));
        }
        // A successful native request must be undone even when its rate or wire type failed.
        io.set_pcm()?;
    }
    if !dop {
        return Ok(None);
    }
    let rate = bit_rate / 16;
    if io.accepts_rate(rate)
        && io.set_dsd_rate(rate).is_ok()
        && (0..channels).all(|channel| {
            AsioWriteFormat::for_sample_type(AsioDsdTransport::Dop, io.output_type(channel)).is_ok()
        })
    {
        return Ok(Some(AsioDsdFormat {
            transport: AsioDsdTransport::Dop,
            hardware_rate: rate,
            channels,
        }));
    }
    Ok(None)
}

/// The rate to ask a driver for, for a source at `source` when the driver is at `current`: the
/// source's own; else its nearest integer multiple the driver can do; else the nearest rate at or
/// above the source, so nothing of its band is cut; else the nearest below; else `current`.
pub(super) fn choose_output_rate(source: u32, current: u32, can: impl Fn(u32) -> bool) -> u32 {
    if source == 0 {
        return current;
    }
    if source == current || can(source) {
        return source;
    }
    let multiples = (2..=16)
        .map(|factor| source.saturating_mul(factor))
        .take_while(|rate| *rate <= 768_000);
    if let Some(rate) = multiples
        .into_iter()
        .find(|rate| *rate == current || can(*rate))
    {
        return rate;
    }
    let supported: Vec<u32> = COMMON_RATES
        .into_iter()
        .filter(|rate| *rate == current || can(*rate))
        .collect();
    supported
        .iter()
        .copied()
        .filter(|rate| *rate >= source)
        .min()
        .or_else(|| supported.iter().copied().max())
        .unwrap_or(current)
}

/// A binding's hold on the driver. The driver is not unloaded and its rate is not changed while
/// any is alive; a binding keeps its lease for exactly as long as it keeps a device or a stream
/// on the driver.
#[derive(Clone)]
pub(crate) struct AsioLease(#[allow(dead_code)] Arc<()>);

pub(super) enum AsioOpenError {
    /// The driver asked for a reset and bindings on it are still closing. Not a failure of the
    /// driver: the rebind ladder's next attempt finds them gone.
    Resetting,
    Failed(String),
}

/// What the life cycle has done, counted: for the tests, which count work rather than time.
#[cfg(feature = "instrument")]
#[derive(Default)]
pub(super) struct AsioWork {
    pub(super) loads: AtomicUsize,
    pub(super) unloads: AtomicUsize,
    pub(super) rate_changes: AtomicUsize,
}

struct Loaded<D> {
    name: String,
    driver: Arc<D>,
    /// One strong count per lease handed out, plus this one.
    leases: Arc<()>,
    dsd_active: bool,
}

impl<D> Loaded<D> {
    /// One writer holds the driver, alone (its lease and the slot's own count), and in PCM: a
    /// DSD writer may take it over.
    fn held_alone(&self) -> bool {
        Arc::strong_count(&self.leases) == 2 && !self.dsd_active
    }
}

type Job = Box<dyn FnOnce() + Send>;

pub(super) struct AsioHost<A: AsioDriverApi> {
    api: Arc<A>,
    jobs: Mutex<Option<mpsc::Sender<Job>>>,
    loaded: Mutex<Option<Loaded<A::Driver>>>,
    reset_requested: AtomicBool,
    idle_wait: Duration,
    /// What each driver said when a DSD song asked it for native DSD, by name: a driver declares
    /// DSD only to a request for it.
    native_dsd: Mutex<Vec<(String, bool)>>,
    #[cfg(feature = "instrument")]
    pub(super) work: Arc<AsioWork>,
}

impl<A: AsioDriverApi> AsioHost<A> {
    pub(super) fn new(api: A, idle_wait: Duration) -> Self {
        Self {
            api: Arc::new(api),
            jobs: Mutex::new(None),
            loaded: Mutex::new(None),
            reset_requested: AtomicBool::new(false),
            idle_wait,
            native_dsd: Mutex::new(Vec::new()),
            #[cfg(feature = "instrument")]
            work: Arc::new(AsioWork::default()),
        }
    }

    fn loaded(&self) -> MutexGuard<'_, Option<Loaded<A::Driver>>> {
        self.loaded.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `job` on the driver thread and waits for it, starting the thread the first time.
    pub(super) fn run<R: Send + 'static>(
        &self,
        job: impl FnOnce(&A) -> R + Send + 'static,
    ) -> Result<R, String> {
        let mut jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
        let sender = match jobs.take() {
            Some(sender) => sender,
            None => {
                let (sender, receiver) = mpsc::channel::<Job>();
                let api = Arc::clone(&self.api);
                thread::Builder::new()
                    .name("player-core-asio".to_owned())
                    .spawn(move || {
                        api.enter_thread();
                        while let Ok(job) = receiver.recv() {
                            job();
                        }
                    })
                    .map_err(|error| format!("the ASIO thread could not start: {error}"))?;
                sender
            }
        };
        let sender = jobs.insert(sender);
        let (reply, answer) = mpsc::sync_channel(1);
        let api = Arc::clone(&self.api);
        sender
            .send(Box::new(move || {
                let _ = reply.send(job(&api));
            }))
            .map_err(|_| "the ASIO thread has stopped".to_owned())?;
        drop(jobs);
        answer
            .recv()
            .map_err(|_| "the ASIO thread dropped a job".to_owned())
    }

    /// The installed drivers' names, read on the driver thread.
    pub(super) fn names(&self) -> Vec<String> {
        self.run(|api| api.names()).unwrap_or_default()
    }

    /// Asks for a reset: the next open unloads the driver, once nothing holds it, and loads it
    /// afresh. Called from the driver's own message callback, so it only sets a flag.
    pub(super) fn request_reset(&self) {
        self.reset_requested.store(true, Ordering::Release);
    }

    /// Waits, up to the idle wait, for every lease on `loaded` to be dropped.
    fn wait_idle(&self, loaded: &Loaded<A::Driver>) -> bool {
        let deadline = Instant::now() + self.idle_wait;
        while Arc::strong_count(&loaded.leases) > 1 {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(2));
        }
        true
    }

    fn unload(&self, slot: &mut Option<Loaded<A::Driver>>) {
        if let Some(loaded) = slot.take() {
            #[cfg(feature = "instrument")]
            eprintln!(
                "player-core[audio] ASIO driver \"{}\" unloaded",
                loaded.name
            );
            let driver = loaded.driver;
            #[cfg(feature = "instrument")]
            let work = Arc::clone(&self.work);
            let _ = self.run(move |api| {
                #[cfg(feature = "instrument")]
                work.unloads.fetch_add(1, Ordering::Relaxed);
                match Arc::try_unwrap(driver) {
                    Ok(driver) => api.unload(driver),
                    // Something still holds it; it goes when that does.
                    Err(driver) => drop(driver),
                }
            });
        }
    }

    /// The driver `name` at the rate a source at `source_rate` wants, and a lease on it; `with`
    /// is handed the driver to build a device from. Loads the driver if it is not loaded; a
    /// different driver, or one that asked for a reset, is unloaded first once nothing holds it.
    /// The rate is changed only when no binding holds the driver: while the previous song's is
    /// still closing, the new one plays at the driver's current rate, resampled, as before.
    pub(super) fn open<R: Send + 'static>(
        &self,
        name: &str,
        source_rate: Option<u32>,
        with: impl FnOnce(&A::Driver) -> R + Send + 'static,
    ) -> Result<(R, AsioLease), AsioOpenError> {
        let mut slot = self.loaded();
        let stale = slot.as_ref().is_some_and(|loaded| loaded.name != name)
            || self.reset_requested.load(Ordering::Acquire);
        if stale {
            if !self.unload_when_idle(&mut slot) {
                return Err(AsioOpenError::Resetting);
            }
            self.reset_requested.store(false, Ordering::Release);
        }
        if slot.is_none() {
            let owned = name.to_owned();
            #[cfg(feature = "instrument")]
            let work = Arc::clone(&self.work);
            let driver = self
                .run(move |api| {
                    #[cfg(feature = "instrument")]
                    work.loads.fetch_add(1, Ordering::Relaxed);
                    api.load(&owned)
                })
                .and_then(|loaded| loaded)
                .map_err(|error| {
                    AsioOpenError::Failed(format!("driver \"{name}\" did not load: {error}"))
                })?;
            #[cfg(feature = "instrument")]
            eprintln!("player-core[audio] ASIO driver \"{name}\" loaded");
            *slot = Some(Loaded {
                name: name.to_owned(),
                driver: Arc::new(driver),
                leases: Arc::new(()),
                dsd_active: false,
            });
        }
        let Some(loaded) = slot.as_ref() else {
            return Err(AsioOpenError::Failed(format!(
                "driver \"{name}\" is not loaded"
            )));
        };
        if loaded.dsd_active {
            return Err(AsioOpenError::Resetting);
        }
        if let Some(source) = source_rate {
            let driver = Arc::clone(&loaded.driver);
            let current = self
                .run(move |api| api.rate(&driver))
                .ok()
                .flatten()
                .unwrap_or(0);
            let driver = Arc::clone(&loaded.driver);
            let target = self
                .run(move |api| {
                    choose_output_rate(source, current, |rate| api.can_rate(&driver, rate))
                })
                .unwrap_or(current);
            if target != current && target != 0 {
                if self.wait_idle(loaded) {
                    let driver = Arc::clone(&loaded.driver);
                    #[cfg(feature = "instrument")]
                    let work = Arc::clone(&self.work);
                    let changed = self.run(move |api| {
                        #[cfg(feature = "instrument")]
                        work.rate_changes.fetch_add(1, Ordering::Relaxed);
                        api.set_rate(&driver, target)
                    });
                    #[cfg(feature = "instrument")]
                    match changed {
                        Ok(Ok(())) => eprintln!("player-core[audio] ASIO rate {current} Hz -> {target} Hz (source {source} Hz)"),
                        Ok(Err(error)) | Err(error) => {
                            eprintln!("player-core[audio] ASIO rate stays {current} Hz: {target} Hz was refused: {error}")
                        }
                    }
                    #[cfg(not(feature = "instrument"))]
                    let _ = changed;
                } else {
                    #[cfg(feature = "instrument")]
                    eprintln!("player-core[audio] ASIO rate stays {current} Hz for a {source} Hz source: the previous song still holds the driver");
                }
            }
        }
        let driver = Arc::clone(&loaded.driver);
        let lease = AsioLease(Arc::clone(&loaded.leases));
        let built = self
            .run(move |_| with(&driver))
            .map_err(AsioOpenError::Failed)?;
        Ok((built, lease))
    }

    /// Opens the one ASIO driver in a DSD transport after prior writers have released their
    /// leases. `None` leaves the driver in PCM and lets the caller use its existing PCM route.
    pub(super) fn open_dsd(
        &self,
        name: &str,
        bit_rate: u32,
        channels: u16,
    ) -> Result<Option<(AsioDsdFormat, u32, AsioLease)>, AsioOpenError> {
        if bit_rate == 0 || channels == 0 || !bit_rate.is_multiple_of(8) {
            return Ok(None);
        }
        let ((), lease) = self.open(name, None, |_| ())?;
        // A song change opens the next song while the one before is still letting go of the
        // driver: its binding closes off this thread. DSD needs the driver alone, so wait for it,
        // without the slot held — a DSD writer's release takes the slot to restore PCM. Still
        // held after the idle wait, this song plays through the PCM conversion instead.
        let deadline = Instant::now() + self.idle_wait;
        while self
            .loaded()
            .as_ref()
            .is_some_and(|loaded| !loaded.held_alone())
        {
            if Instant::now() >= deadline {
                return Ok(None);
            }
            thread::sleep(Duration::from_millis(2));
        }
        let mut slot = self.loaded();
        // The open above loaded the driver; gone since, the song plays through PCM.
        let Some(loaded) = slot.as_mut() else {
            return Ok(None);
        };
        if !loaded.held_alone() {
            return Ok(None);
        }
        let driver = Arc::clone(&loaded.driver);
        let original_rate = self
            .run({
                let driver = Arc::clone(&driver);
                move |api| api.rate(&driver).unwrap_or(0)
            })
            .map_err(AsioOpenError::Failed)?;
        if original_rate == 0 {
            drop(lease);
            return Err(AsioOpenError::Failed(
                "ASIO reported a zero PCM rate".to_owned(),
            ));
        }
        let negotiated = self
            .run(move |api| api.open_dsd(&driver, bit_rate, channels))
            .and_then(|result| result);
        if let Ok(format) = &negotiated {
            self.record_native_dsd(
                name,
                format.is_some_and(|format| format.transport == AsioDsdTransport::Native),
            );
        }
        match negotiated {
            Ok(Some(format)) => {
                loaded.dsd_active = true;
                Ok(Some((format, original_rate, lease)))
            }
            other => {
                let driver = Arc::clone(&loaded.driver);
                self.run(move |api| api.restore_pcm(&driver, original_rate))
                    .and_then(|result| result)
                    .map_err(AsioOpenError::Failed)?;
                other.map(|_| None).map_err(AsioOpenError::Failed)
            }
        }
    }
    /// Whether the loaded driver is in DSD: the one record of it. A DSD writer's lease keeps its
    /// driver loaded, so for the writer holding it this is about its own driver.
    pub(super) fn in_dsd(&self) -> bool {
        self.loaded()
            .as_ref()
            .is_some_and(|loaded| loaded.dsd_active)
    }
    pub(super) fn resume_dsd(&self, bit_rate: u32, channels: u16) -> Result<AsioDsdFormat, String> {
        let mut slot = self.loaded();
        let loaded = slot
            .as_mut()
            .ok_or_else(|| "the ASIO driver is no longer loaded".to_owned())?;
        if !loaded.held_alone() {
            return Err("the ASIO driver is held by another writer".to_owned());
        }
        if self.reset_requested.load(Ordering::Acquire) {
            return Err("the ASIO driver requested a reset".to_owned());
        }
        let driver = Arc::clone(&loaded.driver);
        let original_rate = self
            .run(move |api| api.rate(&driver))?
            .filter(|rate| *rate != 0)
            .ok_or_else(|| "ASIO reported no PCM rate".to_owned())?;
        let driver = Arc::clone(&loaded.driver);
        match self
            .run(move |api| api.open_dsd(&driver, bit_rate, channels))
            .and_then(|result| result)
        {
            Ok(Some(format)) => {
                loaded.dsd_active = true;
                Ok(format)
            }
            other => {
                let driver = Arc::clone(&loaded.driver);
                self.run(move |api| api.restore_pcm(&driver, original_rate))??;
                Err(other
                    .err()
                    .unwrap_or_else(|| "ASIO refused the saved DSD transport".to_owned()))
            }
        }
    }
    /// Returns the driver to PCM after a DSD writer has stopped and disposed its buffers. The
    /// caller still owns its lease while this runs, which keeps another writer from taking the
    /// driver between teardown and the PCM keeper's replacement buffers.
    /// Nothing to do when the driver is already in PCM.
    pub(super) fn restore_pcm(&self, original_rate: u32) -> Result<(), String> {
        let mut slot = self.loaded();
        let loaded = slot
            .as_mut()
            .ok_or_else(|| "the ASIO driver is no longer loaded".to_owned())?;
        if !loaded.dsd_active {
            return Ok(());
        }
        let driver = Arc::clone(&loaded.driver);
        let result = self.run(move |api| api.restore_pcm(&driver, original_rate))?;
        if result.is_ok() {
            loaded.dsd_active = false;
        }
        result
    }
    /// Unloads the driver once no binding holds it, waiting up to the idle wait for the last to
    /// close. Called before WASAPI is opened with ASIO switched off, so the interface an ASIO
    /// driver claims is free. `false` when a binding still held it at the deadline.
    pub(super) fn release_when_idle(&self) -> bool {
        self.unload_when_idle(&mut self.loaded())
    }

    /// Unloads the driver once nothing holds it, waiting up to the idle wait; `false` (and loaded
    /// still) when a binding held it at the deadline.
    fn unload_when_idle(&self, slot: &mut Option<Loaded<A::Driver>>) -> bool {
        if slot.as_ref().is_some_and(|loaded| !self.wait_idle(loaded)) {
            return false;
        }
        self.unload(slot);
        true
    }

    fn record_native_dsd(&self, name: &str, native: bool) {
        let mut known = self
            .native_dsd
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match known.iter_mut().find(|(known, _)| known == name) {
            Some(entry) => entry.1 = native,
            None => known.push((name.to_owned(), native)),
        }
    }

    /// What the driver `name` can do, when it is the loaded one; `None` for any other, which
    /// would have to be loaded — and the loaded one unloaded — to say.
    pub(super) fn loaded_caps(&self, name: &str) -> Option<AudioDeviceCaps> {
        let driver = self
            .loaded()
            .as_ref()
            .filter(|loaded| loaded.name == name)
            .map(|loaded| Arc::clone(&loaded.driver))?;
        let (channels, rates) = self.run(move |api| api.caps(&driver)).ok().flatten()?;
        let dsd = self
            .native_dsd
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, native)| *native);
        Some(AudioDeviceCaps {
            channels,
            rates,
            dsd,
        })
    }

    #[cfg(test)]
    pub(super) fn is_loaded(&self) -> bool {
        self.loaded().is_some()
    }
}
