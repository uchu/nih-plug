//! Live audio changes the app asks for while the standalone runs (spec A16).

use crossbeam::sync::Unparker;
use parking_lot::Mutex;

static PENDING: Mutex<Option<AudioChange>> = Mutex::new(None);
static WAKE: Mutex<Option<Unparker>> = Mutex::new(None);

/// The pending change is process-global and tests run in parallel: every test that touches it
/// holds this.
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

#[cfg(test)]
mod tests {
    use super::*;

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
