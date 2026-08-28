//! The LinkTap RUNTIME — where the protocol client meets the cycle machine and the hub's report
//! path. This is what makes a hub autonomous rather than a remote control: it polls the gateway,
//! enforces the software volume cut, classifies cycle ends, restarts on a timer expiry, and closes
//! every valve on a flood alarm — all with no internet in the path.
//!
//! PORTED from `hub/src/linktapRuntime.ts` (frozen on convergence) against its fixtures.
//!
//! Telemetry model, matching the vendor doc: the gateway's HTTP-push (full status on every change
//! plus a 2-minute heartbeat) is the PRIMARY when configured, and the cmd-3 poll is the FLOOR so a
//! gateway nobody configured for push still works and a missed push cannot strand stale state.
//! Both funnel into ONE `observe()` — the machine cannot tell them apart, which is the point.

use std::collections::HashMap;

use crate::cycle::{self, Action, EndReason, Ledger, Profile, State};
use crate::linktap::{self, Gateway, VolUnit};

/// One valve's live state on this hub.
pub struct Track {
    pub state: State,
    pub ledger: Option<Ledger>,
    /// Per-valve profile from the worker (config-as-state); missing pieces fall to the default.
    pub profile: Option<WireProfile>,
    /// Last `is_flm_plugin` seen — a non-metering valve is bounded by TIME only, said once.
    pub meters: Option<bool>,
}

/// A per-valve profile as the worker sends it. Every field OPTIONAL: the worker omits what the
/// vehicle never set (skip-don't-default), and the hub keeps its own default for those.
#[derive(Clone, Copy, Debug, Default)]
pub struct WireProfile {
    pub duration_secs: Option<u64>,
    pub volume_cap_l: Option<f64>,
    pub auto_restart: Option<bool>,
}

/// A telemetry line the runtime wants reported. The hub's existing batch path spools these.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    pub device: String,
    pub event: String,
    pub params: Vec<(String, String)>,
}

pub struct Runtime {
    pub gateway: Gateway,
    pub unit: VolUnit,
    pub default_profile: Profile,
    tracks: HashMap<String, Track>,
}

impl Runtime {
    pub fn new(gateway: Gateway, dev_ids: &[String], default_profile: Profile) -> Self {
        let mut tracks = HashMap::new();
        for id in dev_ids {
            let id = linktap::normalize_dev_id(id);
            if !id.is_empty() {
                tracks.insert(id, Track { state: State::Idle, ledger: None, profile: None, meters: None });
            }
        }
        Runtime { gateway, unit: VolUnit::Gal, default_profile, tracks }
    }

    pub fn dev_ids(&self) -> Vec<String> {
        self.tracks.keys().cloned().collect()
    }

    /// The effective profile for one valve: the wire profile's fields over the default's, FIELD BY
    /// FIELD — the worker omits what nobody set, so a wrong guess upstream cannot re-cap a valve.
    pub fn profile_for(&self, dev_id: &str) -> Profile {
        let wire = self.tracks.get(dev_id).and_then(|t| t.profile);
        Profile {
            duration_secs: wire.and_then(|w| w.duration_secs).unwrap_or(self.default_profile.duration_secs),
            volume_cap_l: wire.and_then(|w| w.volume_cap_l).unwrap_or(self.default_profile.volume_cap_l),
            auto_restart: wire.and_then(|w| w.auto_restart).unwrap_or(self.default_profile.auto_restart),
        }
    }

    /// Apply per-valve profiles from a worker reply. Valves it does not name keep the default —
    /// what arrives IS the current truth for the valves it names.
    pub fn apply_profiles(&mut self, profiles: &HashMap<String, WireProfile>) {
        for (raw, prof) in profiles {
            let id = linktap::normalize_dev_id(raw);
            if let Some(t) = self.tracks.get_mut(&id) {
                t.profile = Some(*prof);
            }
        }
    }

    /// Feed one status payload for one valve — from a poll reply or a gateway push,
    /// indistinguishably. Returns what to DO and what to report; the caller performs the I/O, which
    /// is what keeps this testable without a gateway.
    pub fn observe(&mut self, dev_id: &str, data: &serde_json::Value, now_ms: i64) -> (Action, Vec<Report>) {
        let id = linktap::normalize_dev_id(dev_id);
        let profile = self.profile_for(&id);
        let unit = self.unit;
        let Some(track) = self.tracks.get_mut(&id) else {
            return (Action::None, Vec::new()); // not a valve this hub watches
        };

        let meters = linktap::reports_volume(data);
        let first_time_not_metering = track.meters.is_none() && !meters;
        track.meters = Some(meters);

        let watering = data.get("is_watering").map(linktap::coerce_watering).unwrap_or(false);
        let volume_l = linktap::cycle_volume_litres(data, unit);
        let remain = data.get("remain_duration").and_then(|v| v.as_f64()).filter(|r| *r > 0.0).map(|r| r as u64);

        let speed_lpm = linktap::flow_rate_litres_per_min(data, unit);
        let obs = cycle::Observation { at: now_ms, watering, volume_l, remain_secs: remain, speed_lpm };
        let r = cycle::step(&track.state, obs, &profile);
        track.state = r.state;

        let mut reports = Vec::new();
        if let Some(ended) = &r.ended {
            let ledger = cycle::apply_to_ledger(track.ledger.as_ref(), ended);
            track.ledger = Some(ledger);
            reports.push(Report {
                device: format!("lt_{id}"),
                // `.change` so it batches — a cycle end is history, not an alarm, and the worker
                // classifies it as telemetry by the same rule every other component uses.
                event: "linktap.cycle.change".into(),
                params: vec![
                    ("mode".into(), ended.mode.as_str().into()),
                    ("reason".into(), ended.reason.as_str().into()),
                    ("vol_l".into(), format!("{:.2}", ended.volume_l)),
                ],
            });
        }

        let mut params = vec![
            ("watering".into(), if watering { "1".to_string() } else { "0".to_string() }),
            ("vol_l".into(), format!("{volume_l:.2}")),
            ("meters".into(), if meters { "1".to_string() } else { "0".to_string() }),
        ];
        // THE LIVE FLOW RATE, in LITRES PER MINUTE. `speed_lpm` has always been computed here —
        // it drives the cutoff's stop-latency lead (cycle::cutoff_trigger_l) — but it was never
        // reported, so an app that is OFF the LAN (relay or cloud) had volume with no rate and
        // drew a flat 0 L/min through an entire watering cycle. CROSS-REPO CONTRACT: the worker
        // maps `flow_lpm` onto the app's `flow` field.
        //
        // WHILE WATERING ONLY, and reported even when it is 0.00: a closed valve has no rate to
        // report, but a valve that IS open and reading zero (a non-metering G1, or a metering
        // valve between pulses) genuinely is at zero, and omitting the param would leave the app
        // showing the last non-zero rate forever. Zero is a fact; silence is a stale number.
        if watering {
            params.push(("flow_lpm".into(), format!("{speed_lpm:.2}")));
        }
        if let Some(l) = &track.ledger {
            params.push(("day".into(), l.day.clone()));
            params.push(("day_vol_l".into(), format!("{:.2}", l.volume_l)));
        }
        reports.push(Report { device: format!("lt_{id}"), event: "linktap.measurement".into(), params });

        if first_time_not_metering {
            eprintln!("linktap: {id} does not meter flow — cycles are bounded by TIME only");
        }
        (r.action, reports)
    }

    /// Should this valve restart after the end this step produced? Kept separate from `observe` so
    /// the caller owns every gateway write.
    pub fn should_restart(&self, dev_id: &str, ended: &cycle::Ended) -> bool {
        cycle::should_auto_restart(ended, self.profile_for(&linktap::normalize_dev_id(dev_id)).auto_restart)
    }

    /// Mark a stop the hub is issuing for a reason the machine cannot infer (manual / flood), so
    /// the eventual close classifies correctly instead of reading as "unknown".
    pub fn note_stop(&mut self, dev_id: &str, reason: EndReason) {
        if let Some(t) = self.tracks.get_mut(&linktap::normalize_dev_id(dev_id)) {
            if let State::Running(r) = &mut t.state {
                r.stop_issued = Some(reason);
            }
        }
    }

    pub fn ledger(&self, dev_id: &str) -> Option<&Ledger> {
        self.tracks.get(&linktap::normalize_dev_id(dev_id)).and_then(|t| t.ledger.as_ref())
    }
}

/// Parse a gateway HTTP-push body (§4.1: the same JSON a cmd 3 reply carries).
pub fn parse_gateway_push(body: &str) -> Vec<(String, serde_json::Value)> {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(linktap::extract_json(body)) else {
        return Vec::new();
    };
    let stats: Vec<serde_json::Value> = match parsed.get("dev_stat") {
        Some(serde_json::Value::Array(a)) => a.clone(),
        _ if parsed.get("dev_id").is_some() => vec![parsed.clone()],
        _ => Vec::new(),
    };
    stats
        .into_iter()
        .filter_map(|d| {
            let id = d.get("dev_id")?.as_str().map(linktap::normalize_dev_id)?;
            if id.is_empty() { None } else { Some((id, d)) }
        })
        .collect()
}

/// The worker's flood-shutoff classification, ported VERBATIM from cloud-server events.ts so the
/// hub closes the valve on exactly the events the cloud would have: flood/leak/alarm, minus
/// clears, minus telemetry.
pub fn is_flood_shutoff(event: &str) -> bool {
    let e = event.to_ascii_lowercase();
    if e.ends_with(".measurement") || e.ends_with(".change") {
        return false;
    }
    if e.ends_with("_off") || e.ends_with(".off") {
        return false;
    }
    e.contains("flood") || e.contains("leak") || e.contains("alarm")
}

// --- Gateway reachability -------------------------------------------------------------------------
//
// The hub polls the gateway every minute, so it is the only thing in the system that KNOWS when the
// gateway stops answering — and until now it told nobody. With the LinkTap cloud removed (owner
// ruling 2026-08-27, option (a): the cloud is gone and a hub is REQUIRED for valve control) there is
// no other source for this fact at all: nothing else on the boat or in the cloud can tell an
// unreachable gateway from a quiet one.

/// The grace window before an unreachable gateway is worth telling anyone about.
///
/// ⚠️ THE WINDOW LIVES HERE, ON THE HUB — not in the cloud. LinkTap gateways FLAP: a brief Wi-Fi
/// hiccup produces an offline and then an online seconds apart, and the first version of this (in
/// the cloud, off the LinkTap webhook) pushed on every single blip, so the owner got a stream of
/// "gateway disconnected" notices about a gateway that was fine. The owner set the window at
/// "30 min plus" — the same number the cloud's `linktapConnectivity.ts::GATEWAY_OFFLINE_GRACE_MS`
/// carries, deliberately, so the two debounces cannot disagree about what counts as an outage.
///
/// Putting it on the hub rather than the cloud is what makes it work at all now: the hub is the
/// only observer, and an outage report that has to survive a WAN round trip to be debounced is one
/// the cloud can only debounce for outages it was told about.
pub const GATEWAY_OFFLINE_GRACE_SECS: i64 = 30 * 60;

/// ⚠️ CROSS-REPO CONTRACT — THE WORKER IS BEING BUILT AGAINST THESE EXACT STRINGS. Do not rename.
///
/// ⚠️ AND NEVER PUT "flood", "leak" OR "alarm" IN THEM. The cloud classifies any event name
/// matching `FLOOD_EVENT_RE = /flood|leak|alarm/i` as a valve-closing flood (events.ts
/// `isFloodShutoff`, ported verbatim into `is_flood_shutoff` above). A gateway-offline notice
/// spelled "gateway.alarm" would therefore CLOSE THE VALVE — an event whose entire meaning is
/// "we cannot reach the gateway" would trigger the one action that needs the gateway. Both names
/// below are pinned against that classifier by test.
pub const GATEWAY_OFFLINE_EVENT: &str = "linktap.gateway.offline";
pub const GATEWAY_ONLINE_EVENT: &str = "linktap.gateway.online";

/// One gateway's reachability episode, as seen by the poll loop. PURE state — the loop owns the
/// clock and the I/O, exactly like `Runtime` owns no timers.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GatewayWatch {
    /// ms epoch of the last poll the gateway ANSWERED, or of the first poll it failed when it has
    /// never answered. `None` only before the very first observation.
    pub last_seen_ms: Option<i64>,
    /// Has `linktap.gateway.offline` already been emitted for THIS episode? Emitting once per
    /// episode is the difference between a notification and a minute-by-minute alarm clock.
    pub offline_reported: bool,
}

/// PURE: fold one poll's outcome into the watch and say what (if anything) to report.
///
/// `reached` is "the gateway answered our HTTP POST", NOT "the command succeeded" — a `ret: 5`
/// (end device not found: valve unpowered, out of RF range) is a gateway that is alive and talking,
/// and calling that an outage would alert on a flat valve battery.
///
/// The two rules that make this quiet enough to be useful:
///   * OFFLINE is emitted ONCE, and only after the gateway has been silent for the whole grace
///     window. A flap inside the window produces nothing at all.
///   * ONLINE is emitted ONLY if an offline was emitted for this episode. A recovery notice for an
///     outage nobody was ever told about is pure noise — it would announce the flaps the grace
///     window exists to swallow.
pub fn gateway_watch_step(
    watch: GatewayWatch,
    gw: &crate::linktap::Gateway,
    reached: bool,
    now_ms: i64,
) -> (GatewayWatch, Option<Report>) {
    let device = format!("lt_gw_{}", gw.gw_id);
    let mut next = watch;
    if reached {
        let was = next.offline_reported;
        let mins = outage_mins(watch.last_seen_ms, now_ms);
        next.last_seen_ms = Some(now_ms);
        next.offline_reported = false;
        if !was {
            return (next, None);
        }
        return (
            next,
            Some(Report {
                device,
                event: GATEWAY_ONLINE_EVENT.into(),
                params: vec![("host".into(), gw.host.clone()), ("mins".into(), mins.to_string())],
            }),
        );
    }
    // Unreachable. A hub that has NEVER seen this gateway answer starts its clock now rather than
    // claiming an outage of unknown length: the grace window then measures "silent since we began
    // watching", which is the only honest reading on a hub that just booted next to a dead gateway.
    let since = match next.last_seen_ms {
        Some(t) => t,
        None => {
            next.last_seen_ms = Some(now_ms);
            return (next, None);
        }
    };
    if next.offline_reported {
        return (next, None); // already said so for this episode
    }
    if now_ms - since < GATEWAY_OFFLINE_GRACE_SECS * 1000 {
        return (next, None); // inside the grace window — this is a flap until proven otherwise
    }
    next.offline_reported = true;
    (
        next,
        Some(Report {
            device,
            event: GATEWAY_OFFLINE_EVENT.into(),
            params: vec![
                ("host".into(), gw.host.clone()),
                ("mins".into(), outage_mins(Some(since), now_ms).to_string()),
            ],
        }),
    )
}

/// Whole minutes of outage, floored at zero. Reported as `mins` on both events so the notification
/// can say how long rather than only that something happened.
fn outage_mins(since_ms: Option<i64>, now_ms: i64) -> i64 {
    match since_ms {
        Some(t) => ((now_ms - t) / 60_000).max(0),
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const T0: i64 = 1_787_140_800_000;
    const DEV: &str = "aaaabbbbccccdddd";

    fn rt(unit: VolUnit) -> Runtime {
        let mut r = Runtime::new(
            Gateway { host: "h".into(), gw_id: "GW02".into() },
            &[DEV.to_string()],
            Profile { duration_secs: 24 * 3600, volume_cap_l: 100.0, auto_restart: false },
        );
        r.unit = unit;
        r
    }

    #[test]
    fn a_gal_gateway_converts_before_the_cap_comparison() {
        // 27 gal = 102.2 L must cut a 100 L cap even though 27 < 100. The conversion IS the safety.
        let mut r = rt(VolUnit::Gal);
        let (a, _) = r.observe(DEV, &json!({"is_watering":1,"volume":26}), T0);
        assert_eq!(a, Action::None);
        let (a2, _) = r.observe(DEV, &json!({"is_watering":1,"volume":27}), T0 + 60_000);
        assert_eq!(a2, Action::Stop(EndReason::VolumeCap));
    }

    #[test]
    fn the_idle_garbage_latch_never_reaches_the_cap_comparison() {
        let mut r = rt(VolUnit::Gal);
        let (a, _) = r.observe(DEV, &json!({"is_watering":1,"volume":15_886_307.0}), T0);
        assert_eq!(a, Action::None, "a closed valve's latch must not read as water");
    }

    #[test]
    fn the_ledger_rides_the_measurement_params_after_a_cycle_ends() {
        let mut r = rt(VolUnit::Litre);
        r.observe(DEV, &json!({"is_watering":1,"volume":40}), T0);
        let (_, reports) = r.observe(DEV, &json!({"is_watering":0,"volume":42}), T0 + 60_000);
        let m = reports.iter().find(|x| x.event == "linktap.measurement").unwrap();
        let day_vol = m.params.iter().find(|(k, _)| k == "day_vol_l").unwrap();
        assert_eq!(day_vol.1, "42.00");
        assert_eq!(m.device, format!("lt_{DEV}"));
        let end = reports.iter().find(|x| x.event == "linktap.cycle.change").unwrap();
        assert!(end.params.iter().any(|(k, v)| k == "reason" && v == "unknown"));
    }

    #[test]
    fn wire_profiles_override_the_default_field_by_field() {
        let mut r = rt(VolUnit::Litre);
        let mut p = HashMap::new();
        p.insert(DEV.to_string(), WireProfile { volume_cap_l: Some(250.0), ..Default::default() });
        r.apply_profiles(&p);
        let eff = r.profile_for(DEV);
        assert_eq!(eff.volume_cap_l, 250.0);
        assert_eq!(eff.duration_secs, 24 * 3600, "unset fields keep the default");
    }

    #[test]
    fn the_wire_cap_drives_the_cutoff_the_moment_it_applies() {
        let mut r = rt(VolUnit::Litre);
        let mut p = HashMap::new();
        p.insert(DEV.to_string(), WireProfile { volume_cap_l: Some(30.0), ..Default::default() });
        r.apply_profiles(&p);
        r.observe(DEV, &json!({"is_watering":1,"volume":10}), T0);
        let (a, _) = r.observe(DEV, &json!({"is_watering":1,"volume":31}), T0 + 60_000);
        assert_eq!(a, Action::Stop(EndReason::VolumeCap), "under the default 100 but over the wire 30");
    }

    #[test]
    fn a_valve_this_hub_does_not_watch_is_ignored_entirely() {
        let mut r = rt(VolUnit::Litre);
        let (a, reports) = r.observe("ffffeeeeddddcccc", &json!({"is_watering":1,"volume":5}), T0);
        assert_eq!(a, Action::None);
        assert!(reports.is_empty());
    }

    #[test]
    fn a_flood_stop_classifies_as_flood_shutoff() {
        let mut r = rt(VolUnit::Litre);
        r.observe(DEV, &json!({"is_watering":1,"volume":3}), T0);
        r.note_stop(DEV, EndReason::FloodShutoff);
        let (_, reports) = r.observe(DEV, &json!({"is_watering":0,"volume":3}), T0 + 20_000);
        let end = reports.iter().find(|x| x.event == "linktap.cycle.change").unwrap();
        assert!(end.params.iter().any(|(k, v)| k == "reason" && v == "flood_shutoff"));
    }

    #[test]
    fn flood_classification_matches_the_workers_line_verbatim() {
        for e in ["flood.alarm", "leak.detected", "alarm", "Flood.Alarm"] {
            assert!(is_flood_shutoff(e), "{e} should close the valve");
        }
        for e in ["flood.alarm_off", "alarm.off", "flood.measurement", "flood.change", "voltmeter.measurement", "button.push"] {
            assert!(!is_flood_shutoff(e), "{e} must NOT close the valve");
        }
        // The real Shelly names that arrive on /api/hub/shelly and MUST close the valve. These are
        // the whole reason the hub has a local ingest at all — with the internet down this
        // classification is the only thing standing between a burst hose and a flooded bilge.
        for e in ["flood", "Flood Detected", "shelly.flood", "water_leak", "smoke.alarm"] {
            assert!(is_flood_shutoff(e), "{e} should close the valve");
        }
        // ⚠️ AND THE TWO GATEWAY-CONNECTIVITY NAMES, pinned here forever. If either ever contains
        // "flood"/"leak"/"alarm" it becomes a valve-closing flood in this classifier AND in the
        // cloud's — so "the gateway is unreachable" would fire the one action that needs the
        // gateway. This assertion is the guard on that rename.
        for e in [GATEWAY_OFFLINE_EVENT, GATEWAY_ONLINE_EVENT] {
            assert!(!is_flood_shutoff(e), "{e} must NOT close the valve");
        }
    }

    #[test]
    fn the_measurement_carries_the_live_flow_rate_while_watering() {
        // The gateway is on GALLONS, so 5.83 gal/min — the rate measured on MVP GW-02 during the
        // 2026-08-22 stop-latency run — must be reported as 22.07 L/min, not as 5.83.
        let mut r = rt(VolUnit::Gal);
        let (_, reports) = r.observe(DEV, &json!({"is_watering":1,"volume":2.0,"speed":5.83}), T0);
        let m = reports.iter().find(|x| x.event == "linktap.measurement").unwrap();
        let flow = m.params.iter().find(|(k, _)| k == "flow_lpm").expect("flow_lpm must be reported");
        assert_eq!(flow.1, "22.07");
    }

    #[test]
    fn a_watering_valve_with_no_flow_reading_reports_zero_rather_than_nothing() {
        // A non-metering G1 (or a metering valve between pulses) genuinely reads zero. Omitting
        // the param would leave an off-LAN app drawing the LAST non-zero rate for the rest of the
        // cycle; zero is a fact, silence is a stale number.
        let mut r = rt(VolUnit::Litre);
        let (_, reports) = r.observe(DEV, &json!({"is_watering":1,"volume":2.0}), T0);
        let m = reports.iter().find(|x| x.event == "linktap.measurement").unwrap();
        assert_eq!(m.params.iter().find(|(k, _)| k == "flow_lpm").unwrap().1, "0.00");
        // ...and a CLOSED valve reports no rate at all — there is nothing flowing to describe.
        let (_, closed) = r.observe(DEV, &json!({"is_watering":0,"volume":2.0}), T0 + 60_000);
        let m2 = closed.iter().find(|x| x.event == "linktap.measurement").unwrap();
        assert!(m2.params.iter().all(|(k, _)| k != "flow_lpm"));
    }

    // --- Gateway reachability -------------------------------------------------------------------

    fn gw() -> crate::linktap::Gateway {
        crate::linktap::Gateway { host: "192.168.8.20".into(), gw_id: "GW02".into() }
    }
    const MIN: i64 = 60_000;

    #[test]
    fn a_flap_inside_the_grace_window_says_absolutely_nothing() {
        // THE BUG THIS EXISTS TO PREVENT: the cloud's first version alerted on every blip, so a
        // 20-second Wi-Fi hiccup pushed "gateway disconnected" about a gateway that was fine.
        let (w, r) = gateway_watch_step(GatewayWatch::default(), &gw(), true, 0);
        assert!(r.is_none(), "the first successful poll is not news");
        let (w, r) = gateway_watch_step(w, &gw(), false, 20_000);
        assert!(r.is_none());
        let (w, r) = gateway_watch_step(w, &gw(), true, 40_000);
        assert!(r.is_none(), "a recovery nobody was told to worry about must stay silent");
        assert!(!w.offline_reported);
    }

    #[test]
    fn a_sustained_outage_reports_once_and_then_recovers_once() {
        let (mut w, _) = gateway_watch_step(GatewayWatch::default(), &gw(), true, 0);
        // Still inside 30 minutes: nothing.
        for t in [5 * MIN, 15 * MIN, 29 * MIN] {
            let (nw, r) = gateway_watch_step(w, &gw(), false, t);
            w = nw;
            assert!(r.is_none(), "reported at {} minutes, inside the grace window", t / MIN);
        }
        // Past it: exactly one offline, carrying the host and the duration.
        let (w2, r) = gateway_watch_step(w, &gw(), false, 31 * MIN);
        let rep = r.expect("a 31-minute outage must be reported");
        assert_eq!(rep.event, "linktap.gateway.offline");
        assert_eq!(rep.device, "lt_gw_GW02");
        assert!(rep.params.contains(&("host".to_string(), "192.168.8.20".to_string())));
        assert!(rep.params.contains(&("mins".to_string(), "31".to_string())));
        // And NEVER again for the same episode — a minute-by-minute repeat is an alarm clock.
        let (w3, again) = gateway_watch_step(w2, &gw(), false, 45 * MIN);
        assert!(again.is_none());
        let (w4, back) = gateway_watch_step(w3, &gw(), true, 60 * MIN);
        let rep = back.expect("an outage that WAS reported must report its recovery");
        assert_eq!(rep.event, "linktap.gateway.online");
        assert_eq!(rep.device, "lt_gw_GW02");
        assert!(rep.params.contains(&("mins".to_string(), "60".to_string())));
        assert!(!w4.offline_reported, "the episode is closed");
        // A second good poll after the recovery is not news either.
        assert!(gateway_watch_step(w4, &gw(), true, 61 * MIN).1.is_none());
    }

    #[test]
    fn a_hub_that_boots_next_to_a_dead_gateway_starts_its_clock_at_the_first_poll() {
        // No prior sighting: claiming an outage of unknown length would be a guess, so the window
        // measures "silent since we began watching" instead. It still fires — 30 minutes later.
        let (w, r) = gateway_watch_step(GatewayWatch::default(), &gw(), false, 1_000);
        assert!(r.is_none());
        assert_eq!(w.last_seen_ms, Some(1_000));
        let (w, r) = gateway_watch_step(w, &gw(), false, 1_000 + 29 * MIN);
        assert!(r.is_none());
        let (_, r) = gateway_watch_step(w, &gw(), false, 1_000 + 31 * MIN);
        assert_eq!(r.expect("must eventually report").event, "linktap.gateway.offline");
    }

    #[test]
    fn the_grace_window_is_the_owners_thirty_minutes() {
        // Provenance, pinned: owner "30 min plus", the same number the cloud's
        // linktapConnectivity.ts carries. Changing it here silently diverges the two debounces.
        assert_eq!(GATEWAY_OFFLINE_GRACE_SECS, 1_800);
    }

    #[test]
    fn parses_both_push_shapes_and_survives_junk() {
        let a = parse_gateway_push(&json!({"gw_id":"G","dev_stat":[{"dev_id":DEV,"is_watering":1}]}).to_string());
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].0, DEV);
        let b = parse_gateway_push(&json!({"dev_id": format!("{DEV}0042"), "is_watering":0}).to_string());
        assert_eq!(b[0].0, DEV, "long ids normalise to the canonical 16");
        assert!(parse_gateway_push("not json").is_empty());
        assert!(parse_gateway_push("{}").is_empty());
    }
}
