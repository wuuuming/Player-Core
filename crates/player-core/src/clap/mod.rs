//! CLAP effects hosted in process: the chain the host builds of them ([`Effects`]), the
//! stage that runs it on the thread that runs the DSP chain, and what a `.clap` file offers
//! ([`describe`]).
//!
//! Threads: one plugin host thread per process makes CLAP's main-thread calls (`host`); the
//! chain's thread (Windows' decode worker, Linux's Pulse writer; never the device callback) is
//! the audio thread. A plugin that errs, or answers with a NaN or an infinity, is taken out of
//! the chain for the rest of its life and says why ([`EffectInfo::failure`]); the sound goes on
//! without it. A plugin's crash is the process's (in process by design); loading files to see
//! what they are is for a child process ([`describe`]).
//!
//! Parameters (`clap.params`, [`Effects::params`] / [`Effects::set_param`]): a value the host sets
//! goes to the plugin as an event with its next block while it plays, through `params.flush` on
//! the host thread otherwise; what the plugin changes itself (its window, output events,
//! `rescan`) moves [`Effects::changes`], and a watcher ([`Effects::watch`]) is told.

mod frame;
mod host;
mod instance;
mod library;
mod stage;

pub use library::ClapDescription;
// What the separately maintained tests reach (`crate::internals`): not API.
#[cfg(feature = "test-support")]
pub use library::Library;
#[cfg(not(feature = "test-support"))]
pub(crate) use stage::EffectsStage;
#[cfg(feature = "test-support")]
pub use stage::EffectsStage;

use instance::Instance;
#[cfg(not(feature = "test-support"))]
use library::Library;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

/// The rate effects are activated at before any sound has asked for one (Linux's always).
const FIRST_RATE: u32 = 48_000;

/// One effect in a chain, for as long as it is there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EffectKey(u64);

/// An effect as the chain has it now.
#[derive(Clone, Debug)]
pub struct EffectInfo {
    pub key: EffectKey,
    pub path: PathBuf,
    pub plugin: ClapDescription,
    pub bypass: bool,
    /// Why the effect was taken out of the chain, if it was.
    pub failure: Option<String>,
    /// Frames its output lags its input.
    pub latency: usize,
    pub has_window: bool,
    /// It has parameters ([`Effects::params`]).
    pub has_params: bool,
}

/// One of an effect's parameters (`clap.params`) as it stands. Hidden ones and the plugin's own
/// bypass (the chain has one) are not listed.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectParam {
    pub id: u32,
    pub name: String,
    /// The group it is in, `/`-separated; empty for none.
    pub module: String,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    /// Its value: the one last set ([`Effects::set_param`]) until the plugin has taken it.
    pub value: f64,
    /// The value as the plugin prints it.
    pub text: String,
    /// Only whole values between `min` and `max`.
    pub stepped: bool,
    /// A stepped one of at most [`MAX_CHOICES`] values: each one's text, from `min` up.
    pub choices: Vec<String>,
    /// The plugin sets it; the host only shows it.
    pub read_only: bool,
}

/// The most values a stepped parameter has for its texts to be listed ([`EffectParam::choices`]).
pub const MAX_CHOICES: usize = 32;

/// The chain's change count, and who is told when it moves ([`Effects::watch`]).
#[derive(Default)]
struct Changes {
    count: AtomicU64,
    watcher: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

impl Changes {
    /// Moves the count and tells the watcher, on whichever thread the change happened.
    fn bump(&self) {
        self.count.fetch_add(1, Ordering::Release);
        if let Some(watcher) = self.watcher.get() {
            watcher();
        }
    }
}

struct Slot {
    key: EffectKey,
    instance: Arc<Instance>,
}

struct Shared {
    slots: Mutex<Vec<Slot>>,
    /// Bumped when an effect is added, removed or moved; a stage takes the new chain at its next
    /// reset.
    structure: AtomicU64,
    /// Bumped on every change a host shows or keeps: the structure, a bypass, a failure, a
    /// plugin's state marked changed, a parameter set by the host or by the plugin.
    changes: Arc<Changes>,
    rate: AtomicU32,
    next_key: AtomicU64,
    /// Effects asked for with [`Effects::add_then`] and not made yet.
    adding: AtomicUsize,
}

/// A chain of CLAP effects, shared by the host and every session it opens (cheap to clone).
/// Changes to its order take effect at the sound's next reset: a seek, or the next song.
#[derive(Clone)]
pub struct Effects {
    shared: Arc<Shared>,
}

impl Default for Effects {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Effects {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.list()).finish()
    }
}

impl PartialEq for Effects {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

impl Effects {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                slots: Mutex::new(Vec::new()),
                structure: AtomicU64::new(0),
                changes: Arc::new(Changes::default()),
                rate: AtomicU32::new(FIRST_RATE),
                next_key: AtomicU64::new(1),
                adding: AtomicUsize::new(0),
            }),
        }
    }

    fn slots(&self) -> std::sync::MutexGuard<'_, Vec<Slot>> {
        self.shared
            .slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn changed(&self, structure: bool) {
        if structure {
            self.shared.structure.fetch_add(1, Ordering::Release);
        }
        self.shared.changes.bump();
    }

    /// Calls `watcher` whenever [`Effects::changes`] moves, on the thread the change happened on
    /// (the host's, the plugin host thread, the chain's): it should only wake its own. One
    /// watcher per chain; false when there is one already.
    pub fn watch(&self, watcher: impl Fn() + Send + Sync + 'static) -> bool {
        self.shared.changes.watcher.set(Box::new(watcher)).is_ok()
    }

    /// Adds `plugin_id` from the file at `path` to the end of the chain, `state` loaded into it
    /// (what [`Effects::state`] gave). Blocks while the host thread loads it.
    pub fn add(
        &self,
        path: &Path,
        plugin_id: &str,
        state: Option<Vec<u8>>,
    ) -> Result<EffectKey, String> {
        let instance = host::run(self.maker(path, plugin_id, state))??;
        Ok(self.push(instance))
    }

    /// [`Effects::add`] without waiting: the effect is made on the plugin host thread and joins
    /// the end of the chain there; `done` gets its key, or why not, on that thread. Adds are
    /// carried out in the order they are asked for. A stage made while one is still to be made
    /// waits for it ([`Effects::stage`]), so a song opened while a kept chain loads plays
    /// through all of it.
    pub fn add_then(
        &self,
        path: &Path,
        plugin_id: &str,
        state: Option<Vec<u8>>,
        done: impl FnOnce(Result<EffectKey, String>) + Send + 'static,
    ) {
        if let Err(why) = host::ready() {
            return done(Err(why));
        }
        let make = self.maker(path, plugin_id, state);
        let me = self.clone();
        self.shared.adding.fetch_add(1, Ordering::AcqRel);
        host::post(move || {
            let made = make().map(|instance| me.push(instance));
            me.shared.adding.fetch_sub(1, Ordering::AcqRel);
            done(made);
        });
    }

    /// What makes the effect, run on the host thread: the library opened, the plugin made with
    /// `state`, activated at the chain's rate.
    fn maker(
        &self,
        path: &Path,
        plugin_id: &str,
        state: Option<Vec<u8>>,
    ) -> impl FnOnce() -> Result<Arc<Instance>, String> + Send + 'static {
        let path = path.to_path_buf();
        let plugin_id = plugin_id.to_owned();
        let changes = self.shared.changes.clone();
        let rate = self.shared.rate.load(Ordering::Acquire);
        move || {
            let library = Library::open(&path)?;
            let desc = library
                .descriptions()
                .into_iter()
                .find(|desc| desc.id == plugin_id)
                .ok_or_else(|| format!("{} has no plugin {plugin_id}", path.display()))?;
            let instance = Instance::create(library, desc, state.as_deref(), changes)?;
            instance.activate(rate)?;
            Ok(instance)
        }
    }

    /// A made effect joins the end of the chain.
    fn push(&self, instance: Arc<Instance>) -> EffectKey {
        let key = EffectKey(self.shared.next_key.fetch_add(1, Ordering::Relaxed));
        self.slots().push(Slot { key, instance });
        self.changed(true);
        key
    }

    /// Takes an effect out of the chain; false when it is not there.
    pub fn remove(&self, key: EffectKey) -> bool {
        let removed = {
            let mut slots = self.slots();
            let index = slots.iter().position(|slot| slot.key == key);
            index.map(|index| slots.remove(index))
        };
        let found = removed.is_some();
        if found {
            self.changed(true);
        }
        found
    }

    /// Moves an effect to `index` (clamped to the chain).
    pub fn move_to(&self, key: EffectKey, index: usize) {
        let moved = {
            let mut slots = self.slots();
            match slots.iter().position(|slot| slot.key == key) {
                Some(from) => {
                    let slot = slots.remove(from);
                    let to = index.min(slots.len());
                    slots.insert(to, slot);
                    from != to
                }
                None => false,
            }
        };
        if moved {
            self.changed(true);
        }
    }

    /// Bypasses an effect, or puts it back; at once, its latency kept.
    pub fn set_bypass(&self, key: EffectKey, bypass: bool) {
        let changed = self
            .slots()
            .iter()
            .find(|slot| slot.key == key)
            .is_some_and(|slot| slot.instance.bypass.swap(bypass, Ordering::Relaxed) != bypass);
        if changed {
            self.changed(false);
        }
    }

    pub fn list(&self) -> Vec<EffectInfo> {
        self.slots()
            .iter()
            .map(|slot| EffectInfo {
                key: slot.key,
                path: slot.instance.path().to_path_buf(),
                plugin: slot.instance.desc.clone(),
                bypass: slot.instance.bypass.load(Ordering::Relaxed),
                failure: slot.instance.failure(),
                latency: slot.instance.latency(),
                has_window: slot.instance.has_window(),
                has_params: slot.instance.has_params(),
            })
            .collect()
    }

    /// A count that moves whenever something [`Effects::list`] or [`Effects::state`] answers may
    /// have changed: compare it to know when to show or save again.
    pub fn changes(&self) -> u64 {
        self.shared.changes.count.load(Ordering::Acquire)
    }

    /// The effect's parameters as they stand, in the plugin's order. Blocks while the host thread
    /// reads them.
    pub fn params(&self, key: EffectKey) -> Result<Vec<EffectParam>, String> {
        self.on_host(key, |instance| instance.params())
    }

    /// [`Effects::params`] without waiting: `done` gets them, on the plugin host thread.
    pub fn params_then(
        &self,
        key: EffectKey,
        done: impl FnOnce(Result<Vec<EffectParam>, String>) + Send + 'static,
    ) {
        match self.instance(key) {
            Ok(instance) => host::post(move || done(Ok(instance.params()))),
            Err(why) => done(Err(why)),
        }
    }

    /// Sets a parameter of the effect (`value` within its range; a stepped one whole), without
    /// waiting: the plugin takes it with its next block while the sound plays, at once
    /// otherwise. Its window shows it; [`Effects::changes`] moves.
    pub fn set_param(&self, key: EffectKey, id: u32, value: f64) -> Result<(), String> {
        self.instance(key)?.set_param(id, value);
        Ok(())
    }

    /// Runs `f` with the effect on the plugin host thread and waits for its answer.
    fn on_host<R: Send + 'static>(
        &self,
        key: EffectKey,
        f: impl FnOnce(&Instance) -> R + Send + 'static,
    ) -> Result<R, String> {
        let instance = self.instance(key)?;
        host::run(move || f(&instance))
    }

    fn instance(&self, key: EffectKey) -> Result<Arc<Instance>, String> {
        self.slots()
            .iter()
            .find(|slot| slot.key == key)
            .map(|slot| slot.instance.clone())
            .ok_or_else(|| "no such effect".to_string())
    }

    /// The effect's state (`clap.state`), to hand back to [`Effects::add`] another time.
    pub fn state(&self, key: EffectKey) -> Result<Vec<u8>, String> {
        self.on_host(key, |instance| instance.save_state())?
    }

    /// Opens the effect's own window, floating above `parent` (an HWND on Windows, an X11
    /// window id on Linux; 0 for none).
    pub fn open_window(&self, key: EffectKey, parent: isize) -> Result<(), String> {
        self.on_host(key, move |instance| instance.show_window(parent))?
    }

    /// Opens the effect's window without waiting for it (a plugin may take most of a second to
    /// build it): `done` gets the outcome, on the plugin host thread.
    pub fn open_window_then(
        &self,
        key: EffectKey,
        parent: isize,
        done: impl FnOnce(Result<(), String>) + Send + 'static,
    ) {
        match self.instance(key) {
            Ok(instance) => host::post(move || done(instance.show_window(parent))),
            Err(why) => done(Err(why)),
        }
    }

    /// Builds every effect's window, hidden, in the background, so that opening one later is
    /// at once (above `parent`, as in [`Effects::open_window`]). A window that cannot be built
    /// says why when it is opened.
    pub fn prepare_windows(&self, parent: isize) {
        for instance in self.snapshot().1 {
            if instance.has_window() {
                host::post(move || {
                    let _ = instance.prepare_window(parent);
                });
            }
        }
    }

    fn structure(&self) -> u64 {
        self.shared.structure.load(Ordering::Acquire)
    }

    /// The structure count and the effects, in order.
    fn snapshot(&self) -> (u64, Vec<Arc<Instance>>) {
        let slots = self.slots();
        (
            self.structure(),
            slots.iter().map(|slot| slot.instance.clone()).collect(),
        )
    }

    /// The stage that runs this chain at `rate` on `channels`: every effect activated at that
    /// rate first (blocking while the host thread does it).
    pub fn stage(&self, rate: NonZeroU32, channels: usize) -> EffectsStage {
        // Effects still being made join first: the host thread takes jobs in order, so once it
        // has answered this one, they are in the chain.
        if self.shared.adding.load(Ordering::Acquire) > 0 {
            let _ = host::run(|| ());
        }
        let rate = rate.get();
        if self.shared.rate.swap(rate, Ordering::AcqRel) != rate {
            let (_, instances) = self.snapshot();
            // An empty chain has nothing new to show or keep.
            if !instances.is_empty() {
                let _ = host::run(move || {
                    for instance in instances {
                        let _ = instance.activate(rate);
                    }
                });
                self.changed(false);
            }
        }
        EffectsStage::new(self.clone(), rate, channels)
    }
}

/// What the `.clap` file at `path` offers. Loads the file into this process: a plugin that
/// crashes as it loads takes the caller with it, so a host calls this in a child process.
pub fn describe(path: &Path) -> Result<Vec<ClapDescription>, String> {
    Ok(Library::open(path)?.descriptions())
}

/// Where plugins are looked for, in order: `CLAP_PATH`, then the platform's folders.
pub fn search_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::env::var_os("CLAP_PATH")
        .map(|value| std::env::split_paths(&value).collect())
        .unwrap_or_default();
    #[cfg(windows)]
    {
        if let Some(common) = std::env::var_os("COMMONPROGRAMFILES") {
            paths.push(PathBuf::from(common).join("CLAP"));
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            paths.push(PathBuf::from(local).join("Programs/Common/CLAP"));
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            paths.push(PathBuf::from(home).join(".clap"));
        }
        paths.push(PathBuf::from("/usr/lib/clap"));
    }
    paths
}

/// Every `.clap` file under the search paths, each once, in order.
pub fn find_plugins() -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        entries.sort();
        for path in entries {
            if path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("clap"))
                && path.is_file()
            {
                if !found.contains(&path) {
                    found.push(path);
                }
            } else if path.is_dir() && depth < 8 {
                walk(&path, depth + 1, found);
            }
        }
    }
    let mut found = Vec::new();
    for dir in search_paths() {
        walk(&dir, 0, &mut found);
    }
    found
}
