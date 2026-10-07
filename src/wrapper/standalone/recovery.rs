//! When the standalone's audio thread tries again after the stream died.
//!
//! A stream that dies right after starting is a device that cannot hold a stream; one that dies after
//! a long healthy run is an unplug. The first is retried a few times with growing pauses, then left
//! alone until the hardware changes; the second always gets a fresh budget. A device that came back
//! is not a failure at all, unless the device the previous return moved onto was refused again
//! although its probe had passed; nor is a fresh stream the device needs (an ASIO reset request, a
//! rate that moved): neither spends the budget, unless the device asks again right after the last
//! restart, and a long healthy run before one gives a fresh budget too. A long stand-in run before
//! a device comes back forgives every failure but the ones a returned device caused: it says
//! nothing about whether that device can hold a stream. A change the app asks for
//! (A16) comes before all of this: it cuts every wait short and is opened instead of the
//! configuration that failed.

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
    /// Reinitialise at once, not after a failure: a requested device is available again, or the
    /// device needs a fresh stream.
    ReinitNow,
    /// The stream keeps dying: wait for the audio hardware to change before starting over.
    WaitForHardware,
}

pub struct Recovery {
    failures: u32,
    /// Of `failures`, the ones a device that had just come back caused: a quick death right after
    /// the return, or a refusal of it.
    relapses: u32,
    /// The run that ends next moved onto a requested device that had come back.
    returned: bool,
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
    /// A restart the device asks for after a shorter run counts as a failure, so a driver that
    /// asks at every start, or a rate that keeps flapping, backs off like a stream that keeps
    /// dying.
    pub const MIN_RESTART_RUN: Duration = Duration::from_secs(2);
    /// The longest a wait for a hardware change lasts before trying again regardless.
    pub const HARDWARE_WAIT: Duration = Duration::from_secs(30);

    pub fn new() -> Self {
        Self {
            failures: 0,
            relapses: 0,
            returned: false,
        }
    }

    /// The stream died after running for `ran_for`.
    pub fn stream_failed(&mut self, ran_for: Duration) -> Action {
        let relapse = std::mem::take(&mut self.returned) && ran_for < Self::STABLE_RUN;
        if ran_for >= Self::STABLE_RUN {
            self.fresh_budget();
        }
        self.count_failure(relapse)
    }

    fn fresh_budget(&mut self) {
        self.failures = 0;
        self.relapses = 0;
    }

    fn count_failure(&mut self, relapse: bool) -> Action {
        if self.failures >= Self::MAX_RETRIES {
            self.fresh_budget();
            return Action::WaitForHardware;
        }
        let backoff = Self::BACKOFF[self.failures as usize];
        self.failures += 1;
        self.relapses += u32::from(relapse);
        Action::Reinit { backoff }
    }

    /// The backend reported that a requested device is back after a run of `ran_for`.
    /// `refused_again`: the device the previous return moved onto was refused by the open or the
    /// run that followed although its probe had passed, so moving onto it again is a failure, and
    /// repeats back off up to a wait for the hardware instead of tearing the stream down every few
    /// seconds. Otherwise a run of `STABLE_RUN` or longer forgives the failures before it, as it
    /// does before a restart, except the ones a returned device caused: a device that comes back,
    /// dies at once and stays away for a while each time still reaches the wait.
    pub fn device_returned(&mut self, ran_for: Duration, refused_again: bool) -> Action {
        if std::mem::replace(&mut self.returned, true) && refused_again {
            return self.count_failure(true);
        }
        if ran_for >= Self::STABLE_RUN {
            self.failures = self.relapses;
        }
        Action::ReinitNow
    }

    /// The device needs a fresh stream after a run of `ran_for`. A run of `STABLE_RUN` or longer
    /// was healthy, so the budget starts fresh, as it does for a stream that dies after one. After
    /// `MIN_RESTART_RUN` the restart says nothing about whether the device can hold a stream, so
    /// the budget is left as it is; sooner, it is a failure.
    pub fn restart(&mut self, ran_for: Duration) -> Action {
        if ran_for >= Self::STABLE_RUN {
            self.fresh_budget();
        }
        if ran_for < Self::MIN_RESTART_RUN {
            self.stream_failed(ran_for)
        } else {
            self.returned = false;
            Action::ReinitNow
        }
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
    fn a_restart_after_a_long_run_never_touches_the_failure_budget() {
        let mut recovery = Recovery::new();
        recovery.stream_failed(quick());
        recovery.stream_failed(quick());
        assert_eq!(
            recovery.restart(Recovery::MIN_RESTART_RUN),
            Action::ReinitNow
        );
        assert_eq!(
            recovery.stream_failed(quick()),
            Action::Reinit {
                backoff: Recovery::BACKOFF[2]
            }
        );
    }

    #[test]
    fn restarts_after_long_runs_never_wait_for_hardware() {
        let mut recovery = Recovery::new();
        for _ in 0..=Recovery::MAX_RETRIES {
            assert_eq!(
                recovery.restart(Recovery::MIN_RESTART_RUN),
                Action::ReinitNow
            );
        }
        assert_eq!(
            recovery.stream_failed(quick()),
            Action::Reinit {
                backoff: Recovery::BACKOFF[0]
            }
        );
    }

    #[test]
    fn a_quick_restart_spends_the_failure_budget() {
        let mut recovery = Recovery::new();
        let quick_restart = Recovery::MIN_RESTART_RUN - Duration::from_millis(1);
        assert_eq!(
            recovery.restart(quick_restart),
            Action::Reinit {
                backoff: Recovery::BACKOFF[0]
            }
        );
        assert_eq!(
            recovery.stream_failed(quick()),
            Action::Reinit {
                backoff: Recovery::BACKOFF[1]
            }
        );
    }

    #[test]
    fn quick_restarts_in_a_row_back_off_then_wait_for_hardware() {
        let mut recovery = Recovery::new();
        for backoff in Recovery::BACKOFF {
            assert_eq!(recovery.restart(quick()), Action::Reinit { backoff });
        }
        assert_eq!(recovery.restart(quick()), Action::WaitForHardware);
    }

    #[test]
    fn a_healthy_run_before_a_quick_restart_starts_a_fresh_budget() {
        let mut recovery = Recovery::new();
        for _ in 0..=Recovery::MAX_RETRIES {
            assert_eq!(recovery.restart(Recovery::STABLE_RUN), Action::ReinitNow);
            assert_eq!(
                recovery.restart(quick()),
                Action::Reinit {
                    backoff: Recovery::BACKOFF[0]
                }
            );
        }
    }

    #[test]
    fn a_returning_device_is_not_a_failure() {
        let mut recovery = Recovery::new();
        recovery.stream_failed(quick());
        recovery.stream_failed(quick());
        assert_eq!(recovery.device_returned(quick(), false), Action::ReinitNow);
        assert_eq!(
            recovery.stream_failed(quick()),
            Action::Reinit {
                backoff: Recovery::BACKOFF[2]
            }
        );
    }

    #[test]
    fn a_returned_device_refused_again_backs_off_then_waits_for_hardware() {
        let mut recovery = Recovery::new();
        assert_eq!(recovery.device_returned(quick(), true), Action::ReinitNow);
        for backoff in Recovery::BACKOFF {
            assert_eq!(
                recovery.device_returned(quick(), true),
                Action::Reinit { backoff }
            );
        }
        assert_eq!(
            recovery.device_returned(quick(), true),
            Action::WaitForHardware
        );
    }

    #[test]
    fn a_return_after_a_run_that_was_not_a_return_is_never_a_failure() {
        let mut recovery = Recovery::new();
        recovery.device_returned(quick(), false);
        recovery.stream_failed(quick());
        assert_eq!(recovery.device_returned(quick(), true), Action::ReinitNow);
        recovery.restart(Recovery::MIN_RESTART_RUN);
        assert_eq!(recovery.device_returned(quick(), true), Action::ReinitNow);
    }

    #[test]
    fn a_returned_device_that_dies_quickly_spends_the_budget() {
        let mut recovery = Recovery::new();
        recovery.device_returned(quick(), false);
        assert_eq!(
            recovery.stream_failed(quick()),
            Action::Reinit {
                backoff: Recovery::BACKOFF[0]
            }
        );
        assert_eq!(
            recovery.device_returned(quick(), true),
            Action::ReinitNow,
            "the refusal belongs to a run that was not a return"
        );
        assert_eq!(
            recovery.device_returned(quick(), true),
            Action::Reinit {
                backoff: Recovery::BACKOFF[1]
            }
        );
    }

    #[test]
    fn a_long_stand_in_run_before_a_return_starts_a_fresh_budget() {
        let mut recovery = Recovery::new();
        recovery.stream_failed(quick());
        recovery.stream_failed(quick());
        assert_eq!(
            recovery.device_returned(Recovery::STABLE_RUN, false),
            Action::ReinitNow
        );
        recovery.restart(Recovery::MIN_RESTART_RUN);
        for backoff in Recovery::BACKOFF {
            assert_eq!(recovery.stream_failed(quick()), Action::Reinit { backoff });
        }
    }

    #[test]
    fn a_device_that_returns_and_dies_at_once_is_never_forgiven_by_the_stand_in() {
        let mut recovery = Recovery::new();
        for backoff in Recovery::BACKOFF {
            assert_eq!(
                recovery.device_returned(Recovery::STABLE_RUN, false),
                Action::ReinitNow
            );
            assert_eq!(recovery.stream_failed(quick()), Action::Reinit { backoff });
        }
        recovery.device_returned(Recovery::STABLE_RUN, false);
        assert_eq!(recovery.stream_failed(quick()), Action::WaitForHardware);
    }
}
