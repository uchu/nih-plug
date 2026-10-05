//! What a live change opens (spec A16): the configuration it asks for, applied part by part to the
//! one that runs, and the rules that decide how the backend opens it.

use anyhow::Result;

use super::Start;
use crate::wrapper::standalone::change::AudioChange;
use crate::wrapper::standalone::{AudioDevicesInUse, FIRST_AVAILABLE_REFUSED};

/// A configuration the backend opens: the host, the requested devices and the ASIO buffer size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub driver: String,
    pub output: Option<String>,
    pub input: Option<String>,
    pub period: Option<u32>,
}

/// `change` applied part by part to `current`. A driver this build cannot run leaves the driver as
/// it is.
pub(crate) fn target_of(current: &Target, change: &AudioChange) -> Target {
    Target {
        driver: change
            .driver
            .clone()
            .filter(|id| host_id_for(id).is_some())
            .unwrap_or_else(|| current.driver.clone()),
        output: change
            .output
            .clone()
            .unwrap_or_else(|| current.output.clone()),
        input: change
            .input
            .clone()
            .unwrap_or_else(|| current.input.clone()),
        period: change.period.unwrap_or(current.period),
    }
}

/// The host behind a backend id, when this build can run it.
pub(crate) fn host_id_for(id: &str) -> Option<cpal::HostId> {
    match id {
        #[cfg(all(target_os = "windows", feature = "asio"))]
        "asio" => Some(cpal::HostId::Asio),
        #[cfg(target_os = "windows")]
        "wasapi" => Some(cpal::HostId::Wasapi),
        #[cfg(target_os = "macos")]
        "core-audio" => Some(cpal::HostId::CoreAudio),
        _ => None,
    }
}

/// Whether a reopen from the `current` driver to the `target` one releases or loads an ASIO
/// driver, which happens only on the GUI thread (A6, A7).
pub(crate) fn touches_asio(current: &str, target: &str) -> bool {
    current == "asio" || target == "asio"
}

/// Whether the host's default output stands in for a requested one that does not open. Never on a
/// duplex start that is not the launch: a restart reloads only the driver that was open, and a
/// picked driver that does not open brings the previous one back (A7, A16).
pub(super) fn stands_in_default(start: Start, duplex: bool) -> bool {
    !duplex || start == Start::Launch
}

/// How a reconfigure opens. A duplex change that picks no driver (a buffer size) reloads the
/// driver the stream runs on, as a restart does, so only a pick moves the stream (A7); every other
/// change opens as a launch does.
pub(super) fn reconfigure_start(duplex: bool, change: &AudioChange) -> Start {
    if duplex && change.output.is_none() {
        Start::Restart
    } else {
        Start::Reconfigure
    }
}

/// What a duplex change that opened nothing publishes in `refused_asio` while the previous driver
/// runs again (see `refused_name`). A change that picked no driver refuses none.
pub(super) fn refused_pick(change: &AudioChange) -> Option<String> {
    change.output.as_ref().map(refused_name)
}

/// An ASIO pick that opened nothing, as `refused_asio` names it: the driver, or
/// [`FIRST_AVAILABLE_REFUSED`] for "First available".
pub(super) fn refused_name(picked: &Option<String>) -> String {
    picked
        .clone()
        .unwrap_or_else(|| FIRST_AVAILABLE_REFUSED.to_string())
}

/// The refusals a failed change left that an open publishes again. A restart stays where that
/// change left the stream, on either host, so it keeps them; a launch or a change that opens
/// starts clean (A16, A18). A host's own device refusals are found afresh by every open.
pub(super) fn keep_refusals(
    start: Start,
    previous: &AudioDevicesInUse,
    in_use: &mut AudioDevicesInUse,
) {
    if start == Start::Restart {
        in_use.refused_driver = previous.refused_driver.clone();
        in_use.refused_asio = previous.refused_asio.clone();
    }
}

/// `refused_output` after a duplex open that is not the launch, which reopens a driver by name: a
/// restart onto a driver standing in for the requested one keeps the refusal that put it there,
/// and any other such open refuses nothing.
pub(super) fn duplex_refused_output(
    start: Start,
    requested_opened: bool,
    previous: &AudioDevicesInUse,
) -> Option<String> {
    if start == Start::Restart && !requested_opened {
        previous.refused_output.clone()
    } else {
        None
    }
}

/// The host a switch leaves the stream on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SwitchPlan {
    /// The host it switched to.
    Keep,
    /// The host it left, by backend id: a switch that opens nothing reopens it (A16, A18).
    RevertTo(String),
}

/// What follows a switch from the host `from` to the host `to` that `opened` or not. A change
/// that stays on one host is no switch and has nothing to go back to.
pub(crate) fn switch_plan(from: &str, to: &str, opened: bool) -> SwitchPlan {
    if opened || from == to {
        SwitchPlan::Keep
    } else {
        SwitchPlan::RevertTo(from.to_string())
    }
}

/// "First available": `names` in order, until `open` opens one. `open` lets go of a driver that
/// does not open before the next one is tried, as the SDK holds one driver at a time.
pub(super) fn first_that_opens<T>(
    names: &[String],
    mut open: impl FnMut(&str) -> Result<T>,
) -> Result<(String, T)> {
    for name in names {
        match open(name) {
            Ok(opened) => return Ok((name.clone(), opened)),
            Err(err) => nih_log!("First available: {err:#}"),
        }
    }
    anyhow::bail!("No ASIO driver opens")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(
        driver: &str,
        output: Option<&str>,
        input: Option<&str>,
        period: Option<u32>,
    ) -> Target {
        Target {
            driver: driver.into(),
            output: output.map(Into::into),
            input: input.map(Into::into),
            period,
        }
    }

    #[test]
    fn a_change_applies_part_by_part_to_the_current_target() {
        let current = target("asio", Some("Focusrite USB ASIO"), None, None);
        let next = target_of(
            &current,
            &AudioChange {
                period: Some(Some(128)),
                ..Default::default()
            },
        );
        assert_eq!(
            next,
            target("asio", Some("Focusrite USB ASIO"), None, Some(128))
        );
        let next = target_of(
            &next,
            &AudioChange {
                output: Some(None),
                ..Default::default()
            },
        );
        assert_eq!(next, target("asio", None, None, Some(128)));
    }

    #[test]
    fn an_unavailable_driver_id_leaves_the_driver_as_it_is() {
        let current = target("core-audio", None, None, None);
        let next = target_of(
            &current,
            &AudioChange {
                driver: Some("asio".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            next.driver,
            if cfg!(all(target_os = "windows", feature = "asio")) {
                "asio"
            } else {
                "core-audio"
            }
        );
    }

    #[test]
    fn only_an_asio_side_moves_the_reopen_to_the_gui_thread() {
        assert!(touches_asio("asio", "wasapi"));
        assert!(touches_asio("wasapi", "asio"));
        assert!(!touches_asio("wasapi", "wasapi"));
        assert!(!touches_asio("core-audio", "core-audio"));
    }

    #[test]
    fn a_duplex_reconfigure_never_stands_a_default_in() {
        assert!(!stands_in_default(Start::Reconfigure, true));
        assert!(!stands_in_default(Start::Restart, true));
        assert!(stands_in_default(Start::Launch, true));
        for start in [Start::Launch, Start::Restart, Start::Reconfigure] {
            assert!(stands_in_default(start, false));
        }
    }

    #[test]
    fn a_duplex_change_that_picks_no_driver_reloads_the_open_one() {
        let size = AudioChange {
            period: Some(Some(128)),
            ..Default::default()
        };
        assert_eq!(reconfigure_start(true, &size), Start::Restart);
        let first_available = AudioChange {
            output: Some(None),
            period: Some(Some(128)),
            ..Default::default()
        };
        assert_eq!(
            reconfigure_start(true, &first_available),
            Start::Reconfigure
        );
        assert_eq!(reconfigure_start(false, &size), Start::Reconfigure);
    }

    #[test]
    fn an_asio_pick_that_opens_nothing_is_published_as_refused() {
        let pick = |output| AudioChange {
            output,
            ..Default::default()
        };
        assert_eq!(
            refused_pick(&pick(Some(Some("Focusrite USB ASIO".into())))).as_deref(),
            Some("Focusrite USB ASIO")
        );
        assert_eq!(
            refused_pick(&pick(Some(None))).as_deref(),
            Some(FIRST_AVAILABLE_REFUSED)
        );
        assert_eq!(refused_pick(&pick(None)), None);
        assert_eq!(
            refused_name(&Some("Focusrite USB ASIO".into())),
            "Focusrite USB ASIO"
        );
        assert_eq!(refused_name(&None), FIRST_AVAILABLE_REFUSED);
    }

    #[test]
    fn refusals_last_through_restarts_on_either_host_and_clear_on_a_change_that_opens() {
        let previous = AudioDevicesInUse {
            refused_driver: Some("asio".into()),
            refused_asio: Some("Focusrite USB ASIO".into()),
            refused_output: Some("Speakers".into()),
            ..Default::default()
        };
        let mut restarted = AudioDevicesInUse::default();
        keep_refusals(Start::Restart, &previous, &mut restarted);
        assert_eq!(restarted.refused_driver.as_deref(), Some("asio"));
        assert_eq!(
            restarted.refused_asio.as_deref(),
            Some("Focusrite USB ASIO")
        );
        assert_eq!(
            restarted.refused_output, None,
            "a host finds its own refusals"
        );
        for start in [Start::Launch, Start::Reconfigure] {
            let mut opened = AudioDevicesInUse::default();
            keep_refusals(start, &previous, &mut opened);
            assert_eq!(opened, AudioDevicesInUse::default());
        }
    }

    #[test]
    fn a_duplex_restart_keeps_only_the_refusal_a_stand_in_driver_runs_for() {
        let previous = AudioDevicesInUse {
            refused_output: Some("Focusrite USB ASIO".into()),
            ..Default::default()
        };
        assert_eq!(
            duplex_refused_output(Start::Restart, false, &previous).as_deref(),
            Some("Focusrite USB ASIO")
        );
        assert_eq!(duplex_refused_output(Start::Restart, true, &previous), None);
        assert_eq!(
            duplex_refused_output(Start::Reconfigure, false, &previous),
            None
        );
    }

    #[test]
    fn a_host_switch_that_opens_nothing_reopens_the_previous_host() {
        let plan = switch_plan("wasapi", "asio", false);
        assert_eq!(plan, SwitchPlan::RevertTo("wasapi".into()));
        assert_eq!(switch_plan("wasapi", "asio", true), SwitchPlan::Keep);
        assert_eq!(switch_plan("asio", "wasapi", true), SwitchPlan::Keep);
        assert_eq!(
            switch_plan("asio", "wasapi", false),
            SwitchPlan::RevertTo("asio".into())
        );
    }

    #[test]
    fn first_available_takes_the_first_listed_driver_that_opens() {
        let names: Vec<String> = ["A", "B", "C"].iter().map(|n| n.to_string()).collect();
        let mut tried = Vec::new();
        let opened = first_that_opens(&names, |name| {
            tried.push(name.to_string());
            if name == "B" {
                Ok(name.to_lowercase())
            } else {
                Err(anyhow::anyhow!("'{name}' does not open"))
            }
        })
        .unwrap();
        assert_eq!(opened, ("B".to_string(), "b".to_string()));
        assert_eq!(tried, ["A", "B"]);
        assert!(first_that_opens(&names, |_| Err::<(), _>(anyhow::anyhow!("no"))).is_err());
        assert!(first_that_opens(&[], |_| Ok(())).is_err());
    }
}
