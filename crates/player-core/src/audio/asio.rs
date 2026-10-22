//! Native ASIO output and the ASIO host: asio-sys under the driver life cycle (`asio_host.rs`), the device a vendor
//! driver is played through, and the watch that turns the driver's reset request into a device
//! fault.
//!
//! cpal's own ASIO host is not used. Its device iterator loads and initialises every installed
//! driver to learn each one's name — slow, and the step a misbehaving driver crashes in — where
//! the SDK lists the names from the registry without loading anything. The device is built here
//! from the one driver the listener chose, out of the same parts cpal's iterator builds it from.

use super::asio_host::{
    negotiate_dsd, AsioDriverApi, AsioDsdFormat, AsioDsdIo, AsioDsdTransport, AsioHost, AsioLease,
    AsioOpenError, ASIO_IDLE_WAIT,
};
use super::asio_writer::{AsioWriteFormat, NativeAsioWriter};
use super::callback::{frames_to_duration, CallbackMoment};
use super::device::{stream_config_for, AudioOutputHost};
use super::rebind::StreamFaultSink;
use asio_sys::{AsioMessageSelectors, AsioStreams, Driver, MessageCallbackId};
use cpal::traits::DeviceTrait;
use cpal::traits::StreamTrait;
use cpal::OutputCallbackInfo;
use std::cell::RefCell;
use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::{atomic::AtomicI32, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

// The shim's status convention, as everywhere in `native/`: 0 is success, anything else failure;
// a value comes back through an out-parameter, written only on success.
unsafe extern "C" {
    fn bn_asio_set_dsd_io_format() -> i32;
    fn bn_asio_restore_pcm_io_format() -> i32;
    fn bn_asio_output_sample_type(channel: i32, sample_type: *mut i32) -> i32;
    fn bn_asio_preferred_buffer_frames(frames: *mut u32) -> i32;
    fn bn_asio_output_latency_frames(frames: *mut u32) -> i32;
    fn bn_asio_create_writer(
        channels: i32,
        opaque: *mut c_void,
        callback: unsafe extern "C" fn(*mut c_void, *mut *mut u8, u32),
        reset: unsafe extern "C" fn(*mut c_void),
        frames: u32,
    ) -> *mut BnNativeAsioWriter;
    fn bn_asio_start_writer(writer: *mut BnNativeAsioWriter) -> i32;
    fn bn_asio_stop_writer(writer: *mut BnNativeAsioWriter) -> i32;
}

/// The ASIO sample type of output `channel`, or -1 when the driver does not report it.
fn output_sample_type(channel: i32) -> i32 {
    let mut sample_type = -1;
    // SAFETY: called only by AsioHost on its driver thread, like every ASIO call here;
    // `sample_type` is a live local the shim writes on success only.
    if unsafe { bn_asio_output_sample_type(channel, &mut sample_type) } != 0 {
        return -1;
    }
    sample_type
}

#[repr(C)]
struct BnNativeAsioWriter {
    _private: [u8; 0],
}

/// The SDK allows one loaded driver per process, and `asio_sys::Asio` is the record of which;
/// so there is one of it.
fn asio() -> &'static asio_sys::Asio {
    static ASIO: OnceLock<asio_sys::Asio> = OnceLock::new();
    ASIO.get_or_init(asio_sys::Asio::new)
}

/// The process's driver life cycle.
pub(super) fn host() -> &'static AsioHost<RealAsio> {
    static HOST: OnceLock<AsioHost<RealAsio>> = OnceLock::new();
    HOST.get_or_init(|| AsioHost::new(RealAsio, ASIO_IDLE_WAIT))
}

/// The installed drivers' names, in the registry's order. Reads the registry; loads nothing.
pub(super) fn driver_names() -> Vec<String> {
    host().names()
}

/// The reservation is shared by the prepared binding and its eventual owned stream. Its final
/// drop restores PCM even when stream construction failed after format negotiation.
struct NativeAsioReservation {
    format: AsioDsdFormat,
    bit_rate: u32,
    original_rate: u32,
    /// Keeps the driver loaded; whether it is in DSD is the host's record (`in_dsd`).
    _lease: AsioLease,
}

impl Drop for NativeAsioReservation {
    fn drop(&mut self) {
        if host().restore_pcm(self.original_rate).is_err() {
            host().request_reset();
        }
    }
}

#[derive(Clone)]
pub(crate) struct NativeAsioDevice(Arc<NativeAsioReservation>);

impl NativeAsioDevice {
    pub(crate) fn transport(&self) -> AsioDsdTransport {
        self.0.format.transport
    }
    pub(crate) fn hardware_rate(&self) -> u32 {
        self.0.format.hardware_rate
    }
    pub(crate) fn channels(&self) -> u16 {
        self.0.format.channels
    }
    fn bit_rate(&self) -> u32 {
        self.0.bit_rate
    }
    fn original_rate(&self) -> u32 {
        self.0.original_rate
    }
    fn park(&self) -> Result<(), String> {
        host().restore_pcm(self.original_rate())
    }
    fn resume(&self) -> Result<(), String> {
        if host().in_dsd() {
            return Ok(());
        }
        let format = host().resume_dsd(self.bit_rate(), self.channels())?;
        if format != self.0.format {
            let _ = host().restore_pcm(self.original_rate());
            return Err("ASIO changed DSD transport while parked".to_owned());
        }
        Ok(())
    }
}

pub(crate) fn open_dsd(
    name: &str,
    bit_rate: u32,
    channels: u16,
) -> Result<Option<NativeAsioDevice>, AsioOpenError> {
    host().open_dsd(name, bit_rate, channels).map(|opened| {
        opened.map(|(format, original_rate, lease)| {
            NativeAsioDevice(Arc::new(NativeAsioReservation {
                format,
                bit_rate,
                original_rate,
                _lease: lease,
            }))
        })
    })
}

/// Fills one period of interleaved, MSB-first packed DSD bytes for the moment it will be heard.
/// PCM on ASIO goes through cpal; only DSD and DoP use this writer.
pub(super) type Render = Box<dyn FnMut(&mut [u8], CallbackMoment) + Send>;

struct RenderState {
    render: Render,
    writers: Vec<NativeAsioWriter>,
    dsd: Vec<u8>,
    frames: usize,
    channels: usize,
    clock_rate: NonZeroU32,
    delivery_delay: Duration,
}

struct RawCallback {
    // ASIO serializes buffer switches. Driver messages may arrive on another thread and read
    // only fault, so their shared reference never aliases a mutable render-state reference.
    render: UnsafeCell<RenderState>,
    fault: StreamFaultSink,
}

/// The shim's buffer switch. `opaque` is the `RawCallback` given to `bn_asio_create_writer`,
/// boxed and kept alive until the writer is stopped; `outputs` holds one ASIO buffer of `frames`
/// (the size the writers were built for) per output.
unsafe extern "C" fn buffer_switch(opaque: *mut c_void, outputs: *mut *mut u8, _frames: u32) {
    // SAFETY: see above; only the shared part is borrowed here.
    let callback = unsafe { &*opaque.cast::<RawCallback>() };
    // SAFETY: ASIO serializes buffer switches, and nothing else touches the render state while
    // the writer runs (`start` rebuilds it before the writer exists).
    let state = unsafe { &mut *callback.render.get() };
    let moment = CallbackMoment {
        observed_at: Instant::now(),
        delivery_delay: state.delivery_delay,
    };
    (state.render)(&mut state.dsd, moment);
    for (channel, writer) in state.writers.iter_mut().enumerate() {
        // SAFETY: one writer per output, so `outputs` has entry `channel`: that output's buffer
        // of `state.frames` frames in the writer's format, ours for this switch.
        unsafe {
            writer.write_dsd_raw_channel(&state.dsd, *outputs.add(channel), state.frames, channel)
        };
    }
}

/// The shim's reset request, on any thread; `opaque` as for `buffer_switch`.
unsafe extern "C" fn reset_request(opaque: *mut c_void) {
    // SAFETY: see above; only `fault`, which is shared, is read.
    let callback = unsafe { &*opaque.cast::<RawCallback>() };
    driver_reset(&callback.fault);
}

/// The driver asked to be reset: it is reloaded at the next open, and the binding's fault is
/// reported, once: a resetting driver may ask again and again, and the binding failed at the
/// first. The native writer's and cpal's streams take a reset request the same way.
fn driver_reset(fault: &StreamFaultSink) {
    host().request_reset();
    if !fault.control.failed.load(Ordering::Acquire) {
        fault.report();
    }
}

enum AsioOwnedState {
    Running {
        writer: *mut BnNativeAsioWriter,
        callback: Box<RawCallback>,
    },
    Parked(Box<RawCallback>),
}

pub(crate) struct NativeAsioStream {
    device: NativeAsioDevice,
    state: RefCell<Option<AsioOwnedState>>,
}

impl NativeAsioStream {
    fn start(
        callback: &mut Box<RawCallback>,
        device: &NativeAsioDevice,
    ) -> Result<*mut BnNativeAsioWriter, String> {
        let opaque = (&mut **callback as *mut RawCallback) as usize;
        let transport = device.transport();
        // A parked driver can report another buffer size or sample type when it resumes.
        // Rebuild those dimensions before CreateBuffers can call us.
        host()
            .run(move |_| {
                // SAFETY: the caller's boxed callback, borrowed by `start` while this runs; no
                // writer exists yet, so no buffer switch reads it concurrently.
                let callback = unsafe { &mut *(opaque as *mut RawCallback) };
                let state = callback.render.get_mut();
                let mut frames = 0;
                // SAFETY: on the ASIO host thread, like every driver call; a live out-parameter.
                if unsafe { bn_asio_preferred_buffer_frames(&mut frames) } != 0 {
                    return Err("the ASIO driver did not report a buffer size".to_owned());
                }
                let frames = frames as usize;
                let byte_frames = if transport == AsioDsdTransport::Native {
                    if !frames.is_multiple_of(8) {
                        return Err("the ASIO DSD buffer is not a whole number of bytes".to_owned());
                    }
                    frames / 8
                } else {
                    frames.checked_mul(2).ok_or("ASIO buffer size overflowed")?
                };
                state.writers = (0..state.channels)
                    .map(|channel| {
                        let format = AsioWriteFormat::for_sample_type(
                            transport,
                            output_sample_type(channel as i32),
                        )?;
                        NativeAsioWriter::new(format, state.channels as u16)
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                state.frames = frames;
                state.dsd.resize(
                    byte_frames
                        .checked_mul(state.channels)
                        .ok_or("ASIO DSD buffer overflowed")?,
                    0x69,
                );
                // SAFETY: on the ASIO host thread; `opaque` is the boxed callback the stream keeps
                // until the writer is stopped, and its state was just sized for `frames`.
                let writer = unsafe {
                    bn_asio_create_writer(
                        state.channels as i32,
                        opaque as *mut c_void,
                        buffer_switch,
                        reset_request,
                        frames as u32,
                    )
                };
                if writer.is_null() {
                    return Err("the ASIO driver did not create output buffers".to_owned());
                }
                // ASIOGetLatencies is valid after buffers exist. Its output latency includes the
                // double-buffer delay; adding another period would bias the media clock.
                let mut latency = frames as u32;
                // SAFETY: on the ASIO host thread; a live out-parameter, left as is on failure.
                unsafe { bn_asio_output_latency_frames(&mut latency) };
                state.delivery_delay = frames_to_duration(latency as usize, state.clock_rate);
                // SAFETY: on the ASIO host thread; the writer just created.
                if unsafe { bn_asio_start_writer(writer) } != 0 {
                    // SAFETY: as above; the writer is not used after this.
                    unsafe { bn_asio_stop_writer(writer) };
                    return Err("the ASIO driver did not start output buffers".to_owned());
                }
                Ok(writer as usize)
            })?
            .map(|writer| writer as *mut BnNativeAsioWriter)
    }

    fn stop(writer: *mut BnNativeAsioWriter) -> Result<(), String> {
        let writer = writer as usize;
        host().run(move |_| {
            // SAFETY: on the ASIO host thread; a writer `start` returned, stopped once.
            if unsafe { bn_asio_stop_writer(writer as *mut BnNativeAsioWriter) } == 0 {
                Ok(())
            } else {
                Err("the ASIO driver did not stop its output callback".to_owned())
            }
        })?
    }

    pub(crate) fn play(&self) -> Result<(), String> {
        let state = self
            .state
            .borrow_mut()
            .take()
            .ok_or("the ASIO stream is inside another operation")?;
        let AsioOwnedState::Parked(mut callback) = state else {
            *self.state.borrow_mut() = Some(state);
            return Ok(());
        };
        let started = self
            .device
            .resume()
            .and_then(|()| Self::start(&mut callback, &self.device));
        match started {
            Ok(writer) => {
                *self.state.borrow_mut() = Some(AsioOwnedState::Running { writer, callback });
                Ok(())
            }
            Err(error) => {
                callback.fault.report();
                let _ = self.device.park();
                *self.state.borrow_mut() = Some(AsioOwnedState::Parked(callback));
                Err(error)
            }
        }
    }

    pub(crate) fn pause(&self) -> Result<(), String> {
        let state = self
            .state
            .borrow_mut()
            .take()
            .ok_or("the ASIO stream is inside another operation")?;
        let AsioOwnedState::Running { writer, callback } = state else {
            *self.state.borrow_mut() = Some(state);
            return Ok(());
        };
        if let Err(error) = Self::stop(writer) {
            callback.fault.report();
            *self.state.borrow_mut() = Some(AsioOwnedState::Running { writer, callback });
            return Err(error);
        }
        let restored = self.device.park();
        *self.state.borrow_mut() = Some(AsioOwnedState::Parked(callback));
        restored
    }
}

impl Drop for NativeAsioStream {
    fn drop(&mut self) {
        if self.pause().is_err() {
            if let Some(AsioOwnedState::Running { callback, .. }) = self.state.get_mut().take() {
                // A driver that refuses Stop may still call its buffers. Keep their context and
                // reservation alive instead of freeing memory under the driver's thread.
                std::mem::forget(callback);
                std::mem::forget(self.device.clone());
            }
        }
    }
}

pub(crate) fn build_native_asio_stream(
    device: NativeAsioDevice,
    render: Render,
    fault: StreamFaultSink,
) -> Result<NativeAsioStream, String> {
    // A driver that reports no rate cannot be clocked: the open fails, the session does not.
    let clock_rate = NonZeroU32::new(device.hardware_rate())
        .ok_or_else(|| "ASIO driver reports a sample rate of 0".to_owned())?;
    let mut callback = Box::new(RawCallback {
        render: UnsafeCell::new(RenderState {
            render,
            writers: Vec::new(),
            dsd: Vec::new(),
            frames: 0,
            channels: usize::from(device.channels()),
            clock_rate,
            delivery_delay: Duration::ZERO,
        }),
        fault,
    });
    let writer = NativeAsioStream::start(&mut callback, &device)?;
    Ok(NativeAsioStream {
        device,
        state: RefCell::new(Some(AsioOwnedState::Running { writer, callback })),
    })
}

/// A loaded driver and the one device value every binding on it shares. Shared, not rebuilt per
/// open: cpal keeps a driver's buffers on the DEVICE value, and a second device value on a loaded
/// driver would create the buffers again underneath a stream still writing into the first ones.
/// On one value, a second stream joins the buffers that exist and cpal sums the streams into them.
pub(super) struct RealDriver {
    driver: Arc<Driver>,
    streams: Mutex<Arc<Mutex<AsioStreams>>>,
    buffer_index: Mutex<Arc<AtomicI32>>,
    native_dsd_format: AtomicBool,
}

impl RealDriver {
    pub(super) fn device(&self) -> cpal::Device {
        cpal::Device::from(cpal::platform::AsioDevice {
            driver: Arc::clone(&self.driver),
            asio_streams: Arc::clone(&self.streams.lock().unwrap_or_else(PoisonError::into_inner)),
            current_buffer_index: Arc::clone(
                &self
                    .buffer_index
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner),
            ),
        })
    }

    /// Stops the keeper and disposes the driver's buffers; disposed, the stream records that
    /// pointed at them are forgotten (not when the dispose failed: the buffers are still there).
    /// Every change of rate or format starts here.
    fn release_buffers(&self) -> Result<(), String> {
        stop_keeper();
        self.driver
            .dispose_buffers()
            .map_err(|error| error.to_string())?;
        self.forget_buffers();
        Ok(())
    }

    /// Back to PCM I/O format from native DSD; the record of native DSD cleared with it.
    fn pcm_io_format(&self) -> Result<(), String> {
        // SAFETY: this ASIO API is called only by AsioHost on its driver thread; no arguments.
        if unsafe { bn_asio_restore_pcm_io_format() } != 0 {
            return Err("the ASIO driver refused PCM I/O format".to_owned());
        }
        self.native_dsd_format.store(false, Ordering::Release);
        Ok(())
    }

    /// Fresh stream records: the old ones point at buffers that were just disposed.
    fn forget_buffers(&self) {
        let (streams, index) = fresh_records();
        *self.streams.lock().unwrap_or_else(PoisonError::into_inner) = streams;
        *self
            .buffer_index
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = index;
    }

    /// The driver can run at `rate` (asked through asio-sys's f64 API).
    fn accepts(&self, rate: u32) -> bool {
        self.driver
            .can_sample_rate(f64::from(rate))
            .unwrap_or(false)
    }

    /// Sets the driver's rate.
    fn set_driver_rate(&self, rate: u32) -> Result<(), String> {
        self.driver
            .set_sample_rate(f64::from(rate))
            .map_err(|error| error.to_string())
    }
}

/// A driver's stream records as they start: no streams, no buffer index (-1).
#[allow(clippy::type_complexity)]
fn fresh_records() -> (Arc<Mutex<AsioStreams>>, Arc<AtomicI32>) {
    (
        Arc::new(Mutex::new(AsioStreams {
            input: None,
            output: None,
        })),
        Arc::new(AtomicI32::new(-1)),
    )
}

thread_local! {
    /// The keeper: a stream the driver thread holds on the loaded driver, writing silence. The
    /// driver runs from its first stream's build to its unload and replays whatever its buffers
    /// hold; between one song's stream and the next nothing else would write them, and the last
    /// buffers of the old song would loop. cpal silences a buffer in the first callback of each
    /// buffer switch, so the keeper costs the songs' streams nothing.
    static KEEPER: RefCell<Option<cpal::Stream>> = const { RefCell::new(None) };
}

fn start_keeper(driver: &RealDriver) -> Result<(), String> {
    let device = driver.device();
    let supported = device
        .default_output_config()
        .map_err(|error| error.to_string())?;
    let config = stream_config_for(AudioOutputHost::Asio, &supported);
    let stream = device
        .build_output_stream_raw(
            &config,
            supported.sample_format(),
            |data: &mut cpal::Data, _: &OutputCallbackInfo| {
                data.bytes_mut().fill(0);
            },
            |_| {},
            None,
        )
        .map_err(|error| error.to_string())?;
    stream.play().map_err(|error| error.to_string())?;
    KEEPER.with(|keeper| *keeper.borrow_mut() = Some(stream));
    Ok(())
}

fn stop_keeper() {
    KEEPER.with(|keeper| drop(keeper.borrow_mut().take()));
}

/// asio-sys, called only on the driver thread.
pub(super) struct RealAsio;

impl AsioDriverApi for RealAsio {
    type Driver = RealDriver;

    fn enter_thread(&self) {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        // The apartment the SDK's own driver list asks for. The thread lives as long as the
        // process, so it is never uninitialised.
        // SAFETY: a plain COM call on this thread.
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    }

    fn names(&self) -> Vec<String> {
        asio().driver_names()
    }

    fn load(&self, name: &str) -> Result<RealDriver, String> {
        let (streams, buffer_index) = fresh_records();
        let driver = RealDriver {
            driver: Arc::new(
                asio()
                    .load_driver(name)
                    .map_err(|error| error.to_string())?,
            ),
            streams: Mutex::new(streams),
            buffer_index: Mutex::new(buffer_index),
            native_dsd_format: AtomicBool::new(false),
        };
        start_keeper(&driver)?;
        Ok(driver)
    }

    fn unload(&self, driver: RealDriver) {
        // The keeper first: it holds the driver too. Then the last handle, whose drop stops,
        // disposes and exits the driver — here, on the thread that loaded it.
        stop_keeper();
        drop(driver);
    }

    fn rate(&self, driver: &RealDriver) -> Option<u32> {
        driver.driver.sample_rate().ok().map(|rate| rate as u32)
    }

    fn can_rate(&self, driver: &RealDriver, rate: u32) -> bool {
        driver.accepts(rate)
    }

    fn set_rate(&self, driver: &RealDriver, rate: u32) -> Result<(), String> {
        let changed = driver
            .release_buffers()
            .and_then(|()| driver.set_driver_rate(rate));
        start_keeper(driver).and(changed)
    }

    fn restore_pcm(&self, driver: &RealDriver, original_rate: u32) -> Result<(), String> {
        driver.release_buffers()?;
        if driver.native_dsd_format.load(Ordering::Acquire) {
            driver.pcm_io_format()?;
        }
        if original_rate != 0 {
            driver.set_driver_rate(original_rate)?;
        }
        start_keeper(driver)
    }

    fn caps(&self, driver: &RealDriver) -> Option<(u16, Vec<u32>)> {
        let channels = u16::try_from(driver.driver.channels().ok()?.outs).ok()?;
        let rates = super::asio_host::COMMON_RATES
            .into_iter()
            .filter(|rate| driver.accepts(*rate))
            .collect();
        Some((channels, rates))
    }

    fn open_dsd(
        &self,
        driver: &RealDriver,
        bit_rate: u32,
        channels: u16,
    ) -> Result<Option<AsioDsdFormat>, String> {
        driver.release_buffers()?;
        negotiate_dsd(
            driver,
            bit_rate,
            channels,
            super::device::DOP_OUTPUT.load(Ordering::Acquire),
        )
    }
}

impl AsioDsdIo for RealDriver {
    fn set_native(&self) -> bool {
        // SAFETY: on the ASIO host thread, like every driver call; no arguments.
        let accepted = unsafe { bn_asio_set_dsd_io_format() } == 0;
        if accepted {
            self.native_dsd_format.store(true, Ordering::Release);
        }
        accepted
    }
    fn set_pcm(&self) -> Result<(), String> {
        self.pcm_io_format()
    }
    fn accepts_rate(&self, rate: u32) -> bool {
        self.accepts(rate)
    }
    fn set_dsd_rate(&self, rate: u32) -> Result<(), String> {
        self.set_driver_rate(rate)
    }
    fn output_type(&self, channel: u16) -> i32 {
        output_sample_type(i32::from(channel))
    }
}
/// A binding's subscription to its driver's messages. cpal 0.15 drops the error callback of an
/// ASIO stream, so without this a driver that resets — its sample rate changed in its panel, its
/// interface was unplugged — would leave the stream silent and the media clock frozen, with the
/// rebind ladder never told. A reset request is reported exactly as a WASAPI stream error is.
pub(crate) struct ResetWatch {
    driver: Arc<Driver>,
    id: MessageCallbackId,
}

impl ResetWatch {
    /// `None` when `device` is not an ASIO device.
    pub(super) fn new(device: &cpal::Device, fault: StreamFaultSink) -> Option<Self> {
        let cpal::platform::DeviceInner::Asio(device) = device.as_inner() else {
            return None;
        };
        let driver = Arc::clone(&device.driver);
        let id = driver.add_message_callback(move |selector| {
            if matches!(selector, AsioMessageSelectors::kAsioResetRequest) {
                driver_reset(&fault);
            }
        });
        Some(Self { driver, id })
    }
}

impl Drop for ResetWatch {
    fn drop(&mut self) {
        self.driver.remove_message_callback(self.id);
    }
}

#[cfg(test)]
mod stream_tests;
