//! Zero-config discovery of a LinkTap gateway and its valves, over the LOCAL HTTP API only.
//!
//! WHY THIS EXISTS (owner ruling 2026-08-19, "I would like to only use a hub to link tap gateway
//! model… I do not wanna support using the linktap cloud anymore"): everything the hub needs to
//! drive a valve is obtainable from the gateway itself. Enumerating an account's devices through
//! LinkTap's cloud, and asking the customer to type a gateway IP, are both avoidable.
//!
//! 🔬 PROVEN ON HARDWARE 2026-08-22/25 against MVP's GW-02 (firmware G0609472606040836I). The whole
//! sequence ran from nothing — no IP, no gateway id, no LinkTap account — in about twelve seconds:
//!
//!   1. derive the /24 from the hub's OWN address
//!   2. POST `{"cmd":16}` to every host on it; a LinkTap gateway is the one that answers with JSON
//!      containing `gw_id`
//!   3. THE BOOTSTRAP TRICK: that reply carries `ret:3` (gateway-id-not-matched) *and the gateway's
//!      own id*, so no prior knowledge is needed to learn it
//!   4. re-ask with the id → `end_dev[]` (valve ids), `dev_name[]`, `vol_unit`, firmware
//!
//! ⚠️ mDNS IS NOT ENOUGH and must not be the only path: measured on the same boat LAN, the GW-02
//! advertises no `_linktap`/`_taplinker` service and no recognisable `_http._tcp` name — a browse
//! found the router and a Shelly, never the gateway. Discovery therefore scans, and mDNS stays a
//! cheap first try only for firmware that may one day advertise.
//!
//! ⚠️ SCANNING IS ACTIVE TRAFFIC, so it is deliberately constrained: the hub's OWN /24 (never a
//! wider mask — a /16 is 65k hosts and minutes of traffic), a short timeout, bounded concurrency,
//! and `cmd 16` ONLY. `cmd 16` is a pure read; no discovery path may ever send a mutating command.
//!
//! ⚠️ THE /24 IS A HEURISTIC, AND ON THIS BOAT THE LAN IS ACTUALLY A /16. MVP's gateway reports
//! `msk 255.255.0.0` — so `172.31.0.0/16`, 65,534 addresses. Discovery finds it only because every
//! device happens to live in `172.31.0.x`; a valve gateway parked at `172.31.5.20` would be missed.
//! That is the deliberate trade (owner 2026-08-25: "the manual IP entry might be needed in some
//! situations like a huge subnet") — sweep the likely /24 in seconds, and leave the manual field as
//! the answer for everything wider. Discovery NEVER overwrites a manual host.
//!
//! ⚠️ AND IT ONLY WORKS WHEN THE HUB SHARES THE GATEWAY'S LAN. Measured 2026-08-26: a Mac that had
//! moved to `192.168.50.163` still reached the gateway — but *routed*, via `192.168.50.1`. Nothing
//! local answers there, and reporting "no gateway" is CORRECT in that case, not a failure. A hub
//! off the vessel LAN must be given the address; it cannot discover across a router.
//!
//! ⚠️ A MANUAL HOST ALWAYS WINS (owner, 2026-08-25: "the manual IP entry might be needed in some
//! situations like a huge subnet"). A configured host short-circuits discovery entirely — this
//! module is the fallback for people who have not typed one, not a replacement for the field.

use crate::linktap::{self, Gateway, VolUnit};
use std::time::Duration;

/// What one gateway told us about itself. Everything here came from the gateway, not the cloud.
#[derive(Clone, Debug, PartialEq)]
pub struct Discovered {
    pub host: String,
    pub gw_id: String,
    pub dev_ids: Vec<String>,
    pub dev_names: Vec<String>,
    pub unit: VolUnit,
}

/// The `/24` an address belongs to, as a dotted prefix (`"172.31.0.251"` → `"172.31.0"`).
///
/// A prefix rather than a mask because the caller only ever walks `.1`–`.254`. Refuses anything
/// that is not four numeric octets, and refuses loopback: scanning 127.0.0.x finds nothing and a
/// hub that fell back to loopback would scan itself 254 times forever.
pub fn slash24_prefix(ip: &str) -> Option<String> {
    let o: Vec<&str> = ip.trim().split('.').collect();
    if o.len() != 4 {
        return None;
    }
    if !o.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) && p.parse::<u16>().is_ok_and(|n| n <= 255)) {
        return None;
    }
    if o[0] == "127" || o[0] == "0" {
        return None;
    }
    Some(format!("{}.{}.{}", o[0], o[1], o[2]))
}

/// The gateway's own id out of ANY `cmd 16` reply — including the `ret:3` refusal you get when you
/// ask without one, which is exactly what makes zero-knowledge bootstrap possible.
pub fn parse_gw_id(v: &serde_json::Value) -> Option<String> {
    v.get("gw_id")
        .and_then(|g| g.as_str())
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .map(str::to_string)
}

/// Valve ids + names + unit from a full `cmd 16` reply.
///
/// Ids are normalised the same way every other path normalises them, so a discovered id compares
/// equal to a configured one. Names are positional against `end_dev`; a short or absent `dev_name`
/// yields empty strings rather than dropping valves — a valve with no label is still a valve.
pub fn parse_gateway_config(host: &str, v: &serde_json::Value) -> Option<Discovered> {
    let gw_id = parse_gw_id(v)?;
    let dev_ids: Vec<String> = v
        .get("end_dev")
        .and_then(|d| d.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(linktap::normalize_dev_id)
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if dev_ids.is_empty() {
        return None; // a gateway with no valves is not yet useful to us
    }
    let names_raw: Vec<String> = v
        .get("dev_name")
        .and_then(|d| d.as_array())
        .map(|a| a.iter().map(|x| x.as_str().unwrap_or("").to_string()).collect())
        .unwrap_or_default();
    let dev_names = dev_ids
        .iter()
        .enumerate()
        .map(|(i, _)| names_raw.get(i).cloned().unwrap_or_default())
        .collect();
    let unit = match v.get("vol_unit").and_then(|u| u.as_str()) {
        Some(u) if u.eq_ignore_ascii_case("l") || u.eq_ignore_ascii_case("litre") || u.eq_ignore_ascii_case("liter") => VolUnit::Litre,
        // Gallons is the safe default: reading litres as gallons under-reports a cap 3.79x, and the
        // cutoff compares against it. Same rule as read_vol_unit.
        _ => VolUnit::Gal,
    };
    Some(Discovered { host: host.to_string(), gw_id, dev_ids, dev_names, unit })
}

/// The local address the kernel would use to reach `target`, without sending anything: a UDP
/// socket that is *connected* but never written picks the outbound interface, and `local_addr`
/// reports it. No packet leaves the machine.
fn local_ipv4_toward(target: &str) -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect((target, 9)).ok()?;
    match s.local_addr().ok()? {
        std::net::SocketAddr::V4(a) => Some(a.ip().to_string()),
        _ => None,
    }
}

/// EVERY LAN this host can reach, not just the default route.
///
/// ⚠️ THE BUG THIS EXISTS FOR, found the first time the scan ran on real hardware (2026-08-26):
/// asking only for the default-route address returned `192.168.50.x` on a Mac that was ALSO on the
/// boat's `172.31.0.0/16` — so discovery swept the wrong network and reported "no gateway" while
/// the gateway sat two hops away on the other interface. A hub is routinely multi-homed (LTE plus
/// the vessel LAN, or two LANs), so "the" local address is not a thing.
///
/// Probing one representative address per RFC1918 block asks the routing table which local address
/// serves each private range, which finds every attached private network without enumerating
/// interfaces (no platform-specific code, no extra dependency). Deduplicated, loopback excluded by
/// `slash24_prefix`.
pub fn local_ipv4s() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for target in [
        "10.0.0.1",
        "172.16.0.1",
        "172.31.0.1",
        "192.168.0.1",
        "192.168.50.1",
        "192.0.2.1", // TEST-NET-1: whatever the DEFAULT route is
    ] {
        if let Some(ip) = local_ipv4_toward(target) {
            if !out.contains(&ip) {
                out.push(ip);
            }
        }
    }
    out
}

#[cfg(test)]
mod probe_contract {
    //! 🔴 THE TESTS THAT WOULD HAVE CAUGHT THE ret:3 BUG, AND WHY THE OLD ONES DID NOT.
    //!
    //! Every existing test fed the `ret:3` payload straight to `parse_gw_id`, which has always
    //! handled it correctly. The defect was one line ABOVE that call — `probe` bailed on
    //! `first.ok`, which `post_command` sets false for any non-zero `ret`, so the refusal that
    //! CARRIES the gateway id was discarded before anything parsed it. Discovery could not find a
    //! gateway on any network, and every unit test still passed.
    //!
    //! So these drive `probe` itself against a stub that answers exactly as MVP's GW-02 does.
    //! A helper tested in isolation says nothing about a caller that throws its input away first.
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A stand-in gateway: the FIRST `cmd 16` is refused with ret:3 (and leaks the id, as the real
    /// hardware does), the second — now addressed — returns the full configuration.
    async fn stub_gateway(full_second: bool) -> String {
        use axum::{routing::post, Router};
        let calls = Arc::new(Mutex::new(0usize));
        let app = Router::new().route(
            "/api.shtml",
            post(move || {
                let calls = calls.clone();
                async move {
                    let mut n = calls.lock().unwrap();
                    *n += 1;
                    if *n == 1 {
                        // Verbatim from MVP, HTML wrapper included — the gateway really does this.
                        r#"<html><body><!--#RET-->{"cmd":16,"gw_id":"1485A036004B1200","ret":3}</body></html>"#
                    } else if full_second {
                        r#"<html><body><!--#RET-->{"cmd":16,"gw_id":"1485A036004B1200","ver":"G0609","vol_unit":"gal","end_dev":["3CC1C335004B1200"],"dev_name":["MVP Fresh Water Valve"]}</body></html>"#
                    } else {
                        r#"<html><body><!--#RET-->{"cmd":16,"ret":1}</body></html>"#
                    }
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        addr.to_string()
    }

    #[tokio::test]
    async fn probe_reads_the_ret3_refusal_because_that_is_where_the_gateway_id_lives() {
        let host = stub_gateway(true).await;
        let d = probe(&reqwest::Client::new(), &host).await.expect(
            "a gateway that refuses the un-addressed cmd 16 with ret:3 MUST still be discovered —              that refusal is the bootstrap, not a failure",
        );
        assert_eq!(d.gw_id, "1485A036004B1200");
        assert_eq!(d.dev_ids, vec!["3CC1C335004B1200"]);
    }

    #[tokio::test]
    async fn a_host_that_is_not_a_gateway_is_still_rejected() {
        // The fix must not turn "anything that answers port 80" into a gateway. A router's 404 page
        // or a Shelly has no gw_id to parse, so it is refused at the parse rather than at `ok`.
        use axum::{routing::post, Router};
        let app = Router::new().route("/api.shtml", post(|| async { "<html>404 not found</html>" }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        assert!(probe(&reqwest::Client::new(), &addr.to_string()).await.is_none());
    }

    #[tokio::test]
    async fn a_dead_host_is_rejected_without_hanging_the_sweep() {
        // Nothing listening: the transport fails, `data` is Null, no gw_id. This is the case that
        // makes up 253 of every 254 probes, so it must be cheap and certain.
        let c = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(400))
            .build()
            .unwrap();
        assert!(probe(&c, "127.0.0.1:9").await.is_none());
    }

    #[tokio::test]
    async fn a_host_that_accepts_and_then_says_nothing_cannot_stall_the_sweep() {
        // THE 105-SECOND SWEEP. A host that is UP with port 80 filtered (or an embedded device that
        // accepts and then thinks about it) used to hold a probe for the full 15 s command timeout,
        // and one of them per 32-wide chunk was enough to make adoption look broken on a fresh
        // install. `GATEWAY_TIMEOUT` is right for a valve command someone is waiting on; it is not
        // right for 254 speculative probes, and the client's own timeout reaches nothing because
        // the transport is a raw TcpStream.
        //
        // Accept the connection and never answer. The probe must give up on ITS budget, not the
        // command budget — asserted as an upper bound well under 15 s so this cannot pass by luck.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = l.accept().await {
                held.push(sock); // hold it open, answer nothing
            }
        });
        let started = std::time::Instant::now();
        assert!(probe(&reqwest::Client::new(), &addr.to_string()).await.is_none());
        let took = started.elapsed();
        assert!(took < Duration::from_secs(5), "a silent host held the probe for {took:?}");
    }

    #[tokio::test]
    async fn a_gateway_that_refuses_the_addressed_read_is_not_adopted() {
        // ret:3 on the bootstrap is expected. A refusal on the SECOND, properly addressed read is a
        // real refusal, and adopting a gateway we cannot read the valve list from would leave the
        // hub configured for hardware it cannot drive.
        let host = stub_gateway(false).await;
        assert!(probe(&reqwest::Client::new(), &host).await.is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn the_ret3_refusal_still_yields_the_gateway_id() {
        // THE BOOTSTRAP TRICK, captured verbatim from MVP: asking cmd 16 with no gw_id is refused
        // with ret:3 — and answers with the id anyway. Without this, discovery would need to know
        // the very thing it is trying to find.
        let v = j(r#"{"cmd":16,"gw_id":"1485A036004B1200","ret":3}"#);
        assert_eq!(parse_gw_id(&v).as_deref(), Some("1485A036004B1200"));
    }

    #[test]
    fn a_full_config_reply_yields_valves_names_and_unit() {
        // Real reply from MVP's GW-02, byte for byte.
        let v = j(r#"{"cmd":16,"gw_id":"1485A036004B1200","ver":"G0609472606040836I","vol_unit":"gal","utc_ofs":-14400,"end_dev":["3CC1C335004B1200"],"dev_name":["TapLinker"]}"#);
        let d = parse_gateway_config("172.31.0.244", &v).expect("should parse");
        assert_eq!(d.gw_id, "1485A036004B1200");
        // normalize_dev_id keeps CASE (it trims to the leading 16 hex chars); the gateway reports
        // uppercase, and every other path normalises the same way, so they compare equal.
        assert_eq!(d.dev_ids, vec!["3CC1C335004B1200"]);
        assert_eq!(d.dev_names, vec!["TapLinker"]);
        assert_eq!(d.unit, VolUnit::Gal);
        assert_eq!(d.host, "172.31.0.244");
    }

    #[test]
    fn a_gateway_with_no_valves_is_not_reported_as_usable() {
        // The state MVP was in right after a cmd 2 removal. Discovering it would write an empty
        // dev_ids into config and the poll loop would spin on nothing.
        let v = j(r#"{"cmd":16,"gw_id":"1485A036004B1200","end_dev":[],"dev_name":[]}"#);
        assert!(parse_gateway_config("172.31.0.244", &v).is_none());
    }

    #[test]
    fn missing_names_do_not_drop_valves() {
        let v = j(r#"{"cmd":16,"gw_id":"GW","end_dev":["aaaabbbbccccdddd","1111222233334444"]}"#);
        let d = parse_gateway_config("h", &v).unwrap();
        assert_eq!(d.dev_ids.len(), 2, "a valve with no label is still a valve");
        assert_eq!(d.dev_names, vec!["", ""]);
    }

    #[test]
    fn litre_gateways_are_honoured_but_anything_unknown_reads_as_gallons() {
        for (u, want) in [("l", VolUnit::Litre), ("LITRE", VolUnit::Litre), ("gal", VolUnit::Gal), ("???", VolUnit::Gal)] {
            let v = j(&format!(r#"{{"cmd":16,"gw_id":"G","vol_unit":"{u}","end_dev":["aaaabbbbccccdddd"]}}"#));
            assert_eq!(parse_gateway_config("h", &v).unwrap().unit, want, "unit {u}");
        }
        // Absent entirely: gallons, because reading litres as gallons under-reports a cap 3.79x.
        let v = j(r#"{"cmd":16,"gw_id":"G","end_dev":["aaaabbbbccccdddd"]}"#);
        assert_eq!(parse_gateway_config("h", &v).unwrap().unit, VolUnit::Gal);
    }

    #[test]
    fn a_non_gateway_reply_is_rejected() {
        // The /24 sweep hits routers, Shellys and 404 pages. Only a gw_id makes it a gateway.
        for s in [r#"{"ok":true}"#, r#"{"cmd":16}"#, r#"{"gw_id":""}"#, r#"{"gw_id":"   "}"#] {
            assert!(parse_gw_id(&j(s)).is_none(), "{s} must not read as a gateway");
        }
    }

    #[test]
    fn the_scan_is_confined_to_this_hosts_own_slash24() {
        assert_eq!(slash24_prefix("172.31.0.251").as_deref(), Some("172.31.0"));
        assert_eq!(slash24_prefix("192.168.8.20").as_deref(), Some("192.168.8"));
        // ⚠️ The boat LAN is a /16. Walking it would be 65k hosts; the /24 is the contract.
        assert_eq!(slash24_prefix("172.31.255.4").as_deref(), Some("172.31.255"));
    }

    #[test]
    fn loopback_and_nonsense_never_produce_a_scan_range() {
        for bad in ["127.0.0.1", "0.0.0.0", "", "1.2.3", "1.2.3.4.5", "a.b.c.d", "1.2.3.999"] {
            assert!(slash24_prefix(bad).is_none(), "{bad} must not be scanned");
        }
    }
}

// --- the scan itself (I/O; the decisions above are what carry the tests) ---------------------

/// How long ONE host gets to prove it is a gateway, both requests included.
///
/// ⚠️ THIS IS NOT REDUNDANT WITH THE CLIENT'S TIMEOUT, and assuming it was cost a 105-second sweep.
/// `post_command` hands `gateway_http::post_json` an explicit `GATEWAY_TIMEOUT` (15 s), which is
/// right for a valve command a person is waiting on and completely wrong for 254 speculative
/// probes. The `reqwest::Client` passed in here has its own short timeout and it is IGNORED — the
/// gateway transport is a raw `TcpStream`, not that client, so the client's settings reach nothing.
/// A host that is simply DOWN is cheap (the LAN answers with an RST), but a host that is up with
/// port 80 filtered swallows the SYN and burns the full 15 s, and one such host per 32-wide chunk
/// is enough to make adoption on a fresh install look broken.
///
/// 2 s is generous for a device on the same /24: a real GW-02 answers in tens of milliseconds.
/// Worst case is now 8 chunks x 2 s, so a /24 completes in about 16 s instead of about 105.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Probe ONE host with a read-only `cmd 16`. `None` for anything that is not a LinkTap gateway.
///
/// Bounded, because the sweep is 254 of these and a single unresponsive host must not hold it up.
/// Dropping the future cancels the in-flight connect, which is what makes the bound real rather
/// than merely reported.
async fn probe(client: &reqwest::Client, host: &str) -> Option<Discovered> {
    tokio::time::timeout(PROBE_TIMEOUT, probe_inner(client, host)).await.ok().flatten()
}

async fn probe_inner(client: &reqwest::Client, host: &str) -> Option<Discovered> {
    let gw = Gateway { host: host.to_string(), gw_id: String::new() };
    // No gw_id: the reply REFUSES with ret:3 and hands us the id anyway (see the module note).
    let first = linktap::post_command(client, &gw, &serde_json::json!({ "cmd": 16 })).await;
    // 🔴 DO NOT REINSTATE AN `if !first.ok { return None }` HERE. It was here, and it made discovery
    // incapable of finding ANY gateway on ANY network — shipped in daemon-v0.3.9 and found on
    // CENTRAL on 2026-08-28, where a gateway sat one hop away answering this exact probe while the
    // hub swept past it every 60 seconds.
    //
    // `post_command` reports `ok: false` for ANY non-zero `ret`, which is right for a command —
    // `ret:3` means refused. But this probe is not a command, it is a QUESTION, and the refusal IS
    // the answer: `{"cmd":16,"gw_id":"1485A036004B1200","ret":3}`. Bailing on `ok` threw away the
    // one field the whole bootstrap exists to read.
    //
    // ⚠️ AND NOTE HOW THE TESTS MISSED IT. Every unit test fed the ret:3 payload straight to
    // `parse_gw_id`, which handles it correctly and always did. Nothing exercised the line ABOVE
    // that parse. A helper tested in isolation says nothing about the caller that discards its
    // input first — the fix therefore comes with a test of `probe`'s own contract, below.
    //
    // What we DO require is a reply we could parse at all: a transport failure (dead host, refused
    // connection, timeout) has `data: Null` and no gw_id, so the parse below rejects it. That is
    // the real discriminator between "not a gateway" and "a gateway saying no".
    let gw_id = parse_gw_id(&first.data)?;
    // Now ask properly for the valve list.
    let gw = Gateway { host: host.to_string(), gw_id };
    let full = linktap::post_command(client, &gw, &linktap::build_get_configuration(&gw)).await;
    if !full.ok {
        return None;
    }
    parse_gateway_config(host, &full.data)
}

/// Sweep this host's own /24 for gateways. Bounded concurrency, read-only, best effort.
///
/// Returns EVERY gateway found, not the first — a vessel may legitimately have more than one, and
/// silently picking one would be the kind of hidden truncation that reads as "there is only one".
pub async fn scan_local_subnet(client: &reqwest::Client) -> Vec<Discovered> {
    let mut prefixes: Vec<String> = Vec::new();
    for ip in local_ipv4s() {
        if let Some(p) = slash24_prefix(&ip) {
            if !prefixes.contains(&p) {
                prefixes.push(p);
            }
        }
    }
    if prefixes.is_empty() {
        crate::hlog!("linktap discovery: no usable LAN address; skipping scan");
        return Vec::new();
    }
    let mut found = Vec::new();
    for prefix in &prefixes {
        crate::hlog!(
            "linktap discovery: scanning {prefix}.0/24 for a gateway (read-only cmd 16); a gateway \
             outside this /24 - or across a router - needs its address set manually"
        );
        scan_one_prefix(client, prefix, &mut found).await;
    }
    if found.is_empty() {
        crate::hlog!("linktap discovery: no gateway answered on any attached LAN");
    }
    found
}

async fn scan_one_prefix(client: &reqwest::Client, prefix: &str, found: &mut Vec<Discovered>) {
    // 32 at a time: fast enough to finish a /24 in seconds, gentle enough not to look like a
    // port sweep to anything else on a boat's network.
    for chunk in (1u16..=254).collect::<Vec<_>>().chunks(32) {
        let mut set = tokio::task::JoinSet::new();
        for n in chunk {
            let host = format!("{prefix}.{n}");
            let c = client.clone();
            set.spawn(async move { probe(&c, &host).await });
        }
        while let Some(res) = set.join_next().await {
            if let Ok(Some(d)) = res {
                crate::hlog!(
                    "linktap discovery: found gateway {} at {} with {} valve(s)",
                    d.gw_id, d.host, d.dev_ids.len()
                );
                found.push(d);
            }
        }
    }
}


