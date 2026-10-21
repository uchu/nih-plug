//! A buffer size the ASIO driver listed and then refused at CreateBuffers or Start fails only
//! inside a run: what the reopen after that run does (E3), and at launch the walk on to the next
//! listed driver (§ 6 f).

use anyhow::Result;

pub(super) use super::target::LaunchWalk;

/// The open a run is the first one after, as far as a stream that does not start in it goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FirstRun {
    /// The launch opened an ASIO driver; `retried` once the driver was reloaded at its own size
    /// because the launch's stream did not start.
    Launch { retried: bool },
    /// A change of the ASIO buffer size alone, from `previous` (`None` follows the driver).
    PeriodChange { previous: Option<u32> },
    /// A pick of an ASIO driver, or a switch onto ASIO, that asked the driver for a buffer size.
    Pick,
    /// A restart, a pick that follows the driver's size, a shared host.
    Other,
}

/// What `reinit()` does after a run whose stream did not start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SetupFailure {
    /// Reload the driver at its own size.
    RetryFollowingDriver,
    /// Reload the picked driver at its own size; a stream that then does not start either is a
    /// failed stream.
    PickFollowingDriver,
    /// Reopen at the size that ran before the change (`None`: the driver's own).
    RevertPeriod(Option<u32>),
    /// The launch's driver starts no stream, at the saved size nor its own, or a driver the
    /// launch walked on to starts none: the next listed driver the launch has not tried opens,
    /// following the driver.
    WalkOn,
    /// The launch's driver starts no stream and no listed driver is left to walk on to: the
    /// shared host stands in on the system default, as it does for a launch where no driver opens.
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

/// What a stream that did not start in the first run after `first_run` leads to. Each answer but
/// `Fail` happens once per launch or change, or once more after a stand-in that opened nothing:
/// the reopen it asks for runs as `first_run_after` says. The launch retry reloads the
/// driver even when it already followed the driver: the reload is what clears a first start the
/// hardware refused once (a clock still locking, another client still letting go).
pub(super) fn setup_failure_action(first_run: FirstRun) -> SetupFailure {
    match first_run {
        FirstRun::Launch { retried: false } => SetupFailure::RetryFollowingDriver,
        FirstRun::Launch { retried: true } => SetupFailure::StandIn,
        FirstRun::PeriodChange { previous } => SetupFailure::RevertPeriod(previous),
        FirstRun::Pick => SetupFailure::PickFollowingDriver,
        FirstRun::Other => SetupFailure::Fail,
    }
}

impl SetupFailure {
    /// The reopen at another size this asks for, the run that did not start having asked the
    /// driver for `requested` and sized its stream at `attempted`. A launch that followed the
    /// driver refused no size the user picked, so it names none.
    pub(super) fn reopen(self, requested: Option<u32>, attempted: Option<u32>) -> Option<Reopen> {
        match self {
            SetupFailure::RetryFollowingDriver | SetupFailure::PickFollowingDriver => {
                Some(Reopen {
                    period: None,
                    refused: requested.and(attempted),
                })
            }
            SetupFailure::RevertPeriod(period) => Some(Reopen {
                period,
                refused: attempted,
            }),
            SetupFailure::WalkOn | SetupFailure::StandIn | SetupFailure::Fail => None,
        }
    }

    /// What the run after this reopen counts as, `first_run` being what is pending now. A plain
    /// recovery leaves it alone, so a launch retry whose reopen did not open still stands the
    /// shared host in when the run after a later reopen does not start either.
    fn first_run_after(self, first_run: FirstRun) -> FirstRun {
        match self {
            SetupFailure::RetryFollowingDriver | SetupFailure::WalkOn => {
                FirstRun::Launch { retried: true }
            }
            SetupFailure::PickFollowingDriver
            | SetupFailure::RevertPeriod(_)
            | SetupFailure::StandIn => FirstRun::Other,
            SetupFailure::Fail => first_run,
        }
    }

    /// Whether `reinit()` goes on with the open the run belonged to (a reopen at another size, the
    /// stand-in) instead of recovering from a failed stream: the stream is not down meanwhile.
    pub(super) fn follows_up(self) -> bool {
        self != SetupFailure::Fail
    }
}

/// The refused size an open publishes: none once the stream runs at it.
pub(super) fn still_refused(refused: Option<u32>, running: Option<u32>) -> Option<u32> {
    refused.filter(|&size| Some(size) != running)
}

/// Where the launch's walk over the listed drivers got to (`SetupRecovery::walk_on`).
#[derive(Debug)]
pub(super) enum Walked<T> {
    /// This driver opened; its first run gets one attempt.
    Opened(String, T),
    /// No driver is left that opens: the shared host stands in.
    Spent,
    /// A pick or a quit stopped the walk.
    Ended,
}

/// The backend's E3 state: what the next run is the first one after, what the last run whose
/// stream did not start left `reinit()` to do, and the launch's walk over the listed drivers. The
/// backend calls these at each step, so the sequence below is the one it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SetupRecovery {
    first_run: FirstRun,
    pending: SetupFailure,
    walk: LaunchWalk,
}

impl SetupRecovery {
    pub(super) fn at_launch(duplex: bool, walk: LaunchWalk) -> Self {
        Self {
            first_run: if duplex {
                FirstRun::Launch { retried: false }
            } else {
                FirstRun::Other
            },
            pending: SetupFailure::Fail,
            walk,
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

    /// `reinit()` starts: what it does, the run after its reopen counted from here on. A stand-in
    /// waits while the launch's walk has a driver left.
    pub(super) fn on_reinit(&mut self) -> SetupFailure {
        let action = match std::mem::replace(&mut self.pending, SetupFailure::Fail) {
            SetupFailure::StandIn if !self.walk.is_spent() => SetupFailure::WalkOn,
            action => action,
        };
        self.first_run = action.first_run_after(self.first_run);
        action
    }

    /// `reinit()` walks on (`SetupFailure::WalkOn`): `open` loads the next untried driver,
    /// following the driver. Once none is left that opens, the stand-in's first run counts.
    pub(super) fn walk_on<T>(
        &mut self,
        open: impl FnMut(&str) -> Result<T>,
        ended: impl Fn() -> bool,
    ) -> Walked<T> {
        match self.walk.next_that_opens(open, &ended) {
            Some((name, opened)) => Walked::Opened(name, opened),
            None if ended() => Walked::Ended,
            None => {
                self.first_run = SetupFailure::StandIn.first_run_after(self.first_run);
                Walked::Spent
            }
        }
    }

    /// The saved driver a driver the launch walked on to stands in for.
    pub(super) fn walk_refused(&self) -> Option<&str> {
        self.walk.refused()
    }

    /// The stand-in opened nothing and the backend stays on the driver it left: the next
    /// `reinit()` reloads that driver, and a stream it then does not start stands the shared host
    /// in again.
    pub(super) fn on_stand_in_failed(&mut self) {
        self.first_run = FirstRun::Launch { retried: true };
        self.pending = SetupFailure::Fail;
    }

    /// A pick is a fresh choice: it replaces whatever a stream that did not start left to do, the
    /// launch's walk included.
    pub(super) fn on_change(&mut self) {
        self.first_run = FirstRun::Other;
        self.pending = SetupFailure::Fail;
        self.walk = LaunchWalk::default();
    }

    /// A change opened; its first run counts as `first_run`.
    pub(super) fn on_change_opened(&mut self, first_run: FirstRun) {
        self.first_run = first_run;
    }
}

/// What the first run after a duplex change that picked no driver counts as: it reverts to
/// `previous_period` only when the driver now runs another size (`size_before`, `size_now`) than
/// before.
pub(super) fn first_run_after_change(
    previous_period: Option<u32>,
    size_before: Option<u32>,
    size_now: Option<u32>,
) -> FirstRun {
    if size_before != size_now {
        FirstRun::PeriodChange {
            previous: previous_period,
        }
    } else {
        FirstRun::Other
    }
}

/// What the first run after a pick or a switch that opened counts as, the host it opened being
/// `duplex` and asked for the buffer size `requested`.
pub(super) fn first_run_after_pick(duplex: bool, requested: Option<u32>) -> FirstRun {
    if duplex && requested.is_some() {
        FirstRun::Pick
    } else {
        FirstRun::Other
    }
}

#[cfg(test)]
mod tests {
    use super::super::target::launch_duplex;
    use super::*;

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    /// No driver left to walk on to.
    fn at_launch(duplex: bool) -> SetupRecovery {
        SetupRecovery::at_launch(duplex, LaunchWalk::default())
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Ends {
        Runs(String),
        StandIn,
    }

    /// A duplex launch driven through the sequence the backend runs: the launch's own walk to the
    /// first driver that opens, then each run, `reinit()` and the walk on. A driver in `opens`
    /// loads and opens, one in `streams` also starts its stream. Returns where it ends, every
    /// load in order and the driver published as refused.
    fn launch(
        requested: Option<&str>,
        listed: &[&str],
        opens: &[&str],
        streams: &[&str],
    ) -> (Ends, Vec<String>, Option<String>) {
        let listed = names(listed);
        let loads = std::cell::RefCell::new(Vec::new());
        let mut open = |name: &str| {
            loads.borrow_mut().push(name.to_string());
            if opens.contains(&name) {
                Ok(())
            } else {
                Err(anyhow::anyhow!("'{name}' does not open"))
            }
        };
        let (_, opened) = launch_duplex(requested, &listed, &mut open);
        let Ok((mut running, ())) = opened else {
            return (Ends::StandIn, loads.into_inner(), None);
        };
        let mut recovery =
            SetupRecovery::at_launch(true, LaunchWalk::after(requested, &listed, &running));
        for _ in 0..=2 * listed.len() {
            let first_run = recovery.on_run_start();
            if streams.contains(&running.as_str()) {
                let refused = recovery.walk_refused().map(str::to_string);
                let later = recovery.on_run_start();
                assert_eq!(later, FirstRun::Other, "the walk ends once a stream runs");
                recovery.on_setup_failed(later);
                assert_eq!(recovery.on_reinit(), SetupFailure::Fail);
                return (Ends::Runs(running), loads.into_inner(), refused);
            }
            recovery.on_setup_failed(first_run);
            match recovery.on_reinit() {
                SetupFailure::RetryFollowingDriver => loads.borrow_mut().push(running.clone()),
                SetupFailure::WalkOn => match recovery.walk_on(&mut open, || false) {
                    Walked::Opened(name, ()) => running = name,
                    Walked::Spent => return (Ends::StandIn, loads.into_inner(), None),
                    Walked::Ended => unreachable!("nothing ends this walk"),
                },
                SetupFailure::StandIn => return (Ends::StandIn, loads.into_inner(), None),
                other => panic!("a launch does not {other:?}"),
            }
        }
        panic!("the launch walk did not end: {:?}", loads.into_inner());
    }

    #[test]
    fn a_saved_driver_that_starts_no_stream_walks_on_to_the_next_listed_driver_that_runs() {
        let (ends, loads, refused) = launch(Some("A"), &["A", "B", "C"], &["A", "C"], &["C"]);
        assert_eq!(ends, Ends::Runs("C".into()));
        assert_eq!(
            loads,
            ["A", "A", "B", "C"],
            "B does not open and is skipped"
        );
        assert_eq!(refused.as_deref(), Some("A"));
    }

    #[test]
    fn a_walk_where_no_driver_runs_stands_the_shared_host_in() {
        let (ends, loads, _) = launch(Some("A"), &["A", "B", "C"], &["A", "C"], &[]);
        assert_eq!(ends, Ends::StandIn);
        assert_eq!(loads, ["A", "A", "B", "C"], "C started no stream, once");
        let (ends, loads, _) = launch(Some("A"), &["A", "B", "C"], &["A"], &[]);
        assert_eq!(ends, Ends::StandIn);
        assert_eq!(loads, ["A", "A", "B", "C"], "no other driver opened");
        let (ends, loads, _) = launch(Some("A"), &["A"], &["A"], &[]);
        assert_eq!(ends, Ends::StandIn);
        assert_eq!(
            loads,
            ["A", "A"],
            "a lone driver stands the shared host in as before"
        );
    }

    #[test]
    fn first_available_walks_on_the_same_way_and_refuses_no_saved_driver() {
        let (ends, loads, refused) = launch(None, &["A", "B", "C"], &["A", "C"], &["C"]);
        assert_eq!(ends, Ends::Runs("C".into()));
        assert_eq!(loads, ["A", "A", "B", "C"]);
        assert_eq!(refused, None);
        let (ends, _, _) = launch(None, &["A", "B", "C"], &["A", "C"], &[]);
        assert_eq!(ends, Ends::StandIn);
    }

    #[test]
    fn a_saved_driver_refused_at_open_walks_on_past_the_driver_that_stood_in() {
        let (ends, loads, refused) = launch(Some("B"), &["A", "B", "C", "D"], &["A", "D"], &["D"]);
        assert_eq!(ends, Ends::Runs("D".into()));
        assert_eq!(loads, ["B", "A", "A", "C", "D"], "B is never loaded again");
        assert_eq!(refused.as_deref(), Some("B"));
    }

    #[test]
    fn a_walked_driver_that_starts_no_stream_gets_one_attempt() {
        let (ends, loads, _) = launch(Some("A"), &["A", "B", "C"], &["A", "B", "C"], &["C"]);
        assert_eq!(ends, Ends::Runs("C".into()));
        assert_eq!(loads, ["A", "A", "B", "C"], "B is not retried");
    }

    #[test]
    fn a_pick_during_the_walk_ends_it() {
        let listed = names(&["A", "B", "C"]);
        let mut recovery = SetupRecovery::at_launch(true, LaunchWalk::after(None, &listed, "A"));
        for _ in 0..2 {
            let first_run = recovery.on_run_start();
            recovery.on_setup_failed(first_run);
            if recovery.on_reinit() == SetupFailure::WalkOn {
                break;
            }
        }
        let mut loads = Vec::new();
        let picked = std::cell::Cell::new(false);
        let walked = recovery.walk_on(
            |name: &str| {
                loads.push(name.to_string());
                picked.set(true);
                Err::<(), _>(anyhow::anyhow!("'{name}' does not open"))
            },
            || picked.get(),
        );
        assert!(matches!(walked, Walked::Ended));
        assert_eq!(loads, ["B"], "C is not loaded after the pick");
        recovery.on_change();
        assert_eq!(recovery.walk_refused(), None);
        assert_eq!(recovery.on_reinit(), SetupFailure::Fail);
        let first_run = recovery.on_run_start();
        assert_eq!(first_run, FirstRun::Other);
        recovery.on_setup_failed(first_run);
        assert_eq!(
            recovery.on_reinit(),
            SetupFailure::Fail,
            "nothing is left to walk on to"
        );
    }

    #[test]
    fn a_pick_before_the_walk_starts_clears_it() {
        let listed = names(&["A", "B"]);
        let mut recovery =
            SetupRecovery::at_launch(true, LaunchWalk::after(Some("A"), &listed, "A"));
        let first_run = recovery.on_run_start();
        recovery.on_setup_failed(first_run);
        recovery.on_change();
        assert_eq!(recovery.on_reinit(), SetupFailure::Fail);
        recovery.on_change_opened(FirstRun::Launch { retried: true });
        let first_run = recovery.on_run_start();
        recovery.on_setup_failed(first_run);
        assert_eq!(recovery.on_reinit(), SetupFailure::StandIn);
    }

    #[test]
    fn a_stand_in_that_opened_nothing_after_the_walk_never_walks_again() {
        let listed = names(&["A", "B"]);
        let mut recovery = SetupRecovery::at_launch(true, LaunchWalk::after(None, &listed, "A"));
        for _ in 0..2 {
            let first_run = recovery.on_run_start();
            recovery.on_setup_failed(first_run);
            recovery.on_reinit();
        }
        assert!(matches!(
            recovery.walk_on(|_| Err::<(), _>(anyhow::anyhow!("no")), || false),
            Walked::Spent
        ));
        recovery.on_stand_in_failed();
        assert_eq!(recovery.on_reinit(), SetupFailure::Fail);
        let first_run = recovery.on_run_start();
        recovery.on_setup_failed(first_run);
        assert_eq!(recovery.on_reinit(), SetupFailure::StandIn);
    }

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
    /// Every path ends in a stand-in or the ordinary recovery within one reopen at another size,
    /// and a launch's walk loads each listed driver it has not tried once, every one opening.
    #[test]
    fn a_driver_that_starts_no_stream_at_any_size_ends_within_one_reopen() {
        let launch = at_launch(true);
        let listed = names(&["A", "B", "C"]);
        let walk = SetupRecovery::at_launch(true, LaunchWalk::after(Some("B"), &listed, "B"));
        let mut period_change = at_launch(false);
        period_change.on_change();
        period_change.on_change_opened(FirstRun::PeriodChange {
            previous: Some(256),
        });
        let mut pick = at_launch(true);
        pick.on_change();
        pick.on_change_opened(first_run_after_pick(true, Some(32)));
        let starts = [
            (launch.clone(), Some(64)),
            (launch, None),
            (walk.clone(), Some(64)),
            (walk, None),
            (period_change, Some(32)),
            (pick, Some(32)),
            (at_launch(false), Some(32)),
        ];
        for (mut recovery, mut period) in starts {
            let mut reopens = 0;
            let mut walked: Vec<String> = Vec::new();
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
                if action == SetupFailure::WalkOn {
                    assert_eq!(action.reopen(period, period), None);
                    let Walked::Opened(name, ()) = recovery.walk_on(|_| Ok(()), || false) else {
                        panic!("every driver opens");
                    };
                    assert!(!walked.contains(&name), "{name} walked onto twice");
                    walked.push(name);
                    assert!(
                        walked.len() < listed.len(),
                        "{walked:?} walks past the list"
                    );
                    period = None;
                    continue;
                }
                break action;
            };
            assert!(walked.is_empty() || walked == ["A", "C"], "{walked:?}");
            assert!(matches!(ended, SetupFailure::StandIn | SetupFailure::Fail));
            if ended == SetupFailure::StandIn {
                recovery.on_stand_in_failed();
                assert_eq!(recovery.on_reinit(), SetupFailure::Fail);
                let first_run = recovery.on_run_start();
                recovery.on_setup_failed(first_run);
                assert_eq!(recovery.on_reinit(), SetupFailure::StandIn);
            }
            assert_eq!(recovery.on_run_start(), FirstRun::Other);
        }
    }

    #[test]
    fn a_launch_retries_once_then_stands_the_shared_host_in() {
        let mut recovery = at_launch(true);
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
        let mut recovery = at_launch(true);
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
        let mut recovery = at_launch(true);
        recovery.on_run_start();
        assert_eq!(recovery.on_reinit(), SetupFailure::Fail);
        let first_run = recovery.on_run_start();
        assert_eq!(first_run, FirstRun::Other);
        recovery.on_setup_failed(first_run);
        assert_eq!(recovery.on_reinit(), SetupFailure::Fail);
    }

    #[test]
    fn a_change_replaces_what_a_failed_stream_left_to_do() {
        let mut recovery = at_launch(true);
        let first_run = recovery.on_run_start();
        recovery.on_setup_failed(first_run);
        recovery.on_change();
        assert_eq!(recovery.on_reinit(), SetupFailure::Fail);
        assert_eq!(recovery.on_run_start(), FirstRun::Other);
    }

    #[test]
    fn a_pick_that_asked_for_a_size_retries_once_following_the_driver() {
        assert_eq!(
            setup_failure_action(FirstRun::Pick),
            SetupFailure::PickFollowingDriver
        );
        assert_eq!(
            SetupFailure::PickFollowingDriver.reopen(Some(32), Some(32)),
            Some(Reopen {
                period: None,
                refused: Some(32)
            })
        );
        let mut recovery = at_launch(true);
        recovery.on_run_start();
        recovery.on_change();
        recovery.on_change_opened(first_run_after_pick(true, Some(32)));
        let first_run = recovery.on_run_start();
        assert_eq!(first_run, FirstRun::Pick);
        recovery.on_setup_failed(first_run);
        assert_eq!(recovery.on_reinit(), SetupFailure::PickFollowingDriver);
        let first_run = recovery.on_run_start();
        recovery.on_setup_failed(first_run);
        assert_eq!(recovery.on_reinit(), SetupFailure::Fail);
    }

    #[test]
    fn only_a_duplex_pick_that_asked_for_a_size_counts_as_a_pick() {
        assert_eq!(first_run_after_pick(true, Some(64)), FirstRun::Pick);
        assert_eq!(first_run_after_pick(true, None), FirstRun::Other);
        assert_eq!(first_run_after_pick(false, Some(64)), FirstRun::Other);
    }

    #[test]
    fn a_stand_in_that_opened_nothing_reloads_the_driver_before_standing_in_again() {
        let mut recovery = at_launch(true);
        for _ in 0..2 {
            let first_run = recovery.on_run_start();
            recovery.on_setup_failed(first_run);
            recovery.on_reinit();
        }
        recovery.on_stand_in_failed();
        assert_eq!(recovery.on_reinit(), SetupFailure::Fail);
        let first_run = recovery.on_run_start();
        assert_eq!(first_run, FirstRun::Launch { retried: true });
        recovery.on_setup_failed(first_run);
        assert_eq!(recovery.on_reinit(), SetupFailure::StandIn);
    }

    #[test]
    fn every_failure_but_a_plain_one_is_followed_up_by_the_same_open() {
        assert!(SetupFailure::RetryFollowingDriver.follows_up());
        assert!(SetupFailure::PickFollowingDriver.follows_up());
        assert!(SetupFailure::RevertPeriod(Some(256)).follows_up());
        assert!(SetupFailure::StandIn.follows_up());
        assert!(!SetupFailure::Fail.follows_up());
    }

    #[test]
    fn a_period_change_counts_only_when_the_driver_runs_another_size() {
        assert_eq!(
            first_run_after_change(Some(256), Some(256), Some(32)),
            FirstRun::PeriodChange {
                previous: Some(256)
            }
        );
        assert_eq!(
            first_run_after_change(None, Some(256), Some(32)),
            FirstRun::PeriodChange { previous: None }
        );
        assert_eq!(
            first_run_after_change(None, Some(256), Some(256)),
            FirstRun::Other,
            "the same size snapped twice has nothing to revert to"
        );
    }
}
