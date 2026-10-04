//! Lifecycle calls racing a busy audio thread. A lock-order regression aborts the process (a
//! panic cannot unwind out of an `extern "C"` CLAP entry point), so this is a binary of its own.

mod support;

use clap_sys::ext::state::{clap_plugin_state, CLAP_EXT_STATE};
use clap_sys::stream::{clap_istream, clap_ostream};
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use support::clap_rig::{note_on, param_value, transport_at, Ev, Rig};
use support::*;

struct SharedRig(Rig<SaaTestPlugin>);

// The host struct only holds static strings and function pointers, and the wrapper is built to
// be called from the host's audio and main threads at once.
unsafe impl Send for SharedRig {}
unsafe impl Sync for SharedRig {}

fn state_ext(rig: &Rig<SaaTestPlugin>) -> *const clap_plugin_state {
    let p = rig.plugin();
    unsafe { ((*p).get_extension.unwrap())(p, CLAP_EXT_STATE.as_ptr()) as *const _ }
}

struct Sink(RefCell<Vec<u8>>);

unsafe extern "C" fn sink_write(s: *const clap_ostream, buf: *const c_void, size: u64) -> i64 {
    let sink = &*((*s).ctx as *const Sink);
    let bytes = std::slice::from_raw_parts(buf as *const u8, size as usize);
    sink.0.borrow_mut().extend_from_slice(bytes);
    size as i64
}

fn save_state(rig: &Rig<SaaTestPlugin>) -> Vec<u8> {
    let sink = Sink(RefCell::new(Vec::new()));
    let stream = clap_ostream {
        ctx: &sink as *const Sink as *mut c_void,
        write: Some(sink_write),
    };
    let p = rig.plugin();
    unsafe { assert!(((*state_ext(rig)).save.unwrap())(p, &stream)) };
    sink.0.into_inner()
}

struct Source {
    data: Vec<u8>,
    pos: Cell<usize>,
}

unsafe extern "C" fn source_read(s: *const clap_istream, buf: *mut c_void, size: u64) -> i64 {
    let src = &*((*s).ctx as *const Source);
    let pos = src.pos.get();
    let n = (src.data.len() - pos).min(size as usize);
    std::ptr::copy_nonoverlapping(src.data.as_ptr().add(pos), buf as *mut u8, n);
    src.pos.set(pos + n);
    n as i64
}

fn load_state(rig: &Rig<SaaTestPlugin>, data: Vec<u8>) {
    let src = Source {
        data,
        pos: Cell::new(0),
    };
    let stream = clap_istream {
        ctx: &src as *const Source as *mut c_void,
        read: Some(source_read),
    };
    let p = rig.plugin();
    unsafe { assert!(((*state_ext(rig)).load.unwrap())(p, &stream)) };
}

/// Audio thread: sample-accurate parameter events, notes, a mid-block transport event and
/// blocks longer than the activated maximum. Main thread (the instance's creator, so
/// `is_main_thread()` holds): reactivation, state load and save, reset. A deadlock shows up as
/// the watchdog aborting.
#[test]
fn lifecycle_calls_against_a_busy_audio_thread_never_deadlock() {
    let shared = Arc::new(SharedRig(Rig::<SaaTestPlugin>::build()));
    let rig = &shared.0;
    rig.activate(512);
    let p = rig.plugin();
    let gain = rig.gain_param_id();
    let blob = save_state(rig);
    let calls = calls_of_any();
    let stop = Arc::new(AtomicBool::new(false));
    let blocks = Arc::new(AtomicUsize::new(0));
    let (audio_tx, audio_rx) = mpsc::channel();
    let audio = {
        let shared = shared.clone();
        let stop = stop.clone();
        let blocks = blocks.clone();
        std::thread::spawn(move || {
            let start = transport_at(0, 3.9);
            let events = [
                param_value(gain, 0, 0.1),
                note_on(60, 100),
                param_value(gain, 700, 0.2),
                param_value(gain, 1500, 0.3),
                note_on(61, 1600),
                Ev::Transport(transport_at(2000, 16.0)),
                param_value(gain, 3000, 0.4),
                note_on(62, 4000),
            ];
            while !stop.load(Ordering::Relaxed) {
                shared.0.process(4096, &events, Some(&start), 2);
                blocks.fetch_add(1, Ordering::Relaxed);
            }
            audio_tx.send(()).unwrap();
        })
    };
    while blocks.load(Ordering::Relaxed) == 0 {
        std::thread::yield_now();
    }

    let (main_done_tx, main_done_rx) = mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if main_done_rx.recv_timeout(Duration::from_secs(60)).is_err() {
            eprintln!("the main thread is stuck: deadlock");
            std::process::abort();
        }
    });
    let t0 = Instant::now();
    let mut i = 0usize;
    while t0.elapsed() < Duration::from_secs(2) {
        i += 1;
        unsafe {
            match i % 5 {
                0 => {
                    ((*p).stop_processing.unwrap())(p);
                    ((*p).deactivate.unwrap())(p);
                    let max_frames = if i % 2 == 0 { 256 } else { 1024 };
                    assert!(((*p).activate.unwrap())(p, 48000.0, 1, max_frames));
                    assert!(((*p).start_processing.unwrap())(p));
                }
                1 => load_state(rig, blob.clone()),
                2 => {
                    save_state(rig);
                }
                3 => ((*p).reset.unwrap())(p),
                _ => ((*p).on_main_thread.unwrap())(p),
            }
        }
        if i % 50 == 0 {
            calls.lock().unwrap().clear();
        }
    }
    main_done_tx.send(()).unwrap();
    watchdog.join().unwrap();

    stop.store(true, Ordering::Relaxed);
    assert!(
        audio_rx.recv_timeout(Duration::from_secs(30)).is_ok(),
        "the audio thread is stuck: deadlock"
    );
    audio.join().unwrap();
    assert!(blocks.load(Ordering::Relaxed) > 10);
}

/// The worker sits in `process` while this thread deactivates and reactivates.
#[test]
fn reactivation_while_another_thread_processes_does_not_panic() {
    struct Shared<'a>(&'a Rig);
    unsafe impl Sync for Shared<'_> {}
    let rig = Rig::new();
    rig.activate(256);
    let shared = Shared(&rig);
    let stop = AtomicBool::new(false);
    let blocks = AtomicUsize::new(0);
    std::thread::scope(|s| {
        let worker = s.spawn(|| {
            let shared = &shared;
            while !stop.load(Ordering::Relaxed) {
                let _ = shared.0.process(256, &[], None, 2);
                blocks.fetch_add(1, Ordering::Relaxed);
            }
        });
        while blocks.load(Ordering::Relaxed) == 0 {
            std::thread::yield_now();
        }
        let p = rig.plugin();
        for _ in 0..5000 {
            unsafe {
                ((*p).deactivate.unwrap())(p);
                assert!(((*p).activate.unwrap())(p, 48000.0, 1, 256));
            }
        }
        stop.store(true, Ordering::Relaxed);
        assert!(worker.join().is_ok());
    });
}
