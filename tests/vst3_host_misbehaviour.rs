mod support;

use nih_plug::wrapper::vst3::vst3::Steinberg::Vst::*;
use nih_plug::wrapper::vst3::vst3::Steinberg::*;
use nih_plug::wrapper::vst3::Wrapper;
use support::*;

fn gain_id(w: &Wrapper<TestPlugin>) -> u32 {
    let mut info: ParameterInfo = unsafe { std::mem::zeroed() };
    unsafe { assert_eq!(w.getParameterInfo(0, &mut info), kResultOk) };
    info.id
}

fn state_with_gain(normalized: f64) -> Vec<u8> {
    let w = new_wrapper();
    let id = gain_id(&w);
    unsafe {
        assert_eq!(w.setParamNormalized(id, normalized), kResultOk);
        let out = TestStream::new(Vec::new()).into_com();
        assert_eq!(IComponentTrait::getState(&w, stream_ptr(&out)), kResultOk);
        let blob = out.data.borrow().clone();
        blob
    }
}

#[test]
fn state_restores_from_a_stream_that_cannot_seek_and_reads_in_7_byte_chunks() {
    let blob = state_with_gain(0.25);
    let w = new_wrapper();
    let mut s = TestStream::new(blob);
    s.seekable = false;
    s.chunk = 7;
    let s = s.into_com();
    unsafe {
        assert_eq!(IComponentTrait::setState(&w, stream_ptr(&s)), kResultOk);
        assert!((w.getParamNormalized(gain_id(&w)) - 0.25).abs() < 1e-9);
    }
}

#[test]
fn state_restores_when_the_host_reports_eof_as_an_error() {
    let blob = state_with_gain(0.75);
    let w = new_wrapper();
    let mut s = TestStream::new(blob);
    s.eof_is_error = true;
    let s = s.into_com();
    unsafe {
        assert_eq!(IComponentTrait::setState(&w, stream_ptr(&s)), kResultOk);
        assert!((w.getParamNormalized(gain_id(&w)) - 0.75).abs() < 1e-9);
    }
}

#[test]
fn an_empty_stream_is_refused_without_panicking() {
    let w = new_wrapper();
    let s = TestStream::new(Vec::new()).into_com();
    unsafe { assert_eq!(IComponentTrait::setState(&w, stream_ptr(&s)), kResultFalse) };
}

#[test]
fn an_endless_stream_is_refused_at_the_size_ceiling() {
    let w = new_wrapper();
    let mut s = TestStream::new(Vec::new());
    s.endless = true;
    // The old code measured a seekable endless stream as 0 bytes; non-seekable pins the ceiling.
    s.seekable = false;
    let s = s.into_com();
    unsafe { assert_eq!(IComponentTrait::setState(&w, stream_ptr(&s)), kResultFalse) };
    assert_eq!(s.delivered.get(), 64 * 1024 * 1024 + 1);
}

#[test]
fn get_state_writes_to_completion_through_a_short_writing_stream() {
    let w = new_wrapper();
    let id = gain_id(&w);
    unsafe {
        // 0.875, not 0.5: 0.5 is the gain's default (1.0 in 0..2) and would pass unrestored.
        assert_eq!(w.setParamNormalized(id, 0.875), kResultOk);
        let mut s = TestStream::new(Vec::new());
        s.chunk = 5;
        let s = s.into_com();
        assert_eq!(IComponentTrait::getState(&w, stream_ptr(&s)), kResultOk);
        let blob = s.data.borrow().clone();
        assert!(blob.len() > 5);
        let w2 = new_wrapper();
        let s2 = TestStream::new(blob).into_com();
        assert_eq!(IComponentTrait::setState(&w2, stream_ptr(&s2)), kResultOk);
        assert!((w2.getParamNormalized(gain_id(&w2)) - 0.875).abs() < 1e-9);
    }
}

#[test]
fn get_state_reports_a_refusing_stream() {
    let w = new_wrapper();
    let mut s = TestStream::new(Vec::new());
    s.write_fails = true;
    let s = s.into_com();
    unsafe { assert_eq!(IComponentTrait::getState(&w, stream_ptr(&s)), kResultFalse) };
}

#[test]
fn set_state_while_another_thread_processes_does_not_panic() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    let blob = state_with_gain(0.6);
    let w = Arc::new(new_wrapper());
    unsafe { setup_and_activate(&w, 256) };
    let stop = Arc::new(AtomicBool::new(false));
    let blocks = Arc::new(AtomicUsize::new(0));
    let worker = {
        let w = w.clone();
        let stop = stop.clone();
        let blocks = blocks.clone();
        std::thread::spawn(move || {
            let mut main_in = HostBuffers::new(2, 256, 0.0);
            let mut aux_in = HostBuffers::new(2, 256, 0.0);
            let mut main_out = HostBuffers::new(2, 256, 0.0);
            while !stop.load(Ordering::Relaxed) {
                let mut inputs = [main_in.bus, aux_in.bus];
                let mut outputs = [main_out.bus];
                let mut data = process_data(256, &mut inputs, &mut outputs);
                unsafe { assert_eq!(w.process(&mut data), kResultOk) };
                blocks.fetch_add(1, Ordering::Relaxed);
            }
            let _ = (&mut main_in, &mut aux_in, &mut main_out);
        })
    };
    while blocks.load(Ordering::Relaxed) == 0 {
        std::thread::yield_now();
    }
    for _ in 0..50 {
        let s = TestStream::new(blob.clone()).into_com();
        unsafe { assert_eq!(IComponentTrait::setState(&*w, stream_ptr(&s)), kResultOk) };
    }
    stop.store(true, Ordering::Relaxed);
    worker.join().unwrap();
    // The gain is an f32 inside the plug-in: 0.6 survives the round trip to f32 precision.
    unsafe { assert!((w.getParamNormalized(gain_id(&w)) - 0.6).abs() < 1e-6) };
    assert!(blocks.load(Ordering::Relaxed) > 0);
}

fn setup_only(w: &Wrapper<TestPlugin>) {
    let mut setup = ProcessSetup {
        processMode: ProcessModes_::kRealtime as i32,
        symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
        maxSamplesPerBlock: 512,
        sampleRate: 48000.0,
    };
    unsafe { assert_eq!(w.setupProcessing(&mut setup), kResultOk) };
}

#[test]
fn process_before_setup_processing_is_not_initialized() {
    let w = new_wrapper();
    let mut outb = HostBuffers::new(2, 64, 1.0);
    let mut outputs = [outb.bus];
    let mut data = process_data(64, &mut [], &mut outputs);
    unsafe { assert_eq!(w.process(&mut data), kNotInitialized) };
    let _ = &mut outb;
}

#[test]
fn audio_before_set_active_is_silence_not_a_crash() {
    let w = new_wrapper();
    setup_only(&w);
    let outb = HostBuffers::new(2, 64, 1.0);
    let mut outputs = [outb.bus];
    let mut data = process_data(64, &mut [], &mut outputs);
    unsafe { assert_eq!(w.process(&mut data), kResultOk) };
    assert!(outb.channels[0].iter().all(|&x| x == 0.0));
    assert!(outb.channels[1].iter().all(|&x| x == 0.0));
    assert!(calls_of(&w).lock().unwrap().is_empty());
}

#[test]
fn a_zero_sample_flush_is_served_before_activation() {
    let w = new_wrapper();
    setup_only(&w);
    let mut data = process_data(0, &mut [], &mut []);
    unsafe { assert_eq!(w.process(&mut data), kResultOk) };
}

#[test]
fn audio_after_set_active_false_is_silence_again() {
    let w = new_wrapper();
    unsafe { setup_and_activate(&w, 256) };
    unsafe { assert_eq!(w.setActive(0), kResultOk) };
    calls_of(&w).lock().unwrap().clear();
    let outb = HostBuffers::new(2, 64, 1.0);
    let mut outputs = [outb.bus];
    let mut data = process_data(64, &mut [], &mut outputs);
    unsafe { assert_eq!(w.process(&mut data), kResultOk) };
    assert!(outb.channels[0].iter().all(|&x| x == 0.0));
    assert!(outb.channels[1].iter().all(|&x| x == 0.0));
    assert!(calls_of(&w).lock().unwrap().is_empty());
}

#[test]
fn a_null_output_channel_array_is_a_flush() {
    let w = new_wrapper();
    unsafe { setup_and_activate(&w, 256) };
    calls_of(&w).lock().unwrap().clear();
    let mut bus: AudioBusBuffers = unsafe { std::mem::zeroed() };
    bus.numChannels = 2;
    let mut outputs = [bus];
    let mut data = process_data(128, &mut [], &mut outputs);
    unsafe { assert_eq!(w.process(&mut data), kResultOk) };
    assert!(calls_of(&w).lock().unwrap().is_empty());
}

#[test]
fn parameter_changes_in_audio_before_activation_still_apply() {
    let w = new_wrapper();
    setup_only(&w);
    let id = gain_id(&w);
    let changes = TestParamChanges::single(id, vec![(0, 0.125)]);
    let outb = HostBuffers::new(2, 64, 1.0);
    let mut outputs = [outb.bus];
    let mut data = process_data(64, &mut [], &mut outputs);
    data.inputParameterChanges = param_changes_ptr(&changes);
    unsafe { assert_eq!(w.process(&mut data), kResultOk) };
    assert!(outb.channels[0].iter().all(|&x| x == 0.0));
    unsafe { assert!((w.getParamNormalized(id) - 0.125).abs() < 1e-6) };
    assert!(calls_of(&w).lock().unwrap().is_empty());
}

fn run_once(
    w: &Wrapper<TestPlugin>,
    inputs: &mut [AudioBusBuffers],
    outputs: &mut [AudioBusBuffers],
    n: i32,
) -> tresult {
    let mut data = process_data(n, inputs, outputs);
    unsafe { w.process(&mut data) }
}

#[test]
fn a_six_channel_host_output_bus_gets_two_channels_of_audio_and_four_of_silence() {
    let w = new_wrapper();
    unsafe { setup_and_activate(&w, 256) };
    let mut inb = HostBuffers::new(2, 64, 0.0);
    let mut outb = HostBuffers::new(6, 64, 1.0);
    let mut inputs = [inb.bus];
    let mut outputs = [outb.bus];
    assert_eq!(run_once(&w, &mut inputs, &mut outputs, 64), kResultOk);
    assert!(outb.channels[0].iter().all(|&x| x == 0.5));
    assert!(outb.channels[1].iter().all(|&x| x == 0.25));
    for ch in 2..6 {
        assert!(outb.channels[ch].iter().all(|&x| x == 0.0), "channel {ch}");
    }
    let _ = (&mut inb, &mut outb);
}

#[test]
fn a_mono_host_output_still_runs_the_plugin_and_gets_channel_0() {
    let w = new_wrapper();
    unsafe { setup_and_activate(&w, 256) };
    calls_of(&w).lock().unwrap().clear();
    let mut inb = HostBuffers::new(2, 64, 0.0);
    let mut outb = HostBuffers::new(1, 64, 1.0);
    let mut inputs = [inb.bus];
    let mut outputs = [outb.bus];
    assert_eq!(run_once(&w, &mut inputs, &mut outputs, 64), kResultOk);
    assert!(outb.channels[0].iter().all(|&x| x == 0.5));
    assert_eq!(calls_of(&w).lock().unwrap().len(), 1);
    let _ = (&mut inb, &mut outb);
}

#[test]
fn stereo_input_into_mono_output_does_not_read_past_the_host_array() {
    let w = new_wrapper();
    unsafe { setup_and_activate(&w, 256) };
    let mut inb = HostBuffers::new(2, 64, 0.3);
    let mut outb = HostBuffers::new(1, 64, 0.0);
    let mut inputs = [inb.bus];
    let mut outputs = [outb.bus];
    assert_eq!(run_once(&w, &mut inputs, &mut outputs, 64), kResultOk);
    assert!(outb.channels[0].iter().all(|&x| x == 0.5));
    let _ = (&mut inb, &mut outb);
}

#[test]
fn a_null_aux_channel_pointer_reads_as_silence() {
    let w = new_wrapper();
    unsafe { setup_and_activate(&w, 256) };
    calls_of(&w).lock().unwrap().clear();
    let mut inb = HostBuffers::new(2, 64, 0.0);
    let mut auxb = HostBuffers::new(2, 64, 0.125);
    auxb.null_channel(1);
    let mut outb = HostBuffers::new(2, 64, 0.0);
    let mut inputs = [inb.bus, auxb.bus];
    let mut outputs = [outb.bus];
    assert_eq!(run_once(&w, &mut inputs, &mut outputs, 64), kResultOk);
    // Aux channel 0 (0.125) lands on the left; the plug-in ran.
    assert!(outb.channels[0].iter().all(|&x| (x - 0.625).abs() < 1e-6));
    assert_eq!(calls_of(&w).lock().unwrap().len(), 1);
    let _ = (&mut inb, &mut auxb, &mut outb);
}

#[test]
fn a_null_main_output_channel_pointer_is_backed_by_scratch() {
    let w = new_wrapper();
    unsafe { setup_and_activate(&w, 256) };
    calls_of(&w).lock().unwrap().clear();
    let mut inb = HostBuffers::new(2, 64, 0.0);
    let mut outb = HostBuffers::new(2, 64, 1.0);
    outb.null_channel(1);
    let mut inputs = [inb.bus];
    let mut outputs = [outb.bus];
    assert_eq!(run_once(&w, &mut inputs, &mut outputs, 64), kResultOk);
    assert!(outb.channels[0].iter().all(|&x| x == 0.5));
    assert!(outb.channels[1].iter().all(|&x| x == 1.0));
    assert_eq!(calls_of(&w).lock().unwrap().len(), 1);
    let _ = (&mut inb, &mut outb);
}

#[test]
fn an_aux_bus_that_vanishes_before_a_longer_block_is_zero_for_the_whole_block() {
    let w = new_wrapper();
    unsafe { setup_and_activate(&w, 512) };
    let mut inb = HostBuffers::new(2, 512, 0.0);
    let mut auxb = HostBuffers::new(2, 256, 0.125);
    let mut outb = HostBuffers::new(2, 512, 0.0);
    {
        let mut inputs = [inb.bus, auxb.bus];
        let mut outputs = [outb.bus];
        assert_eq!(run_once(&w, &mut inputs, &mut outputs, 256), kResultOk);
    }
    calls_of(&w).lock().unwrap().clear();
    let mut inputs = [inb.bus];
    let mut outputs = [outb.bus];
    assert_eq!(run_once(&w, &mut inputs, &mut outputs, 512), kResultOk);
    assert!(outb.channels[0].iter().all(|&x| x == 0.5));
    // H6: the aux slice is as long as the block (the old code left it at 256).
    assert_eq!(calls_of(&w).lock().unwrap().last().unwrap().aux_len, 512);
    let _ = (&mut inb, &mut auxb, &mut outb);
}

#[test]
fn a_zero_channel_main_output_is_a_flush() {
    // Ableton Live's flush: samples, but zero channels on the output bus.
    let w = new_wrapper();
    unsafe { setup_and_activate(&w, 256) };
    calls_of(&w).lock().unwrap().clear();
    let mut inb = HostBuffers::new(2, 64, 0.0);
    let mut outb = HostBuffers::new(0, 64, 0.0);
    let mut inputs = [inb.bus];
    let mut outputs = [outb.bus];
    assert_eq!(run_once(&w, &mut inputs, &mut outputs, 64), kResultOk);
    assert!(calls_of(&w).lock().unwrap().is_empty());
    let _ = (&mut inb, &mut outb);
}

#[test]
fn a_4096_block_against_max_512_is_eight_blocks_with_the_note_in_the_right_one() {
    use ProcessContext_::StatesAndFlags_::*;
    let w = new_wrapper();
    unsafe { setup_and_activate(&w, 512) };
    calls_of(&w).lock().unwrap().clear();
    let events = TestEventList::new(vec![note_on(60, 1500)]).into_com();
    let mut inb = HostBuffers::new(2, 4096, 0.0);
    let outb = HostBuffers::new(2, 4096, 0.0);
    let mut inputs = [inb.bus];
    let mut outputs = [outb.bus];
    let mut data = process_data(4096, &mut inputs, &mut outputs);
    data.inputEvents = event_list_ptr(&events);
    // 120 bpm in 4/4 from beat 3.9: every size split must advance `pos_beats` and recompute the
    // bar start, not only sample-accurate ones (H5). Block 5 crosses into the bar at beat 4.
    let mut ctx: ProcessContext = unsafe { std::mem::zeroed() };
    ctx.state = kTempoValid | kProjectTimeMusicValid | kBarPositionValid | kTimeSigValid;
    ctx.sampleRate = 48000.0;
    ctx.tempo = 120.0;
    ctx.timeSigNumerator = 4;
    ctx.timeSigDenominator = 4;
    ctx.projectTimeMusic = 3.9;
    ctx.barPositionMusic = 0.0;
    ctx.projectTimeSamples = 0;
    data.processContext = &mut ctx;
    unsafe { assert_eq!(w.process(&mut data), kResultOk) };
    let calls = calls_of(&w).lock().unwrap().clone();
    assert_eq!(calls.len(), 8);
    assert!(calls.iter().all(|c| c.samples == 512));
    assert_eq!(calls[2].notes, vec![(60, 476)]);
    assert!(calls
        .iter()
        .enumerate()
        .all(|(i, c)| c.notes.is_empty() || i == 2));
    assert_eq!(calls[1].pos_samples, Some(512));
    let beats_per_block = 512.0 / 48000.0 / 60.0 * 120.0;
    assert!((calls[1].pos_beats.unwrap() - (3.9 + beats_per_block)).abs() < 1e-9);
    assert!((calls[7].pos_beats.unwrap() - (3.9 + 7.0 * beats_per_block)).abs() < 1e-9);
    assert_eq!(calls[0].bar_start_pos_beats, Some(0.0));
    assert_eq!(calls[7].bar_start_pos_beats, Some(4.0));
    assert!(outb.channels[0].iter().all(|&s| s == 0.5));
    let _ = &mut inb;
}

#[test]
fn a_bigger_max_announced_while_active_still_splits_by_the_allocated_size() {
    let w = new_wrapper();
    unsafe { setup_and_activate(&w, 512) };
    let mut setup = ProcessSetup {
        processMode: ProcessModes_::kRealtime as i32,
        symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
        maxSamplesPerBlock: 4096,
        sampleRate: 48000.0,
    };
    unsafe { assert_eq!(w.setupProcessing(&mut setup), kResultOk) };
    calls_of(&w).lock().unwrap().clear();
    let inb = HostBuffers::new(2, 4096, 0.0);
    let outb = HostBuffers::new(2, 4096, 0.0);
    let mut inputs = [inb.bus];
    let mut outputs = [outb.bus];
    assert_eq!(run_once(&w, &mut inputs, &mut outputs, 4096), kResultOk);
    let calls = calls_of(&w).lock().unwrap().clone();
    assert_eq!(calls.len(), 8);
    assert!(calls.iter().all(|c| c.samples == 512));
    assert!(outb.channels[0].iter().all(|&s| s == 0.5));
}

#[test]
fn a_negative_sample_count_is_a_flush() {
    let w = new_wrapper();
    unsafe { setup_and_activate(&w, 256) };
    calls_of(&w).lock().unwrap().clear();
    let mut outb = HostBuffers::new(2, 64, 0.0);
    let mut outputs = [outb.bus];
    let mut data = process_data(-5, &mut [], &mut outputs);
    data.numInputs = -3;
    unsafe { assert_eq!(w.process(&mut data), kResultOk) };
    assert!(calls_of(&w).lock().unwrap().is_empty());
    let _ = &mut outb;
}

#[test]
fn a_negative_output_count_is_a_flush() {
    let w = new_wrapper();
    unsafe { setup_and_activate(&w, 256) };
    calls_of(&w).lock().unwrap().clear();
    let mut outb = HostBuffers::new(2, 64, 0.0);
    let mut outputs = [outb.bus];
    let mut data = process_data(64, &mut [], &mut outputs);
    data.numOutputs = -1;
    unsafe { assert_eq!(w.process(&mut data), kResultOk) };
    assert!(calls_of(&w).lock().unwrap().is_empty());
    let _ = &mut outb;
}

#[test]
fn a_negative_max_block_size_still_activates_and_processes() {
    let w = new_wrapper();
    let mut setup = ProcessSetup {
        processMode: ProcessModes_::kRealtime as i32,
        symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
        maxSamplesPerBlock: -1,
        sampleRate: 48000.0,
    };
    unsafe {
        assert_eq!(w.setupProcessing(&mut setup), kResultOk);
        assert_eq!(w.setActive(1), kResultOk);
        assert_eq!(w.setProcessing(1), kResultOk);
    }
    assert_eq!(INIT_MAX_BUFFER.with(|m| m.get()), Some(1));
    calls_of(&w).lock().unwrap().clear();
    let mut inb = HostBuffers::new(2, 4, 0.0);
    let outb = HostBuffers::new(2, 4, 0.0);
    let mut inputs = [inb.bus];
    let mut outputs = [outb.bus];
    assert_eq!(run_once(&w, &mut inputs, &mut outputs, 4), kResultOk);
    assert!(outb.channels[0].iter().all(|&x| x == 0.5));
    assert!(calls_of(&w).lock().unwrap().iter().all(|c| c.samples == 1));
    let _ = &mut inb;
}

#[test]
fn a_negative_event_offset_lands_on_the_first_sample() {
    let w = new_wrapper();
    unsafe { setup_and_activate(&w, 256) };
    calls_of(&w).lock().unwrap().clear();
    let events = TestEventList::new(vec![note_on(60, -1)]).into_com();
    let mut inb = HostBuffers::new(2, 64, 0.0);
    let mut outb = HostBuffers::new(2, 64, 0.0);
    let mut inputs = [inb.bus];
    let mut outputs = [outb.bus];
    let mut data = process_data(64, &mut inputs, &mut outputs);
    data.inputEvents = event_list_ptr(&events);
    unsafe { assert_eq!(w.process(&mut data), kResultOk) };
    let calls = calls_of(&w).lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].notes, vec![(60, 0)]);
    let _ = (&mut inb, &mut outb);
}

fn sysex_event(size: u32, bytes: *const u8) -> Event {
    let mut e: Event = unsafe { std::mem::zeroed() };
    e.r#type = Event_::EventTypes_::kDataEvent as u16;
    e.__field0.data = DataEvent {
        size,
        r#type: 0,
        bytes,
    };
    e
}

#[test]
fn a_sysex_event_with_null_bytes_is_ignored() {
    let w = new_wrapper();
    unsafe { setup_and_activate(&w, 256) };
    calls_of(&w).lock().unwrap().clear();
    let events =
        TestEventList::new(vec![sysex_event(3, std::ptr::null()), note_on(60, 10)]).into_com();
    let mut inb = HostBuffers::new(2, 64, 0.0);
    let mut outb = HostBuffers::new(2, 64, 0.0);
    let mut inputs = [inb.bus];
    let mut outputs = [outb.bus];
    let mut data = process_data(64, &mut inputs, &mut outputs);
    data.inputEvents = event_list_ptr(&events);
    unsafe { assert_eq!(w.process(&mut data), kResultOk) };
    let calls = calls_of(&w).lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].notes, vec![(60, 10)]);
    let _ = (&mut inb, &mut outb);
}

#[test]
fn a_sysex_event_with_zero_size_is_ignored() {
    let w = new_wrapper();
    unsafe { setup_and_activate(&w, 256) };
    calls_of(&w).lock().unwrap().clear();
    let bytes = [0x90u8, 60, 100];
    let events = TestEventList::new(vec![sysex_event(0, bytes.as_ptr())]).into_com();
    let mut inb = HostBuffers::new(2, 64, 0.0);
    let mut outb = HostBuffers::new(2, 64, 0.0);
    let mut inputs = [inb.bus];
    let mut outputs = [outb.bus];
    let mut data = process_data(64, &mut inputs, &mut outputs);
    data.inputEvents = event_list_ptr(&events);
    unsafe { assert_eq!(w.process(&mut data), kResultOk) };
    let calls = calls_of(&w).lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].notes.is_empty());
    let _ = (&mut inb, &mut outb);
}

#[test]
fn a_negative_channel_count_on_the_main_output_is_a_flush() {
    let w = new_wrapper();
    unsafe { setup_and_activate(&w, 256) };
    calls_of(&w).lock().unwrap().clear();
    let mut outb = HostBuffers::new(2, 64, 1.0);
    outb.bus.numChannels = -1;
    let mut outputs = [outb.bus];
    let mut data = process_data(64, &mut [], &mut outputs);
    unsafe { assert_eq!(w.process(&mut data), kResultOk) };
    assert!(calls_of(&w).lock().unwrap().is_empty());
    let _ = &mut outb;
}

#[test]
fn a_negative_channel_count_on_an_aux_input_is_zero_channels() {
    let w = new_wrapper();
    unsafe { setup_and_activate(&w, 256) };
    calls_of(&w).lock().unwrap().clear();
    let mut inb = HostBuffers::new(2, 64, 0.0);
    let mut auxb = HostBuffers::new(2, 64, 0.125);
    auxb.bus.numChannels = -1;
    let mut outb = HostBuffers::new(2, 64, 0.0);
    let mut inputs = [inb.bus, auxb.bus];
    let mut outputs = [outb.bus];
    assert_eq!(run_once(&w, &mut inputs, &mut outputs, 64), kResultOk);
    // Zero channels, not `usize::MAX` of them: the aux reads as silence.
    assert!(outb.channels[0].iter().all(|&x| x == 0.5));
    assert_eq!(calls_of(&w).lock().unwrap().len(), 1);
    let _ = (&mut inb, &mut auxb, &mut outb);
}

#[test]
fn a_negative_channel_count_on_the_main_input_is_no_input() {
    let w = new_wrapper();
    unsafe { setup_and_activate(&w, 256) };
    calls_of(&w).lock().unwrap().clear();
    // No channel memory at all behind a bus that claims -1 channels.
    let mut inb = HostBuffers::new(0, 64, 0.0);
    inb.bus.numChannels = -1;
    let mut outb = HostBuffers::new(2, 64, 1.0);
    let mut inputs = [inb.bus];
    let mut outputs = [outb.bus];
    assert_eq!(run_once(&w, &mut inputs, &mut outputs, 64), kResultOk);
    assert!(outb.channels[0].iter().all(|&x| x == 0.5));
    assert_eq!(calls_of(&w).lock().unwrap().len(), 1);
    let _ = (&mut inb, &mut outb);
}

#[test]
fn an_event_the_host_fails_to_hand_over_is_skipped() {
    let w = new_wrapper();
    unsafe { setup_and_activate(&w, 256) };
    calls_of(&w).lock().unwrap().clear();
    let mut list = TestEventList::new(vec![note_on(60, 10)]);
    list.phantom = 1;
    let events = list.into_com();
    let mut inb = HostBuffers::new(2, 64, 0.0);
    let mut outb = HostBuffers::new(2, 64, 0.0);
    let mut inputs = [inb.bus];
    let mut outputs = [outb.bus];
    let mut data = process_data(64, &mut inputs, &mut outputs);
    data.inputEvents = event_list_ptr(&events);
    unsafe { assert_eq!(w.process(&mut data), kResultOk) };
    let calls = calls_of(&w).lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].notes, vec![(60, 10)]);
    let _ = (&mut inb, &mut outb);
}
