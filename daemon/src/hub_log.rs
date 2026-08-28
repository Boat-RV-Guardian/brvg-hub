//! The hub's log file — and the reason it exists is a real failure, not tidiness.
//!
//! ⚠️ EVERY DIAGNOSTIC THIS DAEMON WRITES USED TO GO NOWHERE. The hub runs as a Windows service
//! (and a macOS LaunchDaemon), neither of which has a console, so every `eprintln!` was discarded
//! the moment the SCM started the binary. On 2026-08-28 that cost hours: zero-config discovery was
//! not populating on CENTRAL, and the daemon contained several lines written precisely to say why
//! — "no usable LAN address; skipping scan", "N gateways answered — not guessing which is this
//! vehicle's", "found X but could not save it" — any ONE of which would have ended the
//! investigation immediately. All of them were being thrown away, so the cause had to be guessed
//! at from the outside and still is not known.
//!
//! A hub is an unattended process on a boat nobody is standing on. If it cannot say what it did,
//! nobody can find out.
//!
//! DESIGN, and it is deliberately dull:
//!   * Append a timestamped line to `<base>/logs/hub.log`, and ALSO write to stderr, so running the
//!     binary by hand in a terminal behaves exactly as it always has.
//!   * Open-append-close per line rather than holding a handle. The volume is a handful of lines a
//!     minute; a held handle would mean a locked file that cannot be read or rotated while the
//!     service runs, which on Windows is precisely the thing that makes a log useless.
//!   * Rotate at 2 MB, keeping ONE previous file. A boat hub can run for months unattended, and an
//!     unbounded log on a small SSD is a different outage.
//!   * NEVER fail the caller. Logging that can break the thing it observes is worse than no
//!     logging: every error here is swallowed on purpose.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Where to append. `None` until `init` runs, which keeps early `hlog!` calls harmless.
static LOG_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Rotate past this size, keeping one previous file.
const MAX_BYTES: u64 = 2 * 1024 * 1024;

pub fn init(base: &Path) {
    let dir = base.join("BoatRVGuardian").join("logs");
    if std::fs::create_dir_all(&dir).is_err() {
        return; // no log file; stderr still works, and nothing else changes
    }
    if let Ok(mut g) = LOG_PATH.lock() {
        *g = Some(dir.join("hub.log"));
    }
}

/// The active log file, for the read endpoint. `None` before init or if the directory was unusable.
pub fn path() -> Option<PathBuf> {
    LOG_PATH.lock().ok().and_then(|g| g.clone())
}

/// UTC `YYYY-MM-DD HH:MM:SS`, computed rather than pulled in as a dependency.
///
/// Civil-from-days is Howard Hinnant's algorithm, which is correct across leap years and centuries
/// — the naive "365.25 days" version drifts by a day and makes two logs impossible to line up.
fn stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Append one line. Never panics, never propagates — see the module note.
pub fn write_line(msg: &str) {
    let line = format!("{} {}\n", stamp(), msg);
    eprint!("{line}");
    let Some(p) = path() else { return };
    // Rotate BEFORE appending, so the cap is honoured even by the line that would exceed it.
    if let Ok(md) = std::fs::metadata(&p) {
        if md.len() >= MAX_BYTES {
            let _ = std::fs::rename(&p, p.with_extension("log.1"));
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// The last `n` lines, newest last — what the read endpoint serves.
///
/// Reads the whole file rather than seeking backwards: it is capped at 2 MB by rotation, and a
/// simple correct reader beats a clever one on a path that only runs when somebody is debugging.
pub fn tail(n: usize) -> String {
    let Some(p) = path() else { return String::new() };
    let Ok(s) = std::fs::read_to_string(&p) else { return String::new() };
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Log a line to the file and stderr. Drop-in for `eprintln!`.
#[macro_export]
macro_rules! hlog {
    ($($arg:tt)*) => { $crate::hub_log::write_line(&format!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_timestamp_is_a_real_civil_date_not_a_365_25_approximation() {
        // Spot dates chosen to break the naive version: a leap day, and a century boundary that is
        // a leap year (2000 is, 1900 is not). Two logs that disagree by a day cannot be lined up.
        assert_eq!(&stamp_at(0)[..10], "1970-01-01");
        assert_eq!(&stamp_at(951_782_400)[..10], "2000-02-29");
        assert_eq!(&stamp_at(1_787_888_341)[..10], "2026-08-28");
    }

    #[test]
    fn the_time_of_day_is_right() {
        assert_eq!(stamp_at(0), "1970-01-01 00:00:00");
        assert_eq!(stamp_at(86_399), "1970-01-01 23:59:59");
        assert_eq!(stamp_at(86_400), "1970-01-02 00:00:00");
    }

    // The pure half of `stamp()`, so the formatting is testable without mocking the clock.
    fn stamp_at(secs: i64) -> String {
        let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, (rem % 3600) / 60, rem % 60)
    }

    #[test]
    fn tail_and_write_are_inert_before_init() {
        // Early calls must not panic and must not create files anywhere — the daemon logs before it
        // knows its base directory.
        write_line("this goes to stderr only");
        assert_eq!(tail(10), "");
    }
}
