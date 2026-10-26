//! Seed keeper, every 5 minutes, first run 90 s after start: the only thing that deletes
//! torrents (plan "New behaviour" 3). qBittorrent's own share limits stay off.
//!
//! ## For `/api/status` (Phase 2a)
//! ```ignore
//! pub fn snapshot(app: &AppState) -> HashMap<String, SeedInfo>
//! pub struct SeedInfo { pub seconds: f64, pub watched: Option<bool>, pub waiting_watch: bool }
//! ```
//! Keyed by the torrent hash in lowercase (as qBittorrent reports it). `seconds` is the
//! ledger's seeded time (use it for the H&R progress instead of qBittorrent's
//! `seeding_time`), `watched` is `Some(true/false)` once the torrent is mapped to Radarr /
//! Sonarr and Plex, `None` when unknown; `waiting_watch` is "H&R done, not watched yet" (the
//! PC tab shows "gata, aștept să-l vezi"). Cheap: an in-memory copy of the last tick, read
//! from `seed-ledger.json` on first use. A torrent missing from the map has no ledger entry
//! yet (fall back to qBittorrent's numbers, `watched: null`, `waiting_watch: false`).
//!
//! ## The ledger (`seed-ledger.json` next to `state.local.json`, atomic writes)
//! Per completed FileList torrent: `seconds` seeded, `last_tick`, and the Radarr/Sonarr
//! mapping once found (`movie_id`, or `series_id` + `episode_ids`; remembered because the
//! *arr apps drop the history of a deleted item). A new entry starts at qBittorrent's
//! `seeding_time`; afterwards each tick adds the time since the last tick, at most 10 min,
//! and only while qBittorrent answers, the torrent is seeding (`uploading`, `stalledUP`,
//! `forcedUP`) and a FileList tracker reports status 2 (working).
//!
//! ## Deletion (`deleteFiles=true`: the download copy; the library hardlink stays)
//! Only for categories `radarr` / `sonarr`, content under `D:\Media\Downloads`, and only
//! when BOTH hold: H&R satisfied (ledger >= 49 h or ratio >= 1.0) AND watched-or-gone
//! (Radarr: movie file -> Plex section 1 `viewCount > 0`, or the movie is no longer in
//! Radarr; Sonarr: every episode of the download watched in Plex section 2, or the series
//! is no longer in Sonarr). Fallback for an unmapped torrent (no *arr history at all and no
//! stored mapping, e.g. the movie was deleted with its history): once H&R is satisfied it
//! is deleted only if every file of the torrent exists under the downloads root with a
//! hard-link count of 1 (no library copy shares it). Any other unknown mapping, API error,
//! missing file, failed link query or link count > 1 keeps the torrent. Dry-run:
//! the ledger is kept (it is the app's own file), deletions are only logged.

use crate::app::AppState;
use crate::jobs::common::{id_of, job_str, unix_now, Quiet};
use crate::jobs::Stop;
use crate::services::arr_jobs::Arr;
use crate::services::plex_jobs::{path_key, PlexJobs};
use crate::services::qbit_jobs::QbitJobs;
use crate::{log_info, log_warning};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub const INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const FIRST_RUN: Duration = Duration::from_secs(90);
/// The ledger file, next to `state.local.json`.
pub const LEDGER_FILE: &str = "seed-ledger.json";
/// FileList asks for 48 h; one hour of margin.
pub const HR_SECONDS: f64 = 49.0 * 3600.0;
pub const HR_RATIO: f64 = 1.0;
/// One tick never adds more than this.
pub const MAX_TICK_SECONDS: f64 = 600.0;
/// Ledger entries of torrents gone from qBittorrent are dropped after this.
pub const PRUNE_AFTER_SECONDS: f64 = 7.0 * 86_400.0;
const SEEDING_STATES: [&str; 3] = ["uploading", "stalledUP", "forcedUP"];
const TRACKER_WORKING: i64 = 2;
const PLEX_MOVIES: u32 = 1;
const PLEX_SERIES: u32 = 2;

/// What `/api/status` shows for one seeding torrent.
#[derive(Debug, Clone, PartialEq)]
pub struct SeedInfo {
    /// Seconds seeded according to the ledger.
    pub seconds: f64,
    /// Watched in Plex (`None`: unknown mapping or not checked yet).
    pub watched: Option<bool>,
    /// H&R satisfied but not watched yet.
    pub waiting_watch: bool,
}

/// `<state dir>\seed-ledger.json`.
pub fn ledger_path(app: &AppState) -> PathBuf {
    app.state_file.path().parent().map(Path::to_path_buf).unwrap_or_default().join(LEDGER_FILE)
}

fn snapshots() -> &'static Mutex<HashMap<PathBuf, HashMap<String, SeedInfo>>> {
    static S: OnceLock<Mutex<HashMap<PathBuf, HashMap<String, SeedInfo>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The seed info of every torrent in the ledger (see the module docs).
pub fn snapshot(app: &AppState) -> HashMap<String, SeedInfo> {
    let path = ledger_path(app);
    let mut g = snapshots().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = g.get(&path) {
        return s.clone();
    }
    let s = infos_of(&load_ledger(&path));
    g.insert(path, s.clone());
    s
}

fn publish(path: &Path, infos: HashMap<String, SeedInfo>) {
    snapshots().lock().unwrap_or_else(|e| e.into_inner()).insert(path.to_path_buf(), infos);
}

/// `{"torrents": {"<hash>": {...}}}`; a missing or broken file is empty.
pub fn load_ledger(path: &Path) -> Map<String, Value> {
    let Ok(bytes) = std::fs::read(path) else { return Map::new() };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(m)) => match m.get("torrents") {
            Some(Value::Object(t)) => t.clone(),
            _ => Map::new(),
        },
        _ => Map::new(),
    }
}

/// Atomic write (`.tmp` + rename), sorted, pretty.
pub fn save_ledger(path: &Path, torrents: &Map<String, Value>) {
    let v = crate::json::sorted(&json!({"version": 1, "torrents": torrents}));
    let mut bytes = crate::json::dump_pretty(&v);
    bytes.push(b'\n');
    let tmp = crate::config::tmp_path(path);
    if let Err(e) = std::fs::write(&tmp, &bytes).and_then(|_| std::fs::rename(&tmp, path)) {
        log_warning!("seed keeper: cannot save the ledger: {}", crate::state::io_error_name(&e));
    }
}

fn infos_of(torrents: &Map<String, Value>) -> HashMap<String, SeedInfo> {
    torrents
        .iter()
        .map(|(h, e)| {
            let info = SeedInfo {
                seconds: e.get("seconds").and_then(Value::as_f64).unwrap_or(0.0),
                watched: e.get("watched").and_then(Value::as_bool),
                waiting_watch: e.get("waiting_watch").and_then(Value::as_bool).unwrap_or(false),
            };
            (h.to_ascii_lowercase(), info)
        })
        .collect()
}

// -- decisions ----------------------------------------------------------------------------

/// The ledger's new `seconds` for one tick (pure).
pub fn accrue(seconds: f64, last_tick: f64, now: f64, seeding: bool, tracker_working: bool) -> f64 {
    if !(seeding && tracker_working) {
        return seconds;
    }
    seconds + (now - last_tick).clamp(0.0, MAX_TICK_SECONDS)
}

pub fn hr_done(seconds: f64, ratio: f64) -> bool {
    seconds >= HR_SECONDS || ratio >= HR_RATIO
}

/// Watched-or-gone state of one torrent's content.
#[derive(Debug, Clone, PartialEq)]
pub enum Watch {
    Watched,
    NotWatched,
    /// The movie / series is no longer in Radarr / Sonarr.
    Gone,
    /// No Radarr/Sonarr history at all and no stored mapping (the link-count fallback).
    Unmapped,
    Unknown(String),
}

impl Watch {
    /// A stable name of the variant (the log-on-change kind).
    pub fn kind(&self) -> &'static str {
        match self {
            Watch::Watched => "watched",
            Watch::NotWatched => "unwatched",
            Watch::Gone => "gone",
            Watch::Unmapped => "unmapped",
            Watch::Unknown(_) => "unknown",
        }
    }
}

/// `path` is under `root` (`D:\Media\Downloads`), no `..` tricks.
pub fn under_root(path: &str, root: &str) -> bool {
    let (p, r) = (path_key(path), path_key(root));
    !p.is_empty() && !r.is_empty() && p.starts_with(&format!("{r}\\")) && !p.split('\\').any(|c| c == "..")
}

/// Hard-link count of a file (a trait so tests can fake it).
pub trait LinkCounter: Send + Sync {
    fn links(&self, path: &Path) -> Result<u32, String>;
}

/// `GetFileInformationByHandle(...).nNumberOfLinks`.
pub struct Win32Links;

impl LinkCounter for Win32Links {
    #[cfg(windows)]
    fn links(&self, path: &Path) -> Result<u32, String> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
        let f = std::fs::File::open(path).map_err(|e| crate::state::io_error_name(&e).to_string())?;
        // SAFETY: a valid open handle and a zeroed out struct for the call to fill.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) };
        if ok == 0 {
            return Err("GetFileInformationByHandle failed".into());
        }
        Ok(info.nNumberOfLinks)
    }

    #[cfg(not(windows))]
    fn links(&self, path: &Path) -> Result<u32, String> {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).map(|m| m.nlink() as u32).map_err(|e| e.to_string())
    }
}

/// The fallback check: every file of the torrent (`save_path` + qBittorrent's relative
/// names) is under `root` and `content_path` and has exactly one link. `Err` says why not.
pub fn no_library_copy(save_path: &str, content_path: &str, root: &str, names: &[String], links: &dyn LinkCounter) -> Result<(), String> {
    if names.is_empty() {
        return Err("qBittorrent lists no files".into());
    }
    let content = path_key(content_path);
    for n in names {
        let full = format!("{}\\{}", save_path.trim_end_matches(['\\', '/']), n.replace('/', "\\"));
        let key = path_key(&full);
        if !under_root(&full, root) || !(key == content || key.starts_with(&format!("{content}\\"))) {
            return Err(format!("file outside the torrent folder: {n}"));
        }
        match links.links(Path::new(&full)) {
            Ok(1) => {}
            Ok(c) => return Err(format!("{n} has {c} links (library copy)")),
            Err(e) => return Err(format!("{n}: {e}")),
        }
    }
    Ok(())
}

/// Per-tick cache of the upstream lists (each fetched at most once per tick).
#[derive(Default)]
struct Ctx {
    movies: Option<Result<HashMap<i64, Value>, String>>,
    series: Option<Result<HashSet<i64>, String>>,
    views: HashMap<u32, Result<HashMap<String, i64>, String>>,
    episodes: HashMap<i64, Result<SeriesEpisodes, String>>,
}

/// Sonarr episodes by id and episode file paths by id, of one series.
type SeriesEpisodes = (HashMap<i64, Value>, HashMap<i64, String>);

pub struct Keeper {
    pub qbit: QbitJobs,
    pub links: Box<dyn LinkCounter>,
    filelist: Regex,
    last: HashMap<String, String>,
    quiet: Quiet,
}

impl Keeper {
    pub fn new(app: &AppState) -> Keeper {
        let pat = app
            .config
            .get("filelist_tracker_regex")
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| crate::config::DEFAULT_FILELIST_REGEX.into());
        let filelist = Regex::new(&format!("(?i){pat}"))
            .unwrap_or_else(|_| Regex::new(&format!("(?i){}", crate::config::DEFAULT_FILELIST_REGEX)).expect("regex"));
        Keeper { qbit: QbitJobs::new(app.services.qbittorrent.c.clone()), links: Box::new(Win32Links), filelist, last: HashMap::new(), quiet: Quiet::default() }
    }

    /// Log a keep/delete reason when its kind changes for this torrent.
    fn note(&mut self, hash: &str, kind: &str, line: String) {
        if self.last.get(hash).map(String::as_str) != Some(kind) {
            log_info!("{line}");
            self.last.insert(hash.to_string(), kind.to_string());
        }
    }
}

/// Start the loop.
pub fn spawn(app: Arc<AppState>, mut stop: Stop) {
    tokio::spawn(async move {
        if stop.sleep(FIRST_RUN).await {
            return;
        }
        let mut k = Keeper::new(&app);
        loop {
            tick(&app, &mut k, unix_now()).await;
            if stop.sleep(INTERVAL).await {
                return;
            }
        }
    });
}

fn s<'a>(t: &'a Value, k: &str) -> &'a str {
    t.get(k).and_then(Value::as_str).unwrap_or("")
}

fn f(t: &Value, k: &str) -> f64 {
    t.get(k).and_then(Value::as_f64).unwrap_or(0.0)
}

/// One run. `now` is unix seconds.
pub async fn tick(app: &AppState, k: &mut Keeper, now: f64) {
    let path = ledger_path(app);
    let torrents = match k.qbit.torrents().await {
        Ok(t) => {
            if k.quiet.clear("qbit") {
                log_info!("seed keeper: qBittorrent reachable again");
            }
            t
        }
        Err(e) => {
            k.quiet.warn("qbit", format!("seed keeper: qBittorrent failed, nothing accrued: {e}"));
            return;
        }
    };
    let root = job_str(app, "seed_keeper", "downloads_root", "D:\\Media\\Downloads");
    let mut ledger = load_ledger(&path);
    let mut ctx = Ctx::default();
    let mut present = HashSet::new();
    for t in &torrents {
        let hash = s(t, "hash").trim().to_ascii_lowercase();
        if hash.is_empty() || f(t, "progress") < 1.0 {
            continue;
        }
        present.insert(hash.clone());
        let name = s(t, "name").to_string();
        let trackers = k.qbit.trackers(&hash, &k.filelist).await;
        let (is_fl, fl_working) = match &trackers {
            Ok(ts) => (ts.iter().any(|x| x.filelist), ts.iter().any(|x| x.filelist && x.status == TRACKER_WORKING)),
            Err(_) => (ledger.contains_key(&hash), false),
        };
        if !is_fl {
            continue; // not a FileList torrent: no H&R ledger, never deleted here
        }
        let mut entry = match ledger.get(&hash) {
            Some(Value::Object(e)) => e.clone(),
            _ => {
                let mut e = Map::new();
                e.insert("seconds".into(), json!(f(t, "seeding_time").max(0.0)));
                e.insert("last_tick".into(), json!(now));
                ledger.insert(hash.clone(), Value::Object(e.clone()));
                e
            }
        };
        let seeding = SEEDING_STATES.contains(&s(t, "state"));
        let secs = accrue(
            entry.get("seconds").and_then(Value::as_f64).unwrap_or(0.0),
            entry.get("last_tick").and_then(Value::as_f64).unwrap_or(now),
            now,
            seeding,
            fl_working && trackers.is_ok(),
        );
        entry.insert("seconds".into(), json!(secs));
        entry.insert("last_tick".into(), json!(now));

        let cat = s(t, "category").to_ascii_lowercase();
        let ratio = f(t, "ratio");
        let done = hr_done(secs, ratio);
        let hours = format!("{:.1}", secs / 3600.0);
        let mut watched = None;
        let mut waiting = false;
        if cat != "radarr" && cat != "sonarr" {
            k.note(&hash, "category", format!("seed keeper: keeping '{name}': category '{cat}' is not radarr/sonarr"));
        } else if !under_root(s(t, "content_path"), &root) || !under_root(s(t, "save_path"), &root) {
            k.note(&hash, "path", format!("seed keeper: keeping '{name}': content is not under {root}"));
        } else {
            let w = watch_state(app, &cat, &hash, &mut entry, &mut ctx).await;
            watched = match w {
                Watch::Watched => Some(true),
                Watch::NotWatched => Some(false),
                _ => None,
            };
            waiting = done && w == Watch::NotWatched;
            let why = match &w {
                Watch::Watched => "watched".to_string(),
                Watch::Gone => format!("no longer in {}", if cat == "radarr" { "Radarr" } else { "Sonarr" }),
                Watch::NotWatched => "not watched yet".to_string(),
                Watch::Unmapped => "not in Radarr/Sonarr history".to_string(),
                Watch::Unknown(r) => format!("unknown ({r})"),
            };
            // A tracker query error is an API error too: keep.
            let mut delete = done && trackers.is_ok() && matches!(w, Watch::Watched | Watch::Gone);
            let mut why = why;
            if done && trackers.is_ok() && w == Watch::Unmapped {
                let check = match k.qbit.files(&hash).await {
                    Ok(names) => no_library_copy(s(t, "save_path"), s(t, "content_path"), &root, &names, &*k.links),
                    Err(e) => Err(format!("qBittorrent files: {e}")),
                };
                match check {
                    Ok(()) => {
                        delete = true;
                        why = "no library copy (link count 1)".into();
                    }
                    Err(r) => why = format!("{why}, kept: {r}"),
                }
            }
            let detail = format!("seeded {hours} h, ratio {ratio:.2}, {why}");
            if !delete {
                let kind = format!("{}|{}", if done { "hr" } else { "pending" }, w.kind());
                let hr = if done { "H&R done" } else { "H&R pending" };
                k.note(&hash, &kind, format!("seed keeper: keeping '{name}': {hr} ({detail})"));
            } else if app.dry_run {
                k.note(&hash, "dry-delete", format!("dry-run: would delete torrent '{name}' with its download copy ({detail})"));
            } else {
                match k.qbit.delete_with_files(&hash).await {
                    Ok(()) => {
                        log_info!("seed keeper: deleted torrent '{name}' with its download copy ({detail})");
                        ledger.remove(&hash);
                        k.last.remove(&hash);
                        continue;
                    }
                    Err(e) => k.note(&hash, "delete-failed", format!("seed keeper: deleting '{name}' failed, kept: {e}")),
                }
            }
        }
        entry.insert("watched".into(), watched.map(Value::Bool).unwrap_or(Value::Null));
        entry.insert("waiting_watch".into(), Value::Bool(waiting));
        ledger.insert(hash, Value::Object(entry));
    }
    // Torrents removed from qBittorrent by hand age out of the ledger.
    ledger.retain(|h, e| present.contains(h) || now - e.get("last_tick").and_then(Value::as_f64).unwrap_or(0.0) < PRUNE_AFTER_SECONDS);
    save_ledger(&path, &ledger);
    publish(&path, infos_of(&ledger));
}


async fn movies(app: &AppState, ctx: &mut Ctx) -> Result<HashMap<i64, Value>, String> {
    if ctx.movies.is_none() {
        let r = Arr::radarr(app.services.radarr.c.clone()).items().await.map_err(|e| format!("Radarr movies: {e}"));
        // An empty library is suspicious (fresh database, wrong instance): never "gone".
        let r = r.and_then(|l| if l.is_empty() { Err("Radarr returned no movies".into()) } else { Ok(l) });
        ctx.movies = Some(r.map(|l| l.into_iter().filter_map(|m| Some((id_of(m.get("id"))?, m))).collect()));
    }
    ctx.movies.clone().unwrap_or_else(|| Err("unreachable".into()))
}

async fn series_ids(app: &AppState, ctx: &mut Ctx) -> Result<HashSet<i64>, String> {
    if ctx.series.is_none() {
        let r = app.services.sonarr.arr().items().await.map_err(|e| format!("Sonarr series: {e}"));
        let r = r.and_then(|l| if l.is_empty() { Err("Sonarr returned no series".into()) } else { Ok(l) });
        ctx.series = Some(r.map(|l| l.iter().filter_map(|s| id_of(s.get("id"))).collect()));
    }
    ctx.series.clone().unwrap_or_else(|| Err("unreachable".into()))
}

async fn views(app: &AppState, ctx: &mut Ctx, section: u32) -> Result<HashMap<String, i64>, String> {
    if let Some(v) = ctx.views.get(&section) {
        return v.clone();
    }
    let r = PlexJobs::new(app.services.plex.c.clone())
        .view_counts(section, section == PLEX_SERIES)
        .await
        .map_err(|e| format!("Plex section {section}: {e}"));
    ctx.views.insert(section, r.clone());
    r
}

/// Resolve (and remember) the mapping, then decide watched / gone / unknown.
async fn watch_state(app: &AppState, cat: &str, hash: &str, entry: &mut Map<String, Value>, ctx: &mut Ctx) -> Watch {
    let radarr = cat == "radarr";
    let arr = if radarr { Arr::radarr(app.services.radarr.c.clone()) } else { app.services.sonarr.arr() };
    let id_key = if radarr { "movie_id" } else { "series_id" };
    if id_of(entry.get(id_key)).is_none() {
        let recs = match arr.history_for_download(hash).await {
            Ok(r) => r,
            Err(e) => return Watch::Unknown(format!("{} history: {e}", arr.label())),
        };
        let field = if radarr { "movieId" } else { "seriesId" };
        if recs.is_empty() {
            return Watch::Unmapped;
        }
        let ids: HashSet<i64> = recs.iter().filter_map(|r| id_of(r.get(field))).collect();
        if ids.len() != 1 {
            return Watch::Unknown(format!("{} history has {} item(s) for it", arr.label(), ids.len()));
        }
        let id = *ids.iter().next().unwrap_or(&0);
        entry.insert("arr".into(), json!(cat));
        entry.insert(id_key.into(), json!(id));
        if !radarr {
            let mut eps: Vec<i64> = recs.iter().filter_map(|r| id_of(r.get("episodeId"))).collect::<HashSet<_>>().into_iter().collect();
            eps.sort_unstable();
            if eps.is_empty() {
                entry.remove(id_key);
                return Watch::Unknown("Sonarr history has no episodes for it".into());
            }
            entry.insert("episode_ids".into(), json!(eps));
        }
    }
    let id = id_of(entry.get(id_key)).unwrap_or(0);
    if radarr {
        let m = match movies(app, ctx).await {
            Ok(m) => m,
            Err(e) => return Watch::Unknown(e),
        };
        let Some(movie) = m.get(&id) else { return Watch::Gone };
        let Some(file) = movie.get("movieFile").and_then(|f| f.get("path")).and_then(Value::as_str) else {
            return Watch::Unknown("the movie has no file".into());
        };
        let v = match views(app, ctx, PLEX_MOVIES).await {
            Ok(v) => v,
            Err(e) => return Watch::Unknown(e),
        };
        return match v.get(&path_key(file)) {
            Some(n) if *n > 0 => Watch::Watched,
            Some(_) => Watch::NotWatched,
            None => Watch::Unknown("not in Plex".into()),
        };
    }
    let ids = match series_ids(app, ctx).await {
        Ok(s) => s,
        Err(e) => return Watch::Unknown(e),
    };
    if !ids.contains(&id) {
        return Watch::Gone;
    }
    let cached = ctx.episodes.get(&id).cloned();
    let r = match cached {
        Some(r) => r,
        None => {
            let (e, fl) = (arr.episodes(id).await, arr.episode_files(id).await);
            let r: Result<SeriesEpisodes, String> = match (e, fl) {
                (Ok(e), Ok(fl)) => Ok((
                    e.into_iter().filter_map(|x| Some((id_of(x.get("id"))?, x))).collect(),
                    fl.iter().filter_map(|x| Some((id_of(x.get("id"))?, x.get("path")?.as_str()?.to_string()))).collect(),
                )),
                (Err(e), _) | (_, Err(e)) => Err(format!("Sonarr episodes: {e}")),
            };
            ctx.episodes.insert(id, r.clone());
            r
        }
    };
    let (eps, files) = match r {
        Ok(x) => x,
        Err(e) => return Watch::Unknown(e),
    };
    let v = match views(app, ctx, PLEX_SERIES).await {
        Ok(v) => v,
        Err(e) => return Watch::Unknown(e),
    };
    let wanted: Vec<i64> = entry.get("episode_ids").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_i64).collect()).unwrap_or_default();
    if wanted.is_empty() {
        return Watch::Unknown("no episodes".into());
    }
    let mut any_unwatched = false;
    for e in wanted {
        let Some(ep) = eps.get(&e) else { return Watch::Unknown(format!("episode {e} not in Sonarr")) };
        let Some(path) = id_of(ep.get("episodeFileId")).and_then(|fid| files.get(&fid)) else {
            return Watch::Unknown(format!("episode {e} has no file"));
        };
        match v.get(&path_key(path)) {
            Some(n) if *n > 0 => {}
            Some(_) => any_unwatched = true,
            None => return Watch::Unknown(format!("episode {e} not in Plex")),
        }
    }
    if any_unwatched {
        Watch::NotWatched
    } else {
        Watch::Watched
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::testutil::{app, fake, ok, status, Fake, Req, Resp};

    const H: &str = "abcdef0123456789abcdef0123456789abcdef01";

    struct World {
        torrent: Mutex<Value>,
        tracker_status: Mutex<i64>,
        radarr_history: Mutex<Resp>,
        movies: Mutex<Resp>,
        plex_movie_views: Mutex<i64>,
        sonarr_series: Value,
        sonarr_eps_watched: Vec<i64>,
    }

    fn torrent(cat: &str, state: &str, seeding_time: i64, ratio: f64) -> Value {
        json!({"hash": H, "name": "Dune.2021.2160p", "category": cat, "state": state, "progress": 1.0,
               "ratio": ratio, "seeding_time": seeding_time,
               "content_path": "D:\\Media\\Downloads\\radarr\\Dune.2021.2160p", "save_path": "D:\\Media\\Downloads\\radarr"})
    }

    fn world() -> World {
        World {
            torrent: Mutex::new(torrent("radarr", "stalledUP", 1000, 0.1)),
            tracker_status: Mutex::new(2),
            radarr_history: Mutex::new(ok(json!({"records": [{"movieId": 10, "downloadId": H.to_uppercase()}]}))),
            movies: Mutex::new(ok(json!([{"id": 10, "title": "Dune", "movieFile": {"path": "D:\\Media\\Movies\\Dune (2021)\\Dune.mkv"}}, {"id": 11}]))),
            plex_movie_views: Mutex::new(0),
            sonarr_series: json!([{"id": 4, "title": "Show"}]),
            sonarr_eps_watched: vec![],
        }
    }

    async fn serve(w: Arc<World>) -> Fake {
        fake(move |r: &Req| {
            let p = r.path.as_str();
            let sonarr = r.header("x-api-key") == Some("sonarr-key");
            if p == "/api/v2/torrents/info" {
                return ok(json!([w.torrent.lock().unwrap().clone()]));
            }
            if p.starts_with("/api/v2/torrents/trackers") {
                let st = *w.tracker_status.lock().unwrap();
                return ok(json!([{"url": "** [DHT] **", "status": 0}, {"url": "https://reactor.filelist.io/x/announce", "status": st}]));
            }
            if p.starts_with("/api/v2/torrents/files") {
                return ok(json!([{"name": "Dune.2021.2160p/Dune.mkv", "size": 1}, {"name": "Dune.2021.2160p/Sample/s.mkv", "size": 1}]));
            }
            if p.starts_with("/api/v2/torrents/delete") {
                return ok(json!({}));
            }
            if p.starts_with("/api/v3/history") {
                if sonarr {
                    return ok(json!({"records": [{"seriesId": 4, "episodeId": 41}, {"seriesId": 4, "episodeId": 42}]}));
                }
                return w.radarr_history.lock().unwrap().clone();
            }
            if p == "/api/v3/movie" {
                return w.movies.lock().unwrap().clone();
            }
            if p == "/api/v3/series" {
                return ok(w.sonarr_series.clone());
            }
            if p.starts_with("/api/v3/episodefile") {
                return ok(json!([{"id": 141, "path": "D:\\Media\\Seriale\\Show\\S01\\e1.mkv"}, {"id": 142, "path": "D:\\Media\\Seriale\\Show\\S01\\e2.mkv"}]));
            }
            if p.starts_with("/api/v3/episode?") {
                return ok(json!([{"id": 41, "episodeFileId": 141}, {"id": 42, "episodeFileId": 142}]));
            }
            if p.starts_with("/library/sections/1/all") {
                let v = *w.plex_movie_views.lock().unwrap();
                return ok(json!({"MediaContainer": {"Metadata": [{"viewCount": v, "Media": [{"Part": [{"file": "D:\\Media\\Movies\\Dune (2021)\\Dune.mkv"}]}]}]}}));
            }
            if p.starts_with("/library/sections/2/all") {
                let md: Vec<Value> = [1, 2].iter().map(|n| json!({"viewCount": i64::from(w.sonarr_eps_watched.contains(n)), "Media": [{"Part": [{"file": format!("D:\\Media\\Seriale\\Show\\S01\\e{n}.mkv")}]}]})).collect();
                return ok(json!({"MediaContainer": {"Metadata": md}}));
            }
            status(404)
        })
        .await
    }

    async fn setup(w: World, dry: bool) -> (Fake, Arc<AppState>, Arc<World>, Keeper) {
        let w = Arc::new(w);
        let f = serve(w.clone()).await;
        let (a, _d) = app(&f.base, dry, json!({}));
        let k = Keeper::new(&a);
        (f, a, w, k)
    }

    fn entry(a: &AppState) -> Value {
        load_ledger(&ledger_path(a)).get(H).cloned().unwrap_or(Value::Null)
    }

    fn set_entry(a: &AppState, e: Value) {
        let mut m = Map::new();
        m.insert(H.into(), e);
        save_ledger(&ledger_path(a), &m);
    }

    fn deletes(f: &Fake) -> Vec<Req> {
        f.find("POST", "/api/v2/torrents/delete")
    }

    #[tokio::test]
    async fn new_entry_starts_at_qbittorrent_seeding_time_then_accrues_capped() {
        let (_f, a, _w, mut k) = setup(world(), false).await;
        tick(&a, &mut k, 10_000.0).await;
        assert_eq!(entry(&a)["seconds"], json!(1000.0));
        // 1 hour later: only 10 minutes are added.
        tick(&a, &mut k, 13_600.0).await;
        assert_eq!(entry(&a)["seconds"], json!(1600.0));
        // 5 minutes later: 5 minutes.
        tick(&a, &mut k, 13_900.0).await;
        assert_eq!(entry(&a)["seconds"], json!(1900.0));
        assert_eq!(entry(&a)["movie_id"], json!(10));
        let snap = snapshot(&a);
        assert_eq!(snap[H], SeedInfo { seconds: 1900.0, watched: Some(false), waiting_watch: false });
    }

    #[tokio::test]
    async fn no_accrual_without_a_working_tracker_or_while_stopped() {
        let (_f, a, w, mut k) = setup(world(), false).await;
        tick(&a, &mut k, 0.0).await;
        *w.tracker_status.lock().unwrap() = 4;
        tick(&a, &mut k, 300.0).await;
        assert_eq!(entry(&a)["seconds"], json!(1000.0));
        *w.tracker_status.lock().unwrap() = 2;
        *w.torrent.lock().unwrap() = torrent("radarr", "stoppedUP", 1000, 0.1);
        tick(&a, &mut k, 600.0).await;
        assert_eq!(entry(&a)["seconds"], json!(1000.0));
        *w.torrent.lock().unwrap() = torrent("radarr", "uploading", 1000, 0.1);
        tick(&a, &mut k, 900.0).await;
        assert_eq!(entry(&a)["seconds"], json!(1300.0));
    }

    #[tokio::test]
    async fn qbittorrent_down_accrues_nothing() {
        let f = fake(|_r: &Req| status(500)).await;
        let (a, _d) = app(&f.base, false, json!({}));
        let mut k = Keeper::new(&a);
        set_entry(&a, json!({"seconds": 5.0, "last_tick": 0.0}));
        tick(&a, &mut k, 300.0).await;
        assert_eq!(entry(&a)["seconds"], json!(5.0));
    }

    #[tokio::test]
    async fn hr_and_watched_deletes_with_files() {
        let (f, a, w, mut k) = setup(world(), false).await;
        *w.plex_movie_views.lock().unwrap() = 1;
        set_entry(&a, json!({"seconds": HR_SECONDS - 100.0, "last_tick": 0.0}));
        tick(&a, &mut k, 300.0).await;
        let d = deletes(&f);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].body, format!("hashes={H}&deleteFiles=true"));
        assert_eq!(entry(&a), Value::Null);
        // The history lookup used the upper-case hash.
        assert!(f.find("GET", "/api/v3/history").iter().all(|r| r.path.contains(&H.to_uppercase())));
    }

    #[tokio::test]
    async fn ratio_one_counts_as_hr() {
        let (f, a, w, mut k) = setup(world(), false).await;
        *w.plex_movie_views.lock().unwrap() = 3;
        *w.torrent.lock().unwrap() = torrent("radarr", "stalledUP", 10, 1.0);
        tick(&a, &mut k, 0.0).await;
        assert_eq!(deletes(&f).len(), 1);
    }

    #[tokio::test]
    async fn hr_but_unwatched_keeps_and_waits() {
        let (f, a, _w, mut k) = setup(world(), false).await;
        set_entry(&a, json!({"seconds": HR_SECONDS + 1.0, "last_tick": 0.0}));
        tick(&a, &mut k, 300.0).await;
        assert!(deletes(&f).is_empty());
        let s = snapshot(&a);
        assert_eq!(s[H].watched, Some(false));
        assert!(s[H].waiting_watch);
    }

    #[tokio::test]
    async fn watched_but_hr_pending_keeps() {
        let (f, a, w, mut k) = setup(world(), false).await;
        *w.plex_movie_views.lock().unwrap() = 1;
        tick(&a, &mut k, 0.0).await;
        assert!(deletes(&f).is_empty());
        assert_eq!(snapshot(&a)[H], SeedInfo { seconds: 1000.0, watched: Some(true), waiting_watch: false });
    }

    #[tokio::test]
    async fn removed_from_radarr_deletes_using_the_remembered_mapping() {
        let (f, a, w, mut k) = setup(world(), false).await;
        tick(&a, &mut k, 0.0).await; // mapping remembered: movie 10
        // Radarr deleted the movie and its history.
        *w.radarr_history.lock().unwrap() = ok(json!({"records": []}));
        *w.movies.lock().unwrap() = ok(json!([{"id": 11}]));
        let mut e = entry(&a);
        e["seconds"] = json!(HR_SECONDS);
        set_entry(&a, e);
        tick(&a, &mut k, 300.0).await;
        assert_eq!(deletes(&f).len(), 1);
    }

    #[tokio::test]
    async fn api_errors_and_unknown_mappings_keep() {
        for case in 0..4 {
            let (f, a, w, mut k) = setup(world(), false).await;
            *w.plex_movie_views.lock().unwrap() = 1;
            match case {
                0 => *w.radarr_history.lock().unwrap() = status(500),
                1 => *w.radarr_history.lock().unwrap() = ok(json!({"records": []})),
                2 => *w.movies.lock().unwrap() = status(503),
                _ => *w.movies.lock().unwrap() = ok(json!([])), // empty library is never "gone"
            }
            set_entry(&a, json!({"seconds": HR_SECONDS + 1.0, "last_tick": 0.0}));
            tick(&a, &mut k, 300.0).await;
            assert!(deletes(&f).is_empty(), "case {case}");
            assert_eq!(snapshot(&a)[H].watched, None, "case {case}");
        }
    }

    #[tokio::test]
    async fn wrong_category_or_path_keeps() {
        for t in [
            torrent("tv", "stalledUP", 999_999, 5.0),
            {
                let mut t = torrent("radarr", "stalledUP", 999_999, 5.0);
                t["content_path"] = json!("D:\\Media\\Movies\\Dune (2021)");
                t
            },
            {
                let mut t = torrent("radarr", "stalledUP", 999_999, 5.0);
                t["content_path"] = json!("D:\\Media\\Downloads\\..\\Movies\\Dune");
                t
            },
            {
                let mut t = torrent("radarr", "stalledUP", 999_999, 5.0);
                t["save_path"] = json!("D:\\Media\\DownloadsX");
                t
            },
        ] {
            let mut w = world();
            *w.plex_movie_views.get_mut().unwrap() = 1;
            *w.torrent.get_mut().unwrap() = t;
            let (f, a, _w, mut k) = setup(w, false).await;
            tick(&a, &mut k, 0.0).await;
            assert!(deletes(&f).is_empty());
        }
    }

    #[tokio::test]
    async fn sonarr_all_episodes_watched_deletes_one_unwatched_keeps() {
        for (watched, expect) in [(vec![1, 2], 1), (vec![1], 0)] {
            let mut w = world();
            w.sonarr_eps_watched = watched;
            let mut t = torrent("sonarr", "stalledUP", 999_999, 0.0);
            t["content_path"] = json!("D:\\Media\\Downloads\\sonarr\\Show.S01");
            t["save_path"] = json!("D:\\Media\\Downloads\\sonarr");
            *w.torrent.get_mut().unwrap() = t;
            let (f, a, _w, mut k) = setup(w, false).await;
            tick(&a, &mut k, 0.0).await;
            assert_eq!(deletes(&f).len(), expect);
            if expect == 0 {
                assert_eq!(entry(&a)["episode_ids"], json!([41, 42]));
                assert_eq!(entry(&a)["series_id"], json!(4));
            }
        }
    }

    #[tokio::test]
    async fn ledger_survives_a_restart() {
        let (_f, a, _w, mut k) = setup(world(), false).await;
        tick(&a, &mut k, 0.0).await;
        tick(&a, &mut k, 300.0).await;
        // A new process: new keeper, snapshot cache cleared, qBittorrent lost seeding_time.
        snapshots().lock().unwrap().remove(&ledger_path(&a));
        assert_eq!(snapshot(&a)[H].seconds, 1300.0);
        let mut k2 = Keeper::new(&a);
        tick(&a, &mut k2, 600.0).await;
        assert_eq!(entry(&a)["seconds"], json!(1600.0));
        let text = std::fs::read_to_string(ledger_path(&a)).unwrap();
        assert!(text.starts_with("{\n  \"torrents\": {"), "{text}");
        assert!(!text.contains("filelist"), "tracker URLs never stored");
    }

    #[tokio::test]
    async fn dry_run_never_deletes() {
        let (f, a, w, mut k) = setup(world(), true).await;
        *w.plex_movie_views.lock().unwrap() = 1;
        set_entry(&a, json!({"seconds": HR_SECONDS + 1.0, "last_tick": 0.0}));
        tick(&a, &mut k, 300.0).await;
        assert!(f.mutations().is_empty(), "{:?}", f.mutations());
        assert!(entry(&a).is_object());
    }

    #[tokio::test]
    async fn logs_in_on_403_and_retries() {
        let f = fake(|r: &Req| {
            if r.path == "/api/v2/auth/login" {
                return (200, "Ok.".into(), vec![("Set-Cookie".into(), "SID=s3cr3t; HttpOnly; path=/".into())]);
            }
            if r.header("cookie") == Some("SID=s3cr3t") {
                return ok(json!([]));
            }
            status(403)
        })
        .await;
        let (a, _d) = app(&f.base, false, json!({}));
        let q = QbitJobs::new(a.services.qbittorrent.c.clone());
        assert_eq!(q.torrents().await.unwrap(), Vec::<Value>::new());
        let login = f.find("POST", "/api/v2/auth/login");
        assert_eq!(login.len(), 1);
        assert_eq!(login[0].body, "username=admin&password=x");
        assert_eq!(login[0].header("referer"), Some(f.base.as_str()));
    }

    /// Link counts by lower-cased path; a missing path is an open error.
    struct FakeLinks(HashMap<String, u32>);

    impl LinkCounter for FakeLinks {
        fn links(&self, path: &Path) -> Result<u32, String> {
            self.0.get(&path_key(&path.to_string_lossy())).copied().ok_or_else(|| "FileNotFoundError".into())
        }
    }

    const MAIN: &str = r"D:\Media\Downloads\radarr\Dune.2021.2160p\Dune.mkv";
    const SAMPLE: &str = r"D:\Media\Downloads\radarr\Dune.2021.2160p\Sample\s.mkv";

    fn links(main: Option<u32>, sample: Option<u32>) -> Box<FakeLinks> {
        let mut m = HashMap::new();
        if let Some(c) = main {
            m.insert(path_key(MAIN), c);
        }
        if let Some(c) = sample {
            m.insert(path_key(SAMPLE), c);
        }
        Box::new(FakeLinks(m))
    }

    #[test]
    fn link_count_decision_table() {
        let names = vec!["Dune.2021.2160p/Dune.mkv".to_string(), "Dune.2021.2160p/Sample/s.mkv".to_string()];
        let (save, content, root) = (r"D:\Media\Downloads\radarr\\", r"D:\Media\Downloads\radarr\Dune.2021.2160p", r"D:\Media\Downloads");
        let chk = |l: &FakeLinks, n: &[String]| no_library_copy(save, content, root, n, l);
        assert_eq!(chk(&links(Some(1), Some(1)), &names), Ok(()));
        assert!(chk(&links(Some(2), Some(1)), &names).unwrap_err().contains("2 links"));
        assert!(chk(&links(Some(1), Some(3)), &names).is_err());
        assert!(chk(&links(Some(1), None), &names).unwrap_err().contains("FileNotFoundError"));
        assert!(chk(&links(Some(1), Some(1)), &[]).is_err());
        let outside = vec!["../../Movies/Dune/Dune.mkv".to_string()];
        assert!(chk(&links(Some(1), Some(1)), &outside).unwrap_err().contains("outside"));
        let other = vec!["Other/x.mkv".to_string()];
        assert!(chk(&links(Some(1), Some(1)), &other).unwrap_err().contains("outside"));
        // A single-file torrent: content_path is the file itself.
        let single = vec!["Dune.mkv".to_string()];
        let mut m = HashMap::new();
        m.insert(path_key(r"D:\Media\Downloads\radarr\Dune.mkv"), 1);
        assert_eq!(no_library_copy(save, r"D:\Media\Downloads\radarr\Dune.mkv", root, &single, &FakeLinks(m)), Ok(()));
    }

    #[test]
    fn win32_link_count_reader() {
        let d = crate::jobs::testutil::temp_dir("links");
        let a = d.join("a.mkv");
        std::fs::write(&a, b"x").unwrap();
        assert_eq!(Win32Links.links(&a), Ok(1));
        std::fs::hard_link(&a, d.join("b.mkv")).unwrap();
        assert_eq!(Win32Links.links(&a), Ok(2));
        assert_eq!(Win32Links.links(&d.join("b.mkv")), Ok(2));
        assert!(Win32Links.links(&d.join("missing.mkv")).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    async fn unmapped_tick(dry: bool, l: Box<FakeLinks>, history: Resp) -> (Fake, Arc<AppState>) {
        let w = world();
        *w.radarr_history.lock().unwrap() = history;
        let (f, a, _w, mut k) = setup(w, dry).await;
        k.links = l;
        set_entry(&a, json!({"seconds": HR_SECONDS + 1.0, "last_tick": 0.0}));
        tick(&a, &mut k, 300.0).await;
        (f, a)
    }

    #[tokio::test]
    async fn unmapped_with_no_library_copy_is_deleted() {
        let (f, a) = unmapped_tick(false, links(Some(1), Some(1)), ok(json!({"records": []}))).await;
        assert_eq!(deletes(&f).len(), 1);
        assert_eq!(entry(&a), Value::Null);
    }

    #[tokio::test]
    async fn unmapped_with_a_library_link_or_missing_file_is_kept() {
        for l in [links(Some(2), Some(1)), links(Some(1), None), links(None, None)] {
            let (f, _a) = unmapped_tick(false, l, ok(json!({"records": []}))).await;
            assert!(deletes(&f).is_empty());
        }
    }

    #[tokio::test]
    async fn unmapped_but_hr_pending_is_kept() {
        let w = world();
        *w.radarr_history.lock().unwrap() = ok(json!({"records": []}));
        let (f, a, _w, mut k) = setup(w, false).await;
        k.links = links(Some(1), Some(1));
        tick(&a, &mut k, 0.0).await;
        assert!(deletes(&f).is_empty());
        assert!(f.find("GET", "/api/v2/torrents/files").is_empty());
    }

    #[tokio::test]
    async fn fallback_never_applies_to_known_or_errored_mappings() {
        // Known mapping, unwatched: kept even with link count 1.
        let (f, _a) = unmapped_tick(false, links(Some(1), Some(1)), ok(json!({"records": [{"movieId": 10}]}))).await;
        assert!(deletes(&f).is_empty());
        assert!(f.find("GET", "/api/v2/torrents/files").is_empty());
        // History error: kept.
        let (f, _a) = unmapped_tick(false, links(Some(1), Some(1)), status(500)).await;
        assert!(deletes(&f).is_empty());
        // History with records but no movie id: kept.
        let (f, _a) = unmapped_tick(false, links(Some(1), Some(1)), ok(json!({"records": [{"eventType": "grabbed"}]}))).await;
        assert!(deletes(&f).is_empty());
    }

    #[tokio::test]
    async fn unmapped_dry_run_only_logs() {
        crate::log::start_capture();
        let (f, _a) = unmapped_tick(true, links(Some(1), Some(1)), ok(json!({"records": []}))).await;
        assert!(f.mutations().is_empty());
        assert!(crate::log::captured().iter().any(|l| l.contains("dry-run: would delete torrent 'Dune.2021.2160p'") && l.contains("no library copy (link count 1)")));
    }

    #[test]
    fn pure_rules() {
        assert_eq!(accrue(10.0, 0.0, 5000.0, true, true), 610.0);
        assert_eq!(accrue(10.0, 100.0, 50.0, true, true), 10.0);
        assert_eq!(accrue(10.0, 0.0, 300.0, false, true), 10.0);
        assert_eq!(accrue(10.0, 0.0, 300.0, true, false), 10.0);
        assert!(hr_done(HR_SECONDS, 0.0) && hr_done(0.0, 1.0) && !hr_done(48.0 * 3600.0, 0.99));
        assert!(under_root("d:/media/downloads/radarr/x", "D:\\Media\\Downloads"));
        assert!(!under_root("D:\\Media\\Downloads", "D:\\Media\\Downloads"));
        assert!(!under_root("D:\\Media\\DownloadsX\\a", "D:\\Media\\Downloads"));
        assert!(!under_root("", "D:\\Media\\Downloads"));
    }
}
