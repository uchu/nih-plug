use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::prelude::{AuxiliaryBuffers, PluginNoteEvent, Transport};

mod cpal;
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
}
