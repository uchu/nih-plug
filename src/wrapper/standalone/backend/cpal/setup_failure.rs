//! A buffer size the ASIO driver listed and then refused at CreateBuffers or Start fails only
//! inside a run: what the reopen after that run does (E3), and at launch the walk on to the next
//! listed driver (§ 6 f). At launch a driver that starts its stream and never calls back fails
//! the same way (`run_ended`).

use anyhow::Result;

pub(super) use super::target::LaunchWalk;
use super::{run_outcome_after, Lapse, RunEnd, RunOutcome};

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
    /// launch walked on to starts none (a stream that never calls back counting as none): the
    /// next listed driver the launch has not tried opens, following the driver.
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

/// Whether the run that started as `first_run` counts as one whose stream did not start: it
/// was not built or did not start (`setup_failed`), or, in a launch's run (the launch, its retry,
/// a driver the walk opened, the driver a stand-in that opened nothing reloaded), the driver
/// started its stream and the liveness check gave up before its first callback
/// (`never_called_back`). Such a launch run goes on as for a refused stream: the retry, the walk
/// on, the stand-in. After a pick or a change a driver that never calls back is a failed stream,
/// left not running with its control panel at hand, as the user who picked it may need to set
/// its clock source there; one that called back once is a stream that died, wherever it ran.
fn stream_not_started(first_run: FirstRun, setup_failed: bool, never_called_back: bool) -> bool {
    setup_failed || (never_called_back && matches!(first_run, FirstRun::Launch { .. }))
}

/// How a run ended, decided by `run_ended` for `run()` and the launch tests alike.
#[derive(Clone, Copy, Debug)]
pub(super) struct Ended {
    /// What the run saw, `setup_failed` counting a launch's driver that never called back.
    pub end: RunEnd,
    /// The open the run belonged to goes on with its stream that did not start.
    pub follows_up: bool,
    pub outcome: RunOutcome,
    /// The output called back at least once in the run.
    pub called_back: bool,
}

/// How the run that started as `first_run` ended: `end` as its streams left it, `lapse` how the
/// duplex liveness check gave up on it, if it did, and `called_back` whether its output called
/// back.
pub(super) fn run_ended(
    first_run: FirstRun,
    end: RunEnd,
    lapse: Option<Lapse>,
    called_back: bool,
) -> Ended {
    let never_called_back = lapse == Some(Lapse::NeverCalledBack);
    let end = RunEnd {
        setup_failed: stream_not_started(first_run, end.setup_failed, never_called_back),
        ..end
    };
    let follows_up = end.setup_failed && setup_failure_action(first_run).follows_up();
    Ended {
        end,
        follows_up,
        outcome: run_outcome_after(end, follows_up),
        called_back,
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

    /// The run that started as `first_run` ended as `ended`. A stream that did not start leaves
    /// `reinit()` its follow-up. A launch's run that the driver restarted (a reset request, a rate
    /// that moved) before its first callback leaves the launch pending for the restart's run, so
    /// a driver that then stays silent is still followed up as one that did not start.
    pub(super) fn on_run_end(&mut self, first_run: FirstRun, ended: &Ended) {
        if ended.end.setup_failed {
            self.on_setup_failed(first_run);
        } else if ended.outcome == RunOutcome::Restart
            && !ended.called_back
            && matches!(first_run, FirstRun::Launch { .. })
        {
            self.first_run = first_run;
        }
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

    /// The reopen `reinit()` made for `after` did not open. The launch retry's reload
    /// (`SetupFailure::RetryFollowingDriver`) is refused as a second stream that did not start
    /// is, so the same `reinit()` walks on, or stands the shared host in once the walk is spent.
    /// Any other reopen that did not open recovers as a failed reopen (`Fail`), nothing changed.
    pub(super) fn on_reopen_refused(&mut self, after: SetupFailure) -> SetupFailure {
        if after != SetupFailure::RetryFollowingDriver {
            return SetupFailure::Fail;
        }
        self.pending = setup_failure_action(FirstRun::Launch { retried: true });
        self.on_reinit()
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
    use super::super::{published_after_run_end, Liveness, DUPLEX_LIVENESS_TIMEOUT_MS};
    use super::*;
    use crate::wrapper::standalone::recovery::{Action, Recovery};
    use crate::wrapper::standalone::AudioDevicesInUse;
    use std::time::Duration;

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
        /// This driver's stream died after it called back: the ordinary recovery takes over.
        Died(String),
    }

    /// How a driver's stream fares in a run of it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Stream {
        Plays,
        /// CreateBuffers or Start refused.
        Refused,
        /// Started, and never called back before the liveness check gave up.
        Silent,
        /// Called back, then fell silent.
        Dies,
        /// Asked for a reset (kAsioResetRequest) before its first callback.
        Resets,
    }

    /// Where a duplex launch ended, every load in order, the driver published as refused, and
    /// what the wrapper made of the runs that ended: the pauses before each reopen, whether it
    /// waited for the hardware, how many stream deaths it was charged and how many runs left the
    /// driver published as not running.
    struct Launched {
        ends: Ends,
        loads: Vec<String>,
        refused: Option<String>,
        paused: Duration,
        waited_for_hardware: bool,
        charged: u32,
        not_running: u32,
    }

    fn launch(
        requested: Option<&str>,
        listed: &[&str],
        opens: &[&str],
        streams: &[&str],
    ) -> (Ends, Vec<String>, Option<String>) {
        let launched = launch_with(requested, listed, opens, streams, true);
        (launched.ends, launched.loads, launched.refused)
    }

    /// `launch_runs` where a driver in `streams` plays and every other one refuses its stream.
    fn launch_with(
        requested: Option<&str>,
        listed: &[&str],
        opens: &[&str],
        streams: &[&str],
        reload_opens: bool,
    ) -> Launched {
        let stream = |name: &str, _| {
            if streams.contains(&name) {
                Stream::Plays
            } else {
                Stream::Refused
            }
        };
        launch_runs(requested, listed, opens, stream, reload_opens)
    }

    /// A duplex launch driven through the sequence the backend runs: the launch's own walk to the
    /// first driver that opens, then each run, `reinit()` and the walk on. Every run that did not
    /// play ends through `run_ended`, from a `Liveness` its callbacks stamped, and is reported to
    /// the wrapper's `Recovery` as `run()` reports it. A driver in `opens` loads and opens (its
    /// launch retry's reload only when `reload_opens`); `stream` says how its stream fares in its
    /// nth run.
    fn launch_runs(
        requested: Option<&str>,
        listed: &[&str],
        opens: &[&str],
        stream: impl Fn(&str, usize) -> Stream,
        reload_opens: bool,
    ) -> Launched {
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
        let mut launched = Launched {
            ends: Ends::StandIn,
            loads: Vec::new(),
            refused: None,
            paused: Duration::ZERO,
            waited_for_hardware: false,
            charged: 0,
            not_running: 0,
        };
        let (_, opened) = launch_duplex(requested, &listed, &mut open);
        let Ok((mut running, ())) = opened else {
            launched.loads = loads.into_inner();
            return launched;
        };
        let mut recovery =
            SetupRecovery::at_launch(true, LaunchWalk::after(requested, &listed, &running));
        let mut budget = Recovery::new();
        let mut runs: Vec<String> = Vec::new();
        for _ in 0..=3 * listed.len() {
            let first_run = recovery.on_run_start();
            let fares = stream(&running, runs.iter().filter(|&run| *run == running).count());
            runs.push(running.clone());
            if fares == Stream::Plays {
                launched.refused = recovery.walk_refused().map(str::to_string);
                let later = recovery.on_run_start();
                assert_eq!(later, FirstRun::Other, "the walk ends once a stream runs");
                recovery.on_setup_failed(later);
                assert_eq!(recovery.on_reinit(), SetupFailure::Fail);
                launched.ends = Ends::Runs(running);
                launched.loads = loads.into_inner();
                return launched;
            }
            let liveness = Liveness::new();
            liveness.stamp();
            if fares == Stream::Dies {
                liveness.callback();
            }
            let lapse = match fares {
                Stream::Silent | Stream::Dies => {
                    let lapse =
                        liveness.lapse_at(liveness.now_ms() + DUPLEX_LIVENESS_TIMEOUT_MS + 1);
                    assert!(lapse.is_some(), "the liveness check gives up after 2 s");
                    lapse
                }
                _ => None,
            };
            let end = RunEnd {
                setup_failed: fares == Stream::Refused,
                stream_error: lapse.is_some(),
                restart: fares == Stream::Resets,
                ..RunEnd::default()
            };
            let ended = run_ended(first_run, end, lapse, liveness.called_back());
            recovery.on_run_end(first_run, &ended);
            let published = published_after_run_end(
                AudioDevicesInUse {
                    running: true,
                    ..AudioDevicesInUse::default()
                },
                ended.end,
                ended.follows_up,
            );
            if !published.running {
                launched.not_running += 1;
            }
            let action = match ended.outcome {
                RunOutcome::FollowUp => budget.follow_up(),
                RunOutcome::StreamFailed => {
                    launched.charged += 1;
                    budget.stream_failed(Duration::ZERO)
                }
                RunOutcome::Restart => budget.restart(Duration::ZERO),
                other => panic!("a stream that did not play ends as {other:?}"),
            };
            match action {
                Action::FollowUp => launched.paused += Recovery::FOLLOW_UP,
                Action::Reinit { backoff } => launched.paused += backoff,
                Action::ReinitNow => {}
                Action::WaitForHardware => launched.waited_for_hardware = true,
            }
            let mut action = recovery.on_reinit();
            if ended.outcome == RunOutcome::Restart {
                assert_eq!(
                    action,
                    SetupFailure::Fail,
                    "a restart reloads the same driver"
                );
                loads.borrow_mut().push(running.clone());
                continue;
            }
            if action == SetupFailure::RetryFollowingDriver {
                loads.borrow_mut().push(running.clone());
                if !reload_opens {
                    action = recovery.on_reopen_refused(action);
                }
            }
            match action {
                SetupFailure::RetryFollowingDriver => {}
                SetupFailure::WalkOn => match recovery.walk_on(&mut open, || false) {
                    Walked::Opened(name, ()) => running = name,
                    Walked::Spent => break,
                    Walked::Ended => unreachable!("nothing ends this walk"),
                },
                SetupFailure::StandIn => break,
                SetupFailure::Fail => {
                    launched.ends = Ends::Died(running);
                    launched.loads = loads.into_inner();
                    return launched;
                }
                other => panic!("a launch does not {other:?}"),
            }
        }
        assert_eq!(
            budget.stream_failed(Duration::ZERO),
            Action::Reinit {
                backoff: Recovery::BACKOFF[0]
            },
            "the stand-in starts with the whole budget"
        );
        launched.loads = loads.into_inner();
        launched
    }

    #[test]
    fn a_walk_over_drivers_that_open_and_never_stream_spends_no_stream_death_budget() {
        let listed = ["A", "B", "C", "D", "E", "F", "G"];
        let launched = launch_with(Some("A"), &listed, &listed, &[], true);
        assert_eq!(launched.ends, Ends::StandIn);
        assert_eq!(launched.loads, ["A", "A", "B", "C", "D", "E", "F", "G"]);
        assert!(!launched.waited_for_hardware);
        assert_eq!(
            launched.paused,
            Recovery::FOLLOW_UP * 8,
            "one short pause per reopen the launch makes"
        );
    }

    /// A launch where each driver in `listed` opens and fares in its nth run as the nth entry of
    /// its `runs` says, the last entry for every run after.
    fn launch_sequences(requested: Option<&str>, listed: &[&str], runs: &[&[Stream]]) -> Launched {
        let stream = |name: &str, nth: usize| {
            let fares = runs[listed.iter().position(|&l| l == name).unwrap()];
            fares[nth.min(fares.len() - 1)]
        };
        launch_runs(requested, listed, listed, stream, true)
    }

    /// A launch where each driver in `listed` opens and fares as `fares` says in every run.
    fn launch_fares(requested: Option<&str>, listed: &[&str], fares: &[Stream]) -> Launched {
        let runs: Vec<&[Stream]> = fares.iter().map(std::slice::from_ref).collect();
        launch_sequences(requested, listed, &runs)
    }

    #[test]
    fn a_saved_driver_that_starts_and_never_calls_back_walks_on_to_the_next_that_plays() {
        let launched = launch_fares(Some("A"), &["A", "B"], &[Stream::Silent, Stream::Plays]);
        assert_eq!(launched.ends, Ends::Runs("B".into()));
        assert_eq!(launched.loads, ["A", "A", "B"], "A is retried once");
        assert_eq!(launched.refused.as_deref(), Some("A"));
        assert_eq!(launched.charged, 0, "no stream death is charged");
        assert_eq!(launched.not_running, 0, "no 'not running' meanwhile");
        assert_eq!(launched.paused, Recovery::FOLLOW_UP * 2);
        let launched = launch_fares(
            Some("A"),
            &["A", "B", "C"],
            &[Stream::Refused, Stream::Silent, Stream::Plays],
        );
        assert_eq!(launched.ends, Ends::Runs("C".into()));
        assert_eq!(
            launched.loads,
            ["A", "A", "B", "C"],
            "a walked driver gets one run"
        );
        assert_eq!(launched.charged, 0);
    }

    #[test]
    fn a_launch_where_every_driver_starts_and_never_calls_back_stands_the_shared_host_in() {
        let launched = launch_fares(Some("A"), &["A", "B", "C"], &[Stream::Silent; 3]);
        assert_eq!(launched.ends, Ends::StandIn);
        assert_eq!(launched.loads, ["A", "A", "B", "C"]);
        assert_eq!(launched.charged, 0);
        assert_eq!(launched.not_running, 0);
        assert!(!launched.waited_for_hardware);
        let launched = launch_fares(None, &["A"], &[Stream::Silent]);
        assert_eq!(launched.ends, Ends::StandIn, "a lone silent driver");
        assert_eq!(launched.loads, ["A", "A"]);
    }

    #[test]
    fn a_driver_that_called_back_and_then_died_is_a_stream_death_and_ends_the_walk() {
        let launched = launch_fares(Some("A"), &["A", "B"], &[Stream::Dies, Stream::Plays]);
        assert_eq!(launched.ends, Ends::Died("A".into()));
        assert_eq!(launched.loads, ["A"], "no retry, no walk");
        assert_eq!(launched.charged, 1);
        assert_eq!(launched.not_running, 1);
        let launched = launch_fares(
            Some("A"),
            &["A", "B", "C"],
            &[Stream::Silent, Stream::Dies, Stream::Plays],
        );
        assert_eq!(launched.ends, Ends::Died("B".into()));
        assert_eq!(launched.loads, ["A", "A", "B"]);
        assert_eq!(launched.charged, 1);
    }

    #[test]
    fn a_launch_driver_that_resets_before_its_first_callback_is_still_walked_on_when_silent() {
        let launched = launch_sequences(
            Some("A"),
            &["A", "B"],
            &[&[Stream::Resets, Stream::Silent], &[Stream::Plays]],
        );
        assert_eq!(launched.ends, Ends::Runs("B".into()));
        assert_eq!(
            launched.loads,
            ["A", "A", "A", "B"],
            "the reset's reload, the retry, then the walk"
        );
        assert_eq!(launched.refused.as_deref(), Some("A"));
        assert_eq!(launched.charged, 0);
        assert_eq!(launched.not_running, 0);
    }

    #[test]
    fn a_reset_after_a_callback_uses_the_launch_up() {
        let mut recovery = at_launch(true);
        let first_run = recovery.on_run_start();
        let reset = RunEnd {
            restart: true,
            ..RunEnd::default()
        };
        let ended = run_ended(first_run, reset, None, true);
        assert_eq!(ended.outcome, RunOutcome::Restart);
        recovery.on_run_end(first_run, &ended);
        assert_eq!(recovery.on_reinit(), SetupFailure::Fail);
        assert_eq!(recovery.on_run_start(), FirstRun::Other);
        let mut recovery = at_launch(true);
        recovery.on_run_start();
        recovery.on_change();
        recovery.on_change_opened(FirstRun::Pick);
        let first_run = recovery.on_run_start();
        recovery.on_run_end(first_run, &run_ended(first_run, reset, None, false));
        assert_eq!(recovery.on_reinit(), SetupFailure::Fail);
        assert_eq!(
            recovery.on_run_start(),
            FirstRun::Other,
            "only a launch's run is kept"
        );
    }

    #[test]
    fn a_driver_that_never_calls_back_after_a_pick_or_a_change_is_a_failed_stream() {
        let silent = RunEnd {
            stream_error: true,
            ..RunEnd::default()
        };
        for retried in [false, true] {
            let ended = run_ended(
                FirstRun::Launch { retried },
                silent,
                Some(Lapse::NeverCalledBack),
                false,
            );
            assert_eq!(ended.outcome, RunOutcome::FollowUp);
        }
        let mut recovery = at_launch(true);
        recovery.on_run_start();
        recovery.on_change();
        recovery.on_change_opened(first_run_after_pick(true, Some(64)));
        let pick = recovery.on_run_start();
        assert_eq!(pick, FirstRun::Pick);
        for first_run in [
            pick,
            FirstRun::PeriodChange {
                previous: Some(256),
            },
            FirstRun::Other,
        ] {
            let ended = run_ended(first_run, silent, Some(Lapse::NeverCalledBack), false);
            assert!(!ended.end.setup_failed, "{first_run:?}");
            assert!(!ended.follows_up, "{first_run:?}");
            assert_eq!(ended.outcome, RunOutcome::StreamFailed, "{first_run:?}");
            let refused = RunEnd {
                setup_failed: true,
                ..RunEnd::default()
            };
            assert!(run_ended(first_run, refused, None, false).end.setup_failed);
        }
        recovery.on_run_end(
            pick,
            &run_ended(pick, silent, Some(Lapse::NeverCalledBack), false),
        );
        assert_eq!(
            recovery.on_reinit(),
            SetupFailure::Fail,
            "a pick is not walked on"
        );
        let ended = run_ended(
            FirstRun::Launch { retried: false },
            silent,
            Some(Lapse::FellSilent),
            true,
        );
        assert_eq!(
            ended.outcome,
            RunOutcome::StreamFailed,
            "it called back once"
        );
    }

    #[test]
    fn a_launch_retry_whose_reload_does_not_open_walks_on_at_once() {
        let launched = launch_with(Some("A"), &["A", "B", "C"], &["A", "B"], &["B"], false);
        assert_eq!(launched.ends, Ends::Runs("B".into()));
        assert_eq!(launched.loads, ["A", "A", "B"]);
        assert_eq!(launched.refused.as_deref(), Some("A"));
        assert!(!launched.waited_for_hardware);
        let launched = launch_with(Some("A"), &["A"], &["A"], &[], false);
        assert_eq!(
            launched.ends,
            Ends::StandIn,
            "no other driver: the stand-in"
        );
        assert_eq!(launched.loads, ["A", "A"]);
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
    fn a_launch_retry_that_did_not_reopen_walks_on_or_stands_in() {
        let listed = names(&["A", "B"]);
        let mut recovery =
            SetupRecovery::at_launch(true, LaunchWalk::after(Some("A"), &listed, "A"));
        let first_run = recovery.on_run_start();
        recovery.on_setup_failed(first_run);
        assert_eq!(recovery.on_reinit(), SetupFailure::RetryFollowingDriver);
        assert_eq!(
            recovery.on_reopen_refused(SetupFailure::RetryFollowingDriver),
            SetupFailure::WalkOn
        );
        let mut recovery = at_launch(true);
        let first_run = recovery.on_run_start();
        recovery.on_setup_failed(first_run);
        assert_eq!(recovery.on_reinit(), SetupFailure::RetryFollowingDriver);
        assert_eq!(
            recovery.on_reopen_refused(SetupFailure::RetryFollowingDriver),
            SetupFailure::StandIn
        );
        assert_eq!(recovery.on_run_start(), FirstRun::Other);
    }

    #[test]
    fn any_other_reopen_that_did_not_open_recovers_as_a_failed_reopen() {
        for first_run in [
            FirstRun::Pick,
            FirstRun::PeriodChange {
                previous: Some(256),
            },
        ] {
            let mut recovery = at_launch(true);
            recovery.on_run_start();
            recovery.on_change_opened(first_run);
            let first_run = recovery.on_run_start();
            recovery.on_setup_failed(first_run);
            let after = recovery.on_reinit();
            assert_ne!(after, SetupFailure::RetryFollowingDriver);
            let before = recovery.clone();
            assert_eq!(recovery.on_reopen_refused(after), SetupFailure::Fail);
            assert_eq!(recovery, before, "{after:?} leaves the recovery as it was");
        }
        let mut recovery = at_launch(true);
        recovery.on_run_start();
        let before = recovery.clone();
        assert_eq!(
            recovery.on_reopen_refused(SetupFailure::Fail),
            SetupFailure::Fail
        );
        assert_eq!(recovery, before);
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
