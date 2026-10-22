//! A `.clap` file: loaded once per path while any instance or description needs it, its entry
//! initialised once, and deinitialised and unloaded when the last user lets go.

use clap_sys::entry::clap_plugin_entry;
use clap_sys::factory::plugin_factory::{clap_plugin_factory, CLAP_PLUGIN_FACTORY_ID};
use clap_sys::plugin::clap_plugin_descriptor;
use clap_sys::version::clap_version_is_compatible;
use std::collections::HashMap;
use std::ffi::{c_char, CString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};

/// What a plugin says it is, read from its factory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClapDescription {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub description: String,
    pub features: Vec<String>,
}

impl ClapDescription {
    /// An effect: what the chain hosts (instruments and analyzers are not).
    pub fn is_effect(&self) -> bool {
        self.features
            .iter()
            .any(|feature| feature == "audio-effect")
    }
}

pub struct Library {
    path: PathBuf,
    /// Kept loaded while anything of the library is in use; unloaded after `deinit`.
    _module: os::Module,
    entry: *const clap_plugin_entry,
    pub(super) factory: *const clap_plugin_factory,
}

// SAFETY: the entry and factory are process-wide tables a plugin must let any thread read (CLAP
// `entry.h`: the factory's functions are thread-safe); the module handle is only freed in `Drop`.
unsafe impl Send for Library {}
// SAFETY: as for `Send`; nothing here is mutated after `open`.
unsafe impl Sync for Library {}

static OPEN: Mutex<Option<HashMap<PathBuf, Weak<Library>>>> = Mutex::new(None);

impl Library {
    /// A symbol the library exports (the test effect's hooks).
    #[cfg(feature = "test-support")]
    pub fn symbol(&self, name: &std::ffi::CStr) -> Result<*const std::ffi::c_void, String> {
        self._module.symbol(name)
    }

    /// The library at `path`, loading it unless it is already loaded.
    pub fn open(path: &Path) -> Result<Arc<Library>, String> {
        let mut open = OPEN.lock().unwrap_or_else(PoisonError::into_inner);
        let open = open.get_or_insert_with(HashMap::new);
        if let Some(library) = open.get(path).and_then(Weak::upgrade) {
            return Ok(library);
        }
        let library = Arc::new(Self::load(path)?);
        open.retain(|_, library| library.strong_count() > 0);
        open.insert(path.to_path_buf(), Arc::downgrade(&library));
        Ok(library)
    }

    fn load(path: &Path) -> Result<Library, String> {
        let module = os::Module::load(path)?;
        let entry = module.symbol(c"clap_entry")?.cast::<clap_plugin_entry>();
        // SAFETY: `clap_entry` is the exported `clap_plugin_entry` every CLAP library has; it
        // stays mapped while `module` is loaded.
        let table = unsafe { &*entry };
        if !clap_version_is_compatible(table.clap_version) {
            return Err(format!(
                "CLAP {}.{} is not supported",
                table.clap_version.major, table.clap_version.minor
            ));
        }
        let (Some(init), Some(get_factory)) = (table.init, table.get_factory) else {
            return Err("the entry has no init or factory".into());
        };
        let c_path = CString::new(path.to_string_lossy().as_bytes())
            .map_err(|_| "the path has a NUL".to_string())?;
        // SAFETY: the entry's own function, called once per load, with a null-terminated path.
        if !unsafe { init(c_path.as_ptr()) } {
            return Err("the plugin's entry refused to start".into());
        }
        let mut library = Library {
            path: path.to_path_buf(),
            _module: module,
            entry,
            factory: std::ptr::null(),
        };
        // SAFETY: the entry is initialised; the id is null-terminated.
        let factory = unsafe { get_factory(CLAP_PLUGIN_FACTORY_ID.as_ptr()) };
        if factory.is_null() {
            return Err("the library has no plugin factory".into());
        }
        library.factory = factory.cast();
        Ok(library)
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// Every plugin the factory offers.
    pub(super) fn descriptions(&self) -> Vec<ClapDescription> {
        // SAFETY: the factory outlives `self`; its functions are thread-safe.
        let factory = unsafe { &*self.factory };
        let (Some(count), Some(get)) = (factory.get_plugin_count, factory.get_plugin_descriptor)
        else {
            return Vec::new();
        };
        // SAFETY: the factory's own function on itself.
        let count = unsafe { count(self.factory) };
        (0..count)
            .filter_map(|index| {
                // SAFETY: an index below the count; the descriptor lives as long as the library.
                let desc = unsafe { get(self.factory, index) };
                // SAFETY: a non-null descriptor from the factory.
                (!desc.is_null()).then(|| unsafe { describe(&*desc) })
            })
            .filter(|desc| !desc.id.is_empty())
            .collect()
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: the entry was initialised in `load` and every instance of it is destroyed (each
        // holds an `Arc` of this library).
        if let Some(deinit) = unsafe { (*self.entry).deinit } {
            // SAFETY: the entry's own function, once, after its last use.
            unsafe { deinit() };
        }
    }
}

/// A C string from a plugin, or empty for null.
///
/// # Safety
/// `text` is null or a null-terminated string alive for the call.
pub(super) unsafe fn text(text: *const c_char) -> String {
    // SAFETY: as the caller promises.
    unsafe { crate::c_text(text) }.unwrap_or_default()
}

/// # Safety
/// `desc` is a descriptor from a plugin factory, with its strings alive.
unsafe fn describe(desc: &clap_plugin_descriptor) -> ClapDescription {
    let mut features = Vec::new();
    if !desc.features.is_null() {
        for index in 0.. {
            // SAFETY: a null-terminated array of strings (CLAP `plugin.h`).
            let feature = unsafe { *desc.features.add(index) };
            if feature.is_null() {
                break;
            }
            // SAFETY: an entry of that array.
            features.push(unsafe { text(feature) });
        }
    }
    // SAFETY: the descriptor's strings, as the caller promises.
    unsafe {
        ClapDescription {
            id: text(desc.id),
            name: text(desc.name),
            vendor: text(desc.vendor),
            version: text(desc.version),
            description: text(desc.description),
            features,
        }
    }
}

#[cfg(windows)]
mod os {
    use std::ffi::{c_void, CStr};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows::core::{PCSTR, PCWSTR};
    use windows::Win32::Foundation::{FreeLibrary, HMODULE};
    use windows::Win32::System::LibraryLoader::{
        GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
    };

    pub(super) struct Module(HMODULE);

    impl Module {
        pub(super) fn load(path: &Path) -> Result<Module, String> {
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
            // SAFETY: a null-terminated wide path; the plugin's own folder is searched for its
            // dependencies.
            unsafe {
                LoadLibraryExW(
                    PCWSTR(wide.as_ptr()),
                    None,
                    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
                )
            }
            .map(Module)
            .map_err(|error| format!("cannot load: {}", error.message()))
        }

        pub(super) fn symbol(&self, name: &CStr) -> Result<*const c_void, String> {
            // SAFETY: a loaded module and a null-terminated name.
            unsafe { GetProcAddress(self.0, PCSTR(name.as_ptr().cast())) }
                .map(|function| function as *const c_void)
                .ok_or_else(|| format!("no {} in the library", name.to_string_lossy()))
        }
    }

    impl Drop for Module {
        fn drop(&mut self) {
            // SAFETY: loaded by `load`, freed once.
            let _ = unsafe { FreeLibrary(self.0) };
        }
    }
}

#[cfg(target_os = "linux")]
mod os {
    use std::ffi::{c_void, CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    pub(super) struct Module(*mut c_void);

    fn last_error() -> String {
        // SAFETY: `dlerror` returns null or a string valid until the next dl call on this thread.
        let error = unsafe { libc::dlerror() };
        // SAFETY: as above.
        unsafe { crate::c_text(error) }.unwrap_or_else(|| "unknown error".into())
    }

    impl Module {
        pub(super) fn load(path: &Path) -> Result<Module, String> {
            let c_path =
                CString::new(path.as_os_str().as_bytes()).map_err(|_| "the path has a NUL")?;
            // SAFETY: a null-terminated path; local symbols so plugins do not see each other's.
            let handle =
                unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if handle.is_null() {
                Err(format!("cannot load: {}", last_error()))
            } else {
                Ok(Module(handle))
            }
        }

        pub(super) fn symbol(&self, name: &CStr) -> Result<*const c_void, String> {
            // SAFETY: a loaded module and a null-terminated name.
            let symbol = unsafe { libc::dlsym(self.0, name.as_ptr()) };
            if symbol.is_null() {
                Err(format!("no {} in the library", name.to_string_lossy()))
            } else {
                Ok(symbol.cast_const())
            }
        }
    }

    impl Drop for Module {
        fn drop(&mut self) {
            // SAFETY: opened by `load`, closed once.
            unsafe { libc::dlclose(self.0) };
        }
    }
}
