mod support;

use clap_sys::events::*;
use clap_sys::ext::state::{clap_plugin_state, CLAP_EXT_STATE};
use clap_sys::fixedpoint::CLAP_BEATTIME_FACTOR;
use clap_sys::process::*;
use clap_sys::stream::clap_istream;
use std::cell::Cell;
use std::ffi::c_void;
use support::clap_rig::{header, note_on, param_value, transport_at, Ev, Rig};
use support::*;

/// Beats covered by `samples` at 120 BPM and 48 kHz.
fn beats_in(samples: usize) -> f64 {
    samples as f64 / 48000.0 / 60.0 * 120.0
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
    assert_eq!(INIT_MAX_BUFFER.with(|m| m.get()), Some(1));
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

#[test]
fn a_sysex_event_with_a_null_buffer_is_ignored() {
    let rig = Rig::new();
    rig.activate(512);
    let sysex = Ev::SysEx(clap_event_midi_sysex {
        header: header::<clap_event_midi_sysex>(0, CLAP_EVENT_MIDI_SYSEX),
        port_index: 0,
        buffer: std::ptr::null(),
        size: 4,
    });
    let status = rig.process(256, &[sysex, note_on(60, 10)], None, 2);
    assert_ne!(status, CLAP_PROCESS_ERROR);
    assert_eq!(calls().len(), 1);
    assert_eq!(calls()[0].notes, vec![(60, 10)]);
}

#[test]
fn params_get_info_one_past_the_end_is_refused() {
    use clap_sys::ext::params::{clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS};
    let rig = Rig::new();
    let p = rig.plugin();
    unsafe {
        assert!(((*p).init.unwrap())(p));
        let ext =
            ((*p).get_extension.unwrap())(p, CLAP_EXT_PARAMS.as_ptr()) as *const clap_plugin_params;
        assert!(!ext.is_null());
        let count = ((*ext).count.unwrap())(p);
        let mut info: clap_param_info = std::mem::zeroed();
        assert!(((*ext).get_info.unwrap())(p, count - 1, &mut info));
        assert!(!((*ext).get_info.unwrap())(p, count, &mut info));
    }
}

#[test]
fn size_splits_keep_the_hosts_bar_lines() {
    let rig = Rig::new();
    rig.activate(512);
    let mut t = transport_at(0, 4.0);
    t.bar_start = 3 * CLAP_BEATTIME_FACTOR;
    t.bar_number = 1;
    rig.process(1024, &[], Some(&t), 2);
    let bars: Vec<Option<f64>> = calls().iter().map(|c| c.bar_start_pos_beats).collect();
    assert_eq!(bars, vec![Some(3.0), Some(3.0)]);
    let numbers: Vec<Option<i32>> = calls().iter().map(|c| c.bar_number).collect();
    assert_eq!(numbers, vec![Some(1), Some(1)]);
}

#[test]
fn a_size_split_past_the_hosts_bar_line_counts_the_bars_it_crossed() {
    let rig = Rig::new();
    rig.activate(512);
    // Bar 1 runs from beat 3 to 7; the second block starts 512 samples past beat 6.99.
    let mut t = transport_at(0, 6.99);
    t.bar_start = 3 * CLAP_BEATTIME_FACTOR;
    t.bar_number = 1;
    rig.process(1024, &[], Some(&t), 2);
    let bars: Vec<Option<f64>> = calls().iter().map(|c| c.bar_start_pos_beats).collect();
    assert_eq!(bars, vec![Some(3.0), Some(7.0)]);
    let numbers: Vec<Option<i32>> = calls().iter().map(|c| c.bar_number).collect();
    assert_eq!(numbers, vec![Some(1), Some(2)]);
}

#[test]
fn process_after_deactivate_is_refused_like_before_activate() {
    let rig = Rig::new();
    rig.activate(512);
    let p = rig.plugin();
    unsafe {
        ((*p).stop_processing.unwrap())(p);
        ((*p).deactivate.unwrap())(p);
    }
    calls_of_any().lock().unwrap().clear();
    assert_eq!(rig.process(64, &[], None, 2), CLAP_PROCESS_ERROR);
    assert!(calls().is_empty());
}

#[test]
fn an_absurd_maximum_block_size_is_capped() {
    let rig = Rig::new();
    rig.activate(u32::MAX);
    assert_eq!(INIT_MAX_BUFFER.with(|m| m.get()), Some(1 << 16));
}

// Deliberate change from upstream: the first event in the queue is held to the same rule as
// every other one. Upstream delivered it at sample 0 whatever its time.
#[test]
fn a_first_queued_parameter_change_after_sample_0_takes_effect_at_its_own_sample() {
    let rig = Rig::<SaaTestPlugin>::build();
    rig.activate(512);
    let gain = rig.gain_param_id();
    // nih-plug hands CLAP the normalized range: 0.75 is a gain of 1.5
    rig.process(100, &[param_value(gain, 50, 0.75)], None, 2);
    let calls = calls();
    let lens: Vec<usize> = calls.iter().map(|c| c.samples).collect();
    assert_eq!(lens, vec![50, 50]);
    let gains: Vec<f32> = calls.iter().map(|c| c.gain).collect();
    assert_eq!(gains, vec![1.0, 1.5]);
}

#[test]
fn a_first_queued_transport_event_after_sample_0_splits_there_and_is_the_origin() {
    let rig = Rig::new();
    rig.activate(512);
    let start = transport_at(0, 4.0);
    let jump = Ev::Transport(transport_at(50, 16.0));
    rig.process(1024, &[jump], Some(&start), 2);
    let calls = calls();
    let lens: Vec<usize> = calls.iter().map(|c| c.samples).collect();
    assert_eq!(lens, vec![50, 512, 462]);
    let beats: Vec<f64> = calls.iter().map(|c| c.pos_beats.unwrap()).collect();
    let expected = [4.0, 16.0, 16.0 + beats_in(512)];
    for (got, want) in beats.iter().zip(expected) {
        assert!((got - want).abs() < 1e-9, "{beats:?} vs {expected:?}");
    }
}

#[test]
fn a_capped_activation_never_announces_a_minimum_above_the_maximum() {
    let rig = Rig::new();
    let p = rig.plugin();
    unsafe {
        assert!(((*p).init.unwrap())(p));
        assert!(((*p).activate.unwrap())(p, 48000.0, 131_072, 131_072));
    }
    let max = INIT_MAX_BUFFER.with(|m| m.get()).unwrap();
    let min = INIT_MIN_BUFFER.with(|m| m.get()).unwrap();
    assert!(
        min.map_or(true, |min| min <= max),
        "min {min:?} > max {max}"
    );
}

#[test]
fn a_legit_131072_frame_offline_block_is_split_not_refused() {
    let rig = Rig::new();
    rig.activate(131_072);
    let start = transport_at(0, 0.0);
    let status = rig.process(131_072, &[note_on(60, 100_000)], Some(&start), 2);
    assert_ne!(status, CLAP_PROCESS_ERROR);
    let calls = calls();
    let lens: Vec<usize> = calls.iter().map(|c| c.samples).collect();
    assert_eq!(lens, vec![65_536, 65_536]);
    assert_eq!(calls[1].notes, vec![(60, 100_000 - 65_536)]);
    assert!((calls[1].pos_beats.unwrap() - beats_in(65_536)).abs() < 1e-9);
}

#[test]
fn a_note_queued_before_a_parameter_at_the_same_sample_stays_inside_its_block() {
    let rig = Rig::<SaaTestPlugin>::build();
    rig.activate(512);
    let gain = rig.gain_param_id();
    rig.process(
        512,
        &[note_on(60, 100), param_value(gain, 100, 0.75)],
        None,
        2,
    );
    let got: Vec<(usize, Vec<u32>)> = calls()
        .iter()
        .map(|c| (c.samples, c.event_timings.clone()))
        .collect();
    assert_eq!(got, vec![(100, vec![]), (412, vec![0])]);
}

#[test]
fn a_note_queued_before_a_transport_event_at_the_same_sample_stays_inside_its_block() {
    let rig = Rig::new();
    rig.activate(512);
    let start = transport_at(0, 0.0);
    let jump = Ev::Transport(transport_at(100, 16.0));
    rig.process(512, &[note_on(60, 100), jump], Some(&start), 2);
    let got: Vec<(usize, Vec<u32>)> = calls()
        .iter()
        .map(|c| (c.samples, c.event_timings.clone()))
        .collect();
    assert_eq!(got, vec![(100, vec![]), (412, vec![0])]);
}
