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
    /// The `BoatRVGuardianHub` Windows service is registered (`sc query` succeeds). Named
    /// `service_present` since the 2026-08-20 conversion from a scheduled task — the field means
    /// "the persistence entry exists", and the quarantine signature below reads the same either way.
    pub service_present: bool,
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
    /// The service is still registered but the program file is gone. That combination does not
    /// happen by accident — an uninstall removes both. It is the quarantine signature we measured
    /// (on the scheduled task; the service is expected to be flagged far less, but the detection
    /// costs nothing and defends against any AV that still does).
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

/// Escape a string for use as WINDOWS MENU text.
///
/// Windows menus treat `&` as the accelerator marker: it is swallowed and the next character gets
/// underlined. Our product name contains one, so the tray menu rendered "Boat  RV Guardian hub"
/// with a hole in it while the TOOLTIP — which has no mnemonics — showed it correctly. Caught in a
/// screenshot from CENTRAL, 2026-08-20.
///
/// It lives HERE rather than in the tray binary for the reason that file states about itself: the
/// Windows half only compiles in CI, so anything that can be tested should not live there. The bug
/// was invisible to every existing test because the STRING was right — only the surface differed.
pub fn for_menu(s: &str) -> String {
    s.replace('&', "&&")
}

/// The status colour for the tray icon's badge, as RGB. Presentation, but deterministic, so it
/// lives here with a test rather than in the Windows-only binary.
pub fn status_rgb(state: Icon) -> (u8, u8, u8) {
    match state {
        Icon::Ok => (0x22, 0xA5, 0x5A),           // green — watching
        Icon::NeedsSigning => (0xE0, 0x9B, 0x20), // amber — running, not signed to a vehicle
        Icon::Bad => (0xC8, 0x32, 0x32),          // red — should be running and is not
        Icon::Absent => (0x8A, 0x8A, 0x8A),       // grey — no hub here
    }
}

/// Paint a filled STATUS DOT into a `w×h` RGBA8 buffer (the decoded BRVG brand mark), lower-right,
/// with a 1px dark ring for contrast against a light logo. A solid dot reads at the 16px the tray
/// renders at, where a thin frame around the mark would vanish.
///
/// PURE and tested on host on purpose: the tray binary only compiles in CI, so the pixel maths —
/// the one part that can be wrong in a way no reviewer will spot — is verified here where every
/// platform runs it. `rgba` shorter than `w*h*4` is left untouched past its end rather than
/// panicking; the shell only ever passes a correctly sized buffer.
pub fn paint_status_dot(rgba: &mut [u8], w: u32, h: u32, state: Icon) {
    let (r, g, b) = status_rgb(state);
    let cx = w as f32 * 0.72;
    let cy = h as f32 * 0.72;
    let rad = w.min(h) as f32 * 0.26;
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            if i + 3 >= rgba.len() {
                continue;
            }
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let d = (dx * dx + dy * dy).sqrt();
            if d <= rad {
                rgba[i] = r;
                rgba[i + 1] = g;
                rgba[i + 2] = b;
                rgba[i + 3] = 0xFF;
            } else if d <= rad + 1.2 {
                rgba[i] = 0x12;
                rgba[i + 1] = 0x16;
                rgba[i + 2] = 0x1A;
                rgba[i + 3] = 0xFF;
            }
        }
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
    /// Whether a hub was ever even INSTALLED here — task or binary present, answering or not.
    ///
    /// Found the hard way on CENTRAL, 2026-08-20: an install completed, the task and binary
    /// existed for ~10 seconds, and Sophos removed both before the hub ever served a request. With
    /// only `seen_working` to go on, that machine reverted to `Absent` — "no hub here" — and the
    /// monitor went quiet about a hub that had just been destroyed in front of it.
    seen_installed: bool,
    /// The reason we last complained. A hub can stay `Bad` while the reason CHANGES underneath —
    /// stopped, then its program file quarantined — and the second fact is the one worth saying.
    last_reason: Option<Alert>,
}

impl Monitor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one observation in. Returns the icon to show and, at most, one alert to raise.
    pub fn observe(&mut self, o: &Observation) -> (Icon, Option<Alert>) {
        if o.service_present || o.binary_present {
            self.seen_installed = true;
        }
        let icon = self.classify(o);
        if icon == Icon::Ok || icon == Icon::NeedsSigning {
            self.seen_working = true;
        }

        // First observation establishes a baseline and never interrupts. Logging in to be told
        // something has been true since before you arrived is noise, not news — the icon says it.
        let first = self.last.is_none();
        let was_bad = self.last == Some(Icon::Bad);

        let alert = if first {
            None
        } else if icon == Icon::Bad {
            // Alert on the REASON changing, not merely the icon. A hub that stops and is THEN
            // quarantined never leaves `Bad`, so an icon-only rule would say "it stopped" and stay
            // silent about the removal — which is the half the user has to act on.
            let reason = Self::diagnose(o);
            if !was_bad || self.last_reason != Some(reason) {
                Some(reason)
            } else {
                None
            }
        } else if was_bad {
            Some(Alert::Recovered)
        } else {
            None
        };

        self.last_reason = if icon == Icon::Bad {
            alert.or(self.last_reason)
        } else {
            None
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
        // Not answering. Is a hub supposed to exist here at all? `seen_installed` is what stops a
        // hub wiped seconds after install from reading as "this machine never had one".
        if o.service_present || o.binary_present || self.seen_working || self.seen_installed {
            Icon::Bad
        } else {
            Icon::Absent
        }
    }

    /// The reason a bad state is bad, for the alert text. Separate from `classify` because the
    /// icon only has to say "something is wrong" while the message has to be actionable.
    pub fn diagnose(o: &Observation) -> Alert {
        // Service registered, program file gone. An uninstall takes both; a crash takes neither.
        if o.service_present && !o.binary_present {
            Alert::RemovedBySecuritySoftware
        } else if !o.binary_present {
            // Everything gone, and `seen_working` got us here, so it was present before.
            Alert::RemovedBySecuritySoftware
        } else {
            Alert::Stopped
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(answering: bool, registered: bool, binary: bool, service: bool) -> Observation {
        Observation {
            answering,
            registered,
            binary_present: binary,
            service_present: service,
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
    fn service_registered_but_binary_gone_is_the_quarantine_signature() {
        // MEASURED on CENTRAL (schtasks era): Sophos blocked the call and quarantined the binary,
        // and in a later run let the install finish and removed the file afterwards. An UNINSTALL
        // removes both; a crash removes neither. Only security software leaves this exact pair —
        // and it reads identically whether the persistence entry is a task or a service.
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
    fn a_hub_wiped_seconds_after_install_is_not_mistaken_for_never_installed() {
        // THE CENTRAL TIMELINE, 2026-08-20, verbatim: install exit 0, task + binary present at
        // t+5s and t+10s, both GONE by t+15s -- Sophos removed them before the hub ever answered a
        // single request. With only `seen_working` to go on the monitor called that machine
        // "Absent" and said nothing, which is the exact failure it exists to prevent.
        let mut m = Monitor::new();
        m.observe(&obs(false, false, false, false)); // before the install: genuinely no hub
        m.observe(&obs(false, false, true, true)); // t+5s: installed, not yet serving
        let (icon, alert) = m.observe(&obs(false, false, false, false)); // t+15s: wiped
        assert_eq!(
            icon,
            Icon::Bad,
            "a hub that existed and vanished is not 'never installed'"
        );
        assert_eq!(alert, Some(Alert::RemovedBySecuritySoftware));
    }

    #[test]
    fn a_stopped_hub_that_is_then_quarantined_says_so() {
        // Both facts matter and they arrive apart: first it stops, then its program file is taken.
        // The icon is `Bad` throughout, so an icon-only rule would report "it stopped" and never
        // mention the removal -- leaving the user restarting a service whose binary is gone.
        let mut m = Monitor::new();
        m.observe(&HEALTHY());
        assert_eq!(
            m.observe(&obs(false, false, true, true)).1,
            Some(Alert::Stopped)
        );
        assert_eq!(
            m.observe(&obs(false, false, false, true)).1,
            Some(Alert::RemovedBySecuritySoftware),
            "the escalation must be reported even though the icon never changed"
        );
        // ...and then it settles. Same reason, no repeat.
        assert_eq!(m.observe(&obs(false, false, false, true)).1, None);
    }

    #[test]
    fn the_status_dot_paints_the_center_and_leaves_the_far_corner_alone() {
        // A 32×32 buffer, pre-filled with an opaque sentinel so we can see what the dot changed.
        let (w, h) = (32u32, 32u32);
        let mut buf = vec![0x77u8; (w * h * 4) as usize];
        paint_status_dot(&mut buf, w, h, Icon::Ok);
        let (r, g, b) = status_rgb(Icon::Ok);
        // The dot centre (~0.72 across) is the status colour, fully opaque.
        let ci = (((h as f32 * 0.72) as u32 * w + (w as f32 * 0.72) as u32) * 4) as usize;
        assert_eq!(&buf[ci..ci + 4], &[r, g, b, 0xFF], "dot centre must be the status colour");
        // The top-left corner is nowhere near the lower-right dot — untouched sentinel.
        assert_eq!(&buf[0..4], &[0x77, 0x77, 0x77, 0x77], "far corner must be left alone");
    }

    #[test]
    fn the_status_dot_color_differs_per_state_so_the_glance_still_works() {
        // The whole point of keeping a dot on the brand mark: OK and Bad must not look the same.
        assert_ne!(status_rgb(Icon::Ok), status_rgb(Icon::Bad));
        assert_ne!(status_rgb(Icon::Ok), status_rgb(Icon::NeedsSigning));
        assert_ne!(status_rgb(Icon::Bad), status_rgb(Icon::Absent));
    }

    #[test]
    fn paint_status_dot_never_reads_past_a_short_buffer() {
        // Robustness: a mis-sized buffer must not panic the tray. Give it too small a slice.
        let mut tiny = vec![0u8; 10];
        paint_status_dot(&mut tiny, 32, 32, Icon::Bad); // must simply do nothing dangerous
    }

    #[test]
    fn menu_text_escapes_the_ampersand_in_our_own_product_name() {
        // The exact string that rendered as "Boat  RV Guardian hub" in the tray menu on CENTRAL.
        assert_eq!(
            for_menu("Boat & RV Guardian hub — watching this vehicle"),
            "Boat && RV Guardian hub — watching this vehicle"
        );
        // Every tooltip feeds the menu, so every one of them must survive the trip.
        for i in [Icon::Ok, Icon::NeedsSigning, Icon::Bad, Icon::Absent] {
            let raw = tooltip_for_test(i);
            let escaped = for_menu(raw);
            assert_eq!(
                escaped.matches("&&").count(),
                raw.matches('&').count(),
                "every & in {raw:?} must be doubled for the menu"
            );
            assert!(!escaped.contains("&&&"), "no over-escaping in {escaped:?}");
        }
    }

    /// Mirrors the shell's tooltip strings. Duplicated deliberately: the real ones live in the
    /// Windows-only binary, and a test that cannot run is not a test.
    fn tooltip_for_test(i: Icon) -> &'static str {
        match i {
            Icon::Ok => "Boat & RV Guardian hub — watching this vehicle",
            Icon::NeedsSigning => "Boat & RV Guardian hub — running, but not signed to a vehicle",
            Icon::Bad => "Boat & RV Guardian hub — NOT running. This vehicle is not being watched.",
            Icon::Absent => "Boat & RV Guardian hub — not installed on this computer",
        }
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
