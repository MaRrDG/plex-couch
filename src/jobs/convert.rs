//! "Conversie pentru TV" (PC tab): episodes the Samsung TV cannot direct-play are re-encoded
//! to HEVC Main10 on the AMD GPU (`hevc_amf`), on request only. The rule is
//! [`tv_needs_conversion`]: H.264 with more than 8 bits per sample (Hi10P). Plex would
//! transcode those to DASH and the TV buffers every ~25 s; HEVC Main10 direct-plays.
//!
//! ## The list (`GET /api/convert`)
//! From Sonarr only (series, episode files with `mediaInfo.videoCodec` / `videoBitDepth`,
//! episodes), never an ffprobe of the library; "watched" is Plex's `viewCount` per file path
//! (section 2, every episode); files in the ledger are already converted. Per series: how many
//! unwatched episodes need it and the next one. Cached 60 s (a failure 15 s) and loaded in the
//! background, so a slow Sonarr never blocks the page.
//!
//! ## A batch (`POST /api/convert/start` `{seriesId, count}`, count 1..=20)
//! One batch at a time, on the job's own OS thread, one episode at a time in episode order
//! (season, episode; specials last), from the first unwatched one. An episode playing in Plex
//! (a session's file path, or show + season + episode) is passed over for the next one. Per
//! episode: ffprobe (still Hi10P? else it is skipped), find the torrent file the library file is
//! hardlinked to (below), ffmpeg with exactly [`ffmpeg_args`] (below-normal priority, no
//! window) into the temp dir, verify the result (HEVC, duration within 1 s, as many audio and
//! subtitle streams), then one rename over the library file while it still has the stamp it had
//! before (across volumes via `<file>.partial`; retried every 30 s for 10 min while locked or
//! playing; on failure the temp file is deleted and the library file stays). Then the ledger
//! entry, a Plex refresh of the folder and Sonarr `RescanSeries`. Renaming over the library
//! path breaks its hardlink: the torrent copy stays byte-identical (FileList seeding). ntfy
//! when the batch ends (also after errors). A failed episode counts towards the count, so a
//! broken encoder never runs through the whole series.
//!
//! **Stop** (`POST /api/convert/stop`) kills ffmpeg at once and deletes its temp file; the
//! library file is untouched and the batch ends.
//!
//! ## Finding the torrent file (before converting)
//! Only when the library file has more than one hard link. Candidates: Sonarr's
//! `downloadFolderImported` history of the episode (`data.droppedPath`, newest first), then
//! every completed qBittorrent torrent under the downloads root with a file of the same size.
//! A candidate counts only if it is the same file (NTFS volume serial + file index,
//! `GetFileInformationByHandle`). None found: the entry has no original (no revert).
//!
//! ## Revert after watching (every 5 min while no batch runs)
//! The ledger (`convert-ledger.json` next to the state file, atomic writes) keeps per converted
//! file: library path, original torrent file (or none), both sizes, the time. For each entry:
//! - the library file is gone or has another size (an upgrade replaced it): drop the entry;
//! - not watched, or playing now, or Plex/qBittorrent did not answer: keep it for later;
//! - watched and the original still exists with its size and is still a file of a torrent in
//!   qBittorrent: hard-link it next to the library file (`<name>.revert.tmp`, same folder),
//!   rename that over the converted file, Plex refresh + `RescanSeries`, drop the entry;
//! - watched but no original, or the original is gone (the seed keeper deleted the torrent):
//!   keep the converted file, drop the entry.
//!
//! Dry-run: no ffmpeg, no replace, no hard link; would-be actions go to the app log (the
//! ledger is the app's own file and is kept). Demo: fixed data, start/stop do nothing.

use crate::app::AppState;
use crate::jobs::common::{ffmpeg_bin, file_id, free_bytes, id_of, job_cfg, job_str, quiet_command, run_capture, run_lines, same_volume, unix_now};
use crate::jobs::seed_keeper::under_root;
use crate::jobs::strip_dv::{duration, stamp, DvConfig, Ffmpeg};
use crate::jobs::Stop;
use crate::services::plex_jobs::{path_key, PlexJobs};
use crate::services::qbit_jobs::QbitJobs;
use crate::services::UpstreamError;
use crate::{log_info, log_warning};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::runtime::Handle;

/// The revert pass while idle.
pub const INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const FIRST_RUN: Duration = Duration::from_secs(120);
/// The ledger, next to `state.local.json`.
pub const LEDGER_FILE: &str = "convert-ledger.json";
pub const MAX_COUNT: i64 = 20;
pub const DEFAULT_COUNT: i64 = 2;
pub const DEFAULT_TMP_DIR: &str = "D:\\Media\\Downloads\\.reencode";
/// `<stem>.hevc.mkv` in the temp dir.
pub const TMP_SUFFIX: &str = ".hevc.mkv";
/// The converted file may be this much shorter or longer than the source.
pub const DURATION_TOLERANCE: f64 = 1.0;
/// Sonarr's `EpisodeHistoryEventType.DownloadFolderImported`.
const SONARR_IMPORTED: i64 = 3;
const PROBE_TIMEOUT: Duration = Duration::from_secs(120);
const ENCODE_TIMEOUT: Duration = Duration::from_secs(4 * 3600);
/// Replacing the library file: 20 tries, 30 s apart (Plex may hold it).
const REPLACE_TRIES: u32 = 20;
const REPLACE_WAIT: Duration = Duration::from_secs(30);
const LIST_TTL: Duration = Duration::from_secs(60);
const LIST_ERROR_TTL: Duration = Duration::from_secs(15);
/// How long `GET /api/convert` waits for a list it had to load.
const LIST_WAIT: Duration = Duration::from_secs(5);
const START_TIMEOUT: Duration = Duration::from_secs(10);

// -- the rule ----------------------------------------------------------------------------

/// The TV cannot direct-play this video: H.264 with more than 8 bits per sample (Hi10P).
/// The one place to extend (another codec, 4:2:2, ...).
pub fn tv_needs_conversion(codec: &str, bit_depth: i64) -> bool {
    let c = codec.trim().to_ascii_lowercase();
    matches!(c.as_str(), "h264" | "x264" | "avc") && bit_depth > 8
}

/// A Sonarr episode file, by its `mediaInfo` (`videoCodec` is `x264` / `h264` for H.264).
pub fn file_needs_conversion(file: &Value) -> bool {
    let mi = file.get("mediaInfo");
    let codec = mi.and_then(|m| m.get("videoCodec")).and_then(Value::as_str).unwrap_or("");
    let depth = mi.and_then(|m| m.get("videoBitDepth")).and_then(Value::as_i64).unwrap_or(0);
    tv_needs_conversion(codec, depth)
}

fn first_video(probe: &Value) -> Option<&Value> {
    probe.get("streams")?.as_array()?.iter().find(|s| s.get("codec_type").and_then(Value::as_str) == Some("video"))
}

/// Bits per sample of an ffprobe video stream: `bits_per_raw_sample`, else from `pix_fmt`
/// (`yuv420p10le` -> 10), else 8.
fn probe_bit_depth(v: &Value) -> i64 {
    let raw = v.get("bits_per_raw_sample").and_then(|b| b.as_i64().or_else(|| b.as_str().and_then(|s| s.trim().parse().ok())));
    if let Some(n) = raw.filter(|n| *n > 0) {
        return n;
    }
    let pf = v.get("pix_fmt").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
    [16, 14, 12, 10, 9].into_iter().find(|d| pf.contains(&format!("p{d}"))).unwrap_or(8)
}

/// The same rule on an ffprobe result (the file itself, checked before converting).
pub fn probe_needs_conversion(probe: &Value) -> bool {
    first_video(probe).is_some_and(|v| tv_needs_conversion(v.get("codec_name").and_then(Value::as_str).unwrap_or(""), probe_bit_depth(v)))
}

// -- episodes and candidates (pure) ------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Episode {
    pub series_id: i64,
    pub series_title: String,
    pub episode_id: i64,
    pub season: i64,
    pub number: i64,
    /// The library file.
    pub path: String,
    pub size: u64,
}

impl Episode {
    pub fn label(&self) -> String {
        label(self.season, self.number)
    }
}

pub fn label(season: i64, number: i64) -> String {
    format!("S{season:02}E{number:02}")
}

/// The episodes of one series that need converting, not watched (Plex `viewCount`; a file
/// Plex does not know yet counts as unwatched) and not converted already, in episode order
/// with specials last. A file holding several episodes is listed once.
pub fn candidates(series: &Value, episodes: &[Value], files: &[Value], views: &HashMap<String, i64>, converted: &HashSet<String>) -> Vec<Episode> {
    let series_id = id_of(series.get("id")).unwrap_or(0);
    let title = series.get("title").and_then(Value::as_str).unwrap_or("").to_string();
    let by_id: HashMap<i64, &Value> = files.iter().filter_map(|f| Some((id_of(f.get("id"))?, f))).collect();
    let mut out = Vec::new();
    for e in episodes {
        let Some(f) = id_of(e.get("episodeFileId")).and_then(|id| by_id.get(&id)) else { continue };
        let Some(path) = f.get("path").and_then(Value::as_str) else { continue };
        let key = path_key(path);
        if !file_needs_conversion(f) || converted.contains(&key) || views.get(&key).is_some_and(|n| *n > 0) {
            continue;
        }
        out.push(Episode {
            series_id,
            series_title: title.clone(),
            episode_id: id_of(e.get("id")).unwrap_or(0),
            season: e.get("seasonNumber").and_then(Value::as_i64).unwrap_or(0),
            number: e.get("episodeNumber").and_then(Value::as_i64).unwrap_or(0),
            path: path.to_string(),
            size: f.get("size").and_then(Value::as_u64).unwrap_or(0),
        });
    }
    out.sort_by_key(|e| (e.season == 0, e.season, e.number, e.episode_id));
    let mut seen = HashSet::new();
    out.retain(|e| seen.insert(path_key(&e.path)));
    out
}

/// What Plex is playing right now: file paths and (show, season, episode).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Playing {
    pub files: HashSet<String>,
    pub episodes: HashSet<(String, i64, i64)>,
}

impl Playing {
    pub fn contains(&self, path: &str, show: &str, season: i64, number: i64) -> bool {
        self.files.contains(&path_key(path)) || self.episodes.contains(&(show.trim().to_lowercase(), season, number))
    }

    pub fn has(&self, e: &Episode) -> bool {
        self.contains(&e.path, &e.series_title, e.season, e.number)
    }
}

/// Plex `/status/sessions` JSON -> [`Playing`] (`Media[].Part[].file`; episodes also by
/// `grandparentTitle` + `parentIndex` + `index`).
pub fn playing_from_sessions(raw: Option<&Value>) -> Playing {
    let mut p = Playing::default();
    let metas = raw.and_then(|r| r.get("MediaContainer")).and_then(|m| m.get("Metadata")).and_then(Value::as_array);
    for m in metas.into_iter().flatten() {
        for media in m.get("Media").and_then(Value::as_array).into_iter().flatten() {
            for part in media.get("Part").and_then(Value::as_array).into_iter().flatten() {
                if let Some(f) = part.get("file").and_then(Value::as_str) {
                    p.files.insert(path_key(f));
                }
            }
        }
        if m.get("type").and_then(Value::as_str) == Some("episode") {
            let show = m.get("grandparentTitle").and_then(Value::as_str).unwrap_or("").trim().to_lowercase();
            let (s, e) = (m.get("parentIndex").and_then(Value::as_i64), m.get("index").and_then(Value::as_i64));
            if let (false, Some(s), Some(e)) = (show.is_empty(), s, e) {
                p.episodes.insert((show, s, e));
            }
        }
    }
    p
}

/// The next episode from the front of `queue` that is not playing, and the playing ones
/// passed over on the way (not retried in this batch).
pub fn next_pick(queue: &mut VecDeque<Episode>, playing: &dyn Fn(&Episode) -> bool) -> (Option<Episode>, Vec<Episode>) {
    let mut skipped = Vec::new();
    while let Some(e) = queue.pop_front() {
        if playing(&e) {
            skipped.push(e);
        } else {
            return (Some(e), skipped);
        }
    }
    (None, skipped)
}

/// Up to `count` episodes a batch would take (with the playing ones it passes over), the
/// same steps as the batch loop when nothing fails.
pub fn pick(cands: &[Episode], playing: &dyn Fn(&Episode) -> bool, count: usize) -> (Vec<Episode>, Vec<Episode>) {
    let mut q: VecDeque<Episode> = cands.iter().cloned().collect();
    let (mut take, mut skipped) = (Vec::new(), Vec::new());
    while take.len() < count {
        let (e, s) = next_pick(&mut q, playing);
        skipped.extend(s);
        match e {
            Some(e) => take.push(e),
            None => break,
        }
    }
    (take, skipped)
}

// -- ffmpeg ------------------------------------------------------------------------------

/// The hand-tested command (HEVC Main10 on the AMD encoder, BT.709 tags written into the
/// stream too: the sources say "unknown" and the TV then shows wrong colours; everything
/// else copied), plus `-progress pipe:1 -nostats` for the progress bar.
pub fn ffmpeg_args(input: &Path, out: &Path) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-y", "-progress", "pipe:1", "-nostats", "-i"].map(String::from).to_vec();
    a.push(input.to_string_lossy().into_owned());
    a.extend(
        [
            "-map", "0", "-c", "copy", "-c:v", "hevc_amf", "-profile:v", "main10", "-pix_fmt", "p010le", "-quality", "quality",
            "-rc", "cqp", "-qp_i", "19", "-qp_p", "21", "-qp_b", "23", "-color_primaries", "bt709", "-color_trc", "bt709",
            "-colorspace", "bt709", "-color_range", "tv", "-bsf:v",
            "hevc_metadata=colour_primaries=1:transfer_characteristics=1:matrix_coefficients=1:video_full_range_flag=0",
            "-tag:v", "hvc1",
        ]
        .map(String::from),
    );
    a.push(out.to_string_lossy().into_owned());
    a
}

/// `<stem>.hevc.mkv` in the temp dir.
pub fn tmp_path(tmp_dir: &Path, file: &Path) -> PathBuf {
    let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    tmp_dir.join(format!("{stem}{TMP_SUFFIX}"))
}

/// Seconds encoded so far from one `-progress` line (`out_time_us=`, `out_time_ms=` which is
/// also microseconds, or `out_time=HH:MM:SS.ffffff`). Anything else (`N/A` too) is None.
pub fn parse_progress(line: &str) -> Option<f64> {
    let (k, v) = line.trim().split_once('=')?;
    let v = v.trim();
    match k.trim() {
        "out_time_us" | "out_time_ms" => v.parse::<i64>().ok().filter(|n| *n >= 0).map(|n| n as f64 / 1_000_000.0),
        "out_time" => {
            let mut p = v.split(':');
            let (h, m, s) = (p.next()?.parse::<f64>().ok()?, p.next()?.parse::<f64>().ok()?, p.next()?.parse::<f64>().ok()?);
            (h >= 0.0 && m >= 0.0 && s >= 0.0).then_some(h * 3600.0 + m * 60.0 + s)
        }
        _ => None,
    }
}

/// Percent done, 0..=100 (0 while the length is unknown), one decimal.
pub fn percent(done: f64, total: f64) -> f64 {
    if total <= 0.0 || !done.is_finite() {
        return 0.0;
    }
    crate::json::py_round((done / total * 100.0).clamp(0.0, 100.0), 1)
}

fn stream_count(probe: &Value, kind: &str) -> usize {
    probe.get("streams").and_then(Value::as_array).map(|a| a.iter().filter(|s| s.get("codec_type").and_then(Value::as_str) == Some(kind)).count()).unwrap_or(0)
}

/// The converted file is usable: HEVC video, the same length within 1 s, as many audio and
/// subtitle streams as the source. `Err` says why not (log text).
pub fn verify(src: &Value, out: &Value) -> Result<(), String> {
    let codec = first_video(out).and_then(|v| v.get("codec_name")).and_then(Value::as_str).unwrap_or("");
    if codec != "hevc" {
        return Err(format!("video {codec:?}, nu hevc"));
    }
    let (a, b) = (duration(src), duration(out));
    if a <= 0.0 || (a - b).abs() > DURATION_TOLERANCE {
        return Err(format!("durata {b:.1} s fata de {a:.1} s"));
    }
    for kind in ["audio", "subtitle"] {
        let (x, y) = (stream_count(src, kind), stream_count(out, kind));
        if x != y {
            return Err(format!("{y} fluxuri {kind} fata de {x}"));
        }
    }
    Ok(())
}

/// The external programs, behind a trait so tests can fake ffmpeg/ffprobe.
pub trait Tools: Send + Sync {
    /// `ffprobe -v error -show_format -show_streams -of json <file>`.
    fn probe(&self, file: &Path) -> Result<Value, String>;
    /// Run ffmpeg with `args`, stdout lines to `on_line`: `(exit code, stderr lines)`. Returns
    /// early (killing ffmpeg) when `stop` says so.
    fn encode(&self, args: &[String], on_line: &mut dyn FnMut(&str), stop: &dyn Fn() -> bool) -> Result<(i32, Vec<String>), String>;
}

impl Tools for Ffmpeg {
    fn probe(&self, file: &Path) -> Result<Value, String> {
        let mut c = quiet_command(&self.bin.join("ffprobe.exe"));
        c.args(["-v", "error", "-show_format", "-show_streams", "-of", "json"]).arg(file);
        let (_code, out, _err) = run_capture(c, PROBE_TIMEOUT, &|| false)?;
        serde_json::from_slice(&out).map_err(|_| "ffprobe: raspuns JSON invalid".to_string())
    }

    fn encode(&self, args: &[String], on_line: &mut dyn FnMut(&str), stop: &dyn Fn() -> bool) -> Result<(i32, Vec<String>), String> {
        let mut c = quiet_command(&self.bin.join("ffmpeg.exe"));
        c.args(args);
        let (code, err) = run_lines(c, ENCODE_TIMEOUT, stop, on_line)?;
        let lines = String::from_utf8_lossy(&err).lines().map(|l| l.trim_end().to_string()).filter(|l| !l.is_empty()).collect();
        Ok((code, lines))
    }
}

// -- the ledger --------------------------------------------------------------------------

/// One converted file.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub library: String,
    /// The torrent file the library file was hardlinked to, if any.
    pub original: Option<String>,
    /// The torrent's hash, when known (a hint for the qBittorrent check).
    pub torrent: Option<String>,
    pub original_size: u64,
    pub converted_size: u64,
    pub converted_at: f64,
    pub series_id: i64,
    pub series: String,
    pub episode_id: i64,
    pub season: i64,
    pub episode: i64,
}

impl Entry {
    pub fn to_json(&self) -> Value {
        json!({
            "library": self.library, "original": self.original, "torrent": self.torrent,
            "original_size": self.original_size, "converted_size": self.converted_size, "converted_at": self.converted_at,
            "series_id": self.series_id, "series": self.series, "episode_id": self.episode_id,
            "season": self.season, "episode": self.episode,
        })
    }

    pub fn from_json(v: &Value) -> Option<Entry> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).filter(|x| !x.is_empty()).map(String::from);
        let n = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
        let u = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        Some(Entry {
            library: s("library")?,
            original: s("original"),
            torrent: s("torrent"),
            original_size: u("original_size"),
            converted_size: u("converted_size"),
            converted_at: v.get("converted_at").and_then(Value::as_f64).unwrap_or(0.0),
            series_id: n("series_id"),
            series: s("series").unwrap_or_default(),
            episode_id: n("episode_id"),
            season: n("season"),
            episode: n("episode"),
        })
    }

    pub fn label(&self) -> String {
        label(self.season, self.episode)
    }
}

/// `<state dir>\convert-ledger.json`.
pub fn ledger_path(app: &AppState) -> PathBuf {
    app.state_file.path().parent().map(Path::to_path_buf).unwrap_or_default().join(LEDGER_FILE)
}

/// `{"version": 1, "files": {"<path key>": {...}}}`. Only a missing file is empty; an
/// unreadable or unparsable one is an error (nothing is converted or written then).
pub fn load_ledger(path: &Path) -> Result<Vec<Entry>, String> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(crate::state::io_error_name(&e).to_string()),
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(m)) => match m.get("files") {
            Some(Value::Object(f)) => Ok(f.values().filter_map(Entry::from_json).collect()),
            None => Ok(Vec::new()),
            Some(_) => Err("TypeError".into()),
        },
        Ok(_) => Err("TypeError".into()),
        Err(_) => Err("JSONDecodeError".into()),
    }
}

/// Atomic write (`.tmp` + rename), sorted, pretty.
pub fn save_ledger(path: &Path, entries: &[Entry]) {
    let files: Map<String, Value> = entries.iter().map(|e| (path_key(&e.library), e.to_json())).collect();
    let v = crate::json::sorted(&json!({"version": 1, "files": files}));
    let mut bytes = crate::json::dump_pretty(&v);
    bytes.push(b'\n');
    let tmp = crate::config::tmp_path(path);
    if let Err(e) = fs::write(&tmp, &bytes).and_then(|_| fs::rename(&tmp, path)) {
        log_warning!("convert: cannot save the ledger: {}", crate::state::io_error_name(&e));
    }
}

/// The library files already converted (path keys).
pub fn converted_keys(entries: &[Entry]) -> HashSet<String> {
    entries.iter().map(|e| path_key(&e.library)).collect()
}

// -- revert decision (pure) --------------------------------------------------------------

/// The original torrent file of a ledger entry, as found now.
#[derive(Debug, Clone, PartialEq)]
pub enum Original {
    /// None was recorded (the library file had no hardlinked download copy).
    NotRecorded,
    /// Missing, or another size than recorded.
    Gone,
    /// There with its size; `in_torrent`: still a file of a torrent in qBittorrent (`None`:
    /// qBittorrent did not answer, or it was not asked).
    Present { in_torrent: Option<bool> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Facts {
    /// The library file exists with the converted size.
    pub library_ok: bool,
    /// Plex `viewCount > 0` (`None`: not in Plex).
    pub watched: Option<bool>,
    pub playing: bool,
    pub original: Original,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Keep,
    Revert,
    /// Forget the entry (the converted file stays); the reason for the log.
    Drop(&'static str),
}

pub fn revert_decision(f: &Facts) -> Decision {
    if !f.library_ok {
        return Decision::Drop("the library file changed or is gone");
    }
    if f.watched != Some(true) || f.playing {
        return Decision::Keep;
    }
    match f.original {
        Original::NotRecorded => Decision::Drop("watched, no torrent file to go back to"),
        Original::Gone => Decision::Drop("watched, the torrent file is gone"),
        Original::Present { in_torrent: Some(true) } => Decision::Revert,
        Original::Present { in_torrent: Some(false) } => Decision::Drop("watched, the file is no longer in a torrent"),
        Original::Present { in_torrent: None } => Decision::Keep,
    }
}

// -- texts -------------------------------------------------------------------------------

/// `1 episod`, `3 episoade`, `20 de episoade`.
pub fn episodes_ro(n: usize) -> String {
    let de = n > 0 && (n.is_multiple_of(100) || n % 100 >= 20);
    match n {
        1 => "1 episod".into(),
        _ if de => format!("{n} de episoade"),
        _ => format!("{n} episoade"),
    }
}

/// The ntfy text when a batch ends: `(message, is an error)`.
pub fn summary(title: &str, ok: usize, failed: usize, stopped: bool, dry_run: bool) -> (String, bool) {
    if dry_run {
        return (format!("Probă: s-ar fi convertit {} ({title})", episodes_ro(ok)), false);
    }
    if stopped {
        return if ok > 0 {
            (format!("Conversie oprită după {} ({title})", episodes_ro(ok)), false)
        } else {
            (format!("Conversie oprită ({title})"), false)
        };
    }
    match (ok, failed) {
        (0, 0) => (format!("Nimic de convertit ({title})"), false),
        (0, f) => (format!("Conversie eșuată: {} ({title})", episodes_ro(f)), true),
        (o, 0) => (format!("Conversie gata: {} ({title})", episodes_ro(o)), false),
        (o, f) => (format!("Conversie gata: {}, {f} {} ({title})", episodes_ro(o), if f == 1 { "eșuat" } else { "eșuate" }), true),
    }
}

// -- shared state (routes <-> job) -------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub series_id: i64,
    pub title: String,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub label: String,
    /// `ok`, `failed`, `skipped`, `stopped`, `dry`.
    pub result: &'static str,
    pub note: String,
}

/// The current (or last) batch, as the PC tab shows it.
#[derive(Debug, Clone, Default)]
pub struct Batch {
    pub series_id: i64,
    pub title: String,
    pub requested: usize,
    pub running: bool,
    pub stopping: bool,
    pub dry_run: bool,
    /// The episode being converted and its percent.
    pub current: Option<(String, f64)>,
    /// Episodes still to do after the current one.
    pub remaining: usize,
    pub items: Vec<Item>,
    pub summary: Option<String>,
    pub started_at: f64,
    pub finished_at: Option<f64>,
}

impl Batch {
    pub fn to_json(&self) -> Value {
        json!({
            "series_id": self.series_id, "title": self.title, "requested": self.requested,
            "running": self.running, "stopping": self.stopping, "dry_run": self.dry_run,
            "current": self.current.as_ref().map(|(l, p)| json!({"label": l, "percent": p})),
            "remaining": self.remaining,
            "items": self.items.iter().map(|i| json!({"label": i.label, "result": i.result, "note": i.note})).collect::<Vec<_>>(),
            "summary": self.summary, "started_at": self.started_at, "finished_at": self.finished_at,
        })
    }

    fn push(&mut self, label: String, result: &'static str, note: &str) {
        self.items.push(Item { label, result, note: note.to_string() });
    }
}

struct ListCache {
    at: Instant,
    value: Result<Vec<Value>, String>,
}

/// Held by `AppState`: the batch status, the start request for the job thread, the stop
/// flag, the cached series list.
#[derive(Default)]
pub struct Control {
    batch: Mutex<Option<Batch>>,
    pending: Mutex<Option<Request>>,
    stop: AtomicBool,
    pub wake: tokio::sync::Notify,
    list: Mutex<Option<ListCache>>,
    loading: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Clears the "list is loading" flag even if the load panics.
struct Loading<'a>(&'a AtomicBool);

impl Drop for Loading<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Control {
    pub fn new() -> Control {
        Control::default()
    }

    pub fn running(&self) -> bool {
        lock(&self.batch).as_ref().is_some_and(|b| b.running)
    }

    /// Start a batch: false (refused) while one runs. Wakes the job thread.
    pub fn begin(&self, r: Request, dry_run: bool) -> bool {
        let mut b = lock(&self.batch);
        if b.as_ref().is_some_and(|b| b.running) {
            return false;
        }
        *b = Some(Batch {
            series_id: r.series_id,
            title: r.title.clone(),
            requested: r.count,
            running: true,
            dry_run,
            remaining: r.count,
            started_at: unix_now(),
            ..Default::default()
        });
        self.stop.store(false, Ordering::SeqCst);
        *lock(&self.pending) = Some(r);
        drop(b);
        self.wake.notify_one();
        true
    }

    /// Ask the running batch to stop; false if none runs.
    pub fn request_stop(&self) -> bool {
        let mut b = lock(&self.batch);
        match b.as_mut() {
            Some(b) if b.running => {
                b.stopping = true;
                self.stop.store(true, Ordering::SeqCst);
                true
            }
            _ => false,
        }
    }

    pub fn stop_requested(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    pub fn take_request(&self) -> Option<Request> {
        lock(&self.pending).take()
    }

    pub fn update(&self, f: impl FnOnce(&mut Batch)) {
        if let Some(b) = lock(&self.batch).as_mut() {
            f(b);
        }
    }

    pub fn batch(&self) -> Option<Batch> {
        lock(&self.batch).clone()
    }

    pub fn status_json(&self) -> Value {
        lock(&self.batch).as_ref().map(Batch::to_json).unwrap_or(Value::Null)
    }

    /// The next `GET /api/convert` reloads the list.
    pub fn invalidate_list(&self) {
        *lock(&self.list) = None;
    }

    fn cached_list(&self, fresh_only: bool) -> Option<Result<Vec<Value>, String>> {
        let g = lock(&self.list);
        let c = g.as_ref()?;
        let ttl = if c.value.is_ok() { LIST_TTL } else { LIST_ERROR_TTL };
        (!fresh_only || c.at.elapsed() < ttl).then(|| c.value.clone())
    }

    /// `(list, error, loading)`: the cached list; when it is old, reload it in the background
    /// and wait up to 5 s for it (else the old one, or "loading").
    pub async fn listing(&self, app: &Arc<AppState>) -> (Option<Vec<Value>>, Option<String>, bool) {
        let split = |r: Result<Vec<Value>, String>| match r {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e)),
        };
        if let Some(r) = self.cached_list(true) {
            let (l, e) = split(r);
            return (l, e, false);
        }
        if !self.loading.swap(true, Ordering::SeqCst) {
            let a = app.clone();
            let h = tokio::spawn(async move {
                let _g = Loading(&a.convert.loading);
                let value = list_series(&a).await;
                *lock(&a.convert.list) = Some(ListCache { at: Instant::now(), value });
            });
            let _ = tokio::time::timeout(LIST_WAIT, h).await;
        }
        match self.cached_list(false) {
            Some(r) => {
                let (l, e) = split(r);
                (l, e, self.loading.load(Ordering::SeqCst))
            }
            None => (None, None, true),
        }
    }
}

// -- Sonarr / Plex reads -----------------------------------------------------------------

/// The settings (`jobs.convert`; the Plex section defaults to strip_dv's Sonarr library).
#[derive(Debug, Clone)]
pub struct Settings {
    pub tmp_dir: PathBuf,
    pub section: u32,
    pub downloads_root: String,
    pub ffmpeg_bin: String,
    pub ffmpeg_packages: String,
    pub replace_tries: u32,
    pub replace_wait: Duration,
    /// Tests: take the cross-volume path (copy to `<file>.partial`) on one volume.
    pub force_cross_volume: bool,
}

impl Settings {
    pub fn from_app(app: &AppState) -> Settings {
        let dv = DvConfig::from_app(app);
        let section = job_cfg(app, "convert", "section")
            .and_then(|v| v.as_u64())
            .map(|n| n as u32)
            .or_else(|| dv.libraries.iter().find(|l| l.arr == "sonarr").map(|l| l.section))
            .unwrap_or(2);
        Settings {
            tmp_dir: job_str(app, "convert", "tmp_dir", DEFAULT_TMP_DIR).into(),
            section,
            downloads_root: job_str(app, "seed_keeper", "downloads_root", "D:\\Media\\Downloads"),
            ffmpeg_bin: job_str(app, "convert", "ffmpeg_bin", &dv.ffmpeg_bin),
            ffmpeg_packages: dv.ffmpeg_packages,
            replace_tries: REPLACE_TRIES,
            replace_wait: REPLACE_WAIT,
            force_cross_volume: false,
        }
    }
}

const SONARR_DOWN: &str = "Sonarr nu răspunde.";
const PLEX_DOWN: &str = "Plex nu răspunde, nu știu ce episoade ai văzut.";

fn ledger_error(t: &str) -> String {
    log_warning!("convert: the ledger is unreadable ({t})");
    format!("Lista conversiilor ({LEDGER_FILE}) nu se poate citi.")
}

async fn plex_views(app: &AppState, section: u32) -> Result<HashMap<String, i64>, String> {
    PlexJobs::new(app.services.plex.c.clone()).view_counts(section, true).await.map_err(|e| {
        log_warning!("convert: Plex section {section} failed: {}", e.type_name());
        PLEX_DOWN.to_string()
    })
}

/// Every series with unwatched episodes to convert:
/// `[{"id", "title", "pending", "next"}]`, by title. `Err`: a friendly message.
pub async fn list_series(app: &AppState) -> Result<Vec<Value>, String> {
    let arr = app.services.sonarr.arr();
    let series = arr.items().await.map_err(|e| {
        log_warning!("convert: Sonarr series failed: {}", e.type_name());
        SONARR_DOWN.to_string()
    })?;
    let converted = converted_keys(&load_ledger(&ledger_path(app)).map_err(|t| ledger_error(&t))?);
    let mut flagged = Vec::new();
    for s in &series {
        let Some(id) = id_of(s.get("id")) else { continue };
        let files_n = s.get("statistics").and_then(|x| x.get("episodeFileCount")).and_then(Value::as_i64);
        if files_n == Some(0) {
            continue;
        }
        let files = arr.episode_files(id).await.map_err(|_| SONARR_DOWN.to_string())?;
        if files.iter().any(|f| file_needs_conversion(f) && !converted.contains(&path_key(f.get("path").and_then(Value::as_str).unwrap_or("")))) {
            flagged.push((s, id, files));
        }
    }
    if flagged.is_empty() {
        return Ok(Vec::new());
    }
    let views = plex_views(app, Settings::from_app(app).section).await?;
    let mut rows = Vec::new();
    for (s, id, files) in flagged {
        let eps = arr.episodes(id).await.map_err(|_| SONARR_DOWN.to_string())?;
        let c = candidates(s, &eps, &files, &views, &converted);
        if let Some(first) = c.first() {
            rows.push(json!({"id": id, "title": first.series_title, "pending": c.len(), "next": first.label()}));
        }
    }
    rows.sort_by_key(|r| r["title"].as_str().unwrap_or("").to_lowercase());
    Ok(rows)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Lookup {
    NotFound,
    /// A service failed: the message for the page.
    Down(String),
}

/// One series' title and candidates, fresh (the start request and the batch).
pub async fn series_candidates(app: &AppState, series_id: i64) -> Result<(String, Vec<Episode>), Lookup> {
    let arr = app.services.sonarr.arr();
    let series = match arr.series(series_id).await {
        Ok(s) => s,
        Err(UpstreamError::Status(404)) => return Err(Lookup::NotFound),
        Err(e) => {
            log_warning!("convert: Sonarr series {series_id} failed: {}", e.type_name());
            return Err(Lookup::Down(SONARR_DOWN.into()));
        }
    };
    if id_of(series.get("id")) != Some(series_id) {
        return Err(Lookup::NotFound);
    }
    let title = series.get("title").and_then(Value::as_str).unwrap_or("").to_string();
    let converted = converted_keys(&load_ledger(&ledger_path(app)).map_err(|t| Lookup::Down(ledger_error(&t)))?);
    let files = arr.episode_files(series_id).await.map_err(|_| Lookup::Down(SONARR_DOWN.into()))?;
    if !files.iter().any(file_needs_conversion) {
        return Ok((title, Vec::new()));
    }
    let eps = arr.episodes(series_id).await.map_err(|_| Lookup::Down(SONARR_DOWN.into()))?;
    let views = plex_views(app, Settings::from_app(app).section).await.map_err(Lookup::Down)?;
    Ok((title, candidates(&series, &eps, &files, &views, &converted)))
}

// -- routes ------------------------------------------------------------------------------

fn demo_status() -> Value {
    json!({
        "series": [
            {"id": 1, "title": "Demon Slayer", "pending": 3, "next": "S02E09"},
            {"id": 2, "title": "Frieren: Beyond Journey's End", "pending": 12, "next": "S01E05"},
        ],
        "error": null, "loading": false, "job": null, "dry_run": true, "demo": true,
        "max": MAX_COUNT, "default": DEFAULT_COUNT,
    })
}

/// `GET /api/convert`: the list and the batch status.
pub async fn api_status(app: &Arc<AppState>) -> Value {
    if app.demo {
        return demo_status();
    }
    let (series, error, loading) = app.convert.listing(app).await;
    json!({
        "series": series, "error": error, "loading": loading, "job": app.convert.status_json(),
        "dry_run": app.dry_run, "demo": false, "max": MAX_COUNT, "default": DEFAULT_COUNT,
    })
}

/// `POST /api/convert/start` `{"seriesId": n, "count": 1..=20}` -> `(status, body)`.
pub async fn api_start(app: &Arc<AppState>, body: &Map<String, Value>) -> (u16, Value) {
    let series_id = body.get("seriesId").and_then(Value::as_i64).filter(|n| *n > 0);
    let count = body.get("count").and_then(Value::as_i64).filter(|n| (1..=MAX_COUNT).contains(n));
    let (Some(series_id), Some(count)) = (series_id, count) else {
        return (400, json!({"error": "Cerere invalidă."}));
    };
    if app.demo {
        return (200, json!({"ok": true, "demo": true, "job": null}));
    }
    let busy = || (409, json!({"error": "O conversie rulează deja. Oprește-o sau așteaptă să se termine."}));
    if app.convert.running() {
        return busy();
    }
    let (title, cands) = match tokio::time::timeout(START_TIMEOUT, series_candidates(app, series_id)).await {
        Err(_) => return (504, json!({"error": "Sonarr sau Plex răspunde greu. Încearcă din nou."})),
        Ok(Err(Lookup::NotFound)) => return (404, json!({"error": "Serialul nu există în Sonarr."})),
        Ok(Err(Lookup::Down(m))) => return (502, json!({"error": m})),
        Ok(Ok(x)) => x,
    };
    if cands.is_empty() {
        return (409, json!({"error": "Nu e nimic de convertit la acest serial."}));
    }
    let count = (count as usize).min(cands.len());
    if !app.convert.begin(Request { series_id, title, count }, app.dry_run) {
        return busy();
    }
    app.convert.invalidate_list();
    (200, json!({"ok": true, "job": app.convert.status_json()}))
}

/// `POST /api/convert/stop`.
pub fn api_stop(app: &AppState) -> (u16, Value) {
    if app.demo {
        return (200, json!({"ok": true, "demo": true, "job": null}));
    }
    if app.convert.request_stop() {
        (200, json!({"ok": true, "job": app.convert.status_json()}))
    } else {
        (409, json!({"error": "Nicio conversie nu rulează."}))
    }
}

// -- the job -----------------------------------------------------------------------------

/// What one episode came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Done {
    Converted,
    /// Dry-run: only logged.
    DryRun,
    /// ffprobe says it is no Hi10P (Sonarr's media info was stale): does not count.
    NotNeeded,
    Stopped,
    Failed(String),
}

/// A found original: the torrent file, its torrent's hash (when known), its size.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub path: String,
    pub torrent: Option<String>,
    pub size: u64,
}

pub struct Job<'a> {
    pub app: &'a AppState,
    pub rt: Handle,
    pub cfg: Settings,
    pub stop: &'a Stop,
}

fn ascii_log(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii() { c } else { '?' }).collect()
}

impl Job<'_> {
    fn control(&self) -> &Control {
        &self.app.convert
    }

    fn stopped(&self) -> bool {
        self.stop.is_stopped() || self.control().stop_requested()
    }

    /// Plex sessions now; None if Plex did not answer (treated as "nothing playing").
    fn playing(&self) -> Option<Playing> {
        match self.rt.block_on(self.app.services.plex.sessions_raw()) {
            Ok(v) => Some(playing_from_sessions(v.as_ref())),
            Err(e) => {
                log_info!("convert: Plex sessions failed ({}), assuming nothing plays", e.type_name());
                None
            }
        }
    }

    fn sleep(&self, d: Duration) -> bool {
        self.rt.block_on(self.stop.clone().sleep(d)) || self.control().stop_requested()
    }

    /// Delete leftover `*.hevc.mkv` in the temp dir (no batch runs at this point).
    fn clean_tmp(&self) {
        let Ok(rd) = fs::read_dir(&self.cfg.tmp_dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if !e.file_name().to_string_lossy().to_lowercase().ends_with(TMP_SUFFIX) || !p.is_file() {
                continue;
            }
            if self.app.dry_run {
                log_info!("convert (dry-run): would delete the old temp file {}", p.display());
            } else if fs::remove_file(&p).is_ok() {
                log_info!("convert: deleted the old temp file {}", p.display());
            }
        }
    }

    /// One batch, then the ntfy summary.
    pub fn run_batch(&self, req: &Request, tools: Option<&dyn Tools>) {
        let title = req.title.clone();
        log_info!("convert: batch of {} for '{}' (series {})", req.count, ascii_log(&title), req.series_id);
        let fail = |msg: String| {
            log_warning!("convert: batch for '{}' failed: {}", ascii_log(&title), ascii_log(&msg));
            let text = format!("Conversie eșuată ({title}): {msg}");
            self.finish(text, true);
        };
        let Some(tools) = tools else {
            return fail("ffmpeg nu e instalat (winget Gyan.FFmpeg).".into());
        };
        self.clean_tmp();
        let cands = match self.rt.block_on(series_candidates(self.app, req.series_id)) {
            Ok((_, c)) => c,
            Err(Lookup::NotFound) => return fail("serialul nu mai există în Sonarr.".into()),
            Err(Lookup::Down(m)) => return fail(m),
        };
        let mut queue: VecDeque<Episode> = cands.into_iter().collect();
        let (mut attempts, mut ok, mut failed, mut stopped) = (0usize, 0usize, 0usize, false);
        while attempts < req.count {
            if self.stopped() {
                stopped = true;
                break;
            }
            let playing = self.playing().unwrap_or_default();
            let (next, skipped) = next_pick(&mut queue, &|e| playing.has(e));
            for s in skipped {
                log_info!("convert: {} {} is playing, taking the next one", ascii_log(&title), s.label());
                self.control().update(|b| b.push(s.label(), "skipped", "se redă acum"));
            }
            let Some(ep) = next else { break };
            let left = (req.count - attempts - 1).min(queue.len());
            self.control().update(|b| {
                b.current = Some((ep.label(), 0.0));
                b.remaining = left;
            });
            let done = match tools.probe(Path::new(&ep.path)) {
                Ok(src) => self.convert_one(&ep, &src, tools),
                Err(e) => Done::Failed(format!("ffprobe: {e}")),
            };
            let (result, note) = match &done {
                Done::Converted => ("ok", String::new()),
                Done::DryRun => ("dry", "probă: nimic schimbat".to_string()),
                Done::NotNeeded => ("skipped", "nu mai e nevoie".to_string()),
                Done::Stopped => ("stopped", "oprit".to_string()),
                Done::Failed(e) => ("failed", e.clone()),
            };
            if let Done::Failed(e) = &done {
                log_warning!("convert: {} {} failed: {}", ascii_log(&title), ep.label(), ascii_log(e));
            }
            self.control().update(|b| {
                b.current = None;
                b.push(ep.label(), result, &note);
            });
            match done {
                Done::Converted | Done::DryRun => {
                    attempts += 1;
                    ok += 1;
                }
                Done::Failed(_) => {
                    attempts += 1;
                    failed += 1;
                }
                Done::NotNeeded => {}
                Done::Stopped => {
                    stopped = true;
                    break;
                }
            }
            let left = (req.count.saturating_sub(attempts)).min(queue.len());
            self.control().update(|b| b.remaining = left);
        }
        let (text, error) = summary(&title, ok, failed, stopped, self.app.dry_run);
        log_info!("convert: batch done: {ok} converted, {failed} failed{}", if stopped { ", stopped" } else { "" });
        self.finish(text, error);
    }

    /// Mark the batch finished, refresh the list, send ntfy (not when the app is stopping).
    fn finish(&self, text: String, error: bool) {
        self.control().update(|b| {
            b.running = false;
            b.current = None;
            b.remaining = 0;
            b.summary = Some(text.clone());
            b.finished_at = Some(unix_now());
        });
        self.control().invalidate_list();
        if self.stop.is_stopped() {
            return;
        }
        let tags = vec![if error { "warning".to_string() } else { "tv".to_string() }];
        self.rt.block_on(self.app.notifier.send("Conversie pentru TV", &text, Some(tags), None, crate::services::ntfy::TIMEOUT));
    }

    /// Convert one episode whose source probe is `src`.
    pub fn convert_one(&self, ep: &Episode, src: &Value, tools: &dyn Tools) -> Done {
        let file = Path::new(&ep.path);
        let Ok(before) = stamp(file) else { return Done::Failed("fișierul lipsește".into()) };
        if !probe_needs_conversion(src) {
            log_info!("convert: {} is no Hi10P any more, skipped", file.display());
            return Done::NotNeeded;
        }
        let size = fs::metadata(file).map(|m| m.len()).unwrap_or(0);
        if free_bytes(&self.cfg.tmp_dir).is_some_and(|free| (free as f64) < size as f64 * 1.1) {
            return Done::Failed("spațiu insuficient pe disc".into());
        }
        let original = self.find_original(file, ep);
        let tmp = tmp_path(&self.cfg.tmp_dir, file);
        if self.app.dry_run {
            let o = original.as_ref().map(|f| f.path.clone()).unwrap_or_else(|| "none".into());
            log_info!("convert (dry-run): would convert {} -> {} (torrent file: {o})", file.display(), tmp.display());
            return Done::DryRun;
        }
        if let Err(e) = fs::create_dir_all(&self.cfg.tmp_dir) {
            return Done::Failed(format!("dosarul temporar: {}", crate::state::io_error_name(&e)));
        }
        log_info!("convert: start {} ({:.1} GB)", file.display(), size as f64 / 1_073_741_824.0);
        let t0 = Instant::now();
        let total = duration(src);
        let label = ep.label();
        let mut last_pct = -1.0;
        let mut on_line = |l: &str| {
            if let Some(t) = parse_progress(l) {
                let p = percent(t, total);
                if p != last_pct {
                    last_pct = p;
                    self.control().update(|b| b.current = Some((label.clone(), p)));
                }
            }
        };
        let r = tools.encode(&ffmpeg_args(file, &tmp), &mut on_line, &|| self.stopped());
        let (code, lines) = match r {
            Ok(x) => x,
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return if self.stopped() { Done::Stopped } else { Done::Failed(format!("ffmpeg: {e}")) };
            }
        };
        for l in &lines {
            log_info!("convert:   ffmpeg: {}", ascii_log(l));
        }
        if code != 0 || !tmp.is_file() {
            let _ = fs::remove_file(&tmp);
            let last = lines.last().map(|l| format!(": {l}")).unwrap_or_default();
            return Done::Failed(format!("ffmpeg a eșuat ({code}){last}"));
        }
        let checked = tools.probe(&tmp).and_then(|out| verify(src, &out));
        if let Err(e) = checked {
            let _ = fs::remove_file(&tmp);
            return Done::Failed(format!("verificare eșuată: {e}"));
        }
        if let Err(d) = self.install(&tmp, file, &before, ep) {
            return d;
        }
        let converted_size = fs::metadata(file).map(|m| m.len()).unwrap_or(0);
        self.record(Entry {
            library: ep.path.clone(),
            original: original.as_ref().map(|f| f.path.clone()),
            torrent: original.as_ref().and_then(|f| f.torrent.clone()),
            original_size: original.as_ref().map(|f| f.size).unwrap_or(size),
            converted_size,
            converted_at: unix_now(),
            series_id: ep.series_id,
            series: ep.series_title.clone(),
            episode_id: ep.episode_id,
            season: ep.season,
            episode: ep.number,
        });
        log_info!(
            "convert: done in {:.1} min: {} ({:.2} -> {:.2} GB)",
            t0.elapsed().as_secs_f64() / 60.0,
            file.display(),
            size as f64 / 1_073_741_824.0,
            converted_size as f64 / 1_073_741_824.0
        );
        self.update_apps(ep.series_id, file);
        Done::Converted
    }

    /// Put the verified `tmp` in place of `file` with one rename, only while `file` still has
    /// the stamp `before` and is not playing; retried while it is locked. On failure the temp
    /// file is deleted and the library file stays.
    fn install(&self, tmp: &Path, file: &Path, before: &str, ep: &Episode) -> Result<(), Done> {
        let cross = self.cfg.force_cross_volume || !same_volume(tmp, file);
        let src = if cross {
            let mut name = file.file_name().unwrap_or_default().to_os_string();
            name.push(".partial");
            let partial = file.with_file_name(name);
            let want = fs::metadata(tmp).map(|m| m.len()).ok();
            let copied = fs::copy(tmp, &partial).ok();
            if want.is_none() || copied != want || fs::metadata(&partial).map(|m| m.len()).ok() != want {
                let _ = fs::remove_file(&partial);
                let _ = fs::remove_file(tmp);
                return Err(Done::Failed("copierea pe discul bibliotecii a eșuat".into()));
            }
            partial
        } else {
            tmp.to_path_buf()
        };
        let cleanup = || {
            let _ = fs::remove_file(&src);
            let _ = fs::remove_file(tmp);
        };
        for i in 0..self.cfg.replace_tries {
            if self.stopped() {
                cleanup();
                return Err(Done::Stopped);
            }
            if stamp(file).ok().as_deref() != Some(before) {
                cleanup();
                return Err(Done::Failed("fișierul s-a schimbat în timpul conversiei".into()));
            }
            let playing = self.playing().is_some_and(|p| p.has(ep));
            if !playing && fs::rename(&src, file).is_ok() {
                if cross {
                    let _ = fs::remove_file(tmp);
                }
                return Ok(());
            }
            if i + 1 < self.cfg.replace_tries && self.sleep(self.cfg.replace_wait) {
                cleanup();
                return Err(Done::Stopped);
            }
        }
        cleanup();
        Err(Done::Failed("fișierul e blocat (se redă?)".into()))
    }

    fn record(&self, e: Entry) {
        let path = ledger_path(self.app);
        match load_ledger(&path) {
            Ok(mut all) => {
                let key = path_key(&e.library);
                all.retain(|x| path_key(&x.library) != key);
                all.push(e);
                save_ledger(&path, &all);
            }
            Err(t) => log_warning!("convert: the ledger is unreadable ({t}), {} is not recorded (no revert)", e.library),
        }
    }

    /// The torrent file `file` is hardlinked to (see the module docs), or None.
    pub fn find_original(&self, file: &Path, ep: &Episode) -> Option<Found> {
        let id = file_id(file).ok()?;
        if id.links < 2 {
            return None;
        }
        let same = |p: &str| file_id(Path::new(p)).is_ok_and(|x| x.same_file(&id));
        let size = fs::metadata(file).map(|m| m.len()).unwrap_or(0);
        let arr = self.app.services.sonarr.arr();
        match self.rt.block_on(arr.series_history(ep.series_id, SONARR_IMPORTED)) {
            Ok(mut recs) => {
                recs.retain(|r| {
                    r.get("eventType").and_then(Value::as_str).is_none_or(|t| t.eq_ignore_ascii_case("downloadFolderImported"))
                        && id_of(r.get("episodeId")) == Some(ep.episode_id)
                });
                recs.sort_by_key(|r| std::cmp::Reverse(r.get("date").and_then(Value::as_str).unwrap_or("").to_string()));
                for r in recs {
                    let Some(p) = r.get("data").and_then(|d| d.get("droppedPath")).and_then(Value::as_str) else { continue };
                    if same(p) {
                        let torrent = r.get("downloadId").and_then(Value::as_str).map(|h| h.trim().to_ascii_lowercase()).filter(|h| !h.is_empty());
                        return Some(Found { path: p.to_string(), torrent, size });
                    }
                }
            }
            Err(e) => log_info!("convert: Sonarr history failed ({}), looking in qBittorrent", e.type_name()),
        }
        let qbit = QbitJobs::new(self.app.services.qbittorrent.c.clone());
        let torrents = self.rt.block_on(qbit.torrents()).ok()?;
        for t in torrents {
            let s = |k: &str| t.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let (hash, save) = (s("hash").trim().to_ascii_lowercase(), s("save_path"));
            if hash.is_empty() || t.get("progress").and_then(Value::as_f64).unwrap_or(0.0) < 1.0 || !under_root(&save, &self.cfg.downloads_root) {
                continue;
            }
            let Ok(files) = self.rt.block_on(qbit.files_sized(&hash)) else { continue };
            for (name, sz) in files {
                let full = format!("{}\\{}", save.trim_end_matches(['\\', '/']), name.replace('/', "\\"));
                if sz == size && same(&full) {
                    return Some(Found { path: full, torrent: Some(hash), size });
                }
            }
        }
        log_info!("convert: {} has {} links but no torrent file was found", file.display(), id.links);
        None
    }

    /// `path` is a file of a torrent in qBittorrent (the hinted torrent first, then every
    /// torrent whose save path holds it).
    async fn in_qbit(&self, qbit: &QbitJobs, path: &str, hint: Option<&str>) -> Result<bool, String> {
        let torrents = qbit.torrents().await.map_err(|e| e.type_name().to_string())?;
        let key = path_key(path);
        let mut list: Vec<(String, String)> = torrents
            .iter()
            .filter_map(|t| Some((t.get("hash")?.as_str()?.trim().to_ascii_lowercase(), t.get("save_path")?.as_str()?.to_string())))
            .filter(|(_, sp)| { let sp = path_key(sp); !sp.is_empty() && key.starts_with(&format!("{sp}\\")) })
            .collect();
        list.sort_by_key(|(h, _)| Some(h.as_str()) != hint);
        for (hash, save) in list {
            let names = qbit.files(&hash).await.map_err(|e| e.type_name().to_string())?;
            if names.iter().any(|n| path_key(&format!("{}\\{}", save.trim_end_matches(['\\', '/']), n.replace('/', "\\"))) == key) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The revert pass (see the module docs).
    pub fn revert_pass(&self) {
        let path = ledger_path(self.app);
        let entries = match load_ledger(&path) {
            Ok(e) => e,
            Err(t) => {
                log_warning!("convert: the ledger is unreadable ({t}), revert skipped");
                return;
            }
        };
        if entries.is_empty() {
            return;
        }
        let Ok(views) = self.rt.block_on(plex_views(self.app, self.cfg.section)) else { return };
        let playing = match self.rt.block_on(self.app.services.plex.sessions_raw()) {
            Ok(v) => playing_from_sessions(v.as_ref()),
            Err(_) => return,
        };
        let qbit = QbitJobs::new(self.app.services.qbittorrent.c.clone());
        let mut keep = Vec::new();
        let mut changed = false;
        for e in entries {
            if self.stop.is_stopped() {
                keep.push(e);
                continue;
            }
            let lib_size = fs::metadata(&e.library).ok().filter(|m| m.is_file()).map(|m| m.len());
            let watched = views.get(&path_key(&e.library)).map(|n| *n > 0);
            let is_playing = playing.contains(&e.library, &e.series, e.season, e.episode);
            let original = match &e.original {
                None => Original::NotRecorded,
                Some(o) => match fs::metadata(o) {
                    Ok(m) if m.is_file() && m.len() == e.original_size => {
                        let asked = watched == Some(true) && !is_playing;
                        let in_torrent = if asked { self.rt.block_on(self.in_qbit(&qbit, o, e.torrent.as_deref())).ok() } else { None };
                        Original::Present { in_torrent }
                    }
                    _ => Original::Gone,
                },
            };
            let facts = Facts { library_ok: lib_size == Some(e.converted_size), watched, playing: is_playing, original };
            match revert_decision(&facts) {
                Decision::Keep => keep.push(e),
                Decision::Drop(why) => {
                    log_info!("convert: forgetting {} ({why}), the converted file stays", e.library);
                    changed = true;
                }
                Decision::Revert if self.app.dry_run => {
                    log_info!("convert (dry-run): would put the torrent file back in place of {}", e.library);
                    keep.push(e);
                }
                Decision::Revert => match revert_file(Path::new(&e.library), Path::new(e.original.as_deref().unwrap_or(""))) {
                    Ok(()) => {
                        log_info!("convert: watched, {} is the torrent file again", e.library);
                        self.update_apps(e.series_id, Path::new(&e.library));
                        changed = true;
                    }
                    Err(why) => {
                        log_warning!("convert: cannot put the torrent file back for {}: {why}", e.library);
                        keep.push(e);
                    }
                },
            }
        }
        if changed {
            save_ledger(&path, &keep);
            self.control().invalidate_list();
        }
    }

    /// Sonarr `RescanSeries` and a Plex refresh of the file's folder. Failures are logged.
    fn update_apps(&self, series_id: i64, file: &Path) {
        let dir = file.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
        let app = self.app;
        self.rt.block_on(async {
            if let Err(e) = app.services.sonarr.arr().command(&json!({"name": "RescanSeries", "seriesId": series_id})).await {
                log_warning!("convert: Sonarr rescan of {series_id} failed: {}", e.type_name());
            }
            if app.services.keys.get("plex").is_empty() {
                return;
            }
            if let Err(e) = PlexJobs::new(app.services.plex.c.clone()).refresh_path(self.cfg.section, &dir).await {
                log_warning!("convert: Plex refresh failed: {}", e.type_name());
            }
        });
    }
}

/// Make `library` a hard link to `original` again: a new link next to it, then one rename
/// over the converted file. `Err` says why not (the converted file stays).
pub fn revert_file(library: &Path, original: &Path) -> Result<(), String> {
    let mut name = library.file_name().unwrap_or_default().to_os_string();
    name.push(".revert.tmp");
    let tmp = library.with_file_name(name);
    let _ = fs::remove_file(&tmp);
    fs::hard_link(original, &tmp).map_err(|e| format!("hard link: {}", crate::state::io_error_name(&e)))?;
    let same = match (file_id(&tmp), file_id(original)) {
        (Ok(a), Ok(b)) => a.same_file(&b),
        _ => false,
    };
    if !same {
        let _ = fs::remove_file(&tmp);
        return Err("the new link is not the torrent file".into());
    }
    if let Err(e) = fs::rename(&tmp, library) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("rename: {} (playing?)", crate::state::io_error_name(&e)));
    }
    Ok(())
}

/// Start the job on its own thread: batches on request, the revert pass every 5 minutes.
pub fn spawn(app: Arc<AppState>, stop: Stop) {
    let rt = Handle::current();
    let r = std::thread::Builder::new().name("convert".into()).spawn(move || {
        let mut s = stop.clone();
        let mut next_revert = Instant::now() + FIRST_RUN;
        loop {
            let wait = next_revert.saturating_duration_since(Instant::now());
            let stopping = rt.block_on(async {
                tokio::select! {
                    _ = app.convert.wake.notified() => false,
                    st = s.sleep(wait) => st,
                }
            });
            if stopping || stop.is_stopped() {
                return;
            }
            let job = Job { app: &app, rt: rt.clone(), cfg: Settings::from_app(&app), stop: &stop };
            if let Some(req) = app.convert.take_request() {
                let tools = ffmpeg_bin(&job.cfg.ffmpeg_bin, &job.cfg.ffmpeg_packages).map(|bin| Ffmpeg { bin });
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.run_batch(&req, tools.as_ref().map(|t| t as &dyn Tools))));
                if r.is_err() {
                    log_warning!("convert: batch failed (panic)");
                    if app.convert.running() {
                        job.finish(format!("Conversie eșuată ({}): eroare internă.", req.title), true);
                    }
                }
            }
            if Instant::now() >= next_revert {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.revert_pass()));
                if r.is_err() {
                    log_warning!("convert: revert pass failed (panic)");
                }
                next_revert = Instant::now() + INTERVAL;
            }
        }
    });
    if r.is_err() {
        log_warning!("convert: cannot start its thread");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::testutil::{app, fake, ok, status, temp_dir, Fake, Req};
    use std::sync::Mutex as StdMutex;

    fn file(id: i64, path: &str, codec: &str, depth: i64) -> Value {
        json!({"id": id, "path": path, "size": 100, "mediaInfo": {"videoCodec": codec, "videoBitDepth": depth}})
    }

    fn ep(id: i64, season: i64, number: i64, file_id: i64) -> Value {
        json!({"id": id, "seasonNumber": season, "episodeNumber": number, "episodeFileId": file_id, "hasFile": file_id != 0})
    }

    #[test]
    fn the_rule() {
        assert!(tv_needs_conversion("x264", 10));
        assert!(tv_needs_conversion("H264", 10));
        assert!(tv_needs_conversion("AVC", 12));
        assert!(!tv_needs_conversion("x264", 8));
        assert!(!tv_needs_conversion("x265", 10));
        assert!(!tv_needs_conversion("hevc", 10));
        assert!(!tv_needs_conversion("", 0));
        assert!(file_needs_conversion(&file(1, "a", "x264", 10)));
        assert!(!file_needs_conversion(&json!({"id": 1, "path": "a"})));
        let probe = |codec: &str, pf: &str, bits: Value| json!({"streams": [{"codec_type": "video", "codec_name": codec, "pix_fmt": pf, "bits_per_raw_sample": bits}]});
        assert!(probe_needs_conversion(&probe("h264", "yuv420p10le", Value::Null)));
        assert!(probe_needs_conversion(&probe("h264", "yuv420p", json!("10"))));
        assert!(!probe_needs_conversion(&probe("h264", "yuv420p", json!("8"))));
        assert!(!probe_needs_conversion(&probe("hevc", "yuv420p10le", Value::Null)));
        assert!(!probe_needs_conversion(&json!({"streams": []})));
    }

    fn show() -> Value {
        json!({"id": 4, "title": "Show"})
    }

    #[test]
    fn candidates_order_and_filters() {
        let files = vec![
            file(1, "D:\\S\\e1.mkv", "x264", 10),
            file(2, "D:\\S\\e2.mkv", "x264", 10),
            file(3, "D:\\S\\e3.mkv", "h264", 8),
            file(4, "D:\\S\\e4.mkv", "x264", 10),
            file(5, "D:\\S\\e5-6.mkv", "x264", 10),
            file(6, "D:\\S\\sp.mkv", "x264", 10),
            file(7, "D:\\S\\s2e1.mkv", "x264", 10),
        ];
        let eps = vec![
            ep(17, 2, 1, 7),
            ep(10, 0, 1, 6),
            ep(11, 1, 1, 1),
            ep(12, 1, 2, 2),
            ep(13, 1, 3, 3),
            ep(14, 1, 4, 4),
            ep(16, 1, 6, 5),
            ep(15, 1, 5, 5),
            ep(18, 1, 7, 0),
        ];
        let mut views = HashMap::new();
        views.insert(path_key("d:/s/E1.mkv"), 2); // watched
        views.insert(path_key("D:\\S\\e2.mkv"), 0);
        let converted: HashSet<String> = [path_key("D:\\S\\e4.mkv")].into_iter().collect();
        let c = candidates(&show(), &eps, &files, &views, &converted);
        let labels: Vec<String> = c.iter().map(Episode::label).collect();
        // e1 watched, e3 8-bit, e4 converted, e5-6 listed once, e7 has no file; specials last.
        assert_eq!(labels, ["S01E02", "S01E05", "S02E01", "S00E01"]);
        assert_eq!((c[0].series_id, c[0].series_title.as_str(), c[0].episode_id, c[0].size), (4, "Show", 12, 100));
    }

    fn episode(n: i64) -> Episode {
        Episode { series_id: 4, series_title: "Show".into(), episode_id: n, season: 1, number: n, path: format!("D:\\S\\e{n}.mkv"), size: 1 }
    }

    #[test]
    fn pick_skips_playing_and_respects_the_count() {
        let c: Vec<Episode> = (1..=5).map(episode).collect();
        let (take, skipped) = pick(&c, &|e| e.number == 2, 2);
        assert_eq!(take.iter().map(|e| e.number).collect::<Vec<_>>(), [1, 3]);
        assert_eq!(skipped.iter().map(|e| e.number).collect::<Vec<_>>(), [2]);
        let (take, _) = pick(&c, &|_| false, 3);
        assert_eq!(take.iter().map(|e| e.number).collect::<Vec<_>>(), [1, 2, 3]);
        let (take, skipped) = pick(&c, &|_| true, 2);
        assert!(take.is_empty() && skipped.len() == 5);
        let (take, _) = pick(&c, &|_| false, 9);
        assert_eq!(take.len(), 5);
        // Playing by session: the file path, or show + season + episode.
        let raw = json!({"MediaContainer": {"Metadata": [
            {"type": "episode", "grandparentTitle": "Show", "parentIndex": 1, "index": 4},
            {"type": "movie", "Media": [{"Part": [{"file": "d:/s/E3.mkv"}]}]}
        ]}});
        let p = playing_from_sessions(Some(&raw));
        assert!(p.has(&episode(3)) && p.has(&episode(4)) && !p.has(&episode(1)));
        assert_eq!(playing_from_sessions(None), Playing::default());
    }

    #[test]
    fn ffmpeg_command_and_progress() {
        let a = ffmpeg_args(Path::new("D:\\S\\e1.mkv"), Path::new("D:\\T\\e1.hevc.mkv"));
        assert_eq!(
            a.join(" "),
            "-hide_banner -loglevel error -y -progress pipe:1 -nostats -i D:\\S\\e1.mkv -map 0 -c copy -c:v hevc_amf -profile:v main10 \
             -pix_fmt p010le -quality quality -rc cqp -qp_i 19 -qp_p 21 -qp_b 23 -color_primaries bt709 -color_trc bt709 \
             -colorspace bt709 -color_range tv -bsf:v hevc_metadata=colour_primaries=1:transfer_characteristics=1:matrix_coefficients=1:video_full_range_flag=0 \
             -tag:v hvc1 D:\\T\\e1.hevc.mkv"
        );
        assert_eq!(tmp_path(Path::new("D:\\T"), Path::new("D:\\S\\Show - S01E01.mkv")), PathBuf::from("D:\\T\\Show - S01E01.hevc.mkv"));
        assert_eq!(parse_progress("out_time_us=62500000"), Some(62.5));
        assert_eq!(parse_progress("out_time_ms=1000000"), Some(1.0));
        assert_eq!(parse_progress("out_time=00:01:02.500000"), Some(62.5));
        assert_eq!(parse_progress("out_time=01:00:00.000000"), Some(3600.0));
        assert_eq!(parse_progress("out_time_us=N/A"), None);
        assert_eq!(parse_progress("out_time=N/A"), None);
        assert_eq!(parse_progress("progress=continue"), None);
        assert_eq!(parse_progress("frame=10"), None);
        assert_eq!(percent(62.5, 250.0), 25.0);
        assert_eq!(percent(300.0, 250.0), 100.0);
        assert_eq!(percent(10.0, 0.0), 0.0);
        assert_eq!(percent(1.0, 3.0), 33.3);
    }

    fn src_probe(codec: &str, dur: &str, audio: usize, subs: usize) -> Value {
        let mut streams = vec![json!({"index": 0, "codec_type": "video", "codec_name": codec, "pix_fmt": "yuv420p10le"})];
        for _ in 0..audio {
            streams.push(json!({"codec_type": "audio", "codec_name": "flac"}));
        }
        for _ in 0..subs {
            streams.push(json!({"codec_type": "subtitle", "codec_name": "ass"}));
        }
        streams.push(json!({"codec_type": "attachment", "codec_name": "ttf"}));
        json!({"streams": streams, "format": {"duration": dur}})
    }

    #[test]
    fn verification() {
        let src = src_probe("h264", "1420.100", 2, 3);
        assert_eq!(verify(&src, &src_probe("hevc", "1420.900", 2, 3)), Ok(()));
        assert!(verify(&src, &src_probe("h264", "1420.1", 2, 3)).is_err());
        assert!(verify(&src, &src_probe("hevc", "1418.0", 2, 3)).is_err());
        assert!(verify(&src, &src_probe("hevc", "1420.1", 1, 3)).is_err());
        assert!(verify(&src, &src_probe("hevc", "1420.1", 2, 2)).is_err());
        assert!(verify(&src_probe("h264", "0", 2, 3), &src_probe("hevc", "0", 2, 3)).is_err());
    }

    fn entry(lib: &str, orig: Option<&str>) -> Entry {
        Entry {
            library: lib.into(),
            original: orig.map(String::from),
            torrent: Some("abc".into()),
            original_size: 10,
            converted_size: 7,
            converted_at: 1_793_017_815.5,
            series_id: 4,
            series: "Show".into(),
            episode_id: 12,
            season: 1,
            episode: 2,
        }
    }

    #[test]
    fn ledger_round_trip() {
        let d = temp_dir("conv-ledger");
        let p = d.join(LEDGER_FILE);
        assert_eq!(load_ledger(&p), Ok(vec![]));
        let a = entry("D:\\S\\e2.mkv", Some("D:\\Downloads\\x\\e2.mkv"));
        let mut b = entry("D:\\S\\e3.mkv", None);
        b.torrent = None;
        save_ledger(&p, &[a.clone(), b.clone()]);
        let mut back = load_ledger(&p).unwrap();
        back.sort_by(|x, y| x.library.cmp(&y.library));
        assert_eq!(back, vec![a.clone(), b]);
        assert_eq!(back[0].label(), "S01E02");
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains("\"version\": 1") && text.contains("\"d:\\\\s\\\\e2.mkv\"") && text.ends_with("}\n"), "{text}");
        assert!(!crate::config::tmp_path(&p).exists());
        assert_eq!(converted_keys(&[a]), [path_key("d:/S/E2.mkv")].into_iter().collect());
        fs::write(&p, "{broken").unwrap();
        assert_eq!(load_ledger(&p), Err("JSONDecodeError".into()));
        fs::write(&p, "[]").unwrap();
        assert!(load_ledger(&p).is_err());
    }

    #[test]
    fn revert_decisions() {
        let present = |t: Option<bool>| Original::Present { in_torrent: t };
        let f = |library_ok, watched, playing, original| Facts { library_ok, watched, playing, original };
        assert_eq!(revert_decision(&f(true, Some(true), false, present(Some(true)))), Decision::Revert);
        assert!(matches!(revert_decision(&f(true, Some(true), false, Original::Gone)), Decision::Drop(_)));
        assert!(matches!(revert_decision(&f(true, Some(true), false, Original::NotRecorded)), Decision::Drop(_)));
        assert!(matches!(revert_decision(&f(true, Some(true), false, present(Some(false)))), Decision::Drop(_)));
        assert_eq!(revert_decision(&f(true, Some(true), false, present(None))), Decision::Keep);
        assert_eq!(revert_decision(&f(true, Some(false), false, present(Some(true)))), Decision::Keep);
        assert_eq!(revert_decision(&f(true, None, false, Original::Gone)), Decision::Keep);
        assert_eq!(revert_decision(&f(true, Some(true), true, present(Some(true)))), Decision::Keep);
        assert!(matches!(revert_decision(&f(false, Some(false), false, present(Some(true)))), Decision::Drop(_)));
    }

    #[test]
    fn texts() {
        assert_eq!(episodes_ro(1), "1 episod");
        assert_eq!(episodes_ro(3), "3 episoade");
        assert_eq!(episodes_ro(20), "20 de episoade");
        assert_eq!(summary("Demon Slayer", 3, 0, false, false), ("Conversie gata: 3 episoade (Demon Slayer)".into(), false));
        assert_eq!(summary("X", 2, 1, false, false), ("Conversie gata: 2 episoade, 1 eșuat (X)".into(), true));
        assert_eq!(summary("X", 0, 2, false, false), ("Conversie eșuată: 2 episoade (X)".into(), true));
        assert_eq!(summary("X", 1, 0, true, false), ("Conversie oprită după 1 episod (X)".into(), false));
        assert_eq!(summary("X", 0, 0, true, false).0, "Conversie oprită (X)");
        assert!(summary("X", 2, 0, false, true).0.starts_with("Probă"));
    }

    // -- the job against fake services and a fake ffmpeg ----------------------------------

    /// Probes by file name (`*.hevc.mkv`: `out`); encode writes the output and reports progress.
    struct FakeTools {
        out: Value,
        exit: i32,
        calls: StdMutex<Vec<Vec<String>>>,
        /// Press Stop during this encode (1-based): ffmpeg is "killed".
        stop_on: Option<(usize, Arc<AppState>)>,
    }

    impl Tools for FakeTools {
        fn probe(&self, file: &Path) -> Result<Value, String> {
            let name = file.file_name().unwrap().to_string_lossy().to_string();
            Ok(if name.ends_with(TMP_SUFFIX) { self.out.clone() } else { src_probe("h264", "100.0", 2, 1) })
        }

        fn encode(&self, args: &[String], on_line: &mut dyn FnMut(&str), stop: &dyn Fn() -> bool) -> Result<(i32, Vec<String>), String> {
            let n = {
                let mut c = self.calls.lock().unwrap();
                c.push(args.to_vec());
                c.len()
            };
            fs::write(args.last().unwrap(), b"encoded").unwrap();
            on_line("out_time_us=50000000");
            if let Some((at, a)) = &self.stop_on {
                if *at == n {
                    assert!(a.convert.request_stop());
                }
            }
            if stop() {
                return Err("oprit".into());
            }
            on_line("progress=end");
            Ok((self.exit, vec![]))
        }
    }

    fn tools(exit: i32) -> FakeTools {
        FakeTools { out: src_probe("hevc", "100.4", 2, 1), exit, calls: StdMutex::new(vec![]), stop_on: None }
    }

    const HASH: &str = "abcdef0123456789abcdef0123456789abcdef01";

    /// A library of 4 episodes; E1, E2, E4 are hard links to the torrent files, E3 a copy.
    struct World {
        root: PathBuf,
        lib: PathBuf,
        downloads: PathBuf,
        views: StdMutex<Vec<(String, i64)>>,
        sessions: StdMutex<Value>,
    }

    impl World {
        fn new() -> Arc<World> {
            let root = temp_dir("conv");
            let lib = root.join("Seriale").join("Show {tvdb-77}").join("Season 01");
            let downloads = root.join("Downloads");
            let tor = downloads.join("sonarr").join("Show.S01");
            fs::create_dir_all(&lib).unwrap();
            fs::create_dir_all(&tor).unwrap();
            for n in 1..=4 {
                let t = tor.join(format!("Show.S01E0{n}.mkv"));
                fs::write(&t, format!("original-{n}")).unwrap();
                let l = lib.join(format!("Show - S01E0{n}.mkv"));
                if n == 3 {
                    fs::copy(&t, &l).unwrap();
                } else {
                    fs::hard_link(&t, &l).unwrap();
                }
            }
            Arc::new(World { root, lib, downloads, views: StdMutex::new(vec![]), sessions: StdMutex::new(json!({"MediaContainer": {"size": 0}})) })
        }

        fn lib_file(&self, n: i64) -> PathBuf {
            self.lib.join(format!("Show - S01E0{n}.mkv"))
        }

        fn torrent_file(&self, n: i64) -> PathBuf {
            self.downloads.join("sonarr").join("Show.S01").join(format!("Show.S01E0{n}.mkv"))
        }

        fn watch(&self, n: i64) {
            self.views.lock().unwrap().push((self.lib_file(n).display().to_string(), 1));
        }

        fn play(&self, v: Value) {
            *self.sessions.lock().unwrap() = v;
        }
    }

    async fn serve(w: Arc<World>) -> Fake {
        fake(move |r: &Req| {
            let p = r.path.as_str();
            let files: Vec<Value> = (1..=4).map(|n| file(1000 + n, &w.lib_file(n).display().to_string(), "x264", 10)).collect();
            if p == "/api/v3/series/4" {
                return ok(show());
            }
            if p == "/api/v3/series/5" {
                return status(404);
            }
            if p == "/api/v3/series" {
                return ok(json!([show(), {"id": 5, "title": "Empty", "statistics": {"episodeFileCount": 0}}]));
            }
            if p.starts_with("/api/v3/episodefile") {
                return ok(json!(files));
            }
            if p.starts_with("/api/v3/episode?") {
                return ok(json!((1..=4).map(|n| ep(100 + n, 1, n, 1000 + n)).collect::<Vec<_>>()));
            }
            if p.starts_with("/api/v3/history/series") {
                // E1's record points at a file that is not its library file (found via qBittorrent).
                return ok(json!([
                    {"eventType": "downloadFolderImported", "episodeId": 102, "downloadId": HASH.to_uppercase(), "date": "2026-10-01T00:00:00Z",
                     "data": {"droppedPath": w.torrent_file(2).display().to_string()}},
                    {"eventType": "downloadFolderImported", "episodeId": 101, "downloadId": "OTHER", "date": "2026-10-01T00:00:00Z",
                     "data": {"droppedPath": w.torrent_file(3).display().to_string()}}
                ]));
            }
            if p == "/api/v3/command" {
                return ok(json!({}));
            }
            if p.starts_with("/library/sections/2/all") {
                let meta: Vec<Value> = w.views.lock().unwrap().iter().map(|(f, v)| json!({"viewCount": v, "Media": [{"Part": [{"file": f}]}]})).collect();
                return ok(json!({"MediaContainer": {"Metadata": meta}}));
            }
            if p == "/status/sessions" {
                return ok(w.sessions.lock().unwrap().clone());
            }
            if p.starts_with("/library/sections/2/refresh") {
                return ok(json!({}));
            }
            if p == "/api/v2/torrents/info" {
                return ok(json!([{"hash": HASH, "progress": 1.0, "save_path": w.downloads.join("sonarr").display().to_string()}]));
            }
            if p.starts_with("/api/v2/torrents/files") {
                return ok(json!((1..=4).map(|n| json!({"name": format!("Show.S01/Show.S01E0{n}.mkv"), "size": 10})).collect::<Vec<_>>()));
            }
            if p == "/" {
                return ok(json!({})); // ntfy
            }
            status(404)
        })
        .await
    }

    struct Rig {
        f: Fake,
        a: Arc<AppState>,
        w: Arc<World>,
    }

    async fn rig(dry: bool) -> Rig {
        let w = World::new();
        let f = serve(w.clone()).await;
        let extra = json!({"jobs": {"convert": {"tmp_dir": w.root.join("tmp").display().to_string()},
                                    "seed_keeper": {"downloads_root": w.downloads.display().to_string()}}});
        let (a, _d) = app(&f.base, dry, extra);
        Rig { f, a, w }
    }

    fn settings(a: &AppState) -> Settings {
        let mut s = Settings::from_app(a);
        s.replace_wait = Duration::from_millis(1);
        s.replace_tries = 3;
        s
    }

    async fn run_with(r: &Rig, req: Request, t: Arc<FakeTools>, cross: bool) {
        let a = r.a.clone();
        let rt = Handle::current();
        tokio::task::spawn_blocking(move || {
            let (_h, stop) = crate::jobs::stop_pair();
            let mut cfg = settings(&a);
            cfg.force_cross_volume = cross;
            let job = Job { app: &a, rt, cfg, stop: &stop };
            job.run_batch(&req, Some(&*t as &dyn Tools));
        })
        .await
        .unwrap();
    }

    async fn run(r: &Rig, req: Request, t: Arc<FakeTools>) {
        run_with(r, req, t, false).await;
    }

    async fn revert(r: &Rig) {
        let a = r.a.clone();
        let rt = Handle::current();
        tokio::task::spawn_blocking(move || {
            let (_h, stop) = crate::jobs::stop_pair();
            Job { app: &a, rt, cfg: settings(&a), stop: &stop }.revert_pass();
        })
        .await
        .unwrap();
    }

    async fn start(r: &Rig, count: i64) -> (u16, Value) {
        let body = json!({"seriesId": 4, "count": count});
        api_start(&r.a, body.as_object().unwrap()).await
    }

    fn req(count: usize) -> Request {
        Request { series_id: 4, title: "Show".into(), count }
    }

    /// `begin` + `take_request`, as the route and the job thread do.
    fn begun(r: &Rig, count: usize) -> Request {
        assert!(r.a.convert.begin(req(count), r.a.dry_run));
        r.a.convert.take_request().unwrap()
    }

    fn ledger(r: &Rig) -> Vec<Entry> {
        let mut l = load_ledger(&ledger_path(&r.a)).unwrap();
        l.sort_by(|x, y| x.library.cmp(&y.library));
        l
    }

    fn tmp_is_empty(r: &Rig) -> bool {
        fs::read_dir(r.w.root.join("tmp")).map(|mut d| d.next().is_none()).unwrap_or(true)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn batch_converts_in_order_then_reverts_after_watching() {
        let r = rig(false).await;
        r.w.watch(1);
        let (code, out) = start(&r, 2).await;
        assert_eq!(code, 200, "{out}");
        assert_eq!(out["job"]["running"], json!(true));
        let rq = r.a.convert.take_request().unwrap();
        assert_eq!(rq, req(2));
        let t = Arc::new(tools(0));
        run(&r, rq, t.clone()).await;
        // E1 is watched: E2 (hardlinked) and E3 (a plain copy) are converted, E4 is left.
        assert_eq!(t.calls.lock().unwrap().len(), 2);
        for n in [2, 3] {
            assert_eq!(fs::read(r.w.lib_file(n)).unwrap(), b"encoded");
            assert_eq!(fs::read(r.w.torrent_file(n)).unwrap(), format!("original-{n}").as_bytes(), "the torrent copy is untouched");
        }
        assert_eq!(fs::read(r.w.lib_file(4)).unwrap(), b"original-4");
        assert!(tmp_is_empty(&r), "no temp file left");
        let b = r.a.convert.batch().unwrap();
        assert!(!b.running && b.current.is_none());
        assert_eq!(b.items.iter().map(|i| (i.label.as_str(), i.result)).collect::<Vec<_>>(), [("S01E02", "ok"), ("S01E03", "ok")]);
        assert_eq!(b.summary.as_deref(), Some("Conversie gata: 2 episoade (Show)"));
        // The ledger: E2 points at its torrent file (from the Sonarr history), E3 at none.
        let l = ledger(&r);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].original.as_deref(), Some(r.w.torrent_file(2).display().to_string().as_str()));
        assert_eq!((l[0].torrent.as_deref(), l[0].original_size, l[0].converted_size), (Some(HASH), 10, 7));
        assert_eq!(l[1].original, None);
        let cmds: Vec<Value> = r.f.find("POST", "/api/v3/command").iter().map(Req::json).collect();
        assert_eq!(cmds, vec![json!({"name": "RescanSeries", "seriesId": 4}); 2]);
        assert_eq!(r.f.find("GET", "/library/sections/2/refresh").len(), 2);
        let ntfy: Vec<Value> = r.f.find("POST", "/").iter().filter(|q| q.path == "/").map(Req::json).collect();
        assert_eq!(ntfy.len(), 1);
        assert_eq!(ntfy[0]["message"], json!("Conversie gata: 2 episoade (Show)"));
        // Converted files are not offered again: only E4 is left.
        let list = list_series(&r.a).await.unwrap();
        assert_eq!(list, vec![json!({"id": 4, "title": "Show", "pending": 1, "next": "S01E04"})]);

        // Nothing watched yet: kept.
        revert(&r).await;
        assert_eq!(ledger(&r).len(), 2);
        // E2 watched but playing: kept.
        r.w.watch(2);
        r.w.play(json!({"MediaContainer": {"Metadata": [{"type": "episode", "grandparentTitle": "Show", "parentIndex": 1, "index": 2}]}}));
        revert(&r).await;
        assert_eq!(ledger(&r).len(), 2);
        assert_eq!(fs::read(r.w.lib_file(2)).unwrap(), b"encoded");
        // Watched, not playing: E2 is the torrent file again, E3 (no original) stays converted.
        r.w.watch(3);
        r.w.play(json!({"MediaContainer": {"size": 0}}));
        revert(&r).await;
        assert!(ledger(&r).is_empty());
        assert_eq!(fs::read(r.w.lib_file(2)).unwrap(), b"original-2");
        let (x, y) = (file_id(&r.w.lib_file(2)).unwrap(), file_id(&r.w.torrent_file(2)).unwrap());
        assert!(x.same_file(&y) && x.links == 2, "a hard link again");
        assert_eq!(fs::read(r.w.lib_file(3)).unwrap(), b"encoded");
        assert!(!r.w.lib.join("Show - S01E02.mkv.revert.tmp").exists());
        assert_eq!(r.f.find("POST", "/api/v3/command").len(), 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn original_gone_keeps_the_converted_file() {
        let r = rig(false).await;
        run(&r, req(1), Arc::new(tools(0))).await;
        assert_eq!(fs::read(r.w.lib_file(1)).unwrap(), b"encoded");
        // E1's history record points at another file: found through qBittorrent instead.
        let l = ledger(&r);
        assert_eq!(l[0].original.as_deref(), Some(r.w.torrent_file(1).display().to_string().as_str()));
        assert_eq!(l[0].torrent.as_deref(), Some(HASH));
        // The seed keeper deleted the torrent with its files.
        fs::remove_file(r.w.torrent_file(1)).unwrap();
        r.w.watch(1);
        revert(&r).await;
        assert!(ledger(&r).is_empty());
        assert_eq!(fs::read(r.w.lib_file(1)).unwrap(), b"encoded");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn playing_episode_is_passed_over() {
        let r = rig(false).await;
        r.w.play(json!({"MediaContainer": {"Metadata": [{"type": "episode", "Media": [{"Part": [{"file": r.w.lib_file(1).display().to_string()}]}]}]}}));
        let rq = begun(&r, 2);
        run(&r, rq, Arc::new(tools(0))).await;
        assert_eq!(fs::read(r.w.lib_file(1)).unwrap(), b"original-1");
        assert_eq!(fs::read(r.w.lib_file(2)).unwrap(), b"encoded");
        assert_eq!(fs::read(r.w.lib_file(3)).unwrap(), b"encoded");
        assert_eq!(fs::read(r.w.lib_file(4)).unwrap(), b"original-4");
        let b = r.a.convert.batch().unwrap();
        assert_eq!(b.items.iter().map(|i| (i.label.as_str(), i.result)).collect::<Vec<_>>(), [("S01E01", "skipped"), ("S01E02", "ok"), ("S01E03", "ok")]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failures_keep_the_library_file() {
        // ffmpeg fails, then the verification fails: originals stay, temp files go.
        for (exit, out) in [(1, src_probe("hevc", "100", 2, 1)), (0, src_probe("hevc", "90", 2, 1))] {
            let r = rig(false).await;
            let rq = begun(&r, 2);
            let mut t = tools(exit);
            t.out = out;
            let t = Arc::new(t);
            run(&r, rq, t.clone()).await;
            assert_eq!(t.calls.lock().unwrap().len(), 2, "a failure counts towards the count");
            for n in 1..=4 {
                assert_eq!(fs::read(r.w.lib_file(n)).unwrap(), format!("original-{n}").as_bytes());
            }
            assert!(tmp_is_empty(&r));
            assert!(ledger(&r).is_empty());
            let b = r.a.convert.batch().unwrap();
            assert!(b.items.iter().all(|i| i.result == "failed"));
            assert_eq!(b.summary.as_deref(), Some("Conversie eșuată: 2 episoade (Show)"));
            assert!(r.f.find("POST", "/api/v3/command").is_empty());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stop_kills_ffmpeg_and_ends_the_batch() {
        // Stop pressed before the first episode starts.
        let r = rig(false).await;
        let rq = begun(&r, 3);
        assert!(r.a.convert.request_stop());
        let t = Arc::new(tools(0));
        run(&r, rq, t.clone()).await;
        assert!(t.calls.lock().unwrap().is_empty());
        let b = r.a.convert.batch().unwrap();
        assert!(!b.running);
        assert_eq!(b.summary.as_deref(), Some("Conversie oprită (Show)"));
        // Stop pressed while ffmpeg runs on the second episode.
        let r = rig(false).await;
        let rq = begun(&r, 3);
        let mut t = tools(0);
        t.stop_on = Some((2, r.a.clone()));
        let t = Arc::new(t);
        run(&r, rq, t.clone()).await;
        assert_eq!(t.calls.lock().unwrap().len(), 2);
        assert_eq!(fs::read(r.w.lib_file(1)).unwrap(), b"encoded");
        assert_eq!(fs::read(r.w.lib_file(2)).unwrap(), b"original-2");
        assert!(tmp_is_empty(&r), "the killed encode's temp file is deleted");
        let b = r.a.convert.batch().unwrap();
        assert_eq!(b.items.iter().map(|i| i.result).collect::<Vec<_>>(), ["ok", "stopped"]);
        assert_eq!(b.summary.as_deref(), Some("Conversie oprită după 1 episod (Show)"));
        assert!(!b.running);
        // A new batch may start after it.
        assert!(r.a.convert.begin(req(1), false));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dry_run_touches_nothing() {
        let r = rig(true).await;
        let t = Arc::new(tools(0));
        let rq = begun(&r, 2);
        run(&r, rq, t.clone()).await;
        assert!(t.calls.lock().unwrap().is_empty());
        for n in 1..=4 {
            assert_eq!(fs::read(r.w.lib_file(n)).unwrap(), format!("original-{n}").as_bytes());
        }
        assert!(!r.w.root.join("tmp").exists());
        assert!(!ledger_path(&r.a).exists());
        assert!(r.f.mutations().iter().all(|q| q.path == "/"), "only the (local) ntfy server");
        let b = r.a.convert.batch().unwrap();
        assert_eq!(b.items.iter().map(|i| i.result).collect::<Vec<_>>(), ["dry", "dry"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cross_volume_copies_to_partial_then_renames() {
        let r = rig(false).await;
        run_with(&r, req(1), Arc::new(tools(0)), true).await;
        assert_eq!(fs::read(r.w.lib_file(1)).unwrap(), b"encoded");
        assert!(!r.w.lib.join("Show - S01E01.mkv.partial").exists());
        assert!(tmp_is_empty(&r));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn start_validates_and_allows_one_batch() {
        let r = rig(false).await;
        for body in [
            json!({}),
            json!({"seriesId": 4}),
            json!({"seriesId": 4, "count": 0}),
            json!({"seriesId": 4, "count": 21}),
            json!({"seriesId": "4", "count": 2}),
            json!({"seriesId": 4, "count": true}),
            json!({"seriesId": -1, "count": 2}),
            json!({"seriesId": 4, "count": 2.5}),
        ] {
            let (code, out) = api_start(&r.a, body.as_object().unwrap()).await;
            assert_eq!((code, out), (400, json!({"error": "Cerere invalidă."})), "{body}");
        }
        let (code, out) = api_start(&r.a, json!({"seriesId": 5, "count": 2}).as_object().unwrap()).await;
        assert_eq!((code, out["error"].clone()), (404, json!("Serialul nu există în Sonarr.")));
        assert_eq!(api_stop(&r.a).0, 409);
        // More than there is: capped to the candidates.
        let (code, out) = start(&r, 20).await;
        assert_eq!((code, out["job"]["requested"].clone()), (200, json!(4)));
        let (code, out) = start(&r, 2).await;
        assert_eq!(code, 409, "{out}");
        let (code, out) = api_stop(&r.a);
        assert_eq!((code, out["job"]["stopping"].clone()), (200, json!(true)));
        assert!(r.a.convert.stop_requested());
        assert!(r.f.mutations().is_empty(), "the route converts nothing itself");
        let v = api_status(&r.a).await;
        assert_eq!(v["job"]["running"], json!(true));
        assert_eq!(v["series"][0]["pending"], json!(4));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn services_down_give_a_message() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let (a, _d) = app(&format!("http://127.0.0.1:{port}"), false, json!({}));
        let v = api_status(&a).await;
        assert_eq!((v["series"].clone(), v["error"].clone(), v["loading"].clone()), (Value::Null, json!("Sonarr nu răspunde."), json!(false)));
        let (code, out) = api_start(&a, json!({"seriesId": 4, "count": 2}).as_object().unwrap()).await;
        assert_eq!((code, out), (502, json!({"error": "Sonarr nu răspunde."})));
        // Plex down: the list says so.
        let w = World::new();
        let f = fake(move |r: &Req| {
            if r.path == "/api/v3/series" {
                ok(json!([show()]))
            } else if r.path.starts_with("/api/v3/episodefile") {
                ok(json!([file(1, &w.lib_file(1).display().to_string(), "x264", 10)]))
            } else {
                status(500)
            }
        })
        .await;
        let (a, _d) = app(&f.base, false, json!({}));
        assert_eq!(list_series(&a).await, Err(PLEX_DOWN.to_string()));
    }

    #[test]
    fn demo_has_data() {
        let v = demo_status();
        assert_eq!(v["series"].as_array().unwrap().len(), 2);
        assert_eq!(v["demo"], json!(true));
    }
}
