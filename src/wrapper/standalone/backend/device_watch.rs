//! Which requested audio devices the backend could not open, and when to try them again.
//!
//! A backend that had to stand a device in (the system default for a missing output, no capture for
//! a missing input) keeps the names it actually wanted here. While anything is wanted, `run()` polls
//! the host's device NAMES — a cheap listing, no configuration queries — and asks for a probe of a
//! wanted device the moment it shows up. The run itself ends only once a probe passes: a device
//! that is listed but not yet ready, or that cannot run the session's stream, is probed again
//! later, later each time, so it never costs the stand-in stream a restart.

use std::time::{Duration, Instant};

/// Which direction a wanted device is opened for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Output,
    Input,
}

/// A requested device the backend has not opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wanted {
    pub name: String,
    pub kind: Kind,
}

struct Watched {
    wanted: Wanted,
    present: bool,
    /// `None`: probe as soon as the device is listed.
    next_probe: Option<Instant>,
    /// The pause after the next failed probe.
    retry: Duration,
}

impl Watched {
    fn new(wanted: Wanted) -> Self {
        Self {
            wanted,
            present: false,
            next_probe: None,
            retry: DeviceWatch::FIRST_RETRY,
        }
    }

    fn probe_failed(&mut self, now: Instant) {
        self.next_probe = Some(now + self.retry);
        self.retry = (self.retry * 2).min(DeviceWatch::MAX_RETRY);
    }
}

pub struct DeviceWatch {
    watched: Vec<Watched>,
}

impl DeviceWatch {
    /// The pause after a probe that failed; it doubles after each further failure.
    pub const FIRST_RETRY: Duration = Duration::from_secs(2);
    /// The longest pause between two probes of a device that keeps refusing.
    pub const MAX_RETRY: Duration = Duration::from_secs(15);

    /// `absent` are the requested devices the host did not list at the attempt, `refused` the ones
    /// it listed but that would not open.
    pub fn new(absent: Vec<Wanted>, refused: Vec<Wanted>, now: Instant) -> Self {
        let mut watched: Vec<Watched> = absent.into_iter().map(Watched::new).collect();
        for wanted in refused {
            let mut entry = Watched::new(wanted);
            entry.present = true;
            entry.probe_failed(now);
            watched.push(entry);
        }
        Self { watched }
    }

    /// Nothing to wait for: every requested device is open.
    pub fn is_idle(&self) -> bool {
        self.watched.is_empty()
    }

    /// A fresh device listing: the wanted devices worth a probe now. An absent device the moment it
    /// is listed, a device that failed a probe once its pause is over. A device that left the
    /// listing starts over when it is back.
    pub fn due(&mut self, names: &[String], now: Instant) -> Vec<Wanted> {
        let mut due = Vec::new();
        for entry in &mut self.watched {
            let listed = names.contains(&entry.wanted.name);
            if listed != entry.present {
                entry.present = listed;
                entry.next_probe = None;
                entry.retry = Self::FIRST_RETRY;
            }
            if listed && entry.next_probe.map_or(true, |at| now >= at) {
                due.push(entry.wanted.clone());
            }
        }
        due
    }

    /// The probe of `name` failed: try it again later, and later each time.
    pub fn probe_failed(&mut self, name: &str, now: Instant) {
        for entry in &mut self.watched {
            if entry.wanted.name == name {
                entry.probe_failed(now);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn output(name: &str) -> Wanted {
        Wanted {
            name: name.to_string(),
            kind: Kind::Output,
        }
    }

    fn input(name: &str) -> Wanted {
        Wanted {
            name: name.to_string(),
            kind: Kind::Input,
        }
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn nothing_wanted_is_idle_and_never_due() {
        let t0 = Instant::now();
        let mut watch = DeviceWatch::new(Vec::new(), Vec::new(), t0);
        assert!(watch.is_idle());
        assert!(watch
            .due(&names(&["Speakers", "Apollo"]), t0 + secs(2))
            .is_empty());
        assert!(watch
            .due(&names(&["Speakers", "Apollo"]), t0 + secs(100))
            .is_empty());
    }

    #[test]
    fn an_absent_device_is_due_the_moment_it_appears() {
        let t0 = Instant::now();
        let mut watch = DeviceWatch::new(vec![output("Apollo")], Vec::new(), t0);
        assert!(!watch.is_idle());
        assert!(watch.due(&names(&["Speakers"]), t0 + secs(2)).is_empty());
        assert_eq!(
            watch.due(&names(&["Speakers", "Apollo"]), t0 + secs(4)),
            vec![output("Apollo")]
        );
    }

    #[test]
    fn a_device_that_appeared_but_failed_its_probe_is_retried_after_the_first_interval() {
        let t0 = Instant::now();
        let here = names(&["Speakers", "Apollo"]);
        let mut watch = DeviceWatch::new(vec![output("Apollo")], Vec::new(), t0);
        assert_eq!(watch.due(&here, t0 + secs(2)), vec![output("Apollo")]);
        watch.probe_failed("Apollo", t0 + secs(2));
        assert!(watch
            .due(&here, t0 + secs(2) + DeviceWatch::FIRST_RETRY - secs(1))
            .is_empty());
        assert_eq!(
            watch.due(&here, t0 + secs(2) + DeviceWatch::FIRST_RETRY),
            vec![output("Apollo")]
        );
    }

    #[test]
    fn a_refused_device_is_retried_later_each_time_up_to_the_cap() {
        let t0 = Instant::now();
        let here = names(&["Speakers", "Apollo"]);
        let mut watch = DeviceWatch::new(Vec::new(), vec![output("Apollo")], t0);
        assert!(!watch.is_idle());
        // 2 s, then 4 s, 8 s, 15 s, 15 s after each failed probe.
        let mut due_at = t0 + secs(2);
        for gap in [secs(4), secs(8), secs(15), secs(15)] {
            assert!(watch.due(&here, due_at - secs(1)).is_empty());
            assert_eq!(watch.due(&here, due_at), vec![output("Apollo")]);
            watch.probe_failed("Apollo", due_at);
            due_at += gap;
        }
        assert!(watch.due(&here, due_at - secs(1)).is_empty());
        assert_eq!(watch.due(&here, due_at), vec![output("Apollo")]);
    }

    #[test]
    fn a_device_that_leaves_and_returns_is_due_at_once_and_starts_its_retries_over() {
        let t0 = Instant::now();
        let here = names(&["Speakers", "Apollo"]);
        let gone = names(&["Speakers"]);
        let mut watch = DeviceWatch::new(Vec::new(), vec![output("Apollo")], t0);
        assert_eq!(watch.due(&here, t0 + secs(2)), vec![output("Apollo")]);
        watch.probe_failed("Apollo", t0 + secs(2));
        // Next retry would be at 6 s; the device leaves before that.
        assert!(watch.due(&gone, t0 + secs(4)).is_empty());
        assert_eq!(watch.due(&here, t0 + secs(5)), vec![output("Apollo")]);
        watch.probe_failed("Apollo", t0 + secs(5));
        assert!(watch.due(&here, t0 + secs(6)).is_empty());
        assert_eq!(watch.due(&here, t0 + secs(7)), vec![output("Apollo")]);
    }

    #[test]
    fn unrelated_device_churn_is_never_due() {
        let t0 = Instant::now();
        let mut watch = DeviceWatch::new(vec![output("Apollo")], Vec::new(), t0);
        assert!(watch
            .due(&names(&["Speakers", "AirPods"]), t0 + secs(2))
            .is_empty());
        assert!(watch.due(&names(&["Speakers"]), t0 + secs(4)).is_empty());
        assert!(watch.due(&names(&[]), t0 + secs(6)).is_empty());
    }

    #[test]
    fn each_wanted_device_is_due_on_its_own() {
        let t0 = Instant::now();
        let mut watch = DeviceWatch::new(vec![output("Apollo"), input("Mic")], Vec::new(), t0);
        assert_eq!(
            watch.due(&names(&["Speakers", "Mic"]), t0 + secs(2)),
            vec![input("Mic")]
        );
        watch.probe_failed("Mic", t0 + secs(2));
        assert_eq!(
            watch.due(&names(&["Speakers", "Mic", "Apollo"]), t0 + secs(3)),
            vec![output("Apollo")]
        );
        assert_eq!(
            watch.due(&names(&["Speakers", "Mic", "Apollo"]), t0 + secs(4)),
            vec![output("Apollo"), input("Mic")]
        );
    }
}
