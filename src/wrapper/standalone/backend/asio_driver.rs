//! What an open ASIO driver tells the host outside its buffer callbacks, stopping it at the end
//! of a run (spec A6), and reloading it by name (A7). Off ASIO every function here is inert.

use super::buffer_sizes::BufferFacts;
use cpal::traits::StreamTrait;
use cpal::{Device, Stream};
use crossbeam::sync::Unparker;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

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
pub(crate) struct DriverStream(Stream);

// SAFETY: cpal marks every `Stream` as not `Send` for Android's AAudio. `DriverStream::new()`
// admits only cpal's ASIO stream, a play flag, the driver and its buffers behind `Arc`s and a
// callback id, all `Send`, which the assertion below checks where ASIO builds.
unsafe impl Send for DriverStream {}

impl DriverStream {
    /// `None` for any stream but an ASIO driver's: no other host's stream may cross threads.
    #[cfg(all(target_os = "windows", feature = "asio"))]
    pub(crate) fn new(stream: Stream) -> Option<Self> {
        matches!(stream.as_inner(), cpal::platform::StreamInner::Asio(_)).then_some(Self(stream))
    }

    #[cfg(not(all(target_os = "windows", feature = "asio")))]
    pub(crate) fn new(stream: Stream) -> Option<Self> {
        drop(stream);
        None
    }

    pub(crate) fn play(&self) -> Result<(), cpal::PlayStreamError> {
        self.0.play()
    }
}

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
/// application streams on and one whose hardware is absent. The listing is logged as read, with a
/// warning for every name that does not load the driver it names.
pub(crate) fn names() -> Vec<String> {
    let registered = registered();
    nih_log!("ASIO drivers registered: {registered:?}");
    for warning in listing_warnings(&registered) {
        nih_warn!("{warning}");
    }
    super::cpal::dedupe_names(registered)
}

/// The bytes a listed driver name fits in (asio-sys's name buffers and the length the SDK's
/// listing passes). A longer name is listed as its first `LISTED_NAME_BYTES - 4` bytes and "...".
const LISTED_NAME_BYTES: usize = 32;

/// Names in a driver listing that do not load the driver they name. A load by name opens the first
/// driver listed under it, so of several listed alike (names cut short to the same text, or
/// registered alike) only the first loads. A name with U+FFFD came from bytes that are not UTF-8
/// (the listing is in the system's ANSI code page), and a load by it matches no driver. A blank
/// name is not listed at all.
fn listing_warnings(registered: &[String]) -> Vec<String> {
    let mut warnings = Vec::new();
    match registered
        .iter()
        .filter(|name| name.trim().is_empty())
        .count()
    {
        0 => {}
        1 => warnings
            .push("An ASIO driver is registered with a blank name: it is not listed".to_string()),
        blank => warnings.push(format!(
            "{blank} ASIO drivers are registered with blank names: none of them is listed"
        )),
    }
    for (index, name) in registered.iter().enumerate() {
        if name.trim().is_empty() || registered.iter().take(index).any(|other| other == name) {
            continue;
        }
        let count = registered.iter().filter(|other| *other == name).count();
        if count > 1 {
            // Decoding grows each byte that is not UTF-8 into three, so a cut name can decode
            // longer than it was listed.
            let cut = name.len() >= LISTED_NAME_BYTES - 1 && name.ends_with("...");
            let how = if cut {
                format!(
                    "listed as '{name}', their names cut to {} bytes",
                    LISTED_NAME_BYTES - 4
                )
            } else {
                format!("registered as '{name}'")
            };
            warnings.push(format!(
                "{count} ASIO drivers are {how}: only the first of them loads"
            ));
        }
        if name.contains(char::REPLACEMENT_CHARACTER) {
            warnings.push(format!(
                "The ASIO driver listed as '{name}' has a name that is not valid UTF-8: it does \
                 not load by that name"
            ));
        }
    }
    warnings
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
///
/// The SDK loads by a list of the registered drivers it takes once, at the first load, while the
/// listing (`names`) reads the registry afresh; asio-sys offers no way to retake that list. A
/// driver registered after the first load is therefore listed but loads only after a relaunch,
/// which is logged when it happens.
#[cfg(all(target_os = "windows", feature = "asio"))]
pub(crate) fn load(name: &str) -> Option<Device> {
    use std::sync::atomic::AtomicI32;
    use std::sync::{Mutex, OnceLock};

    static FIRST_LOAD_LISTING: OnceLock<Vec<String>> = OnceLock::new();
    let first_listing = FIRST_LOAD_LISTING.get_or_init(registered);
    let started = Instant::now();
    let loaded = asio().load_driver(name);
    let ms = millis_since(started);
    let driver = match loaded {
        Ok(driver) => {
            nih_log!("ASIO driver '{name}' loaded in {ms:.1} ms");
            driver
        }
        Err(err) => {
            nih_log!("The ASIO driver '{name}' does not load ({ms:.1} ms): {err}");
            if matches!(err, asio_sys::LoadDriverError::LoadDriverFailed)
                && registered_since(first_listing, &registered(), name)
            {
                nih_warn!(
                    "The ASIO driver '{name}' was registered after the first ASIO driver loaded: \
                     the ASIO SDK loads it only after a relaunch"
                );
            }
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

/// Whether `name` is registered `now` but was not when the SDK took its list (`first`).
#[cfg(any(test, all(target_os = "windows", feature = "asio")))]
fn registered_since(first: &[String], now: &[String], name: &str) -> bool {
    now.iter().any(|listed| listed == name) && !first.iter().any(|listed| listed == name)
}

/// Drop `handles`. When the loaded driver goes with them (its last handle runs
/// ASIODisposeBuffers and ASIOExit), log how long that took.
pub(crate) fn release<T>(handles: T) {
    let Some(name) = loaded_name() else {
        drop(handles);
        return;
    };
    let started = Instant::now();
    drop(handles);
    if loaded_name().is_none() {
        let ms = millis_since(started);
        nih_log!("ASIO driver '{name}' released in {ms:.1} ms");
    }
}

#[cfg(all(target_os = "windows", feature = "asio"))]
fn loaded_name() -> Option<String> {
    asio()
        .loaded_driver()
        .map(|driver| driver.name().to_string())
}

#[cfg(not(all(target_os = "windows", feature = "asio")))]
fn loaded_name() -> Option<String> {
    None
}

fn millis_since(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1e3
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
    fn names_cut_to_the_same_listing_are_warned_once_by_name() {
        let cut = format!("{}...", "Focusrite USB ASIO Driver Mu");
        assert_eq!(cut.len(), LISTED_NAME_BYTES - 1);
        let warnings = listing_warnings(&[
            cut.clone(),
            "ASIO4ALL v2".to_string(),
            cut.clone(),
            cut.clone(),
        ]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains(&format!("'{cut}'")));
        assert!(warnings[0].contains("3 ASIO drivers"));
        assert!(warnings[0].contains("28 bytes"));
    }

    #[test]
    fn a_name_registered_twice_uncut_is_warned_without_truncation() {
        let warnings = listing_warnings(&["Same".to_string(), "Same".to_string()]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("'Same'"));
        assert!(!warnings[0].contains("28 bytes"));
    }

    #[test]
    fn an_undecodable_name_is_warned() {
        let warnings =
            listing_warnings(&["Lyd\u{FFFD}kort".to_string(), "ASIO4ALL v2".to_string()]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("'Lyd\u{FFFD}kort'"));
    }

    #[test]
    fn undecodable_names_cut_alike_are_warned_as_cut_once() {
        let cut = format!("Lydkort \u{FFFD} Studio Interface 2{}", "...");
        assert!(cut.len() > LISTED_NAME_BYTES - 1);
        let warnings = listing_warnings(&[cut.clone(), cut.clone()]);
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("2 ASIO drivers"));
        assert!(warnings[0].contains("28 bytes"));
        assert!(warnings[1].contains("not valid UTF-8"));
    }

    #[test]
    fn blank_names_are_warned_as_unlisted_not_as_duplicates() {
        let warnings = listing_warnings(&[
            String::new(),
            "ASIO4ALL v2".to_string(),
            " ".to_string(),
            String::new(),
        ]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("blank"));
        assert!(warnings[0].contains("3 ASIO drivers"));
        assert!(!warnings[0].contains("''"));
    }

    #[test]
    fn a_clean_listing_warns_nothing() {
        assert!(
            listing_warnings(&["ASIO4ALL v2".to_string(), "Realtek ASIO".to_string()]).is_empty()
        );
        assert!(listing_warnings(&[]).is_empty());
    }

    #[test]
    fn only_a_driver_registered_since_the_first_load_is_one_the_sdk_cannot_load() {
        let first = ["ASIO4ALL v2".to_string()];
        let now = ["ASIO4ALL v2".to_string(), "Focusrite USB ASIO".to_string()];
        assert!(registered_since(&first, &now, "Focusrite USB ASIO"));
        assert!(!registered_since(&first, &now, "ASIO4ALL v2"));
        assert!(!registered_since(&first, &now, "Gone ASIO"));
    }

    #[test]
    fn a_device_with_no_report_offers_no_configuration() {
        assert_eq!(
            no_output_configuration(None),
            "it offers no output configuration"
        );
    }
}
