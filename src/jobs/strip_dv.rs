//! Strip Dolby Vision, every 5 minutes, first run 60 s after start (replaces
//! `strip-dv.ps1 -All`; plan "New behaviour" 2, spec 12.2; plan fix 23: `.mkv` only).
//!
//! The same checks, ffmpeg command, temp dir, seen file (`<FullName>|<Length>|<Ticks>`) and
//! log file (`yyyy-MM-dd HH:mm:ss  <msg>`) as the script, now over `D:\Media\Movies` and
//! `D:\Media\Seriale`. One remux at a time: the job runs on its own OS thread (it never
//! blocks the other jobs, even while it waits up to an hour for a file Plex is playing) and
//! holds the script's `Global\PlexStripDV` mutex while it works, so it never overlaps a
//! still-scheduled `strip-dv.ps1`. After a replacement: Radarr `RescanMovie` / Sonarr
//! `RescanSeries` for the owning item and a Plex refresh of the folder in section 1 / 2
//! (token in the header, plan fix 24). ffmpeg/ffprobe run without a window at below-normal
//! priority.
//!
//! Safety: the verified remux replaces the original with one rename (replace-existing;
//! across volumes via `<file>.partial` next to it), retried while Plex holds the file,
//! and only while the original still has the stamp it had before probing. Files whose
//! remux failed are skipped for 24 h (`strip_dv_failed` in the state file); temp files
//! older than 24 h are removed each pass.
//!
//! Dry-run: files are probed (read-only), nothing is remuxed, the seen file and the
//! strip-dv log are not written; the would-be actions go to the app log.

use crate::app::AppState;
use crate::jobs::common::{
    default_winget_packages, find_ffmpeg_bin, free_bytes, id_of, job_cfg, job_str, quiet_command, run_capture, same_volume, unix_now,
};
use crate::jobs::Stop;
use crate::services::arr_jobs::Arr;
use crate::services::plex_jobs::{path_key, PlexJobs};
use crate::{log_info, log_warning};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};
use tokio::runtime::Handle;

pub const INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const FIRST_RUN: Duration = Duration::from_secs(60);
pub const MUTEX_NAME: &str = "Global\\PlexStripDV";
const PROBE_TIMEOUT: Duration = Duration::from_secs(120);
const REMUX_TIMEOUT: Duration = Duration::from_secs(4 * 3600);
/// Replacing the original: 60 tries, 60 s apart (Plex may be playing it).
const DELETE_TRIES: u32 = 60;
const DELETE_WAIT: Duration = Duration::from_secs(60);
/// A file that failed (ffmpeg, verification, locked) is not retried for this long.
pub const FAILED_RETRY_SECONDS: f64 = 24.0 * 3600.0;
/// Leftover temp files older than this are deleted.
pub const ORPHAN_AGE_SECONDS: f64 = 24.0 * 3600.0;
/// `state.local.json` key: `{"<stamp>": <unix time of the failure>}`.
pub const FAILED_KEY: &str = "strip_dv_failed";

/// What one file's run came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// No DV (any more) or nothing doable: recorded in the seen file.
    Fine,
    /// Try again next pass (missing, no space, changed meanwhile, stopping, dry-run).
    NotNow,
    /// ffmpeg, verification or replacement failed: skipped for 24 h.
    Failed,
}

/// The failed stamps from the state file (unreadable state: none).
pub fn load_failed(app: &AppState) -> HashMap<String, f64> {
    app.state_file
        .try_load()
        .ok()
        .and_then(|m| m.get(FAILED_KEY).and_then(Value::as_object).cloned())
        .map(|m| m.into_iter().filter_map(|(k, v)| Some((k, v.as_f64()?))).collect())
        .unwrap_or_default()
}

/// Save the failed stamps, dropping the ones older than 24 h.
pub fn save_failed(app: &AppState, failed: &HashMap<String, f64>, now: f64) {
    let m: serde_json::Map<String, Value> =
        failed.iter().filter(|(_, t)| now - **t < FAILED_RETRY_SECONDS).map(|(k, t)| (k.clone(), json!(t))).collect();
    let mut c = serde_json::Map::new();
    c.insert(FAILED_KEY.into(), Value::Object(m));
    app.state_file.update(c);
}

/// .NET `DateTime` ticks at the Unix epoch.
const TICKS_AT_EPOCH: u128 = 621_355_968_000_000_000;

/// One library root: which *arr owns it and its Plex section.
#[derive(Debug, Clone, PartialEq)]
pub struct Library {
    pub root: String,
    pub arr: String,
    pub section: u32,
}

#[derive(Debug, Clone)]
pub struct DvConfig {
    pub libraries: Vec<Library>,
    pub tmp_dir: PathBuf,
    pub log: PathBuf,
    pub seen: PathBuf,
    pub ffmpeg_bin: String,
    pub ffmpeg_packages: String,
    /// Waits between replace tries of the original (tests shorten it).
    pub delete_wait: Duration,
    /// Tests: take the cross-volume path (copy to `<file>.partial`) on one volume.
    pub force_cross_volume: bool,
}

impl DvConfig {
    pub fn from_app(app: &AppState) -> DvConfig {
        let s = |k: &str, d: &str| job_str(app, "strip_dv", k, d);
        let libraries = match job_cfg(app, "strip_dv", "libraries") {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|l| {
                    Some(Library {
                        root: l.get("path")?.as_str()?.to_string(),
                        arr: l.get("arr")?.as_str()?.to_string(),
                        section: l.get("section")?.as_u64()? as u32,
                    })
                })
                .collect(),
            _ => vec![
                Library { root: "D:\\Media\\Movies".into(), arr: "radarr".into(), section: 1 },
                Library { root: "D:\\Media\\Seriale".into(), arr: "sonarr".into(), section: 2 },
            ],
        };
        DvConfig {
            libraries,
            tmp_dir: s("tmp_dir", "D:\\Media\\Downloads\\.strip-dv").into(),
            log: s("log", "D:\\Media\\scripts\\logs\\strip-dv.log").into(),
            seen: s("seen", "D:\\Media\\scripts\\logs\\strip-dv-seen.txt").into(),
            ffmpeg_bin: s("ffmpeg_bin", ""),
            ffmpeg_packages: s("ffmpeg_packages", &default_winget_packages()),
            delete_wait: DELETE_WAIT,
            force_cross_volume: false,
        }
    }
}

// -- probing and decisions (pure) ---------------------------------------------------------

fn first_video(probe: &Value) -> Option<&Value> {
    probe.get("streams")?.as_array()?.iter().find(|s| s.get("codec_type").and_then(Value::as_str) == Some("video"))
}

/// The first video stream carries a "DOVI configuration record".
pub fn has_dv(probe: &Value) -> bool {
    first_video(probe)
        .and_then(|v| v.get("side_data_list"))
        .and_then(Value::as_array)
        .is_some_and(|l| l.iter().any(|d| d.get("side_data_type").and_then(Value::as_str) == Some("DOVI configuration record")))
}

/// HDR10 base layer (`color_transfer == smpte2084`): DV can be stripped.
pub fn has_hdr10(probe: &Value) -> bool {
    first_video(probe).and_then(|v| v.get("color_transfer")).and_then(Value::as_str) == Some("smpte2084")
}

pub fn duration(probe: &Value) -> f64 {
    match probe.get("format").and_then(|f| f.get("duration")) {
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        Some(v) => v.as_f64().unwrap_or(0.0),
        None => 0.0,
    }
}

const TEXT_SUBS: [&str; 6] = ["subrip", "ass", "ssa", "mov_text", "webvtt", "text"];
const SUB_LANGS: [&str; 5] = ["eng", "ron", "rum", "und", ""];

/// `-map 0:<first video>`, every audio stream, text subtitles in eng/ron/rum/und/none.
pub fn stream_maps(probe: &Value) -> Vec<String> {
    let mut maps = Vec::new();
    let idx = |s: &Value| s.get("index").and_then(Value::as_i64).unwrap_or(0);
    if let Some(v) = first_video(probe) {
        maps.extend(["-map".to_string(), format!("0:{}", idx(v))]);
    }
    for s in probe.get("streams").and_then(Value::as_array).into_iter().flatten() {
        let ty = s.get("codec_type").and_then(Value::as_str).unwrap_or("");
        let codec = s.get("codec_name").and_then(Value::as_str).unwrap_or("").to_lowercase();
        let lang = s.get("tags").and_then(|t| t.get("language")).and_then(Value::as_str).unwrap_or("").to_lowercase();
        let take = ty == "audio" || (ty == "subtitle" && TEXT_SUBS.contains(&codec.as_str()) && SUB_LANGS.contains(&lang.as_str()));
        if take {
            maps.extend(["-map".to_string(), format!("0:{}", idx(s))]);
        }
    }
    maps
}

/// The full ffmpeg argument list of the script.
pub fn ffmpeg_args(input: &Path, maps: &[String], tmp: &Path) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-nostdin", "-loglevel", "error", "-y", "-i"].map(String::from).to_vec();
    a.push(input.to_string_lossy().into_owned());
    a.extend(maps.iter().cloned());
    a.extend(["-map_chapters", "0", "-c", "copy", "-bsf:v", "dovi_rpu=strip=1"].map(String::from));
    a.push(tmp.to_string_lossy().into_owned());
    a
}

/// `<basename without extension>.nodv.mkv` in the temp dir.
pub fn tmp_path(tmp_dir: &Path, file: &Path) -> PathBuf {
    let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    tmp_dir.join(format!("{stem}.nodv.mkv"))
}

/// `<FullName>|<Length>|<LastWriteTimeUtc.Ticks>`.
pub fn stamp(path: &Path) -> io::Result<String> {
    let m = fs::metadata(path)?;
    let since = m.modified()?.duration_since(UNIX_EPOCH).unwrap_or_default();
    let ticks = TICKS_AT_EPOCH + since.as_nanos() / 100;
    Ok(format!("{}|{}|{}", path.display(), m.len(), ticks))
}

/// The seen file as a set (UTF-8, optional BOM, CRLF or LF).
pub fn load_seen(path: &Path) -> HashSet<String> {
    let Ok(b) = fs::read(path) else { return HashSet::new() };
    let b = b.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&b);
    String::from_utf8_lossy(b).lines().map(|l| l.trim_end_matches('\r').to_string()).filter(|l| !l.is_empty()).collect()
}

/// Append one line (CRLF, like `Add-Content`).
fn append_line(path: &Path, line: &str) -> io::Result<()> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d)?;
    }
    let mut f = fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(format!("{line}\r\n").as_bytes())
}

/// Every `.mkv` under `root`, recursively (case-insensitive extension).
pub fn mkv_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(d) = dirs.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        let mut sub = Vec::new();
        for e in entries {
            let Ok(ft) = e.file_type() else { continue };
            let p = e.path();
            if ft.is_dir() {
                sub.push(p);
            } else if ft.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("mkv")) {
                out.push(p);
            }
        }
        dirs.extend(sub.into_iter().rev());
    }
    out
}

// -- tools --------------------------------------------------------------------------------

/// The external programs, behind a trait so tests can fake ffmpeg/ffprobe.
pub trait Tools: Send + Sync {
    /// `ffprobe -v error -show_format -show_streams -of json <file>`.
    fn probe(&self, file: &Path) -> Result<Value, String>;
    /// Run ffmpeg with `args`: `(exit code, output lines)`. Returns early (killing ffmpeg)
    /// when `stop` says so.
    fn ffmpeg(&self, args: &[String], stop: &dyn Fn() -> bool) -> Result<(i32, Vec<String>), String>;
}

/// ffmpeg.exe and ffprobe.exe in `bin` (also the encoder of [`crate::jobs::convert`]).
pub struct Ffmpeg {
    pub bin: PathBuf,
}

impl Tools for Ffmpeg {
    fn probe(&self, file: &Path) -> Result<Value, String> {
        let mut c = quiet_command(&self.bin.join("ffprobe.exe"));
        c.args(["-v", "error", "-show_format", "-show_streams", "-of", "json"]).arg(file);
        let (_code, out, _err) = run_capture(c, PROBE_TIMEOUT, &|| false)?;
        serde_json::from_slice(&out).map_err(|_| "ffprobe: raspuns JSON invalid".to_string())
    }

    fn ffmpeg(&self, args: &[String], stop: &dyn Fn() -> bool) -> Result<(i32, Vec<String>), String> {
        let mut c = quiet_command(&self.bin.join("ffmpeg.exe"));
        c.args(args);
        let (code, out, err) = run_capture(c, REMUX_TIMEOUT, stop)?;
        let mut lines: Vec<String> = Vec::new();
        for b in [out, err] {
            lines.extend(String::from_utf8_lossy(&b).lines().map(|l| l.trim_end().to_string()).filter(|l| !l.is_empty()));
        }
        Ok((code, lines))
    }
}

// -- the worker ---------------------------------------------------------------------------

/// The strip-dv log: the script's file (real runs), always mirrored to the app log.
pub struct DvLog {
    pub path: PathBuf,
    pub dry_run: bool,
}

impl DvLog {
    pub fn line(&self, msg: &str) {
        if self.dry_run {
            log_info!("strip-dv (dry-run): {msg}");
            return;
        }
        log_info!("strip-dv: {msg}");
        let ts = local_ts();
        if let Err(e) = append_line(&self.path, &format!("{ts}  {msg}")) {
            log_warning!("strip-dv: cannot write its log: {}", crate::state::io_error_name(&e));
        }
    }
}

/// `yyyy-MM-dd HH:mm:ss` local time (the app log's asctime without the millis).
fn local_ts() -> String {
    crate::log::asctime()[..19].to_string()
}

/// `{0:N1}` / `[math]::Round(x, 1)` with a dot.
fn one_decimal(x: f64) -> String {
    format!("{:.1}", crate::json::py_round(x, 1))
}

pub struct Worker<'a> {
    pub app: &'a AppState,
    pub rt: Handle,
    pub cfg: DvConfig,
    pub tools: &'a dyn Tools,
    pub log: DvLog,
    pub stop: &'a Stop,
}

impl Worker<'_> {
    fn stopped(&self) -> bool {
        self.stop.is_stopped()
    }

    fn library_of(&self, file: &Path) -> Option<&Library> {
        let f = path_key(&file.to_string_lossy());
        self.cfg.libraries.iter().find(|l| f.starts_with(&format!("{}\\", path_key(&l.root))))
    }

    /// `-All`: every `.mkv` not in the seen file and not failed in the last 24 h. Returns
    /// the number of files looked at.
    pub fn run_all(&self) -> usize {
        self.clean_orphans();
        let seen = load_seen(&self.cfg.seen);
        let now = unix_now();
        let mut failed = load_failed(self.app);
        let mut n = 0;
        for lib in &self.cfg.libraries {
            for f in mkv_files(Path::new(&lib.root)) {
                if self.stopped() {
                    return n;
                }
                let Ok(st) = stamp(&f) else { continue };
                if seen.contains(&st) {
                    continue;
                }
                if failed.get(&st).is_some_and(|t| now - t < FAILED_RETRY_SECONDS) {
                    continue;
                }
                n += 1;
                let outcome = match self.strip(&f, &st) {
                    Ok(o) => o,
                    Err(e) => {
                        self.log.line(&format!("EROARE: {}: {e}", f.display()));
                        Outcome::Failed
                    }
                };
                match outcome {
                    Outcome::Fine if !self.log.dry_run => {
                        if let Err(e) = stamp(&f).and_then(|s| append_line(&self.cfg.seen, &s)) {
                            log_warning!("strip-dv: cannot write the seen file: {}", crate::state::io_error_name(&e));
                        }
                    }
                    Outcome::Failed if !self.log.dry_run => {
                        failed.insert(st.clone(), unix_now());
                        save_failed(self.app, &failed, unix_now());
                        log_info!("strip-dv: {} failed, skipped for 24 h", f.display());
                    }
                    _ => {}
                }
            }
        }
        n
    }

    /// Delete `*.nodv.mkv` leftovers in the temp dir older than 24 h. A file still open
    /// (an ffmpeg of the old script) cannot be deleted on Windows and is left alone.
    pub fn clean_orphans(&self) {
        let Ok(rd) = fs::read_dir(&self.cfg.tmp_dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_lowercase();
            if !name.ends_with(".nodv.mkv") || !p.is_file() {
                continue;
            }
            let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age.as_secs_f64() > ORPHAN_AGE_SECONDS);
            if !old {
                continue;
            }
            if self.log.dry_run {
                self.log.line(&format!("would delete old temp file: {}", p.display()));
            } else if fs::remove_file(&p).is_ok() {
                self.log.line(&format!("sters fisier temporar vechi: {}", p.display()));
            }
        }
    }

    /// `Invoke-StripDV`. `before` is the original's stamp taken before probing: the file is
    /// only replaced while it still matches.
    pub fn strip(&self, file: &Path, before: &str) -> Result<Outcome, String> {
        let leaf = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if !file.exists() {
            self.log.line(&format!("lipsa: {}", file.display()));
            return Ok(Outcome::NotNow);
        }
        let probe = self.tools.probe(file)?;
        if !has_dv(&probe) {
            self.log.line(&format!("fara DV: {leaf}"));
            return Ok(Outcome::Fine);
        }
        if !has_hdr10(&probe) {
            self.log.line(&format!("DV fara HDR10 (nu pot scoate DV): {}", file.display()));
            return Ok(Outcome::Fine);
        }
        let size = fs::metadata(file).map_err(|e| e.to_string())?.len();
        if let Some(free) = free_bytes(file) {
            if (free as f64) < size as f64 * 1.1 {
                self.log.line(&format!("EROARE: spatiu insuficient pentru {}", file.display()));
                return Ok(Outcome::NotNow);
            }
        }
        let maps = stream_maps(&probe);
        let tmp = tmp_path(&self.cfg.tmp_dir, file);
        let gb = one_decimal(size as f64 / 1_073_741_824.0);
        if self.log.dry_run {
            self.log.line(&format!("would remux: {leaf} ({gb} GB) -> {} with {}", tmp.display(), maps.join(" ")));
            return Ok(Outcome::NotNow);
        }
        fs::create_dir_all(&self.cfg.tmp_dir).map_err(|e| e.to_string())?;
        self.log.line(&format!("remux start: {leaf} ({gb} GB)"));
        let t0 = Instant::now();
        let r = self.tools.ffmpeg(&ffmpeg_args(file, &maps, &tmp), &|| self.stopped());
        let (code, lines) = match r {
            Ok(x) => x,
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                self.log.line(&format!("EROARE: ffmpeg a esuat ({e}), originalul ramane"));
                return Ok(if self.stopped() { Outcome::NotNow } else { Outcome::Failed });
            }
        };
        for l in &lines {
            self.log.line(&format!("  ffmpeg: {l}"));
        }
        if code != 0 || !tmp.exists() {
            self.log.line(&format!("EROARE: ffmpeg a esuat ({code}), originalul ramane"));
            let _ = fs::remove_file(&tmp);
            return Ok(Outcome::Failed);
        }
        let out = self.tools.probe(&tmp)?;
        let (din, dout) = (duration(&probe), duration(&out));
        if has_dv(&out) || (din - dout).abs() > 2.0 {
            self.log.line(&format!("EROARE: verificare esuata (DV inca prezent sau durata {dout} vs {din}), originalul ramane"));
            let _ = fs::remove_file(&tmp);
            return Ok(Outcome::Failed);
        }
        let o = self.install(&tmp, file, before);
        if o != Outcome::Fine {
            return Ok(o);
        }
        self.log.line(&format!("gata in {} min: {leaf}", one_decimal(t0.elapsed().as_secs_f64() / 60.0)));
        if let Err(e) = self.update_apps(file) {
            self.log.line(&format!("  atentie: refresh Radarr/Plex esuat: {e}"));
        }
        Ok(Outcome::Fine)
    }

    /// Put the verified `tmp` in place of `file` with one rename (replace existing). The
    /// library file is a hardlink to the torrent, so replacing it leaves the seed alone.
    /// Across volumes `tmp` is first copied to `<file>.partial` next to it (size checked).
    /// Plex may hold the file: retry for up to an hour. Before each try the original must
    /// still match `before`, else nothing is replaced.
    fn install(&self, tmp: &Path, file: &Path, before: &str) -> Outcome {
        let cross = self.cfg.force_cross_volume || !same_volume(tmp, file);
        let src = if cross {
            let mut name = file.file_name().unwrap_or_default().to_os_string();
            name.push(".partial");
            let partial = file.with_file_name(name);
            let want = fs::metadata(tmp).map(|m| m.len()).ok();
            let copied = fs::copy(tmp, &partial).ok();
            if want.is_none() || copied != want || fs::metadata(&partial).map(|m| m.len()).ok() != want {
                let _ = fs::remove_file(&partial);
                self.log.line(&format!("EROARE: copierea pe discul bibliotecii a esuat, noul fisier ramane in {}", tmp.display()));
                return Outcome::Failed;
            }
            partial
        } else {
            tmp.to_path_buf()
        };
        let drop_src = || {
            if cross {
                let _ = fs::remove_file(&src);
            }
        };
        for i in 0..DELETE_TRIES {
            if stamp(file).ok().as_deref() != Some(before) {
                drop_src();
                let _ = fs::remove_file(tmp);
                self.log.line(&format!("originalul s-a schimbat in timpul remux-ului, il reiau data viitoare: {}", file.display()));
                return Outcome::NotNow;
            }
            if fs::rename(&src, file).is_ok() {
                if cross {
                    let _ = fs::remove_file(tmp);
                }
                return Outcome::Fine;
            }
            if i + 1 < DELETE_TRIES && self.rt.block_on(self.stop.clone().sleep(self.cfg.delete_wait)) {
                break;
            }
        }
        drop_src();
        self.log.line(&format!("EROARE: originalul e blocat, noul fisier ramane in {}", tmp.display()));
        if self.stopped() {
            Outcome::NotNow
        } else {
            Outcome::Failed
        }
    }

    /// `Update-Apps`: rescan the owning movie/series, refresh the folder in Plex.
    pub fn update_apps(&self, file: &Path) -> Result<(), String> {
        let Some(lib) = self.library_of(file).cloned() else { return Ok(()) };
        let dir = file.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
        let app = self.app;
        let dir_key = path_key(&dir);
        self.rt.block_on(async {
            let sonarr = lib.arr == "sonarr";
            let arr = if sonarr { app.services.sonarr.arr() } else { Arr::radarr(app.services.radarr.c.clone()) };
            let items = arr.items().await.map_err(|e| e.to_string())?;
            let owner = items.iter().find(|m| {
                let p = path_key(m.get("path").and_then(Value::as_str).unwrap_or(""));
                !p.is_empty() && (p == dir_key || (sonarr && dir_key.starts_with(&format!("{p}\\"))))
            });
            if let Some(id) = owner.and_then(|m| id_of(m.get("id"))) {
                let body = if sonarr {
                    json!({"name": "RescanSeries", "seriesId": id})
                } else {
                    json!({"name": "RescanMovie", "movieId": id})
                };
                arr.command(&body).await.map_err(|e| e.to_string())?;
                log_info!("strip-dv: {} rescan of {id} requested", arr.label());
            }
            let plex = PlexJobs::new(app.services.plex.c.clone());
            if app.services.keys.get("plex").is_empty() {
                self.log.line("  atentie: fara token Plex, biblioteca se actualizeaza la urmatoarea scanare");
                return Ok(());
            }
            plex.refresh_path(lib.section, &dir).await.map_err(|e| e.to_string())
        })
    }
}

/// The script's global mutex, held for one run on this thread.
struct NamedMutex(#[cfg(windows)] windows_sys::Win32::Foundation::HANDLE);

impl NamedMutex {
    /// None while another process (a still-scheduled `strip-dv.ps1`) holds it.
    #[cfg(windows)]
    fn try_lock(name: &str) -> Option<NamedMutex> {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
        let w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: plain Win32 calls with a valid NUL-terminated name.
        unsafe {
            let h = CreateMutexW(std::ptr::null(), 0, w.as_ptr());
            if h.is_null() {
                return None;
            }
            let r = WaitForSingleObject(h, 0);
            if r == WAIT_OBJECT_0 || r == WAIT_ABANDONED {
                Some(NamedMutex(h))
            } else {
                CloseHandle(h);
                None
            }
        }
    }

    #[cfg(not(windows))]
    fn try_lock(_name: &str) -> Option<NamedMutex> {
        Some(NamedMutex())
    }
}

#[cfg(windows)]
impl Drop for NamedMutex {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::ReleaseMutex;
        // SAFETY: we own the handle and the mutex on this thread.
        unsafe {
            ReleaseMutex(self.0);
            CloseHandle(self.0);
        }
    }
}

/// One `-All` pass with the real tools. Logs once while ffmpeg is missing.
fn pass(app: &AppState, rt: &Handle, stop: &Stop, warned_missing: &mut bool) {
    let cfg = DvConfig::from_app(app);
    let log = DvLog { path: cfg.log.clone(), dry_run: app.dry_run };
    let bin = if cfg.ffmpeg_bin.is_empty() { find_ffmpeg_bin(Path::new(&cfg.ffmpeg_packages)) } else { Some(cfg.ffmpeg_bin.clone().into()) };
    let Some(bin) = bin.filter(|b| b.join("ffmpeg.exe").is_file() && b.join("ffprobe.exe").is_file()) else {
        if !*warned_missing {
            log.line("EROARE: ffmpeg nu e instalat (winget Gyan.FFmpeg)");
            *warned_missing = true;
        }
        return;
    };
    *warned_missing = false;
    let Some(_lock) = NamedMutex::try_lock(MUTEX_NAME) else {
        log_info!("strip-dv: another strip-dv run holds {MUTEX_NAME}, next try in 5 min");
        return;
    };
    let tools = Ffmpeg { bin };
    let w = Worker { app, rt: rt.clone(), cfg, tools: &tools, log, stop };
    let n = w.run_all();
    if app.dry_run {
        log_info!("strip-dv (dry-run): pass done, {n} file(s) not in the seen file");
    }
}

/// Start the loop on its own thread.
pub fn spawn(app: Arc<AppState>, stop: Stop) {
    let rt = Handle::current();
    let r = std::thread::Builder::new().name("strip-dv".into()).spawn(move || {
        let mut s = stop.clone();
        if rt.block_on(s.sleep(FIRST_RUN)) {
            return;
        }
        let mut warned = false;
        loop {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pass(&app, &rt, &stop, &mut warned)));
            if r.is_err() {
                log_warning!("strip-dv: pass failed (panic)");
            }
            if rt.block_on(s.sleep(INTERVAL)) {
                return;
            }
        }
    });
    if r.is_err() {
        log_warning!("strip-dv: cannot start its thread");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::testutil::{app, fake, ok, temp_dir, Req};
    use std::sync::Mutex;

    fn probe(dv: bool, transfer: &str, dur: &str) -> Value {
        let mut video = json!({"index": 0, "codec_type": "video", "codec_name": "hevc", "color_transfer": transfer});
        if dv {
            video["side_data_list"] = json!([{"side_data_type": "DOVI configuration record", "dv_profile": 8}]);
        }
        json!({"streams": [
            video,
            {"index": 1, "codec_type": "audio", "codec_name": "truehd", "tags": {"language": "eng"}},
            {"index": 2, "codec_type": "audio", "codec_name": "ac3", "tags": {"language": "rum"}},
            {"index": 3, "codec_type": "subtitle", "codec_name": "hdmv_pgs_subtitle", "tags": {"language": "eng"}},
            {"index": 4, "codec_type": "subtitle", "codec_name": "subrip", "tags": {"language": "ENG"}},
            {"index": 5, "codec_type": "subtitle", "codec_name": "subrip", "tags": {"language": "fre"}},
            {"index": 6, "codec_type": "subtitle", "codec_name": "ass"},
            {"index": 7, "codec_type": "video", "codec_name": "mjpeg"}
        ], "format": {"duration": dur}})
    }

    #[test]
    fn decisions() {
        assert!(has_dv(&probe(true, "smpte2084", "10")));
        assert!(!has_dv(&probe(false, "smpte2084", "10")));
        assert!(has_hdr10(&probe(true, "smpte2084", "10")));
        assert!(!has_hdr10(&probe(true, "bt709", "10")));
        assert_eq!(duration(&probe(true, "x", "7845.312000")), 7845.312);
        // DV only on a later video stream does not count (the script looks at the first).
        let p = json!({"streams": [{"codec_type": "video"}, {"codec_type": "video", "side_data_list": [{"side_data_type": "DOVI configuration record"}]}]});
        assert!(!has_dv(&p));
        assert_eq!(stream_maps(&probe(true, "smpte2084", "1")).join(" "), "-map 0:0 -map 0:1 -map 0:2 -map 0:4 -map 0:6");
        let a = ffmpeg_args(Path::new("D:\\M\\a.mkv"), &["-map".into(), "0:0".into()], Path::new("D:\\T\\a.nodv.mkv"));
        assert_eq!(
            a.join(" "),
            "-hide_banner -nostdin -loglevel error -y -i D:\\M\\a.mkv -map 0:0 -map_chapters 0 -c copy -bsf:v dovi_rpu=strip=1 D:\\T\\a.nodv.mkv"
        );
        assert_eq!(tmp_path(Path::new("D:\\T"), Path::new("D:\\M\\x.y.mkv")), PathBuf::from("D:\\T\\x.y.nodv.mkv"));
    }

    #[test]
    fn seen_file_format() {
        let d = temp_dir("dv-seen");
        let f = d.join("Film (2020).mkv");
        fs::write(&f, b"12345").unwrap();
        let s = stamp(&f).unwrap();
        let parts: Vec<&str> = s.split('|').collect();
        assert_eq!(parts[0], f.display().to_string());
        assert_eq!(parts[1], "5");
        let ticks: u128 = parts[2].parse().unwrap();
        let secs = fs::metadata(&f).unwrap().modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs() as u128;
        assert_eq!((ticks - TICKS_AT_EPOCH) / 10_000_000, secs);
        // The script's file: BOM, CRLF.
        let seen = d.join("seen.txt");
        fs::write(&seen, format!("\u{feff}{s}\r\nD:\\x.mkv|1|2\r\n")).unwrap();
        let set = load_seen(&seen);
        assert!(set.contains(&s) && set.contains("D:\\x.mkv|1|2") && set.len() == 2);
        append_line(&seen, "D:\\y.mkv|3|4").unwrap();
        assert!(fs::read_to_string(&seen).unwrap().ends_with("D:\\x.mkv|1|2\r\nD:\\y.mkv|3|4\r\n"));
    }

    #[test]
    fn ffmpeg_discovery_takes_the_newest() {
        let d = temp_dir("dv-ff");
        for v in ["ffmpeg-7.1-full_build", "ffmpeg-9.0.2-full_build"] {
            fs::create_dir_all(d.join("Gyan.FFmpeg_Microsoft.Winget.Source_8wekyb3d8bbwe").join(v).join("bin")).unwrap();
        }
        fs::create_dir_all(d.join("Other.Pkg").join("ffmpeg-99").join("bin")).unwrap();
        let b = find_ffmpeg_bin(&d).unwrap();
        assert!(b.ends_with("Gyan.FFmpeg_Microsoft.Winget.Source_8wekyb3d8bbwe\\ffmpeg-9.0.2-full_build\\bin"), "{b:?}");
        assert!(find_ffmpeg_bin(&d.join("none")).is_none());
    }

    #[test]
    fn only_mkv_recursively() {
        let d = temp_dir("dv-walk");
        fs::create_dir_all(d.join("A (2020)")).unwrap();
        fs::create_dir_all(d.join("S").join("Season 01")).unwrap();
        for f in ["A (2020)\\a.mkv", "A (2020)\\a.mp4", "A (2020)\\a.srt", "S\\Season 01\\e1.MKV"] {
            fs::write(d.join(f), b"x").unwrap();
        }
        let files: Vec<String> = mkv_files(&d).iter().map(|p| p.strip_prefix(&d).unwrap().display().to_string()).collect();
        assert_eq!(files, vec!["A (2020)\\a.mkv", "S\\Season 01\\e1.MKV"]);
    }

    /// Fake tools: probes by file name; ffmpeg writes the tmp file.
    struct FakeTools {
        probes: Mutex<Vec<(String, Value)>>,
        ffmpeg_calls: Mutex<Vec<Vec<String>>>,
        exit: i32,
        out_probe: Value,
        /// ffmpeg also changes the original (an upgrade landing meanwhile).
        touch_original: bool,
    }

    impl Tools for FakeTools {
        fn probe(&self, file: &Path) -> Result<Value, String> {
            let name = file.file_name().unwrap().to_string_lossy().to_string();
            self.probes.lock().unwrap().push((name.clone(), Value::Null));
            if name.ends_with(".nodv.mkv") {
                return Ok(self.out_probe.clone());
            }
            if name.starts_with("sdr") {
                Ok(probe(true, "bt709", "100.0"))
            } else if name.starts_with("dv") {
                Ok(probe(true, "smpte2084", "100.0"))
            } else {
                Ok(probe(false, "smpte2084", "100.0"))
            }
        }

        fn ffmpeg(&self, args: &[String], _stop: &dyn Fn() -> bool) -> Result<(i32, Vec<String>), String> {
            self.ffmpeg_calls.lock().unwrap().push(args.to_vec());
            if self.touch_original {
                let input = &args[args.iter().position(|a| a == "-i").unwrap() + 1];
                fs::write(input, b"original+upgrade").unwrap();
            }
            if self.exit == 0 {
                fs::write(args.last().unwrap(), b"remuxed").unwrap();
            }
            Ok((self.exit, if self.exit == 0 { vec![] } else { vec!["boom".into()] }))
        }
    }

    fn tools(exit: i32, out_probe: Value) -> FakeTools {
        FakeTools { probes: Mutex::new(vec![]), ffmpeg_calls: Mutex::new(vec![]), exit, out_probe, touch_original: false }
    }

    struct Setup {
        lib: PathBuf,
        cfg: DvConfig,
    }

    fn setup() -> Setup {
        let d = temp_dir("dv-run");
        let lib = d.join("Movies");
        fs::create_dir_all(lib.join("Dv (2020)")).unwrap();
        fs::create_dir_all(lib.join("Plain (2020)")).unwrap();
        fs::write(lib.join("Dv (2020)").join("dv.mkv"), b"original").unwrap();
        fs::write(lib.join("Plain (2020)").join("plain.mkv"), b"plain").unwrap();
        let cfg = DvConfig {
            libraries: vec![Library { root: lib.display().to_string(), arr: "radarr".into(), section: 1 }],
            tmp_dir: d.join("tmp"),
            log: d.join("logs").join("strip-dv.log"),
            seen: d.join("logs").join("strip-dv-seen.txt"),
            ffmpeg_bin: String::new(),
            ffmpeg_packages: String::new(),
            delete_wait: Duration::from_millis(1),
            force_cross_volume: false,
        };
        Setup { lib, cfg }
    }

    async fn run_worker(dry: bool, t: FakeTools, s: &Setup, movies: Value) -> (crate::jobs::testutil::Fake, Arc<FakeTools>, usize) {
        let f = fake(move |r: &Req| if r.path == "/api/v3/movie" { ok(movies.clone()) } else { ok(json!({})) }).await;
        let (a, _d) = app(&f.base, dry, json!({}));
        let t = Arc::new(t);
        let (t2, cfg) = (t.clone(), s.cfg.clone());
        let rt = Handle::current();
        let n = tokio::task::spawn_blocking(move || {
            let (_h, stop) = crate::jobs::stop_pair();
            let log = DvLog { path: cfg.log.clone(), dry_run: a.dry_run };
            let w = Worker { app: &a, rt, cfg, tools: &*t2, log, stop: &stop };
            w.run_all()
        })
        .await
        .unwrap();
        (f, t, n)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remux_replace_rescan_and_seen() {
        let s = setup();
        let dv_dir = s.lib.join("Dv (2020)").display().to_string();
        let movies = json!([{"id": 3, "path": "x"}, {"id": 9, "path": dv_dir.to_uppercase()}]);
        let (f, t, n) = run_worker(false, tools(0, probe(false, "smpte2084", "101.5")), &s, movies.clone()).await;
        assert_eq!(n, 2);
        let dv = s.lib.join("Dv (2020)").join("dv.mkv");
        assert_eq!(fs::read(&dv).unwrap(), b"remuxed");
        assert!(!s.cfg.tmp_dir.join("dv.nodv.mkv").exists());
        assert_eq!(t.ffmpeg_calls.lock().unwrap().len(), 1);
        let cmds: Vec<Value> = f.find("POST", "/api/v3/command").iter().map(Req::json).collect();
        assert_eq!(cmds, vec![json!({"name": "RescanMovie", "movieId": 9})]);
        let refresh = f.find("GET", "/library/sections/1/refresh");
        assert_eq!(refresh.len(), 1);
        assert!(refresh[0].path.ends_with(&crate::services::arr_jobs::urlencode(&dv_dir)));
        assert_eq!(refresh[0].header("x-plex-token"), Some("plex-tok"));
        assert!(!refresh[0].path.contains("plex-tok"));
        let seen = load_seen(&s.cfg.seen);
        assert!(seen.contains(&stamp(&dv).unwrap()));
        assert!(seen.contains(&stamp(&s.lib.join("Plain (2020)").join("plain.mkv")).unwrap()));
        let log = fs::read_to_string(&s.cfg.log).unwrap();
        assert!(log.contains("  fara DV: plain.mkv"), "{log}");
        assert!(log.contains("  remux start: dv.mkv (0.0 GB)"), "{log}");
        assert!(log.contains("  gata in 0.0 min: dv.mkv"), "{log}");
        assert_eq!(&log[4..5], "-");
        // Second pass: everything is seen, nothing probed.
        let (_f, t, n) = run_worker(false, tools(0, Value::Null), &s, movies).await;
        assert_eq!(n, 0);
        assert!(t.probes.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_ffmpeg_or_verification_keeps_the_original() {
        for (exit, out) in [(1, probe(false, "smpte2084", "100")), (0, probe(true, "smpte2084", "100")), (0, probe(false, "smpte2084", "90"))] {
            let s = setup();
            let (f, _t, _n) = run_worker(false, tools(exit, out), &s, json!([])).await;
            let dv = s.lib.join("Dv (2020)").join("dv.mkv");
            assert_eq!(fs::read(&dv).unwrap(), b"original");
            assert!(!s.cfg.tmp_dir.join("dv.nodv.mkv").exists());
            assert!(!load_seen(&s.cfg.seen).contains(&stamp(&dv).unwrap()));
            assert!(f.mutations().is_empty());
            let log = fs::read_to_string(&s.cfg.log).unwrap();
            assert!(log.contains("EROARE: ffmpeg a esuat (1), originalul ramane") || log.contains("EROARE: verificare esuata"), "{log}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dry_run_touches_nothing() {
        let s = setup();
        let (f, t, n) = run_worker(true, tools(0, probe(false, "smpte2084", "100")), &s, json!([])).await;
        assert_eq!(n, 2);
        assert!(t.ffmpeg_calls.lock().unwrap().is_empty());
        assert_eq!(fs::read(s.lib.join("Dv (2020)").join("dv.mkv")).unwrap(), b"original");
        assert!(!s.cfg.seen.exists() && !s.cfg.log.exists() && !s.cfg.tmp_dir.exists());
        assert!(f.requests().is_empty());
    }

    /// Run passes on one app (one state file) with fresh fake tools each time.
    async fn run_on(a: &Arc<AppState>, t: FakeTools, cfg: &DvConfig) -> Arc<FakeTools> {
        let t = Arc::new(t);
        let (t2, cfg, a) = (t.clone(), cfg.clone(), a.clone());
        let rt = Handle::current();
        tokio::task::spawn_blocking(move || {
            let (_h, stop) = crate::jobs::stop_pair();
            let log = DvLog { path: cfg.log.clone(), dry_run: a.dry_run };
            let w = Worker { app: &a, rt, cfg, tools: &*t2, log, stop: &stop };
            w.run_all()
        })
        .await
        .unwrap();
        t
    }

    async fn test_app() -> (crate::jobs::testutil::Fake, Arc<AppState>) {
        let f = fake(|_r: &Req| ok(json!([]))).await;
        let (a, _d) = app(&f.base, false, json!({}));
        (f, a)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn original_changed_during_the_remux_is_not_replaced() {
        let s = setup();
        let (f, a) = test_app().await;
        let mut t = tools(0, probe(false, "smpte2084", "100"));
        t.touch_original = true;
        run_on(&a, t, &s.cfg).await;
        let dv = s.lib.join("Dv (2020)").join("dv.mkv");
        assert_eq!(fs::read(&dv).unwrap(), b"original+upgrade");
        assert!(!s.cfg.tmp_dir.join("dv.nodv.mkv").exists());
        assert!(!load_seen(&s.cfg.seen).contains(&stamp(&dv).unwrap()));
        assert!(load_failed(&a).is_empty(), "a change is not a failure");
        assert!(fs::read_to_string(&s.cfg.log).unwrap().contains("originalul s-a schimbat"));
        assert!(f.mutations().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failures_are_skipped_for_a_day() {
        let s = setup();
        let (_f, a) = test_app().await;
        run_on(&a, tools(1, Value::Null), &s.cfg).await;
        let dv = s.lib.join("Dv (2020)").join("dv.mkv");
        let st = stamp(&dv).unwrap();
        assert!(load_failed(&a).contains_key(&st));
        // Next pass: not retried (the plain file is seen, the DV one failed recently).
        let t = run_on(&a, tools(0, probe(false, "smpte2084", "100")), &s.cfg).await;
        assert!(t.ffmpeg_calls.lock().unwrap().is_empty());
        assert!(t.probes.lock().unwrap().is_empty());
        // A day later it is tried again (and succeeds); old entries are pruned.
        let mut old = HashMap::new();
        old.insert(st.clone(), unix_now() - FAILED_RETRY_SECONDS - 1.0);
        let mut m = serde_json::Map::new();
        m.insert(FAILED_KEY.into(), json!(old));
        a.state_file.update(m);
        let t = run_on(&a, tools(0, probe(false, "smpte2084", "100")), &s.cfg).await;
        assert_eq!(t.ffmpeg_calls.lock().unwrap().len(), 1);
        assert_eq!(fs::read(&dv).unwrap(), b"remuxed");
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread")]
    async fn locked_original_keeps_tmp_and_records_the_failure() {
        use std::os::windows::fs::OpenOptionsExt;
        let s = setup();
        let (_f, a) = test_app().await;
        let dv = s.lib.join("Dv (2020)").join("dv.mkv");
        let st = stamp(&dv).unwrap();
        // Held open without sharing: like Plex reading it.
        let lock = fs::OpenOptions::new().read(true).share_mode(0).open(&dv).unwrap();
        run_on(&a, tools(0, probe(false, "smpte2084", "100")), &s.cfg).await;
        drop(lock);
        assert_eq!(fs::read(&dv).unwrap(), b"original");
        assert!(s.cfg.tmp_dir.join("dv.nodv.mkv").exists(), "the remuxed file stays in tmp");
        assert!(load_failed(&a).contains_key(&st));
        assert!(fs::read_to_string(&s.cfg.log).unwrap().contains("EROARE: originalul e blocat"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cross_volume_copies_to_partial_then_renames() {
        let mut s = setup();
        s.cfg.force_cross_volume = true;
        let (_f, a) = test_app().await;
        run_on(&a, tools(0, probe(false, "smpte2084", "100")), &s.cfg).await;
        let dir = s.lib.join("Dv (2020)");
        assert_eq!(fs::read(dir.join("dv.mkv")).unwrap(), b"remuxed");
        assert!(!dir.join("dv.mkv.partial").exists());
        assert!(!s.cfg.tmp_dir.join("dv.nodv.mkv").exists());
        assert!(same_volume(Path::new("D:\\a\\b"), Path::new("d:\\c")));
        assert!(!same_volume(Path::new("D:\\a"), Path::new("C:\\a")));
        assert!(!same_volume(Path::new("relative"), Path::new("relative")));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn old_orphan_temp_files_are_deleted() {
        let s = setup();
        fs::create_dir_all(&s.cfg.tmp_dir).unwrap();
        let old = s.cfg.tmp_dir.join("old.nodv.mkv");
        let fresh = s.cfg.tmp_dir.join("fresh.nodv.mkv");
        let other = s.cfg.tmp_dir.join("keep.txt");
        for p in [&old, &fresh, &other] {
            fs::write(p, b"x").unwrap();
        }
        let two_days = std::time::SystemTime::now() - Duration::from_secs(48 * 3600);
        for p in [&old, &other] {
            fs::OpenOptions::new().write(true).open(p).unwrap().set_modified(two_days).unwrap();
        }
        // Dry-run: only logged.
        let f = fake(|_r: &Req| ok(json!([]))).await;
        let (dry, _d) = app(&f.base, true, json!({}));
        run_on(&dry, tools(0, Value::Null), &s.cfg).await;
        assert!(old.exists());
        let (_f, a) = test_app().await;
        run_on(&a, tools(0, probe(false, "smpte2084", "100")), &s.cfg).await;
        assert!(!old.exists());
        assert!(fresh.exists() && other.exists());
    }
}
