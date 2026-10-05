//! What an open ASIO driver tells the host outside its buffer callbacks, stopping it at the end
//! of a run (spec A6), and reloading it by name (A7). Off ASIO every function here is inert.

use cpal::Device;
use crossbeam::sync::Unparker;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Stops listening for the driver's reset requests when dropped.
pub(crate) struct ResetListener(Option<Box<dyn FnOnce() + Send>>);

impl Drop for ResetListener {
    fn drop(&mut self) {
        if let Some(remove) = self.0.take() {
            remove();
        }
    }
}

/// Raise `requested` and wake the run when the driver asks to be reset (asio.h
/// `kAsioResetRequest`: a buffer-size or clock change in its control panel, a replug). asio-sys
/// forwards only that selector to listeners, so the argument is not inspected.
#[cfg(all(target_os = "windows", feature = "asio"))]
pub(crate) fn listen_for_reset(
    device: &Device,
    requested: Arc<AtomicBool>,
    unparker: Unparker,
) -> ResetListener {
    use std::sync::atomic::Ordering;

    let cpal::platform::DeviceInner::Asio(asio) = device.as_inner() else {
        return ResetListener(None);
    };
    let driver = asio.driver.clone();
    let id = driver.add_message_callback(move |_| {
        requested.store(true, Ordering::Release);
        unparker.unpark();
    });
    ResetListener(Some(Box::new(move || driver.remove_message_callback(id))))
}

#[cfg(not(all(target_os = "windows", feature = "asio")))]
pub(crate) fn listen_for_reset(
    _device: &Device,
    _requested: Arc<AtomicBool>,
    _unparker: Unparker,
) -> ResetListener {
    ResetListener(None)
}

/// The rate the driver runs at right now. ASIOGetSampleRate is legal while the driver streams.
#[cfg(all(target_os = "windows", feature = "asio"))]
pub(crate) fn driver_rate(device: &Device) -> Option<f64> {
    match device.as_inner() {
        cpal::platform::DeviceInner::Asio(asio) => asio.driver.sample_rate().ok(),
        _ => None,
    }
}

#[cfg(not(all(target_os = "windows", feature = "asio")))]
pub(crate) fn driver_rate(_device: &Device) -> Option<f64> {
    None
}

/// A driver that keeps streaming at another rate leaves the plugin processing at the session's
/// rate on hardware running at a different one.
pub(crate) fn rate_moved(driver_rate: f64, session_rate: f32) -> bool {
    (driver_rate - session_rate as f64).abs() > 0.1
}

/// Stop the driver. Dropping a cpal ASIO stream only removes its callback, and a running driver
/// keeps replaying its last two periods with nothing writing them; ASIOStop returns after the
/// last buffer switch, so the hardware is quiet until the driver is released or started again.
#[cfg(all(target_os = "windows", feature = "asio"))]
pub(crate) fn stop(device: &Device) {
    if let cpal::platform::DeviceInner::Asio(asio) = device.as_inner() {
        if let Err(err) = asio.driver.stop() {
            nih_error!("Could not stop the ASIO driver: {err}");
        }
    }
}

#[cfg(not(all(target_os = "windows", feature = "asio")))]
pub(crate) fn stop(_device: &Device) {}

/// The driver `name`, loaded by name. Enumerating through the cpal host instead loads and
/// releases every driver registered ahead of it, and all of them while it does not load. Called
/// only with every handle to the previous driver released, so no driver is loaded.
#[cfg(all(target_os = "windows", feature = "asio"))]
pub(crate) fn load(name: &str) -> Option<Device> {
    use std::sync::atomic::AtomicI32;
    use std::sync::{Mutex, OnceLock};

    // The SDK holds one driver per process; this instance tracks the one it loaded.
    static ASIO: OnceLock<asio_sys::Asio> = OnceLock::new();
    let driver = match ASIO.get_or_init(asio_sys::Asio::new).load_driver(name) {
        Ok(driver) => driver,
        Err(err) => {
            nih_log!("The ASIO driver '{name}' does not load: {err}");
            return None;
        }
    };
    let device = cpal::platform::AsioDevice {
        driver: Arc::new(driver),
        asio_streams: Arc::new(Mutex::new(asio_sys::AsioStreams {
            input: None,
            output: None,
        })),
        current_buffer_index: Arc::new(AtomicI32::new(-1)),
    };
    Some(device.into())
}

#[cfg(not(all(target_os = "windows", feature = "asio")))]
pub(crate) fn load(_name: &str) -> Option<Device> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_change_is_anything_past_a_tenth_of_a_hertz() {
        assert!(!rate_moved(48_000.0, 48_000.0));
        assert!(!rate_moved(48_000.05, 48_000.0));
        assert!(rate_moved(44_100.0, 48_000.0));
        assert!(rate_moved(96_000.0, 48_000.0));
    }
}
