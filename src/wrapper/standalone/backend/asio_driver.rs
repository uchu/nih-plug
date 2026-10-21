//! What an open ASIO driver tells the host outside its buffer callbacks, stopping it at the end
//! of a run (spec A6), and reloading it by name (A7). Off ASIO every function here is inert.

use super::buffer_sizes::BufferFacts;
use cpal::{Device, Stream};
use crossbeam::sync::Unparker;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Whether the backend holds an opened ASIO driver: raised once a duplex open succeeded, lowered
/// before that driver is released.
static DRIVER_OPEN: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_driver_open(open: bool) {
    DRIVER_OPEN.store(open, std::sync::atomic::Ordering::Release);
}

pub(crate) fn driver_open() -> bool {
    DRIVER_OPEN.load(std::sync::atomic::Ordering::Acquire)
}

/// A duplex driver's stream, which a run hands to the GUI thread to start, stop and drop (E1).
pub(crate) struct DriverStream(pub(crate) Stream);

// SAFETY: cpal marks every `Stream` as not `Send` for Android's AAudio. Only a duplex (ASIO) run
// makes a `DriverStream`, and cpal's ASIO stream is a play flag, the driver and its buffers behind
// `Arc`s and a callback id, all `Send`, which the assertion below checks where ASIO builds.
unsafe impl Send for DriverStream {}

#[cfg(all(target_os = "windows", feature = "asio"))]
fn assert_send<T: Send>() {}

#[cfg(all(target_os = "windows", feature = "asio"))]
const _: fn() = assert_send::<cpal::platform::AsioStream>;

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

/// The loaded driver's buffer sizes. `None` off ASIO.
#[cfg(all(target_os = "windows", feature = "asio"))]
pub(crate) fn buffer_facts(device: &Device) -> Option<anyhow::Result<BufferFacts>> {
    use std::os::raw::c_long;

    let cpal::platform::DeviceInner::Asio(_) = device.as_inner() else {
        return None;
    };
    let (mut min, mut max, mut preferred, mut granularity): (c_long, c_long, c_long, c_long) =
        (0, 0, 0, 0);
    // The device holds the loaded driver, which `ASIOGetBufferSize` reads. asio-sys's safe
    // `buffersize_range()` drops the preferred size and the granularity (A12).
    let result = unsafe {
        asio_sys::bindings::asio_import::ASIOGetBufferSize(
            &mut min,
            &mut max,
            &mut preferred,
            &mut granularity,
        )
    };
    Some(if result == 0 {
        Ok(BufferFacts {
            min,
            max,
            preferred,
            granularity,
        })
    } else {
        Err(anyhow::anyhow!(
            "ASIOGetBufferSize failed with ASIO error {result}"
        ))
    })
}

#[cfg(not(all(target_os = "windows", feature = "asio")))]
pub(crate) fn buffer_facts(_device: &Device) -> Option<anyhow::Result<BufferFacts>> {
    None
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

/// The SDK holds one driver per process; this instance tracks the one `load` loaded.
#[cfg(all(target_os = "windows", feature = "asio"))]
fn asio() -> &'static asio_sys::Asio {
    static ASIO: std::sync::OnceLock<asio_sys::Asio> = std::sync::OnceLock::new();
    ASIO.get_or_init(asio_sys::Asio::new)
}

/// The registered drivers, by name, as the registry lists them: nothing is loaded. The cpal
/// host's device listing loads and releases every driver instead, including one another
/// application streams on and one whose hardware is absent.
pub(crate) fn names() -> Vec<String> {
    super::cpal::dedupe_names(registered())
}

#[cfg(all(target_os = "windows", feature = "asio"))]
fn registered() -> Vec<String> {
    asio().driver_names()
}

#[cfg(not(all(target_os = "windows", feature = "asio")))]
fn registered() -> Vec<String> {
    Vec::new()
}

/// What a loaded driver reports about its outputs, read in the order cpal reads it for an output
/// configuration. Errors are the driver's, as text.
pub(crate) struct OutputReport {
    pub(crate) outputs: Result<i64, String>,
    pub(crate) rate: Result<f64, String>,
    pub(crate) buffer_sizes: Result<(), String>,
    /// As asio-sys names it.
    pub(crate) sample_type: Result<String, String>,
    /// Whether cpal converts samples of `sample_type` (E7).
    pub(crate) converts: bool,
}

/// Why a loaded driver offers cpal no output configuration: the first report that rules one out.
/// Every report reading fine leaves the rates, none of which the driver accepts.
pub(crate) fn no_output_configuration(report: Option<OutputReport>) -> String {
    let Some(report) = report else {
        return "it offers no output configuration".to_string();
    };
    match report.outputs {
        Err(err) => return format!("its channel count is unreadable: {err}"),
        Ok(outputs) if outputs < 1 => return "it reports no output channels".to_string(),
        Ok(_) => {}
    }
    let rate = match report.rate {
        Ok(rate) => rate,
        Err(err) => return format!("its sample rate is unreadable: {err}"),
    };
    if let Err(err) = report.buffer_sizes {
        return format!("its buffer sizes are unreadable: {err}");
    }
    let sample_type = match report.sample_type {
        Ok(sample_type) => sample_type,
        Err(err) => return format!("its sample type is unreadable: {err}"),
    };
    if report.converts {
        format!("no usable output configuration at {rate} Hz (sample type {sample_type})")
    } else {
        format!("no supported sample format ({sample_type})")
    }
}

#[cfg(all(target_os = "windows", feature = "asio"))]
pub(crate) fn output_report(device: &Device) -> Option<OutputReport> {
    use cpal::traits::DeviceTrait;

    let cpal::platform::DeviceInner::Asio(asio) = device.as_inner() else {
        return None;
    };
    let driver = &asio.driver;
    Some(OutputReport {
        outputs: driver
            .channels()
            .map(|channels| i64::from(channels.outs))
            .map_err(|err| err.to_string()),
        rate: driver.sample_rate().map_err(|err| err.to_string()),
        buffer_sizes: driver
            .buffersize_range()
            .map(drop)
            .map_err(|err| err.to_string()),
        sample_type: driver
            .output_data_type()
            .map(|sample_type| format!("{sample_type:?}"))
            .map_err(|err| err.to_string()),
        // cpal also answers this for an unreadable rate, which the report names first.
        converts: !matches!(
            device.default_output_config(),
            Err(cpal::DefaultStreamConfigError::StreamTypeNotSupported)
        ),
    })
}

#[cfg(not(all(target_os = "windows", feature = "asio")))]
pub(crate) fn output_report(_device: &Device) -> Option<OutputReport> {
    None
}

/// The driver `name`, loaded by name. Enumerating through the cpal host instead loads and
/// releases every driver registered ahead of it, and all of them while it does not load. Called
/// only with every handle to the previous driver released, so no driver is loaded.
#[cfg(all(target_os = "windows", feature = "asio"))]
pub(crate) fn load(name: &str) -> Option<Device> {
    use std::sync::atomic::AtomicI32;
    use std::sync::Mutex;

    let driver = match asio().load_driver(name) {
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

    fn report() -> OutputReport {
        OutputReport {
            outputs: Ok(2),
            rate: Ok(48000.0),
            buffer_sizes: Ok(()),
            sample_type: Ok("ASIOSTInt32LSB".to_string()),
            converts: true,
        }
    }

    #[test]
    fn a_supported_sample_type_is_not_blamed() {
        assert_eq!(
            no_output_configuration(Some(report())),
            "no usable output configuration at 48000 Hz (sample type ASIOSTInt32LSB)"
        );
    }

    #[test]
    fn an_unsupported_sample_type_is_named() {
        let report = OutputReport {
            sample_type: Ok("ASIOSTFloat64LSB".to_string()),
            converts: false,
            ..report()
        };
        assert_eq!(
            no_output_configuration(Some(report)),
            "no supported sample format (ASIOSTFloat64LSB)"
        );
    }

    #[test]
    fn the_first_report_that_rules_a_configuration_out_is_the_reason() {
        let no_clock = OutputReport {
            rate: Err("no clock".to_string()),
            converts: false,
            ..report()
        };
        assert_eq!(
            no_output_configuration(Some(no_clock)),
            "its sample rate is unreadable: no clock"
        );
        let no_outputs = OutputReport {
            outputs: Ok(0),
            converts: false,
            ..report()
        };
        assert_eq!(
            no_output_configuration(Some(no_outputs)),
            "it reports no output channels"
        );
        let channels = OutputReport {
            outputs: Err("gone".to_string()),
            ..report()
        };
        assert_eq!(
            no_output_configuration(Some(channels)),
            "its channel count is unreadable: gone"
        );
        let buffers = OutputReport {
            buffer_sizes: Err("gone".to_string()),
            ..report()
        };
        assert_eq!(
            no_output_configuration(Some(buffers)),
            "its buffer sizes are unreadable: gone"
        );
        let sample_type = OutputReport {
            sample_type: Err("gone".to_string()),
            converts: false,
            ..report()
        };
        assert_eq!(
            no_output_configuration(Some(sample_type)),
            "its sample type is unreadable: gone"
        );
    }

    #[test]
    fn a_device_with_no_report_offers_no_configuration() {
        assert_eq!(
            no_output_configuration(None),
            "it offers no output configuration"
        );
    }
}
