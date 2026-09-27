use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::prelude::{AuxiliaryBuffers, PluginNoteEvent, Transport};

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

/// An audio+MIDI backend for the standalone wrapper.
pub trait Backend<P: Plugin>: 'static + Send + Sync {
    /// Start processing audio and MIDI on this thread. The process callback will be called whenever
    /// there's a new block of audio to be processed. The process callback receives the audio
    /// buffers for the wrapped plugin's outputs. Any inputs will have already been copied to this
    /// buffer. This will block until the process callback returns `false`, `should_stop` is set to
    /// `true`, or the audio stream dies.
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

    /// Block until the audio devices on the host change, `max` has passed or `should_stop` is
    /// raised. Used between failed [`reinit()`][Self::reinit()] attempts so a device that comes
    /// back is picked up promptly without hammering the backend. Backends without a device list
    /// simply wait.
    fn wait_for_device_change(&self, should_stop: &AtomicBool, max: Duration) {
        sleep_unless(should_stop, max);
    }

    /// The stream keeps dying on a requested device: leave that device alone for a while and let
    /// the next [`reinit()`][Self::reinit()] stand something else in. `false` when nothing
    /// requested is open (the stand-in itself keeps dying), so the caller waits for the hardware
    /// to change instead.
    fn quarantine_requested(&mut self) -> bool {
        false
    }
}
