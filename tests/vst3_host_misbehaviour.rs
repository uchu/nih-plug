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
