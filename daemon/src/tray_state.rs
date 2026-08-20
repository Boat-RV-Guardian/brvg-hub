//! The tray monitor's decision layer: what the user should see, and when to interrupt them.
//!
//! WHY THIS EXISTS AT ALL (measured, 2026-08-19/20, on CENTRAL)
//! An installer cannot report a failure that happens after it exits, and that is the failure this
//! product actually has. Three times in one night a hub install reported success and was then
//! taken apart by security software seconds later:
//!
//!   - `/S` returned exit 0, the self-check in the installer passed, `uninstall-hub.exe` was
//!     written — and ~8s later the binary and the scheduled task were both gone.
//!   - A later attempt could not even start: the setup.exe itself had been quarantined on sight.
//!
//! Nothing that exits can catch that. Something resident can, which is the whole job here.
//!
//! THE SECOND REASON, which is worse because it recurs: an AV exclusion is per FILE, and every
//! release changes the binary's hash even when no source changed (0.3.1 `a82f5d3b…` -> 0.3.2
//! `73741e2d…`, installer.nsi the only diff). So a hub that a user allowed once can be removed
//! again by the next update, silently, forever. A monitor notices; a user does not.
//!
//! This module is PURE on purpose — no HTTP, no tray, no Windows. The shell polls and calls
//! `Monitor::observe`; every judgement about icons and interruptions is decided here where it can
//! be tested on any platform, which matters because the Windows half can only be built in CI.

/// What a single poll saw. The shell fills this in; it does no interpretation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    /// The hub answered `GET /api/hub/ping`.
    pub answering: bool,
    /// From the ping body — a hub can be running but not yet signed to a vehicle.
    pub registered: bool,
    /// `%ProgramData%\BoatRVGuardian\bin\brvg-hub.exe` exists.
    pub binary_present: bool,
    /// The `BoatRVGuardianHub` scheduled task is registered.
    pub task_present: bool,
}

/// What the icon should say at a glance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    /// Watching, signed to a vehicle.
    Ok,
    /// Running but not signed to a vehicle yet — working, but not doing anything for anyone.
    NeedsSigning,
    /// Should be running and is not.
    Bad,
    /// No hub on this machine. Not an error, and never an interruption.
    Absent,
}

/// A notification worth interrupting the user for. `None` is the common case — a monitor that
/// talks on every poll gets muted, and then it is worth nothing on the night it matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alert {
    /// The task is still registered but the program file is gone. That combination does not
    /// happen by accident — an uninstall removes both. It is the quarantine signature we measured.
    RemovedBySecuritySoftware,
    /// The program is there and simply is not running: crashed, stopped, or never started.
    Stopped,
    /// Back after being down. Worth saying so the user knows they can stop worrying.
    Recovered,
}

/// The user-facing text for an alert. Kept here, next to the decision, so the two cannot drift —
/// and deliberately close to the installer's wording, because a user who saw one may see the other.
pub fn alert_text(a: Alert) -> (&'static str, &'static str) {
    match a {
        Alert::RemovedBySecuritySoftware => (
            "The hub was removed",
            "Your security software has removed the Boat & RV Guardian hub, so this vehicle is no \
             longer being watched. Open your antivirus, find the blocked or quarantined item named \
             brvg-hub, and choose Allow or Restore. Then reinstall the hub.",
        ),
        Alert::Stopped => (
            "The hub has stopped",
            "The Boat & RV Guardian hub is installed but not running, so this vehicle is not being \
             watched. Use Start hub from this menu, or restart the computer.",
        ),
        Alert::Recovered => (
            "The hub is running again",
            "Boat & RV Guardian is watching this vehicle again.",
        ),
    }
}

/// Tracks state across polls so alerts fire on CHANGE, not on condition.
#[derive(Debug, Default)]
pub struct Monitor {
    last: Option<Icon>,
    /// Whether a working hub has ever been seen on this machine, in this session. This is what
    /// separates "the AV took it" from "there is no hub here" when BOTH the binary and the task
    /// are missing — the end state we actually measured, where nothing is left to point at.
    seen_working: bool,
}

impl Monitor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one observation in. Returns the icon to show and, at most, one alert to raise.
    pub fn observe(&mut self, o: &Observation) -> (Icon, Option<Alert>) {
        let icon = self.classify(o);
        if icon == Icon::Ok || icon == Icon::NeedsSigning {
            self.seen_working = true;
        }

        // First observation establishes a baseline and never interrupts. Logging in to be told
        // something has been true since before you arrived is noise, not news — the icon already
        // says it, and the one case worth shouting about (a hub that DISAPPEARS) is a change by
        // definition.
        let alert = match self.last {
            None => None,
            Some(prev) if prev == icon => None,
            Some(prev) => transition_alert(prev, icon),
        };

        self.last = Some(icon);
        (icon, alert)
    }

    fn classify(&self, o: &Observation) -> Icon {
        if o.answering {
            return if o.registered {
                Icon::Ok
            } else {
                Icon::NeedsSigning
            };
        }
        // Not answering. Is there supposed to be a hub here at all?
        if o.task_present || o.binary_present || self.seen_working {
            Icon::Bad
        } else {
            Icon::Absent
        }
    }

    /// The reason a bad state is bad, for the alert text. Separate from `classify` because the
    /// icon only has to say "something is wrong" while the message has to be actionable.
    pub fn diagnose(o: &Observation) -> Alert {
        // Task registered, program file gone. An uninstall takes both; a crash takes neither.
        if o.task_present && !o.binary_present {
            Alert::RemovedBySecuritySoftware
        } else if !o.binary_present {
            // Everything gone, and `seen_working` got us here, so it was present before.
            Alert::RemovedBySecuritySoftware
        } else {
            Alert::Stopped
        }
    }
}

fn transition_alert(prev: Icon, now: Icon) -> Option<Alert> {
    match (prev, now) {
        // Into trouble. The caller pairs this with `Monitor::diagnose` for the specific reason.
        (Icon::Ok | Icon::NeedsSigning, Icon::Bad) => Some(Alert::Stopped),
        (Icon::Absent, Icon::Bad) => Some(Alert::Stopped),
        // Out of trouble.
        (Icon::Bad, Icon::Ok | Icon::NeedsSigning) => Some(Alert::Recovered),
        // Signing a hub to a vehicle, or a hub appearing for the first time, is something the user
        // just DID. Telling them it happened is the definition of a notification nobody wants.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(answering: bool, registered: bool, binary: bool, task: bool) -> Observation {
        Observation {
            answering,
            registered,
            binary_present: binary,
            task_present: task,
        }
    }
    const HEALTHY: fn() -> Observation = || obs(true, true, true, true);

    #[test]
    fn a_healthy_hub_shows_ok_and_says_nothing() {
        let mut m = Monitor::new();
        assert_eq!(m.observe(&HEALTHY()), (Icon::Ok, None));
        // ...and keeps saying nothing. A monitor that talks every poll gets muted.
        assert_eq!(m.observe(&HEALTHY()), (Icon::Ok, None));
        assert_eq!(m.observe(&HEALTHY()), (Icon::Ok, None));
    }

    #[test]
    fn a_hub_that_is_running_but_unsigned_is_distinct_from_broken() {
        // It works; it just is not doing anything for a vehicle yet. Telling the user it is BROKEN
        // would send them debugging something that is merely unfinished.
        let mut m = Monitor::new();
        assert_eq!(
            m.observe(&obs(true, false, true, true)),
            (Icon::NeedsSigning, None)
        );
    }

    #[test]
    fn the_first_observation_never_interrupts() {
        // Logging in to a notification about a condition that predates you is noise. The icon says
        // it; that is enough.
        for o in [
            HEALTHY(),
            obs(false, false, true, true),
            obs(false, false, false, false),
        ] {
            assert_eq!(
                Monitor::new().observe(&o).1,
                None,
                "first poll alerted for {o:?}"
            );
        }
    }

    #[test]
    fn a_machine_with_no_hub_is_absent_not_broken_and_stays_silent() {
        // Every login on a laptop that never hosted a hub must not raise an alarm.
        let mut m = Monitor::new();
        for _ in 0..5 {
            assert_eq!(
                m.observe(&obs(false, false, false, false)),
                (Icon::Absent, None)
            );
        }
    }

    #[test]
    fn a_hub_that_goes_down_alerts_exactly_once() {
        let mut m = Monitor::new();
        m.observe(&HEALTHY());
        let (icon, alert) = m.observe(&obs(false, false, true, true));
        assert_eq!(icon, Icon::Bad);
        assert!(alert.is_some());
        // Still down 10 polls later: the state has not CHANGED, so it must not nag.
        for _ in 0..10 {
            assert_eq!(m.observe(&obs(false, false, true, true)), (Icon::Bad, None));
        }
    }

    #[test]
    fn recovery_is_reported() {
        let mut m = Monitor::new();
        m.observe(&HEALTHY());
        m.observe(&obs(false, false, true, true));
        assert_eq!(m.observe(&HEALTHY()), (Icon::Ok, Some(Alert::Recovered)));
    }

    #[test]
    fn task_registered_but_binary_gone_is_the_quarantine_signature() {
        // MEASURED on CENTRAL: Sophos blocked the schtasks call and quarantined the binary, and in
        // a later run let the install finish and removed the file afterwards. An UNINSTALL removes
        // both; a crash removes neither. Only security software leaves this exact pair.
        assert_eq!(
            Monitor::diagnose(&obs(false, false, false, true)),
            Alert::RemovedBySecuritySoftware
        );
    }

    #[test]
    fn binary_present_but_not_answering_is_merely_stopped() {
        // Crashed, stopped by hand, or still starting. Blaming antivirus here would send the user
        // to rummage in the wrong place.
        assert_eq!(
            Monitor::diagnose(&obs(false, false, true, true)),
            Alert::Stopped
        );
    }

    #[test]
    fn everything_gone_after_a_working_hub_still_reads_as_removal() {
        // The end state we actually measured: binary AND task both gone, nothing left to point at.
        // `seen_working` is what distinguishes it from a machine that never had a hub.
        let mut m = Monitor::new();
        m.observe(&HEALTHY());
        let vanished = obs(false, false, false, false);
        assert_eq!(
            m.observe(&vanished).0,
            Icon::Bad,
            "must not read as Absent once seen working"
        );
        assert_eq!(
            Monitor::diagnose(&vanished),
            Alert::RemovedBySecuritySoftware
        );
    }

    #[test]
    fn signing_a_hub_is_not_worth_a_notification() {
        // The user just did this, in the app, deliberately.
        let mut m = Monitor::new();
        m.observe(&obs(true, false, true, true));
        assert_eq!(m.observe(&HEALTHY()), (Icon::Ok, None));
    }

    #[test]
    fn alert_text_tells_the_user_what_to_do_about_it() {
        // Removal is the one the installer cannot report, so its text has to carry the whole
        // remedy — and must name the item to look for, not just say "check your antivirus".
        let (title, body) = alert_text(Alert::RemovedBySecuritySoftware);
        assert!(title.to_lowercase().contains("removed"));
        assert!(body.contains("brvg-hub"));
        assert!(body.to_lowercase().contains("quarantin"));
        // And it must never tell anyone to switch protection off.
        for a in [
            Alert::RemovedBySecuritySoftware,
            Alert::Stopped,
            Alert::Recovered,
        ] {
            let (_, b) = alert_text(a);
            let b = b.to_lowercase();
            assert!(
                !b.contains("disable"),
                "never advise disabling security software"
            );
            assert!(
                !b.contains("turn off"),
                "never advise turning protection off"
            );
        }
    }
}
