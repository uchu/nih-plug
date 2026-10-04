//! A minimal CLAP host for the CLAP wrapper tests.

use super::{calls_of_any, HostBuffers, TestPlugin};
use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::events::*;
use clap_sys::fixedpoint::CLAP_BEATTIME_FACTOR;
use clap_sys::host::clap_host;
use clap_sys::plugin::clap_plugin;
use clap_sys::process::*;
use clap_sys::version::CLAP_VERSION;
use nih_plug::prelude::ClapPlugin;
use nih_plug::wrapper::clap::Wrapper;
use std::ffi::{c_char, c_void};
use std::sync::Arc;

unsafe extern "C" fn host_get_extension(_h: *const clap_host, _id: *const c_char) -> *const c_void {
    std::ptr::null()
}

unsafe extern "C" fn host_noop(_h: *const clap_host) {}

/// One host-side input event of either kind the tests send.
pub enum Ev {
    Note(clap_event_note),
    Transport(clap_event_transport),
    SysEx(clap_event_midi_sysex),
    Param(clap_event_param_value),
}

impl Ev {
    pub fn header(&self) -> *const clap_event_header {
        match self {
            Ev::Note(e) => &e.header,
            Ev::Transport(e) => &e.header,
            Ev::SysEx(e) => &e.header,
            Ev::Param(e) => &e.header,
        }
    }
}

unsafe fn events_of<'a>(list: *const clap_input_events) -> &'a [Ev] {
    *((*list).ctx as *const &[Ev])
}

unsafe extern "C" fn ev_size(list: *const clap_input_events) -> u32 {
    events_of(list).len() as u32
}

unsafe extern "C" fn ev_get(list: *const clap_input_events, i: u32) -> *const clap_event_header {
    match events_of(list).get(i as usize) {
        Some(ev) => ev.header(),
        None => std::ptr::null(),
    }
}

unsafe extern "C" fn out_try_push(
    _l: *const clap_output_events,
    _e: *const clap_event_header,
) -> bool {
    true
}

pub fn header<T>(time: u32, type_: u16) -> clap_event_header {
    clap_event_header {
        size: std::mem::size_of::<T>() as u32,
        time,
        space_id: CLAP_CORE_EVENT_SPACE_ID,
        type_,
        flags: 0,
    }
}

pub fn note_on(key: i16, time: u32) -> Ev {
    Ev::Note(clap_event_note {
        header: header::<clap_event_note>(time, CLAP_EVENT_NOTE_ON),
        note_id: -1,
        port_index: 0,
        channel: 0,
        key,
        velocity: 0.8,
    })
}

pub fn param_value(param_id: u32, time: u32, value: f64) -> Ev {
    Ev::Param(clap_event_param_value {
        header: header::<clap_event_param_value>(time, CLAP_EVENT_PARAM_VALUE),
        param_id,
        cookie: std::ptr::null_mut(),
        note_id: -1,
        port_index: -1,
        channel: -1,
        key: -1,
        value,
    })
}

/// 120 BPM in 4/4, playing, at `beats` on the timeline.
pub fn transport_at(time: u32, beats: f64) -> clap_event_transport {
    clap_event_transport {
        header: header::<clap_event_transport>(time, CLAP_EVENT_TRANSPORT),
        flags: CLAP_TRANSPORT_HAS_TEMPO
            | CLAP_TRANSPORT_HAS_BEATS_TIMELINE
            | CLAP_TRANSPORT_HAS_TIME_SIGNATURE
            | CLAP_TRANSPORT_IS_PLAYING,
        song_pos_beats: (beats * CLAP_BEATTIME_FACTOR as f64) as i64,
        song_pos_seconds: 0,
        tempo: 120.0,
        tempo_inc: 0.0,
        loop_start_beats: 0,
        loop_end_beats: 0,
        loop_start_seconds: 0,
        loop_end_seconds: 0,
        bar_start: 0,
        bar_number: 0,
        tsig_num: 4,
        tsig_denom: 4,
    }
}

pub struct Rig<P: ClapPlugin = TestPlugin> {
    // Declared first so it drops before the host it was handed.
    pub wrapper: Arc<Wrapper<P>>,
    _host: Box<clap_host>,
}

impl Rig {
    pub fn new() -> Self {
        Self::build()
    }
}

impl<P: ClapPlugin> Rig<P> {
    pub fn build() -> Self {
        let host = Box::new(clap_host {
            clap_version: CLAP_VERSION,
            host_data: std::ptr::null_mut(),
            name: c"test host".as_ptr(),
            vendor: c"nih-plug fork tests".as_ptr(),
            url: c"".as_ptr(),
            version: c"0.0.0".as_ptr(),
            get_extension: Some(host_get_extension),
            request_restart: Some(host_noop),
            request_process: Some(host_noop),
            request_callback: Some(host_noop),
        });
        let wrapper = unsafe { Wrapper::<P>::new(&*host) };
        Self {
            wrapper,
            _host: host,
        }
    }

    pub fn plugin(&self) -> *const clap_plugin {
        self.wrapper.clap_plugin.as_ptr()
    }

    /// The CLAP id of the plug-in's one real parameter, the gain.
    pub fn gain_param_id(&self) -> u32 {
        use clap_sys::ext::params::{clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS};
        let p = self.plugin();
        unsafe {
            let ext = ((*p).get_extension.unwrap())(p, CLAP_EXT_PARAMS.as_ptr())
                as *const clap_plugin_params;
            let mut info: clap_param_info = std::mem::zeroed();
            assert!(((*ext).get_info.unwrap())(p, 0, &mut info));
            info.id
        }
    }

    pub fn activate(&self, max_frames: u32) {
        let p = self.plugin();
        unsafe {
            assert!(((*p).init.unwrap())(p));
            assert!(((*p).activate.unwrap())(p, 48000.0, 1, max_frames));
            assert!(((*p).start_processing.unwrap())(p));
        }
        calls_of_any().lock().unwrap().clear();
    }

    /// One `process` call with a stereo main input and a main output of `out_channels`.
    pub fn process(
        &self,
        frames: u32,
        events: &[Ev],
        transport: Option<&clap_event_transport>,
        out_channels: u32,
    ) -> clap_process_status {
        let mut inb = HostBuffers::new(2, frames as usize, 0.0);
        let mut out = HostBuffers::new(2, frames as usize, 0.0);
        let inputs = [clap_audio_buffer {
            data32: inb.ptrs.as_mut_ptr(),
            data64: std::ptr::null_mut(),
            channel_count: 2,
            latency: 0,
            constant_mask: 0,
        }];
        let mut outputs = [clap_audio_buffer {
            data32: out.ptrs.as_mut_ptr(),
            data64: std::ptr::null_mut(),
            channel_count: out_channels,
            latency: 0,
            constant_mask: 0,
        }];
        let events: &[Ev] = events;
        let in_events = clap_input_events {
            ctx: &events as *const &[Ev] as *mut c_void,
            size: Some(ev_size),
            get: Some(ev_get),
        };
        let out_events = clap_output_events {
            ctx: std::ptr::null_mut(),
            try_push: Some(out_try_push),
        };
        let process = clap_process {
            steady_time: 0,
            frames_count: frames,
            transport: transport.map_or(std::ptr::null(), |t| t as *const _),
            audio_inputs: inputs.as_ptr(),
            audio_outputs: outputs.as_mut_ptr(),
            audio_inputs_count: 1,
            audio_outputs_count: 1,
            in_events: &in_events,
            out_events: &out_events,
        };
        let p = self.plugin();
        unsafe { ((*p).process.unwrap())(p, &process) }
    }
}
