use crossbeam::channel::{self, RecvTimeoutError};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::change::{audio_change_pending, AudioChange};
use crate::prelude::{AuxiliaryBuffers, PluginNoteEvent, Transport};

pub(super) mod asio_driver;
mod buffer_sizes;
mod coreaudio_rate;
mod cpal;
pub mod device_watch;
mod dummy;
mod jack;

pub use self::cpal::CpalMidir;
pub use self::dummy::Dummy;
pub use self::jack::Jack;
pub use crate::buffer::Buffer;
pub use crate::plugin::Plugin;

/// Why a backend's [`Backend::run()`] call returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// The process callback returned `false` or a stop was requested through `should_stop`. The
    /// caller must not restart the backend.
    Stopped,
    /// The audio stream failed, e.g. because the audio device was disconnected. The caller may try
    /// [`Backend::reinit()`] followed by another [`Backend::run()`] call to recover.
    StreamFailed,
    /// A requested device that another device was standing in for is available again. Not a
    /// failure: the caller should [`Backend::reinit()`] and [`Backend::run()`] again to move onto it.
    DeviceReturned,
    /// The device asked for, or changed in a way that needs, a fresh stream: an ASIO driver's reset
    /// request, an output whose rate moved (A17). Not a failure: the caller should
    /// [`Backend::reinit()`] and [`Backend::run()`] again at once.
    Restart,
    /// A change was requested (A16): the caller takes it and calls [`Backend::reconfigure()`].
    Reconfigure,
}

/// Why [`wait_unless()`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wake {
    Stop,
    Change,
    Elapsed,
}

/// Sleep for `duration` in short slices. `true` when `should_stop` was raised meanwhile.
pub(crate) fn sleep_unless(should_stop: &AtomicBool, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if should_stop.load(Ordering::SeqCst) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    should_stop.load(Ordering::SeqCst)
}

/// Sleep for `max` in short slices, cut short by a stop or a pending audio change.
pub(crate) fn wait_unless(should_stop: &AtomicBool, max: Duration) -> Wake {
    let deadline = Instant::now() + max;
    loop {
        if should_stop.load(Ordering::SeqCst) {
            return Wake::Stop;
        }
        if audio_change_pending() {
            return Wake::Change;
        }
        if Instant::now() >= deadline {
            return Wake::Elapsed;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Posts a job to the GUI thread, which loads and releases an ASIO driver and so makes every
/// other call to it (A7, E1). `false` when the job was not taken: the GUI thread pumps no more.
pub type GuiExecutor = Arc<dyn Fn(Box<dyn FnOnce() + Send>) -> bool + Send + Sync>;

/// Run `job` on `input` on the GUI thread through `exec` and wait for its value, or right here
/// without one. When the application quits first (the executor refuses the job or `should_stop`
/// is raised while it waits), `input` comes back instead of staying with a job the GUI thread may
/// still run, so whatever it holds (a driver handle, a stream) is let go of here; `None` in its
/// place only when there is no job to take it back from. A job that took its input before the
/// stop is waited for: what it made (a started stream) is never dropped unseen.
pub(crate) fn run_on_gui_owning<I: Send + 'static, T: Send + 'static>(
    exec: Option<&GuiExecutor>,
    should_stop: &AtomicBool,
    input: I,
    job: impl FnOnce(I) -> T + Send + 'static,
) -> Result<T, Option<I>> {
    let Some(exec) = exec else {
        return Ok(job(input));
    };
    if should_stop.load(Ordering::SeqCst) {
        return Err(Some(input));
    }
    let slot = Arc::new(Mutex::new(Some(input)));
    let job_slot = slot.clone();
    let (reply_sender, reply) = channel::bounded(1);
    let posted = exec(Box::new(move || {
        let taken = job_slot.lock().take();
        if let Some(input) = taken {
            let _ = reply_sender.send(job(input));
        }
    }));
    if !posted {
        return Err(slot.lock().take());
    }
    loop {
        match reply.recv_timeout(Duration::from_millis(50)) {
            Ok(value) => return Ok(value),
            Err(RecvTimeoutError::Timeout) if !should_stop.load(Ordering::SeqCst) => {}
            Err(RecvTimeoutError::Timeout) => {
                let left = slot.lock().take();
                return match left {
                    Some(input) => Err(Some(input)),
                    None => reply.recv().map_err(|_| None),
                };
            }
            Err(RecvTimeoutError::Disconnected) => return Err(slot.lock().take()),
        }
    }
}

/// Run `job` on `input` on the GUI thread through `exec`, however long that thread takes, or right
/// here when there is no executor, it refuses the job or the job is dropped unrun. Unlike
/// [`run_on_gui_owning()`] a stop does not end the wait: this is how a run lets go of a driver, and the
/// wrapper runs such jobs until the audio thread ends.
pub(crate) fn run_on_gui_or_here<I: Send + 'static>(
    exec: Option<&GuiExecutor>,
    input: I,
    job: impl FnOnce(I) + Send + 'static,
) {
    let Some(exec) = exec else {
        return job(input);
    };
    let slot = Arc::new(Mutex::new(Some((input, job))));
    let job_slot = slot.clone();
    let (reply_sender, reply) = channel::bounded(1);
    let posted = exec(Box::new(move || {
        let taken = job_slot.lock().take();
        if let Some((input, job)) = taken {
            job(input);
        }
        let _ = reply_sender.send(());
    }));
    if posted && reply.recv().is_ok() {
        return;
    }
    let left = slot.lock().take();
    if let Some((input, job)) = left {
        job(input);
    }
}

/// A rate read on the GUI thread that nothing waits for: [`Self::post()`] asks for one, a later
/// [`Self::take()`] collects it. At most one read is in flight, so a GUI thread held by a modal
/// control panel never collects a backlog.
#[derive(Default)]
pub(crate) struct GuiRead {
    in_flight: AtomicBool,
    fresh: AtomicBool,
    bits: AtomicU64,
}

impl GuiRead {
    /// Post `read` through `exec`, or run it right here without one, unless a read is in flight.
    /// `wake` runs once the read is done.
    pub(crate) fn post(
        self: &Arc<Self>,
        exec: Option<&GuiExecutor>,
        read: impl FnOnce() -> Option<f64> + Send + 'static,
        wake: impl FnOnce() + Send + 'static,
    ) {
        if self.in_flight.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = self.clone();
        let job = move || {
            if let Some(value) = read() {
                this.bits.store(value.to_bits(), Ordering::Relaxed);
                this.fresh.store(true, Ordering::Release);
            }
            this.in_flight.store(false, Ordering::Release);
            wake();
        };
        match exec {
            Some(exec) => {
                if !exec(Box::new(job)) {
                    self.in_flight.store(false, Ordering::Release);
                }
            }
            None => job(),
        }
    }

    /// The value a read brought since the last call, if any.
    pub(crate) fn take(&self) -> Option<f64> {
        self.fresh
            .swap(false, Ordering::AcqRel)
            .then(|| f64::from_bits(self.bits.load(Ordering::Relaxed)))
    }
}

/// An audio+MIDI backend for the standalone wrapper.
pub trait Backend<P: Plugin>: 'static + Send + Sync {
    /// Start processing audio and MIDI on this thread. The process callback will be called whenever
    /// there's a new block of audio to be processed. The process callback receives the audio
    /// buffers for the wrapped plugin's outputs. Any inputs will have already been copied to this
    /// buffer. This will block until the process callback returns `false`, `should_stop` is set to
    /// `true`, the audio stream dies, or an audio change is requested.
    fn run(
        &mut self,
        should_stop: Arc<AtomicBool>,
        cb: impl FnMut(
                &mut Buffer,
                &mut AuxiliaryBuffers,
                Transport,
                &[PluginNoteEvent<P>],
                &mut Vec<PluginNoteEvent<P>>,
            ) -> bool
            + 'static
            + Send,
    ) -> RunOutcome;

    /// Rebuild the backend's audio resources after [`run()`][Self::run()] returned
    /// [`RunOutcome::StreamFailed`], [`RunOutcome::DeviceReturned`] or [`RunOutcome::Restart`], so
    /// that `run()` can be called again. Backends that don't support recovery return an error.
    fn reinit(&mut self) -> anyhow::Result<()> {
        anyhow::bail!("Audio device recovery is not supported by this backend")
    }

    /// Close what is open and open `change` applied to the current configuration. On failure the
    /// backend is left open on something it could open (see the implementor), or returns an error
    /// when nothing opens, in which case the caller recovers as after a failed `reinit()`.
    fn reconfigure(&mut self, change: AudioChange) -> anyhow::Result<()> {
        let _ = change;
        anyhow::bail!("Live audio changes are not supported by this backend")
    }

    /// Whether [`reconfigure(change)`][Self::reconfigure()] must run on the GUI thread: when the
    /// current or the target configuration involves a driver that is a single-threaded COM object
    /// (ASIO), see [`reinit_on_gui_thread()`][Self::reinit_on_gui_thread()].
    fn reconfigure_on_gui_thread(&self, change: &AudioChange) -> bool {
        let _ = change;
        self.reinit_on_gui_thread()
    }

    /// Block until the audio devices on the host change, `max` has passed, `should_stop` is
    /// raised or an audio change is requested. Used between failed [`reinit()`][Self::reinit()]
    /// attempts so a device that comes back is picked up promptly without hammering the backend.
    /// Backends without a device list simply wait.
    fn wait_for_device_change(&self, should_stop: &AtomicBool, max: Duration) {
        wait_unless(should_stop, max);
    }

    /// The stream keeps dying on a requested device: leave that device alone for a while and let
    /// the next [`reinit()`][Self::reinit()] stand something else in. `false` when nothing
    /// requested is open (the stand-in itself keeps dying), so the caller waits for the hardware
    /// to change instead.
    fn quarantine_requested(&mut self) -> bool {
        false
    }

    /// After [`RunOutcome::DeviceReturned`]: the requested device the previous return moved the
    /// stream onto was refused again, by the open or by the run that just ended, although the
    /// probe that brought it back had passed. The caller counts that as a failure, so a device
    /// whose probe and real use disagree backs off like a stream that keeps dying, and
    /// [`quarantine_requested()`][Self::quarantine_requested()] then quarantines that device.
    fn returned_device_refused(&self) -> bool {
        false
    }

    /// The process callback runs on a thread the audio driver owns and schedules itself (ASIO),
    /// so the wrapper must not register it with MMCSS or raise its priority: the driver manages
    /// that thread, and a registration would outlive the stream since it is never reverted.
    fn callback_thread_is_driver_owned(&self) -> bool {
        false
    }

    /// [`reinit()`][Self::reinit()] releases and reloads a driver that is a single-threaded COM
    /// object (ASIO), so it must run on the thread that loaded it at launch: the GUI thread, which
    /// pumps the messages such a driver relies on. The audio thread never pumps any.
    ///
    /// Every driver call outside the driver's own callbacks runs on the GUI thread: `run()` builds,
    /// starts, polls, stops and drops such a driver's stream through the [`GuiExecutor`] given to
    /// [`set_gui_executor()`][Self::set_gui_executor()], and waits for each job but the rate read,
    /// which it collects on a later wake.
    ///
    /// While such a stream is down, nothing on the GUI thread may block on the audio thread:
    /// `GuiContext::set_state()` waits for an audio callback, which would then never come.
    fn reinit_on_gui_thread(&self) -> bool {
        false
    }

    /// The way to the GUI thread, given once before the first [`run()`][Self::run()]. Without
    /// one, everything runs on the calling thread.
    fn set_gui_executor(&mut self, exec: GuiExecutor) {
        let _ = exec;
    }

    /// The (sample rate, plugin block) the stream that is open runs with, which the wrapper
    /// follows after every reopen (A17). `None` when the backend cannot say, and the plugin keeps
    /// the configuration it was initialized with.
    fn stream_format(&self) -> Option<(f32, u32)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wrapper::standalone::change::{request_audio_change, take_audio_change};
    use std::thread;

    fn run_on_gui<T: Send + 'static>(
        exec: Option<&GuiExecutor>,
        should_stop: &AtomicBool,
        job: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        run_on_gui_owning(exec, should_stop, (), move |()| job()).ok()
    }

    /// An executor that runs every job it takes on one spawned thread, in order.
    fn forwarding_executor() -> GuiExecutor {
        let (jobs, received) = channel::unbounded::<Box<dyn FnOnce() + Send>>();
        thread::spawn(move || {
            while let Ok(job) = received.recv() {
                job();
            }
        });
        Arc::new(move |job| jobs.send(job).is_ok())
    }

    #[test]
    fn run_on_gui_runs_the_job_on_the_executor_thread_and_returns_its_value() {
        let exec = forwarding_executor();
        let stop = AtomicBool::new(false);
        let caller = thread::current().id();
        let ran_on = run_on_gui(Some(&exec), &stop, || thread::current().id());
        assert!(ran_on.is_some_and(|id| id != caller));
        assert_eq!(run_on_gui(Some(&exec), &stop, || 42), Some(42));
    }

    #[test]
    fn run_on_gui_without_an_executor_runs_inline() {
        let stop = AtomicBool::new(false);
        let caller = thread::current().id();
        assert_eq!(
            run_on_gui(None, &stop, || thread::current().id()),
            Some(caller)
        );
    }

    #[test]
    fn run_on_gui_returns_none_when_the_executor_refuses() {
        let exec: GuiExecutor = Arc::new(|_| false);
        let stop = AtomicBool::new(false);
        assert_eq!(run_on_gui(Some(&exec), &stop, || 1), None);
    }

    #[test]
    fn a_stop_ends_the_wait_for_a_job_the_gui_never_runs() {
        let parked = Arc::new(Mutex::new(Vec::new()));
        let exec: GuiExecutor = {
            let parked = parked.clone();
            Arc::new(move |job| {
                parked.lock().push(job);
                true
            })
        };
        let stop = Arc::new(AtomicBool::new(false));
        {
            let stop = stop.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(120));
                stop.store(true, Ordering::SeqCst);
            });
        }
        let started = Instant::now();
        assert_eq!(run_on_gui(Some(&exec), &stop, || 1), None);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(parked.lock().len(), 1);
        assert_eq!(run_on_gui(Some(&exec), &stop, || 1), None);
        assert_eq!(
            parked.lock().len(),
            1,
            "nothing is posted once the application quits"
        );
    }

    #[test]
    fn an_owning_job_consumes_its_input_on_the_executor_thread() {
        let exec = forwarding_executor();
        let stop = AtomicBool::new(false);
        let caller = thread::current().id();
        let ran = run_on_gui_owning(Some(&exec), &stop, vec![1, 2], |input| {
            (input.len(), thread::current().id())
        });
        assert!(matches!(ran, Ok((2, id)) if id != caller));
    }

    #[test]
    fn an_owning_job_that_never_runs_hands_its_input_back() {
        let refused: GuiExecutor = Arc::new(|_| false);
        let stop = AtomicBool::new(false);
        let held = Arc::new(());
        let back = run_on_gui_owning(Some(&refused), &stop, held.clone(), |_| ());
        assert!(matches!(back, Err(Some(_))));
        drop(back);
        assert_eq!(Arc::strong_count(&held), 1);

        let parked = Arc::new(Mutex::new(Vec::new()));
        let queued: GuiExecutor = {
            let parked = parked.clone();
            Arc::new(move |job| {
                parked.lock().push(job);
                true
            })
        };
        let stop = AtomicBool::new(true);
        assert!(matches!(
            run_on_gui_owning(Some(&queued), &stop, held.clone(), |_| ()),
            Err(Some(_))
        ));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let stop = stop.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(120));
                stop.store(true, Ordering::SeqCst);
            });
        }
        let back = run_on_gui_owning(Some(&queued), &stop, held.clone(), |_| ());
        assert!(matches!(back, Err(Some(_))));
        drop(back);
        assert_eq!(parked.lock().len(), 1);
        assert_eq!(
            Arc::strong_count(&held),
            1,
            "the job left queued holds nothing of the input"
        );
    }

    #[test]
    fn a_job_that_took_its_input_before_the_stop_is_waited_for() {
        let exec: GuiExecutor = Arc::new(|job| {
            thread::spawn(job);
            true
        });
        let stop = Arc::new(AtomicBool::new(false));
        let raise = stop.clone();
        let ran = run_on_gui_owning(Some(&exec), &stop, vec![1, 2], move |input| {
            raise.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(150));
            input.len()
        });
        assert!(
            matches!(ran, Ok(2)),
            "what the job made comes back, never dropped on the GUI thread"
        );
    }

    /// Runs `end` through [`run_on_gui_or_here()`] and reports how often it ran and on which
    /// threads.
    fn end_a_run(exec: Option<&GuiExecutor>) -> Vec<thread::ThreadId> {
        let ran = Arc::new(Mutex::new(Vec::new()));
        let held = Arc::new(());
        let record = ran.clone();
        run_on_gui_or_here(exec, held.clone(), move |input| {
            drop(input);
            record.lock().push(thread::current().id());
        });
        assert_eq!(Arc::strong_count(&held), 1, "the input is consumed");
        let ran = ran.lock().clone();
        ran
    }

    #[test]
    fn the_end_of_a_run_runs_once_on_the_gui_thread() {
        let exec = forwarding_executor();
        let ran = end_a_run(Some(&exec));
        assert_eq!(ran.len(), 1);
        assert_ne!(ran[0], thread::current().id());
    }

    #[test]
    fn the_end_of_a_run_runs_once_here_without_a_gui_thread_to_take_it() {
        let here = thread::current().id();
        assert_eq!(end_a_run(None), vec![here]);
        let refused: GuiExecutor = Arc::new(|_| false);
        assert_eq!(end_a_run(Some(&refused)), vec![here]);
    }

    #[test]
    fn the_end_of_a_run_left_unrun_by_the_gui_thread_runs_once_here() {
        let parked = Arc::new(Mutex::new(Vec::new()));
        let exec: GuiExecutor = {
            let parked = parked.clone();
            Arc::new(move |job| {
                parked.lock().push(job);
                true
            })
        };
        {
            let parked = parked.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(120));
                parked.lock().clear();
            });
        }
        assert_eq!(end_a_run(Some(&exec)), vec![thread::current().id()]);
    }

    #[test]
    fn a_gui_read_arrives_without_a_wait_and_is_taken_once() {
        let exec = forwarding_executor();
        let read = Arc::new(GuiRead::default());
        let (woken, wake) = channel::bounded(1);
        read.post(
            Some(&exec),
            || Some(48000.0),
            move || {
                let _ = woken.send(());
            },
        );
        assert!(wake.recv_timeout(Duration::from_secs(1)).is_ok());
        assert_eq!(read.take(), Some(48000.0));
        assert_eq!(read.take(), None);

        read.post(None, || None, || ());
        assert_eq!(read.take(), None, "a read without a value brings nothing");
    }

    #[test]
    fn a_gui_read_is_posted_once_while_it_waits_for_the_gui_thread() {
        let parked = Arc::new(Mutex::new(Vec::new()));
        let exec: GuiExecutor = {
            let parked = parked.clone();
            Arc::new(move |job| {
                parked.lock().push(job);
                true
            })
        };
        let read = Arc::new(GuiRead::default());
        read.post(Some(&exec), || Some(44100.0), || ());
        read.post(Some(&exec), || Some(44100.0), || ());
        assert_eq!(parked.lock().len(), 1);
        assert_eq!(read.take(), None);

        let job = parked.lock().pop();
        if let Some(job) = job {
            job();
        }
        assert_eq!(read.take(), Some(44100.0));
        read.post(Some(&exec), || Some(44100.0), || ());
        assert_eq!(
            parked.lock().len(),
            1,
            "a read that came in lets the next one go"
        );

        let refused: GuiExecutor = Arc::new(|_| false);
        let read = Arc::new(GuiRead::default());
        read.post(Some(&refused), || Some(1.0), || ());
        read.post(Some(&exec), || Some(1.0), || ());
        assert_eq!(
            parked.lock().len(),
            2,
            "a refused read lets the next one go"
        );
    }

    #[test]
    fn a_pending_change_cuts_every_recovery_wait_short() {
        let _guard = crate::wrapper::standalone::change::TEST_LOCK.lock();
        let _ = take_audio_change();
        let stop = AtomicBool::new(false);
        request_audio_change(AudioChange {
            output: Some(None),
            ..Default::default()
        });
        let started = Instant::now();
        assert_eq!(wait_unless(&stop, Duration::from_secs(30)), Wake::Change);
        assert!(started.elapsed() < Duration::from_millis(200));
        let _ = take_audio_change();
    }

    #[test]
    fn a_stop_wins_over_a_change_and_an_idle_wait_elapses() {
        let _guard = crate::wrapper::standalone::change::TEST_LOCK.lock();
        let _ = take_audio_change();
        let stop = AtomicBool::new(true);
        assert_eq!(wait_unless(&stop, Duration::from_secs(5)), Wake::Stop);
        let stop = AtomicBool::new(false);
        assert_eq!(wait_unless(&stop, Duration::from_millis(60)), Wake::Elapsed);
    }
}
