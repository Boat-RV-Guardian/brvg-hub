// The hub SERVER — increment A of HUB-PROXY.md 2026-08-18 (late): "the hub is a SERVER; apps are
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
    })
}

pub fn router(rt: Shared) -> Router {
    Router::new()
        .route("/api/hub/status", get(h_status))
        .route("/api/hub/config", post(h_config))
        .route("/api/hub/token", post(h_token))
        .route("/api/hub/clear", post(h_clear))
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
    }
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
/// by the worker, which authenticated the user itself (HUB-PROXY.md 2026-08-18 late). Either way
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
            if let Err(e) = send_heartbeat_once(&client, &rt.worker_base, &cfg).await {
                eprintln!("hub: heartbeat failed: {e}");
            }
            let secs = u64::from(cfg.heartbeat_secs).max(HEARTBEAT_FLOOR_SECS);
            tokio::time::sleep(Duration::from_secs(secs)).await;
        } else {
            tokio::time::sleep(Duration::from_secs(UNREGISTERED_POLL_SECS)).await;
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
        for path in ["/api/hub/config", "/api/hub/token"] {
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
}
