//! A buffer size the ASIO driver listed and then refused at CreateBuffers or Start fails only
//! inside a run: what the reopen after that run does (E3).

/// The open a run is the first one after, as far as a stream that does not start in it goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FirstRun {
    /// The launch opened an ASIO driver; `retried` once the driver was reopened at its own size
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
    /// Reopen the driver at its own size.
    RetryFollowingDriver,
    /// Reopen at the size that ran before the change (`None`: the driver's own).
    RevertPeriod(Option<u32>),
    /// The launch's driver starts no stream: the shared host stands in on the system default, as
    /// it does for a launch where no driver opens.
    StandIn,
    /// Recover as from any stream that failed.
    Fail,
}

/// What a stream that did not start in the first run after `first_run` leads to, the backend
/// asking the driver for `requested_period`. Each answer but `Fail` happens once per launch or
/// change: the reopen it asks for runs as `first_run_after` says.
pub(super) fn setup_failure_action(
    first_run: FirstRun,
    requested_period: Option<u32>,
) -> SetupFailure {
    match first_run {
        FirstRun::Launch { retried: false } if requested_period.is_some() => {
            SetupFailure::RetryFollowingDriver
        }
        FirstRun::Launch { .. } => SetupFailure::StandIn,
        FirstRun::PeriodChange { previous } => SetupFailure::RevertPeriod(previous),
        FirstRun::Other => SetupFailure::Fail,
    }
}

impl SetupFailure {
    /// The buffer size the reopen asks the driver for, when it changes it.
    pub(super) fn reopen_period(self) -> Option<Option<u32>> {
        match self {
            SetupFailure::RetryFollowingDriver => Some(None),
            SetupFailure::RevertPeriod(period) => Some(period),
            SetupFailure::StandIn | SetupFailure::Fail => None,
        }
    }

    /// What the run after this reopen counts as, `first_run` being what is pending now. A plain
    /// recovery leaves it alone, so a launch retry whose reopen did not open still stands the
    /// shared host in when the run after a later reopen does not start either.
    pub(super) fn first_run_after(self, first_run: FirstRun) -> FirstRun {
        match self {
            SetupFailure::RetryFollowingDriver => FirstRun::Launch { retried: true },
            SetupFailure::RevertPeriod(_) | SetupFailure::StandIn => FirstRun::Other,
            SetupFailure::Fail => first_run,
        }
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
    fn a_launch_with_a_buffer_size_retries_following_the_driver() {
        assert_eq!(
            setup_failure_action(FirstRun::Launch { retried: false }, Some(64)),
            SetupFailure::RetryFollowingDriver
        );
    }

    #[test]
    fn a_period_only_change_reverts_to_the_previous_size() {
        let reverted =
            |previous| setup_failure_action(FirstRun::PeriodChange { previous }, Some(32));
        assert_eq!(reverted(Some(256)), SetupFailure::RevertPeriod(Some(256)));
        assert_eq!(reverted(None), SetupFailure::RevertPeriod(None));
    }

    #[test]
    fn any_other_first_run_recovers_as_a_failed_stream() {
        for requested in [None, Some(32)] {
            assert_eq!(
                setup_failure_action(FirstRun::Other, requested),
                SetupFailure::Fail
            );
        }
    }

    #[test]
    fn a_second_setup_failure_at_launch_stands_the_shared_host_in() {
        for requested in [None, Some(64)] {
            assert_eq!(
                setup_failure_action(FirstRun::Launch { retried: true }, requested),
                SetupFailure::StandIn
            );
        }
    }

    #[test]
    fn a_launch_that_already_follows_the_driver_stands_in_at_its_first_failure() {
        assert_eq!(
            setup_failure_action(FirstRun::Launch { retried: false }, None),
            SetupFailure::StandIn
        );
    }

    #[test]
    fn only_the_retry_and_the_revert_reopen_at_another_size() {
        assert_eq!(
            SetupFailure::RetryFollowingDriver.reopen_period(),
            Some(None)
        );
        assert_eq!(
            SetupFailure::RevertPeriod(Some(256)).reopen_period(),
            Some(Some(256))
        );
        assert_eq!(SetupFailure::StandIn.reopen_period(), None);
        assert_eq!(SetupFailure::Fail.reopen_period(), None);
    }

    /// A driver whose every stream refuses to start, run through the backend's transitions: the
    /// run consumes its first-run state and `reinit()` applies the action. Every path ends in a
    /// stand-in or the ordinary recovery within two reopens, never in another retry or revert.
    #[test]
    fn a_driver_that_starts_no_stream_at_any_size_ends_within_two_reopens() {
        let starts = [
            (FirstRun::Launch { retried: false }, Some(64)),
            (FirstRun::Launch { retried: false }, None),
            (
                FirstRun::PeriodChange {
                    previous: Some(256),
                },
                Some(32),
            ),
            (FirstRun::PeriodChange { previous: None }, Some(32)),
            (FirstRun::Other, Some(32)),
        ];
        for (mut first_run, mut period) in starts {
            let mut reopens = 0;
            loop {
                let action = setup_failure_action(first_run, period);
                first_run = FirstRun::Other;
                if let Some(next) = action.reopen_period() {
                    period = next;
                }
                first_run = action.first_run_after(first_run);
                if matches!(action, SetupFailure::StandIn | SetupFailure::Fail) {
                    break;
                }
                reopens += 1;
                assert!(reopens <= 2, "{first_run:?} keeps reopening");
            }
            assert!(reopens <= 1);
            assert_eq!(first_run, FirstRun::Other);
        }
    }

    #[test]
    fn a_reopen_that_did_not_open_keeps_the_launch_retry_pending() {
        let retried = FirstRun::Launch { retried: true };
        assert_eq!(SetupFailure::Fail.first_run_after(retried), retried);
        assert_eq!(
            SetupFailure::RetryFollowingDriver.first_run_after(FirstRun::Other),
            retried
        );
        assert_eq!(
            SetupFailure::RevertPeriod(None).first_run_after(FirstRun::Other),
            FirstRun::Other
        );
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
