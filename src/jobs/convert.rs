//! "Conversie pentru TV" (Seriale tab and the movie sheet in Filme): episodes and movies the
//! Samsung TV cannot direct-play are re-encoded to HEVC Main10 on the AMD GPU (`hevc_amf`), on
//! request only. The rule is [`tv_needs_conversion`]: H.264 with more than 8 bits per sample
//! (Hi10P). Plex would transcode those to DASH and the TV buffers every ~25 s; HEVC Main10
//! direct-plays.
//!
//! ## What needs it
//! From Sonarr/Radarr only (episode files / the movie file with `mediaInfo.videoCodec` /
//! `videoBitDepth`), never an ffprobe of the library; "watched" is Plex's `viewCount` per file
//! path (section 2 for every episode, `jobs.convert.movie_section` (1) for movies); files in
//! the ledger are already converted. The Seriale grid and the series screen
//! (`crate::logic::series`) show it; the grid list is cached here ([`Control::listing`]: 60 s,
//! a failure 15 s, loaded in the background so a slow Sonarr never blocks the page).
//! `GET /api/convert` is only the batch status (cheap, polled by the Acasă banner).
//!
//! ## A batch (`POST /api/convert/start`)
//! `{seriesId, count}` (1..=20: the next unwatched ones), `{seriesId, episodeIds: [...]}`
//! (these, 1..=20, each must need it and be unwatched) or `{movieId}` (a Radarr movie).
//! One batch at a time, on the job's own OS thread, one episode at a time in episode order
//! (season, episode; specials last), from the first unwatched one. An episode playing in Plex
//! (a session's file path, or show + season + episode; a movie by path or title) is passed
//! over for the next one. Per
//! episode: ffprobe (still Hi10P? else it is skipped), find the torrent file the library file is
//! hardlinked to (below), ffmpeg in two passes (below-normal priority, no window): the encode
//! with exactly [`ffmpeg_args`] into `<stem>.hevc.enc.mkv`, then a stream-copy remux with
//! [`remux_args`] (`hevc_metadata`: BT.709 in the HEVC VUI; `hevc_amf` gives that filter no
//! extradata in one pass) into `<stem>.hevc.mkv`, both in the temp dir (the encode is deleted
//! after the remux). Verify the result (HEVC, BT.709 primaries and transfer, duration within
//! 1 s, as many audio and subtitle streams), then one rename over the library file while it still has the stamp it had
//! before (across volumes via `<file>.partial`; retried every 30 s for 10 min while locked or
//! playing; on failure the temp file is deleted and the library file stays). Then the ledger
//! entry, a Plex refresh of the folder and Sonarr `RescanSeries` (Radarr `RescanMovie` for a
//! movie: same command, checks, ledger and revert). Renaming over the library
//! path breaks its hardlink: the torrent copy stays byte-identical (FileList seeding). ntfy
//! when the batch ends (also after errors). A failed episode counts towards the count, so a
//! broken encoder never runs through the whole series.
//!
//! **Stop** (`POST /api/convert/stop`) kills ffmpeg (either pass) at once and deletes its temp files; the
//! library file is untouched and the batch ends.
//!
//! ## Finding the torrent file (before converting)
//! Only when the library file has more than one hard link. Candidates: Sonarr's (Radarr's for a
//! movie) `downloadFolderImported` history of the item (`data.droppedPath`, newest first), then
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
//!   rename that over the converted file, Plex refresh + `RescanSeries` / `RescanMovie`, drop
//!   the entry;
//! - watched but no original, or the original is gone (the seed keeper deleted the torrent):
//!   keep the converted file, drop the entry.
//!
//! Dry-run: no ffmpeg, no replace, no hard link; would-be actions go to the app log (the
//! ledger is the app's own file and is kept). Demo: fixed data, start/stop do nothing.

use crate::app::AppState;
use crate::jobs::common::{
    ffmpeg_bin, file_id, free_bytes, id_of, job_cfg, job_str, quiet_command, run_capture, run_lines, same_volume, unix_now, FileId,
};
use crate::jobs::seed_keeper::under_root;
use crate::jobs::strip_dv::{duration, stamp, DvConfig, Ffmpeg};
use crate::jobs::Stop;
use crate::services::arr_jobs::Arr;
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
/// `<stem>.hevc.enc.mkv` in the temp dir: the encode before the remux that writes the BT.709
/// VUI ([`remux_args`]).
pub const ENC_SUFFIX: &str = ".hevc.enc.mkv";
/// The progress bar: the encode is 0..=97 %, the remux shows [`REMUX_PERCENT`].
const ENCODE_SHARE: f64 = 97.0;
const REMUX_PERCENT: f64 = 99.0;
/// `<library file>.partial` (cross-volume install) and `<library file>.revert.tmp` (revert).
pub const PARTIAL_SUFFIX: &str = ".partial";
pub const REVERT_SUFFIX: &str = ".revert.tmp";
/// `state.local.json` key: the files the job is creating right now (deleted after a crash).
pub const LEFTOVERS_KEY: &str = "convert_leftovers";
/// The converted file may be this much shorter or longer than the source.
pub const DURATION_TOLERANCE: f64 = 1.0;
/// Sonarr's `EpisodeHistoryEventType.DownloadFolderImported` (Radarr's movie one is 3 too).
const ARR_IMPORTED: i64 = 3;
/// The Plex movie section when neither `jobs.convert.movie_section` nor strip_dv's Radarr
/// library says otherwise.
pub const DEFAULT_MOVIE_SECTION: u32 = 1;
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

/// One file to convert: an episode, or a movie (`movie_id` set, `series_title` is the movie's
/// title, the episode fields 0).
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
    /// A Radarr movie id (0 for an episode).
    pub movie_id: i64,
}

impl Episode {
    /// `S01E04`, or the movie title.
    pub fn label(&self) -> String {
        if self.movie_id != 0 {
            self.series_title.clone()
        } else {
            label(self.season, self.number)
        }
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
            movie_id: 0,
        });
    }
    out.sort_by_key(|e| (e.season == 0, e.season, e.number, e.episode_id));
    let mut seen = HashSet::new();
    out.retain(|e| seen.insert(path_key(&e.path)));
    out
}

/// What Plex is playing right now: file paths, (show, season, episode) and movie titles.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Playing {
    pub files: HashSet<String>,
    pub episodes: HashSet<(String, i64, i64)>,
    pub movies: HashSet<String>,
}

impl Playing {
    pub fn contains(&self, path: &str, show: &str, season: i64, number: i64) -> bool {
        self.files.contains(&path_key(path)) || self.episodes.contains(&(show.trim().to_lowercase(), season, number))
    }

    /// A movie: by its file path or its title.
    pub fn contains_movie(&self, path: &str, title: &str) -> bool {
        self.files.contains(&path_key(path)) || self.movies.contains(&title.trim().to_lowercase())
    }

    pub fn has(&self, e: &Episode) -> bool {
        if e.movie_id != 0 {
            self.contains_movie(&e.path, &e.series_title)
        } else {
            self.contains(&e.path, &e.series_title, e.season, e.number)
        }
    }

    /// A ledger entry's library file is playing.
    pub fn has_entry(&self, e: &Entry) -> bool {
        if e.movie_id != 0 {
            self.contains_movie(&e.library, &e.series)
        } else {
            self.contains(&e.library, &e.series, e.season, e.episode)
        }
    }
}

/// Plex `/status/sessions` JSON -> [`Playing`] (`Media[].Part[].file`; episodes also by
/// `grandparentTitle` + `parentIndex` + `index`, movies by `title`).
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
        if m.get("type").and_then(Value::as_str) == Some("movie") {
            let title = m.get("title").and_then(Value::as_str).unwrap_or("").trim().to_lowercase();
            if !title.is_empty() {
                p.movies.insert(title);
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

/// The BT.709 VUI the TV needs: the sources say "unknown" and `hevc_amf` writes BT.2020/PQ
/// into the bitstream, so the TV shows wrong colours.
const HEVC_METADATA: &str = "hevc_metadata=colour_primaries=1:transfer_characteristics=1:matrix_coefficients=1:video_full_range_flag=0";
const BT709_TAGS: [&str; 8] = ["-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709", "-color_range", "tv"];

/// Pass 1, the hand-tested encode (HEVC Main10 on the AMD encoder, BT.709 container tags,
/// everything else copied) into `<stem>.hevc.enc.mkv`, plus `-progress pipe:1 -nostats` for
/// the progress bar. No `-bsf:v` here: `hevc_amf` has no extradata when the filter starts
/// ("Failed to read extradata"), so the VUI is fixed in pass 2 ([`remux_args`]).
pub fn ffmpeg_args(input: &Path, enc: &Path) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-y", "-progress", "pipe:1", "-nostats", "-i"].map(String::from).to_vec();
    a.push(input.to_string_lossy().into_owned());
    a.extend(
        [
            "-map", "0", "-c", "copy", "-c:v", "hevc_amf", "-profile:v", "main10", "-pix_fmt", "p010le", "-quality", "quality",
            "-rc", "cqp", "-qp_i", "19", "-qp_p", "21", "-qp_b", "23",
        ]
        .map(String::from),
    );
    a.extend(BT709_TAGS.map(String::from));
    a.extend(["-tag:v", "hvc1"].map(String::from));
    a.push(enc.to_string_lossy().into_owned());
    a
}

/// Pass 2, a stream-copy remux of the encode that writes BT.709 into the HEVC VUI
/// (`hevc_metadata`) and the container tags (seconds, no progress).
pub fn remux_args(enc: &Path, out: &Path) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-y", "-i"].map(String::from).to_vec();
    a.push(enc.to_string_lossy().into_owned());
    a.extend(["-map", "0", "-c", "copy", "-bsf:v", HEVC_METADATA].map(String::from));
    a.extend(BT709_TAGS.map(String::from));
    a.push(out.to_string_lossy().into_owned());
    a
}

fn stem(file: &Path) -> String {
    file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// `<stem>.hevc.mkv` in the temp dir (the final, remuxed file).
pub fn tmp_path(tmp_dir: &Path, file: &Path) -> PathBuf {
    tmp_dir.join(format!("{}{TMP_SUFFIX}", stem(file)))
}

/// `<stem>.hevc.enc.mkv` in the temp dir (pass 1's output, deleted after the remux).
pub fn enc_path(tmp_dir: &Path, file: &Path) -> PathBuf {
    tmp_dir.join(format!("{}{ENC_SUFFIX}", stem(file)))
}

/// The temp dir must be a full path that does not overlap a library root, a torrent's
/// content (either way round) or equal a torrent's save path. `Err`: the message.
pub fn check_tmp_dir(tmp: &Path, overlap: &[String], equal: &[String]) -> Result<(), String> {
    let s = tmp.to_string_lossy().to_string();
    if s.trim().is_empty() || !tmp.is_absolute() {
        return Err("dosarul temporar (jobs.convert.tmp_dir) nu e o cale completă.".into());
    }
    let k = path_key(&s);
    let inside = |a: &str, b: &str| a == b || a.starts_with(&format!("{b}\\"));
    for p in overlap {
        let pk = path_key(p);
        if !pk.is_empty() && (inside(&k, &pk) || inside(&pk, &k)) {
            return Err(format!("dosarul temporar {s} se suprapune cu {p}."));
        }
    }
    if let Some(p) = equal.iter().find(|p| path_key(p) == k) {
        return Err(format!("dosarul temporar {s} e dosarul unui torrent ({p})."));
    }
    Ok(())
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

/// The converted file is usable: HEVC video tagged BT.709 (`color_primaries` and
/// `color_transfer`), the same length within 1 s, as many audio and subtitle streams as the
/// source. `Err` says why not (log text).
pub fn verify(src: &Value, out: &Value) -> Result<(), String> {
    let v = first_video(out);
    let field = |k: &str| v.and_then(|v| v.get(k)).and_then(Value::as_str).unwrap_or("");
    let codec = field("codec_name");
    if codec != "hevc" {
        return Err(format!("video {codec:?}, nu hevc"));
    }
    let (prim, trc) = (field("color_primaries"), field("color_transfer"));
    if prim != "bt709" || trc != "bt709" {
        return Err(format!("culori {prim:?}/{trc:?}, nu bt709"));
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
    /// The torrent file's identity when converted (NTFS volume serial, file index): another
    /// file put there later under the same name is never linked back.
    pub original_id: Option<(u64, u64)>,
    pub converted_size: u64,
    pub converted_at: f64,
    pub series_id: i64,
    /// The series title (the movie title for a movie).
    pub series: String,
    pub episode_id: i64,
    pub season: i64,
    pub episode: i64,
    /// A Radarr movie id (0 for an episode; entries from before movies were supported).
    pub movie_id: i64,
}

impl Entry {
    pub fn to_json(&self) -> Value {
        json!({
            "library": self.library, "original": self.original, "torrent": self.torrent,
            "original_size": self.original_size, "converted_size": self.converted_size, "converted_at": self.converted_at,
            "original_volume": self.original_id.map(|x| x.0), "original_index": self.original_id.map(|x| x.1),
            "series_id": self.series_id, "series": self.series, "episode_id": self.episode_id,
            "season": self.season, "episode": self.episode, "movie_id": self.movie_id,
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
            original_id: match (v.get("original_volume").and_then(Value::as_u64), v.get("original_index").and_then(Value::as_u64)) {
                (Some(a), Some(b)) => Some((a, b)),
                _ => None,
            },
            converted_size: u("converted_size"),
            converted_at: v.get("converted_at").and_then(Value::as_f64).unwrap_or(0.0),
            series_id: n("series_id"),
            series: s("series").unwrap_or_default(),
            episode_id: n("episode_id"),
            season: n("season"),
            episode: n("episode"),
            movie_id: n("movie_id"),
        })
    }

    pub fn label(&self) -> String {
        if self.movie_id != 0 {
            self.series.clone()
        } else {
            label(self.season, self.episode)
        }
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
    /// Missing (`NotFound`), or another size than recorded.
    Gone,
    /// Another file than the one converted from (NTFS file id differs, or none was recorded).
    Replaced,
    /// Could not be checked now (an error other than `NotFound`): try again next pass.
    Unknown,
    /// The same file with its size; `in_torrent`: still a file of a torrent in qBittorrent
    /// (`None`: qBittorrent did not answer, or it was not asked).
    Present { in_torrent: Option<bool> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Facts {
    /// The library file exists with the converted size (`None`: could not be checked now).
    pub library_ok: Option<bool>,
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
    match f.library_ok {
        Some(false) => return Decision::Drop("the library file changed or is gone"),
        None => return Decision::Keep,
        Some(true) => {}
    }
    if f.watched != Some(true) || f.playing {
        return Decision::Keep;
    }
    match f.original {
        Original::NotRecorded => Decision::Drop("watched, no torrent file to go back to"),
        Original::Gone => Decision::Drop("watched, the torrent file is gone"),
        Original::Replaced => Decision::Drop("watched, the torrent path holds another file now"),
        Original::Unknown => Decision::Keep,
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

/// The ntfy text when a batch ends: `(message, is an error)`. `skipped`: passed over
/// (playing, or refused as unsafe).
pub fn summary(title: &str, ok: usize, failed: usize, skipped: usize, stopped: bool, dry_run: bool) -> (String, bool) {
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
        (0, 0) if skipped > 0 => {
            (format!("Nicio conversie: {} {} ({title})", episodes_ro(skipped), if skipped == 1 { "sărit" } else { "sărite" }), true)
        }
        (0, 0) => (format!("Nimic de convertit ({title})"), false),
        (0, f) => (format!("Conversie eșuată: {} ({title})", episodes_ro(f)), true),
        (o, 0) => (format!("Conversie gata: {} ({title})", episodes_ro(o)), false),
        (o, f) => (format!("Conversie gata: {}, {f} {} ({title})", episodes_ro(o), if f == 1 { "eșuat" } else { "eșuate" }), true),
    }
}

/// [`summary`] for a movie (one file, no episode count).
pub fn movie_summary(title: &str, ok: usize, failed: usize, skipped: usize, stopped: bool, dry_run: bool) -> (String, bool) {
    if dry_run {
        return (format!("Probă: s-ar fi convertit {title}"), false);
    }
    if stopped {
        return (format!("Conversie oprită ({title})"), false);
    }
    match (ok, failed) {
        (0, 0) if skipped > 0 => (format!("Nicio conversie: {title} a fost sărit"), true),
        (0, 0) => (format!("Nimic de convertit ({title})"), false),
        (0, _) => (format!("Conversie eșuată ({title})"), true),
        _ => (format!("Conversie gata ({title})"), false),
    }
}

// -- shared state (routes <-> job) -------------------------------------------------------

/// What to convert: the next `count` unwatched episodes of a series, these `episode_ids` of
/// it, or one movie (`movie_id`, `series_id` 0).
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub series_id: i64,
    pub movie_id: i64,
    /// Only these episodes (empty: the next `count` from the first unwatched one).
    pub episode_ids: Vec<i64>,
    /// The series or movie title.
    pub title: String,
    pub count: usize,
}

impl Request {
    /// The next `count` episodes of a series.
    pub fn series(series_id: i64, title: &str, count: usize) -> Request {
        Request { series_id, movie_id: 0, episode_ids: Vec::new(), title: title.into(), count }
    }

    pub fn is_movie(&self) -> bool {
        self.movie_id != 0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub label: String,
    /// The Sonarr episode id (0 for a movie).
    pub episode_id: i64,
    /// `ok`, `failed`, `skipped`, `stopped`, `dry`.
    pub result: &'static str,
    pub note: String,
}

/// The episode (or movie) being converted.
#[derive(Debug, Clone, PartialEq)]
pub struct Current {
    pub label: String,
    pub percent: f64,
    pub episode_id: i64,
}

/// The current (or last) batch, as the Seriale tab, the movie sheet and the Acasă banner show it.
#[derive(Debug, Clone, Default)]
pub struct Batch {
    pub series_id: i64,
    pub movie_id: i64,
    pub episode_ids: Vec<i64>,
    pub title: String,
    pub requested: usize,
    pub running: bool,
    pub stopping: bool,
    pub dry_run: bool,
    /// The episode being converted and its percent.
    pub current: Option<Current>,
    /// Episodes still to do after the current one.
    pub remaining: usize,
    /// Their ids, in order.
    pub queued: Vec<i64>,
    pub items: Vec<Item>,
    pub summary: Option<String>,
    pub started_at: f64,
    pub finished_at: Option<f64>,
}

impl Batch {
    pub fn to_json(&self) -> Value {
        json!({
            "series_id": self.series_id, "movie_id": self.movie_id, "episode_ids": self.episode_ids,
            "title": self.title, "requested": self.requested,
            "running": self.running, "stopping": self.stopping, "dry_run": self.dry_run,
            "current": self.current.as_ref().map(|c| json!({"label": c.label, "percent": c.percent, "episode_id": c.episode_id})),
            "remaining": self.remaining, "queued": self.queued,
            "items": self.items.iter().map(|i| json!({"label": i.label, "episode_id": i.episode_id, "result": i.result, "note": i.note})).collect::<Vec<_>>(),
            "summary": self.summary, "started_at": self.started_at, "finished_at": self.finished_at,
        })
    }

    fn push(&mut self, e: &Episode, result: &'static str, note: &str) {
        self.items.push(Item { label: e.label(), episode_id: e.episode_id, result, note: note.to_string() });
    }
}

struct ListCache {
    at: Instant,
    value: Result<Vec<Value>, String>,
}

/// How long one series screen (`GET /api/series/<id>`) is reused.
const DETAIL_TTL: Duration = Duration::from_secs(20);

/// Held by `AppState`: the batch status, the start request for the job thread, the stop
/// flag, the cached Seriale grid and series screens.
#[derive(Default)]
pub struct Control {
    batch: Mutex<Option<Batch>>,
    pending: Mutex<Option<Request>>,
    stop: AtomicBool,
    pub wake: tokio::sync::Notify,
    list: Mutex<Option<ListCache>>,
    details: Mutex<HashMap<i64, (Instant, Value)>>,
    loading: AtomicBool,
    unavailable: AtomicBool,
}

/// What [`Control::begin`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Begin {
    Started,
    /// A batch runs already.
    Busy,
    /// The job thread is not running.
    Unavailable,
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

    /// The job thread could not start: no batch can run.
    pub fn set_unavailable(&self) {
        self.unavailable.store(true, Ordering::SeqCst);
    }

    pub fn available(&self) -> bool {
        !self.unavailable.load(Ordering::SeqCst)
    }

    /// End the running batch with `text` (an internal failure: no ntfy).
    pub fn abort(&self, text: String) {
        self.update(|b| {
            if b.running {
                b.running = false;
                b.stopping = false;
                b.current = None;
                b.remaining = 0;
                b.queued.clear();
                b.summary = Some(text);
                b.finished_at = Some(unix_now());
            }
        });
        self.invalidate_list();
    }

    /// Start a batch and wake the job thread; refused while one runs or without the thread.
    pub fn begin(&self, r: Request, dry_run: bool) -> Begin {
        if !self.available() {
            return Begin::Unavailable;
        }
        let mut b = lock(&self.batch);
        if b.as_ref().is_some_and(|b| b.running) {
            return Begin::Busy;
        }
        *b = Some(Batch {
            series_id: r.series_id,
            movie_id: r.movie_id,
            episode_ids: r.episode_ids.clone(),
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
        Begin::Started
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

    /// The next `GET /api/series` (and `/api/series/<id>`) reloads.
    pub fn invalidate_list(&self) {
        *lock(&self.list) = None;
        lock(&self.details).clear();
    }

    /// A series screen loaded in the last 20 s.
    pub fn cached_detail(&self, id: i64) -> Option<Value> {
        lock(&self.details).get(&id).filter(|(at, _)| at.elapsed() < DETAIL_TTL).map(|(_, v)| v.clone())
    }

    pub fn store_detail(&self, id: i64, v: Value) {
        let mut g = lock(&self.details);
        g.retain(|_, (at, _)| at.elapsed() < DETAIL_TTL);
        g.insert(id, (Instant::now(), v));
    }

    fn cached_list(&self, fresh_only: bool) -> Option<Result<Vec<Value>, String>> {
        let g = lock(&self.list);
        let c = g.as_ref()?;
        let ttl = if c.value.is_ok() { LIST_TTL } else { LIST_ERROR_TTL };
        (!fresh_only || c.at.elapsed() < ttl).then(|| c.value.clone())
    }

    /// `(list, error, loading)`: the cached Seriale grid ([`crate::logic::series::grid`]); when
    /// it is old, reload it in the background and wait up to 5 s for it (else the old one, or
    /// "loading").
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
                let value = crate::logic::series::grid(&a).await;
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

// -- Sonarr / Radarr / Plex reads --------------------------------------------------------

/// The settings (`jobs.convert`; the Plex sections default to strip_dv's Sonarr / Radarr
/// libraries).
#[derive(Debug, Clone)]
pub struct Settings {
    pub tmp_dir: PathBuf,
    /// The Plex TV section (episodes).
    pub section: u32,
    /// The Plex movie section.
    pub movie_section: u32,
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
        let section_of = |key: &str, arr: &str, default: u32| {
            job_cfg(app, "convert", key)
                .and_then(|v| v.as_u64())
                .map(|n| n as u32)
                .or_else(|| dv.libraries.iter().find(|l| l.arr == arr).map(|l| l.section))
                .unwrap_or(default)
        };
        Settings {
            tmp_dir: job_str(app, "convert", "tmp_dir", DEFAULT_TMP_DIR).into(),
            section: section_of("section", "sonarr", 2),
            movie_section: section_of("movie_section", "radarr", DEFAULT_MOVIE_SECTION),
            downloads_root: job_str(app, "seed_keeper", "downloads_root", "D:\\Media\\Downloads"),
            ffmpeg_bin: job_str(app, "convert", "ffmpeg_bin", &dv.ffmpeg_bin),
            ffmpeg_packages: dv.ffmpeg_packages.clone(),
            replace_tries: REPLACE_TRIES,
            replace_wait: REPLACE_WAIT,
            force_cross_volume: false,
        }
    }
}

pub const SONARR_DOWN: &str = "Sonarr nu răspunde.";
pub const RADARR_DOWN: &str = "Radarr nu răspunde.";
pub const QBIT_DOWN: &str = "qBittorrent nu răspunde, nu pot verifica torrentele.";
pub const PLEX_DOWN: &str = "Plex nu răspunde, nu știu ce episoade ai văzut.";
pub const PLEX_DOWN_MOVIE: &str = "Plex nu răspunde, nu știu dacă ai văzut filmul.";

/// The log line and the page message for an unreadable ledger.
pub fn ledger_error(t: &str) -> String {
    log_warning!("convert: the ledger is unreadable ({t})");
    format!("Lista conversiilor ({LEDGER_FILE}) nu se poate citi.")
}

/// Plex `viewCount` per episode file (the TV section).
pub async fn plex_views(app: &AppState, section: u32) -> Result<HashMap<String, i64>, String> {
    PlexJobs::new(app.services.plex.c.clone()).view_counts(section, true).await.map_err(|e| {
        log_warning!("convert: Plex section {section} failed: {}", e.type_name());
        PLEX_DOWN.to_string()
    })
}

/// Plex `viewCount` per movie file (the movie section).
pub async fn plex_movie_views(app: &AppState, section: u32) -> Result<HashMap<String, i64>, String> {
    PlexJobs::new(app.services.plex.c.clone()).view_counts(section, false).await.map_err(|e| {
        log_warning!("convert: Plex section {section} failed: {}", e.type_name());
        PLEX_DOWN_MOVIE.to_string()
    })
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

/// A Radarr movie's title and, if it needs converting, is not converted and not watched
/// (a file Plex does not know counts as unwatched), itself as the one candidate.
pub async fn movie_candidates(app: &AppState, movie_id: i64) -> Result<(String, Vec<Episode>), Lookup> {
    let arr = Arr::radarr(app.services.radarr.c.clone());
    let movie = match arr.movie(movie_id).await {
        Ok(m) => m,
        Err(UpstreamError::Status(404)) => return Err(Lookup::NotFound),
        Err(e) => {
            log_warning!("convert: Radarr movie {movie_id} failed: {}", e.type_name());
            return Err(Lookup::Down(RADARR_DOWN.into()));
        }
    };
    if id_of(movie.get("id")) != Some(movie_id) {
        return Err(Lookup::NotFound);
    }
    let title = movie.get("title").and_then(Value::as_str).unwrap_or("").to_string();
    let Some(file) = movie.get("movieFile").filter(|f| f.is_object()) else { return Ok((title, Vec::new())) };
    let Some(path) = file.get("path").and_then(Value::as_str).filter(|p| !p.trim().is_empty()) else { return Ok((title, Vec::new())) };
    if !file_needs_conversion(file) {
        return Ok((title, Vec::new()));
    }
    let converted = converted_keys(&load_ledger(&ledger_path(app)).map_err(|t| Lookup::Down(ledger_error(&t)))?);
    if converted.contains(&path_key(path)) {
        return Ok((title, Vec::new()));
    }
    let views = plex_movie_views(app, Settings::from_app(app).movie_section).await.map_err(Lookup::Down)?;
    if views.get(&path_key(path)).is_some_and(|n| *n > 0) {
        return Ok((title, Vec::new()));
    }
    let ep = Episode {
        series_id: 0,
        series_title: title.clone(),
        episode_id: 0,
        season: 0,
        number: 0,
        path: path.to_string(),
        size: file.get("size").and_then(Value::as_u64).unwrap_or(0),
        movie_id,
    };
    Ok((title, vec![ep]))
}

/// The candidates of a request: the movie, the requested episodes (in episode order), or the
/// whole series from the first unwatched one.
pub async fn request_candidates(app: &AppState, req: &Request) -> Result<(String, Vec<Episode>), Lookup> {
    if req.is_movie() {
        return movie_candidates(app, req.movie_id).await;
    }
    let (title, c) = series_candidates(app, req.series_id).await?;
    Ok((title, only_ids(c, &req.episode_ids)))
}

/// `c` limited to `ids` (all of `c` when `ids` is empty), order kept.
pub fn only_ids(c: Vec<Episode>, ids: &[i64]) -> Vec<Episode> {
    if ids.is_empty() {
        return c;
    }
    c.into_iter().filter(|e| ids.contains(&e.episode_id)).collect()
}

// -- routes ------------------------------------------------------------------------------

/// The demo batch: Frieren S01E06 converting (the percent runs over 15 minutes), S01E07 next.
pub fn demo_job(elapsed_s: f64) -> Value {
    let pct = crate::json::py_round((elapsed_s % 900.0) / 9.0, 1);
    Batch {
        series_id: 1,
        title: "Frieren: Beyond Journey's End".into(),
        requested: 2,
        running: true,
        dry_run: true,
        current: Some(Current { label: "S01E06".into(), percent: pct, episode_id: crate::logic::series::demo_episode_id(1, 1, 6) }),
        remaining: 1,
        queued: vec![crate::logic::series::demo_episode_id(1, 1, 7)],
        started_at: unix_now() - elapsed_s,
        ..Default::default()
    }
    .to_json()
}

fn status_body(job: Value, dry_run: bool, demo: bool, available: bool) -> Value {
    json!({"job": job, "dry_run": dry_run, "demo": demo, "available": available, "max": MAX_COUNT, "default": DEFAULT_COUNT})
}

/// `GET /api/convert`: the batch status only (no upstream call; the Acasă banner polls it).
pub fn api_status(app: &AppState) -> Value {
    if app.demo {
        return status_body(demo_job(app.started.elapsed().as_secs_f64()), true, true, true);
    }
    status_body(app.convert.status_json(), app.dry_run, false, app.convert.available())
}

/// A valid `POST /api/convert/start` body.
#[derive(Debug, Clone, PartialEq)]
pub enum Start {
    /// `{seriesId, count}`: the next `count` (1..=20) unwatched episodes.
    Next { series_id: i64, count: usize },
    /// `{seriesId, episodeIds}`: these (1..=20 distinct ids).
    Episodes { series_id: i64, ids: Vec<i64> },
    /// `{movieId}`.
    Movie { movie_id: i64 },
}

/// The start body, strictly: positive JSON integers only; `movieId` alone, or `seriesId` with
/// exactly one of `count` / `episodeIds`.
pub fn parse_start(body: &Map<String, Value>) -> Option<Start> {
    let pos = |k: &str| match body.get(k) {
        None => Ok(None),
        Some(v) => v.as_i64().filter(|n| *n > 0).map(Some).ok_or(()),
    };
    let (movie, series, count) = (pos("movieId").ok()?, pos("seriesId").ok()?, body.get("count"));
    let ids = body.get("episodeIds");
    if let Some(movie_id) = movie {
        return (series.is_none() && count.is_none() && ids.is_none()).then_some(Start::Movie { movie_id });
    }
    let series_id = series?;
    match (count, ids) {
        (Some(c), None) => {
            let n = c.as_i64().filter(|n| (1..=MAX_COUNT).contains(n))?;
            Some(Start::Next { series_id, count: n as usize })
        }
        (None, Some(Value::Array(a))) => {
            let ids: Vec<i64> = a.iter().map(|v| v.as_i64().filter(|n| *n > 0)).collect::<Option<_>>()?;
            let distinct: HashSet<i64> = ids.iter().copied().collect();
            (!ids.is_empty() && ids.len() <= MAX_COUNT as usize && distinct.len() == ids.len()).then_some(Start::Episodes { series_id, ids })
        }
        _ => None,
    }
}

/// `POST /api/convert/start` (see [`parse_start`]) -> `(status, body)`.
pub async fn api_start(app: &Arc<AppState>, body: &Map<String, Value>) -> (u16, Value) {
    let Some(start) = parse_start(body) else {
        return (400, json!({"error": "Cerere invalidă."}));
    };
    if app.demo {
        return (200, json!({"ok": true, "demo": true, "job": null}));
    }
    let busy = || (409, json!({"error": "O conversie rulează deja. Oprește-o sau așteaptă să se termine."}));
    let unavailable = || (503, json!({"error": "Conversia nu poate porni (lucrarea din fundal nu rulează, vezi jurnalul)."}));
    if !app.convert.available() {
        return unavailable();
    }
    if app.convert.running() {
        return busy();
    }
    let movie = matches!(start, Start::Movie { .. });
    let mut req = match &start {
        Start::Next { series_id, count } => Request::series(*series_id, "", *count),
        Start::Episodes { series_id, ids } => Request { episode_ids: ids.clone(), ..Request::series(*series_id, "", ids.len()) },
        Start::Movie { movie_id } => Request { movie_id: *movie_id, ..Request::series(0, "", 1) },
    };
    let slow = if movie { "Radarr sau Plex răspunde greu. Încearcă din nou." } else { "Sonarr sau Plex răspunde greu. Încearcă din nou." };
    let (title, cands) = match tokio::time::timeout(START_TIMEOUT, request_candidates(app, &req)).await {
        Err(_) => return (504, json!({"error": slow})),
        Ok(Err(Lookup::NotFound)) if movie => return (404, json!({"error": "Filmul nu există în Radarr."})),
        Ok(Err(Lookup::NotFound)) => return (404, json!({"error": "Serialul nu există în Sonarr."})),
        Ok(Err(Lookup::Down(m))) => return (502, json!({"error": m})),
        Ok(Ok(x)) => x,
    };
    match &start {
        Start::Movie { .. } if cands.is_empty() => {
            return (409, json!({"error": "Filmul nu trebuie convertit: merge direct pe TV, e convertit deja sau l-ai văzut."}));
        }
        Start::Episodes { ids, .. } if cands.len() != ids.len() => {
            return (409, json!({"error": "Episodul nu trebuie convertit: merge direct pe TV, e convertit deja sau l-ai văzut."}));
        }
        _ if cands.is_empty() => return (409, json!({"error": "Nu e nimic de convertit la acest serial."})),
        _ => {}
    }
    req.title = title;
    req.count = req.count.min(cands.len());
    match app.convert.begin(req, app.dry_run) {
        Begin::Started => {}
        Begin::Busy => return busy(),
        Begin::Unavailable => return unavailable(),
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
    /// Refused as unsafe (the library path is itself a torrent file, or that cannot be
    /// checked): does not count.
    Skipped(String),
    Stopped,
    Failed(String),
}

/// A found original: the torrent file, its torrent's hash (when known), its size and id.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub path: String,
    pub torrent: Option<String>,
    pub size: u64,
    pub id: FileId,
}

/// `Ok(Some(x))`, `Ok(None)` for `NotFound`, `Err` for any other error.
fn found<T>(r: std::io::Result<T>) -> Result<Option<T>, ()> {
    match r {
        Ok(x) => Ok(Some(x)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
    }
}

/// The library file now: converted size? (`None`: could not be checked).
pub fn library_state(path: &Path, converted_size: u64) -> Option<bool> {
    match found(fs::metadata(path)) {
        Ok(Some(m)) => Some(m.is_file() && m.len() == converted_size),
        Ok(None) => Some(false),
        Err(()) => None,
    }
}

/// The recorded original now (see [`Original`]); `in_torrent` is filled in by the caller.
pub fn original_state(e: &Entry) -> Original {
    let Some(o) = e.original.as_deref() else { return Original::NotRecorded };
    match found(fs::metadata(o)) {
        Ok(Some(m)) if m.is_file() && m.len() == e.original_size => {}
        Ok(_) => return Original::Gone,
        Err(()) => return Original::Unknown,
    }
    let Some((vol, idx)) = e.original_id else { return Original::Replaced };
    match found(file_id(Path::new(o))) {
        Ok(Some(id)) if id.volume == vol && id.index == idx => Original::Present { in_torrent: None },
        Ok(Some(_)) => Original::Replaced,
        Ok(None) => Original::Gone,
        Err(()) => Original::Unknown,
    }
}

/// Why a revert did not happen.
#[derive(Debug, Clone, PartialEq)]
pub enum RevertError {
    /// The library or the torrent file is not what it was: forget the entry.
    Changed(&'static str),
    /// Try again next pass.
    Failed(String),
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

    /// The files the job is creating right now, from the state file.
    fn leftovers(&self) -> Vec<String> {
        let st = self.app.state_file.load();
        st.get(LEFTOVERS_KEY).and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default()
    }

    fn set_leftovers(&self, v: Vec<String>) {
        if self.app.dry_run {
            return;
        }
        let mut m = Map::new();
        m.insert(LEFTOVERS_KEY.into(), json!(v));
        self.app.state_file.update(m);
    }

    /// Remember a file before creating it (a crash leaves it to [`Job::sweep`]).
    fn note_leftover(&self, p: &Path) {
        let s = p.to_string_lossy().to_string();
        let mut v = self.leftovers();
        if !v.contains(&s) {
            v.push(s);
            self.set_leftovers(v);
        }
    }

    /// Delete `paths` if they are still there and forget them.
    fn drop_leftovers(&self, paths: &[&Path]) {
        if self.app.dry_run {
            return;
        }
        for p in paths {
            if p.is_file() {
                let _ = fs::remove_file(p);
            }
        }
        let keys: Vec<String> = paths.iter().map(|p| path_key(&p.to_string_lossy())).collect();
        let v = self.leftovers();
        let left: Vec<String> = v.iter().filter(|s| !keys.contains(&path_key(s))).cloned().collect();
        if left.len() != v.len() {
            self.set_leftovers(left);
        }
    }

    /// Delete what an earlier run may have left: the files recorded as being created, and
    /// `<file>.partial` / `<file>.revert.tmp` next to `libraries` (ledger and batch paths).
    /// Only these exact names; nothing else is ever scanned or deleted.
    pub fn sweep(&self, libraries: &[String]) {
        let ours = |p: &str| {
            let l = p.to_lowercase();
            l.ends_with(TMP_SUFFIX) || l.ends_with(ENC_SUFFIX) || l.ends_with(&format!(".mkv{PARTIAL_SUFFIX}")) || l.ends_with(&format!(".mkv{REVERT_SUFFIX}"))
        };
        let recorded = self.leftovers();
        let mut paths: Vec<PathBuf> = recorded.iter().filter(|p| ours(p)).map(PathBuf::from).collect();
        for l in libraries {
            paths.push(partial_path(Path::new(l)));
            paths.push(revert_tmp(Path::new(l)));
        }
        for p in paths.iter().filter(|p| p.is_file()) {
            if self.app.dry_run {
                log_info!("convert (dry-run): would delete the leftover {}", p.display());
            } else if fs::remove_file(p).is_ok() {
                log_info!("convert: deleted the leftover {}", p.display());
            }
        }
        if !recorded.is_empty() {
            self.set_leftovers(Vec::new());
        }
    }

    /// The temp dir is safe to write into (see [`check_tmp_dir`]); also checks that the owning
    /// app (Sonarr, or Radarr for a movie) and qBittorrent answer (needed to convert safely).
    async fn check_tmp(&self, movie: bool) -> Result<(), String> {
        let sonarr = self.app.services.sonarr.arr().root_folders().await;
        let radarr = Arr::radarr(self.app.services.radarr.c.clone()).root_folders().await;
        let (own, other, down, other_name) =
            if movie { (radarr, sonarr, RADARR_DOWN, "Sonarr") } else { (sonarr, radarr, SONARR_DOWN, "Radarr") };
        let mut overlap = own.map_err(|_| down.to_string())?;
        match other {
            Ok(r) => overlap.extend(r),
            Err(e) => log_info!("convert: {other_name} root folders unknown ({}), not checked", e.type_name()),
        }
        let qbit = QbitJobs::new(self.app.services.qbittorrent.c.clone());
        let torrents = qbit.torrents().await.map_err(|_| QBIT_DOWN.to_string())?;
        let s = |t: &Value, k: &str| t.get(k).and_then(Value::as_str).map(String::from);
        overlap.extend(torrents.iter().filter_map(|t| s(t, "content_path")));
        let equal: Vec<String> = torrents.iter().filter_map(|t| s(t, "save_path")).collect();
        check_tmp_dir(&self.cfg.tmp_dir, &overlap, &equal)
    }

    /// One batch, then the ntfy summary.
    pub fn run_batch(&self, req: &Request, tools: Option<&dyn Tools>) {
        let title = req.title.clone();
        if req.is_movie() {
            log_info!("convert: '{}' (movie {})", ascii_log(&title), req.movie_id);
        } else {
            log_info!("convert: batch of {} for '{}' (series {})", req.count, ascii_log(&title), req.series_id);
        }
        let fail = |msg: String| {
            log_warning!("convert: batch for '{}' failed: {}", ascii_log(&title), ascii_log(&msg));
            let text = format!("Conversie eșuată ({title}): {msg}");
            self.finish(text, true);
        };
        let Some(tools) = tools else {
            return fail("ffmpeg nu e instalat (winget Gyan.FFmpeg).".into());
        };
        if let Err(m) = self.rt.block_on(self.check_tmp(req.is_movie())) {
            return fail(m);
        }
        let cands = match self.rt.block_on(request_candidates(self.app, req)) {
            Ok((_, c)) => c,
            Err(Lookup::NotFound) if req.is_movie() => return fail("filmul nu mai există în Radarr.".into()),
            Err(Lookup::NotFound) => return fail("serialul nu mai există în Sonarr.".into()),
            Err(Lookup::Down(m)) => return fail(m),
        };
        self.sweep(&cands.iter().map(|e| e.path.clone()).collect::<Vec<_>>());
        let mut queue: VecDeque<Episode> = cands.into_iter().collect();
        let (mut attempts, mut ok, mut failed, mut skipped_n, mut stopped) = (0usize, 0usize, 0usize, 0usize, false);
        while attempts < req.count {
            if self.stopped() {
                stopped = true;
                break;
            }
            let playing = self.playing().unwrap_or_default();
            let (next, skipped) = next_pick(&mut queue, &|e| playing.has(e));
            for s in skipped {
                log_info!("convert: {} {} is playing, taking the next one", ascii_log(&title), s.label());
                self.control().update(|b| b.push(&s, "skipped", "se redă acum"));
                skipped_n += 1;
            }
            let Some(ep) = next else { break };
            let left = (req.count - attempts - 1).min(queue.len());
            let queued: Vec<i64> = queue.iter().take(left).map(|e| e.episode_id).collect();
            self.control().update(|b| {
                b.current = Some(Current { label: ep.label(), percent: 0.0, episode_id: ep.episode_id });
                b.remaining = left;
                b.queued = queued;
            });
            let done = match tools.probe(Path::new(&ep.path)) {
                Ok(src) => self.convert_one(&ep, &src, tools),
                Err(e) => Done::Failed(format!("ffprobe: {e}")),
            };
            let file = Path::new(&ep.path);
            self.drop_leftovers(&[&enc_path(&self.cfg.tmp_dir, file), &tmp_path(&self.cfg.tmp_dir, file), &partial_path(file)]);
            let (result, note) = match &done {
                Done::Converted => ("ok", String::new()),
                Done::DryRun => ("dry", "probă: nimic schimbat".to_string()),
                Done::NotNeeded => ("skipped", "nu mai e nevoie".to_string()),
                Done::Skipped(why) => ("skipped", why.clone()),
                Done::Stopped => ("stopped", "oprit".to_string()),
                Done::Failed(e) => ("failed", e.clone()),
            };
            match &done {
                Done::Failed(e) => log_warning!("convert: {} {} failed: {}", ascii_log(&title), ep.label(), ascii_log(e)),
                Done::Skipped(e) => log_warning!("convert: {} {} refused: {}", ascii_log(&title), ep.label(), ascii_log(e)),
                _ => {}
            }
            self.control().update(|b| {
                b.current = None;
                b.push(&ep, result, &note);
            });
            match done {
                Done::Converted | Done::DryRun => {
                    attempts += 1;
                    ok += 1;
                    // The Seriale grid and screens show it at once.
                    self.control().invalidate_list();
                }
                Done::Failed(_) => {
                    attempts += 1;
                    failed += 1;
                }
                Done::NotNeeded => {}
                Done::Skipped(_) => skipped_n += 1,
                Done::Stopped => {
                    stopped = true;
                    break;
                }
            }
            let left = (req.count.saturating_sub(attempts)).min(queue.len());
            let queued: Vec<i64> = queue.iter().take(left).map(|e| e.episode_id).collect();
            self.control().update(|b| {
                b.remaining = left;
                b.queued = queued;
            });
        }
        let (text, error) = if req.is_movie() {
            movie_summary(&title, ok, failed, skipped_n, stopped, self.app.dry_run)
        } else {
            summary(&title, ok, failed, skipped_n, stopped, self.app.dry_run)
        };
        log_info!("convert: batch done: {ok} converted, {failed} failed{}", if stopped { ", stopped" } else { "" });
        self.finish(text, error);
    }

    /// Mark the batch finished, refresh the list, send ntfy (not when the app is stopping).
    fn finish(&self, text: String, error: bool) {
        self.control().update(|b| {
            b.running = false;
            b.current = None;
            b.remaining = 0;
            b.queued.clear();
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
        // Renaming over the library path must never touch a torrent's own file.
        let qbit = QbitJobs::new(self.app.services.qbittorrent.c.clone());
        match self.rt.block_on(self.in_qbit(&qbit, &ep.path, None)) {
            Ok(false) => {}
            Ok(true) => return Done::Skipped("fișierul din bibliotecă e chiar fișierul torrentului".into()),
            Err(e) => return Done::Skipped(format!("qBittorrent nu răspunde ({e}), nu pot verifica torrentul")),
        }
        let size = fs::metadata(file).map(|m| m.len()).unwrap_or(0);
        let short = |p: &Path| free_bytes(p).is_some_and(|free| (free as f64) < size as f64 * 1.1);
        if short(&self.cfg.tmp_dir) {
            return Done::Failed("spațiu insuficient pe disc".into());
        }
        let tmp = tmp_path(&self.cfg.tmp_dir, file);
        let cross = self.cfg.force_cross_volume || !same_volume(&tmp, file);
        if cross && short(file) {
            return Done::Failed("spațiu insuficient pe discul bibliotecii".into());
        }
        let original = self.find_original(file, ep);
        if self.app.dry_run {
            let o = original.as_ref().map(|f| f.path.clone()).unwrap_or_else(|| "none".into());
            log_info!("convert (dry-run): would convert {} -> {} (torrent file: {o})", file.display(), tmp.display());
            return Done::DryRun;
        }
        if let Err(e) = fs::create_dir_all(&self.cfg.tmp_dir) {
            return Done::Failed(format!("dosarul temporar: {}", crate::state::io_error_name(&e)));
        }
        let enc = enc_path(&self.cfg.tmp_dir, file);
        self.note_leftover(&enc);
        self.note_leftover(&tmp);
        if cross {
            self.note_leftover(&partial_path(file));
        }
        let clean = || {
            let _ = fs::remove_file(&enc);
            let _ = fs::remove_file(&tmp);
        };
        log_info!("convert: start {} ({:.1} GB)", file.display(), size as f64 / 1_073_741_824.0);
        let t0 = Instant::now();
        let total = duration(src);
        let mut last_pct = -1.0;
        let set_pct = |p: f64| {
            self.control().update(|b| {
                if let Some(c) = b.current.as_mut() {
                    c.percent = p;
                }
            })
        };
        let mut on_line = |l: &str| {
            if let Some(t) = parse_progress(l) {
                let p = crate::json::py_round(percent(t, total) * ENCODE_SHARE / 100.0, 1);
                if p != last_pct {
                    last_pct = p;
                    set_pct(p);
                }
            }
        };
        // Pass 1: the encode (no bitstream filter, see `ffmpeg_args`).
        let r = tools.encode(&ffmpeg_args(file, &enc), &mut on_line, &|| self.stopped());
        let (code, lines) = match r {
            Ok(x) => x,
            Err(e) => {
                clean();
                return if self.stopped() { Done::Stopped } else { Done::Failed(format!("ffmpeg: {e}")) };
            }
        };
        for l in &lines {
            log_info!("convert:   ffmpeg: {}", ascii_log(l));
        }
        if code != 0 || !enc.is_file() {
            clean();
            let last = lines.last().map(|l| format!(": {l}")).unwrap_or_default();
            return Done::Failed(format!("ffmpeg a eșuat ({code}){last}"));
        }
        if self.stopped() {
            clean();
            return Done::Stopped;
        }
        // Pass 2: the remux writes the BT.709 VUI; the encode and the result are both in the
        // temp dir until the encode is deleted.
        let enc_size = fs::metadata(&enc).map(|m| m.len()).unwrap_or(0);
        if free_bytes(&self.cfg.tmp_dir).is_some_and(|free| (free as f64) < enc_size as f64 * 1.1) {
            clean();
            return Done::Failed("spațiu insuficient pe disc".into());
        }
        set_pct(REMUX_PERCENT);
        let r = tools.encode(&remux_args(&enc, &tmp), &mut |_| {}, &|| self.stopped());
        let (code, lines) = match r {
            Ok(x) => x,
            Err(e) => {
                clean();
                return if self.stopped() { Done::Stopped } else { Done::Failed(format!("ffmpeg (remux): {e}")) };
            }
        };
        for l in &lines {
            log_info!("convert:   ffmpeg remux: {}", ascii_log(l));
        }
        let _ = fs::remove_file(&enc);
        if code != 0 || !tmp.is_file() {
            clean();
            let last = lines.last().map(|l| format!(": {l}")).unwrap_or_default();
            return Done::Failed(format!("remux a eșuat ({code}){last}"));
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
            original_id: original.as_ref().map(|f| (f.id.volume, f.id.index)),
            converted_size,
            converted_at: unix_now(),
            series_id: ep.series_id,
            series: ep.series_title.clone(),
            episode_id: ep.episode_id,
            season: ep.season,
            episode: ep.number,
            movie_id: ep.movie_id,
        });
        log_info!(
            "convert: done in {:.1} min: {} ({:.2} -> {:.2} GB)",
            t0.elapsed().as_secs_f64() / 60.0,
            file.display(),
            size as f64 / 1_073_741_824.0,
            converted_size as f64 / 1_073_741_824.0
        );
        self.update_apps(ep.movie_id, ep.series_id, file);
        Done::Converted
    }

    /// Put the verified `tmp` in place of `file` with one rename, only while `file` still has
    /// the stamp `before` and is not playing; retried while it is locked. On failure the temp
    /// file is deleted and the library file stays.
    fn install(&self, tmp: &Path, file: &Path, before: &str, ep: &Episode) -> Result<(), Done> {
        let cross = self.cfg.force_cross_volume || !same_volume(tmp, file);
        let src = if cross {
            let partial = partial_path(file);
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
        let movie = ep.movie_id != 0;
        let history = if movie {
            self.rt.block_on(Arr::radarr(self.app.services.radarr.c.clone()).movie_history(ep.movie_id, ARR_IMPORTED))
        } else {
            self.rt.block_on(self.app.services.sonarr.arr().series_history(ep.series_id, ARR_IMPORTED))
        };
        let (id_key, want) = if movie { ("movieId", ep.movie_id) } else { ("episodeId", ep.episode_id) };
        match history {
            Ok(mut recs) => {
                recs.retain(|r| {
                    r.get("eventType").and_then(Value::as_str).is_none_or(|t| t.eq_ignore_ascii_case("downloadFolderImported"))
                        && id_of(r.get(id_key)) == Some(want)
                });
                recs.sort_by_key(|r| std::cmp::Reverse(r.get("date").and_then(Value::as_str).unwrap_or("").to_string()));
                for r in recs {
                    let Some(p) = r.get("data").and_then(|d| d.get("droppedPath")).and_then(Value::as_str) else { continue };
                    if same(p) {
                        let torrent = r.get("downloadId").and_then(Value::as_str).map(|h| h.trim().to_ascii_lowercase()).filter(|h| !h.is_empty());
                        return Some(Found { path: p.to_string(), torrent, size, id });
                    }
                }
            }
            Err(e) => log_info!("convert: {} history failed ({}), looking in qBittorrent", if movie { "Radarr" } else { "Sonarr" }, e.type_name()),
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
                    return Some(Found { path: full, torrent: Some(hash), size, id });
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
        self.sweep(&entries.iter().map(|e| e.library.clone()).collect::<Vec<_>>());
        if entries.is_empty() {
            return;
        }
        // "Watched" per kind; a section Plex did not answer for keeps its entries for later.
        let series_views =
            if entries.iter().any(|e| e.movie_id == 0) { self.rt.block_on(plex_views(self.app, self.cfg.section)).ok() } else { None };
        let movie_views = if entries.iter().any(|e| e.movie_id != 0) {
            self.rt.block_on(plex_movie_views(self.app, self.cfg.movie_section)).ok()
        } else {
            None
        };
        if series_views.is_none() && movie_views.is_none() {
            return;
        }
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
            let Some(views) = (if e.movie_id != 0 { movie_views.as_ref() } else { series_views.as_ref() }) else {
                keep.push(e);
                continue;
            };
            let lib = Path::new(&e.library);
            let library_ok = library_state(lib, e.converted_size);
            let lib_stamp = stamp(lib).ok();
            let watched = views.get(&path_key(&e.library)).map(|n| *n > 0);
            let is_playing = playing.has_entry(&e);
            let mut original = original_state(&e);
            if let (Original::Present { .. }, Some(o)) = (&original, e.original.as_deref()) {
                if watched == Some(true) && !is_playing {
                    original = Original::Present { in_torrent: self.rt.block_on(self.in_qbit(&qbit, o, e.torrent.as_deref())).ok() };
                }
            }
            let facts = Facts { library_ok, watched, playing: is_playing, original };
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
                Decision::Revert => {
                    let (Some(o), Some(id), Some(st)) = (e.original.as_deref(), e.original_id, lib_stamp) else {
                        keep.push(e);
                        continue;
                    };
                    let tmp = revert_tmp(lib);
                    self.note_leftover(&tmp);
                    let r = revert_file(lib, Path::new(o), id, &st);
                    self.drop_leftovers(&[&tmp]);
                    match r {
                        Ok(()) => {
                            log_info!("convert: watched, {} is the torrent file again", e.library);
                            self.update_apps(e.movie_id, e.series_id, lib);
                            changed = true;
                        }
                        Err(RevertError::Changed(why)) => {
                            log_info!("convert: forgetting {} ({why}), the converted file stays", e.library);
                            changed = true;
                        }
                        Err(RevertError::Failed(why)) => {
                            log_warning!("convert: cannot put the torrent file back for {}: {why}", e.library);
                            keep.push(e);
                        }
                    }
                }
            }
        }
        if changed {
            save_ledger(&path, &keep);
            self.control().invalidate_list();
        }
    }

    /// Sonarr `RescanSeries` (Radarr `RescanMovie` for a movie) and a Plex refresh of the
    /// file's folder in its section. Failures are logged.
    fn update_apps(&self, movie_id: i64, series_id: i64, file: &Path) {
        let dir = file.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
        let app = self.app;
        let (arr, cmd, section) = if movie_id != 0 {
            (Arr::radarr(app.services.radarr.c.clone()), json!({"name": "RescanMovie", "movieId": movie_id}), self.cfg.movie_section)
        } else {
            (app.services.sonarr.arr(), json!({"name": "RescanSeries", "seriesId": series_id}), self.cfg.section)
        };
        self.rt.block_on(async {
            if let Err(e) = arr.command(&cmd).await {
                log_warning!("convert: {} rescan of {} failed: {}", arr.label(), movie_id.max(series_id), e.type_name());
            }
            if app.services.keys.get("plex").is_empty() {
                return;
            }
            if let Err(e) = PlexJobs::new(app.services.plex.c.clone()).refresh_path(section, &dir).await {
                log_warning!("convert: Plex refresh failed: {}", e.type_name());
            }
        });
    }
}

/// `<file>.revert.tmp` next to the library file.
pub fn revert_tmp(library: &Path) -> PathBuf {
    with_suffix(library, REVERT_SUFFIX)
}

/// `<file>.partial` next to the library file (cross-volume install).
pub fn partial_path(library: &Path) -> PathBuf {
    with_suffix(library, PARTIAL_SUFFIX)
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut name = p.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    p.with_file_name(name)
}

/// Make `library` a hard link to `original` again: a new link next to it, then one rename
/// over the converted file. Only while `original` is still the file `original_id` and
/// `library` still has the stamp `library_stamp` (checked right before the rename).
pub fn revert_file(library: &Path, original: &Path, original_id: (u64, u64), library_stamp: &str) -> Result<(), RevertError> {
    match file_id(original) {
        Ok(id) if (id.volume, id.index) == original_id => {}
        Ok(_) => return Err(RevertError::Changed("the torrent path holds another file now")),
        Err(e) => return Err(RevertError::Failed(format!("torrent file: {}", crate::state::io_error_name(&e)))),
    }
    let tmp = revert_tmp(library);
    let _ = fs::remove_file(&tmp);
    fs::hard_link(original, &tmp).map_err(|e| RevertError::Failed(format!("hard link: {}", crate::state::io_error_name(&e))))?;
    let same = match (file_id(&tmp), file_id(original)) {
        (Ok(a), Ok(b)) => a.same_file(&b) && (b.volume, b.index) == original_id,
        _ => false,
    };
    if !same {
        let _ = fs::remove_file(&tmp);
        return Err(RevertError::Failed("the new link is not the torrent file".into()));
    }
    if stamp(library).ok().as_deref() != Some(library_stamp) {
        let _ = fs::remove_file(&tmp);
        return Err(RevertError::Changed("the library file changed"));
    }
    if let Err(e) = fs::rename(&tmp, library) {
        let _ = fs::remove_file(&tmp);
        return Err(RevertError::Failed(format!("rename: {} (playing?)", crate::state::io_error_name(&e))));
    }
    Ok(())
}

/// Start the job on its own thread: batches on request, the revert pass every 5 minutes.
pub fn spawn(app: Arc<AppState>, stop: Stop) {
    let rt = Handle::current();
    let a2 = app.clone();
    let r = std::thread::Builder::new().name("convert".into()).spawn(move || {
        let guarded = |what: &str, f: &dyn Fn()| {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
                log_warning!("convert: {what} failed (panic)");
            }
        };
        // Whatever a crash left behind (temp files of a batch, a half-made revert link).
        guarded("cleanup", &|| {
            let job = Job { app: &app, rt: rt.clone(), cfg: Settings::from_app(&app), stop: &stop };
            let libs: Vec<String> = load_ledger(&ledger_path(&app)).unwrap_or_default().into_iter().map(|e| e.library).collect();
            job.sweep(&libs);
        });
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
            if let Some(req) = app.convert.take_request() {
                guarded("batch", &|| {
                    let job = Job { app: &app, rt: rt.clone(), cfg: Settings::from_app(&app), stop: &stop };
                    let tools = ffmpeg_bin(&job.cfg.ffmpeg_bin, &job.cfg.ffmpeg_packages).map(|bin| Ffmpeg { bin });
                    job.run_batch(&req, tools.as_ref().map(|t| t as &dyn Tools));
                });
                // A panic anywhere above (settings, ffmpeg lookup, the batch) must not leave
                // the batch "running" forever.
                if app.convert.running() {
                    app.convert.abort(format!("Conversie eșuată ({}): eroare internă, vezi jurnalul.", req.title));
                }
            }
            if Instant::now() >= next_revert {
                guarded("revert pass", &|| Job { app: &app, rt: rt.clone(), cfg: Settings::from_app(&app), stop: &stop }.revert_pass());
                next_revert = Instant::now() + INTERVAL;
            }
        }
    });
    if r.is_err() {
        log_warning!("convert: cannot start its thread, conversions are off");
        a2.convert.set_unavailable();
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
        Episode { series_id: 4, series_title: "Show".into(), episode_id: n, season: 1, number: n, path: format!("D:\\S\\e{n}.mkv"), size: 1, movie_id: 0 }
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
        // A movie: by its file path or its title.
        let film = Episode { movie_id: 7, series_title: "The Film".into(), path: "D:\\M\\f.mkv".into(), ..episode(9) };
        assert!(!p.has(&film));
        let raw = json!({"MediaContainer": {"Metadata": [{"type": "movie", "title": "The film"}]}});
        assert!(playing_from_sessions(Some(&raw)).has(&film));
        assert_eq!(film.label(), "The Film");
    }

    #[test]
    fn ffmpeg_command_and_progress() {
        // Pass 1: the encode, no bitstream filter (hevc_amf gives it no extradata).
        let a = ffmpeg_args(Path::new("D:\\S\\e1.mkv"), Path::new("D:\\T\\e1.hevc.enc.mkv"));
        assert_eq!(
            a.join(" "),
            "-hide_banner -loglevel error -y -progress pipe:1 -nostats -i D:\\S\\e1.mkv -map 0 -c copy -c:v hevc_amf -profile:v main10 \
             -pix_fmt p010le -quality quality -rc cqp -qp_i 19 -qp_p 21 -qp_b 23 -color_primaries bt709 -color_trc bt709 \
             -colorspace bt709 -color_range tv -tag:v hvc1 D:\\T\\e1.hevc.enc.mkv"
        );
        assert!(!a.iter().any(|x| x.starts_with("-bsf")), "no -bsf in the encode");
        // Pass 2: the stream-copy remux writes the BT.709 VUI.
        let b = remux_args(Path::new("D:\\T\\e1.hevc.enc.mkv"), Path::new("D:\\T\\e1.hevc.mkv"));
        assert_eq!(
            b.join(" "),
            "-hide_banner -loglevel error -y -i D:\\T\\e1.hevc.enc.mkv -map 0 -c copy \
             -bsf:v hevc_metadata=colour_primaries=1:transfer_characteristics=1:matrix_coefficients=1:video_full_range_flag=0 \
             -color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range tv D:\\T\\e1.hevc.mkv"
        );
        assert!(!b.iter().any(|x| x == "hevc_amf" || x == "-c:v"), "the remux encodes nothing");
        assert_eq!(tmp_path(Path::new("D:\\T"), Path::new("D:\\S\\Show - S01E01.mkv")), PathBuf::from("D:\\T\\Show - S01E01.hevc.mkv"));
        assert_eq!(enc_path(Path::new("D:\\T"), Path::new("D:\\S\\Show - S01E01.mkv")), PathBuf::from("D:\\T\\Show - S01E01.hevc.enc.mkv"));
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
        let mut streams = vec![json!({"index": 0, "codec_type": "video", "codec_name": codec, "pix_fmt": "yuv420p10le",
                                      "color_primaries": "bt709", "color_transfer": "bt709"})];
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
        // The colours: BT.709 primaries and transfer, else (HDR from AMF, untagged) refused.
        for (k, v) in [("color_primaries", json!("bt2020")), ("color_transfer", json!("smpte2084")), ("color_primaries", Value::Null)] {
            let out = colours(src_probe("hevc", "1420.1", 2, 3), k, v);
            assert!(verify(&src, &out).unwrap_err().starts_with("culori"), "{k}");
        }
    }

    /// `probe` with its video stream's `key` set to `v` (null removes it).
    fn colours(mut probe: Value, key: &str, v: Value) -> Value {
        let s = probe["streams"][0].as_object_mut().unwrap();
        if v.is_null() {
            s.remove(key);
        } else {
            s.insert(key.into(), v);
        }
        probe
    }

    fn entry(lib: &str, orig: Option<&str>) -> Entry {
        Entry {
            library: lib.into(),
            original: orig.map(String::from),
            torrent: Some("abc".into()),
            original_size: 10,
            original_id: Some((7, 42)),
            converted_size: 7,
            converted_at: 1_793_017_815.5,
            series_id: 4,
            series: "Show".into(),
            episode_id: 12,
            season: 1,
            episode: 2,
            movie_id: 0,
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
        b.movie_id = 7;
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
        let f = |ok: bool, watched, playing, original| Facts { library_ok: Some(ok), watched, playing, original };
        assert_eq!(revert_decision(&f(true, Some(true), false, present(Some(true)))), Decision::Revert);
        assert!(matches!(revert_decision(&f(true, Some(true), false, Original::Gone)), Decision::Drop(_)));
        assert!(matches!(revert_decision(&f(true, Some(true), false, Original::NotRecorded)), Decision::Drop(_)));
        assert!(matches!(revert_decision(&f(true, Some(true), false, Original::Replaced)), Decision::Drop(_)));
        assert!(matches!(revert_decision(&f(true, Some(true), false, present(Some(false)))), Decision::Drop(_)));
        assert_eq!(revert_decision(&f(true, Some(true), false, present(None))), Decision::Keep);
        assert_eq!(revert_decision(&f(true, Some(true), false, Original::Unknown)), Decision::Keep);
        assert_eq!(revert_decision(&f(true, Some(false), false, present(Some(true)))), Decision::Keep);
        assert_eq!(revert_decision(&f(true, None, false, Original::Gone)), Decision::Keep);
        assert_eq!(revert_decision(&f(true, Some(true), true, present(Some(true)))), Decision::Keep);
        assert!(matches!(revert_decision(&f(false, Some(false), false, present(Some(true)))), Decision::Drop(_)));
        // The library file could not be checked (not NotFound): kept for the next pass.
        let unknown = Facts { library_ok: None, watched: Some(true), playing: false, original: present(Some(true)) };
        assert_eq!(revert_decision(&unknown), Decision::Keep);
    }

    #[test]
    fn file_states() {
        let d = temp_dir("conv-states");
        let lib = d.join("lib.mkv");
        let orig = d.join("orig.mkv");
        fs::write(&lib, b"encoded").unwrap();
        fs::write(&orig, b"original-1").unwrap();
        assert_eq!(library_state(&lib, 7), Some(true));
        assert_eq!(library_state(&lib, 8), Some(false));
        assert_eq!(library_state(&d.join("none.mkv"), 7), Some(false));
        let id = file_id(&orig).unwrap();
        let mut e = entry(&lib.display().to_string(), Some(&orig.display().to_string()));
        e.original_id = Some((id.volume, id.index));
        assert_eq!(original_state(&e), Original::Present { in_torrent: None });
        e.original_size = 9;
        assert_eq!(original_state(&e), Original::Gone);
        e.original_size = 10;
        // No recorded id (an entry from before ids were kept): never linked back.
        let old = Entry { original_id: None, ..e.clone() };
        assert_eq!(original_state(&old), Original::Replaced);
        // Another file of the same size under the same name.
        fs::remove_file(&orig).unwrap();
        fs::write(&orig, b"original-1").unwrap();
        assert_eq!(original_state(&e), Original::Replaced);
        fs::remove_file(&orig).unwrap();
        assert_eq!(original_state(&e), Original::Gone);
        assert_eq!(original_state(&entry("x", None)), Original::NotRecorded);
    }

    #[test]
    fn revert_file_checks_both_files_right_before_the_rename() {
        let d = temp_dir("conv-revert");
        let lib = d.join("lib.mkv");
        let orig = d.join("orig.mkv");
        fs::write(&lib, b"encoded").unwrap();
        fs::write(&orig, b"original").unwrap();
        let id = file_id(&orig).unwrap();
        let st = stamp(&lib).unwrap();
        // The torrent path holds another file now.
        assert!(matches!(revert_file(&lib, &orig, (id.volume, id.index + 1), &st), Err(RevertError::Changed(_))));
        // The library file changed since it was checked.
        assert!(matches!(revert_file(&lib, &orig, (id.volume, id.index), "D:\\x|1|2"), Err(RevertError::Changed(_))));
        assert_eq!(fs::read(&lib).unwrap(), b"encoded");
        assert!(!revert_tmp(&lib).exists());
        assert_eq!(revert_file(&lib, &orig, (id.volume, id.index), &st), Ok(()));
        assert!(file_id(&lib).unwrap().same_file(&id));
        assert!(!revert_tmp(&lib).exists());
    }

    #[test]
    fn tmp_dir_rules() {
        let roots = vec!["D:\\Media\\Seriale".to_string(), "D:\\Media\\Downloads\\sonarr\\Show.S01".to_string()];
        let saves = vec!["D:\\Media\\Downloads\\sonarr".to_string()];
        assert_eq!(check_tmp_dir(Path::new("D:\\Media\\Downloads\\.reencode"), &roots, &saves), Ok(()));
        for bad in ["", "relative\\tmp", "D:\\Media\\Seriale", "d:\\media\\seriale\\tmp", "D:\\Media", "D:\\Media\\Downloads\\sonarr",
                    "D:\\Media\\Downloads\\sonarr\\Show.S01\\x"] {
            assert!(check_tmp_dir(Path::new(bad), &roots, &saves).is_err(), "{bad}");
        }
    }

    #[test]
    fn control_abort_and_unavailable() {
        let c = Control::new();
        assert_eq!(c.begin(req(2), false), Begin::Started);
        assert_eq!(c.begin(req(2), false), Begin::Busy);
        c.abort("Conversie eșuată (Show): eroare internă.".into());
        let b = c.batch().unwrap();
        assert!(!b.running && b.summary.as_deref() == Some("Conversie eșuată (Show): eroare internă."));
        c.set_unavailable();
        assert_eq!(c.begin(req(2), false), Begin::Unavailable);
    }

    #[test]
    fn texts() {
        assert_eq!(episodes_ro(1), "1 episod");
        assert_eq!(episodes_ro(3), "3 episoade");
        assert_eq!(episodes_ro(20), "20 de episoade");
        assert_eq!(summary("Demon Slayer", 3, 0, 0, false, false), ("Conversie gata: 3 episoade (Demon Slayer)".into(), false));
        assert_eq!(summary("X", 2, 1, 0, false, false), ("Conversie gata: 2 episoade, 1 eșuat (X)".into(), true));
        assert_eq!(summary("X", 0, 2, 0, false, false), ("Conversie eșuată: 2 episoade (X)".into(), true));
        assert_eq!(summary("X", 1, 0, 0, true, false), ("Conversie oprită după 1 episod (X)".into(), false));
        assert_eq!(summary("X", 0, 0, 0, true, false).0, "Conversie oprită (X)");
        assert_eq!(summary("X", 0, 0, 2, false, false), ("Nicio conversie: 2 episoade sărite (X)".into(), true));
        assert_eq!(summary("X", 0, 0, 0, false, false).0, "Nimic de convertit (X)");
        assert!(summary("X", 2, 0, 0, false, true).0.starts_with("Probă"));
        assert_eq!(movie_summary("Dune", 1, 0, 0, false, false), ("Conversie gata (Dune)".into(), false));
        assert_eq!(movie_summary("Dune", 0, 1, 0, false, false), ("Conversie eșuată (Dune)".into(), true));
        assert_eq!(movie_summary("Dune", 0, 0, 0, true, false).0, "Conversie oprită (Dune)");
        assert!(movie_summary("Dune", 0, 0, 1, false, false).1);
    }

    #[test]
    fn start_bodies() {
        let p = |v: Value| parse_start(v.as_object().unwrap());
        assert_eq!(p(json!({"seriesId": 4, "count": 3})), Some(Start::Next { series_id: 4, count: 3 }));
        assert_eq!(p(json!({"seriesId": 4, "episodeIds": [12, 11]})), Some(Start::Episodes { series_id: 4, ids: vec![12, 11] }));
        assert_eq!(p(json!({"movieId": 7})), Some(Start::Movie { movie_id: 7 }));
        for bad in [
            json!({}),
            json!({"seriesId": 4}),
            json!({"seriesId": 4, "count": 2, "episodeIds": [1]}),
            json!({"seriesId": 4, "episodeIds": []}),
            json!({"seriesId": 4, "episodeIds": [1, 1]}),
            json!({"seriesId": 4, "episodeIds": [1, "2"]}),
            json!({"seriesId": 4, "episodeIds": [0]}),
            json!({"seriesId": 4, "episodeIds": 5}),
            json!({"seriesId": 4, "episodeIds": (1..=21).collect::<Vec<i64>>()}),
            json!({"movieId": 0}),
            json!({"movieId": "7"}),
            json!({"movieId": 7, "seriesId": 4}),
            json!({"movieId": 7, "count": 1}),
            json!({"seriesId": 0, "count": 1}),
        ] {
            assert_eq!(p(bad.clone()), None, "{bad}");
        }
    }

    #[test]
    fn demo_status_has_a_running_batch() {
        let v = demo_job(450.0);
        assert_eq!((v["running"].clone(), v["series_id"].clone(), v["current"]["percent"].clone()), (json!(true), json!(1), json!(50.0)));
        assert_eq!(v["current"]["label"], json!("S01E06"));
    }

    // -- the job against fake services and a fake ffmpeg ----------------------------------

    /// Probes by file name (`*.hevc.mkv`: `out`). The encode (pass 1, no `-bsf`) writes
    /// `*.hevc.enc.mkv` and reports progress; the remux (pass 2, `-bsf:v hevc_metadata`) needs
    /// that file and writes `*.hevc.mkv`.
    struct FakeTools {
        out: Value,
        /// Pass 1's exit code.
        exit: i32,
        /// Pass 2's exit code.
        remux_exit: i32,
        calls: StdMutex<Vec<Vec<String>>>,
        /// Press Stop during this ffmpeg run (1-based, either pass): ffmpeg is "killed".
        stop_on: Option<(usize, Arc<AppState>)>,
        /// The progress bar when Stop was pressed.
        pct_at_stop: StdMutex<Option<f64>>,
    }

    fn is_remux(args: &[String]) -> bool {
        args.iter().any(|a| a == "-bsf:v")
    }

    impl FakeTools {
        /// (encodes, remuxes) run so far.
        fn passes(&self) -> (usize, usize) {
            let c = self.calls.lock().unwrap();
            let r = c.iter().filter(|a| is_remux(a)).count();
            (c.len() - r, r)
        }
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
            let out = args.last().unwrap();
            let remux = is_remux(args);
            if remux {
                assert!(out.ends_with(TMP_SUFFIX) && !out.ends_with(ENC_SUFFIX), "{out}");
                assert!(!args.iter().any(|a| a == "hevc_amf"), "the remux encodes nothing");
                let input = &args[args.iter().position(|a| a == "-i").unwrap() + 1];
                assert!(input.ends_with(ENC_SUFFIX), "{input}");
                assert_eq!(fs::read(input).unwrap(), b"encoded", "the remux reads pass 1's file");
                fs::write(out, b"encoded").unwrap();
            } else {
                assert!(out.ends_with(ENC_SUFFIX), "{out}");
                assert!(args.iter().any(|a| a == "hevc_amf"));
                fs::write(out, b"encoded").unwrap();
                on_line("out_time_us=50000000");
            }
            if let Some((at, a)) = &self.stop_on {
                if *at == n {
                    *self.pct_at_stop.lock().unwrap() = a.convert.batch().and_then(|b| b.current).map(|c| c.percent);
                    assert!(a.convert.request_stop());
                }
            }
            if stop() {
                return Err("oprit".into());
            }
            if remux {
                return Ok((self.remux_exit, if self.remux_exit == 0 { vec![] } else { vec!["remux error".into()] }));
            }
            on_line("progress=end");
            Ok((self.exit, vec![]))
        }
    }

    fn tools(exit: i32) -> FakeTools {
        FakeTools { out: src_probe("hevc", "100.4", 2, 1), exit, remux_exit: 0, calls: StdMutex::new(vec![]), stop_on: None, pct_at_stop: StdMutex::new(None) }
    }

    const HASH: &str = "abcdef0123456789abcdef0123456789abcdef01";
    const MOVIE_HASH: &str = "fedcba9876543210fedcba9876543210fedcba98";

    /// A library of 4 episodes; E1, E2, E4 are hard links to the torrent files, E3 a copy.
    /// Plus one movie (Radarr 7), a hard link to its torrent file.
    struct World {
        root: PathBuf,
        lib: PathBuf,
        downloads: PathBuf,
        views: StdMutex<Vec<(String, i64)>>,
        movie_views: StdMutex<Vec<(String, i64)>>,
        sessions: StdMutex<Value>,
        qbit_up: StdMutex<bool>,
        lib_is_torrent: StdMutex<bool>,
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
            let (mlib, mtor) = (root.join("Movies").join("Film (2020)"), downloads.join("radarr").join("Film.2020"));
            fs::create_dir_all(&mlib).unwrap();
            fs::create_dir_all(&mtor).unwrap();
            fs::write(mtor.join("Film.2020.mkv"), b"movie-original").unwrap();
            fs::hard_link(mtor.join("Film.2020.mkv"), mlib.join("Film (2020).mkv")).unwrap();
            Arc::new(World {
                root,
                lib,
                downloads,
                views: StdMutex::new(vec![]),
                movie_views: StdMutex::new(vec![]),
                sessions: StdMutex::new(json!({"MediaContainer": {"size": 0}})),
                qbit_up: StdMutex::new(true),
                lib_is_torrent: StdMutex::new(false),
            })
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

        fn movie_file(&self) -> PathBuf {
            self.root.join("Movies").join("Film (2020)").join("Film (2020).mkv")
        }

        fn movie_torrent(&self) -> PathBuf {
            self.downloads.join("radarr").join("Film.2020").join("Film.2020.mkv")
        }

        fn watch_movie(&self) {
            self.movie_views.lock().unwrap().push((self.movie_file().display().to_string(), 1));
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
            if p == "/api/v3/movie/7" {
                return ok(json!({"id": 7, "title": "Film", "year": 2020, "hasFile": true,
                                 "movieFile": {"id": 70, "path": w.movie_file().display().to_string(), "size": 14,
                                               "mediaInfo": {"videoCodec": "x264", "videoBitDepth": 10}}}));
            }
            if p == "/api/v3/movie/8" {
                return ok(json!({"id": 8, "title": "Hevc", "hasFile": true,
                                 "movieFile": {"path": "D:\\M\\h.mkv", "mediaInfo": {"videoCodec": "x265", "videoBitDepth": 10}}}));
            }
            if p == "/api/v3/movie/9" {
                return status(404);
            }
            if p.starts_with("/api/v3/history/movie?movieId=7") {
                return ok(json!([{"eventType": "downloadFolderImported", "movieId": 7, "downloadId": MOVIE_HASH.to_uppercase(),
                                  "date": "2026-10-01T00:00:00Z", "data": {"droppedPath": w.movie_torrent().display().to_string()}}]));
            }
            if p.starts_with("/library/sections/1/all") {
                let meta: Vec<Value> =
                    w.movie_views.lock().unwrap().iter().map(|(f, v)| json!({"viewCount": v, "Media": [{"Part": [{"file": f}]}]})).collect();
                return ok(json!({"MediaContainer": {"Metadata": meta}}));
            }
            if p.starts_with("/library/sections/1/refresh") {
                return ok(json!({}));
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
            if p.starts_with("/api/v2/") && !*w.qbit_up.lock().unwrap() {
                return status(500);
            }
            if p == "/api/v3/rootfolder" {
                return ok(json!([{"path": w.root.join("Seriale").display().to_string()}]));
            }
            if p == "/api/v2/torrents/info" {
                let mut t = vec![
                    json!({"hash": HASH, "progress": 1.0, "save_path": w.downloads.join("sonarr").display().to_string(),
                           "content_path": w.downloads.join("sonarr").join("Show.S01").display().to_string()}),
                    json!({"hash": MOVIE_HASH, "progress": 1.0, "save_path": w.downloads.join("radarr").display().to_string(),
                           "content_path": w.downloads.join("radarr").join("Film.2020").display().to_string()}),
                ];
                if *w.lib_is_torrent.lock().unwrap() {
                    // A torrent saved straight into the library season folder.
                    t.push(json!({"hash": "libtorrent", "progress": 1.0, "save_path": w.lib.display().to_string(),
                                  "content_path": w.lib_file(1).display().to_string()}));
                }
                return ok(json!(t));
            }
            if p.starts_with("/api/v2/torrents/files?hash=libtorrent") {
                return ok(json!([{"name": "Show - S01E01.mkv", "size": 10}]));
            }
            if p.starts_with(&format!("/api/v2/torrents/files?hash={MOVIE_HASH}")) {
                return ok(json!([{"name": "Film.2020/Film.2020.mkv", "size": 14}]));
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
        Request::series(4, "Show", count)
    }

    /// `begin` + `take_request`, as the route and the job thread do.
    fn begun(r: &Rig, count: usize) -> Request {
        assert_eq!(r.a.convert.begin(req(count), r.a.dry_run), Begin::Started);
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
        // Each one: the encode, then the remux of that encode.
        assert_eq!(t.passes(), (2, 2));
        let calls = t.calls.lock().unwrap().clone();
        assert!(!is_remux(&calls[0]) && is_remux(&calls[1]) && !is_remux(&calls[2]) && is_remux(&calls[3]));
        assert!(calls[0].last().unwrap().ends_with("Show - S01E02.hevc.enc.mkv"));
        assert!(calls[1].last().unwrap().ends_with("Show - S01E02.hevc.mkv"));
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
        let (_, left) = series_candidates(&r.a, 4).await.unwrap();
        assert_eq!(left.iter().map(Episode::label).collect::<Vec<_>>(), ["S01E04"]);
        let grid = crate::logic::series::grid(&r.a).await.unwrap();
        assert_eq!((grid[0]["convert"].clone(), grid[0]["unwatched"].clone()), (json!(1), json!(3)));
        assert_eq!(b.items.iter().map(|i| i.episode_id).collect::<Vec<_>>(), [102, 103]);

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
    async fn a_new_file_under_the_torrent_name_is_never_linked_back() {
        let r = rig(false).await;
        run(&r, req(1), Arc::new(tools(0))).await;
        assert_eq!(ledger(&r)[0].original_id.map(|x| x.0), Some(file_id(&r.w.torrent_file(1)).unwrap().volume));
        // The torrent was re-downloaded: same name and size, another file.
        fs::remove_file(r.w.torrent_file(1)).unwrap();
        fs::write(r.w.torrent_file(1), b"original-1").unwrap();
        r.w.watch(1);
        revert(&r).await;
        assert!(ledger(&r).is_empty());
        assert_eq!(fs::read(r.w.lib_file(1)).unwrap(), b"encoded");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_library_file_that_is_a_torrent_file_is_refused() {
        let r = rig(false).await;
        *r.w.lib_is_torrent.lock().unwrap() = true;
        let rq = begun(&r, 1);
        let t = Arc::new(tools(0));
        run(&r, rq, t.clone()).await;
        // E1 is refused (not counted), E2 is converted instead.
        assert_eq!(fs::read(r.w.lib_file(1)).unwrap(), b"original-1");
        assert_eq!(fs::read(r.w.lib_file(2)).unwrap(), b"encoded");
        let b = r.a.convert.batch().unwrap();
        assert_eq!(b.items.iter().map(|i| (i.label.as_str(), i.result)).collect::<Vec<_>>(), [("S01E01", "skipped"), ("S01E02", "ok")]);
        assert!(b.items[0].note.contains("torrent"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn qbittorrent_down_converts_nothing() {
        let r = rig(false).await;
        *r.w.qbit_up.lock().unwrap() = false;
        let rq = begun(&r, 2);
        let t = Arc::new(tools(0));
        run(&r, rq, t.clone()).await;
        assert!(t.calls.lock().unwrap().is_empty());
        let b = r.a.convert.batch().unwrap();
        assert!(!b.running);
        assert_eq!(b.summary.as_deref(), Some(format!("Conversie eșuată (Show): {QBIT_DOWN}").as_str()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bad_tmp_dir_converts_nothing() {
        let w = World::new();
        let f = serve(w.clone()).await;
        let extra = json!({"jobs": {"convert": {"tmp_dir": w.root.join("Seriale").join("tmp").display().to_string()}}});
        let (a, _d) = app(&f.base, false, extra);
        let r = Rig { f, a, w };
        let rq = begun(&r, 2);
        let t = Arc::new(tools(0));
        run(&r, rq, t.clone()).await;
        assert!(t.calls.lock().unwrap().is_empty());
        assert!(r.a.convert.batch().unwrap().summary.unwrap().contains("se suprapune"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn leftovers_are_swept_only_by_their_exact_names() {
        let r = rig(false).await;
        run(&r, req(1), Arc::new(tools(0))).await;
        let a = r.a.clone();
        // What a crash would leave: a half-made revert link next to a ledger file, a recorded
        // temp file; plus files the job never makes, which must stay.
        let revert_left = revert_tmp(&r.w.lib_file(1));
        let tmp_left = r.w.root.join("tmp").join("Show - S01E03.hevc.mkv");
        let enc_left = r.w.root.join("tmp").join("Show - S01E03.hevc.enc.mkv");
        let foreign = [
            r.w.lib.join("other.partial"),
            r.w.root.join("tmp").join("keep.hevc.mkv"),
            r.w.root.join("tmp").join("keep.hevc.enc.mkv"),
            partial_path(&r.w.lib_file(4)),
        ];
        fs::create_dir_all(r.w.root.join("tmp")).unwrap();
        for p in [&revert_left, &tmp_left, &enc_left].into_iter().chain(foreign.iter()) {
            fs::write(p, b"x").unwrap();
        }
        let mut m = Map::new();
        m.insert(LEFTOVERS_KEY.into(), json!([tmp_left.display().to_string(), enc_left.display().to_string(), "C:\\Windows\\win.ini"]));
        a.state_file.update(m);
        let rt = Handle::current();
        let libs: Vec<String> = ledger(&r).into_iter().map(|e| e.library).collect();
        tokio::task::spawn_blocking(move || {
            let (_h, stop) = crate::jobs::stop_pair();
            Job { app: &a, rt, cfg: settings(&a), stop: &stop }.sweep(&libs);
        })
        .await
        .unwrap();
        assert!(!revert_left.exists() && !tmp_left.exists() && !enc_left.exists());
        assert!(foreign.iter().all(|p| p.exists()), "nothing else is touched");
        assert!(r.a.state_file.load()[LEFTOVERS_KEY].as_array().unwrap().is_empty());
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
        // The encode fails, the remux fails, the verification fails (length, then the colours
        // still HDR): originals stay, temp files (the encode too) go.
        let hdr = colours(colours(src_probe("hevc", "100", 2, 1), "color_primaries", json!("bt2020")), "color_transfer", json!("smpte2084"));
        for (exit, remux_exit, out, passes, why) in [
            (1, 0, src_probe("hevc", "100", 2, 1), (2, 0), "ffmpeg a eșuat"),
            (0, 1, src_probe("hevc", "100", 2, 1), (2, 2), "remux a eșuat (1): remux error"),
            (0, 0, src_probe("hevc", "90", 2, 1), (2, 2), "verificare eșuată: durata"),
            (0, 0, hdr, (2, 2), "verificare eșuată: culori"),
        ] {
            let r = rig(false).await;
            let rq = begun(&r, 2);
            let mut t = tools(exit);
            t.remux_exit = remux_exit;
            t.out = out;
            let t = Arc::new(t);
            run(&r, rq, t.clone()).await;
            assert_eq!(t.passes(), passes, "a failure counts towards the count ({why})");
            assert!(r.a.state_file.load()[LEFTOVERS_KEY].as_array().is_none_or(|a| a.is_empty()), "{why}");
            assert!(r.a.convert.batch().unwrap().items.iter().all(|i| i.note.starts_with(why)), "{why}");
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
        // Stop pressed during the first episode's remux: nothing installed, both temps gone.
        let r = rig(false).await;
        let rq = begun(&r, 3);
        let mut t = tools(0);
        t.stop_on = Some((2, r.a.clone()));
        let t = Arc::new(t);
        run(&r, rq, t.clone()).await;
        assert_eq!(t.passes(), (1, 1));
        assert_eq!(fs::read(r.w.lib_file(1)).unwrap(), b"original-1");
        assert!(tmp_is_empty(&r), "the killed remux's files are deleted");
        assert!(!ledger_path(&r.a).exists() || ledger(&r).is_empty());
        assert_eq!(*t.pct_at_stop.lock().unwrap(), Some(REMUX_PERCENT), "the remux shows 99 %");
        let b = r.a.convert.batch().unwrap();
        assert_eq!(b.items.iter().map(|i| i.result).collect::<Vec<_>>(), ["stopped"]);
        assert_eq!(b.summary.as_deref(), Some("Conversie oprită (Show)"));
        // Stop pressed while ffmpeg encodes the second episode.
        let r = rig(false).await;
        let rq = begun(&r, 3);
        let mut t = tools(0);
        t.stop_on = Some((3, r.a.clone()));
        let t = Arc::new(t);
        run(&r, rq, t.clone()).await;
        assert_eq!(t.passes(), (2, 1));
        assert_eq!(*t.pct_at_stop.lock().unwrap(), Some(48.5), "50 % of the encode is 48.5 % of the bar");
        assert_eq!(fs::read(r.w.lib_file(1)).unwrap(), b"encoded");
        assert_eq!(fs::read(r.w.lib_file(2)).unwrap(), b"original-2");
        assert!(tmp_is_empty(&r), "the killed encode's temp file is deleted");
        let b = r.a.convert.batch().unwrap();
        assert_eq!(b.items.iter().map(|i| i.result).collect::<Vec<_>>(), ["ok", "stopped"]);
        assert_eq!(b.summary.as_deref(), Some("Conversie oprită după 1 episod (Show)"));
        assert!(!b.running);
        // A new batch may start after it.
        assert_eq!(r.a.convert.begin(req(1), false), Begin::Started);
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
        let v = api_status(&r.a);
        assert_eq!((v["job"]["running"].clone(), v["available"].clone(), v["max"].clone()), (json!(true), json!(true), json!(20)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn start_episode_ids_converts_only_those() {
        let r = rig(false).await;
        r.w.watch(1);
        // E1 is watched: refused, nothing starts.
        let body = json!({"seriesId": 4, "episodeIds": [101]});
        let (code, out) = api_start(&r.a, body.as_object().unwrap()).await;
        assert_eq!(code, 409, "{out}");
        assert!(!r.a.convert.running());
        let body = json!({"seriesId": 4, "episodeIds": [104, 102]});
        let (code, out) = api_start(&r.a, body.as_object().unwrap()).await;
        assert_eq!(code, 200, "{out}");
        assert_eq!((out["job"]["requested"].clone(), out["job"]["episode_ids"].clone()), (json!(2), json!([104, 102])));
        let rq = r.a.convert.take_request().unwrap();
        assert_eq!((rq.episode_ids.clone(), rq.count), (vec![104, 102], 2));
        let t = Arc::new(tools(0));
        run(&r, rq, t.clone()).await;
        // In episode order, E3 is left alone.
        assert_eq!(fs::read(r.w.lib_file(2)).unwrap(), b"encoded");
        assert_eq!(fs::read(r.w.lib_file(3)).unwrap(), b"original-3");
        assert_eq!(fs::read(r.w.lib_file(4)).unwrap(), b"encoded");
        let b = r.a.convert.batch().unwrap();
        assert_eq!(b.items.iter().map(|i| (i.label.as_str(), i.result)).collect::<Vec<_>>(), [("S01E02", "ok"), ("S01E04", "ok")]);
        assert!(b.queued.is_empty() && b.current.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_movie_converts_and_reverts_like_an_episode() {
        let r = rig(false).await;
        // Validation and the rule: unknown, HEVC.
        let start = |id: i64| json!({"movieId": id});
        assert_eq!(api_start(&r.a, start(9).as_object().unwrap()).await, (404, json!({"error": "Filmul nu există în Radarr."})));
        assert_eq!(api_start(&r.a, start(8).as_object().unwrap()).await.0, 409);
        let (code, out) = api_start(&r.a, start(7).as_object().unwrap()).await;
        assert_eq!(code, 200, "{out}");
        assert_eq!((out["job"]["movie_id"].clone(), out["job"]["title"].clone(), out["job"]["series_id"].clone()), (json!(7), json!("Film"), json!(0)));
        let rq = r.a.convert.take_request().unwrap();
        let t = Arc::new(tools(0));
        run(&r, rq, t.clone()).await;
        assert_eq!(t.passes(), (1, 1));
        assert_eq!(fs::read(r.w.movie_file()).unwrap(), b"encoded");
        assert_eq!(fs::read(r.w.movie_torrent()).unwrap(), b"movie-original", "the torrent copy is untouched");
        assert!(tmp_is_empty(&r));
        let b = r.a.convert.batch().unwrap();
        assert_eq!(b.items.iter().map(|i| (i.label.as_str(), i.result)).collect::<Vec<_>>(), [("Film", "ok")]);
        assert_eq!(b.summary.as_deref(), Some("Conversie gata (Film)"));
        let l = ledger(&r);
        assert_eq!(l.len(), 1);
        assert_eq!((l[0].movie_id, l[0].original.as_deref(), l[0].torrent.as_deref()), (7, Some(r.w.movie_torrent().display().to_string().as_str()), Some(MOVIE_HASH)));
        let cmds: Vec<Value> = r.f.find("POST", "/api/v3/command").iter().map(Req::json).collect();
        assert_eq!(cmds, vec![json!({"name": "RescanMovie", "movieId": 7})]);
        assert_eq!(r.f.find("GET", "/library/sections/1/refresh").len(), 1);
        // Converted: not offered again.
        assert_eq!(api_start(&r.a, start(7).as_object().unwrap()).await.0, 409);
        // Not watched yet: kept; playing (by title): kept; watched: the torrent file again.
        revert(&r).await;
        assert_eq!(ledger(&r).len(), 1);
        r.w.watch_movie();
        r.w.play(json!({"MediaContainer": {"Metadata": [{"type": "movie", "title": "Film"}]}}));
        revert(&r).await;
        assert_eq!(fs::read(r.w.movie_file()).unwrap(), b"encoded");
        r.w.play(json!({"MediaContainer": {"size": 0}}));
        revert(&r).await;
        assert!(ledger(&r).is_empty());
        assert_eq!(fs::read(r.w.movie_file()).unwrap(), b"movie-original");
        let (x, y) = (file_id(&r.w.movie_file()).unwrap(), file_id(&r.w.movie_torrent()).unwrap());
        assert!(x.same_file(&y) && x.links == 2, "a hard link again");
        assert_eq!(r.f.find("POST", "/api/v3/command").len(), 2);
        // The series section was never needed for a movie-only ledger.
        assert!(r.f.find("GET", "/library/sections/2/all").is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_watched_movie_is_not_converted() {
        let r = rig(false).await;
        r.w.watch_movie();
        let (code, out) = api_start(&r.a, json!({"movieId": 7}).as_object().unwrap()).await;
        assert_eq!(code, 409, "{out}");
        assert!(out["error"].as_str().unwrap().contains("l-ai văzut"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn services_down_give_a_message() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let (a, _d) = app(&format!("http://127.0.0.1:{port}"), false, json!({}));
        let v = api_status(&a);
        assert_eq!((v["job"].clone(), v["demo"].clone()), (Value::Null, json!(false)));
        let (code, out) = api_start(&a, json!({"seriesId": 4, "count": 2}).as_object().unwrap()).await;
        assert_eq!((code, out), (502, json!({"error": "Sonarr nu răspunde."})));
        let (code, out) = api_start(&a, json!({"movieId": 7}).as_object().unwrap()).await;
        assert_eq!((code, out), (502, json!({"error": "Radarr nu răspunde."})));
        // Plex down: the start says so.
        let w = World::new();
        let f = fake(move |r: &Req| {
            if r.path == "/api/v3/series/4" {
                ok(show())
            } else if r.path.starts_with("/api/v3/episodefile") {
                ok(json!([file(1, &w.lib_file(1).display().to_string(), "x264", 10)]))
            } else if r.path.starts_with("/api/v3/episode?") {
                ok(json!([ep(101, 1, 1, 1)]))
            } else {
                status(500)
            }
        })
        .await;
        let (a, _d) = app(&f.base, false, json!({}));
        let (code, out) = api_start(&a, json!({"seriesId": 4, "count": 2}).as_object().unwrap()).await;
        assert_eq!((code, out), (502, json!({"error": PLEX_DOWN})));
    }

    #[test]
    fn demo_has_data() {
        let (a, _d) = app("http://127.0.0.1:9", false, json!({}));
        assert_eq!(api_status(&a)["job"], Value::Null);
        let demo = AppState::new(a.config.clone(), crate::app::AppOptions { demo: true, ..Default::default() }).unwrap();
        let v = api_status(&demo);
        assert_eq!((v["demo"].clone(), v["job"]["running"].clone()), (json!(true), json!(true)));
    }
}
