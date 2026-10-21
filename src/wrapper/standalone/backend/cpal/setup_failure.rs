//! A buffer size the ASIO driver listed and then refused at CreateBuffers or Start fails only
//! inside a run: what the reopen after that run does (E3).

/// The open a run is the first one after, as far as a stream that does not start in it goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FirstRun {
    /// The launch opened an ASIO driver; `retried` once the driver was reloaded at its own size
    /// because the launch's stream did not start.
    Launch { retried: bool },
    /// A change of the ASIO buffer size alone, from `previous` (`None` follows the driver).
    PeriodChange { previous: Option<u32> },
    /// A restart, a pick of a driver or a host, a shared host.
    Other,
}

/// What `reinit()` does after a run whose stream did not start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SetupFailure {
    /// Reload the driver at its own size.
    RetryFollowingDriver,
    /// Reopen at the size that ran before the change (`None`: the driver's own).
    RevertPeriod(Option<u32>),
    /// The launch's driver starts no stream: the shared host stands in on the system default, as
    /// it does for a launch where no driver opens.
    StandIn,
    /// Recover as from any stream that failed.
    Fail,
}

/// The buffer size a reopen after a stream that did not start asks the driver for, and the size
/// it publishes as refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Reopen {
    pub period: Option<u32>,
    pub refused: Option<u32>,
}

/// What a stream that did not start in the first run after `first_run` leads to. Each answer but `Fail` happens once per launch or
/// change: the reopen it asks for runs as `first_run_after` says. The launch retry reloads the
/// driver even when it already followed the driver: the reload is what clears a first start the
/// hardware refused once (a clock still locking, another client still letting go).
pub(super) fn setup_failure_action(first_run: FirstRun) -> SetupFailure {
    match first_run {
        FirstRun::Launch { retried: false } => SetupFailure::RetryFollowingDriver,
        FirstRun::Launch { retried: true } => SetupFailure::StandIn,
        FirstRun::PeriodChange { previous } => SetupFailure::RevertPeriod(previous),
        FirstRun::Other => SetupFailure::Fail,
    }
}

impl SetupFailure {
    /// The reopen at another size this asks for, the run that did not start having asked the
    /// driver for `requested` and sized its stream at `attempted`. A launch that followed the
    /// driver refused no size the user picked, so it names none.
    pub(super) fn reopen(self, requested: Option<u32>, attempted: Option<u32>) -> Option<Reopen> {
        match self {
            SetupFailure::RetryFollowingDriver => Some(Reopen {
                period: None,
                refused: requested.and(attempted),
            }),
            SetupFailure::RevertPeriod(period) => Some(Reopen {
                period,
                refused: attempted,
            }),
            SetupFailure::StandIn | SetupFailure::Fail => None,
        }
    }

    /// What the run after this reopen counts as, `first_run` being what is pending now. A plain
    /// recovery leaves it alone, so a launch retry whose reopen did not open still stands the
    /// shared host in when the run after a later reopen does not start either.
    fn first_run_after(self, first_run: FirstRun) -> FirstRun {
        match self {
            SetupFailure::RetryFollowingDriver => FirstRun::Launch { retried: true },
            SetupFailure::RevertPeriod(_) | SetupFailure::StandIn => FirstRun::Other,
            SetupFailure::Fail => first_run,
        }
    }
}

/// The refused size an open publishes: none once the stream runs at it.
pub(super) fn still_refused(refused: Option<u32>, running: Option<u32>) -> Option<u32> {
    refused.filter(|&size| Some(size) != running)
}

/// The backend's E3 state: what the next run is the first one after, and what the last run whose
/// stream did not start left `reinit()` to do. The backend calls these at each step, so the
/// sequence below is the one it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SetupRecovery {
    first_run: FirstRun,
    pending: SetupFailure,
}

impl SetupRecovery {
    pub(super) fn at_launch(duplex: bool) -> Self {
        Self {
            first_run: if duplex {
                FirstRun::Launch { retried: false }
            } else {
                FirstRun::Other
            },
            pending: SetupFailure::Fail,
        }
    }

    /// A run starts: it is the first run after what was pending, which it uses up.
    pub(super) fn on_run_start(&mut self) -> FirstRun {
        self.pending = SetupFailure::Fail;
        std::mem::replace(&mut self.first_run, FirstRun::Other)
    }

    /// The run that started as `first_run` ended because its stream did not start.
    pub(super) fn on_setup_failed(&mut self, first_run: FirstRun) {
        self.pending = setup_failure_action(first_run);
    }

    /// `reinit()` starts: what it does, the run after its reopen counted from here on.
    pub(super) fn on_reinit(&mut self) -> SetupFailure {
        let action = std::mem::replace(&mut self.pending, SetupFailure::Fail);
        self.first_run = action.first_run_after(self.first_run);
        action
    }

    /// The stand-in opened nothing: the next `reinit()` tries it again.
    pub(super) fn on_stand_in_failed(&mut self) {
        self.pending = SetupFailure::StandIn;
    }

    /// A pick is a fresh choice: it replaces whatever a stream that did not start left to do.
    pub(super) fn on_change(&mut self) {
        self.first_run = FirstRun::Other;
        self.pending = SetupFailure::Fail;
    }

    /// A change opened; its first run counts as `first_run`.
    pub(super) fn on_change_opened(&mut self, first_run: FirstRun) {
        self.first_run = first_run;
    }
}

/// What the first run after a live change that opened counts as. `period_only`: a duplex change
/// that picked no driver; it reverts to `previous_period` only when the driver now runs another
/// size (`size_before`, `size_now`) than before.
pub(super) fn first_run_after_change(
    period_only: bool,
    previous_period: Option<u32>,
    size_before: Option<u32>,
    size_now: Option<u32>,
) -> FirstRun {
    if period_only && size_before != size_now {
        FirstRun::PeriodChange {
            previous: previous_period,
        }
    } else {
        FirstRun::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_first_launch_failure_retries_following_the_driver() {
        assert_eq!(
            setup_failure_action(FirstRun::Launch { retried: false }),
            SetupFailure::RetryFollowingDriver
        );
    }

    #[test]
    fn a_period_only_change_reverts_to_the_previous_size() {
        let reverted = |previous| setup_failure_action(FirstRun::PeriodChange { previous });
        assert_eq!(reverted(Some(256)), SetupFailure::RevertPeriod(Some(256)));
        assert_eq!(reverted(None), SetupFailure::RevertPeriod(None));
    }

    #[test]
    fn any_other_first_run_recovers_as_a_failed_stream() {
        assert_eq!(setup_failure_action(FirstRun::Other), SetupFailure::Fail);
    }

    #[test]
    fn a_second_setup_failure_at_launch_stands_the_shared_host_in() {
        assert_eq!(
            setup_failure_action(FirstRun::Launch { retried: true }),
            SetupFailure::StandIn
        );
    }

    #[test]
    fn only_the_retry_and_the_revert_reopen_at_another_size() {
        assert_eq!(
            SetupFailure::RetryFollowingDriver.reopen(Some(64), Some(64)),
            Some(Reopen {
                period: None,
                refused: Some(64)
            })
        );
        assert_eq!(
            SetupFailure::RevertPeriod(Some(256)).reopen(Some(32), Some(32)),
            Some(Reopen {
                period: Some(256),
                refused: Some(32)
            })
        );
        assert_eq!(SetupFailure::StandIn.reopen(Some(32), Some(32)), None);
        assert_eq!(SetupFailure::Fail.reopen(Some(32), Some(32)), None);
    }

    #[test]
    fn a_launch_retry_without_a_size_names_no_refused_size() {
        assert_eq!(
            SetupFailure::RetryFollowingDriver.reopen(None, Some(256)),
            Some(Reopen {
                period: None,
                refused: None
            })
        );
    }

    #[test]
    fn a_reverted_change_to_the_drivers_own_size_names_the_size_it_ran() {
        assert_eq!(
            SetupFailure::RevertPeriod(Some(256)).reopen(None, Some(512)),
            Some(Reopen {
                period: Some(256),
                refused: Some(512)
            })
        );
    }

    #[test]
    fn a_size_that_runs_is_not_refused() {
        assert_eq!(still_refused(Some(256), Some(256)), None);
        assert_eq!(still_refused(Some(256), Some(512)), Some(256));
        assert_eq!(still_refused(Some(256), None), Some(256));
        assert_eq!(still_refused(None, Some(256)), None);
    }

    /// A driver whose every stream refuses to start, driven through the transitions the backend
    /// calls: each run starts and its stream does not, then `reinit()` runs and its reopen opens.
    /// Every path ends in a stand-in or the ordinary recovery within one reopen at another size.
    #[test]
    fn a_driver_that_starts_no_stream_at_any_size_ends_within_one_reopen() {
        let launch = SetupRecovery::at_launch(true);
        let mut period_change = SetupRecovery::at_launch(false);
        period_change.on_change();
        period_change.on_change_opened(FirstRun::PeriodChange {
            previous: Some(256),
        });
        let starts = [
            (launch, Some(64)),
            (launch, None),
            (period_change, Some(32)),
            (SetupRecovery::at_launch(false), Some(32)),
        ];
        for (mut recovery, mut period) in starts {
            let mut reopens = 0;
            let ended = loop {
                let first_run = recovery.on_run_start();
                recovery.on_setup_failed(first_run);
                let action = recovery.on_reinit();
                if let Some(reopen) = action.reopen(period, period) {
                    period = reopen.period;
                    reopens += 1;
                    assert!(reopens <= 1, "{recovery:?} keeps reopening");
                    continue;
                }
                break action;
            };
            assert!(matches!(ended, SetupFailure::StandIn | SetupFailure::Fail));
            if ended == SetupFailure::StandIn {
                recovery.on_stand_in_failed();
                assert_eq!(recovery.on_reinit(), SetupFailure::StandIn);
            }
            assert_eq!(recovery.on_run_start(), FirstRun::Other);
        }
    }

    #[test]
    fn a_launch_retries_once_then_stands_the_shared_host_in() {
        let mut recovery = SetupRecovery::at_launch(true);
        let first_run = recovery.on_run_start();
        assert_eq!(first_run, FirstRun::Launch { retried: false });
        recovery.on_setup_failed(first_run);
        assert_eq!(recovery.on_reinit(), SetupFailure::RetryFollowingDriver);
        let first_run = recovery.on_run_start();
        assert_eq!(first_run, FirstRun::Launch { retried: true });
        recovery.on_setup_failed(first_run);
        assert_eq!(recovery.on_reinit(), SetupFailure::StandIn);
    }

    #[test]
    fn a_reopen_that_did_not_open_keeps_the_launch_retry_pending() {
        let mut recovery = SetupRecovery::at_launch(true);
        let first_run = recovery.on_run_start();
        recovery.on_setup_failed(first_run);
        assert_eq!(recovery.on_reinit(), SetupFailure::RetryFollowingDriver);
        // That reopen failed: the wrapper calls `reinit()` again with no run between.
        assert_eq!(recovery.on_reinit(), SetupFailure::Fail);
        let first_run = recovery.on_run_start();
        assert_eq!(first_run, FirstRun::Launch { retried: true });
        recovery.on_setup_failed(first_run);
        assert_eq!(recovery.on_reinit(), SetupFailure::StandIn);
    }

    #[test]
    fn a_run_whose_stream_started_uses_the_first_run_up() {
        let mut recovery = SetupRecovery::at_launch(true);
        recovery.on_run_start();
        assert_eq!(recovery.on_reinit(), SetupFailure::Fail);
        let first_run = recovery.on_run_start();
        assert_eq!(first_run, FirstRun::Other);
        recovery.on_setup_failed(first_run);
        assert_eq!(recovery.on_reinit(), SetupFailure::Fail);
    }

    #[test]
    fn a_change_replaces_what_a_failed_stream_left_to_do() {
        let mut recovery = SetupRecovery::at_launch(true);
        let first_run = recovery.on_run_start();
        recovery.on_setup_failed(first_run);
        recovery.on_change();
        assert_eq!(recovery.on_reinit(), SetupFailure::Fail);
        assert_eq!(recovery.on_run_start(), FirstRun::Other);
    }

    #[test]
    fn a_period_change_counts_only_when_the_driver_runs_another_size() {
        assert_eq!(
            first_run_after_change(true, Some(256), Some(256), Some(32)),
            FirstRun::PeriodChange {
                previous: Some(256)
            }
        );
        assert_eq!(
            first_run_after_change(true, None, Some(256), Some(32)),
            FirstRun::PeriodChange { previous: None }
        );
        assert_eq!(
            first_run_after_change(true, None, Some(256), Some(256)),
            FirstRun::Other,
            "the same size snapped twice has nothing to revert to"
        );
        assert_eq!(
            first_run_after_change(false, Some(256), Some(256), Some(32)),
            FirstRun::Other,
            "a driver or host pick is no period change"
        );
    }
}
