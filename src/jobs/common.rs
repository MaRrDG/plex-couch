//! Helpers shared by the Phase 2b jobs: the optional `jobs` config section, clocks, ISO
//! dates, quiet error logging, and for the ffmpeg jobs (strip_dv, convert) finding ffmpeg,
//! running it quietly, free space and file identity (hard links).
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
//!   "seed_keeper": {"downloads_root": "D:\\Media\\Downloads"},
//!   "convert":     {"ffmpeg_bin": "", "tmp_dir": "D:\\Media\\Downloads\\.reencode", "section": 2}
//! }
//! ```

use crate::app::AppState;
use crate::log_warning;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

// -- ffmpeg and files (strip_dv, convert) -------------------------------------------------

/// Where winget unpacks portable packages such as Gyan.FFmpeg for the current user.
pub fn default_winget_packages() -> String {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_default();
    format!("{base}\\Microsoft\\WinGet\\Packages")
}

/// The newest `<packages>\Gyan.FFmpeg*\ffmpeg-*\bin` (full path sorted descending).
pub fn find_ffmpeg_bin(packages: &Path) -> Option<PathBuf> {
    let starts = |p: &Path, pre: &str| {
        p.file_name().is_some_and(|n| n.to_string_lossy().to_lowercase().starts_with(&pre.to_lowercase()))
    };
    let mut found = Vec::new();
    for pkg in fs::read_dir(packages).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_dir() && starts(p, "Gyan.FFmpeg")) {
        let Ok(rd) = fs::read_dir(&pkg) else { continue };
        for d in rd.flatten().map(|e| e.path()).filter(|p| p.is_dir() && starts(p, "ffmpeg-")) {
            let bin = d.join("bin");
            if bin.is_dir() {
                found.push(bin);
            }
        }
    }
    found.sort_by_key(|p| std::cmp::Reverse(p.to_string_lossy().to_lowercase()));
    found.into_iter().next()
}

/// `bin` if set, else the newest winget ffmpeg under `packages`; only a folder that has both
/// `ffmpeg.exe` and `ffprobe.exe`.
pub fn ffmpeg_bin(bin: &str, packages: &str) -> Option<PathBuf> {
    let b = if bin.is_empty() { find_ffmpeg_bin(Path::new(packages)) } else { Some(PathBuf::from(bin)) };
    b.filter(|b| b.join("ffmpeg.exe").is_file() && b.join("ffprobe.exe").is_file())
}

/// A command without a console window, at below-normal priority (the user games on this PC).
#[cfg(windows)]
pub fn quiet_command(exe: &Path) -> Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
    let mut c = Command::new(exe);
    c.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
    c
}

#[cfg(not(windows))]
pub fn quiet_command(exe: &Path) -> Command {
    Command::new(exe)
}

fn read_all(r: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(mut s) = r {
            let _ = s.read_to_end(&mut b);
        }
        b
    })
}

/// Run to completion (or `timeout` / `stop`), collecting stdout and stderr.
pub fn run_capture(cmd: Command, timeout: Duration, stop: &dyn Fn() -> bool) -> Result<(i32, Vec<u8>, Vec<u8>), String> {
    let mut out = Vec::new();
    let (code, err) = run_lines(cmd, timeout, stop, &mut |l: &str| {
        out.extend_from_slice(l.as_bytes());
        out.push(b'\n');
    })?;
    Ok((code, out, err))
}

/// Like [`run_capture`], but stdout goes to `on_line` line by line while the program runs
/// (ffmpeg `-progress pipe:1`). Returns the exit code and stderr. `stop` or the timeout
/// kills the program.
pub fn run_lines(mut cmd: Command, timeout: Duration, stop: &dyn Fn() -> bool, on_line: &mut dyn FnMut(&str)) -> Result<(i32, Vec<u8>), String> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("cannot start: {e}"))?;
    let so = child.stdout.take();
    let t_err = read_all(child.stderr.take());
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let t_out = std::thread::spawn(move || {
        let Some(s) = so else { return };
        let mut r = BufReader::new(s);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match r.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf).trim_end().to_string();
                    let _ = tx.send(line); // keep reading even if nobody listens: never block the child
                }
            }
        }
    });
    let started = Instant::now();
    let code = loop {
        while let Ok(l) = rx.try_recv() {
            on_line(&l);
        }
        match child.try_wait() {
            Ok(Some(st)) => break st.code().unwrap_or(-1),
            Ok(None) => {}
            Err(e) => return Err(format!("wait failed: {e}")),
        }
        if stop() || started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = (t_out.join(), t_err.join());
            return Err(if stop() { "oprit".into() } else { "timeout".into() });
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let _ = t_out.join();
    while let Ok(l) = rx.try_recv() {
        on_line(&l);
    }
    Ok((code, t_err.join().unwrap_or_default()))
}

/// Same drive / UNC share (a rename cannot cross volumes).
pub fn same_volume(a: &Path, b: &Path) -> bool {
    let root = |p: &Path| match p.components().next() {
        Some(std::path::Component::Prefix(x)) => Some(x.as_os_str().to_string_lossy().to_lowercase()),
        _ => None,
    };
    root(a).is_some() && root(a) == root(b)
}

/// Free bytes on the file's drive (`Get-PSDrive <letter>`). None if unknown.
#[cfg(windows)]
pub fn free_bytes(file: &Path) -> Option<u64> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let s = file.to_string_lossy();
    let root = format!("{}:\\", s.chars().next()?);
    let w: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
    let mut free: u64 = 0;
    // SAFETY: valid NUL-terminated path, out pointers to locals.
    let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
    (ok != 0).then_some(free)
}

#[cfg(not(windows))]
pub fn free_bytes(_file: &Path) -> Option<u64> {
    None
}

/// Which file on disk a path is: NTFS volume serial + file index (hard links of one file
/// share both), and its link count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileId {
    pub volume: u64,
    pub index: u64,
    pub links: u32,
}

impl FileId {
    /// The same file (another hard link to it counts).
    pub fn same_file(&self, other: &FileId) -> bool {
        self.volume == other.volume && self.index == other.index
    }
}

/// `GetFileInformationByHandle`: volume serial, file index, `nNumberOfLinks`.
#[cfg(windows)]
pub fn file_id(path: &Path) -> std::io::Result<FileId> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
    let f = fs::File::open(path)?;
    // SAFETY: a valid open handle and a zeroed out struct for the call to fill.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(FileId {
        volume: info.dwVolumeSerialNumber as u64,
        index: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
        links: info.nNumberOfLinks,
    })
}

#[cfg(not(windows))]
pub fn file_id(path: &Path) -> std::io::Result<FileId> {
    use std::os::unix::fs::MetadataExt;
    let m = fs::metadata(path)?;
    Ok(FileId { volume: m.dev(), index: m.ino(), links: m.nlink() as u32 })
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
