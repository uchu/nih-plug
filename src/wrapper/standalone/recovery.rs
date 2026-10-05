//! When the standalone's audio thread tries again after the stream died.
//!
//! A stream that dies right after starting is a device that cannot hold a stream; one that dies after
//! a long healthy run is an unplug. The first is retried a few times with growing pauses, then left
//! alone until the hardware changes; the second always gets a fresh budget. A device that came back
//! is not a failure at all. A change the app asks for (A16) comes before all of this: it cuts every
//! wait short and is opened instead of the configuration that failed.

use std::time::Duration;

/// What the audio thread does next after a run ended or a reopen failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// Open the change the app asked for.
    Reconfigure,
    /// Reopen the configuration the stream was on.
    Recover,
}

pub(crate) fn next_step(change_pending: bool) -> Step {
    if change_pending {
        Step::Reconfigure
    } else {
        Step::Recover
    }
}

/// What the audio thread does before the next `reinit()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Pause, then reinitialise: the stream died.
    Reinit { backoff: Duration },
    /// Reinitialise at once: a requested device is available again.
    ReinitNow,
    /// The stream keeps dying: wait for the audio hardware to change before starting over.
    WaitForHardware,
}

pub struct Recovery {
    failures: u32,
}

impl Recovery {
    pub const MAX_RETRIES: u32 = 5;
    pub const BACKOFF: [Duration; 5] = [
        Duration::from_millis(250),
        Duration::from_millis(500),
        Duration::from_millis(1000),
        Duration::from_millis(2000),
        Duration::from_millis(4000),
    ];
    /// A run at least this long counts as healthy: the next failure starts a fresh budget.
    pub const STABLE_RUN: Duration = Duration::from_secs(10);
    /// The longest a wait for a hardware change lasts before trying again regardless.
    pub const HARDWARE_WAIT: Duration = Duration::from_secs(30);

    pub fn new() -> Self {
        Self { failures: 0 }
    }

    /// The stream died after running for `ran_for`.
    pub fn stream_failed(&mut self, ran_for: Duration) -> Action {
        if ran_for >= Self::STABLE_RUN {
            self.failures = 0;
        }
        if self.failures >= Self::MAX_RETRIES {
            self.failures = 0;
            return Action::WaitForHardware;
        }
        let backoff = Self::BACKOFF[self.failures as usize];
        self.failures += 1;
        Action::Reinit { backoff }
    }

    /// The backend reported that a requested device is back.
    pub fn device_returned(&mut self) -> Action {
        Action::ReinitNow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quick() -> Duration {
        Duration::from_millis(50)
    }

    #[test]
    fn repeated_quick_deaths_back_off_then_wait_for_hardware() {
        let mut recovery = Recovery::new();
        for backoff in Recovery::BACKOFF {
            assert_eq!(recovery.stream_failed(quick()), Action::Reinit { backoff });
        }
        assert_eq!(recovery.stream_failed(quick()), Action::WaitForHardware);
        // A fresh budget after the wait.
        assert_eq!(
            recovery.stream_failed(quick()),
            Action::Reinit {
                backoff: Recovery::BACKOFF[0]
            }
        );
    }

    #[test]
    fn a_stable_run_resets_the_budget() {
        let mut recovery = Recovery::new();
        for _ in 0..4 {
            recovery.stream_failed(quick());
        }
        assert_eq!(
            recovery.stream_failed(Recovery::STABLE_RUN),
            Action::Reinit {
                backoff: Recovery::BACKOFF[0]
            }
        );
    }

    #[test]
    fn a_run_just_short_of_stable_keeps_counting() {
        let mut recovery = Recovery::new();
        recovery.stream_failed(quick());
        assert_eq!(
            recovery.stream_failed(Recovery::STABLE_RUN - Duration::from_millis(1)),
            Action::Reinit {
                backoff: Recovery::BACKOFF[1]
            }
        );
    }

    #[test]
    fn a_pending_change_comes_before_recovering_the_old_configuration() {
        assert_eq!(next_step(true), Step::Reconfigure);
        assert_eq!(next_step(false), Step::Recover);
    }

    #[test]
    fn a_returning_device_is_not_a_failure() {
        let mut recovery = Recovery::new();
        recovery.stream_failed(quick());
        recovery.stream_failed(quick());
        assert_eq!(recovery.device_returned(), Action::ReinitNow);
        assert_eq!(
            recovery.stream_failed(quick()),
            Action::Reinit {
                backoff: Recovery::BACKOFF[2]
            }
        );
    }
}
