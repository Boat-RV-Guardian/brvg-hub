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

use crate::cycle::{self, Action, EndReason, Ledger, Mode, Profile, State};
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

        let obs = cycle::Observation { at: now_ms, watering, volume_l, remain_secs: remain };
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
