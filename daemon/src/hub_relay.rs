// The hub's OUTBOUND WebSocket to the worker (owner, 2026-08-18: "it will also have a websocket to
// BRVG so a remote app can also control it").
//
// The LAN management API in hub_server.rs only answers people who are aboard. A boat sits behind
// marina NAT with no inbound route and no stable address, so the hub DIALS OUT and holds a socket
// to the worker; a remote app's authenticated call is relayed down it. Nothing is forwarded, nothing
// new listens on the public internet, and the connection is only ever established by the hub.
//
// Two kinds of frame come down:
//   * `keys` — the vehicle's member-key set, pushed by the worker. The hub applies it immediately,
//     so a member who was just added (or a hub that just reconnected) does not wait out the
//     five-minute HTTP poll. The poll stays as the backstop when the socket is down.
//   * `call` — one relayed management call, carrying the uid and role the WORKER authenticated.
//     It goes through the same `dispatch` as a LAN call, so both doors obey identical rules; the
//     hub re-applies its own role gates rather than trusting that the worker checked.
//
// The socket is authenticated by the hub's own device token, in the query string — the same
// credential its telemetry uses. That means the URL is a secret, so nothing here ever logs it, and
// anything that might carry it is redacted before it reaches a log line.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::Message;

use crate::hub_config::{self, HubConfig, MemberKey};
use crate::hub_server::{dispatch, Answer, Caller, Shared};

/// Longest wait between reconnection attempts. A hub that cannot reach the cloud has nothing better
/// to do than keep trying, but a boat on a metered cellular link should not retry in a tight loop.
const MAX_BACKOFF_SECS: u64 = 60;

/// PURE: the socket URL for this hub. `https` → `wss` so one pinned base serves both.
pub fn relay_socket_url(worker_base: &str, cfg: &HubConfig) -> Result<String, String> {
    let base = worker_base.trim_end_matches('/');
    let mut u = url::Url::parse(&format!("{base}/api/hub/ws")).map_err(|e| e.to_string())?;
    let scheme = match u.scheme() {
        "https" => "wss",
        "http" => "ws",
        other => return Err(format!("unsupported worker scheme {other}")),
    };
    u.set_scheme(scheme).map_err(|_| "could not set the websocket scheme".to_string())?;
    u.query_pairs_mut()
        .append_pair("vid", &cfg.vid)
        .append_pair("device", &cfg.hub_id)
        .append_pair("t", &cfg.token);
    Ok(u.to_string())
}

/// PURE: exponential backoff, capped. Attempt 0 is the first retry.
pub fn backoff_secs(attempt: u32) -> u64 {
    let secs = 1u64.checked_shl(attempt.min(16)).unwrap_or(MAX_BACKOFF_SECS);
    secs.min(MAX_BACKOFF_SECS)
}

/// PURE: strip a secret out of anything on its way to a log. The socket URL carries the hub token,
/// and a transport error that quotes the URL would otherwise write the credential to disk.
pub fn redact(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        return text.to_string();
    }
    text.replace(secret, "…redacted…")
}

/// What the worker can say. Anything else is IGNORED rather than guessed at — this end is talking
/// to a service we trust, but a bug there must not become undefined behaviour here.
#[derive(Debug, PartialEq)]
pub enum WorkerMessage {
    Keys(Vec<MemberKey>),
    Call {
        id: String,
        uid: String,
        role: String,
        method: String,
        path: String,
        body: String,
    },
}

#[derive(Deserialize)]
struct RawFrame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    keys: Option<Vec<MemberKey>>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    uid: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

/// PURE: read one frame from the worker.
pub fn parse_worker_message(raw: &str) -> Option<WorkerMessage> {
    let f: RawFrame = serde_json::from_str(raw).ok()?;
    match f.kind.as_str() {
        "keys" => Some(WorkerMessage::Keys(f.keys.unwrap_or_default())),
        "call" => {
            let id = f.id.filter(|s| !s.is_empty())?;
            // A call with no caller is not answerable: every action here is role-gated, and a
            // blank role would fall through to "denied" anyway. Refuse it at the parse instead,
            // so it can never be mistaken for an anonymous-but-valid request.
            let uid = f.uid.filter(|s| !s.is_empty())?;
            let role = f.role.filter(|s| !s.is_empty())?;
            Some(WorkerMessage::Call {
                id,
                uid,
                role,
                method: f.method.unwrap_or_default(),
                path: f.path.unwrap_or_default(),
                body: f.body.unwrap_or_default(),
            })
        }
        _ => None,
    }
}

/// PURE: the frame that answers one relayed call.
pub fn result_frame(id: &str, answer: &Answer) -> String {
    serde_json::json!({
        "type": "result",
        "id": id,
        "status": answer.status,
        "body": answer.body,
    })
    .to_string()
}

fn hello_frame(cfg: &HubConfig) -> String {
    serde_json::json!({
        "type": "hello",
        "hubId": cfg.hub_id,
        "version": env!("CARGO_PKG_VERSION"),
    })
    .to_string()
}

/// Apply a pushed key set: live for the running server, and persisted so a reboot with no internet
/// still authenticates known members.
async fn apply_keys(rt: &Shared, keys: Vec<MemberKey>) {
    *rt.keys.write().await = keys.clone();
    let _g = rt.store.lock().await;
    let mut cfg = hub_config::read_config_in(&rt.base);
    cfg.member_keys = keys;
    if let Err(e) = hub_config::write_config_in(&rt.base, &cfg) {
        eprintln!("hub: could not persist pushed member keys: {e}");
    }
}

/// Connect, serve, reconnect — forever. Returns only if the hub is not registered, which cannot
/// happen while the caller holds it in a loop.
pub async fn run(rt: Shared) {
    let mut attempt: u32 = 0;
    loop {
        let cfg = hub_config::read_config_in(&rt.base);
        if cfg.token.is_empty() || cfg.vid.is_empty() || cfg.hub_id.is_empty() {
            // Not registered yet. The bootstrap seed may land at any moment, so wait and re-read
            // rather than exiting — the same reasoning as the heartbeat loop.
            tokio::time::sleep(Duration::from_secs(30)).await;
            continue;
        }
        match serve_once(&rt, &cfg).await {
            Ok(()) => {
                eprintln!("hub: relay socket closed; reconnecting");
                attempt = 0;
            }
            Err(e) => {
                eprintln!("hub: relay socket failed: {}", redact(&e, &cfg.token));
                attempt = attempt.saturating_add(1);
            }
        }
        tokio::time::sleep(Duration::from_secs(backoff_secs(attempt))).await;
    }
}

/// One connection's lifetime. `Ok` means a clean close; `Err` carries a reason worth backing off for.
async fn serve_once(rt: &Shared, cfg: &HubConfig) -> Result<(), String> {
    let url = relay_socket_url(&rt.worker_base, cfg)?;
    let (mut socket, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .map_err(|e| e.to_string())?;
    eprintln!("hub: relay connected");
    socket
        .send(Message::Text(hello_frame(cfg)))
        .await
        .map_err(|e| e.to_string())?;

    while let Some(frame) = socket.next().await {
        let frame = frame.map_err(|e| e.to_string())?;
        let text = match frame {
            Message::Text(t) => t,
            Message::Ping(p) => {
                socket.send(Message::Pong(p)).await.map_err(|e| e.to_string())?;
                continue;
            }
            Message::Close(_) => return Ok(()),
            // Binary/Pong/raw frames are not part of this protocol.
            _ => continue,
        };
        let Some(msg) = parse_worker_message(&text) else { continue };
        match msg {
            WorkerMessage::Keys(keys) => {
                eprintln!("hub: member keys pushed ({})", keys.len());
                apply_keys(rt, keys).await;
            }
            WorkerMessage::Call { id, uid, role, method, path, body } => {
                let caller = Caller { uid, role };
                let answer = dispatch(rt, &caller, &method, &path, body.as_bytes()).await;
                socket
                    .send(Message::Text(result_frame(&id, &answer)))
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> HubConfig {
        HubConfig {
            hub_id: "hub_abc".into(), vid: "v1".into(), name: "Central".into(),
            enabled: true, heartbeat_secs: 60, token: "hubtok-secret".into(),
            ..HubConfig::default()
        }
    }

    #[test]
    fn the_socket_url_upgrades_the_scheme_and_carries_the_hub_identity() {
        let raw = relay_socket_url("https://api.example/", &cfg()).unwrap();
        assert!(raw.starts_with("wss://"), "{raw}");
        let u = url::Url::parse(&raw).unwrap();
        assert_eq!(u.path(), "/api/hub/ws");
        let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
        assert_eq!(q["vid"], "v1");
        assert_eq!(q["device"], "hub_abc");
        assert_eq!(q["t"], "hubtok-secret");
        // Plain http (a local worker in development) maps to ws, not wss.
        assert!(relay_socket_url("http://127.0.0.1:8787", &cfg()).unwrap().starts_with("ws://"));
        assert!(relay_socket_url("ftp://nope", &cfg()).is_err());
    }

    #[test]
    fn the_token_never_reaches_a_log_line() {
        let leaked = format!("connect failed: {}", relay_socket_url("https://api.example", &cfg()).unwrap());
        let safe = redact(&leaked, &cfg().token);
        assert!(!safe.contains("hubtok-secret"), "{safe}");
        assert!(safe.contains("…redacted…"));
        // An empty secret must not turn every log line into redaction soup.
        assert_eq!(redact("plain", ""), "plain");
    }

    #[test]
    fn backoff_climbs_and_then_stops_climbing() {
        assert_eq!(backoff_secs(0), 1);
        assert_eq!(backoff_secs(1), 2);
        assert_eq!(backoff_secs(4), 16);
        assert_eq!(backoff_secs(6), MAX_BACKOFF_SECS); // 64 → capped
        assert_eq!(backoff_secs(u32::MAX), MAX_BACKOFF_SECS); // no overflow, no zero-length wait
    }

    #[test]
    fn a_pushed_key_set_is_read_including_an_empty_one() {
        assert_eq!(
            parse_worker_message(r#"{"type":"keys","keys":[{"key":"k","uid":"u","role":"owner"}]}"#),
            Some(WorkerMessage::Keys(vec![MemberKey { key: "k".into(), uid: "u".into(), role: "owner".into() }])),
        );
        // "trust nobody" is a legitimate instruction — it must not read as "no message".
        assert_eq!(parse_worker_message(r#"{"type":"keys","keys":[]}"#), Some(WorkerMessage::Keys(vec![])));
        assert_eq!(parse_worker_message(r#"{"type":"keys"}"#), Some(WorkerMessage::Keys(vec![])));
    }

    #[test]
    fn a_relayed_call_carries_the_caller_the_worker_authenticated() {
        let msg = parse_worker_message(
            r#"{"type":"call","id":"r1","uid":"u1","role":"coowner","method":"POST","path":"/api/hub/config","body":"{\"name\":\"x\"}"}"#,
        );
        assert_eq!(msg, Some(WorkerMessage::Call {
            id: "r1".into(), uid: "u1".into(), role: "coowner".into(),
            method: "POST".into(), path: "/api/hub/config".into(), body: "{\"name\":\"x\"}".into(),
        }));
    }

    #[test]
    fn a_call_with_no_caller_is_refused_rather_than_treated_as_anonymous() {
        for raw in [
            r#"{"type":"call","id":"r1","role":"owner","method":"GET","path":"/api/hub/status"}"#,
            r#"{"type":"call","id":"r1","uid":"","role":"owner","method":"GET","path":"/api/hub/status"}"#,
            r#"{"type":"call","id":"r1","uid":"u1","method":"GET","path":"/api/hub/status"}"#,
            r#"{"type":"call","id":"r1","uid":"u1","role":"","method":"GET","path":"/api/hub/status"}"#,
            r#"{"type":"call","uid":"u1","role":"owner","method":"GET","path":"/api/hub/status"}"#,
        ] {
            assert_eq!(parse_worker_message(raw), None, "{raw}");
        }
    }

    #[test]
    fn frames_we_do_not_understand_are_ignored() {
        for raw in ["not json", "", "[]", "null", r#"{"type":"result","id":"r"}"#, r#"{"type":"whatever"}"#] {
            assert_eq!(parse_worker_message(raw), None, "{raw}");
        }
    }

    /// END TO END over a real socket: connect → hello → a pushed key set is applied and persisted
    /// → relayed calls are dispatched through the SAME core the LAN door uses, role gates included.
    /// A stub worker stands in for the real one; everything on the hub side is production code.
    #[tokio::test]
    async fn a_relayed_call_runs_through_the_same_gates_as_a_lan_call() {
        use crate::hub_server::new_rt;
        use tokio_tungstenite::tungstenite::Message as M;

        // --- the stub worker -------------------------------------------------------------------
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(frame)) = ws.next().await {
                let M::Text(t) = frame else { continue };
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "hello" {
                    tx.send(v).unwrap();
                    // Push a key set, then three calls: one a monitor may not make, one they may,
                    // and one path that is not the hub's at all.
                    ws.send(M::Text(r#"{"type":"keys","keys":[{"key":"k-mon","uid":"u-mon","role":"monitor"}]}"#.into())).await.unwrap();
                    ws.send(M::Text(r#"{"type":"call","id":"c1","uid":"u-mon","role":"monitor","method":"POST","path":"/api/hub/config","body":"{\"name\":\"Hacked\"}"}"#.into())).await.unwrap();
                    ws.send(M::Text(r#"{"type":"call","id":"c2","uid":"u-own","role":"owner","method":"POST","path":"/api/hub/config","body":"{\"name\":\"Boat PC\"}"}"#.into())).await.unwrap();
                    ws.send(M::Text(r#"{"type":"call","id":"c3","uid":"u-own","role":"owner","method":"GET","path":"/etc/passwd"}"#.into())).await.unwrap();
                } else {
                    tx.send(v).unwrap();
                }
            }
        });

        // --- the hub ---------------------------------------------------------------------------
        let base = temp_base("relay");
        hub_config::write_config_in(&base, &cfg()).unwrap();
        let rt = new_rt(base.clone(), format!("http://{addr}"));
        let serving = tokio::spawn({
            let rt = rt.clone();
            async move { let _ = serve_once(&rt, &cfg()).await; }
        });

        async fn next(
            rx: &mut tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
        ) -> serde_json::Value {
            tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("timed out").unwrap()
        }

        let hello = next(&mut rx).await;
        assert_eq!(hello["hubId"], "hub_abc");

        let c1 = next(&mut rx).await;
        assert_eq!(c1["id"], "c1");
        assert_eq!(c1["status"], 403, "a monitor must not be able to reconfigure the hub");
        assert!(c1["body"].as_str().unwrap().contains("admin"));

        let c2 = next(&mut rx).await;
        assert_eq!(c2["id"], "c2");
        assert_eq!(c2["status"], 200);
        // The write really happened, through the same store the LAN door writes.
        assert_eq!(hub_config::read_config_in(&base).name, "Boat PC");

        let c3 = next(&mut rx).await;
        assert_eq!(c3["status"], 404, "the relay reaches only the hub's own endpoints");

        // The pushed keys are live AND persisted, so a reboot with no internet still knows them.
        assert_eq!(rt.keys.read().await.len(), 1);
        assert_eq!(hub_config::read_config_in(&base).member_keys[0].uid, "u-mon");

        serving.abort();
        let _ = std::fs::remove_dir_all(&base);
    }

    fn temp_base(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("brvg-hub-relay-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_result_frame_is_what_the_worker_correlates_on() {
        let f = result_frame("r1", &Answer { status: 403, body: r#"{"error":"nope"}"#.into() });
        let v: serde_json::Value = serde_json::from_str(&f).unwrap();
        assert_eq!(v["type"], "result");
        assert_eq!(v["id"], "r1");
        assert_eq!(v["status"], 403);
        assert_eq!(v["body"], r#"{"error":"nope"}"#);
        // The worker refuses a non-numeric status, so this must never be stringified.
        assert!(v["status"].is_number());
    }
}
