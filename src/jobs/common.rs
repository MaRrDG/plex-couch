//! Helpers shared by the Phase 2b jobs: the optional `jobs` config section, clocks, ISO
//! dates, quiet error logging.
//!
//! Config (all optional; the defaults are the live paths):
//! ```json
//! "jobs": {
//!   "watchlist":   {"discover_url": "https://discover.provider.plex.tv"},
//!   "strip_dv":    {"ffmpeg_bin": "", "ffmpeg_packages": "%LOCALAPPDATA%\\Microsoft\\WinGet\\Packages",
//!                   "tmp_dir": "D:\\Media\\Downloads\\.strip-dv", "log": "D:\\Media\\scripts\\logs\\strip-dv.log",
//!                   "seen": "D:\\Media\\scripts\\logs\\strip-dv-seen.txt",
//!                   "libraries": [{"path": "D:\\Media\\Movies", "arr": "radarr", "section": 1},
//!                                 {"path": "D:\\Media\\Seriale", "arr": "sonarr", "section": 2}]},
//!   "seed_keeper": {"downloads_root": "D:\\Media\\Downloads"}
//! }
//! ```

use crate::app::AppState;
use crate::log_warning;
use serde_json::Value;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// `jobs.<job>.<key>` from the config, if set (non-null).
pub fn job_cfg(app: &AppState, job: &str, key: &str) -> Option<Value> {
    app.config.get("jobs").and_then(|j| j.get(job).and_then(|x| x.get(key)).cloned()).filter(|v| !v.is_null())
}

/// A string setting, `default` when missing or empty.
pub fn job_str(app: &AppState, job: &str, key: &str, default: &str) -> String {
    match job_cfg(app, job, key) {
        Some(Value::String(s)) if !s.trim().is_empty() => s,
        _ => default.to_string(),
    }
}

pub fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// A monotonic clock in seconds that does not advance while the PC sleeps or hibernates
/// (`QueryUnbiasedInterruptTime`, 100 ns units). Only differences within one process
/// mean anything.
#[cfg(windows)]
pub fn unbiased_secs() -> f64 {
    #[link(name = "kernel32")]
    extern "system" {
        fn QueryUnbiasedInterruptTime(unbiased_time: *mut u64) -> i32;
    }
    let mut t: u64 = 0;
    // SAFETY: writes one u64 we own.
    unsafe { QueryUnbiasedInterruptTime(&mut t) };
    t as f64 / 10_000_000.0
}

#[cfg(not(windows))]
pub fn unbiased_secs() -> f64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_secs_f64()
}

/// Days since 1970-01-01 of a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `2026-10-01T01:00:00Z` (or with fractions / no time) -> unix seconds, UTC. Offsets other
/// than `Z` are not used by the *arr APIs; anything unparsable is None.
pub fn parse_iso_utc(s: &str) -> Option<i64> {
    let b = s.trim();
    let num = |a: usize, n: usize| b.get(a..a + n).and_then(|x| x.parse::<i64>().ok());
    let (y, mo, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    if b.get(4..5) != Some("-") || b.get(7..8) != Some("-") || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let (h, mi, se) = if b.len() >= 19 { (num(11, 2)?, num(14, 2)?, num(17, 2)?) } else { (0, 0, 0) };
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se)
}

/// Logs a failure once until it changes or clears (the jobs run every few minutes; a
/// service that stays down must not flood the log).
#[derive(Default)]
pub struct Quiet {
    last: HashMap<String, String>,
}

impl Quiet {
    pub fn warn(&mut self, key: &str, msg: String) {
        if self.last.get(key) != Some(&msg) {
            log_warning!("{msg}");
            self.last.insert(key.to_string(), msg);
        }
    }

    /// True if `key` was failing (the caller may log the recovery).
    pub fn clear(&mut self, key: &str) -> bool {
        self.last.remove(key).is_some()
    }
}

/// A usable id: a non-zero JSON integer (the scripts' `-not $q.movieId`).
pub fn id_of(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_i64).filter(|n| *n != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_dates() {
        assert_eq!(parse_iso_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_utc("2026-10-26T12:30:15Z"), Some(1_793_017_815));
        assert_eq!(parse_iso_utc("2026-10-26T12:30:15.123Z"), Some(1_793_017_815));
        assert_eq!(parse_iso_utc("2026-10-26"), Some(1_792_972_800));
        assert_eq!(parse_iso_utc("nope"), None);
        assert_eq!(parse_iso_utc(""), None);
    }

    #[test]
    fn unbiased_clock_moves_forward() {
        let a = unbiased_secs();
        std::thread::sleep(std::time::Duration::from_millis(30));
        let b = unbiased_secs();
        assert!(a > 0.0 && b > a && b - a < 5.0, "{a} {b}");
    }
}
