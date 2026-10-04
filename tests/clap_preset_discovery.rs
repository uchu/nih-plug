//! The CLAP preset-discovery provider obeys its metadata receiver: once `begin_preset()`
//! returns false the provider calls nothing more on it, and a preset whose name or load key
//! cannot cross the C ABI is skipped rather than announced under an empty key.

mod support;

use clap_sys::factory::preset_discovery::*;
use clap_sys::universal_plugin_id::clap_universal_plugin_id;
use clap_sys::version::CLAP_VERSION;
use nih_plug::prelude::*;
use nih_plug::wrapper::clap::PresetDiscoveryFactory;
use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_void, CStr};
use std::num::NonZeroU32;
use std::sync::Arc;
use support::TestParams;

struct Discovery;

impl ClapPresetDiscovery for Discovery {
    fn provider_id(&self) -> &str {
        "test.discovery.provider"
    }

    fn provider_name(&self) -> &str {
        "Discovery Test"
    }

    fn provider_vendor(&self) -> &str {
        "nih-plug fork tests"
    }

    fn enumerate_presets(&self) -> Vec<ClapPresetEntry> {
        [
            ("One", "one"),
            ("Bad\0Name", "bad-name"),
            ("Bad Key", "bad\0key"),
            ("Two", "two"),
        ]
        .into_iter()
        .map(|(name, load_key)| ClapPresetEntry {
            name: name.to_owned(),
            load_key: load_key.to_owned(),
            creator: Some("Creator".to_owned()),
            description: Some("Description".to_owned()),
            flags: CLAP_PRESET_DISCOVERY_IS_FACTORY_CONTENT,
        })
        .collect()
    }
}

struct DiscoveryPlugin {
    params: Arc<TestParams>,
}

impl Default for DiscoveryPlugin {
    fn default() -> Self {
        Self {
            params: Arc::new(TestParams {
                gain: FloatParam::new("Gain", 1.0, FloatRange::Linear { min: 0.0, max: 2.0 }),
            }),
        }
    }
}

impl Plugin for DiscoveryPlugin {
    const NAME: &'static str = "Preset Discovery Test";
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

impl ClapPlugin for DiscoveryPlugin {
    const CLAP_ID: &'static str = "test.preset-discovery";
    const CLAP_DESCRIPTION: Option<&'static str> = None;
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[ClapFeature::AudioEffect];

    fn clap_preset_discovery() -> Option<Box<dyn ClapPresetDiscovery>> {
        Some(Box::new(Discovery))
    }
}

/// Every receiver call in order. `begin_preset` accepts `accepted` presets, then refuses.
struct ReceiverLog {
    accepted: Cell<usize>,
    calls: RefCell<Vec<String>>,
}

unsafe fn log_of<'a>(r: *const clap_preset_discovery_metadata_receiver) -> &'a ReceiverLog {
    &*((*r).receiver_data as *const ReceiverLog)
}

unsafe fn text(s: *const c_char) -> String {
    CStr::from_ptr(s).to_string_lossy().into_owned()
}

unsafe extern "C" fn on_error(
    r: *const clap_preset_discovery_metadata_receiver,
    _os_error: i32,
    _message: *const c_char,
) {
    log_of(r).calls.borrow_mut().push("on_error".to_owned());
}

unsafe extern "C" fn begin_preset(
    r: *const clap_preset_discovery_metadata_receiver,
    name: *const c_char,
    load_key: *const c_char,
) -> bool {
    let log = log_of(r);
    log.calls
        .borrow_mut()
        .push(format!("begin {}/{}", text(name), text(load_key)));
    match log.accepted.get() {
        0 => false,
        n => {
            log.accepted.set(n - 1);
            true
        }
    }
}

unsafe extern "C" fn add_plugin_id(
    r: *const clap_preset_discovery_metadata_receiver,
    _id: *const clap_universal_plugin_id,
) {
    log_of(r).calls.borrow_mut().push("plugin_id".to_owned());
}

unsafe extern "C" fn set_flags(r: *const clap_preset_discovery_metadata_receiver, _flags: u32) {
    log_of(r).calls.borrow_mut().push("flags".to_owned());
}

unsafe extern "C" fn add_creator(
    r: *const clap_preset_discovery_metadata_receiver,
    _creator: *const c_char,
) {
    log_of(r).calls.borrow_mut().push("creator".to_owned());
}

unsafe extern "C" fn set_description(
    r: *const clap_preset_discovery_metadata_receiver,
    _description: *const c_char,
) {
    log_of(r).calls.borrow_mut().push("description".to_owned());
}

unsafe extern "C" fn declare_location(
    _i: *const clap_preset_discovery_indexer,
    _location: *const clap_preset_discovery_location,
) -> bool {
    true
}

/// Runs the provider's `get_metadata()` against a receiver accepting `accepted` presets and
/// returns what the receiver saw.
fn crawl(accepted: usize) -> Vec<String> {
    let indexer = clap_preset_discovery_indexer {
        clap_version: CLAP_VERSION,
        name: c"test indexer".as_ptr(),
        vendor: c"nih-plug fork tests".as_ptr(),
        url: c"".as_ptr(),
        version: c"0.0.0".as_ptr(),
        indexer_data: std::ptr::null_mut(),
        declare_filetype: None,
        declare_location: Some(declare_location),
        declare_soundpack: None,
        get_extension: None,
    };
    let log = ReceiverLog {
        accepted: Cell::new(accepted),
        calls: RefCell::new(Vec::new()),
    };
    let receiver = clap_preset_discovery_metadata_receiver {
        receiver_data: &log as *const ReceiverLog as *mut c_void,
        on_error: Some(on_error),
        begin_preset: Some(begin_preset),
        add_plugin_id: Some(add_plugin_id),
        set_soundpack_id: None,
        set_flags: Some(set_flags),
        add_creator: Some(add_creator),
        set_description: Some(set_description),
        set_timestamps: None,
        add_feature: None,
        add_extra_info: None,
    };
    let factory = PresetDiscoveryFactory::<DiscoveryPlugin>::factory();
    unsafe {
        let provider =
            (factory.create.unwrap())(factory, &indexer, c"test.discovery.provider".as_ptr());
        assert!(!provider.is_null());
        assert!(((*provider).init.unwrap())(provider));
        assert!(((*provider).get_metadata.unwrap())(
            provider,
            CLAP_PRESET_DISCOVERY_LOCATION_PLUGIN,
            std::ptr::null(),
            &receiver,
        ));
        ((*provider).destroy.unwrap())(provider);
    }
    log.calls.into_inner()
}

fn preset(name_and_key: &str) -> Vec<String> {
    [
        format!("begin {name_and_key}"),
        "plugin_id".to_owned(),
        "flags".to_owned(),
        "creator".to_owned(),
        "description".to_owned(),
    ]
    .into()
}

#[test]
fn every_preset_that_can_cross_the_c_abi_is_announced_and_none_under_an_empty_key() {
    let calls = crawl(usize::MAX);

    assert_eq!(calls, [preset("One/one"), preset("Two/two")].concat());
}

#[test]
fn a_refused_begin_preset_stops_the_crawl() {
    let calls = crawl(0);

    assert_eq!(calls, vec!["begin One/one".to_owned()]);
}

#[test]
fn a_refusal_after_accepted_presets_stops_the_crawl_there() {
    let calls = crawl(1);

    assert_eq!(
        calls,
        [preset("One/one"), vec!["begin Two/two".to_owned()]].concat()
    );
}
