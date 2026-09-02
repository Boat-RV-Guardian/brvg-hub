//! Update visibility for the daemon hub (phase 1a of remote hub updates).
//!
//! The daemon is the FULL hub, but until now it was the LESS updatable one: no self-update, no
//! remote trigger, and — the gap this closes — no way for anyone to even SEE that a hub was behind.
//! A hub-lite router advertises its version on every report and the fleet console shows it; a
//! daemon hub reported its running version but never whether a newer one existed.
//!
//! This module answers only "is there a newer release, and which". It performs NO self-modification
//! — that is phase 1b. Knowing is separable from acting, and shipping the knowing first makes the
//! fleet legible (the operator console shows "0.3.23 → 0.3.24 available") at near-zero risk.
//!
//! WHERE "latest" COMES FROM. The daemon's releases are `daemon-v*` tags on the public brvg-hub
//! repo, and the daemon release workflow marks that family `--latest`. So GitHub's own
//! `releases/latest` redirect resolves to the newest daemon tag — no API token, no rate-limited
//! API call, just the redirect every "install the hub" link already follows. The hub-lite feed
//! release is deliberately `--latest=false`, so it can never be mistaken for a daemon version here.

/// A semver triple. Pre-release/build suffixes are ignored — releases are plain `x.y.z`.
pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    // Accept a bare "0.3.23" or a tag "daemon-v0.3.23" / "v0.3.23".
    let v = s.trim();
    let v = v.rsplit(['-', 'v']).next().unwrap_or(v); // after the last '-' or 'v'
    let core = v.split(['+', '-']).next().unwrap_or(v); // drop any build/pre suffix
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None; // "1.2.3.4" is not a version we understand — refuse rather than guess
    }
    Some((major, minor, patch))
}

/// Is `latest` strictly newer than `current`? Unparseable input is never "newer" — a version check
/// that cannot read a version must not cry wolf, and must never hide a real update either. Both
/// unreadable → not newer (report nothing).
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// Extract the daemon version from the URL `releases/latest` redirected to, e.g.
/// `https://github.com/DockNeighbor/DockNeighbor-Hub/releases/tag/daemon-v0.3.24` → `0.3.24`.
/// Only the `daemon-v` family counts: a `hub-lite-feed` or any other tag returns None, so this can
/// never mistake the feed release for a daemon version.
pub fn version_from_release_url(url: &str) -> Option<String> {
    let tag = url.rsplit("/tag/").next().filter(|t| *t != url)?;
    let ver = tag.strip_prefix("daemon-v")?;
    parse_version(ver).map(|_| ver.to_string())
}

/// The version newer than `current`, or None if `current` is up to date (or `latest` is unreadable).
pub fn newer_than(latest: &str, current: &str) -> Option<String> {
    if is_newer(latest, current) { Some(latest.to_string()) } else { None }
}

/// The URL whose redirect names the latest daemon release. Public repo, no token — the same
/// `releases/latest` link the installer follows.
///
/// ⚠️ Slug moved 2026-09-02 — see the note on `self_update::LATEST_DOWNLOAD_BASE`, including the
/// standing rule never to create a repo at the old slug. `version_from_release_url` reads the tag
/// out of whatever url the redirect lands on and ignores the org/repo path entirely, so the rename
/// cannot confuse version detection; this constant is updated so NEW installs stop depending on a
/// redirect at all.
pub const LATEST_RELEASE_URL: &str =
    "https://github.com/DockNeighbor/DockNeighbor-Hub/releases/latest";

/// Ask GitHub for the latest daemon version by following the `releases/latest` redirect and reading
/// the tag out of the URL it landed on. Returns None on any network/parse failure — a hub that
/// cannot reach GitHub (offline, or under a router lockdown) simply reports no update, never an error
/// and never a false positive.
pub async fn fetch_latest_version(client: &reqwest::Client) -> Option<String> {
    let res = client.get(LATEST_RELEASE_URL).send().await.ok()?;
    // reqwest follows redirects by default, so the final URL is the tag page.
    version_from_release_url(res.url().as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_and_tagged() {
        assert_eq!(parse_version("0.3.23"), Some((0, 3, 23)));
        assert_eq!(parse_version("daemon-v0.3.23"), Some((0, 3, 23)));
        assert_eq!(parse_version("v1.0.0"), Some((1, 0, 0)));
        assert_eq!(parse_version(" 2.10.4 "), Some((2, 10, 4)));
    }

    #[test]
    fn rejects_junk() {
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("0.3"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("latest"), None);
    }

    #[test]
    fn newer_is_a_real_semver_compare_not_string_compare() {
        // 0.3.9 vs 0.3.10 — the classic string-compare trap ("0.3.9" > "0.3.10" lexically).
        assert!(is_newer("0.3.10", "0.3.9"));
        assert!(!is_newer("0.3.9", "0.3.10"));
        // minor and major dominate patch.
        assert!(is_newer("0.4.0", "0.3.99"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.3.23", "0.3.23")); // equal is not newer
    }

    #[test]
    fn unreadable_never_cries_wolf() {
        assert!(!is_newer("garbage", "0.3.23"));
        assert!(!is_newer("0.3.24", "garbage"));
        assert_eq!(newer_than("garbage", "0.3.23"), None);
    }

    #[test]
    fn extracts_only_the_daemon_family_from_a_redirect_url() {
        assert_eq!(
            version_from_release_url("https://github.com/DockNeighbor/DockNeighbor-Hub/releases/tag/daemon-v0.3.24"),
            Some("0.3.24".to_string()),
        );
        // The feed release must never be read as a daemon version.
        assert_eq!(
            version_from_release_url("https://github.com/DockNeighbor/DockNeighbor-Hub/releases/tag/hub-lite-feed"),
            None,
        );
        // A URL that never redirected to a tag page (no /tag/ segment) yields nothing.
        assert_eq!(
            version_from_release_url("https://github.com/DockNeighbor/DockNeighbor-Hub/releases/latest"),
            None,
        );
    }

    #[test]
    fn end_to_end_shape() {
        let latest = version_from_release_url(
            "https://github.com/DockNeighbor/DockNeighbor-Hub/releases/tag/daemon-v0.3.24",
        ).unwrap();
        assert_eq!(newer_than(&latest, "0.3.23"), Some("0.3.24".to_string()));
        assert_eq!(newer_than(&latest, "0.3.24"), None);
    }
}
