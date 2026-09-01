//! Remote self-update for the daemon hub (phase 1b).
//!
//! Phase 1a made the gap VISIBLE (the hub reports when a newer release exists). This closes it: the
//! hub can install that release itself, triggered remotely, so a Pi or Mac hub on a boat behind
//! CGNAT no longer needs someone aboard with the app to update it — the same reach the hub-lite
//! router already has.
//!
//! THE MECHANISM, and why it is safe on Unix. The daemon downloads the platform binary from the
//! SAME signed release the app installer already trusts (releases/latest/download + SHA256SUMS),
//! verifies the hash, and — the belt to that suspenders — runs the new binary's `--version` to prove
//! it actually executes on THIS box (a correct hash on an incompatible binary would still not run).
//! Only then does it back up the running binary and swap the new one in. Then it EXITS: the service
//! supervisor relaunches it from the replaced path (macOS LaunchDaemon KeepAlive; a Linux unit with
//! Restart=always). Replace-and-exit rather than exec-in-place because a supervised appliance's
//! restart is the supervisor's job, and a half-swapped running process is the failure we refuse.
//!
//! ROLLBACK. The previous binary is kept alongside as `.prev`. `restore_previous` swaps it back —
//! the local undo for a hub nobody can reach, mirroring hub-lite's `/etc/brvg-hub-lite.prev`.
//!
//! WINDOWS IS NOT HERE (phase 1c). A running .exe cannot be replaced on Windows without a helper
//! that swaps it after this process exits; until that exists, `asset_for` returns None on windows
//! and the update path refuses cleanly, leaving the app's local installer as the Windows route.

/// The release asset for a platform+arch, or None if remote self-update is not supported there.
/// Names mirror brvg-hub's daemon-release.yml exactly — a rename there must move here too.
pub fn asset_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", _) => Some("brvg-hub-macos-universal"), // one universal binary serves both arches
        ("linux", "x86_64") => Some("brvg-hub-linux-x64"),
        ("linux", "aarch64") => Some("brvg-hub-linux-arm64"),
        // Windows: a running .exe can't replace itself in place — phase 1c.
        _ => None,
    }
}

/// The expected SHA-256 for `asset` out of a SHA256SUMS file (`<hex>␠␠<name>` per line, the format
/// `sha256sum` writes and the release publishes). Lowercased; None if the asset is not listed.
pub fn sha256_for(sums: &str, asset: &str) -> Option<String> {
    for line in sums.lines() {
        let mut it = line.split_whitespace();
        let hash = it.next()?;
        // The filename is the LAST field — sha256sum writes "<hash>  <name>" (two spaces), and a
        // name never contains whitespace here.
        let name = it.last().unwrap_or("");
        if name == asset && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

/// Does `bytes` hash to `expected` (case-insensitive hex)? The one gate every downloaded byte passes
/// before it is allowed anywhere near the running binary.
pub fn sha256_matches(bytes: &[u8], expected: &str) -> bool {
    use sha2::{Digest, Sha256};
    let got = Sha256::digest(bytes);
    let got_hex = got.iter().map(|b| format!("{b:02x}")).collect::<String>();
    got_hex.eq_ignore_ascii_case(expected.trim())
}

/// The three sibling paths involved in a swap, derived from the running binary's path. Kept pure so
/// the naming (and the invariant that all three live in the same directory, so a rename is atomic)
/// is testable without touching a filesystem.
pub struct SwapPaths {
    pub current: std::path::PathBuf,
    pub incoming: std::path::PathBuf,
    pub previous: std::path::PathBuf,
}

pub fn swap_paths(current: &std::path::Path) -> SwapPaths {
    let dir = current
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let name = current
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("brvg-hub");
    SwapPaths {
        current: current.to_path_buf(),
        incoming: dir.join(format!("{name}.new")),
        previous: dir.join(format!("{name}.prev")),
    }
}

/// The base URL every release asset hangs off — the stable `latest` link the daemon family claims.
pub const LATEST_DOWNLOAD_BASE: &str =
    "https://github.com/Boat-RV-Guardian/brvg-hub/releases/latest/download";

/// Outcome of an update attempt, for the caller's log and the HTTP reply.
#[derive(Debug)]
pub enum UpdateOutcome {
    /// The binary was replaced; the process is about to exit for the supervisor to relaunch it.
    Swapped { to_version: String },
    /// Nothing to do — already current.
    UpToDate,
    /// Refused or failed, with a reason safe to show an operator.
    Failed(String),
}

/// Run the whole update: resolve the asset, download it and SHA256SUMS, verify the hash, prove the
/// new binary executes, back up the current one and swap. Does NOT exit — the caller decides when to
/// (so the HTTP reply is sent first). Every failure leaves the running binary untouched.
pub async fn perform_update(client: &reqwest::Client) -> UpdateOutcome {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let Some(asset) = asset_for(os, arch) else {
        return UpdateOutcome::Failed(format!(
            "remote self-update is not supported on {os}/{arch} yet — use the app's installer"
        ));
    };

    // Where is the running binary? current_exe() is the path the supervisor launched, which is
    // exactly what we must replace so the relaunch picks up the new one.
    let current = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            return UpdateOutcome::Failed(format!("could not locate the running binary: {e}"))
        }
    };
    let paths = swap_paths(&current);

    // 1. The checksums, then the binary. Both from the signed `latest` release.
    let sums = match client
        .get(format!("{LATEST_DOWNLOAD_BASE}/SHA256SUMS"))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
        Ok(r) => {
            return UpdateOutcome::Failed(format!(
                "could not fetch checksums: HTTP {}",
                r.status().as_u16()
            ))
        }
        Err(e) => {
            return UpdateOutcome::Failed(format!("could not fetch checksums: {}", e.without_url()))
        }
    };
    let Some(expected) = sha256_for(&sums, asset) else {
        return UpdateOutcome::Failed(format!("the release has no checksum for {asset}"));
    };

    let bytes = match client
        .get(format!("{LATEST_DOWNLOAD_BASE}/{asset}"))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => match r.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return UpdateOutcome::Failed(format!("download failed: {}", e.without_url()))
            }
        },
        Ok(r) => {
            return UpdateOutcome::Failed(format!("download failed: HTTP {}", r.status().as_u16()))
        }
        Err(e) => return UpdateOutcome::Failed(format!("download failed: {}", e.without_url())),
    };

    // 2. Verify BEFORE anything touches disk beside the running binary.
    if !sha256_matches(&bytes, &expected) {
        return UpdateOutcome::Failed("the download did not match its published checksum".into());
    }

    // 3. Write the incoming binary, make it executable, and PROVE it runs on this box — a valid hash
    //    on a binary that cannot exec here (wrong libc, wrong arch slipped through) must not be swapped.
    if let Err(e) = std::fs::write(&paths.incoming, &bytes) {
        return UpdateOutcome::Failed(format!("could not stage the new binary: {e}"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) =
            std::fs::set_permissions(&paths.incoming, std::fs::Permissions::from_mode(0o755))
        {
            let _ = std::fs::remove_file(&paths.incoming);
            return UpdateOutcome::Failed(format!("could not make the new binary executable: {e}"));
        }
    }
    let new_version = match std::process::Command::new(&paths.incoming)
        .arg("--version")
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        Ok(_) | Err(_) => {
            let _ = std::fs::remove_file(&paths.incoming);
            return UpdateOutcome::Failed(
                "the downloaded binary would not run its version check".into(),
            );
        }
    };
    let current_version = env!("CARGO_PKG_VERSION");
    if !crate::update_check::is_newer(&new_version, current_version) {
        // Not newer (someone triggered an update with nothing to install) — clean up, do nothing.
        let _ = std::fs::remove_file(&paths.incoming);
        return UpdateOutcome::UpToDate;
    }

    // 4. Swap: keep the current binary as .prev, move the new one into place. Same-directory renames
    //    so each is atomic; on failure the running binary is still where the supervisor expects it.
    let _ = std::fs::remove_file(&paths.previous);
    if let Err(e) = std::fs::rename(&paths.current, &paths.previous) {
        let _ = std::fs::remove_file(&paths.incoming);
        return UpdateOutcome::Failed(format!("could not back up the current binary: {e}"));
    }
    if let Err(e) = std::fs::rename(&paths.incoming, &paths.current) {
        // Put the current one back — better a failed update than no binary at all.
        let _ = std::fs::rename(&paths.previous, &paths.current);
        return UpdateOutcome::Failed(format!("could not install the new binary: {e}"));
    }
    UpdateOutcome::Swapped {
        to_version: new_version,
    }
}

/// Roll back to the `.prev` binary kept by the last successful swap. The local undo for a hub nobody
/// can reach. Returns Ok(()) once .prev is back in place (the caller then exits to relaunch it).
pub fn restore_previous() -> Result<(), String> {
    let current =
        std::env::current_exe().map_err(|e| format!("could not locate the running binary: {e}"))?;
    let paths = swap_paths(&current);
    if !paths.previous.exists() {
        return Err("there is no previous version to roll back to".into());
    }
    // Move current aside (so a failed rename doesn't lose it), put prev back, drop the aside copy.
    let aside = paths.current.with_extension("rollback-tmp");
    std::fs::rename(&paths.current, &aside)
        .map_err(|e| format!("could not move the current binary aside: {e}"))?;
    if let Err(e) = std::fs::rename(&paths.previous, &paths.current) {
        let _ = std::fs::rename(&aside, &paths.current); // undo
        return Err(format!("could not restore the previous binary: {e}"));
    }
    let _ = std::fs::remove_file(&aside);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_names_match_the_release_workflow_and_windows_is_declined() {
        assert_eq!(
            asset_for("macos", "aarch64"),
            Some("brvg-hub-macos-universal")
        );
        assert_eq!(
            asset_for("macos", "x86_64"),
            Some("brvg-hub-macos-universal")
        );
        assert_eq!(asset_for("linux", "x86_64"), Some("brvg-hub-linux-x64"));
        assert_eq!(asset_for("linux", "aarch64"), Some("brvg-hub-linux-arm64"));
        assert_eq!(asset_for("windows", "x86_64"), None); // phase 1c
        assert_eq!(asset_for("linux", "riscv64"), None); // no build for it
    }

    #[test]
    fn sha256_for_picks_the_right_line() {
        let sums = "\
641417d64da634a0e45530368b6f519649b60671547937cde9511b927c323274  brvg-hub-linux-arm64
ed85222860fdf06c27399f3c96d68e5b68b43ac568bf35a4eab1e384d5ab73ea  brvg-hub-linux-x64
ef1142ca3e7b8a87e7d606af5aca0b0ec1dec629ac8ba43bde75837bcf46da22  brvg-hub-macos-universal
";
        assert_eq!(
            sha256_for(sums, "brvg-hub-linux-x64").as_deref(),
            Some("ed85222860fdf06c27399f3c96d68e5b68b43ac568bf35a4eab1e384d5ab73ea"),
        );
        assert_eq!(
            sha256_for(sums, "brvg-hub-macos-universal").unwrap().len(),
            64
        );
        assert_eq!(sha256_for(sums, "brvg-hub-not-a-thing"), None);
    }

    #[test]
    fn sha256_for_rejects_a_malformed_hash() {
        // A truncated or non-hex "hash" must not be accepted as one.
        assert_eq!(
            sha256_for("xyz  brvg-hub-linux-x64", "brvg-hub-linux-x64"),
            None
        );
        assert_eq!(
            sha256_for("dead  brvg-hub-linux-x64", "brvg-hub-linux-x64"),
            None
        );
        assert_eq!(sha256_for("", "brvg-hub-linux-x64"), None);
    }

    #[test]
    fn sha256_matches_is_a_real_digest_check() {
        // echo -n "hello" | sha256sum
        let expected = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(sha256_matches(b"hello", expected));
        assert!(sha256_matches(b"hello", &expected.to_ascii_uppercase())); // case-insensitive
        assert!(!sha256_matches(b"hell0", expected)); // one byte off → no match
        assert!(!sha256_matches(b"hello", "deadbeef"));
    }

    #[test]
    fn swap_paths_are_siblings_so_the_rename_is_atomic() {
        let p = swap_paths(std::path::Path::new(
            "/Library/Application Support/BoatRVGuardian/bin/brvg-hub",
        ));
        assert_eq!(p.incoming.file_name().unwrap(), "brvg-hub.new");
        assert_eq!(p.previous.file_name().unwrap(), "brvg-hub.prev");
        assert_eq!(p.incoming.parent(), p.current.parent());
        assert_eq!(p.previous.parent(), p.current.parent());
    }
}
