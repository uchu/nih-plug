//! CLAP hosts cache parameter values, so a load the host initiates must end in
//! `clap_host_params::rescan(CLAP_PARAM_RESCAN_VALUES)`, on the main thread, once per load.
//! A load that fails changes nothing the host has to rescan.

mod support;

use clap_sys::ext::params::{
    clap_host_params, clap_param_clear_flags, clap_param_rescan_flags, CLAP_EXT_PARAMS,
    CLAP_PARAM_RESCAN_VALUES,
};
use clap_sys::ext::preset_load::{clap_plugin_preset_load, CLAP_EXT_PRESET_LOAD};
use clap_sys::factory::preset_discovery::CLAP_PRESET_DISCOVERY_LOCATION_PLUGIN;
use clap_sys::host::clap_host;
use clap_sys::id::clap_id;
use nih_plug::prelude::*;
use std::ffi::{c_char, c_void, CStr};
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use support::clap_rig::Rig;
use support::TestParams;

/// Every `rescan()` the host received: its flags and the calling thread.
#[derive(Default)]
struct HostLog {
    rescans: Mutex<Vec<(clap_param_rescan_flags, ThreadId)>>,
}

unsafe extern "C" fn host_rescan(h: *const clap_host, flags: clap_param_rescan_flags) {
    let log = &*((*h).host_data as *const HostLog);
    log.rescans
        .lock()
        .unwrap()
        .push((flags, std::thread::current().id()));
}

unsafe extern "C" fn host_clear(_h: *const clap_host, _id: clap_id, _f: clap_param_clear_flags) {}

unsafe extern "C" fn host_request_flush(_h: *const clap_host) {}

static HOST_PARAMS: clap_host_params = clap_host_params {
    rescan: Some(host_rescan),
    clear: Some(host_clear),
    request_flush: Some(host_request_flush),
};

unsafe extern "C" fn host_get_extension(_h: *const clap_host, id: *const c_char) -> *const c_void {
    if CStr::from_ptr(id) == CLAP_EXT_PARAMS {
        &HOST_PARAMS as *const _ as *const c_void
    } else {
        std::ptr::null()
    }
}

struct Host<P: ClapPlugin> {
    // Declared first so it drops before the log the host points at.
    rig: Rig<P>,
    log: Box<HostLog>,
}

// The host struct only holds static strings, function pointers and the log, and the wrapper is
// built to be called from more than one thread.
unsafe impl<P: ClapPlugin> Send for Host<P> {}
unsafe impl<P: ClapPlugin> Sync for Host<P> {}

impl<P: ClapPlugin> Host<P> {
    fn new() -> Self {
        let log = Box::<HostLog>::default();
        let rig = Rig::<P>::with_host(&*log as *const HostLog as *mut c_void, host_get_extension);
        rig.init();
        Self { rig, log }
    }

    fn rescans(&self) -> Vec<(clap_param_rescan_flags, ThreadId)> {
        self.log.rescans.lock().unwrap().clone()
    }

    /// The host's main loop servicing the queued tasks.
    fn run_main_thread_callback(&self) {
        let p = self.rig.plugin();
        unsafe { ((*p).on_main_thread.unwrap())(p) };
    }
}

/// A state saved with the gain at half its default (CLAP values are normalized: 0.5 -> 0.25).
fn changed_state() -> Vec<u8> {
    let source = Rig::new();
    source.activate(256);
    let gain = source.gain_param_id();
    source.process(
        64,
        &[support::clap_rig::param_value(gain, 0, 0.25)],
        None,
        2,
    );
    assert_eq!(source.param_value(gain), 0.25);
    source.save_state()
}

#[test]
fn a_host_state_load_rescans_values_once_inside_load_on_the_main_thread() {
    let state = changed_state();
    let host = Host::<support::TestPlugin>::new();
    let gain = host.rig.gain_param_id();
    assert_eq!(host.rig.param_value(gain), 0.5);

    assert!(host.rig.load_state(&state));

    assert_eq!(host.rig.param_value(gain), 0.25);
    assert_eq!(
        host.rescans(),
        vec![(CLAP_PARAM_RESCAN_VALUES, std::thread::current().id())]
    );
    host.run_main_thread_callback();
    assert_eq!(host.rescans().len(), 1);
}

#[test]
fn a_state_load_off_the_main_thread_rescans_on_the_main_thread() {
    let state = changed_state();
    let host = Host::<support::TestPlugin>::new();

    std::thread::scope(|s| {
        s.spawn(|| assert!(host.rig.load_state(&state)));
    });
    assert!(host.rescans().is_empty());

    host.run_main_thread_callback();
    assert_eq!(
        host.rescans(),
        vec![(CLAP_PARAM_RESCAN_VALUES, std::thread::current().id())]
    );
}

#[test]
fn a_failed_or_refused_state_load_requests_no_rescan() {
    let state = changed_state();
    let host = Host::<support::TestPlugin>::new();

    let truncated = &state[..state.len() / 2];
    assert!(!host.rig.load_state(truncated));

    let mut oversized = u64::MAX.to_le_bytes().to_vec();
    oversized.extend_from_slice(&state[8..]);
    assert!(!host.rig.load_state(&oversized));

    let garbage = b"not json";
    let mut not_json = (garbage.len() as u64).to_le_bytes().to_vec();
    not_json.extend_from_slice(garbage);
    assert!(!host.rig.load_state(&not_json));

    host.run_main_thread_callback();
    assert!(host.rescans().is_empty());
}

/// Loads the preset `half` (gain at half its range) through `clap.preset-load` and refuses any
/// other key.
struct PresetPlugin {
    params: Arc<TestParams>,
}

impl Default for PresetPlugin {
    fn default() -> Self {
        Self {
            params: Arc::new(TestParams {
                gain: FloatParam::new("Gain", 1.0, FloatRange::Linear { min: 0.0, max: 2.0 }),
            }),
        }
    }
}

impl Plugin for PresetPlugin {
    const NAME: &'static str = "Preset Load Test";
    const VENDOR: &'static str = "nih-plug fork tests";
    const URL: &'static str = "";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = "0.0.0";
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: NonZeroU32::new(2),
        main_output_channels: NonZeroU32::new(2),
        aux_input_ports: &[],
        aux_output_ports: &[],
        names: PortNames::const_default(),
    }];
    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn process(
        &mut self,
        _buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        _context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        ProcessStatus::Normal
    }
}

impl ClapPlugin for PresetPlugin {
    const CLAP_ID: &'static str = "test.preset-load";
    const CLAP_DESCRIPTION: Option<&'static str> = None;
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[ClapFeature::AudioEffect];

    fn load_preset_from_location(
        &mut self,
        _location_kind: u32,
        _location: &str,
        load_key: &str,
        context: &dyn PresetLoadContext,
    ) -> bool {
        if load_key != "half" {
            return false;
        }
        context.set_param_normalized(self.params.gain.as_ptr(), 0.25);
        true
    }
}

fn load_preset(host: &Host<PresetPlugin>, load_key: &CStr) -> bool {
    let p = host.rig.plugin();
    unsafe {
        let ext = ((*p).get_extension.unwrap())(p, CLAP_EXT_PRESET_LOAD.as_ptr())
            as *const clap_plugin_preset_load;
        ((*ext).from_location.unwrap())(
            p,
            CLAP_PRESET_DISCOVERY_LOCATION_PLUGIN,
            std::ptr::null(),
            load_key.as_ptr(),
        )
    }
}

#[test]
fn a_host_preset_load_rescans_values_once() {
    let host = Host::<PresetPlugin>::new();
    let gain = host.rig.gain_param_id();

    assert!(load_preset(&host, c"half"));

    assert_eq!(host.rig.param_value(gain), 0.25);
    assert_eq!(
        host.rescans(),
        vec![(CLAP_PARAM_RESCAN_VALUES, std::thread::current().id())]
    );
}

#[test]
fn a_refused_preset_load_requests_no_rescan() {
    let host = Host::<PresetPlugin>::new();

    assert!(!load_preset(&host, c"missing"));

    host.run_main_thread_callback();
    assert!(host.rescans().is_empty());
}
