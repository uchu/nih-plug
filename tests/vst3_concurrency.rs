//! Host calls racing a busy audio thread: lifecycle calls on another thread, and flushes a host
//! sends from its UI thread while a process call is still running.

mod support;

use nih_plug::prelude::Vst3Plugin;
use nih_plug::wrapper::vst3::vst3::Steinberg::Vst::*;
use nih_plug::wrapper::vst3::vst3::Steinberg::*;
use nih_plug::wrapper::vst3::Wrapper;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;
use support::*;

fn gain_id<P: Vst3Plugin>(w: &Wrapper<P>) -> u32 {
    let mut info: ParameterInfo = unsafe { std::mem::zeroed() };
    unsafe { assert_eq!(w.getParameterInfo(0, &mut info), kResultOk) };
    info.id
}

fn saa_state(normalized: f64) -> Vec<u8> {
    let w = new_saa_wrapper();
    let id = gain_id(&w);
    unsafe {
        assert_eq!(w.setParamNormalized(id, normalized), kResultOk);
        let out = TestStream::new(Vec::new()).into_com();
        assert_eq!(IComponentTrait::getState(&w, stream_ptr(&out)), kResultOk);
        let blob = out.data.borrow().clone();
        blob
    }
}

fn setup(w: &Wrapper<SaaTestPlugin>, max: i32) {
    let mut setup = ProcessSetup {
        processMode: ProcessModes_::kRealtime as i32,
        symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
        maxSamplesPerBlock: max,
        sampleRate: 48000.0,
    };
    unsafe { assert_eq!(w.setupProcessing(&mut setup), kResultOk) };
}

/// Audio thread: SAA param changes, notes, output events, transport, oversized blocks and
/// flushes. UI thread: every lifecycle call the wrapper takes the plug-in lock in. A deadlock
/// shows up as the watchdog firing.
#[test]
fn lifecycle_calls_against_a_busy_audio_thread_never_deadlock() {
    use ProcessContext_::StatesAndFlags_::*;
    let blob = saa_state(0.6);
    let w = Arc::new(new_saa_wrapper());
    let gain = gain_id(&*w);
    unsafe { setup_and_activate(&*w, 512) };
    let calls = calls_of_any();
    let stop = Arc::new(AtomicBool::new(false));
    let blocks = Arc::new(AtomicUsize::new(0));
    let (audio_tx, audio_rx) = mpsc::channel();
    let audio = {
        let w = w.clone();
        let stop = stop.clone();
        let blocks = blocks.clone();
        std::thread::spawn(move || {
            let changes = TestParamChanges::single(
                gain,
                vec![(0, 0.1), (700, 0.2), (1500, 0.3), (3000, 0.4)],
            );
            let events =
                TestEventList::new(vec![note_on(60, 100), note_on(61, 1600), note_on(62, 4000)])
                    .into_com();
            let out_events = TestEventList::new(vec![]).into_com();
            let mut main_in = HostBuffers::new(2, 4096, 0.0);
            let mut aux_in = HostBuffers::new(2, 4096, 0.0);
            let mut main_out = HostBuffers::new(2, 4096, 0.0);
            let mut ctx: ProcessContext = unsafe { std::mem::zeroed() };
            ctx.state =
                kTempoValid | kProjectTimeMusicValid | kBarPositionValid | kTimeSigValid | kPlaying;
            ctx.sampleRate = 48000.0;
            ctx.tempo = 120.0;
            ctx.timeSigNumerator = 4;
            ctx.timeSigDenominator = 4;
            ctx.projectTimeMusic = 3.9;
            while !stop.load(Ordering::Relaxed) {
                let mut inputs = [main_in.bus, aux_in.bus];
                let mut outputs = [main_out.bus];
                let mut data = process_data(4096, &mut inputs, &mut outputs);
                data.inputParameterChanges = param_changes_ptr(&changes);
                data.inputEvents = event_list_ptr(&events);
                data.outputEvents = event_list_ptr(&out_events);
                data.processContext = &mut ctx;
                unsafe { assert_eq!(w.process(&mut data), kResultOk) };
                // A correctly placed flush, on the audio thread between blocks
                let mut flush = process_data(0, &mut [], &mut []);
                flush.inputParameterChanges = param_changes_ptr(&changes);
                unsafe { assert_eq!(w.process(&mut flush), kResultOk) };
                blocks.fetch_add(1, Ordering::Relaxed);
            }
            let _ = (&mut main_in, &mut aux_in, &mut main_out);
            audio_tx.send(()).unwrap();
        })
    };
    while blocks.load(Ordering::Relaxed) == 0 {
        std::thread::yield_now();
    }
    let (ui_tx, ui_rx) = mpsc::channel();
    let ui = {
        let w = w.clone();
        std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            let mut i = 0usize;
            while t0.elapsed() < Duration::from_secs(2) {
                i += 1;
                unsafe {
                    match i % 7 {
                        0 => {
                            assert_eq!(w.setActive(0), kResultOk);
                            assert_eq!(w.setActive(1), kResultOk);
                        }
                        1 => {
                            assert_eq!(w.setProcessing(0), kResultOk);
                            assert_eq!(w.setProcessing(1), kResultOk);
                        }
                        2 => {
                            let s = TestStream::new(blob.clone()).into_com();
                            assert_eq!(IComponentTrait::setState(&*w, stream_ptr(&s)), kResultOk);
                        }
                        3 => {
                            setup(&w, if i % 2 == 0 { 256 } else { 1024 });
                            assert_eq!(w.setActive(0), kResultOk);
                            assert_eq!(w.setActive(1), kResultOk);
                        }
                        4 => {
                            let out = TestStream::new(Vec::new()).into_com();
                            assert_eq!(IComponentTrait::getState(&*w, stream_ptr(&out)), kResultOk);
                        }
                        5 => {
                            // setupProcessing while active, no reactivation
                            setup(&w, if i % 2 == 0 { 128 } else { 2048 });
                        }
                        _ => {
                            assert_eq!(w.setParamNormalized(gain, 0.3), kResultOk);
                        }
                    }
                }
                if i % 50 == 0 {
                    calls.lock().unwrap().clear();
                }
            }
            ui_tx.send(()).unwrap();
        })
    };
    let ui_done = ui_rx.recv_timeout(Duration::from_secs(60));
    stop.store(true, Ordering::Relaxed);
    let audio_done = audio_rx.recv_timeout(Duration::from_secs(30));
    assert!(ui_done.is_ok(), "UI thread stuck: deadlock");
    assert!(audio_done.is_ok(), "audio thread stuck: deadlock");
    ui.join().unwrap();
    audio.join().unwrap();
    assert!(blocks.load(Ordering::Relaxed) > 10);
}

/// A flush the host sends from its UI thread while the audio thread is inside `process`.
#[test]
fn a_ui_thread_flush_during_process_does_not_panic() {
    let w = Arc::new(new_wrapper());
    let gain = gain_id(&*w);
    unsafe { setup_and_activate(&*w, 512) };
    let calls = calls_of_any();
    let stop = Arc::new(AtomicBool::new(false));
    let blocks = Arc::new(AtomicUsize::new(0));
    let audio = {
        let w = w.clone();
        let stop = stop.clone();
        let blocks = blocks.clone();
        std::thread::spawn(move || {
            let mut main_in = HostBuffers::new(2, 4096, 0.0);
            let mut aux_in = HostBuffers::new(2, 4096, 0.0);
            let mut main_out = HostBuffers::new(2, 4096, 0.0);
            while !stop.load(Ordering::Relaxed) {
                let mut inputs = [main_in.bus, aux_in.bus];
                let mut outputs = [main_out.bus];
                let mut data = process_data(4096, &mut inputs, &mut outputs);
                unsafe { assert_eq!(w.process(&mut data), kResultOk) };
                blocks.fetch_add(1, Ordering::Relaxed);
            }
            let _ = (&mut main_in, &mut aux_in, &mut main_out);
        })
    };
    while blocks.load(Ordering::Relaxed) == 0 {
        std::thread::yield_now();
    }
    let flushes = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let changes = TestParamChanges::single(gain, vec![(0, 0.25)]);
        for i in 0..20000 {
            let mut flush = process_data(0, &mut [], &mut []);
            flush.inputParameterChanges = param_changes_ptr(&changes);
            unsafe { assert_eq!(w.process(&mut flush), kResultOk) };
            if i % 100 == 0 {
                calls.lock().unwrap().clear();
            }
        }
    }));
    stop.store(true, Ordering::Relaxed);
    let audio = audio.join();
    assert!(flushes.is_ok(), "the UI-thread flush panicked");
    assert!(audio.is_ok(), "process panicked during a UI-thread flush");
}
