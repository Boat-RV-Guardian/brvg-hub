//! The valve CYCLE STATE MACHINE — the logic the owner ruled "does not exist on the valve alone"
//! (2026-08-19). The hub owns it: watering plans are never written into the gateway, the hub
//! decides starts, stops, restarts and the daily ledger, and the gateway is just the actuator.
//!
//! PORTED from `hub/src/cycle.ts` (frozen on convergence) against `hub/test/cycle.test.ts`, whose
//! cases are mirrored below — the one-contract rule. Every rule here is an owner ruling:
//!   * THREE MODES ONLY: washdown = TIME limit and NO volume cap; tank fill = both; Normal/Daily =
//!     both (default 100 gal / 24 h). Do not invent a fourth.
//!   * A manual press (button / external open) ENTERS NORMAL RUN and adopts the profile's cap.
//!     With the app's control plane routed through the hub (owner ruling, same day), an open the
//!     hub did not start can ONLY be a button press — which is what makes this rule exact rather
//!     than a guess.
//!   * A cycle cut short by its VOLUME cap must NOT auto-restart.
//!   * Washdown volume does NOT count against the daily total.
//!   * A cycle already running when the hub starts is ADOPTED, never closed.
//!
//! PURE by construction: no timers, no I/O, no clock reads. The caller feeds timestamped
//! observations and gets back the new state plus what to DO — which is what makes the restart bug
//! testable instead of waiting for a boat to hit it.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Washdown,
    Tankfill,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Normal => "normal",
            Mode::Washdown => "washdown",
            Mode::Tankfill => "tankfill",
        }
    }
}

/// Why a cycle ended. `VolumeCap` and `Timer` drive the restart decision; `FloodShutoff` exists so
/// the event log can say what actually happened instead of "stopped".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    Timer,
    VolumeCap,
    Manual,
    FloodShutoff,
    Unknown,
}

impl EndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            EndReason::Timer => "timer",
            EndReason::VolumeCap => "volume_cap",
            EndReason::Manual => "manual",
            EndReason::FloodShutoff => "flood_shutoff",
            EndReason::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provenance {
    Hub,
    Adopted,
}

/// The Normal Run profile the hub holds for a valve — the source of the manual-press cap.
#[derive(Clone, Copy, Debug)]
pub struct Profile {
    pub duration_secs: u64,
    pub volume_cap_l: f64,
    pub auto_restart: bool,
}

#[derive(Clone, Debug)]
pub struct Running {
    pub mode: Mode,
    /// ms epoch when the cycle was observed to start (or adopted).
    pub started_at: i64,
    pub duration_secs: u64,
    /// Volume ceiling in LITRES, 0 = none. By the mode rules this is 0 exactly for washdown.
    pub volume_cap_l: f64,
    pub volume_l: f64,
    pub provenance: Provenance,
    /// Set once the hub has issued a stop it has not yet seen confirmed, so we do not double-stop.
    pub stop_issued: Option<EndReason>,
}

#[derive(Clone, Debug)]
pub enum State {
    Idle,
    Running(Running),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Ended {
    pub mode: Mode,
    pub ended_at: i64,
    pub reason: EndReason,
    pub volume_l: f64,
    pub provenance: Provenance,
}

#[derive(Clone, Copy, Debug)]
pub struct Observation {
    pub at: i64,
    pub watering: bool,
    /// Cycle volume in litres (0 when unknown/garbage — cycle_volume_litres already guards).
    pub volume_l: f64,
    pub remain_secs: Option<u64>,
    /// Instantaneous flow in LITRES PER MINUTE, 0 when unknown or not flowing. Feeds the cutoff's
    /// lead time — see `cutoff_trigger_l`. Zero simply disables the lead, so an old payload or a
    /// non-metering valve degrades to the previous trigger-at-the-cap behaviour rather than
    /// misfiring.
    pub speed_lpm: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Stop(EndReason),
}

pub struct StepResult {
    pub state: State,
    pub action: Action,
    /// Present exactly when this step closed a cycle — feed it to the ledger and event log.
    pub ended: Option<Ended>,
}

/// The hub started a cycle itself. Returns Err on the two combinations the owner has outlawed,
/// rather than silently "fixing" them — that is how the external-cap bug shipped last time.
pub fn start_hub_cycle(at: i64, mode: Mode, duration_secs: u64, volume_cap_l: f64) -> Result<State, String> {
    if mode == Mode::Washdown && volume_cap_l > 0.0 {
        return Err("washdown is time-limited only — a volume cap on washdown is the outlawed shape".into());
    }
    if mode != Mode::Washdown && volume_cap_l <= 0.0 {
        return Err(format!("{} must carry a volume cap — time-only runs are washdown by definition", mode.as_str()));
    }
    Ok(State::Running(Running {
        mode,
        started_at: at,
        duration_secs: duration_secs.max(1),
        volume_cap_l,
        volume_l: 0.0,
        provenance: Provenance::Hub,
        stop_issued: None,
    }))
}

/// Adopt a cycle the hub did not start. Owner rule: an external open IS a Normal Run and takes the
/// profile's cap.
pub fn adopt_cycle(at: i64, profile: &Profile, remain_secs: Option<u64>) -> Running {
    Running {
        mode: Mode::Normal,
        started_at: at,
        duration_secs: remain_secs.filter(|s| *s > 0).unwrap_or(profile.duration_secs),
        volume_cap_l: profile.volume_cap_l,
        volume_l: 0.0,
        provenance: Provenance::Adopted,
        stop_issued: None,
    }
}

/// Seconds of flow between ISSUING a stop and the valve actually closing.
///
/// 🔬 MEASURED ON HARDWARE (MVP GW-02, 2026-08-22), because guessing this number is what makes a
/// cap wrong: a watchdog fired at 2.02 gal and the valve did not close until 2.81 gal — 0.79 gal at
/// the 5.83 gal/min the gateway was reporting, i.e. ~8 s of flow. An earlier run on the same valve
/// agreed (stop at 10.5 gal, closed at 11.29). The latency is not one command's round trip: a
/// single `cmd 7` frequently returns `ret:0` while the valve keeps running, so the real close costs
/// a confirm-and-retry loop, and this constant covers that whole sequence.
pub const STOP_LATENCY_SECS: f64 = 8.0;

/// The volume at which the software cutoff must FIRE so the cycle LANDS on `cap_l`.
///
/// ⚠️ THE CAP IS NOT THE TRIGGER. The hardware ignores `volume_limit` entirely on GW-02 (proven
/// 2026-08-22: the field reads `0.00` during an active cycle, water crosses it with
/// `is_cutoff:false`, and clearing the watering plan changes nothing), so this software cutoff is
/// the ONLY volume enforcement that exists. Firing it AT the cap therefore guarantees an overshoot
/// of one stop-latency's worth of flow — measured at ~0.79 gal, so every "10 gallon" cap really
/// delivered ~11.
///
/// Lead by what will still flow: `speed × STOP_LATENCY`. The rate comes from the gateway's own
/// `speed` field per observation rather than a constant, so a trickle leads by almost nothing and a
/// full-bore fill leads by a lot — the correction tracks reality instead of assuming it.
///
/// Clamped at zero: when the cap is smaller than the overshoot the valve physically cannot deliver
/// it, and the honest answer is to stop at the first sign of flow rather than to pretend a floor
/// exists. `speed_lpm <= 0` (unknown, stale, or not yet flowing) yields the old behaviour exactly.
pub fn cutoff_trigger_l(cap_l: f64, speed_lpm: f64) -> f64 {
    if !cap_l.is_finite() || cap_l <= 0.0 {
        return 0.0;
    }
    if !speed_lpm.is_finite() || speed_lpm <= 0.0 {
        return cap_l;
    }
    let lead_l = speed_lpm * (STOP_LATENCY_SECS / 60.0);
    (cap_l - lead_l).max(0.0)
}

/// One observation step — the whole machine. Classification order matters: what the hub DID
/// outranks inference, because a volume-capped stop also looks "early" on the clock.
pub fn step(state: &State, obs: Observation, profile: &Profile) -> StepResult {
    match state {
        State::Idle => {
            if !obs.watering {
                return StepResult { state: State::Idle, action: Action::None, ended: None };
            }
            let mut r = adopt_cycle(obs.at, profile, obs.remain_secs);
            r.volume_l = obs.volume_l;
            StepResult { state: State::Running(r), action: Action::None, ended: None }
        }
        State::Running(cur) => {
            let mut next = cur.clone();
            if obs.volume_l > 0.0 {
                next.volume_l = obs.volume_l;
            }
            if obs.watering {
                // The software cutoff. The hardware "often ignores volume limits passed to cmd 6"
                // — measured inert on GW-02 — so THIS is the cap that actually holds. It fires
                // EARLY by the stop latency so the cycle lands on the cap instead of past it.
                let trigger_l = cutoff_trigger_l(next.volume_cap_l, obs.speed_lpm);
                if next.stop_issued.is_none() && next.volume_cap_l > 0.0 && next.volume_l >= trigger_l {
                    next.stop_issued = Some(EndReason::VolumeCap);
                    return StepResult {
                        state: State::Running(next),
                        action: Action::Stop(EndReason::VolumeCap),
                        ended: None,
                    };
                }
                return StepResult { state: State::Running(next), action: Action::None, ended: None };
            }
            // Closed — classify why.
            let elapsed = (obs.at - cur.started_at) / 1000;
            let reason = if let Some(r) = cur.stop_issued {
                r
            } else if next.volume_cap_l > 0.0 && next.volume_l >= next.volume_cap_l {
                EndReason::VolumeCap
            } else if elapsed >= cur.duration_secs as i64 - 60 {
                // Within a minute of the issued duration = the timer. The margin absorbs poll
                // jitter without reopening the old heuristic's hole — a volume stop was already
                // caught above, by evidence rather than by clock.
                EndReason::Timer
            } else {
                EndReason::Unknown
            };
            let ended = Ended {
                mode: cur.mode,
                ended_at: obs.at,
                reason,
                volume_l: next.volume_l,
                provenance: cur.provenance,
            };
            StepResult { state: State::Idle, action: Action::None, ended: Some(ended) }
        }
    }
}

/// ONLY a timer expiry of a NORMAL run restarts. Volume-capped, manual, flood-stopped or
/// unexplained ends never do — the first is the owner's explicit rule, the last is "when unsure,
/// spend no water".
pub fn should_auto_restart(ended: &Ended, auto_restart_enabled: bool) -> bool {
    auto_restart_enabled && ended.mode == Mode::Normal && ended.reason == EndReason::Timer
}

/// The daily ledger. Washdown does NOT count (owner rule); everything else does, including adopted
/// cycles — a manual hose run is exactly the water the daily number exists to see.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ledger {
    /// UTC day key, YYYY-MM-DD. Storage is UTC; display converts (house rule).
    pub day: String,
    pub volume_l: f64,
}

pub fn day_key(at_ms: i64) -> String {
    // Days since the epoch → civil date. Avoids a chrono dependency for one function.
    let days = at_ms.div_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's civil_from_days — the standard integer algorithm, no dependency.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn apply_to_ledger(ledger: Option<&Ledger>, ended: &Ended) -> Ledger {
    let day = day_key(ended.ended_at);
    let base = match ledger {
        Some(l) if l.day == day => l.volume_l,
        _ => 0.0,
    };
    let add = if ended.mode == Mode::Washdown { 0.0 } else { ended.volume_l };
    Ledger { day, volume_l: base + add }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_787_140_800_000; // 2026-08-19T12:00:00Z
    fn profile() -> Profile {
        Profile { duration_secs: 24 * 3600, volume_cap_l: 378.5, auto_restart: false }
    }
    // --- the lead-time cutoff, pinned to the numbers measured on MVP 2026-08-22 -------------

    #[test]
    fn the_trigger_leads_the_cap_by_what_will_still_flow() {
        // THE REAL CASE: 10 gal (37.85 L) cap at the 5.83 gal/min (22.07 L/min) the gateway
        // reported. 8 s of that flow is ~2.94 L (0.78 gal) — the overshoot actually measured.
        let cap_l = 37.85;
        let speed_lpm = 22.07;
        let trigger = cutoff_trigger_l(cap_l, speed_lpm);
        let lead = cap_l - trigger;
        assert!((lead - 2.94).abs() < 0.05, "lead was {lead} L, expected ~2.94 L");
        // Stopping AT the cap is what shipped ~11 gal for a 10 gal ask; the trigger must be lower.
        assert!(trigger < cap_l);
    }

    #[test]
    fn a_trickle_barely_leads_and_a_torrent_leads_a_lot() {
        // The whole point of using live `speed` instead of a constant: the correction must track
        // reality. Same cap, two flow rates, two very different triggers.
        let cap_l = 40.0;
        let slow = cutoff_trigger_l(cap_l, 1.0);
        let fast = cutoff_trigger_l(cap_l, 30.0);
        assert!(slow > fast, "a faster flow must trigger earlier: slow={slow} fast={fast}");
        assert!((cap_l - slow) < 0.2, "a trickle should barely lead at all");
    }

    #[test]
    fn no_flow_reading_degrades_to_the_old_trigger_at_the_cap() {
        // Unknown/stale/zero speed must not invent a lead — it falls back to the previous
        // behaviour exactly, rather than firing early on a number it does not have.
        for s in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(cutoff_trigger_l(50.0, s), 50.0, "speed {s} should disable the lead");
        }
    }

    #[test]
    fn a_cap_smaller_than_the_overshoot_stops_at_the_first_sign_of_flow() {
        // Physically undeliverable: at 22 L/min the valve cannot pass less than ~2.9 L before it
        // closes. Clamp to zero and stop immediately — do not pretend a floor exists.
        assert_eq!(cutoff_trigger_l(1.0, 22.07), 0.0);
        assert_eq!(cutoff_trigger_l(0.0, 22.07), 0.0);
        assert_eq!(cutoff_trigger_l(-5.0, 22.07), 0.0);
    }

    #[test]
    fn the_cutoff_fires_early_in_the_state_machine_not_at_the_cap() {
        // End to end through `step`: a cycle capped at 37.85 L flowing at 22.07 L/min must issue
        // its stop BEFORE the cap, at ~34.9 L. Firing at 37.85 is the bug this exists to prevent.
        let profile = Profile { duration_secs: 3600, volume_cap_l: 37.85, auto_restart: false };
        let started = start_hub_cycle(0, Mode::Normal, 3600, 37.85).unwrap();
        let o = Observation { at: 60_000, watering: true, volume_l: 35.0, remain_secs: Some(3540), speed_lpm: 22.07 };
        let r = step(&started, o, &profile);
        assert!(
            matches!(r.action, Action::Stop(EndReason::VolumeCap)),
            "35.0 L is past the 34.9 L trigger and must stop, got {:?}", r.action
        );
        // ...and the same volume with NO flow reading must NOT stop early.
        let o2 = Observation { at: 60_000, watering: true, volume_l: 35.0, remain_secs: Some(3540), speed_lpm: 0.0 };
        let r2 = step(&start_hub_cycle(0, Mode::Normal, 3600, 37.85).unwrap(), o2, &profile);
        assert!(matches!(r2.action, Action::None), "no speed => no lead => no early stop");
    }

    fn obs(at: i64, watering: bool, volume_l: f64) -> Observation {
        Observation { at, watering, volume_l, remain_secs: None, speed_lpm: 0.0 }
    }

    #[test]
    fn the_three_modes_are_enforced_at_the_door() {
        assert!(start_hub_cycle(T0, Mode::Normal, 3600, 0.0).is_err());
        assert!(start_hub_cycle(T0, Mode::Tankfill, 3600, 0.0).is_err());
        // The outlawed shape that cut 2-hour hose runs at ~26 gal.
        assert!(start_hub_cycle(T0, Mode::Washdown, 7200, 100.0).is_err());
        assert!(start_hub_cycle(T0, Mode::Washdown, 7200, 0.0).is_ok());
    }

    #[test]
    fn issues_a_stop_the_moment_the_cap_is_reached_and_never_twice() {
        let s = start_hub_cycle(T0, Mode::Normal, 24 * 3600, 100.0).unwrap();
        let r = step(&s, obs(T0 + 60_000, true, 100.2), &profile());
        assert_eq!(r.action, Action::Stop(EndReason::VolumeCap));
        let r2 = step(&r.state, obs(T0 + 75_000, true, 101.0), &profile());
        assert_eq!(r2.action, Action::None, "no re-issue storm while the valve closes");
    }

    #[test]
    fn washdown_never_volume_stops_whatever_the_meter_says() {
        let s = start_hub_cycle(T0, Mode::Washdown, 7200, 0.0).unwrap();
        let r = step(&s, obs(T0 + 60_000, true, 5000.0), &profile());
        assert_eq!(r.action, Action::None);
    }

    #[test]
    fn the_bug_this_module_exists_for_a_hardware_cap_stop_inside_one_poll() {
        // The old heuristic read any close near the end of a poll gap as "natural expiry" and
        // restarted — spending more water after a cap had already fired.
        let s = start_hub_cycle(T0, Mode::Normal, 600, 100.0).unwrap();
        let closed = step(&s, obs(T0 + 120_000, false, 100.3), &profile());
        let ended = closed.ended.unwrap();
        assert_eq!(ended.reason, EndReason::VolumeCap);
        assert!(!should_auto_restart(&ended, true));
    }

    #[test]
    fn classifies_timer_manual_flood_and_unknown() {
        let s = start_hub_cycle(T0, Mode::Normal, 600, 100.0).unwrap();
        let timer = step(&s, obs(T0 + 590_000, false, 40.0), &profile()).ended.unwrap();
        assert_eq!(timer.reason, EndReason::Timer);

        let mut r = match start_hub_cycle(T0, Mode::Normal, 600, 100.0).unwrap() {
            State::Running(x) => x,
            _ => unreachable!(),
        };
        r.stop_issued = Some(EndReason::Manual);
        let manual = step(&State::Running(r.clone()), obs(T0 + 60_000, false, 5.0), &profile()).ended.unwrap();
        assert_eq!(manual.reason, EndReason::Manual);

        r.stop_issued = Some(EndReason::FloodShutoff);
        let flood = step(&State::Running(r), obs(T0 + 60_000, false, 5.0), &profile()).ended.unwrap();
        assert_eq!(flood.reason, EndReason::FloodShutoff);

        let s2 = start_hub_cycle(T0, Mode::Normal, 600, 100.0).unwrap();
        let mystery = step(&s2, obs(T0 + 60_000, false, 5.0), &profile()).ended.unwrap();
        assert_eq!(mystery.reason, EndReason::Unknown);
    }

    #[test]
    fn only_a_timer_expiry_of_a_normal_run_restarts() {
        let mk = |reason, mode| Ended { mode, ended_at: T0, reason, volume_l: 10.0, provenance: Provenance::Hub };
        assert!(should_auto_restart(&mk(EndReason::Timer, Mode::Normal), true));
        for r in [EndReason::VolumeCap, EndReason::Manual, EndReason::FloodShutoff, EndReason::Unknown] {
            assert!(!should_auto_restart(&mk(r, Mode::Normal), true), "{r:?} must not restart");
        }
        assert!(!should_auto_restart(&mk(EndReason::Timer, Mode::Normal), false));
        assert!(!should_auto_restart(&mk(EndReason::Timer, Mode::Washdown), true));
    }

    #[test]
    fn an_external_open_is_adopted_as_a_normal_run_with_the_profile_cap() {
        let r = step(&State::Idle, obs(T0, true, 0.5), &profile());
        match &r.state {
            State::Running(c) => {
                assert_eq!(c.mode, Mode::Normal);
                assert_eq!(c.volume_cap_l, profile().volume_cap_l);
                assert_eq!(c.provenance, Provenance::Adopted);
            }
            _ => panic!("should have adopted"),
        }
        assert_eq!(r.action, Action::None, "adoption never closes a running valve");
    }

    #[test]
    fn an_adopted_run_is_volume_cut_exactly_like_a_hub_run() {
        let r = step(&State::Idle, obs(T0, true, 0.0), &profile());
        let cut = step(&r.state, obs(T0 + 300_000, true, 380.0), &profile());
        assert_eq!(cut.action, Action::Stop(EndReason::VolumeCap));
    }

    #[test]
    fn the_ledger_counts_normal_and_tankfill_and_adopted_but_never_washdown() {
        let at = T0 + 3 * 3600 * 1000;
        let mk = |mode, vol, prov| Ended { mode, ended_at: at, reason: EndReason::Timer, volume_l: vol, provenance: prov };
        let l = apply_to_ledger(None, &mk(Mode::Normal, 40.0, Provenance::Hub));
        let l = apply_to_ledger(Some(&l), &mk(Mode::Tankfill, 60.0, Provenance::Hub));
        let l = apply_to_ledger(Some(&l), &mk(Mode::Washdown, 500.0, Provenance::Hub));
        let l = apply_to_ledger(Some(&l), &mk(Mode::Normal, 25.0, Provenance::Adopted));
        assert!((l.volume_l - 125.0).abs() < 1e-9, "got {}", l.volume_l);
    }

    #[test]
    fn the_ledger_rolls_to_a_new_utc_day() {
        let late = 1_787_183_940_000; // 2026-08-19T23:59:00Z
        let early = 1_787_184_300_000; // 2026-08-20T00:05:00Z
        let mk = |at| Ended { mode: Mode::Normal, ended_at: at, reason: EndReason::Timer, volume_l: 30.0, provenance: Provenance::Hub };
        let l = apply_to_ledger(None, &mk(late));
        assert_eq!(l.day, "2026-08-19");
        let l2 = apply_to_ledger(Some(&l), &mk(early));
        assert_eq!(l2, Ledger { day: "2026-08-20".into(), volume_l: 30.0 });
    }

    #[test]
    fn day_keys_are_utc_iso_dates() {
        assert_eq!(day_key(1_787_140_800_000), "2026-08-19");
        assert_eq!(day_key(0), "1970-01-01");
    }
}
