//! Live audio changes the app asks for while the standalone runs (spec A16), and the ASIO
//! driver's control panel (A15).

use crossbeam::sync::Unparker;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use super::backend::asio_driver::driver_open;

pub(crate) type GuiRunner = Box<dyn Fn(Box<dyn FnOnce() + Send>) -> bool + Send + Sync>;

static PENDING: Mutex<Option<AudioChange>> = Mutex::new(None);
static WAKE: Mutex<Option<Unparker>> = Mutex::new(None);
static GUI_RUNNER: Mutex<Option<GuiRunner>> = Mutex::new(None);
static CONTROL_PANEL: Mutex<Option<fn()>> = Mutex::new(None);
/// A control panel job is queued on the GUI thread or running there.
static PANEL_PENDING: AtomicBool = AtomicBool::new(false);

/// These slots are process-global and tests run in parallel: every test that touches one holds
/// this.
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

/// A live change to the standalone's audio configuration. `None` leaves a part as it is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioChange {
    /// The backend id to run on (`"wasapi"`, `"asio"`, `"core-audio"`).
    pub driver: Option<String>,
    /// The output device or ASIO driver; `Some(None)` is the system default / the first driver
    /// that opens.
    pub output: Option<Option<String>>,
    /// The input device on a shared host; `Some(None)` is no input. Ignored on ASIO.
    pub input: Option<Option<String>>,
    /// The ASIO buffer size; `Some(None)` follows the driver. Ignored on shared hosts.
    pub period: Option<Option<u32>>,
}

impl AudioChange {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// A later change wins part by part.
    pub fn merge(&mut self, later: AudioChange) {
        let AudioChange {
            driver,
            output,
            input,
            period,
        } = later;
        if driver.is_some() {
            self.driver = driver;
        }
        if output.is_some() {
            self.output = output;
        }
        if input.is_some() {
            self.input = input;
        }
        if period.is_some() {
            self.period = period;
        }
    }
}

/// Ask the running standalone to move its audio onto `change` applied to what it runs on now.
/// Requests made before the audio thread takes them merge into one. The stream stops, the backend
/// reopens and the stream starts again on the new configuration; what it ends up open on is
/// published through [`audio_devices_in_use()`][super::audio_devices_in_use()].
pub fn request_audio_change(change: AudioChange) {
    if change.is_empty() {
        return;
    }
    {
        let mut pending = PENDING.lock();
        match pending.as_mut() {
            Some(existing) => existing.merge(change),
            None => *pending = Some(change),
        }
    }
    if let Some(unparker) = WAKE.lock().as_ref() {
        unparker.unpark();
    }
}

pub(crate) fn take_audio_change() -> Option<AudioChange> {
    PENDING.lock().take()
}

pub(crate) fn audio_change_pending() -> bool {
    PENDING.lock().is_some()
}

/// The running stream's parker, woken by every request so the run ends without waiting out its
/// poll interval.
pub(crate) fn register_wake(unparker: Unparker) {
    *WAKE.lock() = Some(unparker);
}

/// How a job reaches the running standalone's GUI thread: the runner queues it, or returns
/// `false` when it cannot, without waiting.
pub(crate) fn register_gui_runner(runner: GuiRunner) {
    *GUI_RUNNER.lock() = Some(runner);
}

/// Give the standalone the function that opens the loaded ASIO driver's control panel. It is
/// called on the GUI thread, which loads and releases the driver, and only while a driver is
/// open; the panel may block that thread until it closes.
pub fn register_asio_control_panel(open: fn()) {
    *CONTROL_PANEL.lock() = Some(open);
}

/// Open the ASIO driver's control panel on the GUI thread through the function given to
/// [`register_asio_control_panel()`], without waiting for it. `false` when no ASIO driver is open,
/// none was given, the GUI thread does not take the job, or a panel job is already queued or
/// open: the editor keeps taking clicks while a driver's modal panel is up, and a repeat would
/// reopen the panel once it closes. A driver released before the job runs is not opened. A size
/// or clock change made in the panel reaches the stream as the driver's reset request.
pub fn open_asio_control_panel() -> bool {
    if !driver_open() {
        return false;
    }
    let Some(open) = *CONTROL_PANEL.lock() else {
        return false;
    };
    let runner = GUI_RUNNER.lock();
    let Some(run) = runner.as_ref() else {
        return false;
    };
    if PANEL_PENDING.swap(true, Ordering::AcqRel) {
        return false;
    }
    let pending = PanelPending;
    run(Box::new(move || {
        let _pending = pending;
        if driver_open() {
            open();
        }
    }))
}

/// Lowers [`PANEL_PENDING`] when the panel job is done with: run, refused or dropped unrun.
struct PanelPending;

impl Drop for PanelPending {
    fn drop(&mut self) {
        PANEL_PENDING.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::super::backend::asio_driver::set_driver_open;
    use super::*;
    use crossbeam::channel::{self, Receiver};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static PANELS_OPENED: AtomicUsize = AtomicUsize::new(0);

    fn open_panel() {
        PANELS_OPENED.fetch_add(1, Ordering::SeqCst);
    }

    /// A GUI thread that holds every job it accepts until the test runs it.
    fn queueing_gui() -> Receiver<Box<dyn FnOnce() + Send>> {
        let (sender, jobs) = channel::unbounded();
        register_gui_runner(Box::new(move |job| sender.send(job).is_ok()));
        jobs
    }

    #[test]
    fn the_control_panel_is_not_offered_without_an_open_asio_driver() {
        let _guard = TEST_LOCK.lock();
        let jobs = queueing_gui();
        register_asio_control_panel(open_panel);
        set_driver_open(false);
        assert!(!open_asio_control_panel());
        assert!(jobs.try_recv().is_err());
    }

    #[test]
    fn the_control_panel_opens_on_the_gui_thread_while_a_driver_is_open() {
        let _guard = TEST_LOCK.lock();
        PANELS_OPENED.store(0, Ordering::SeqCst);
        let jobs = queueing_gui();
        register_asio_control_panel(open_panel);
        set_driver_open(true);
        assert!(open_asio_control_panel());
        assert_eq!(PANELS_OPENED.load(Ordering::SeqCst), 0);
        jobs.try_recv().unwrap()();
        assert_eq!(PANELS_OPENED.load(Ordering::SeqCst), 1);
        set_driver_open(false);
    }

    #[test]
    fn a_driver_released_before_the_gui_thread_runs_the_job_is_not_opened() {
        let _guard = TEST_LOCK.lock();
        PANELS_OPENED.store(0, Ordering::SeqCst);
        let jobs = queueing_gui();
        register_asio_control_panel(open_panel);
        set_driver_open(true);
        assert!(open_asio_control_panel());
        set_driver_open(false);
        jobs.try_recv().unwrap()();
        assert_eq!(PANELS_OPENED.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_second_request_posts_nothing_until_the_first_panel_job_has_run() {
        let _guard = TEST_LOCK.lock();
        PANELS_OPENED.store(0, Ordering::SeqCst);
        let jobs = queueing_gui();
        register_asio_control_panel(open_panel);
        set_driver_open(true);
        assert!(open_asio_control_panel());
        assert!(!open_asio_control_panel());
        assert_eq!(jobs.len(), 1);
        jobs.try_recv().unwrap()();
        assert_eq!(PANELS_OPENED.load(Ordering::SeqCst), 1);
        assert!(open_asio_control_panel());
        jobs.try_recv().unwrap()();
        assert_eq!(PANELS_OPENED.load(Ordering::SeqCst), 2);
        set_driver_open(false);
    }

    #[test]
    fn a_refused_or_dropped_panel_job_does_not_hold_back_the_next_request() {
        let _guard = TEST_LOCK.lock();
        register_asio_control_panel(open_panel);
        set_driver_open(true);
        register_gui_runner(Box::new(|_| false));
        assert!(!open_asio_control_panel());
        let jobs = queueing_gui();
        assert!(open_asio_control_panel());
        drop(jobs.try_recv().unwrap());
        assert!(open_asio_control_panel());
        jobs.try_recv().unwrap()();
        set_driver_open(false);
    }

    #[test]
    fn the_control_panel_needs_an_opener_and_a_gui_thread_that_takes_the_job() {
        let _guard = TEST_LOCK.lock();
        set_driver_open(true);
        *CONTROL_PANEL.lock() = None;
        let jobs = queueing_gui();
        assert!(!open_asio_control_panel());
        register_asio_control_panel(open_panel);
        *GUI_RUNNER.lock() = None;
        assert!(!open_asio_control_panel());
        register_gui_runner(Box::new(|_| false));
        assert!(!open_asio_control_panel());
        assert!(jobs.try_recv().is_err());
        set_driver_open(false);
    }

    #[test]
    fn a_later_change_wins_part_by_part() {
        let mut change = AudioChange {
            driver: Some("asio".into()),
            output: Some(Some("A".into())),
            ..Default::default()
        };
        change.merge(AudioChange {
            output: Some(None),
            period: Some(Some(256)),
            ..Default::default()
        });
        assert_eq!(change.driver.as_deref(), Some("asio"));
        assert_eq!(change.output, Some(None));
        assert_eq!(change.period, Some(Some(256)));
        assert_eq!(change.input, None);
    }

    #[test]
    fn requests_merge_until_taken_and_taking_empties_the_slot() {
        let _guard = TEST_LOCK.lock();
        let _ = take_audio_change();
        request_audio_change(AudioChange {
            period: Some(Some(128)),
            ..Default::default()
        });
        request_audio_change(AudioChange {
            output: Some(Some("B".into())),
            ..Default::default()
        });
        assert!(audio_change_pending());
        let taken = take_audio_change().unwrap();
        assert_eq!(taken.period, Some(Some(128)));
        assert_eq!(taken.output, Some(Some("B".into())));
        assert!(!audio_change_pending());
        assert_eq!(take_audio_change(), None);
    }

    #[test]
    fn an_empty_request_is_not_pending() {
        let _guard = TEST_LOCK.lock();
        let _ = take_audio_change();
        request_audio_change(AudioChange::default());
        assert!(!audio_change_pending());
    }
}
