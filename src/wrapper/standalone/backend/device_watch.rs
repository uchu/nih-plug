//! Which requested audio devices the backend could not open, and when to try them again.
//!
//! A backend that had to stand a device in (the system default for a missing output, no capture for
//! a missing input) keeps the names it actually wanted here. While anything is wanted, `run()` polls
//! the host's device NAMES — a cheap listing, no configuration queries — and asks the wrapper to
//! reinitialise the moment a wanted device shows up. A wanted device that is present but refused to
//! open is retried on a slow timer instead, so a device that cannot run the session stream costs one
//! attempt per interval rather than a hot loop.

use std::time::{Duration, Instant};

pub struct DeviceWatch {
    wanted: Vec<String>,
    seen: Vec<String>,
    last_attempt: Instant,
}

impl DeviceWatch {
    /// How often a wanted device that is present but would not open is tried again.
    pub const RETRY_INTERVAL: Duration = Duration::from_secs(15);

    /// `wanted` are the requested devices that are not open; `seen` the device names on the host
    /// right after the attempt that left them unopened.
    pub fn new(wanted: Vec<String>, seen: Vec<String>, now: Instant) -> Self {
        Self {
            wanted,
            seen,
            last_attempt: now,
        }
    }

    /// Nothing to wait for: every requested device is open.
    pub fn is_idle(&self) -> bool {
        self.wanted.is_empty()
    }

    /// A fresh device listing. `true` means a wanted device deserves a new attempt: it appeared
    /// since the previous listing, or it has been present without opening for `RETRY_INTERVAL`.
    pub fn poll(&mut self, names: &[String], now: Instant) -> bool {
        let retry_due = now.duration_since(self.last_attempt) >= Self::RETRY_INTERVAL;
        let fire = self
            .wanted
            .iter()
            .any(|wanted| names.contains(wanted) && (retry_due || !self.seen.contains(wanted)));
        self.seen.clear();
        self.seen.extend_from_slice(names);
        if fire {
            self.last_attempt = now;
        }
        fire
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn nothing_wanted_never_fires() {
        let t0 = Instant::now();
        let mut watch = DeviceWatch::new(Vec::new(), names(&["Speakers"]), t0);
        assert!(watch.is_idle());
        assert!(!watch.poll(&names(&["Speakers", "Apollo"]), t0 + Duration::from_secs(1)));
        assert!(!watch.poll(
            &names(&["Speakers", "Apollo"]),
            t0 + Duration::from_secs(100)
        ));
    }

    #[test]
    fn a_missing_device_fires_the_moment_it_appears() {
        let t0 = Instant::now();
        let mut watch = DeviceWatch::new(names(&["Apollo"]), names(&["Speakers"]), t0);
        assert!(!watch.is_idle());
        assert!(!watch.poll(&names(&["Speakers"]), t0 + Duration::from_secs(2)));
        assert!(watch.poll(&names(&["Speakers", "Apollo"]), t0 + Duration::from_secs(4)));
    }

    #[test]
    fn unrelated_device_churn_does_not_fire() {
        let t0 = Instant::now();
        let mut watch = DeviceWatch::new(names(&["Apollo"]), names(&["Speakers"]), t0);
        assert!(!watch.poll(
            &names(&["Speakers", "AirPods"]),
            t0 + Duration::from_secs(2)
        ));
        assert!(!watch.poll(&names(&["Speakers"]), t0 + Duration::from_secs(4)));
        assert!(!watch.poll(&names(&[]), t0 + Duration::from_secs(6)));
    }

    #[test]
    fn a_present_but_refused_device_is_retried_on_the_slow_timer_only() {
        let t0 = Instant::now();
        let here = names(&["Speakers", "Apollo"]);
        let mut watch = DeviceWatch::new(names(&["Apollo"]), here.clone(), t0);
        assert!(!watch.poll(&here, t0 + Duration::from_secs(2)));
        assert!(!watch.poll(
            &here,
            t0 + DeviceWatch::RETRY_INTERVAL - Duration::from_secs(1)
        ));
        assert!(watch.poll(&here, t0 + DeviceWatch::RETRY_INTERVAL));
        // The attempt the caller now makes restarts the timer.
        assert!(!watch.poll(
            &here,
            t0 + DeviceWatch::RETRY_INTERVAL + Duration::from_secs(2)
        ));
        assert!(watch.poll(&here, t0 + 2 * DeviceWatch::RETRY_INTERVAL));
    }

    #[test]
    fn a_device_that_flaps_fires_on_each_return() {
        let t0 = Instant::now();
        let mut watch = DeviceWatch::new(names(&["Apollo"]), names(&["Speakers"]), t0);
        assert!(watch.poll(&names(&["Speakers", "Apollo"]), t0 + Duration::from_secs(2)));
        assert!(!watch.poll(&names(&["Speakers"]), t0 + Duration::from_secs(4)));
        assert!(watch.poll(&names(&["Speakers", "Apollo"]), t0 + Duration::from_secs(6)));
    }

    #[test]
    fn either_of_two_wanted_devices_fires() {
        let t0 = Instant::now();
        let mut watch = DeviceWatch::new(names(&["Apollo", "Mic"]), names(&["Speakers"]), t0);
        assert!(watch.poll(&names(&["Speakers", "Mic"]), t0 + Duration::from_secs(2)));
    }
}
