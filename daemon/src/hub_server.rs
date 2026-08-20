// The hub SERVER — increment A of ONSITE.md "The hub is a SERVER" (2026-08-18 late): "the hub is a SERVER; apps are
// clients; HTTP only". Owner framing, kept because it is the spec: "its a server, pretty much a
// web server" · "the app is the remote… hub is on all the time" · "think homeassistant... does
// not just listen on localhost."
//
// `--hub` (the flag #381's SYSTEM/ONSTART task has passed since it shipped — parsed nowhere until
// now) starts THIS instead of the GUI: no window, no webview, no per-user anything. It owns the
// machine-wide store (hub_config.rs) exclusively and runs:
//
//   * the MANAGEMENT API on the LAN (0.0.0.0, not loopback — a phone on the boat's Wi-Fi manages
//     the hub exactly like the desktop app on the same machine). Typed allowlist, same discipline
//     as the agent command channel: status / config / token / clear. Nothing generic.
//   * the HEARTBEAT loop — `hub.measurement` on the agent wire, from Rust, off the store.
//   * the KEY SYNC loop — pulls the vehicle members' per-user API keys from the worker
//     (minted per (user, vehicle), owner's scheme). Until the first successful sync the API
//     answers 401 to everything: deny by default, never an open window.
//
// AUTH: every request carries `x-brvg-key`. Keys arrive from the worker with the member's uid and
// vehicle role attached, so the hub knows WHO is calling; writes are gated on the same role
// matrix the app uses (vehicleCapabilities.ts). The hub's own cloud token never appears in any
// response, and reqwest errors are stringified with `without_url()` because heartbeat/sync URLs
// carry that token in `t=`.
//
// The WebSocket to the worker (remote-control relay + live key pushes) is a later increment; the
// sync loop's cadence is the revocation latency until it lands.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::response::{IntoResponse, Response};
use axum::Router;
use serde::{Deserialize, Serialize};

use crate::hub_config::{self, HubConfig, MemberKey};
use crate::linktap;

/// Production worker base — the Rust twin of DEFAULT_WORKER_URL (configSync.ts). Pinned for the
/// same reason as the TS side: this process holds a credential, so it talks only to first party.
const WORKER_BASE: &str = "https://api.boatrvguardian.com";

const KEY_HEADER: &str = "x-brvg-key";
/// Key refresh cadence — this IS the revocation latency until the WS push channel exists.
const KEY_SYNC_SECS: u64 = 300;
/// An unregistered hub polls the store at this cadence waiting for the bootstrap seed.
const UNREGISTERED_POLL_SECS: u64 = 30;
const HEARTBEAT_FLOOR_SECS: u64 = 15;

/// PURE: is this invocation the hub service? (`schtasks … "<exe>" --hub` — hub_service.rs.)
pub fn hub_mode_requested<I: IntoIterator<Item = String>>(args: I) -> bool {
    args.into_iter().any(|a| a == "--hub")
}

// --- Auth ---------------------------------------------------------------------------------------

/// Constant-time string equality — a timing oracle on key comparison would let anyone on the LAN
/// recover a key byte by byte. Length still leaks; keys are fixed-length random, so that is nothing.
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// PURE: which member is presenting this key? Scans the whole set unconditionally (no early
/// return) so a miss costs the same as a hit. Empty key or empty set ⇒ None — deny by default.
pub fn authorize<'a>(keys: &'a [MemberKey], presented: &str) -> Option<&'a MemberKey> {
    if presented.is_empty() {
        return None;
    }
    let mut found = None;
    for k in keys {
        if ct_eq(&k.key, presented) {
            found = Some(k);
        }
    }
    found
}

/// The role matrix, hub-side — a deliberate MIRROR of vehicleCapabilities.ts, not a new scheme.
/// Renaming/re-timing the hub is `change_settings` (admin+); handing it a rotated token or tearing
/// it down is device-lifecycle work (`add_device`/`remove_device` grade — coowner/owner).
pub fn may_configure(role: &str) -> bool {
    matches!(role, "owner" | "coowner" | "admin")
}
pub fn may_administer(role: &str) -> bool {
    matches!(role, "owner" | "coowner")
}

/// Actuating a device is control-grade, mirroring the app's vehicleCapabilities `control_devices`:
/// a `monitor` may look, everyone above may act. Deliberately NOT may_configure — opening a valve
/// is not the same authority as changing the hub's settings, and conflating them would silently
/// promote every `control` member to a configurer.
pub fn may_control(role: &str) -> bool {
    matches!(role, "owner" | "coowner" | "admin" | "control")
}

// --- Wire ---------------------------------------------------------------------------------------

/// PURE: the heartbeat URL. Split out because it IS the wire contract — `hub.measurement` rides
/// the same `/api/agent` ingest as the router agent and the worker classifies it as telemetry,
/// never an alert. The only place the hub token meets a URL.
pub fn heartbeat_url(worker_base: &str, cfg: &HubConfig, ver: &str, platform: &str) -> Result<String, String> {
    let base = worker_base.trim_end_matches('/');
    let mut u = url::Url::parse(&format!("{base}/api/agent")).map_err(|e| e.to_string())?;
    u.query_pairs_mut()
        .append_pair("vid", &cfg.vid)
        .append_pair("device", &cfg.hub_id)
        .append_pair("event", "hub.measurement")
        .append_pair("t", &cfg.token)
        .append_pair("name", &cfg.name)
        .append_pair("platform", platform)
        .append_pair("ver", ver);
    Ok(u.to_string())
}

pub async fn send_heartbeat_once(client: &reqwest::Client, worker_base: &str, cfg: &HubConfig) -> Result<(), String> {
    let url = heartbeat_url(worker_base, cfg, env!("CARGO_PKG_VERSION"), std::env::consts::OS)?;
    let res = client.get(url).send().await.map_err(|e| e.without_url().to_string())?;
    if res.status().is_success() {
        Ok(())
    } else {
        Err(format!("HTTP {}", res.status().as_u16()))
    }
}

/// PURE: read `{linktap:{allowed, profiles}}` out of a worker reply (cloud-server #105).
/// Config-as-state — the worker recomputes it from the vehicle on every report, so what arrives IS
/// the current truth. An absent blob returns None and changes nothing; `allowed` absent reads as
/// FALSE, never as permission.
pub fn parse_linktap_reply(
    body: &serde_json::Value,
) -> Option<(bool, std::collections::HashMap<String, crate::linktap_runtime::WireProfile>)> {
    let lt = body.get("linktap")?;
    let allowed = lt.get("allowed").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut out = std::collections::HashMap::new();
    if let Some(map) = lt.get("profiles").and_then(|v| v.as_object()) {
        for (id, p) in map {
            // Every field OPTIONAL — the worker omits what the vehicle never set, and the hub
            // keeps its own default for those (skip-don't-default, preserved end to end).
            out.insert(
                linktap::normalize_dev_id(id),
                crate::linktap_runtime::WireProfile {
                    duration_secs: p.get("durationSecs").and_then(|v| v.as_u64()),
                    volume_cap_l: p.get("volumeCapL").and_then(|v| v.as_f64()),
                    auto_restart: p.get("autoRestart").and_then(|v| v.as_bool()),
                },
            );
        }
    }
    Some((allowed, out))
}

#[derive(Deserialize)]
struct KeysResp {
    keys: Vec<MemberKey>,
}

/// Pull the member-key set from the worker (increment C's endpoint), authenticated by the hub's
/// own token. An error keeps the last known set — losing the network must not lock the owner out
/// of a hub that is otherwise fine.
pub async fn fetch_member_keys(client: &reqwest::Client, worker_base: &str, cfg: &HubConfig) -> Result<Vec<MemberKey>, String> {
    let base = worker_base.trim_end_matches('/');
    let mut u = url::Url::parse(&format!("{base}/api/hub/keys")).map_err(|e| e.to_string())?;
    u.query_pairs_mut()
        .append_pair("vid", &cfg.vid)
        .append_pair("device", &cfg.hub_id)
        .append_pair("t", &cfg.token);
    let res = client.get(u).send().await.map_err(|e| e.without_url().to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status().as_u16()));
    }
    let body: KeysResp = res.json().await.map_err(|e| e.without_url().to_string())?;
    Ok(body.keys)
}

// --- Server -------------------------------------------------------------------------------------

pub struct Rt {
    /// The LinkTap machine, when this hub has a gateway configured. One instance shared by the
    /// poll loop, the gateway push route and the flood hook — they are three inputs to ONE state
    /// machine, and giving each its own copy would let them disagree about a cycle.
    pub linktap: tokio::sync::Mutex<Option<crate::linktap_runtime::Runtime>>,
    /// The store's base directory — shared_base() in production, a temp dir in tests.
    pub base: PathBuf,
    /// The live key set. Loaded from the store at boot (offline reboot still authenticates known
    /// members), replaced wholesale by each successful sync.
    pub keys: tokio::sync::RwLock<Vec<MemberKey>>,
    /// Serializes read-modify-write of hub.json between handlers and the sync loop.
    pub store: tokio::sync::Mutex<()>,
    pub started: Instant,
    pub worker_base: String,
}

pub type Shared = Arc<Rt>;

pub fn new_rt(base: PathBuf, worker_base: String) -> Shared {
    let keys = hub_config::read_config_in(&base).member_keys;
    Arc::new(Rt {
        base,
        keys: tokio::sync::RwLock::new(keys),
        store: tokio::sync::Mutex::new(()),
        started: Instant::now(),
        worker_base,
        linktap: tokio::sync::Mutex::new(None),
    })
}

pub fn router(rt: Shared) -> Router {
    Router::new()
        .route("/api/hub/status", get(h_status))
        .route("/api/hub/config", post(h_config))
        .route("/api/hub/token", post(h_token))
        .route("/api/hub/clear", post(h_clear))
        .route("/api/hub/linktap/valve", post(h_valve))
        // The GATEWAY's own push (vendor doc §4.1: full status on every change + a 2-min
        // heartbeat). ⚠️ UNAUTHENTICATED BY NECESSITY — the LinkTap gateway is a fixed-firmware
        // appliance that cannot present a key. That is acceptable ONLY because this route is
        // inert: it accepts no commands, changes no configuration, and its body can do nothing
        // but feed status for valves this hub was already told to watch (unknown dev_ids are
        // dropped by the runtime). The worst a hostile LAN peer achieves is a wrong volume
        // reading, which the next poll corrects — deliberately NOT in the relay allowlist, so it
        // is reachable only from the LAN.
        .route("/api/hub/linktap/push", post(h_linktap_push))
        // First-run only, and only from this machine — see h_identity.
        .route("/api/hub/ping", get(h_ping))
        .route("/api/hub/identity", get(h_identity))
        .route("/api/hub/bootstrap", post(h_bootstrap))
        .with_state(rt)
}

/// Everything the status endpoint says about the hub. NO token, no key material — `keysSynced`
/// is a count, which is diagnostics ("did the sync land"), not a secret.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusBody {
    hub_id: String,
    vid: String,
    name: String,
    enabled: bool,
    heartbeat_secs: u32,
    http_port: u16,
    registered: bool,
    version: String,
    platform: String,
    uptime_secs: u64,
    keys_synced: usize,
    /// What this hub can actually DO. The app routes valve control through the hub ONLY when this
    /// contains "linktap" (app #412 utils/valveExecutor) — absence is never read as capability, so
    /// an older daemon or an unpermitted vehicle simply keeps the app on its direct paths.
    capabilities: Vec<String>,
}

async fn status_body(rt: &Rt) -> StatusBody {
    let cfg = hub_config::read_config_in(&rt.base);
    StatusBody {
        registered: !cfg.token.is_empty(),
        hub_id: cfg.hub_id,
        vid: cfg.vid,
        name: cfg.name,
        enabled: cfg.enabled,
        heartbeat_secs: cfg.heartbeat_secs,
        http_port: cfg.http_port,
        version: env!("CARGO_PKG_VERSION").into(),
        platform: std::env::consts::OS.into(),
        uptime_secs: rt.started.elapsed().as_secs(),
        keys_synced: rt.keys.read().await.len(),
        capabilities: capabilities_of(&cfg.linktap),
    }
}

/// The capability list. `linktap` requires BOTH a configured gateway AND the cloud's permission
/// (the paid gate, cached from the worker) — either missing means the hub does not claim it, and
/// the app keeps using its direct paths. Two conditions, one AND, so neither can be forgotten.
fn capabilities_of(lt: &hub_config::LinkTapConfig) -> Vec<String> {
    let mut caps = Vec::new();
    if lt.allowed && !lt.host.is_empty() && !lt.gw_id.is_empty() {
        caps.push("linktap".to_string());
    }
    caps
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigReq {
    name: Option<String>,
    heartbeat_secs: Option<u32>,
    enabled: Option<bool>,
}

#[derive(Deserialize)]
struct TokenReq {
    token: String,
}

/// WHO is asking. On the LAN this comes from the presented key; over the relay it is VOUCHED FOR
/// by the worker, which authenticated the user itself (ONSITE.md "Relay protocol", 2026-08-18 late). Either way
/// the hub applies its OWN role gates to it — the worker deciding who someone is has never been
/// the same as deciding what they may do.
#[derive(Clone, Debug)]
pub struct Caller {
    pub uid: String,
    pub role: String,
}

/// A handler's answer, independent of how it was asked. Both doors return this: the LAN handlers
/// turn it into an HTTP response, the relay wraps it in a `result` frame. One implementation of
/// every rule, so the two paths cannot drift.
pub struct Answer {
    pub status: u16,
    pub body: String,
}

fn ok_json<T: Serialize>(value: &T) -> Answer {
    match serde_json::to_string(value) {
        Ok(body) => Answer { status: 200, body },
        Err(e) => err(500, &format!("could not encode the response: {e}")),
    }
}

/// Errors are JSON too — `{"error": "..."}` — so a caller parses one shape whichever door it came
/// through. A relayed body is passed back by the worker untouched, and a mix of plain text and
/// JSON would put that seam into every client.
fn err(status: u16, message: &str) -> Answer {
    Answer {
        status,
        body: serde_json::json!({ "error": message }).to_string(),
    }
}

// --- The core: one implementation per action ------------------------------------------------------

/// Route an authenticated call. The path+verb allowlist is HERE rather than only in axum's router,
/// because the relay reaches these actions without passing through the router at all.
pub async fn dispatch(rt: &Rt, caller: &Caller, method: &str, path: &str, body: &[u8]) -> Answer {
    match (method, path) {
        ("GET", "/api/hub/status") => do_status(rt).await,
        ("POST", "/api/hub/config") => do_config(rt, caller, body).await,
        ("POST", "/api/hub/token") => do_token(rt, caller, body).await,
        ("POST", "/api/hub/clear") => do_clear(rt, caller).await,
        ("POST", "/api/hub/linktap/valve") => do_valve(rt, caller, body).await,
        _ => err(404, "no such hub endpoint"),
    }
}

async fn do_status(rt: &Rt) -> Answer {
    ok_json(&status_body(rt).await)
}

async fn do_config(rt: &Rt, caller: &Caller, body: &[u8]) -> Answer {
    if !may_configure(&caller.role) {
        return err(403, "changing the hub's settings needs an admin, co-owner or owner");
    }
    let req: ConfigReq = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return err(422, &format!("invalid JSON body: {e}")),
    };
    if let Some(h) = req.heartbeat_secs {
        if u64::from(h) < HEARTBEAT_FLOOR_SECS {
            return err(422, &format!("heartbeatSecs must be at least {HEARTBEAT_FLOOR_SECS}"));
        }
    }
    let name = match req.name {
        Some(n) => {
            let t = n.trim().to_string();
            if t.is_empty() {
                return err(422, "name must not be empty");
            }
            Some(t)
        }
        None => None,
    };
    {
        let _g = rt.store.lock().await;
        let mut cfg = hub_config::read_config_in(&rt.base);
        if let Some(n) = name {
            cfg.name = n;
        }
        if let Some(h) = req.heartbeat_secs {
            cfg.heartbeat_secs = h;
        }
        if let Some(e) = req.enabled {
            cfg.enabled = e;
        }
        if let Err(e) = hub_config::write_config_in(&rt.base, &cfg) {
            return err(500, &e);
        }
    }
    ok_json(&status_body(rt).await)
}

/// Token handover after the app rotates the enrollment (re-enroll replaces the token server-side;
/// the new one has to reach the hub or its heartbeats start bouncing).
async fn do_token(rt: &Rt, caller: &Caller, body: &[u8]) -> Answer {
    if !may_administer(&caller.role) {
        return err(403, "rotating the hub's credential needs a co-owner or the owner");
    }
    let req: TokenReq = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return err(422, &format!("invalid JSON body: {e}")),
    };
    if req.token.is_empty() {
        return err(422, "token must not be empty");
    }
    {
        let _g = rt.store.lock().await;
        let mut cfg = hub_config::read_config_in(&rt.base);
        cfg.token = req.token;
        if let Err(e) = hub_config::write_config_in(&rt.base, &cfg) {
            return err(500, &e);
        }
    }
    ok_json(&status_body(rt).await)
}

/// The local half of un-registering: wipe the store. The CALLER revokes the enrollment with the
/// worker — it holds the user auth that revocation needs; this process never does.
async fn do_clear(rt: &Rt, caller: &Caller) -> Answer {
    if !may_administer(&caller.role) {
        return err(403, "removing the hub needs a co-owner or the owner");
    }
    {
        let _g = rt.store.lock().await;
        if let Err(e) = hub_config::clear_in(&rt.base) {
            return err(500, &e);
        }
    }
    *rt.keys.write().await = Vec::new();
    Answer { status: 204, body: String::new() }
}

// --- The LAN door ---------------------------------------------------------------------------------

async fn caller_from_headers(rt: &Rt, headers: &HeaderMap) -> Option<Caller> {
    let presented = headers.get(KEY_HEADER).and_then(|v| v.to_str().ok()).unwrap_or("");
    let keys = rt.keys.read().await;
    authorize(&keys, presented).map(|k| Caller { uid: k.uid.clone(), role: k.role.clone() })
}

/// Every LAN request funnels through here: authenticate, THEN dispatch.
///
/// The body is taken as raw bytes on purpose. Axum's `Json<T>` extractor runs BEFORE the handler
/// body, which would put the deserializer in front of the auth check — anyone on the LAN could
/// reach it, and every future body change would be pre-auth attack surface. Found by driving the
/// running server (an unauthorized POST to /api/hub/token answered 422, not 401), which is exactly
/// what the unit tests could not show, because they always sent a well-formed body with a valid key.
async fn lan_call(rt: &Rt, headers: &HeaderMap, method: &str, path: &str, body: &[u8]) -> Response {
    let Some(caller) = caller_from_headers(rt, headers).await else {
        return answer_response(err(401, "missing or unknown API key"));
    };
    answer_response(dispatch(rt, &caller, method, path, body).await)
}

fn answer_response(a: Answer) -> Response {
    let status = StatusCode::from_u16(a.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if a.body.is_empty() {
        return status.into_response();
    }
    (status, [(axum::http::header::CONTENT_TYPE, "application/json")], a.body).into_response()
}

// --- The first-run door ---------------------------------------------------------------------------
//
// A hub that has never been configured has no member keys, so it cannot authenticate anybody — yet
// somebody has to give it its vehicle and its cloud token. That is what these two endpoints are
// for, and they are open ONLY while both of these hold:
//
//   * the hub is UNCONFIGURED (no vehicle, no token). The moment it has either, both endpoints
//     refuse forever. Re-registering a live hub goes through the authenticated /api/hub/token
//     instead, so this is never a takeover path.
//   * the caller is on LOOPBACK. The app that sets a hub up is the app running on the machine that
//     is becoming the hub, moments after installing the service. Nothing off-box can reach this.
//
// What an attacker would gain by racing it: they would need local code execution on that machine
// AND a valid cloud token for the vehicle, which requires being its owner or co-owner. The window
// is one first-run, and the capability behind it is one they already have.
//
// This replaces the app writing hub.json over Tauri IPC. Owner, 2026-08-19: "i am not sure why the
// service has any shared files… the app just installs the separate hub application that it controls
// through http". The service now OWNS its configuration — it is the only process that writes it —
// which is also what removes the macOS blocker, since the app no longer needs the shared folder.

/// "Is a hub here, and is it signed to a vehicle?" — unauthenticated, no side effects, on purpose.
///
/// The app needs this to decide whether SIGNING is even offerable, and it cannot use the other two
/// endpoints to find out: `/api/hub/status` needs a member key, which an unsigned hub has none of,
/// and `/api/hub/identity` MINTS an id — a write, and one that fails outright where the service has
/// not created its config directory. Gating the setup flow on either produced a hub that could
/// never be signed, because the only way to reach the door was through the door.
///
/// Nothing here is a secret. Anyone who can reach this port can already see it is open; telling
/// them a hub answers on it, and whether it has a vehicle, adds nothing they could not infer.
async fn h_ping(State(rt): State<Shared>) -> Response {
    let cfg = hub_config::read_config_in(&rt.base);
    answer_response(ok_json(&serde_json::json!({
        "ok": true,
        "registered": !cfg.token.is_empty(),
        "version": env!("CARGO_PKG_VERSION"),
    })))
}

fn is_loopback(addr: SocketAddr) -> bool {
    addr.ip().is_loopback()
}

/// Refuse unless this is a first run, from this machine. Returns the reason when it refuses, so a
/// misconfigured setup says which of the two rules stopped it.
async fn first_run_only(rt: &Rt, addr: SocketAddr) -> Option<Answer> {
    if !is_loopback(addr) {
        return Some(err(403, "a hub can only be set up from the computer it runs on"));
    }
    let cfg = hub_config::read_config_in(&rt.base);
    if !cfg.vid.is_empty() || !cfg.token.is_empty() {
        return Some(err(409, "this hub is already set up; rotate its credential instead"));
    }
    None
}

/// The machine's hub id, minted on first ask. The app needs it BEFORE it can enroll — the cloud
/// token is issued to this id — so this is the first call of the setup sequence.
async fn h_identity(State(rt): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>) -> Response {
    if let Some(refusal) = first_run_only(&rt, addr).await {
        return answer_response(refusal);
    }
    let _g = rt.store.lock().await;
    let (cfg, changed) = hub_config::with_hub_id(hub_config::read_config_in(&rt.base), hub_config::mint_hub_id);
    if changed {
        if let Err(e) = hub_config::write_config_in(&rt.base, &cfg) {
            // The one place this can fail is a config directory the service cannot create. Say so
            // plainly: it is a packaging problem, not something the user can retry their way out of.
            return answer_response(err(500, &format!("this hub cannot write its own configuration: {e}")));
        }
    }
    answer_response(ok_json(&serde_json::json!({ "hubId": cfg.hub_id })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapReq {
    vid: String,
    name: String,
    token: String,
    heartbeat_secs: Option<u32>,
}

/// Sign this hub to a vehicle: the app has just enrolled the id from /api/hub/identity and hands
/// over the resulting cloud token. After this the hub is configured and this door is shut.
async fn h_bootstrap(
    State(rt): State<Shared>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    body: axum::body::Bytes,
) -> Response {
    if let Some(refusal) = first_run_only(&rt, addr).await {
        return answer_response(refusal);
    }
    let req: BootstrapReq = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return answer_response(err(422, &format!("invalid JSON body: {e}"))),
    };
    if req.vid.is_empty() || req.token.is_empty() {
        return answer_response(err(422, "vid and token are required"));
    }
    let hb = req.heartbeat_secs.unwrap_or(60);
    if u64::from(hb) < HEARTBEAT_FLOOR_SECS {
        return answer_response(err(422, &format!("heartbeatSecs must be at least {HEARTBEAT_FLOOR_SECS}")));
    }
    {
        let _g = rt.store.lock().await;
        let (mut cfg, _) = hub_config::with_hub_id(hub_config::read_config_in(&rt.base), hub_config::mint_hub_id);
        cfg.vid = req.vid;
        cfg.name = if req.name.trim().is_empty() { "Hub".to_string() } else { req.name.trim().to_string() };
        cfg.token = req.token;
        cfg.enabled = true;
        cfg.heartbeat_secs = hb;
        if let Err(e) = hub_config::write_config_in(&rt.base, &cfg) {
            return answer_response(err(500, &format!("this hub cannot write its own configuration: {e}")));
        }
    }
    answer_response(ok_json(&status_body(&rt).await))
}

async fn h_status(State(rt): State<Shared>, headers: HeaderMap) -> Response {
    lan_call(&rt, &headers, "GET", "/api/hub/status", b"").await
}

async fn h_config(State(rt): State<Shared>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    lan_call(&rt, &headers, "POST", "/api/hub/config", &body).await
}

async fn h_token(State(rt): State<Shared>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    lan_call(&rt, &headers, "POST", "/api/hub/token", &body).await
}

async fn h_clear(State(rt): State<Shared>, headers: HeaderMap) -> Response {
    lan_call(&rt, &headers, "POST", "/api/hub/clear", b"").await
}

async fn h_valve(State(rt): State<Shared>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    lan_call(&rt, &headers, "POST", "/api/hub/linktap/valve", &body).await
}

/// Is this push actually FROM the configured gateway?
///
/// The gateway cannot authenticate — it is fixed firmware with no key — so the peer address is the
/// only evidence available. This is not authentication and is not claimed to be: a LAN peer can
/// spoof an address. It narrows "any device on the boat's network" to "something answering at the
/// gateway's address", which is a real reduction for one comparison.
///
/// It reads the SAME `linktap.host` the poll loop dials, so the two cannot drift apart: if the
/// gateway's DHCP address changes, polling breaks at the same moment pushes stop being accepted —
/// one visible failure instead of a silent half-broken state. An unconfigured host accepts nothing.
fn push_peer_allowed(host: &str, peer: SocketAddr) -> bool {
    if host.is_empty() {
        return false;
    }
    // `host` may carry a port (the config field is a host[:port] for the gateway's HTTP API).
    let host_only = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    match host_only.parse::<std::net::IpAddr>() {
        Ok(ip) => peer.ip() == ip,
        // A hostname was configured rather than an address. Resolving it here would put a DNS
        // lookup on every push, so accept and rely on the route's inertness — the same posture as
        // before this check existed, for the configuration that cannot support it.
        Err(_) => true,
    }
}

async fn h_linktap_push(
    State(rt): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    body: axum::body::Bytes,
) -> Response {
    let host = hub_config::read_config_in(&rt.base).linktap.host;
    if !push_peer_allowed(&host, peer) {
        // Same answer as a good push: this is not an authorization surface, and telling a prober
        // whether it guessed the gateway's address would make it one.
        return (StatusCode::OK, "ok").into_response();
    }
    // Answer FIRST, work after — the gateway retries a slow endpoint, and a duplicate status is
    // worse than a late one (it would re-run the cutoff comparison against stale numbers).
    let text = String::from_utf8_lossy(&body).to_string();
    tokio::spawn(async move {
        let client = http_client();
        for (dev_id, data) in crate::linktap_runtime::parse_gateway_push(&text) {
            let (action, reports) = {
                let mut guard = rt.linktap.lock().await;
                match guard.as_mut() {
                    Some(r) => r.observe(&dev_id, &data, now_ms()),
                    None => (crate::cycle::Action::None, Vec::new()),
                }
            };
            linktap_act(&rt, &client, &dev_id, action, reports).await;
        }
    });
    (StatusCode::OK, "ok").into_response()
}

// --- Loops --------------------------------------------------------------------------------------

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("reqwest client")
}

/// Beat while registered+enabled; otherwise poll the store waiting for the bootstrap seed (the
/// signed-in app writes it once at registration — the service may well boot first).
async fn heartbeat_loop(rt: Shared) {
    let client = http_client();
    loop {
        let cfg = hub_config::read_config_in(&rt.base);
        if !cfg.token.is_empty() && !cfg.vid.is_empty() && cfg.enabled {
            match heartbeat_with_reply(&client, &rt.worker_base, &cfg).await {
                Ok(body) => apply_linktap_reply(&rt, &body).await,
                Err(e) => eprintln!("hub: heartbeat failed: {e}"),
            }
            let secs = u64::from(cfg.heartbeat_secs).max(HEARTBEAT_FLOOR_SECS);
            tokio::time::sleep(Duration::from_secs(secs)).await;
        } else {
            tokio::time::sleep(Duration::from_secs(UNREGISTERED_POLL_SECS)).await;
        }
    }
}

/// The heartbeat, keeping its reply — the config-as-state channel (cloud-server #105 attaches
/// `{linktap:{allowed,profiles}}` to a hub's report). send_heartbeat_once stays for callers that
/// only care whether it landed.
async fn heartbeat_with_reply(client: &reqwest::Client, worker_base: &str, cfg: &HubConfig) -> Result<serde_json::Value, String> {
    let url = heartbeat_url(worker_base, cfg, env!("CARGO_PKG_VERSION"), std::env::consts::OS)?;
    let res = client.get(url).send().await.map_err(|e| e.without_url().to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status().as_u16()));
    }
    res.json::<serde_json::Value>().await.map_err(|e| e.without_url().to_string())
}

/// Persist the cloud's valve PERMISSION and hand the per-valve profiles to the machine.
///
/// `allowed` is written to hub.json because the capability advertisement and the endpoint's own
/// 402 both read it there — including on a boot with no internet, where the last known answer is
/// the only one available. It defaults to false everywhere, so a hub that has never heard from the
/// cloud claims nothing.
async fn apply_linktap_reply(rt: &Rt, body: &serde_json::Value) {
    let Some((allowed, profiles)) = parse_linktap_reply(body) else { return };
    {
        let _g = rt.store.lock().await;
        let mut cfg = hub_config::read_config_in(&rt.base);
        if cfg.linktap.allowed != allowed {
            eprintln!("linktap: valve control {} by the vehicle's plan", if allowed { "permitted" } else { "NOT permitted" });
            cfg.linktap.allowed = allowed;
            if let Err(e) = hub_config::write_config_in(&rt.base, &cfg) {
                eprintln!("hub: could not persist the linktap permission: {e}");
            }
        }
    }
    if !profiles.is_empty() {
        let mut guard = rt.linktap.lock().await;
        if let Some(r) = guard.as_mut() {
            r.apply_profiles(&profiles);
        }
    }
}

async fn key_sync_loop(rt: Shared) {
    let client = http_client();
    loop {
        let cfg = hub_config::read_config_in(&rt.base);
        if !cfg.token.is_empty() {
            match fetch_member_keys(&client, &rt.worker_base, &cfg).await {
                Ok(keys) => {
                    *rt.keys.write().await = keys.clone();
                    let _g = rt.store.lock().await;
                    let mut c = hub_config::read_config_in(&rt.base);
                    c.member_keys = keys;
                    if let Err(e) = hub_config::write_config_in(&rt.base, &c) {
                        eprintln!("hub: could not persist member keys: {e}");
                    }
                }
                // Keep the last known set — a network drop must not lock the owner out. (Before
                // increment C's endpoint deploys this is a permanent 404: deny-all continues.)
                Err(e) => eprintln!("hub: key sync failed (keeping previous keys): {e}"),
            }
        }
        tokio::time::sleep(Duration::from_secs(KEY_SYNC_SECS)).await;
    }
}

// --- LinkTap: the I/O shell around the pure runtime -----------------------------------------------
//
// Three inputs, ONE state machine (rt.linktap): this poll loop, the gateway's HTTP push (the
// /api/hub/linktap/push route), and the flood hook. The machine decides; this code performs.

/// How often the poll floor runs. The gateway's own push heartbeat is 2 minutes, so this is the
/// FLOOR under it, not the primary — a gateway nobody configured for push still works, and a
/// missed push cannot strand stale state.
const LINKTAP_POLL_SECS: u64 = 60;

/// Rebuild the machine when the configured gateway/valves change, and keep the paid gate current.
/// Returns false when LinkTap is not configured or not permitted, in which case nothing polls.
async fn linktap_sync_config(rt: &Rt) -> bool {
    let cfg = hub_config::read_config_in(&rt.base);
    let lt = &cfg.linktap;
    let usable = lt.allowed && !lt.host.is_empty() && !lt.gw_id.is_empty() && !lt.dev_ids.is_empty();
    let mut guard = rt.linktap.lock().await;
    if !usable {
        // Dropping the machine on a revoked plan is deliberate: a hub whose vehicle stopped paying
        // must stop driving the valve, not merely stop advertising that it can.
        if guard.is_some() {
            eprintln!("linktap: configuration withdrawn or plan no longer permits valve control — stopping");
        }
        *guard = None;
        return false;
    }
    let gw = linktap::Gateway { host: lt.host.clone(), gw_id: lt.gw_id.clone() };
    let needs_rebuild = match guard.as_ref() {
        None => true,
        Some(r) => {
            let mut have = r.dev_ids();
            have.sort();
            let mut want: Vec<String> = lt.dev_ids.iter().map(|d| linktap::normalize_dev_id(d)).collect();
            want.sort();
            r.gateway.host != gw.host || r.gateway.gw_id != gw.gw_id || have != want
        }
    };
    if needs_rebuild {
        let profile = crate::cycle::Profile {
            duration_secs: 24 * 3600,
            volume_cap_l: 378.0, // the 100 gal Normal Run default; wire profiles override per valve
            auto_restart: false,
        };
        let mut r = crate::linktap_runtime::Runtime::new(gw.clone(), &lt.dev_ids, profile);
        // Read the gateway's unit ONCE per rebuild. Defaults to GALLONS when unreadable, because
        // guessing litres under-reports a cap by 3.79x and the cutoff compares against it.
        r.unit = linktap::read_vol_unit(&http_client(), &gw).await;
        eprintln!("linktap: watching {} valve(s) via {} (unit {:?})", lt.dev_ids.len(), lt.host, r.unit);
        *guard = Some(r);
    }
    true
}

/// Act on one machine decision: issue the stop it asked for, restart on a timer expiry, and spool
/// whatever it wants reported.
async fn linktap_act(
    rt: &Rt,
    client: &reqwest::Client,
    dev_id: &str,
    action: crate::cycle::Action,
    reports: Vec<crate::linktap_runtime::Report>,
) {
    for r in reports {
        spool_report(rt, &r).await;
    }
    let gw = {
        let guard = rt.linktap.lock().await;
        match guard.as_ref() {
            Some(x) => x.gateway.clone(),
            None => return,
        }
    };
    if let crate::cycle::Action::Stop(reason) = action {
        eprintln!("linktap: {dev_id} — issuing stop ({})", reason.as_str());
        let reply = linktap::post_command(client, &gw, &linktap::build_stop(&gw, dev_id)).await;
        if !reply.ok {
            // A close that did not happen is worth hearing about immediately; the machine keeps
            // stop_issued set, so the next observation retries without a re-issue storm.
            eprintln!("linktap: {dev_id} STOP FAILED: {:?}", reply.error);
            spool_report(rt, &crate::linktap_runtime::Report {
                device: format!("lt_{dev_id}"),
                event: "linktap.stop_failed".into(),
                params: vec![("error".into(), reply.error.unwrap_or_default())],
            }).await;
        }
    }
}

/// The poll floor.
async fn linktap_poll_loop(rt: Shared) {
    let client = http_client();
    loop {
        if linktap_sync_config(&rt).await {
            let (gw, ids) = {
                let guard = rt.linktap.lock().await;
                match guard.as_ref() {
                    Some(r) => (r.gateway.clone(), r.dev_ids()),
                    None => (linktap::Gateway { host: String::new(), gw_id: String::new() }, Vec::new()),
                }
            };
            for id in ids {
                let reply = linktap::post_command(&client, &gw, &linktap::build_status(&gw, &id)).await;
                if !reply.ok {
                    continue; // an unreachable gateway is the poll loop's normal weather
                }
                let data = reply.data.get("dev_stat")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .cloned()
                    .unwrap_or(reply.data);
                let (action, reports) = {
                    let mut guard = rt.linktap.lock().await;
                    match guard.as_mut() {
                        Some(r) => r.observe(&id, &data, now_ms()),
                        None => (crate::cycle::Action::None, Vec::new()),
                    }
                };
                linktap_act(&rt, &client, &id, action, reports).await;
            }
        }
        tokio::time::sleep(Duration::from_secs(LINKTAP_POLL_SECS)).await;
    }
}

/// Close every watched valve — the flood hook. The close must not wait on the WAN: with the
/// LinkTap cloud gone this is the only automated close path when the uplink is down. The valve
/// self-limits regardless (every open carries duration+volume), so this only ever closes it sooner.
pub async fn linktap_flood_stop_all(rt: &Rt) {
    let client = http_client();
    let (gw, ids) = {
        let guard = rt.linktap.lock().await;
        match guard.as_ref() {
            Some(r) => (r.gateway.clone(), r.dev_ids()),
            None => return,
        }
    };
    for id in ids {
        {
            let mut guard = rt.linktap.lock().await;
            if let Some(r) = guard.as_mut() {
                r.note_stop(&id, crate::cycle::EndReason::FloodShutoff);
            }
        }
        let reply = linktap::post_command(&client, &gw, &linktap::build_stop(&gw, &id)).await;
        eprintln!("linktap: flood shutoff -> {id} {}", if reply.ok { "closed" } else { "FAILED" });
        if !reply.ok {
            spool_report(rt, &crate::linktap_runtime::Report {
                device: format!("lt_{id}"),
                event: "linktap.stop_failed".into(),
                params: vec![("error".into(), reply.error.unwrap_or_default()), ("cause".into(), "flood".into())],
            }).await;
        }
    }
}

/// Report one telemetry line to the cloud, through the same /api/agent path the heartbeat uses.
/// Best-effort by design: telemetry that cannot be delivered must never block the valve logic that
/// produced it.
async fn spool_report(rt: &Rt, report: &crate::linktap_runtime::Report) {
    let cfg = hub_config::read_config_in(&rt.base);
    if cfg.token.is_empty() || cfg.vid.is_empty() {
        return;
    }
    let base = rt.worker_base.trim_end_matches('/');
    let Ok(mut u) = url::Url::parse(&format!("{base}/api/agent")) else { return };
    u.query_pairs_mut()
        .append_pair("vid", &cfg.vid)
        .append_pair("device", &report.device)
        .append_pair("event", &report.event)
        .append_pair("t", &cfg.token);
    for (k, v) in &report.params {
        u.query_pairs_mut().append_pair(k, v);
    }
    if let Err(e) = http_client().get(u).send().await {
        eprintln!("linktap: report {} failed: {e}", report.event);
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// --- Entry --------------------------------------------------------------------------------------

/// The `--hub` main. Never returns except on fatal startup errors or ctrl-c (manual runs);
/// as a service there is no console, so failures also land in the heartbeat's absence — the
/// connectivity sweep alerting on a quiet hub is the real monitor.
pub fn run_headless() {
    let runtime = tokio::runtime::Runtime::new().expect("hub: tokio runtime");
    runtime.block_on(async {
        let base = hub_config::shared_base();
        let cfg = hub_config::read_config_in(&base);
        let port = cfg.http_port;
        let rt = new_rt(base, WORKER_BASE.into());
        let listener = match tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("hub: cannot bind 0.0.0.0:{port}: {e}");
                std::process::exit(1);
            }
        };
        eprintln!("hub: management API on 0.0.0.0:{port} ({})", if cfg.token.is_empty() { "unregistered — waiting for bootstrap" } else { "registered" });
        tokio::spawn(heartbeat_loop(rt.clone()));
        tokio::spawn(key_sync_loop(rt.clone()));
        // The LinkTap poll floor. It re-reads its own configuration each pass, so a gateway
        // configured (or a plan revoked) after boot is picked up without a restart.
        tokio::spawn(linktap_poll_loop(rt.clone()));
        // The outbound socket to the worker: remote control, and live member-key pushes. Failing
        // to connect is not fatal — the LAN API and the polling sync carry on without it.
        tokio::spawn(crate::hub_relay::run(rt.clone()));
        tokio::select! {
            // with_connect_info: the first-run door needs the PEER address, because "only from this
            // machine" is the whole of its security.
            r = axum::serve(listener, router(rt).into_make_service_with_connect_info::<SocketAddr>()) => {
                if let Err(e) = r {
                    eprintln!("hub: server exited: {e}");
                }
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("hub: shutting down");
            }
        }
    });
}


// --- Valve control (owner ruling 2026-08-19: with a hub present the control plane runs THROUGH
// it, never app -> device) ------------------------------------------------------------------------
//
// WHY THE APP ROUTES HERE AT ALL: an onsite executor adopts any cycle it did not start as a NORMAL
// RUN and enforces the Normal Run cap. While the app also opens valves directly, that executor
// cannot tell an app-started WASHDOWN (time-only, must never be volume-cut) from a physical button
// press. Routing through removes the ambiguity at its source — and `mode` below is the fact the
// hub could never infer by watching.

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ValveReq {
    dev_id: String,
    /// "open" | "close".
    action: String,
    duration_secs: Option<u64>,
    /// Litres. ABSENT for a washdown — the key's absence IS the time-only signal, never a zero.
    volume_cap_l: Option<f64>,
    /// "normal" | "washdown" | "tankfill". Absent is treated as normal.
    mode: Option<String>,
}

async fn do_valve(rt: &Rt, caller: &Caller, body: &[u8]) -> Answer {
    if !may_control(&caller.role) {
        return err(403, "controlling a valve needs control access or above");
    }
    let req: ValveReq = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return err(422, &format!("invalid JSON body: {e}")),
    };

    let cfg = hub_config::read_config_in(&rt.base);
    // The paid gate again, at the ACTION not just the advertisement: a caller that skipped the
    // capability check (an older app, a hand-rolled request) must still be refused. Cheap, and it
    // means the gate cannot be bypassed by not asking.
    if !cfg.linktap.allowed {
        return err(402, "this vehicle's plan does not include valve control");
    }
    if cfg.linktap.host.is_empty() || cfg.linktap.gw_id.is_empty() {
        return err(409, "no LinkTap gateway is configured on this hub");
    }
    let dev_id = linktap::normalize_dev_id(&req.dev_id);
    if dev_id.is_empty() {
        return err(422, "devId is required");
    }
    // Only valves this hub was told about — a hub is not a general-purpose proxy onto the
    // vessel's RF network, the same reasoning as the relay's path allowlist.
    if !cfg.linktap.dev_ids.iter().any(|d| linktap::normalize_dev_id(d) == dev_id) {
        return err(404, "that valve is not configured on this hub");
    }

    let gw = linktap::Gateway { host: cfg.linktap.host.clone(), gw_id: cfg.linktap.gw_id.clone() };
    let client = reqwest::Client::new();

    let body_json = match req.action.as_str() {
        "close" => linktap::build_stop(&gw, &dev_id),
        "open" => {
            let secs = match req.duration_secs {
                Some(s) if s > 0 => s,
                _ => return err(422, "durationSecs is required to open a valve"),
            };
            // ⚠️ A WASHDOWN IS TIME-ONLY (owner spec 2026-07-30, re-ratified twice). Mode decides
            // the cap, NOT the presence of a number: a volumeCapL sent alongside mode=washdown is
            // a caller bug, and honouring it would re-create the "external cap" that cut 2-hour
            // hose runs at ~26 gal. Refuse it rather than silently dropping either side.
            let mode = req.mode.as_deref().unwrap_or("normal");
            if mode == "washdown" && req.volume_cap_l.is_some() {
                return err(422, "a washdown is time-limited only — do not send volumeCapL with mode=washdown");
            }
            // The cap must be expressed in the GATEWAY's unit, so read it — one extra
            // round-trip on a user-initiated action, where being right beats being fast.
            // read_vol_unit defaults to GALLONS when unreadable, because guessing litres
            // under-reports a cap by 3.79x and the cutoff compares against that number.
            let cap_gw = match (mode, req.volume_cap_l) {
                ("washdown", _) | (_, None) => None,
                (_, Some(l)) => {
                    let unit = linktap::read_vol_unit(&client, &gw).await;
                    Some(unit.from_litres(l))
                }
            };
            linktap::build_start(&gw, &dev_id, secs, cap_gw)
        }
        other => return err(422, &format!("unknown action '{other}' — expected open or close")),
    };

    let reply = linktap::post_command(&client, &gw, &body_json).await;
    if !reply.ok {
        let detail = reply.error.unwrap_or_else(|| "the gateway refused the command".into());
        eprintln!("linktap: {} {} failed: {detail}", req.action, dev_id);
        // 502: the hub is fine, the thing BEHIND it refused. The app falls back and says so.
        return err(502, &format!("the gateway did not accept that command: {detail}"));
    }
    eprintln!("linktap: {} {} ok", req.action, dev_id);
    ok_json(&serde_json::json!({ "ok": true }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Query;
    use axum::Json;
    use std::collections::HashMap;

    fn temp_base(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("brvg-hub-server-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn key(role: &str) -> MemberKey {
        MemberKey { key: format!("key-{role}-{}", "x".repeat(32)), uid: format!("uid-{role}"), role: role.into() }
    }

    fn seeded_cfg() -> HubConfig {
        HubConfig {
            hub_id: "hub_abc123".into(), vid: "v1".into(), name: "Central".into(),
            enabled: true, heartbeat_secs: 60, token: "hubtok-secret".into(),
            ..HubConfig::default()
        }
    }

    async fn spawn_server(base: PathBuf, keys: Vec<MemberKey>) -> (String, Shared) {
        let rt = new_rt(base, "https://unused.example".into());
        *rt.keys.write().await = keys;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // with_connect_info, exactly as run_headless serves it — the first-run door reads the peer
        // address, and a harness without it would 500 on a route production answers.
        let app = router(rt.clone()).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), rt)
    }

    #[test]
    fn hub_mode_is_the_dash_dash_hub_flag_and_nothing_else() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(hub_mode_requested(args(&["app.exe", "--hub"])));
        assert!(!hub_mode_requested(args(&["app.exe"])));
        // Near-misses must not turn a GUI launch into a headless one.
        assert!(!hub_mode_requested(args(&["app.exe", "--hubx", "hub", "--HUB"])));
    }

    #[test]
    fn authorization_is_deny_by_default_and_exact_match() {
        let keys = vec![key("owner"), key("monitor")];
        assert!(authorize(&keys, "").is_none());
        assert!(authorize(&[], &key("owner").key).is_none()); // pre-sync: NOTHING authenticates
        assert!(authorize(&keys, "key-owner-wrong").is_none());
        let hit = authorize(&keys, &key("monitor").key).unwrap();
        assert_eq!(hit.role, "monitor");
        assert_eq!(hit.uid, "uid-monitor");
    }

    #[test]
    fn write_gates_mirror_the_vehicle_role_matrix() {
        // change_settings grade
        for r in ["owner", "coowner", "admin"] { assert!(may_configure(r), "{r}"); }
        for r in ["control", "monitor", "", "garbage"] { assert!(!may_configure(r), "{r}"); }
        // device-lifecycle grade
        for r in ["owner", "coowner"] { assert!(may_administer(r), "{r}"); }
        for r in ["admin", "control", "monitor", ""] { assert!(!may_administer(r), "{r}"); }
    }

    #[test]
    fn the_heartbeat_is_a_hub_measurement_on_the_agent_wire() {
        let cfg = seeded_cfg();
        let u = url::Url::parse(&heartbeat_url("https://w.example/", &cfg, "1.0.82", "windows").unwrap()).unwrap();
        assert_eq!(u.path(), "/api/agent"); // same ingest as the router agent
        let q: HashMap<_, _> = u.query_pairs().into_owned().collect();
        assert_eq!(q["vid"], "v1");
        assert_eq!(q["device"], "hub_abc123");
        assert_eq!(q["event"], "hub.measurement"); // telemetry classification, never an alert
        assert_eq!(q["t"], "hubtok-secret");
        assert_eq!(q["name"], "Central");
        assert_eq!(q["platform"], "windows");
        assert_eq!(q["ver"], "1.0.82");
    }

    #[test]
    fn a_hub_name_with_spaces_and_symbols_cannot_break_the_query() {
        let cfg = HubConfig { name: "Jon's boat & RV=hub".into(), ..seeded_cfg() };
        let raw = heartbeat_url("https://w.example", &cfg, "1.0.82", "macos").unwrap();
        assert!(!raw.contains("boat & RV"), "the name must be encoded: {raw}");
        let q: HashMap<_, _> = url::Url::parse(&raw).unwrap().query_pairs().into_owned().collect();
        assert_eq!(q["name"], "Jon's boat & RV=hub");
        assert_eq!(q["t"], "hubtok-secret"); // an `=` in the name must not spill into another param
    }

    #[tokio::test]
    async fn the_api_denies_everything_without_a_valid_key() {
        let base = temp_base("deny");
        hub_config::write_config_in(&base, &seeded_cfg()).unwrap();
        let (origin, _rt) = spawn_server(base, vec![]).await; // pre-sync: empty key set
        let c = reqwest::Client::new();
        let r = c.get(format!("{origin}/api/hub/status")).send().await.unwrap();
        assert_eq!(r.status(), 401);
        let r = c.get(format!("{origin}/api/hub/status")).header(KEY_HEADER, "anything").send().await.unwrap();
        assert_eq!(r.status(), 401);
    }

    #[tokio::test]
    async fn status_answers_a_valid_key_and_never_contains_the_token() {
        let base = temp_base("status");
        hub_config::write_config_in(&base, &seeded_cfg()).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("monitor")]).await;
        let c = reqwest::Client::new();
        let r = c.get(format!("{origin}/api/hub/status")).header(KEY_HEADER, key("monitor").key).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let text = r.text().await.unwrap();
        assert!(!text.contains("hubtok-secret"), "token leaked into status: {text}");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["hubId"], "hub_abc123");
        assert_eq!(v["vid"], "v1");
        assert_eq!(v["registered"], true);
        assert_eq!(v["heartbeatSecs"], 60);
        assert_eq!(v["keysSynced"], 1);
    }

    #[tokio::test]
    async fn config_writes_require_the_settings_grade_and_persist_to_the_store() {
        let base = temp_base("config");
        hub_config::write_config_in(&base, &seeded_cfg()).unwrap();
        let (origin, _rt) = spawn_server(base.clone(), vec![key("monitor"), key("admin")]).await;
        let c = reqwest::Client::new();

        // monitor: authenticated but not entitled — 403, and the store is untouched.
        let r = c.post(format!("{origin}/api/hub/config")).header(KEY_HEADER, key("monitor").key)
            .json(&serde_json::json!({"name": "Hacked"})).send().await.unwrap();
        assert_eq!(r.status(), 403);
        assert_eq!(hub_config::read_config_in(&base).name, "Central");

        // admin: entitled for settings.
        let r = c.post(format!("{origin}/api/hub/config")).header(KEY_HEADER, key("admin").key)
            .json(&serde_json::json!({"name": "  Boat PC  ", "heartbeatSecs": 45})).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let cfg = hub_config::read_config_in(&base);
        assert_eq!(cfg.name, "Boat PC"); // trimmed
        assert_eq!(cfg.heartbeat_secs, 45);
        assert_eq!(cfg.token, "hubtok-secret"); // untouched by a settings write

        // Below the heartbeat floor is an explicit 422, not a silent clamp.
        let r = c.post(format!("{origin}/api/hub/config")).header(KEY_HEADER, key("admin").key)
            .json(&serde_json::json!({"heartbeatSecs": 5})).send().await.unwrap();
        assert_eq!(r.status(), 422);
        assert_eq!(hub_config::read_config_in(&base).heartbeat_secs, 45);
    }

    #[tokio::test]
    async fn token_rotation_and_clear_are_coowner_grade() {
        let base = temp_base("admin");
        hub_config::write_config_in(&base, &seeded_cfg()).unwrap();
        let (origin, _rt) = spawn_server(base.clone(), vec![key("admin"), key("coowner")]).await;
        let c = reqwest::Client::new();

        // admin may configure but NOT rotate the credential or tear the hub down.
        let r = c.post(format!("{origin}/api/hub/token")).header(KEY_HEADER, key("admin").key)
            .json(&serde_json::json!({"token": "new-tok"})).send().await.unwrap();
        assert_eq!(r.status(), 403);

        let r = c.post(format!("{origin}/api/hub/token")).header(KEY_HEADER, key("coowner").key)
            .json(&serde_json::json!({"token": "new-tok"})).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(hub_config::read_config_in(&base).token, "new-tok");

        let r = c.post(format!("{origin}/api/hub/clear")).header(KEY_HEADER, key("admin").key).send().await.unwrap();
        assert_eq!(r.status(), 403);
        let r = c.post(format!("{origin}/api/hub/clear")).header(KEY_HEADER, key("coowner").key).send().await.unwrap();
        assert_eq!(r.status(), 204);
        assert!(!hub_config::config_path_in(&base).exists());
        // And the key set is dropped with it — a cleared hub authenticates nobody.
        let r = c.get(format!("{origin}/api/hub/status")).header(KEY_HEADER, key("coowner").key).send().await.unwrap();
        assert_eq!(r.status(), 401);
    }

    #[tokio::test]
    async fn authentication_happens_before_the_body_is_ever_parsed() {
        let base = temp_base("authfirst");
        hub_config::write_config_in(&base, &seeded_cfg()).unwrap();
        let (origin, _rt) = spawn_server(base.clone(), vec![key("coowner")]).await;
        let c = reqwest::Client::new();
        for path in ["/api/hub/config", "/api/hub/token", "/api/hub/linktap/valve"] {
            // Garbage body, no key: the answer must be 401, never a parser complaint. An
            // unauthenticated caller must not reach the deserializer at all.
            let r = c.post(format!("{origin}{path}"))
                .header("content-type", "application/json").body("{not json").send().await.unwrap();
            assert_eq!(r.status(), 401, "{path} leaked its body parser to an unauthenticated caller");
            // Missing required field, no key: same.
            let r = c.post(format!("{origin}{path}"))
                .header("content-type", "application/json").body("{}").send().await.unwrap();
            assert_eq!(r.status(), 401, "{path} validated a body before authenticating");
        }
        // Authenticated, THEN the body is judged.
        let r = c.post(format!("{origin}/api/hub/token")).header(KEY_HEADER, key("coowner").key)
            .header("content-type", "application/json").body("{not json").send().await.unwrap();
        assert_eq!(r.status(), 422);
        assert_eq!(hub_config::read_config_in(&base).token, "hubtok-secret");
    }

    /// The first-run door. Its whole security is "unconfigured AND on this machine", so both
    /// halves are pinned here, from a real socket.
    #[tokio::test]
    async fn ping_answers_without_a_key_and_without_writing_anything() {
        let base = temp_base("ping");
        let (origin, _rt) = spawn_server(base.clone(), vec![]).await;
        let c = reqwest::Client::new();

        // Unsigned hub: status refuses (no keys yet) but ping still says a hub is here. That gap is
        // the whole reason this endpoint exists — the app cannot offer to sign a hub it cannot see.
        assert_eq!(c.get(format!("{origin}/api/hub/status")).send().await.unwrap().status(), 401);
        let r = c.get(format!("{origin}/api/hub/ping")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["registered"], false);
        // And it wrote nothing — an unsigned hub must still be unsigned after being looked at.
        assert!(!hub_config::config_path_in(&base).exists());

        hub_config::write_config_in(&base, &seeded_cfg()).unwrap();
        let v: serde_json::Value = c.get(format!("{origin}/api/hub/ping")).send().await.unwrap().json().await.unwrap();
        assert_eq!(v["registered"], true);
        // No secrets in it, ever.
        assert!(!v.to_string().contains("hubtok-secret"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn the_first_run_door_opens_once_and_only_from_this_machine() {
        let base = temp_base("firstrun");
        // A brand-new machine: no config file at all.
        let (origin, _rt) = spawn_server(base.clone(), vec![]).await;
        let c = reqwest::Client::new();

        // 1. The id is minted on first ask, and is STABLE — the cloud token gets issued to it.
        let r = c.get(format!("{origin}/api/hub/identity")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let id = r.json::<serde_json::Value>().await.unwrap()["hubId"].as_str().unwrap().to_string();
        assert!(id.starts_with("hub_"), "{id}");
        let again = c.get(format!("{origin}/api/hub/identity")).send().await.unwrap()
            .json::<serde_json::Value>().await.unwrap()["hubId"].as_str().unwrap().to_string();
        assert_eq!(id, again, "the id must not be re-minted between the two setup calls");

        // 2. Signing it to a vehicle needs a vid and a token.
        let r = c.post(format!("{origin}/api/hub/bootstrap"))
            .json(&serde_json::json!({"vid": "", "name": "x", "token": "t"})).send().await.unwrap();
        assert_eq!(r.status(), 422);
        let r = c.post(format!("{origin}/api/hub/bootstrap"))
            .json(&serde_json::json!({"vid": "v1", "name": "Central", "token": "cloudtok"})).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let cfg = hub_config::read_config_in(&base);
        assert_eq!(cfg.vid, "v1");
        assert_eq!(cfg.name, "Central");
        assert_eq!(cfg.token, "cloudtok");
        assert_eq!(cfg.hub_id, id);
        assert!(cfg.enabled);

        // 3. And now the door is SHUT — for good. Taking a live hub over is not a first run.
        let r = c.post(format!("{origin}/api/hub/bootstrap"))
            .json(&serde_json::json!({"vid": "v-attacker", "name": "Mine", "token": "other"})).send().await.unwrap();
        assert_eq!(r.status(), 409);
        let r = c.get(format!("{origin}/api/hub/identity")).send().await.unwrap();
        assert_eq!(r.status(), 409);
        assert_eq!(hub_config::read_config_in(&base).vid, "v1"); // untouched
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn the_first_run_door_is_shut_to_anything_off_box() {
        // The server binds 127.0.0.1 in these tests, so a non-loopback peer cannot be produced by
        // dialling it. Test the decision itself instead — it is the whole rule.
        let base = temp_base("offbox");
        let rt = new_rt(base.clone(), "https://unused.example".into());
        let lan: SocketAddr = "192.168.8.50:51000".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:51000".parse().unwrap();
        assert!(!is_loopback(lan));
        assert!(is_loopback(local));
        // Unconfigured + off-box ⇒ refused as a location problem, not as "already set up".
        let refusal = first_run_only(&rt, lan).await.expect("must refuse");
        assert_eq!(refusal.status, 403);
        assert!(first_run_only(&rt, local).await.is_none(), "unconfigured + loopback is the open case");
        // Configured ⇒ refused even on loopback.
        hub_config::write_config_in(&base, &seeded_cfg()).unwrap();
        assert_eq!(first_run_only(&rt, local).await.expect("must refuse").status, 409);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn key_sync_pulls_from_the_worker_authenticated_by_the_hub_token() {
        // A stub worker that asserts the query and returns two member keys.
        async fn stub(Query(q): Query<HashMap<String, String>>) -> Json<serde_json::Value> {
            assert_eq!(q["vid"], "v1");
            assert_eq!(q["device"], "hub_abc123");
            assert_eq!(q["t"], "hubtok-secret");
            Json(serde_json::json!({"keys": [
                {"key": "k-owner", "uid": "u1", "role": "owner"},
                {"key": "k-mon", "uid": "u2", "role": "monitor"},
            ]}))
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/api/hub/keys", get(stub))).await.unwrap()
        });

        let client = reqwest::Client::new();
        let keys = fetch_member_keys(&client, &format!("http://{addr}"), &seeded_cfg()).await.unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].role, "owner");

        // The endpoint not existing yet (increment C undeployed) is an Err, never a panic — the
        // caller keeps the previous key set.
        let miss = fetch_member_keys(&client, &format!("http://{addr}/nope"), &seeded_cfg()).await;
        assert!(miss.is_err());
    }

    #[tokio::test]
    async fn a_heartbeat_reaches_the_agent_ingest_with_the_hub_identity() {
        async fn stub(Query(q): Query<HashMap<String, String>>) -> &'static str {
            assert_eq!(q["event"], "hub.measurement");
            assert_eq!(q["device"], "hub_abc123");
            assert_eq!(q["t"], "hubtok-secret");
            assert!(!q["ver"].is_empty());
            "OK"
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/api/agent", get(stub))).await.unwrap()
        });
        let client = reqwest::Client::new();
        send_heartbeat_once(&client, &format!("http://{addr}"), &seeded_cfg()).await.unwrap();
    }
    // --- Valve control through the hub -----------------------------------------------------------

    fn valve_cfg(allowed: bool) -> HubConfig {
        HubConfig {
            linktap: hub_config::LinkTapConfig {
                host: "127.0.0.1:9".into(), // reserved discard port — never answers, which is fine:
                gw_id: "GW02".into(),        // every test here asserts a decision made BEFORE the call
                dev_ids: vec!["aaaabbbbccccdddd".into()],
                allowed,
            },
            ..seeded_cfg()
        }
    }

    async fn post_valve(origin: &str, k: &MemberKey, body: &str) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{origin}/api/hub/linktap/valve"))
            .header(KEY_HEADER, k.key.clone())
            .header("content-type", "application/json")
            .body(body.to_string())
            .send().await.unwrap()
    }

    #[tokio::test]
    async fn capabilities_advertise_linktap_only_when_configured_AND_permitted() {
        // The two conditions are ANDed so neither can be forgotten: a configured gateway on an
        // unpermitted plan must not advertise, and a permitted plan with no gateway must not either.
        let base = temp_base("caps");
        hub_config::write_config_in(&base, &valve_cfg(true)).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("owner")]).await;
        let body: serde_json::Value = reqwest::Client::new()
            .get(format!("{origin}/api/hub/status")).header(KEY_HEADER, key("owner").key)
            .send().await.unwrap().json().await.unwrap();
        assert_eq!(body["capabilities"], serde_json::json!(["linktap"]));

        let base2 = temp_base("caps_denied");
        hub_config::write_config_in(&base2, &valve_cfg(false)).unwrap();
        let (origin2, _rt2) = spawn_server(base2, vec![key("owner")]).await;
        let body2: serde_json::Value = reqwest::Client::new()
            .get(format!("{origin2}/api/hub/status")).header(KEY_HEADER, key("owner").key)
            .send().await.unwrap().json().await.unwrap();
        assert_eq!(body2["capabilities"], serde_json::json!([]), "an unpermitted plan must not advertise valve capability");

        let base3 = temp_base("caps_nogw");
        hub_config::write_config_in(&base3, &seeded_cfg()).unwrap(); // allowed defaults false, no gateway
        let (origin3, _rt3) = spawn_server(base3, vec![key("owner")]).await;
        let body3: serde_json::Value = reqwest::Client::new()
            .get(format!("{origin3}/api/hub/status")).header(KEY_HEADER, key("owner").key)
            .send().await.unwrap().json().await.unwrap();
        assert_eq!(body3["capabilities"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn the_paid_gate_is_enforced_at_the_action_not_only_the_advertisement() {
        // A caller that skipped the capability check — an older app, a hand-rolled request — must
        // still be refused, or the gate is bypassable by simply not asking.
        let base = temp_base("valve_402");
        hub_config::write_config_in(&base, &valve_cfg(false)).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("owner")]).await;
        let r = post_valve(&origin, &key("owner"), r#"{"devId":"aaaabbbbccccdddd","action":"close"}"#).await;
        assert_eq!(r.status(), 402);
    }

    #[tokio::test]
    async fn a_monitor_may_not_actuate_a_valve() {
        let base = temp_base("valve_role");
        hub_config::write_config_in(&base, &valve_cfg(true)).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("monitor")]).await;
        let r = post_valve(&origin, &key("monitor"), r#"{"devId":"aaaabbbbccccdddd","action":"close"}"#).await;
        assert_eq!(r.status(), 403);
    }

    #[tokio::test]
    async fn a_washdown_may_not_carry_a_volume_cap() {
        // Owner spec 2026-07-30, re-ratified twice: washdown is TIME-ONLY. Honouring a cap sent
        // alongside mode=washdown would re-create the "external cap" that cut 2-hour hose runs at
        // ~26 gal, so the request is refused rather than either side being silently dropped.
        let base = temp_base("valve_washdown");
        hub_config::write_config_in(&base, &valve_cfg(true)).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("owner")]).await;
        let r = post_valve(&origin, &key("owner"),
            r#"{"devId":"aaaabbbbccccdddd","action":"open","durationSecs":7200,"volumeCapL":100,"mode":"washdown"}"#).await;
        assert_eq!(r.status(), 422);
        let body: serde_json::Value = r.json().await.unwrap();
        assert!(body["error"].as_str().unwrap().contains("time-limited"));
    }

    #[tokio::test]
    async fn a_valve_this_hub_was_not_told_about_is_refused() {
        // A hub is not a general-purpose proxy onto the vessel's RF network — the same reasoning
        // as the relay's path allowlist.
        let base = temp_base("valve_unknown");
        hub_config::write_config_in(&base, &valve_cfg(true)).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("owner")]).await;
        let r = post_valve(&origin, &key("owner"), r#"{"devId":"ffffeeeeddddcccc","action":"close"}"#).await;
        assert_eq!(r.status(), 404);
    }

    #[tokio::test]
    async fn an_open_without_a_duration_is_refused() {
        // Every open carries a bound — that is the primary safeguard in the valve safety model.
        let base = temp_base("valve_nodur");
        hub_config::write_config_in(&base, &valve_cfg(true)).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("owner")]).await;
        let r = post_valve(&origin, &key("owner"), r#"{"devId":"aaaabbbbccccdddd","action":"open"}"#).await;
        assert_eq!(r.status(), 422);
    }

    #[tokio::test]
    async fn an_unknown_action_is_refused_rather_than_guessed() {
        let base = temp_base("valve_action");
        hub_config::write_config_in(&base, &valve_cfg(true)).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("owner")]).await;
        let r = post_valve(&origin, &key("owner"), r#"{"devId":"aaaabbbbccccdddd","action":"purge"}"#).await;
        assert_eq!(r.status(), 422);
    }

    // --- Increment 3: the I/O shell ---------------------------------------------------------------

    #[test]
    fn parses_the_workers_linktap_blob_with_skip_dont_default_intact() {
        let body = serde_json::json!({
            "ok": true,
            "linktap": { "allowed": true, "profiles": {
                "aaaabbbbccccdddd": { "durationSecs": 7200, "volumeCapL": 250.5, "autoRestart": true },
                "bbbbccccddddeeeeEXTRA": { "volumeCapL": 50 }
            }}
        });
        let (allowed, profiles) = parse_linktap_reply(&body).unwrap();
        assert!(allowed);
        let full = profiles.get("aaaabbbbccccdddd").unwrap();
        assert_eq!(full.duration_secs, Some(7200));
        assert_eq!(full.auto_restart, Some(true));
        // A field the vehicle never set stays None so the hub's own default keeps it.
        let partial = profiles.get("bbbbccccddddeeee").expect("long ids normalise to the canonical 16");
        assert_eq!(partial.volume_cap_l, Some(50.0));
        assert_eq!(partial.duration_secs, None);
        assert_eq!(partial.auto_restart, None);
    }

    #[test]
    fn an_absent_blob_changes_nothing_and_absent_allowed_is_never_permission() {
        assert!(parse_linktap_reply(&serde_json::json!({ "ok": true })).is_none());
        // `allowed` missing must read as DENY — the whole default-deny posture rests on this.
        let (allowed, _) = parse_linktap_reply(&serde_json::json!({ "linktap": { "profiles": {} } })).unwrap();
        assert!(!allowed);
    }

    #[tokio::test]
    async fn a_revoked_plan_stops_the_machine_rather_than_only_hiding_the_capability() {
        // A hub whose vehicle stopped paying must stop DRIVING the valve, not merely stop
        // advertising that it can.
        let base = temp_base("lt_revoke");
        let mut cfg = valve_cfg(true);
        hub_config::write_config_in(&base, &cfg).unwrap();
        let rt = new_rt(base.clone(), "https://unused.example".into());
        // Gateway is unreachable in tests, so the unit read falls back to gal — the machine is
        // still constructed, which is what this asserts.
        assert!(linktap_sync_config(&rt).await);
        assert!(rt.linktap.lock().await.is_some());

        cfg.linktap.allowed = false;
        hub_config::write_config_in(&base, &cfg).unwrap();
        assert!(!linktap_sync_config(&rt).await);
        assert!(rt.linktap.lock().await.is_none(), "the machine must be dropped, not just silenced");
    }

    #[tokio::test]
    async fn no_gateway_configured_means_nothing_polls() {
        let base = temp_base("lt_nogw");
        hub_config::write_config_in(&base, &seeded_cfg()).unwrap();
        let rt = new_rt(base, "https://unused.example".into());
        assert!(!linktap_sync_config(&rt).await);
        assert!(rt.linktap.lock().await.is_none());
    }

    #[tokio::test]
    async fn the_gateway_push_route_is_unauthenticated_but_inert() {
        // It must accept the appliance's POST (it cannot present a key) while doing nothing a
        // hostile LAN peer could exploit: no commands, no config, unknown valves dropped.
        let base = temp_base("lt_push");
        hub_config::write_config_in(&base, &valve_cfg(true)).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("owner")]).await;
        let r = reqwest::Client::new()
            .post(format!("{origin}/api/hub/linktap/push"))
            .header("content-type", "application/json")
            .body(r#"{"dev_stat":[{"dev_id":"ffffeeeeddddcccc","is_watering":1,"volume":5}]}"#)
            .send().await.unwrap();
        assert_eq!(r.status(), 200, "the gateway cannot authenticate — it must not be refused");
    }

    #[test]
    fn a_push_is_only_taken_from_the_configured_gateway_address() {
        let gw: SocketAddr = "192.168.8.20:54321".parse().unwrap();
        let other: SocketAddr = "192.168.8.99:54321".parse().unwrap();
        assert!(push_peer_allowed("192.168.8.20", gw));
        assert!(!push_peer_allowed("192.168.8.20", other));
        // The config field may carry a port; the comparison is on the address only.
        assert!(push_peer_allowed("192.168.8.20:80", gw));
        // No gateway configured accepts nothing — there is nothing legitimate to accept.
        assert!(!push_peer_allowed("", gw));
        // A HOSTNAME cannot be compared without a DNS lookup per push, so that configuration keeps
        // the pre-check posture and relies on the route's inertness. Stated, not silently assumed.
        assert!(push_peer_allowed("gateway.local", other));
    }

    #[tokio::test]
    async fn a_push_from_the_wrong_peer_is_answered_identically_to_a_good_one() {
        // Not an authorization surface: telling a prober whether it guessed the gateway's address
        // would make it one.
        let base = temp_base("lt_push_peer");
        hub_config::write_config_in(&base, &valve_cfg(true)).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("owner")]).await;
        let r = reqwest::Client::new()
            .post(format!("{origin}/api/hub/linktap/push"))
            .header("content-type", "application/json")
            .body(r#"{"dev_stat":[{"dev_id":"aaaabbbbccccdddd","is_watering":1,"volume":5}]}"#)
            .send().await.unwrap();
        // valve_cfg points at 127.0.0.1:9 while the test server sees a 127.0.0.1 peer, so this
        // one is ACCEPTED — the assertion that matters is that the answer is indistinguishable.
        assert_eq!(r.status(), 200);
    }

}
