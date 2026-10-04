mod support;

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::events::*;
use clap_sys::ext::state::{clap_plugin_state, CLAP_EXT_STATE};
use clap_sys::fixedpoint::CLAP_BEATTIME_FACTOR;
use clap_sys::host::clap_host;
use clap_sys::plugin::clap_plugin;
use clap_sys::process::*;
use clap_sys::stream::clap_istream;
use clap_sys::version::CLAP_VERSION;
use nih_plug::wrapper::clap::Wrapper;
use std::cell::Cell;
use std::ffi::{c_char, c_void};
use std::sync::Arc;
use support::*;

unsafe extern "C" fn host_get_extension(_h: *const clap_host, _id: *const c_char) -> *const c_void {
    std::ptr::null()
}

unsafe extern "C" fn host_noop(_h: *const clap_host) {}

/// One host-side input event of either kind the tests send.
enum Ev {
    Note(clap_event_note),
    Transport(clap_event_transport),
}

impl Ev {
    fn header(&self) -> *const clap_event_header {
        match self {
            Ev::Note(e) => &e.header,
            Ev::Transport(e) => &e.header,
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

fn header<T>(time: u32, type_: u16) -> clap_event_header {
    clap_event_header {
        size: std::mem::size_of::<T>() as u32,
        time,
        space_id: CLAP_CORE_EVENT_SPACE_ID,
        type_,
        flags: 0,
    }
}

fn note_on(key: i16, time: u32) -> Ev {
    Ev::Note(clap_event_note {
        header: header::<clap_event_note>(time, CLAP_EVENT_NOTE_ON),
        note_id: -1,
        port_index: 0,
        channel: 0,
        key,
        velocity: 0.8,
    })
}

/// 120 BPM in 4/4, playing, at `beats` on the timeline.
fn transport_at(time: u32, beats: f64) -> clap_event_transport {
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

/// Beats covered by `samples` at 120 BPM and 48 kHz.
fn beats_in(samples: usize) -> f64 {
    samples as f64 / 48000.0 / 60.0 * 120.0
}

struct Rig {
    // Declared first so it drops before the host it was handed.
    wrapper: Arc<Wrapper<TestPlugin>>,
    _host: Box<clap_host>,
}

impl Rig {
    fn new() -> Self {
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
        let wrapper = unsafe { Wrapper::<TestPlugin>::new(&*host) };
        Self {
            wrapper,
            _host: host,
        }
    }

    fn plugin(&self) -> *const clap_plugin {
        self.wrapper.clap_plugin.as_ptr()
    }

    fn activate(&self, max_frames: u32) {
        let p = self.plugin();
        unsafe {
            assert!(((*p).init.unwrap())(p));
            assert!(((*p).activate.unwrap())(p, 48000.0, 1, max_frames));
            assert!(((*p).start_processing.unwrap())(p));
        }
        calls_of_any().lock().unwrap().clear();
    }

    /// One `process` call with a stereo main input and a main output of `out_channels`.
    fn process(
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

fn calls() -> Vec<Call> {
    calls_of_any().lock().unwrap().clone()
}

#[test]
fn process_before_activate_is_an_error_not_a_crash() {
    let rig = Rig::new();
    assert_eq!(rig.process(64, &[], None, 2), CLAP_PROCESS_ERROR);
    assert!(calls().is_empty());
}

#[test]
fn a_4096_frame_block_against_max_512_is_eight_calls_with_the_note_in_the_right_one() {
    let rig = Rig::new();
    rig.activate(512);
    let status = rig.process(4096, &[note_on(60, 1500)], None, 2);
    assert_ne!(status, CLAP_PROCESS_ERROR);
    let calls = calls();
    assert_eq!(calls.len(), 8);
    assert!(calls.iter().all(|c| c.samples == 512));
    assert_eq!(calls[2].notes, vec![(60, 476)]);
    assert!(calls
        .iter()
        .enumerate()
        .all(|(i, c)| c.notes.is_empty() || i == 2));
}

#[test]
fn an_early_note_in_an_oversized_block_is_delivered_once() {
    let rig = Rig::new();
    rig.activate(512);
    rig.process(1536, &[note_on(60, 100), note_on(62, 600)], None, 2);
    let calls = calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].notes, vec![(60, 100)]);
    assert_eq!(calls[1].notes, vec![(62, 88)]);
    assert!(calls[2].notes.is_empty());
}

#[test]
fn size_splits_advance_the_transport() {
    let rig = Rig::new();
    rig.activate(512);
    let transport = transport_at(0, 4.0);
    rig.process(1024, &[], Some(&transport), 2);
    let calls = calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].pos_beats, Some(4.0));
    let second = calls[1].pos_beats.unwrap();
    assert!((second - (4.0 + beats_in(512))).abs() < 1e-9, "{second}");
}

#[test]
fn a_mid_block_transport_event_is_the_origin_for_later_splits() {
    let rig = Rig::new();
    rig.activate(512);
    let start = transport_at(0, 4.0);
    let jump = Ev::Transport(transport_at(600, 16.0));
    rig.process(1536, &[jump], Some(&start), 2);
    let calls = calls();
    let lens: Vec<usize> = calls.iter().map(|c| c.samples).collect();
    assert_eq!(lens, vec![512, 88, 512, 424]);
    let beats: Vec<f64> = calls.iter().map(|c| c.pos_beats.unwrap()).collect();
    let expected = [4.0, 4.0 + beats_in(512), 16.0, 16.0 + beats_in(512)];
    for (got, want) in beats.iter().zip(expected) {
        assert!((got - want).abs() < 1e-9, "{beats:?} vs {expected:?}");
    }
}

#[test]
fn a_zero_channel_main_output_is_a_flush() {
    let rig = Rig::new();
    rig.activate(512);
    let status = rig.process(256, &[note_on(60, 10)], None, 0);
    assert_ne!(status, CLAP_PROCESS_ERROR);
    assert!(calls().is_empty());
}

struct PrefixStream {
    prefix: [u8; 8],
    handed_out: Cell<usize>,
    reads_after_prefix: Cell<usize>,
}

unsafe extern "C" fn read_prefix(
    stream: *const clap_istream,
    buffer: *mut c_void,
    size: u64,
) -> i64 {
    let s = &*((*stream).ctx as *const PrefixStream);
    let pos = s.handed_out.get();
    if pos >= s.prefix.len() {
        s.reads_after_prefix.set(s.reads_after_prefix.get() + 1);
        return 0;
    }
    let n = (s.prefix.len() - pos).min(size as usize);
    std::ptr::copy_nonoverlapping(s.prefix.as_ptr().add(pos), buffer as *mut u8, n);
    s.handed_out.set(pos + n);
    n as i64
}

fn load_with_prefix(length: u64) -> (bool, usize) {
    let rig = Rig::new();
    let p = rig.plugin();
    let stream_state = PrefixStream {
        prefix: length.to_le_bytes(),
        handed_out: Cell::new(0),
        reads_after_prefix: Cell::new(0),
    };
    let stream = clap_istream {
        ctx: &stream_state as *const PrefixStream as *mut c_void,
        read: Some(read_prefix),
    };
    let loaded = unsafe {
        assert!(((*p).init.unwrap())(p));
        let ext =
            ((*p).get_extension.unwrap())(p, CLAP_EXT_STATE.as_ptr()) as *const clap_plugin_state;
        assert!(!ext.is_null());
        ((*ext).load.unwrap())(p, &stream)
    };
    (loaded, stream_state.reads_after_prefix.get())
}

#[test]
fn a_state_length_prefix_past_the_ceiling_is_refused() {
    let (loaded, reads_after_prefix) = load_with_prefix(64 * 1024 * 1024 + 1);
    assert!(!loaded);
    assert_eq!(reads_after_prefix, 0, "the body must not be read at all");
}

#[test]
fn a_state_length_prefix_of_u64_max_is_refused() {
    let (loaded, reads_after_prefix) = load_with_prefix(u64::MAX);
    assert!(!loaded);
    assert_eq!(reads_after_prefix, 0);
}

#[test]
fn a_zero_max_frames_activation_still_processes_in_bounds() {
    let rig = Rig::new();
    rig.activate(0);
    rig.process(4, &[], None, 2);
    let lens: Vec<usize> = calls().iter().map(|c| c.samples).collect();
    assert_eq!(lens, vec![1, 1, 1, 1]);
    assert!(calls().iter().all(|c| c.aux_len == 1));
}

#[test]
fn a_note_past_the_buffer_lands_on_the_last_sample_of_the_last_block() {
    let rig = Rig::new();
    rig.activate(512);
    rig.process(1024, &[note_on(60, 5000)], None, 2);
    let calls = calls();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].notes.is_empty());
    assert_eq!(calls[1].notes, vec![(60, 511)]);
}

#[test]
fn a_transport_event_past_the_buffer_never_stretches_a_block() {
    let rig = Rig::new();
    rig.activate(512);
    let start = transport_at(0, 4.0);
    let late = Ev::Transport(transport_at(5000, 16.0));
    rig.process(1024, &[late], Some(&start), 2);
    let lens: Vec<usize> = calls().iter().map(|c| c.samples).collect();
    assert_eq!(lens, vec![512, 512]);
}
