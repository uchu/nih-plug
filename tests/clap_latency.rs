//! CLAP allows `clap_host_latency::changed()` only inside `clap_plugin::activate()` (on the main
//! thread). A plug-in whose latency moves while active must ask for a restart instead, and the
//! reactivation is where the new value is announced.

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::events::*;
use clap_sys::ext::latency::{clap_host_latency, clap_plugin_latency, CLAP_EXT_LATENCY};
use clap_sys::ext::params::{clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS};
use clap_sys::ext::state::{clap_plugin_state, CLAP_EXT_STATE};
use clap_sys::host::clap_host;
use clap_sys::plugin::clap_plugin;
use clap_sys::process::*;
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::version::CLAP_VERSION;
use nih_plug::prelude::*;
use nih_plug::wrapper::clap::Wrapper;
use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_void, CStr};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

const LOOKAHEAD: u32 = 64;

#[derive(Params)]
struct LatencyParams {
    #[id = "lookahead"]
    lookahead: BoolParam,
}

/// Reports `LOOKAHEAD` samples of latency while its one switch is on, from `initialize()` and
/// from `process()` alike, the way a limiter with a bypassable look-ahead does.
struct LatencyPlugin {
    params: Arc<LatencyParams>,
}

impl Default for LatencyPlugin {
    fn default() -> Self {
        Self {
            params: Arc::new(LatencyParams {
                lookahead: BoolParam::new("Look-ahead", false),
            }),
        }
    }
}

impl LatencyPlugin {
    fn latency(&self) -> u32 {
        if self.params.lookahead.value() {
            LOOKAHEAD
        } else {
            0
        }
    }
}

impl Plugin for LatencyPlugin {
    const NAME: &'static str = "Latency Test";
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

    fn initialize(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        _buffer_config: &BufferConfig,
        context: &mut impl InitContext<Self>,
    ) -> bool {
        context.set_latency_samples(self.latency());
        true
    }

    fn process(
        &mut self,
        _buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        context.set_latency_samples(self.latency());
        ProcessStatus::Normal
    }
}

impl ClapPlugin for LatencyPlugin {
    const CLAP_ID: &'static str = "test.latency";
    const CLAP_DESCRIPTION: Option<&'static str> = None;
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[ClapFeature::AudioEffect];
}

/// What the host saw, reached through `clap_host::host_data`.
#[derive(Default)]
struct HostLog {
    in_activate: AtomicBool,
    changed_in_activate: AtomicU32,
    changed_elsewhere: AtomicU32,
    restart_requests: AtomicU32,
    callback_requested: AtomicBool,
}

unsafe fn log_of<'a>(h: *const clap_host) -> &'a HostLog {
    &*((*h).host_data as *const HostLog)
}

unsafe extern "C" fn host_latency_changed(h: *const clap_host) {
    let log = log_of(h);
    if log.in_activate.load(Ordering::SeqCst) {
        log.changed_in_activate.fetch_add(1, Ordering::SeqCst);
    } else {
        log.changed_elsewhere.fetch_add(1, Ordering::SeqCst);
    }
}

static HOST_LATENCY: clap_host_latency = clap_host_latency {
    changed: Some(host_latency_changed),
};

unsafe extern "C" fn host_get_extension(_h: *const clap_host, id: *const c_char) -> *const c_void {
    if CStr::from_ptr(id) == CLAP_EXT_LATENCY {
        &HOST_LATENCY as *const _ as *const c_void
    } else {
        std::ptr::null()
    }
}

unsafe extern "C" fn host_request_restart(h: *const clap_host) {
    log_of(h).restart_requests.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn host_request_callback(h: *const clap_host) {
    log_of(h).callback_requested.store(true, Ordering::SeqCst);
}

unsafe extern "C" fn host_noop(_h: *const clap_host) {}

unsafe fn events_of<'a>(list: *const clap_input_events) -> &'a [clap_event_param_value] {
    &*((*list).ctx as *const Vec<clap_event_param_value>)
}

unsafe extern "C" fn ev_size(list: *const clap_input_events) -> u32 {
    events_of(list).len() as u32
}

unsafe extern "C" fn ev_get(list: *const clap_input_events, i: u32) -> *const clap_event_header {
    match events_of(list).get(i as usize) {
        Some(ev) => &ev.header,
        None => std::ptr::null(),
    }
}

unsafe extern "C" fn write_state(
    stream: *const clap_ostream,
    buffer: *const c_void,
    size: u64,
) -> i64 {
    let bytes = &*((*stream).ctx as *const RefCell<Vec<u8>>);
    let chunk = std::slice::from_raw_parts(buffer as *const u8, size as usize);
    bytes.borrow_mut().extend_from_slice(chunk);
    size as i64
}

struct StateReader {
    bytes: Vec<u8>,
    pos: Cell<usize>,
}

unsafe extern "C" fn read_state(
    stream: *const clap_istream,
    buffer: *mut c_void,
    size: u64,
) -> i64 {
    let reader = &*((*stream).ctx as *const StateReader);
    let pos = reader.pos.get();
    let n = (reader.bytes.len() - pos).min(size as usize);
    std::ptr::copy_nonoverlapping(reader.bytes.as_ptr().add(pos), buffer as *mut u8, n);
    reader.pos.set(pos + n);
    n as i64
}

unsafe extern "C" fn out_try_push(
    _l: *const clap_output_events,
    _e: *const clap_event_header,
) -> bool {
    true
}

struct Host {
    // Declared first so it drops before the host it was handed.
    wrapper: Arc<Wrapper<LatencyPlugin>>,
    _host: Box<clap_host>,
    log: Box<HostLog>,
}

/// The plug-in pointer, handed to the thread playing the audio thread.
#[derive(Clone, Copy)]
struct PluginPtr(*const clap_plugin);
unsafe impl Send for PluginPtr {}

impl Host {
    fn new() -> Self {
        let log = Box::<HostLog>::default();
        let host = Box::new(clap_host {
            clap_version: CLAP_VERSION,
            host_data: &*log as *const HostLog as *mut c_void,
            name: c"latency test host".as_ptr(),
            vendor: c"nih-plug fork tests".as_ptr(),
            url: c"".as_ptr(),
            version: c"0.0.0".as_ptr(),
            get_extension: Some(host_get_extension),
            request_restart: Some(host_request_restart),
            request_process: Some(host_noop),
            request_callback: Some(host_request_callback),
        });
        let wrapper = unsafe { Wrapper::<LatencyPlugin>::new(&*host) };
        let this = Self {
            wrapper,
            _host: host,
            log,
        };
        let p = this.plugin();
        unsafe { assert!(((*p).init.unwrap())(p)) };
        this
    }

    fn plugin(&self) -> *const clap_plugin {
        self.wrapper.clap_plugin.as_ptr()
    }

    fn activate(&self) {
        let p = self.plugin();
        self.log.in_activate.store(true, Ordering::SeqCst);
        unsafe { assert!(((*p).activate.unwrap())(p, 48000.0, 1, 512)) };
        self.log.in_activate.store(false, Ordering::SeqCst);
    }

    fn deactivate(&self) {
        let p = self.plugin();
        unsafe { ((*p).deactivate.unwrap())(p) };
    }

    fn latency(&self) -> u32 {
        let p = self.plugin();
        unsafe {
            let ext = ((*p).get_extension.unwrap())(p, CLAP_EXT_LATENCY.as_ptr())
                as *const clap_plugin_latency;
            ((*ext).get.unwrap())(p)
        }
    }

    fn switch_param_id(&self) -> u32 {
        let p = self.plugin();
        unsafe {
            let ext = ((*p).get_extension.unwrap())(p, CLAP_EXT_PARAMS.as_ptr())
                as *const clap_plugin_params;
            let mut info: clap_param_info = std::mem::zeroed();
            assert!(((*ext).get_info.unwrap())(p, 0, &mut info));
            info.id
        }
    }

    fn state_ext(&self) -> *const clap_plugin_state {
        let p = self.plugin();
        unsafe {
            ((*p).get_extension.unwrap())(p, CLAP_EXT_STATE.as_ptr()) as *const clap_plugin_state
        }
    }

    fn save_state(&self) -> Vec<u8> {
        let bytes = RefCell::new(Vec::new());
        let stream = clap_ostream {
            ctx: &bytes as *const RefCell<Vec<u8>> as *mut c_void,
            write: Some(write_state),
        };
        unsafe { assert!(((*self.state_ext()).save.unwrap())(self.plugin(), &stream)) };
        bytes.into_inner()
    }

    fn load_state(&self, bytes: Vec<u8>) {
        let reader = StateReader {
            bytes,
            pos: Cell::new(0),
        };
        let stream = clap_istream {
            ctx: &reader as *const StateReader as *mut c_void,
            read: Some(read_state),
        };
        unsafe { assert!(((*self.state_ext()).load.unwrap())(self.plugin(), &stream)) };
    }

    /// The host's main loop servicing a `request_callback()`.
    fn run_main_thread_callback(&self) {
        if self.log.callback_requested.swap(false, Ordering::SeqCst) {
            let p = self.plugin();
            unsafe { ((*p).on_main_thread.unwrap())(p) };
        }
    }

    /// `start_processing`, a few blocks that turn the look-ahead on, `stop_processing`, all on
    /// a thread other than the one that created the instance.
    fn play_on_audio_thread(&self, lookahead: bool) {
        let p = PluginPtr(self.plugin());
        let param_id = self.switch_param_id();
        std::thread::spawn(move || unsafe {
            let p = p;
            let p = p.0;
            assert!(((*p).start_processing.unwrap())(p));
            for block in 0..4 {
                let events: Vec<clap_event_param_value> = if block == 0 {
                    vec![clap_event_param_value {
                        header: clap_event_header {
                            size: std::mem::size_of::<clap_event_param_value>() as u32,
                            time: 0,
                            space_id: CLAP_CORE_EVENT_SPACE_ID,
                            type_: CLAP_EVENT_PARAM_VALUE,
                            flags: 0,
                        },
                        param_id,
                        cookie: std::ptr::null_mut(),
                        note_id: -1,
                        port_index: -1,
                        channel: -1,
                        key: -1,
                        value: if lookahead { 1.0 } else { 0.0 },
                    }]
                } else {
                    Vec::new()
                };
                let mut chans = [vec![0.0f32; 256], vec![0.0f32; 256]];
                let mut ptrs: Vec<*mut f32> = chans.iter_mut().map(|c| c.as_mut_ptr()).collect();
                let inputs = [clap_audio_buffer {
                    data32: ptrs.as_mut_ptr(),
                    data64: std::ptr::null_mut(),
                    channel_count: 2,
                    latency: 0,
                    constant_mask: 0,
                }];
                let mut outputs = [clap_audio_buffer {
                    data32: ptrs.as_mut_ptr(),
                    data64: std::ptr::null_mut(),
                    channel_count: 2,
                    latency: 0,
                    constant_mask: 0,
                }];
                let in_events = clap_input_events {
                    ctx: &events as *const _ as *mut c_void,
                    size: Some(ev_size),
                    get: Some(ev_get),
                };
                let out_events = clap_output_events {
                    ctx: std::ptr::null_mut(),
                    try_push: Some(out_try_push),
                };
                let process = clap_process {
                    steady_time: block * 256,
                    frames_count: 256,
                    transport: std::ptr::null(),
                    audio_inputs: inputs.as_ptr(),
                    audio_outputs: outputs.as_mut_ptr(),
                    audio_inputs_count: 1,
                    audio_outputs_count: 1,
                    in_events: &in_events,
                    out_events: &out_events,
                };
                assert_ne!(((*p).process.unwrap())(p, &process), CLAP_PROCESS_ERROR);
            }
            ((*p).stop_processing.unwrap())(p);
        })
        .join()
        .unwrap();
    }
}

#[test]
fn a_latency_change_while_active_requests_a_restart_and_never_calls_changed_outside_activate() {
    let host = Host::new();
    host.activate();
    assert_eq!(host.latency(), 0);

    host.play_on_audio_thread(true);
    host.run_main_thread_callback();

    assert_eq!(host.log.changed_elsewhere.load(Ordering::SeqCst), 0);
    assert_eq!(host.log.restart_requests.load(Ordering::SeqCst), 1);
    host.deactivate();
}

#[test]
fn the_reactivation_after_a_restart_announces_the_new_latency_inside_activate() {
    let host = Host::new();
    host.activate();
    host.play_on_audio_thread(true);
    host.run_main_thread_callback();
    let announced_before = host.log.changed_in_activate.load(Ordering::SeqCst);

    host.deactivate();
    host.activate();

    assert_eq!(
        host.log.changed_in_activate.load(Ordering::SeqCst),
        announced_before + 1
    );
    assert_eq!(host.log.changed_elsewhere.load(Ordering::SeqCst), 0);
    assert_eq!(host.latency(), LOOKAHEAD);
    host.deactivate();
}

#[test]
fn an_unchanged_reactivation_announces_nothing_and_requests_no_restart() {
    let host = Host::new();
    host.activate();
    host.deactivate();
    host.activate();

    assert_eq!(host.log.changed_in_activate.load(Ordering::SeqCst), 0);
    assert_eq!(host.log.changed_elsewhere.load(Ordering::SeqCst), 0);
    assert_eq!(host.log.restart_requests.load(Ordering::SeqCst), 0);
    host.deactivate();
}

#[test]
fn a_latency_change_while_inactive_waits_for_the_next_activate() {
    let source = Host::new();
    source.activate();
    source.play_on_audio_thread(true);
    source.deactivate();
    let state = source.save_state();

    let host = Host::new();
    host.activate();
    host.deactivate();
    host.load_state(state);
    host.run_main_thread_callback();

    assert_eq!(host.latency(), LOOKAHEAD);
    assert_eq!(host.log.changed_elsewhere.load(Ordering::SeqCst), 0);
    assert_eq!(host.log.restart_requests.load(Ordering::SeqCst), 0);
    assert_eq!(host.log.changed_in_activate.load(Ordering::SeqCst), 0);

    host.activate();
    assert_eq!(host.log.changed_in_activate.load(Ordering::SeqCst), 1);
    assert_eq!(host.log.changed_elsewhere.load(Ordering::SeqCst), 0);
    host.deactivate();
}

#[test]
fn a_latency_initialize_reports_is_announced_without_a_restart_request() {
    let source = Host::new();
    source.activate();
    source.play_on_audio_thread(true);
    source.deactivate();
    let state = source.save_state();

    let host = Host::new();
    host.load_state(state);
    host.run_main_thread_callback();
    host.activate();

    assert_eq!(host.latency(), LOOKAHEAD);
    assert_eq!(host.log.changed_in_activate.load(Ordering::SeqCst), 1);
    assert_eq!(host.log.changed_elsewhere.load(Ordering::SeqCst), 0);
    assert_eq!(host.log.restart_requests.load(Ordering::SeqCst), 0);
    host.deactivate();
}
