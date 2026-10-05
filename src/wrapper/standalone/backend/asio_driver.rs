//! What an open ASIO driver tells the host outside its buffer callbacks, and what reloading one
//! needs (spec A6). Off ASIO every function here is inert.

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

/// Initialise COM on the calling thread, once. Reloading a driver on the audio thread creates its
/// COM object there (`asiolist.cpp` `CoCreateInstance`), and the SDK initialises COM only on the
/// thread that first loaded a driver. Never uninitialised: the thread owns the driver until exit.
#[cfg(all(target_os = "windows", feature = "asio"))]
pub(crate) fn com_ready_on_this_thread() {
    use std::cell::Cell;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

    thread_local!(static TRIED: Cell<bool> = const { Cell::new(false) });
    TRIED.with(|tried| {
        if !tried.replace(true) {
            // RPC_E_CHANGED_MODE leaves the thread in the multithreaded apartment, which serves
            // CoCreateInstance all the same.
            let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        }
    });
}

#[cfg(not(all(target_os = "windows", feature = "asio")))]
pub(crate) fn com_ready_on_this_thread() {}

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
