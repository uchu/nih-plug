//! A standalone plugin target that directly connects to the system's audio and MIDI ports instead
//! of relying on a plugin host. This is mostly useful for quickly testing GUI changes.

use std::sync::RwLock;

use clap::{CommandFactory, FromArgMatches};

use self::backend::Backend;
use self::config::{WrapperConfig, DEFAULT_PERIOD_SIZE};
use self::wrapper::{Wrapper, WrapperError};
use super::util::setup_logger;
use crate::prelude::Plugin;

mod backend;
mod change;
mod config;
mod context;
mod recovery;
mod wrapper;

pub use change::{request_audio_change, AudioChange};

/// The audio devices the standalone's stream is open on right now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioDevicesInUse {
    /// The output the stream plays through. `None` is the system default: what was asked for, or
    /// standing in for a requested output that is not connected or could not open.
    pub output: Option<String>,
    /// The device audio input is read from. `None` is no capture: none was asked for, or the
    /// requested input is not connected or could not open.
    pub input: Option<String>,
    /// A requested output that is connected but could not open the session's stream. On ASIO, a
    /// driver the user picked that did not load or open while the previous one runs again, or
    /// [`FIRST_AVAILABLE_REFUSED`].
    pub refused_output: Option<String>,
    /// A requested input that is connected but could not open the session's stream.
    pub refused_input: Option<String>,
    /// The device the output stream is open on, by name, however it was picked: the requested
    /// one, or the default standing in. On a duplex host this is the driver.
    pub opened: Option<String>,
    /// Whether the stream runs on a single-device duplex host (ASIO). `false` on a launch that
    /// asked for ASIO is WASAPI standing in for a driver that could not open.
    pub duplex: bool,
    /// Every output the host listed at launch (spec A3), carried through restarts: the app's
    /// pickers read these instead of enumerating on their own, which on ASIO would load and unload
    /// every driver under a running stream. Empty on hosts that are not duplex.
    pub outputs: Vec<String>,
    /// Every input the host listed at launch; on a duplex host, the outputs.
    pub inputs: Vec<String>,
    /// The host the stream runs on, by backend id: `"wasapi"`, `"asio"`, `"core-audio"`, `"alsa"`.
    pub driver: Option<String>,
    /// A driver, by backend id, that was asked for but opened nothing; the stream stayed on `driver`.
    pub refused_driver: Option<String>,
    /// The stream's sample rate in Hz, once an output is open.
    pub sample_rate: Option<u32>,
    /// The stream's period in samples, once an output is open.
    pub buffer_size: Option<u32>,
    /// Every buffer size the ASIO driver accepts; empty on other hosts.
    pub buffer_sizes: Vec<u32>,
    /// The ASIO driver's preferred buffer size; `None` on other hosts.
    pub preferred_buffer_size: Option<u32>,
}

/// [`AudioDevicesInUse::refused_output`] when "First available" was picked on ASIO and no driver
/// opened; the driver that ran before runs again. No driver name contains a NUL, so this never
/// names a real one.
pub const FIRST_AVAILABLE_REFUSED: &str = "\u{0}first-available";

static AUDIO_DEVICES_IN_USE: RwLock<Option<AudioDevicesInUse>> = RwLock::new(None);

/// What the running standalone's audio stream is open on. `None` until a backend that names its
/// devices (CPAL) has opened one. Follows the stream across an unplug, the stand-in and the return.
pub fn audio_devices_in_use() -> Option<AudioDevicesInUse> {
    AUDIO_DEVICES_IN_USE
        .read()
        .ok()
        .and_then(|slot| slot.clone())
}

pub(crate) fn publish_audio_devices_in_use(in_use: AudioDevicesInUse) {
    if let Ok(mut slot) = AUDIO_DEVICES_IN_USE.write() {
        *slot = Some(in_use);
    }
}

/// Open an NIH-plug plugin as a standalone application. If the plugin has an editor, this will open
/// the editor and block until the editor is closed. Otherwise this will block until SIGINT is
/// received. This is mainly useful for quickly testing plugin GUIs. In order to use this, you will
/// first need to make your plugin's main struct `pub` and expose a `lib` artifact in addition to
/// your plugin's `cdylib`:
///
/// ```toml
/// # Cargo.toml
///
/// [lib]
/// # The `lib` artifact is needed for the standalone target
/// crate-type = ["cdylib", "lib"]
/// ```
///
/// You can then create a `src/main.rs` file that calls this function:
///
/// ```ignore
/// // src/main.rs
///
/// use nih_plug::prelude::*;
///
/// use plugin_name::PluginName;
///
/// fn main() {
///     nih_export_standalone::<PluginName>();
/// }
/// ```
///
/// By default this will connect to the 'default' audio and MIDI ports. Use the command line options
/// to change this. `--help` lists all available options.
///
/// If the wrapped plugin fails to initialize or throws an error during audio processing, then this
/// function will return `false`.
pub fn nih_export_standalone<P: Plugin>() -> bool {
    // TODO: If the backend fails to initialize then the standalones will exit normally instead of
    //       with an error code. This should probably be changed.
    nih_export_standalone_with_args::<P, _>(std::env::args())
}

/// The same as [`nih_export_standalone()`], but with the arguments taken from an iterator instead
/// of using [`std::env::args()`].
pub fn nih_export_standalone_with_args<P: Plugin, Args: IntoIterator<Item = String>>(
    args: Args,
) -> bool {
    setup_logger();

    // Instead of parsing this directly, we need to take a bit of a roundabout approach to get the
    // plugin's name and vendor in here since they'd otherwise be taken from NIH-plug's own
    // `Cargo.toml` file.
    let mut config = WrapperConfig::from_arg_matches(
        &WrapperConfig::command()
            .name(P::NAME)
            .author(P::VENDOR)
            .get_matches_from(args),
    )
    .unwrap_or_else(|err| err.exit());
    config.resolve_period();

    match config.backend {
        config::BackendType::Auto => {
            let result = backend::Jack::new::<P>(config.clone()).map(|backend| {
                nih_log!("Using the JACK backend");
                run_wrapper::<P, _>(backend, config.clone())
            });

            #[cfg(target_os = "linux")]
            let result = result.or_else(|_| {
                match backend::CpalMidir::new::<P>(config.clone(), cpal::HostId::Alsa) {
                    Ok(backend) => {
                        let actual_config = config_with_actual_rate(&config, &backend);
                        nih_log!("Using the ALSA backend");
                        Ok(run_wrapper::<P, _>(backend, actual_config))
                    }
                    Err(err) => {
                        nih_error!(
                            "Could not initialize either the JACK or the ALSA backends, falling \
                             back to the dummy audio backend: {err:#}"
                        );
                        Err(())
                    }
                }
            });
            #[cfg(target_os = "macos")]
            let result = result.or_else(|_| {
                match backend::CpalMidir::new::<P>(config.clone(), cpal::HostId::CoreAudio) {
                    Ok(backend) => {
                        let actual_config = config_with_actual_rate(&config, &backend);
                        nih_log!("Using the CoreAudio backend");
                        Ok(run_wrapper::<P, _>(backend, actual_config))
                    }
                    Err(err) => {
                        nih_error!(
                            "Could not initialize either the JACK or the CoreAudio backends, \
                             falling back to the dummy audio backend: {err:#}"
                        );
                        Err(())
                    }
                }
            });
            #[cfg(target_os = "windows")]
            let result = result.or_else(|_| {
                match backend::CpalMidir::new::<P>(config.clone(), cpal::HostId::Wasapi) {
                    Ok(backend) => {
                        let actual_config = config_with_actual_rate(&config, &backend);
                        nih_log!("Using the WASAPI backend");
                        Ok(run_wrapper::<P, _>(backend, actual_config))
                    }
                    Err(err) => {
                        nih_error!(
                            "Could not initialize either the JACK or the WASAPI backends, falling \
                             back to the dummy audio backend: {err:#}"
                        );
                        Err(())
                    }
                }
            });

            result.unwrap_or_else(|_| {
                nih_error!("Falling back to the dummy audio backend, audio and MIDI will not work");
                run_wrapper::<P, _>(backend::Dummy::new::<P>(config.clone()), config)
            })
        }
        config::BackendType::Jack => match backend::Jack::new::<P>(config.clone()) {
            Ok(backend) => run_wrapper::<P, _>(backend, config),
            Err(err) => {
                nih_error!("Could not initialize the JACK backend: {:#}", err);
                false
            }
        },
        #[cfg(target_os = "linux")]
        config::BackendType::Alsa => {
            match backend::CpalMidir::new::<P>(config.clone(), cpal::HostId::Alsa) {
                Ok(backend) => {
                    let actual_config = config_with_actual_rate(&config, &backend);
                    run_wrapper::<P, _>(backend, actual_config)
                }
                Err(err) => {
                    nih_error!("Could not initialize the ALSA backend: {:#}", err);
                    false
                }
            }
        }
        #[cfg(target_os = "macos")]
        config::BackendType::CoreAudio => {
            match backend::CpalMidir::new::<P>(config.clone(), cpal::HostId::CoreAudio) {
                Ok(backend) => {
                    let actual_config = config_with_actual_rate(&config, &backend);
                    run_wrapper::<P, _>(backend, actual_config)
                }
                Err(err) => {
                    nih_error!("Could not initialize the CoreAudio backend: {:#}", err);
                    false
                }
            }
        }
        #[cfg(target_os = "windows")]
        config::BackendType::Wasapi => {
            match backend::CpalMidir::new::<P>(config.clone(), cpal::HostId::Wasapi) {
                Ok(backend) => {
                    let actual_config = config_with_actual_rate(&config, &backend);
                    run_wrapper::<P, _>(backend, actual_config)
                }
                Err(err) => {
                    nih_error!("Could not initialize the WASAPI backend: {:#}", err);
                    false
                }
            }
        }
        // No ASIO driver that loads (none installed, the interface off, a single-client driver
        // held by another application) must not leave the application without a window: WASAPI
        // stands in on the system default, and the stand-in is published (`duplex: false`).
        #[cfg(all(target_os = "windows", feature = "asio"))]
        config::BackendType::Asio => {
            match backend::CpalMidir::new::<P>(config.clone(), cpal::HostId::Asio) {
                Ok(backend) => {
                    let actual_config = config_with_actual_rate(&config, &backend);
                    run_wrapper::<P, _>(backend, actual_config)
                }
                Err(err) => {
                    nih_error!(
                        "Could not open an ASIO driver, WASAPI stands in on the system default: \
                         {err:#}"
                    );
                    let config = stand_in_config(&config);
                    match backend::CpalMidir::new::<P>(config.clone(), cpal::HostId::Wasapi) {
                        Ok(backend) => {
                            let actual_config = config_with_actual_rate(&config, &backend);
                            run_wrapper::<P, _>(backend, actual_config)
                        }
                        Err(err) => {
                            nih_error!("Could not initialize the WASAPI backend: {err:#}");
                            false
                        }
                    }
                }
            }
        }
        config::BackendType::Dummy => {
            run_wrapper::<P, _>(backend::Dummy::new::<P>(config.clone()), config)
        }
    }
}

/// The configuration another host stands in with: the requested devices are the first host's
/// names (ASIO drivers), which mean nothing to it, so it opens the system default, and the
/// requested period was the ASIO driver's, so it runs the shared hosts' default.
#[cfg_attr(not(all(target_os = "windows", feature = "asio")), allow(dead_code))]
fn stand_in_config(config: &WrapperConfig) -> WrapperConfig {
    WrapperConfig {
        output_device: None,
        input_device: None,
        period_request: None,
        period_size: DEFAULT_PERIOD_SIZE,
        ..config.clone()
    }
}

/// Create a config with the actual sample rate from the CPAL backend, which may have adjusted
/// the rate to match the device's native rate.
fn config_with_actual_rate(config: &WrapperConfig, backend: &backend::CpalMidir) -> WrapperConfig {
    let mut actual_config = config.clone();
    actual_config.sample_rate = backend.actual_sample_rate();
    actual_config.period_size = backend.actual_period_size();
    actual_config
}

fn run_wrapper<P: Plugin, B: Backend<P>>(backend: B, config: WrapperConfig) -> bool {
    let wrapper = match Wrapper::<P, _>::new(backend, config) {
        Ok(wrapper) => wrapper,
        Err(err) => {
            print_error(err);
            return false;
        }
    };

    // TODO: Add a repl while the application is running to interact with parameters
    match wrapper.run() {
        Ok(()) => true,
        Err(err) => {
            print_error(err);
            false
        }
    }
}

fn print_error(error: WrapperError) {
    match error {
        WrapperError::InitializationFailed => {
            nih_error!("The plugin failed to initialize");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn a_stand_in_host_opens_the_system_default_and_keeps_the_rest() {
        let mut config = WrapperConfig::parse_from([
            "standalone",
            "-b",
            "dummy",
            "--output-device",
            "Focusrite USB ASIO",
            "--input-device",
            "Focusrite USB ASIO",
            "--period-size",
            "256",
            "--midi-input",
            "Keystation",
        ]);
        config.resolve_period();
        let stand_in = stand_in_config(&config);
        assert_eq!(stand_in.output_device, None);
        assert_eq!(stand_in.input_device, None);
        assert_eq!(stand_in.period_request, None);
        assert_eq!(stand_in.period_size, DEFAULT_PERIOD_SIZE);
        assert_eq!(stand_in.midi_input.as_deref(), Some("Keystation"));
    }
}
