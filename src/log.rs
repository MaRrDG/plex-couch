//! The `plexcouch` log file (spec 1.5): `<app dir>/plex-couch.log`, rotated at 1,000,000 bytes
//! with 2 backups (`.log.1`, `.log.2`), UTF-8 lines `2026-10-26 18:01:02,123 INFO message`.
//!
//! The level is INFO: [`log_debug!`] lines are dropped. Never log the ntfy topic, the ntfy
//! server URL in failures, API keys or tokens, tracker URLs, the Host header value, config
//! contents or the PIN.
//!
//! Use the macros [`log_info!`], [`log_warning!`], [`log_error!`] and [`log_debug!`]
//! (`format!` syntax). Before [`init_file`] is called (unit tests) lines go nowhere, unless
//! [`start_capture`] was called, which keeps them in memory for assertions.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub const LOG_FILE: &str = "plex-couch.log";
pub const MAX_BYTES: u64 = 1_000_000;
pub const BACKUP_COUNT: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warning,
    Error,
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warning => "WARNING",
            Level::Error => "ERROR",
        }
    }
}

struct Sink {
    path: Option<PathBuf>,
    file: Option<File>,
    capture: Option<Vec<String>>,
}

fn sink() -> &'static Mutex<Sink> {
    static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();
    SINK.get_or_init(|| Mutex::new(Sink { path: None, file: None, capture: None }))
}

/// Open (append) the rotating log file. Errors are returned, the caller decides.
pub fn init_file(path: &Path) -> std::io::Result<()> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut s = sink().lock().unwrap_or_else(|e| e.into_inner());
    s.path = Some(path.to_path_buf());
    s.file = Some(file);
    Ok(())
}

/// Keep every emitted line in memory too (tests: "the topic is never logged").
pub fn start_capture() {
    let mut s = sink().lock().unwrap_or_else(|e| e.into_inner());
    if s.capture.is_none() {
        s.capture = Some(Vec::new());
    }
}

/// Lines captured since [`start_capture`] (all threads; tests run in parallel).
pub fn captured() -> Vec<String> {
    let s = sink().lock().unwrap_or_else(|e| e.into_inner());
    s.capture.clone().unwrap_or_default()
}

/// Write one record. DEBUG is below the logger level and is dropped (an INFO
/// logger), so debug lines never reach the file.
pub fn emit(level: Level, msg: &str) {
    if level < Level::Info {
        return;
    }
    let line = format!("{} {} {}\n", asctime(), level.name(), msg);
    let mut s = sink().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = s.capture.as_mut() {
        c.push(line.clone());
    }
    if s.file.is_none() {
        return;
    }
    if should_rollover(&mut s, line.len() as u64) {
        do_rollover(&mut s);
    }
    if let Some(f) = s.file.as_mut() {
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    }
}

/// Rollover rule: never for an empty file or a non-regular file;
/// otherwise when the current size plus the new record reaches maxBytes.
fn should_rollover(s: &mut Sink, add: u64) -> bool {
    let Some(path) = s.path.as_ref() else { return false };
    if path.exists() && !path.is_file() {
        return false;
    }
    let pos = s.file.as_ref().and_then(|f| f.metadata().ok()).map(|m| m.len()).unwrap_or(0);
    pos != 0 && pos + add >= MAX_BYTES
}

/// `.log.1` -> `.log.2` (replacing it), `.log` -> `.log.1`, then a fresh `.log`.
fn do_rollover(s: &mut Sink) {
    let Some(path) = s.path.clone() else { return };
    s.file = None; // close before renaming (Windows)
    let backup = |i: u32| PathBuf::from(format!("{}.{}", path.display(), i));
    for i in (1..BACKUP_COUNT).rev() {
        let (src, dst) = (backup(i), backup(i + 1));
        if src.exists() {
            if dst.exists() {
                let _ = fs::remove_file(&dst);
            }
            let _ = fs::rename(&src, &dst);
        }
    }
    let first = backup(1);
    if first.exists() {
        let _ = fs::remove_file(&first);
    }
    let _ = fs::rename(&path, &first);
    s.file = OpenOptions::new().create(true).append(true).open(&path).ok();
}

/// The record time: local time `YYYY-MM-DD HH:MM:SS,mmm`.
pub fn asctime() -> String {
    let t = local_time();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02},{:03}",
        t.0, t.1, t.2, t.3, t.4, t.5, t.6
    )
}

#[cfg(windows)]
fn local_time() -> (u16, u16, u16, u16, u16, u16, u16) {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    // SAFETY: GetLocalTime only writes the struct we pass.
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    (st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond, st.wMilliseconds)
}

#[cfg(not(windows))]
fn local_time() -> (u16, u16, u16, u16, u16, u16, u16) {
    // UTC fallback (the app only runs on Windows).
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs() as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // civil_from_days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    (
        year as u16,
        month as u16,
        day as u16,
        (rem / 3600) as u16,
        (rem % 3600 / 60) as u16,
        (rem % 60) as u16,
        d.subsec_millis() as u16,
    )
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::Level::Info, &format!($($arg)*)) };
}

#[macro_export]
macro_rules! log_warning {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::Level::Warning, &format!($($arg)*)) };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::Level::Error, &format!($($arg)*)) };
}

/// Dropped (the logger level is INFO), kept so the code documents what is logged at DEBUG.
#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {{
        if false {
            let _ = format!($($arg)*);
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asctime_shape() {
        let a = asctime();
        assert_eq!(a.len(), 23, "{a}");
        assert_eq!(&a[4..5], "-");
        assert_eq!(&a[19..20], ",");
    }

    #[test]
    fn debug_is_dropped_and_capture_works() {
        start_capture();
        log_debug!("debug-line-{}", 1);
        log_info!("info-line-{}", 42);
        let lines = captured();
        assert!(lines.iter().any(|l| l.contains(" INFO info-line-42\n")));
        assert!(!lines.iter().any(|l| l.contains("debug-line")));
    }
}
