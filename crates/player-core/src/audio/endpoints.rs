//! WASAPI's render endpoints as Windows lists them — their ids, which cpal 0.15 does not expose,
//! their names and mix formats — the cpal device an endpoint id names, and the watch that moves
//! outputs when Windows' default output changes or a chosen endpoint comes back.

use super::device::AudioDeviceSelection;
use super::mix::ChannelLayout;
use cpal::traits::DeviceTrait;
use cpal::traits::HostTrait;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use windows::core::{implement, PCWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eAll, eConsole, eRender, EDataFlow, ERole, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator, DEVICE_STATE,
    DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

/// One active render endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub(crate) id: String,
    pub(crate) name: String,
    /// Its shared-mode mix format — channels and rate — when it answers for one.
    pub(crate) format: Option<(u16, u32)>,
}

/// COM on this thread, in the apartment cpal asks for on the same threads, for as long as the
/// thread lives. A thread another library put in the other apartment is used as it is.
struct ComOnThread(bool);

impl Drop for ComOnThread {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances this thread's own successful CoInitializeEx.
            unsafe { CoUninitialize() };
        }
    }
}

thread_local! {
    static COM: ComOnThread = {
        // SAFETY: a plain COM call on this thread.
        // RPC_E_CHANGED_MODE is a failure that leaves COM usable in the other apartment.
        ComOnThread(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok())
    };
}

fn enumerator() -> Result<IMMDeviceEnumerator, String> {
    COM.with(|_| {});
    // SAFETY: COM is initialised on this thread above.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .map_err(|error| error.to_string())
}

fn id_of(device: &IMMDevice) -> Option<String> {
    // SAFETY: GetId hands over a CoTaskMem string, freed here once copied.
    unsafe {
        let id = device.GetId().ok()?;
        let text = id.to_string().ok();
        CoTaskMemFree(Some(id.0 as *const _));
        text
    }
}

fn name_of(device: &IMMDevice) -> Option<String> {
    // SAFETY: a read of the endpoint's own property store.
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        Some(store.GetValue(&PKEY_Device_FriendlyName).ok()?.to_string())
    }
}

/// The shared-mode mix format, read from a client that is activated and never initialised: it
/// opens no stream.
fn mix_format(device: &IMMDevice) -> Option<(u16, u32)> {
    // SAFETY: `with_mix_format` hands a live, non-null format.
    with_mix_format(device, |format| unsafe {
        ((*format).nChannels, (*format).nSamplesPerSec)
    })
}

/// Reads an endpoint's shared-mode mix format through `read`, from a client that is activated
/// and never initialised (it opens no stream); the format is freed once read. `None` when there
/// is none. `read` gets the block as GetMixFormat gave it, live and not null for the call.
fn with_mix_format<R>(
    device: &IMMDevice,
    read: impl FnOnce(*const windows::Win32::Media::Audio::WAVEFORMATEX) -> R,
) -> Option<R> {
    // SAFETY: COM calls on the caller's device; GetMixFormat hands over a CoTaskMem block, read
    // only when not null and freed here once read.
    unsafe {
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ok()?;
        let format = client.GetMixFormat().ok()?;
        let out = (!format.is_null()).then(|| read(format));
        CoTaskMemFree(Some(format as *const _));
        out
    }
}

/// Reads speaker semantics without opening a stream, from the endpoint the binding resolved to:
/// the named device, or `endpoint` (`ResolvedOutput::endpoint`), or the default when it has none.
/// A virtual eight-channel mix format over physical stereo stays stereo, avoiding a second
/// surround processor.
pub(super) fn output_layout(
    selection: &AudioDeviceSelection,
    endpoint: Option<&str>,
    channels: u16,
) -> Result<ChannelLayout, String> {
    use windows::Win32::Media::Audio::{PKEY_AudioEndpoint_PhysicalSpeakers, WAVEFORMATEXTENSIBLE};
    let fallback = ChannelLayout::from_channels(channels)?;
    if channels < 4 {
        return Ok(fallback);
    }
    let read = || -> Option<ChannelLayout> {
        let enumerator = enumerator().ok()?;
        // SAFETY: COM calls on an enumerator this thread made; `wide` is NUL-terminated and
        // outlives `GetDevice`.
        let device = unsafe {
            if let AudioDeviceSelection::Named(name) = selection {
                let list = enumerator
                    .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
                    .ok()?;
                (0..list.GetCount().ok()?)
                    .filter_map(|i| list.Item(i).ok())
                    .find(|d| name_of(d).as_ref() == Some(name))?
            } else if let Some(id) = endpoint {
                let wide: Vec<_> = id.encode_utf16().chain(Some(0)).collect();
                enumerator
                    .GetDevice(PCWSTR(wide.as_ptr()))
                    .or_else(|_| enumerator.GetDefaultAudioEndpoint(eRender, eConsole))
                    .ok()?
            } else {
                enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?
            }
        };
        // SAFETY: COM calls on the device just found; the mix format is read only when not null,
        // as WAVEFORMATEXTENSIBLE only when its tag and size say it is one, and freed once.
        unsafe {
            let mask = with_mix_format(&device, |format| {
                if (*format).wFormatTag == 0xfffe && (*format).cbSize >= 22 {
                    // The tag and size say the block is a WAVEFORMATEXTENSIBLE.
                    (*format.cast::<WAVEFORMATEXTENSIBLE>()).dwChannelMask as u64
                } else {
                    0
                }
            })?;
            let physical = device
                .OpenPropertyStore(STGM_READ)
                .ok()
                .and_then(|store| store.GetValue(&PKEY_AudioEndpoint_PhysicalSpeakers).ok())
                .and_then(|value| u32::try_from(&value).ok())
                .unwrap_or(0) as u64;
            layout_from_masks(channels, mask, physical).ok()
        }
    };
    Ok(read().unwrap_or(fallback))
}

pub(super) fn layout_from_masks(
    channels: u16,
    mask: u64,
    physical: u64,
) -> Result<ChannelLayout, String> {
    let fallback = ChannelLayout::from_channels(channels)?;
    if channels > 2 && physical == super::mix::STEREO_MASK {
        return ChannelLayout::new(physical, 2, false);
    }
    Ok(ChannelLayout::new(mask, channels, false).unwrap_or(fallback))
}

/// The active render endpoints, in Windows' order.
pub(crate) fn render_endpoints() -> Result<Vec<Endpoint>, String> {
    let enumerator = enumerator()?;
    // SAFETY: plain enumeration calls on a live enumerator.
    unsafe {
        let collection = enumerator
            .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
            .map_err(|error| error.to_string())?;
        let count = collection.GetCount().map_err(|error| error.to_string())?;
        Ok((0..count)
            .filter_map(|index| collection.Item(index).ok())
            .filter_map(|device| {
                Some(Endpoint {
                    id: id_of(&device)?,
                    name: name_of(&device).unwrap_or_default(),
                    format: mix_format(&device),
                })
            })
            .collect())
    }
}

/// Windows' default output: the console role's, which is what cpal opens as the default.
pub(crate) fn default_render_id() -> Option<String> {
    let enumerator = enumerator().ok()?;
    // SAFETY: a plain query on a live enumerator.
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }.ok()?;
    id_of(&device)
}

/// The cpal device of the active endpoint `id`; `None` when no active endpoint has that id.
///
/// cpal lists every active endpoint of either direction in the enumerator's own order and names
/// none by id. The same list is read here with ids, and the two are paired by position; the
/// names must agree pairwise, or the list changed between the two reads and is read again.
pub(crate) fn cpal_device_for(id: &str) -> Result<Option<cpal::Device>, String> {
    for _ in 0..3 {
        let enumerator = enumerator()?;
        // SAFETY: plain enumeration calls on a live enumerator.
        let listed: Vec<(Option<String>, Option<String>)> = unsafe {
            let collection = enumerator
                .EnumAudioEndpoints(eAll, DEVICE_STATE_ACTIVE)
                .map_err(|error| error.to_string())?;
            let count = collection.GetCount().map_err(|error| error.to_string())?;
            (0..count)
                .map(|index| {
                    collection
                        .Item(index)
                        .ok()
                        .map_or((None, None), |device| (id_of(&device), name_of(&device)))
                })
                .collect()
        };
        let devices: Vec<cpal::Device> = cpal::default_host()
            .devices()
            .map_err(|error| error.to_string())?
            .collect();
        let agree = listed.len() == devices.len()
            && listed
                .iter()
                .zip(&devices)
                .all(|((_, name), device)| device.name().ok() == *name);
        if agree {
            return Ok(listed
                .iter()
                .position(|(listed, _)| listed.as_deref() == Some(id))
                .map(|index| devices[index].clone()));
        }
    }
    Err("the audio endpoint list kept changing while it was read".to_owned())
}

/// The endpoint watch while it runs: its thread, which holds the registration, and the word
/// that ends it.
struct Watching {
    stop: mpsc::Sender<()>,
    thread: JoinHandle<()>,
}

static WATCH: std::sync::Mutex<Option<Watching>> = std::sync::Mutex::new(None);

/// Starts the endpoint watch, when it is not running: on the first WASAPI bind, and again after
/// [`stop_endpoint_watch`]. The registration lives on a thread of its own, in the multithreaded
/// apartment, so no caller's apartment is changed for it; it costs nothing until a device
/// changes.
pub(crate) fn watch() {
    let mut watching = WATCH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if watching.is_some() {
        return;
    }
    let (stop, stopped) = mpsc::channel::<()>();
    let (registered, answer) = mpsc::sync_channel(1);
    let spawned = thread::Builder::new()
        .name("player-core-audio-endpoints".to_owned())
        .spawn(move || {
            // SAFETY: COM for this thread, undone on its way out; the enumerator and the client are
            // made, registered, unregistered and released here, the client's last reference after
            // its unregistration, as `RegisterEndpointNotificationCallback` asks.
            unsafe {
                let com = CoInitializeEx(None, COINIT_MULTITHREADED);
                let registration = CoCreateInstance::<_, IMMDeviceEnumerator>(
                    &MMDeviceEnumerator,
                    None,
                    CLSCTX_ALL,
                )
                .ok()
                .and_then(|enumerator| {
                    let client: IMMNotificationClient = EndpointWatch.into();
                    enumerator
                        .RegisterEndpointNotificationCallback(&client)
                        .ok()?;
                    Some((enumerator, client))
                });
                let _ = registered.send(registration.is_some());
                // Until the word, or until nobody can say it.
                let _ = stopped.recv();
                if let Some((enumerator, client)) = registration {
                    let _ = enumerator.UnregisterEndpointNotificationCallback(&client);
                    drop(client);
                    drop(enumerator);
                }
                if com.is_ok() {
                    CoUninitialize();
                }
            }
        });
    if let Ok(thread) = spawned {
        let _ = answer.recv();
        *watching = Some(Watching { stop, thread });
    }
}

/// Ends the endpoint watch: unregistered and released on its own thread, and its COM undone,
/// before this returns. For a host shutting down; a later WASAPI bind starts it again.
pub fn stop_endpoint_watch() {
    let watching = WATCH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(Watching { stop, thread }) = watching {
        let _ = stop.send(());
        let _ = thread.join();
    }
}

/// Whether the endpoint watch runs.
#[cfg(test)]
pub(crate) fn watching() -> bool {
    WATCH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some()
}

/// What Windows says of its endpoints, turned into moves of the outputs. Called on Windows' own
/// threads; each answer is a lock and an atomic add.
#[implement(IMMNotificationClient)]
pub(super) struct EndpointWatch;

fn text(id: &PCWSTR) -> Option<String> {
    // SAFETY: Windows passes a live NUL-terminated string for the call's duration.
    (!id.is_null())
        .then(|| unsafe { id.to_string() }.ok())
        .flatten()
}

impl IMMNotificationClient_Impl for EndpointWatch_Impl {
    fn OnDeviceStateChanged(&self, id: &PCWSTR, state: DEVICE_STATE) -> windows::core::Result<()> {
        if let Some(id) = text(id) {
            if state == DEVICE_STATE_ACTIVE {
                super::device::endpoint_arrived(&id);
            } else {
                // Disabled, unplugged, not present: gone as far as a stream on it is concerned.
                super::device::endpoint_left(&id);
            }
        }
        Ok(())
    }

    fn OnDeviceAdded(&self, id: &PCWSTR) -> windows::core::Result<()> {
        if let Some(id) = text(id) {
            super::device::endpoint_arrived(&id);
        }
        Ok(())
    }

    fn OnDeviceRemoved(&self, id: &PCWSTR) -> windows::core::Result<()> {
        // cpal 0.15's WASAPI thread can wait on the endpoint's events for good once it is gone,
        // so a stream on it need never say so: the outputs on it are moved from here.
        if let Some(id) = text(id) {
            super::device::endpoint_left(&id);
        }
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        _id: &PCWSTR,
    ) -> windows::core::Result<()> {
        if flow == eRender && role == eConsole {
            super::device::default_endpoint_changed();
        }
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _id: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}
