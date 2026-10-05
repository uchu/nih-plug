use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::change::{audio_change_pending, AudioChange};
use crate::prelude::{AuxiliaryBuffers, PluginNoteEvent, Transport};

mod asio_driver;
mod buffer_sizes;
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
    /// [`RunOutcome::StreamFailed`], so that `run()` can be called again. Backends that don't
    /// support recovery return an error.
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
    /// While such a stream is down, nothing on the GUI thread may block on the audio thread:
    /// `GuiContext::set_state()` waits for an audio callback, which would then never come.
    fn reinit_on_gui_thread(&self) -> bool {
        false
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
