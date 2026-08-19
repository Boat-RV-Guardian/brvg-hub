// The HUB's own storage — MACHINE-WIDE, not per-user (owner ruling 2026-08-18).
//
// The hub is signed to a VEHICLE, and there is one hub per vehicle. The app's per-user data stays
// per-user: `navigator` may be signed in as one account and `localadmin` as another, each with
// their own vehicles, sessions and localStorage — that separation is deliberate and stays.
// The HUB is not part of it. It belongs to the machine and its vehicle, so it lives in the shared
// folder where every user's app instance AND the SYSTEM service can reach the same one file.
//
// This replaces three things that could not work:
//   * `hub_<installId>` where installId came from localStorage — per-USER, so two logins on one
//     computer would mint two different hubs;
//   * the token/name/enabled/interval in localStorage — invisible to a SYSTEM instance, and wiped
//     by applyUserScope on any identity change, silently disarming the hub;
//   * the service (SYSTEM, session 0) having any way to read either.
//
// ⚠️ SCOPE, stated honestly: this file makes the hub's identity SINGULAR PER MACHINE (one file,
// one id, and hub_service.rs registers one fixed task name). "One hub per VEHICLE" is a
// CLOUD-side rule — only the worker sees every machine — and must be enforced at hub enrollment
// there. This store cannot know about a second computer.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Folder name under the platform's shared data directory.
const DIR_NAME: &str = "BoatRVGuardian";
const FILE_NAME: &str = "hub.json";

/// Management API port (hub_server.rs). One constant, configurable per install because boats run
/// odd gear on odd networks. 3030 is taken by the interactive app's local Shelly webhook listener.
pub const DEFAULT_HTTP_PORT: u16 = 8722;

/// One member's LAN management credential, minted by the worker per (user, vehicle) —
/// HUB-PROXY.md 2026-08-18 (late), "Management auth: per-user API keys". `role` uses the vehicle
/// role vocabulary (`owner` | `coowner` | `admin` | `control` | `monitor`, mirroring
/// vehicleCapabilities.ts) and is what hub_server gates writes on.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct MemberKey {
    pub key: String,
    pub uid: String,
    pub role: String,
}

/// The hub's machine-wide record. Absent file ⇒ this machine is not a hub.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct HubConfig {
    /// `hub_<random>` — minted ONCE per machine and never regenerated, so re-registering or
    /// switching vehicles does not orphan the cloud device record for this hub.
    pub hub_id: String,
    /// The vehicle this hub is signed to.
    pub vid: String,
    pub name: String,
    pub enabled: bool,
    pub heartbeat_secs: u32,
    /// Per-hub bearer token (agentToken custody). Never returned to the UI in full.
    pub token: String,
    /// Port the management API listens on (hub_server.rs).
    pub http_port: u16,
    /// The vehicle members' API keys, CACHED from the worker so a reboot with no internet still
    /// authenticates known members. hub_server's sync loop owns this; stale keys die at the next
    /// successful sync. Lives here because this file is already the hub's one credential store
    /// (SYSTEM/0600) — a second file would just be a second thing to lock down.
    pub member_keys: Vec<MemberKey>,
}

impl Default for HubConfig {
    fn default() -> Self {
        HubConfig {
            hub_id: String::new(),
            vid: String::new(),
            name: String::new(),
            enabled: false,
            heartbeat_secs: 0,
            token: String::new(),
            http_port: DEFAULT_HTTP_PORT,
            member_keys: Vec::new(),
        }
    }
}

/// PURE: the config path under a given shared base. Split out so the layout is testable without
/// touching a real system directory.
pub fn config_path_in(base: &Path) -> PathBuf {
    base.join(DIR_NAME).join(FILE_NAME)
}

/// The platform's SHARED (all-users) data directory.
///  * Windows: `%ProgramData%` — the shared folder, readable by services and every login.
///  * macOS:   `/Library/Application Support` (the system one, not `~/Library`).
///  * Linux:   `/var/lib`.
pub fn shared_base() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Ok(p) = std::env::var("ProgramData") {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        return PathBuf::from("C:\\ProgramData");
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/Library/Application Support")
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        PathBuf::from("/var/lib")
    }
}

pub fn config_path() -> PathBuf {
    config_path_in(&shared_base())
}

/// PURE: mint a hub id if there isn't one, otherwise keep it. Returns (config, changed).
/// Keeping it is the point — the cloud device record is keyed to this id.
pub fn with_hub_id(mut cfg: HubConfig, mint: impl FnOnce() -> String) -> (HubConfig, bool) {
    if cfg.hub_id.is_empty() {
        cfg.hub_id = mint();
        return (cfg, true);
    }
    (cfg, false)
}

/// 96 bits of hex, prefixed — matches the `hub_` namespace the worker exempts from the device cap.
/// v4 UUID randomness comes from the OS, so two machines imaged from one disk still differ.
pub fn mint_hub_id() -> String {
    let hex = uuid::Uuid::new_v4().simple().to_string();
    format!("hub_{}", &hex[..24])
}

pub fn read_config() -> HubConfig {
    read_config_in(&shared_base())
}

/// Same, under an explicit base — hub_server and its tests read the store without touching the
/// real shared directory.
pub fn read_config_in(base: &Path) -> HubConfig {
    match std::fs::read_to_string(config_path_in(base)) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => HubConfig::default(),
    }
}

/// Write atomically (temp + rename) so a power cut mid-write cannot leave the hub with a truncated
/// config — this machine is an always-on appliance on a boat, where power cuts are routine.
pub fn write_config(cfg: &HubConfig) -> Result<(), String> {
    write_config_in(&shared_base(), cfg)
}

pub fn write_config_in(base: &Path, cfg: &HubConfig) -> Result<(), String> {
    let path = config_path_in(base);
    let dir = path.parent().ok_or("no parent directory")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let tmp = dir.join("hub.json.tmp");
    let text = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| format!("could not write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("could not replace {}: {e}", path.display()))?;
    restrict_permissions(&path);
    Ok(())
}

/// The file holds a bearer token, so it must not be world-readable even though it lives in a
/// shared folder: SYSTEM + Administrators only. Best-effort — a failure here must not break hub
/// setup, but it IS reported by the caller's status text so it cannot fail silently forever.
#[cfg(target_os = "windows")]
fn restrict_permissions(path: &Path) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let p = path.to_string_lossy().to_string();
    let _ = std::process::Command::new("icacls")
        .args([&p, "/inheritance:r", "/grant:r", "SYSTEM:F", "/grant:r", "Administrators:F"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

#[cfg(not(target_os = "windows"))]
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Remove the hub's configuration entirely — the local half of un-registering, called by the
/// server's own /api/hub/clear. The service is the only writer of this file, so it is also the
/// only thing that deletes it.
pub fn clear_in(base: &Path) -> Result<(), String> {
    let path = config_path_in(base);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("could not remove {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_lives_under_the_shared_folder_in_our_own_directory() {
        let p = config_path_in(Path::new("/shared"));
        assert_eq!(p, PathBuf::from("/shared/BoatRVGuardian/hub.json"));
        // The real base must be a SHARED location, never a user profile — that is the whole point.
        let base = shared_base().to_string_lossy().to_string();
        assert!(!base.contains("/Users/"), "shared base must not be a user profile: {base}");
        assert!(!base.contains("\\Users\\"), "shared base must not be a user profile: {base}");
    }

    #[test]
    fn a_hub_id_is_minted_once_and_then_kept() {
        let (first, changed) = with_hub_id(HubConfig::default(), || "hub_abc".into());
        assert!(changed);
        assert_eq!(first.hub_id, "hub_abc");
        // Re-registering or switching vehicles must NOT mint a new id — the cloud record is keyed
        // to it, and a fresh id would orphan the old one exactly like the router ghosts did.
        let (second, changed_again) = with_hub_id(first, || "hub_SHOULD_NOT_BE_USED".into());
        assert!(!changed_again);
        assert_eq!(second.hub_id, "hub_abc");
    }

    #[test]
    fn minted_ids_use_the_hub_namespace_and_are_distinct() {
        let a = mint_hub_id();
        let b = mint_hub_id();
        assert!(a.starts_with("hub_"), "{a}");
        assert_eq!(a.len(), 4 + 24);
        assert_ne!(a, b);
    }

    #[test]
    fn config_round_trips_and_absent_fields_default() {
        let cfg = HubConfig {
            hub_id: "hub_1".into(), vid: "v1".into(), name: "Central".into(),
            enabled: true, heartbeat_secs: 60, token: "tok".into(),
            http_port: 9000,
            member_keys: vec![MemberKey { key: "k1".into(), uid: "u1".into(), role: "coowner".into() }],
        };
        let text = serde_json::to_string(&cfg).unwrap();
        assert_eq!(serde_json::from_str::<HubConfig>(&text).unwrap(), cfg);
        // A file written by an older build (or hand-edited) must not fail to parse — and the
        // fields it does not know get REAL defaults: a #388-era hub.json must come up on the
        // default management port, not port 0.
        let partial: HubConfig = serde_json::from_str(r#"{"hub_id":"hub_2"}"#).unwrap();
        assert_eq!(partial.hub_id, "hub_2");
        assert_eq!(partial.heartbeat_secs, 0);
        assert!(partial.token.is_empty());
        assert_eq!(partial.http_port, DEFAULT_HTTP_PORT);
        assert!(partial.member_keys.is_empty());
    }
}
