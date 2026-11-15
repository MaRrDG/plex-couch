//! Library cleanup, every 5 minutes, first run 4 minutes after start: removes what you have
//! watched from the library itself. The seed keeper deletes only the torrent's download copy
//! (`D:\Media\Downloads`); the library hardlink (`D:\Media\Seriale`, `D:\Media\Movies`) stays
//! until this job deletes it.
//!
//! ## Series (Sonarr + Plex section 2)
//! Per Sonarr series, per season >= 1 (specials, season 0, are never touched), the season's
//! episode files are deleted only when ALL hold:
//! - the season has at least one episode file, and every aired episode of it has a file (an
//!   episode without a file must have an air date in the future; an undated one keeps the
//!   season), so a half-downloaded season is never cleaned; Sonarr's episodes and episode
//!   files of the season must agree (same file ids, `hasFile` consistent);
//! - every episode file of the season is watched in Plex section 2 (matched by file path,
//!   `viewCount > 0` and a `lastViewedAt`), and the most recent `lastViewedAt` among them is
//!   at least `grace_hours` ago;
//! - every episode file has a hard-link count of exactly 1 (the torrent's download copy is
//!   gone: the H&R seeding is over and deleting frees the space);
//! - it is NOT the highest-numbered season of the series that has any episode file (that one
//!   always stays: the watchlist's next-season step reads its watched episodes);
//! - every file is under the series folder and none under the downloads root.
//!
//! Action, in this order (a failure stops at that step, never leaving a monitored season
//! without files): `PUT /episode/monitor` `monitored: false` for every episode of the season,
//! the season's `monitored: false` on the series (`GET` + `PUT /series/<id>`), then
//! `DELETE /episodefile/bulk`. Afterwards, in each folder of a deleted file (only inside the
//! series folder): the subtitle sidecars of the deleted videos (`<stem>.*.srt|ass|ssa|sub|idx|vtt`,
//! e.g. Bazarr's `<stem>.ro.srt`; never one that belongs to another video still in the
//! folder) are removed, then the season folder itself if it is empty. Plex refreshes the
//! series folder.
//!
//! ## Movies (Radarr + Plex section 1)
//! A Radarr movie is deleted when ALL hold: it has a movie file, under the movie's folder and
//! not under the downloads root; that file is watched in Plex section 1 (`viewCount > 0`) and
//! its `lastViewedAt` is at least `grace_hours` ago; its hard-link count is 1. Action:
//! `DELETE /movie/<id>?deleteFiles=true&addImportListExclusion=true` (the exclusion keeps the
//! Plex Watchlist import list from adding it again). If the movie folder is still there with
//! only subtitle files of that movie, they and the folder are removed. Plex refreshes the
//! folder above the movie's.
//!
//! ## Safety
//! At most ONE deletion (one season or one movie) per tick; series first. Plex failing skips
//! the whole tick; a failed Sonarr/Radarr fetch, an unknown mapping, a missing file or a failed
//! link query keeps the item. After a deletion: one INFO line and one ntfy message
//! ("Am șters sezonul N din <serial> (X,X GB), l-ai văzut" / "Am șters filmul <titlu> (X,X GB),
//! l-ai văzut"; the size is the sum of the *arr `size` fields).
//!
//! Dry-run (`jobs.library_cleanup.dry_run`, default true, or the global `app.dry_run`): no
//! PUT/DELETE and no file removal; each eligible item is logged once until it changes:
//! "library cleanup (dry-run): would delete season N of <series> (X.X GB, k episodes)" /
//! "... would delete movie <title> (X.X GB)".
//!
//! Config (`jobs.library_cleanup`, all optional): `enabled` (default `true`), `dry_run`
//! (default `true`), `grace_hours` (default `24`). The downloads root is the seed keeper's
//! `jobs.seed_keeper.downloads_root` (default `D:\Media\Downloads`).

use crate::app::AppState;
use crate::jobs::common::{id_of, job_bool, job_cfg, job_str, parse_iso_utc, unix_now, Quiet};
use crate::jobs::seed_keeper::{under_root, LinkCounter, Win32Links};
use crate::jobs::Stop;
use crate::services::arr_jobs::Arr;
use crate::services::plex_jobs::{path_key, PlexJobs};
use crate::{log_info, log_warning};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub const INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const FIRST_RUN: Duration = Duration::from_secs(4 * 60);
pub const JOB: &str = "library_cleanup";
pub const DEFAULT_GRACE_HOURS: f64 = 24.0;
const PLEX_MOVIES: u32 = 1;
const PLEX_SERIES: u32 = 2;
/// Subtitle sidecars removed next to a deleted video.
pub const SUB_EXTS: [&str; 6] = ["srt", "ass", "ssa", "sub", "idx", "vtt"];
const GIB: f64 = 1_073_741_824.0;

/// `path_key(file) -> (viewCount, lastViewedAt)` of one Plex section.
pub type Views = HashMap<String, (i64, i64)>;

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    /// The job's own `dry_run` or the global one.
    pub dry_run: bool,
    pub grace_secs: i64,
}

pub fn settings(app: &AppState) -> Settings {
    let hours = job_cfg(app, JOB, "grace_hours")
        .and_then(|v| v.as_f64())
        .filter(|h| h.is_finite() && *h >= 0.0)
        .unwrap_or(DEFAULT_GRACE_HOURS);
    Settings {
        enabled: job_bool(app, JOB, "enabled", true),
        dry_run: job_bool(app, JOB, "dry_run", true) || app.dry_run,
        grace_secs: (hours * 3600.0).round() as i64,
    }
}

/// One Sonarr episode file.
#[derive(Debug, Clone, PartialEq)]
pub struct EpFile {
    pub id: i64,
    pub path: String,
    pub size: i64,
}

/// A season that may be deleted (link counts not checked yet).
#[derive(Debug, Clone, PartialEq)]
pub struct SeasonCand {
    pub season: i64,
    /// Every episode of the season (all are unmonitored).
    pub episode_ids: Vec<i64>,
    /// Episodes that have a file.
    pub episodes_with_files: usize,
    pub files: Vec<EpFile>,
    pub size: i64,
}

/// A movie that may be deleted (link count not checked yet).
#[derive(Debug, Clone, PartialEq)]
pub struct MovieCand {
    pub id: i64,
    pub title: String,
    /// Radarr's movie folder.
    pub folder: String,
    pub file: String,
    pub size: i64,
}

fn int(v: &Value, k: &str) -> Option<i64> {
    v.get(k).and_then(Value::as_i64)
}

fn text<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}

/// `path` watched with a known `lastViewedAt`: Some(lastViewedAt).
fn watched_at(views: &Views, path: &str) -> Option<i64> {
    match views.get(&path_key(path)) {
        Some((n, last)) if *n > 0 && *last > 0 => Some(*last),
        _ => None,
    }
}

/// The seasons of one series that pass every rule but the link count (pure; see the module
/// docs). `Err` when Sonarr's data cannot be trusted at all (a file without id, season or
/// path): nothing of the series is deleted.
pub fn season_candidates(
    series_path: &str,
    downloads_root: &str,
    episodes: &[Value],
    files: &[Value],
    views: &Views,
    now: i64,
    grace: i64,
) -> Result<Vec<SeasonCand>, String> {
    let mut by_id: HashMap<i64, (i64, EpFile)> = HashMap::new();
    for f in files {
        let (Some(id), Some(season), Some(path)) = (id_of(f.get("id")), int(f, "seasonNumber"), text(f, "path")) else {
            return Err("an episode file without id, season or path".into());
        };
        by_id.insert(id, (season, EpFile { id, path: path.to_string(), size: int(f, "size").unwrap_or(0).max(0) }));
    }
    let mut highest = by_id.values().map(|(s, _)| *s).max();
    for e in episodes.iter().filter(|e| id_of(e.get("episodeFileId")).is_some()) {
        highest = highest.max(int(e, "seasonNumber"));
    }
    let Some(highest) = highest else { return Ok(Vec::new()) };
    let seasons: BTreeSet<i64> = episodes
        .iter()
        .filter_map(|e| int(e, "seasonNumber"))
        .chain(by_id.values().map(|(s, _)| *s))
        .filter(|s| *s >= 1 && *s < highest)
        .collect();
    let mut out = Vec::new();
    'season: for s in seasons {
        let eps: Vec<&Value> = episodes.iter().filter(|e| int(e, "seasonNumber") == Some(s)).collect();
        let mut sfiles: Vec<&EpFile> = by_id.values().filter(|(fs, _)| *fs == s).map(|(_, f)| f).collect();
        sfiles.sort_by_key(|f| f.id);
        if sfiles.is_empty() || eps.is_empty() {
            continue;
        }
        let mut referenced = BTreeSet::new();
        let mut ids = Vec::new();
        let mut with_files = 0;
        for e in &eps {
            let Some(eid) = id_of(e.get("id")) else { continue 'season };
            ids.push(eid);
            match (id_of(e.get("episodeFileId")), e.get("hasFile").and_then(Value::as_bool)) {
                // Sonarr contradicts itself: keep.
                (Some(_), Some(false)) | (None, Some(true)) => continue 'season,
                (Some(fid), _) => {
                    referenced.insert(fid);
                    with_files += 1;
                }
                (None, _) => {
                    // A missing episode that has aired (or has no air date): incomplete season.
                    let aired = text(e, "airDateUtc").and_then(parse_iso_utc);
                    if aired.is_none_or(|t| t <= now) {
                        continue 'season;
                    }
                }
            }
        }
        if referenced != sfiles.iter().map(|f| f.id).collect::<BTreeSet<_>>() {
            continue;
        }
        let mut last = 0;
        for f in &sfiles {
            if !under_root(&f.path, series_path) || under_root(&f.path, downloads_root) {
                continue 'season;
            }
            match watched_at(views, &f.path) {
                Some(l) => last = last.max(l),
                None => continue 'season,
            }
        }
        if now - last < grace {
            continue;
        }
        ids.sort_unstable();
        out.push(SeasonCand {
            season: s,
            episode_ids: ids,
            episodes_with_files: with_files,
            size: sfiles.iter().map(|f| f.size).sum(),
            files: sfiles.into_iter().cloned().collect(),
        });
    }
    Ok(out)
}

/// The movie if it passes every rule but the link count (pure; see the module docs).
pub fn movie_candidate(m: &Value, downloads_root: &str, views: &Views, now: i64, grace: i64) -> Option<MovieCand> {
    let id = id_of(m.get("id"))?;
    let folder = text(m, "path")?;
    if m.get("hasFile").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let mf = m.get("movieFile")?;
    let file = text(mf, "path")?;
    if !under_root(file, folder) || under_root(file, downloads_root) || under_root(folder, downloads_root) {
        return None;
    }
    let last = watched_at(views, file)?;
    if now - last < grace {
        return None;
    }
    Some(MovieCand {
        id,
        title: text(m, "title").unwrap_or("?").to_string(),
        folder: folder.to_string(),
        file: file.to_string(),
        size: int(mf, "size").unwrap_or(0).max(0),
    })
}

/// A Sonarr series (from the list) may have a season below its highest one with files. Only
/// a shortcut to skip the episode fetches: unknown statistics -> true.
fn may_have_old_season(series: &Value) -> bool {
    let Some(seasons) = series.get("seasons").and_then(Value::as_array) else { return true };
    let mut with_files = Vec::new();
    for s in seasons {
        let Some(n) = int(s, "seasonNumber") else { return true };
        match s.get("statistics").and_then(|x| x.get("episodeFileCount")).and_then(Value::as_i64) {
            Some(c) if c > 0 => with_files.push(n),
            Some(_) => {}
            None => return true,
        }
    }
    let top = with_files.iter().copied().max().unwrap_or(0);
    with_files.iter().any(|n| *n >= 1 && *n < top)
}

/// Every file has exactly one link. `Err` says which one does not (or could not be read).
fn links_ok(links: &dyn LinkCounter, paths: &[&str]) -> Result<(), String> {
    for p in paths {
        match links.links(Path::new(p)) {
            Ok(1) => {}
            Ok(n) => return Err(format!("{p} has {n} links (the download copy is still there)")),
            Err(e) => return Err(format!("{p}: link count unknown ({e})")),
        }
    }
    Ok(())
}

fn is_sub(name: &str) -> bool {
    Path::new(name).extension().and_then(|e| e.to_str()).is_some_and(|e| SUB_EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

fn lower_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn lower_stem(p: &Path) -> Option<String> {
    p.file_stem().map(|s| s.to_string_lossy().to_lowercase())
}

/// Remove the subtitle sidecars (`<stem>.<anything>.<sub ext>` or `<stem>.<sub ext>`) of the
/// deleted videos `deleted` in `folder`. Only videos that are really gone count, and a
/// sidecar whose name matches a longer stem of another file still in the folder belongs to
/// that file and stays. Returns the removed paths.
pub fn remove_sidecars(folder: &Path, deleted: &[PathBuf]) -> Vec<PathBuf> {
    let ours: Vec<String> =
        deleted.iter().filter(|v| v.parent() == Some(folder) && !v.exists()).filter_map(|v| lower_stem(v)).collect();
    if ours.is_empty() {
        return Vec::new();
    }
    let Ok(rd) = std::fs::read_dir(folder) else { return Vec::new() };
    let entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect();
    let others: Vec<String> =
        entries.iter().filter(|p| !is_sub(&lower_name(p))).filter_map(|p| lower_stem(p)).filter(|s| !ours.contains(s)).collect();
    let longest = |stems: &[String], name: &str| stems.iter().filter(|s| name.starts_with(&format!("{s}."))).map(String::len).max();
    let mut removed = Vec::new();
    for p in &entries {
        let name = lower_name(p);
        if !is_sub(&name) {
            continue;
        }
        let Some(mine) = longest(&ours, &name) else { continue };
        if longest(&others, &name).is_some_and(|o| o >= mine) {
            continue;
        }
        if std::fs::remove_file(p).is_ok() {
            removed.push(p.clone());
        }
    }
    removed
}

/// Remove `dir` if it exists and is empty.
fn remove_if_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir).map(|mut r| r.next().is_none()).unwrap_or(false) && std::fs::remove_dir(dir).is_ok()
}

/// After Radarr deleted the movie: if `folder` is still there holding only subtitle files of
/// the (gone) `video`, remove them and the folder. True if the folder is gone.
pub fn clean_movie_folder(folder: &Path, video: &Path) -> bool {
    if !folder.exists() {
        return true;
    }
    let Some(stem) = lower_stem(video) else { return false };
    if video.exists() {
        return false;
    }
    let Ok(rd) = std::fs::read_dir(folder) else { return false };
    let entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    let ours = |p: &Path| p.is_file() && is_sub(&lower_name(p)) && lower_name(p).starts_with(&format!("{stem}."));
    if !entries.iter().all(|p| ours(p)) {
        return false;
    }
    entries.iter().all(|p| std::fs::remove_file(p).is_ok()) && std::fs::remove_dir(folder).is_ok()
}

/// `1.5` (logs) from bytes, in GiB like the other jobs.
fn gb(bytes: i64) -> String {
    format!("{:.1}", crate::json::py_round(bytes as f64 / GIB, 1))
}

/// `1,5` (the Romanian ntfy text).
fn gb_ro(bytes: i64) -> String {
    gb(bytes).replace('.', ",")
}

pub struct Cleaner {
    pub links: Box<dyn LinkCounter>,
    quiet: Quiet,
    /// The dry-run lines logged on the previous tick (cleared when the item drops out).
    dry_seen: HashSet<String>,
}

impl Default for Cleaner {
    fn default() -> Self {
        Cleaner { links: Box::new(Win32Links), quiet: Quiet::default(), dry_seen: HashSet::new() }
    }
}

/// Start the loop.
pub fn spawn(app: Arc<AppState>, mut stop: Stop) {
    tokio::spawn(async move {
        if stop.sleep(FIRST_RUN).await {
            return;
        }
        let mut c = Cleaner::default();
        loop {
            tick(&app, &mut c, unix_now() as i64).await;
            if stop.sleep(INTERVAL).await {
                return;
            }
        }
    });
}

/// One run at `now` (unix seconds).
pub async fn tick(app: &AppState, c: &mut Cleaner, now: i64) {
    let cfg = settings(app);
    if !cfg.enabled {
        return;
    }
    let root = job_str(app, "seed_keeper", "downloads_root", "D:\\Media\\Downloads");
    let plex = PlexJobs::new(app.services.plex.c.clone());
    let mut views = Vec::new();
    for (section, episodes) in [(PLEX_SERIES, true), (PLEX_MOVIES, false)] {
        match plex.view_info(section, episodes).await {
            Ok(v) => views.push(v),
            Err(e) => {
                c.quiet.warn("plex", format!("library cleanup: Plex section {section} failed, nothing done: {e}"));
                return;
            }
        }
    }
    if c.quiet.clear("plex") {
        log_info!("library cleanup: Plex reachable again");
    }
    let movie_views = views.pop().unwrap_or_default();
    let ep_views = views.pop().unwrap_or_default();
    let mut seen = HashSet::new();
    let acted = series_pass(app, c, &cfg, &root, &ep_views, now, &mut seen).await;
    if !acted {
        movie_pass(app, c, &cfg, &root, &movie_views, now, &mut seen).await;
    }
    if cfg.dry_run {
        for k in c.dry_seen.difference(&seen) {
            c.quiet.clear(k);
        }
        c.dry_seen = seen;
    }
}

/// True if a deletion was attempted (the tick ends).
async fn series_pass(app: &AppState, c: &mut Cleaner, cfg: &Settings, root: &str, views: &Views, now: i64, seen: &mut HashSet<String>) -> bool {
    let sonarr = app.services.sonarr.arr();
    let mut list = match sonarr.items().await {
        Ok(l) => {
            c.quiet.clear("sonarr");
            l
        }
        Err(e) => {
            c.quiet.warn("sonarr", format!("library cleanup: Sonarr series failed, no season deleted: {e}"));
            return false;
        }
    };
    list.sort_by_key(|s| id_of(s.get("id")).unwrap_or(0));
    for s in &list {
        let (Some(id), Some(path)) = (id_of(s.get("id")), text(s, "path")) else { continue };
        let title = text(s, "title").unwrap_or("?").to_string();
        if !may_have_old_season(s) {
            continue;
        }
        let key = format!("eps-{id}");
        let (eps, files) = match (sonarr.episodes(id).await, sonarr.episode_files(id).await) {
            (Ok(e), Ok(f)) => {
                c.quiet.clear(&key);
                (e, f)
            }
            (Err(e), _) | (_, Err(e)) => {
                c.quiet.warn(&key, format!("library cleanup: Sonarr episodes of '{title}' failed, kept: {e}"));
                continue;
            }
        };
        let cands = match season_candidates(path, root, &eps, &files, views, now, cfg.grace_secs) {
            Ok(x) => {
                c.quiet.clear(&format!("data-{id}"));
                x
            }
            Err(e) => {
                c.quiet.warn(&format!("data-{id}"), format!("library cleanup: keeping '{title}': {e}"));
                continue;
            }
        };
        for cand in cands {
            let paths: Vec<&str> = cand.files.iter().map(|f| f.path.as_str()).collect();
            let lkey = format!("links-{id}-{}", cand.season);
            if let Err(e) = links_ok(&*c.links, &paths) {
                c.quiet.info(&lkey, format!("library cleanup: keeping season {} of {title} (watched): {e}", cand.season));
                continue;
            }
            c.quiet.clear(&lkey);
            if cfg.dry_run {
                let k = format!("dry-season-{id}-{}", cand.season);
                c.quiet.info(
                    &k,
                    format!(
                        "library cleanup (dry-run): would delete season {} of {title} ({} GB, {} episodes)",
                        cand.season,
                        gb(cand.size),
                        cand.episodes_with_files
                    ),
                );
                seen.insert(k);
                continue;
            }
            delete_season(app, &sonarr, &mut c.quiet, id, &title, path, &cand).await;
            return true;
        }
    }
    false
}

async fn delete_season(app: &AppState, sonarr: &Arr, quiet: &mut Quiet, id: i64, title: &str, series_path: &str, cand: &SeasonCand) {
    let n = cand.season;
    let key = format!("act-season-{id}-{n}");
    // A step that keeps failing is logged once, not every tick.
    let mut failed = |step: &str, e: String| quiet.warn(&key, format!("library cleanup: season {n} of {title}: {step} failed, stopped: {e}"));
    // 1. Unmonitor first: a failure later never leaves a monitored season without files.
    if let Err(e) = sonarr.set_episodes_monitored(&cand.episode_ids, false).await {
        return failed("unmonitoring the episodes", e.to_string());
    }
    let mut series = match sonarr.series(id).await {
        Ok(s) => s,
        Err(e) => return failed("reading the series", e.to_string()),
    };
    let season = series
        .get_mut("seasons")
        .and_then(Value::as_array_mut)
        .and_then(|a| a.iter_mut().find(|x| int(x, "seasonNumber") == Some(n)));
    let Some(season) = season else { return failed("unmonitoring the season", "not in the series".into()) };
    season["monitored"] = Value::Bool(false);
    if let Err(e) = sonarr.put_series(id, &series).await {
        return failed("unmonitoring the season", e.to_string());
    }
    // 2. The files.
    let ids: Vec<i64> = cand.files.iter().map(|f| f.id).collect();
    if let Err(e) = sonarr.delete_episode_files(&ids).await {
        return failed("deleting the episode files (the season stays unmonitored)", e.to_string());
    }
    quiet.clear(&key);
    // 3. Sidecars and the empty season folder, only inside the series folder.
    let mut by_folder: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for f in &cand.files {
        let p = PathBuf::from(&f.path);
        if let Some(dir) = p.parent() {
            by_folder.entry(dir.to_path_buf()).or_default().push(p.clone());
        }
    }
    let mut subs = 0;
    for (dir, videos) in &by_folder {
        let d = dir.to_string_lossy();
        let inside = under_root(&d, series_path);
        if !inside && path_key(&d) != path_key(series_path) {
            continue;
        }
        subs += remove_sidecars(dir, videos).len();
        if inside {
            remove_if_empty(dir);
        }
    }
    if let Err(e) = PlexJobs::new(app.services.plex.c.clone()).refresh_path(PLEX_SERIES, series_path).await {
        log_warning!("library cleanup: Plex refresh of {series_path} failed: {e}");
    }
    log_info!(
        "library cleanup: deleted season {n} of {title} ({} GB, {} episodes, {subs} subtitle file(s)), watched",
        gb(cand.size),
        cand.episodes_with_files
    );
    let msg = format!("Am șters sezonul {n} din {title} ({} GB), l-ai văzut", gb_ro(cand.size));
    app.notifier.send("Bibliotecă", &msg, Some(vec!["wastebasket".into()]), None, crate::services::ntfy::TIMEOUT).await;
}

/// True if a deletion was attempted.
async fn movie_pass(app: &AppState, c: &mut Cleaner, cfg: &Settings, root: &str, views: &Views, now: i64, seen: &mut HashSet<String>) -> bool {
    let radarr = Arr::radarr(app.services.radarr.c.clone());
    let mut list = match radarr.items().await {
        Ok(l) => {
            c.quiet.clear("radarr");
            l
        }
        Err(e) => {
            c.quiet.warn("radarr", format!("library cleanup: Radarr movies failed, no movie deleted: {e}"));
            return false;
        }
    };
    list.sort_by_key(|m| id_of(m.get("id")).unwrap_or(0));
    for m in &list {
        let Some(cand) = movie_candidate(m, root, views, now, cfg.grace_secs) else { continue };
        let lkey = format!("links-movie-{}", cand.id);
        if let Err(e) = links_ok(&*c.links, &[cand.file.as_str()]) {
            c.quiet.info(&lkey, format!("library cleanup: keeping movie {} (watched): {e}", cand.title));
            continue;
        }
        c.quiet.clear(&lkey);
        if cfg.dry_run {
            let k = format!("dry-movie-{}", cand.id);
            c.quiet.info(&k, format!("library cleanup (dry-run): would delete movie {} ({} GB)", cand.title, gb(cand.size)));
            seen.insert(k);
            continue;
        }
        delete_movie(app, &radarr, &mut c.quiet, root, &cand).await;
        return true;
    }
    false
}

async fn delete_movie(app: &AppState, radarr: &Arr, quiet: &mut Quiet, root: &str, cand: &MovieCand) {
    let key = format!("act-movie-{}", cand.id);
    if let Err(e) = radarr.delete_movie_excluded(cand.id).await {
        quiet.warn(&key, format!("library cleanup: deleting movie {} failed, kept: {e}", cand.title));
        return;
    }
    quiet.clear(&key);
    let video = PathBuf::from(&cand.file);
    if let Some(dir) = video.parent() {
        let d = dir.to_string_lossy();
        let in_movie = path_key(&d) == path_key(&cand.folder) || under_root(&d, &cand.folder);
        if in_movie && !under_root(&d, root) && !clean_movie_folder(dir, &video) {
            log_info!("library cleanup: {} still has other files, left as is", dir.display());
        }
    }
    if let Some(parent) = Path::new(&cand.folder).parent().map(|p| p.to_string_lossy().to_string()).filter(|p| !p.is_empty()) {
        if let Err(e) = PlexJobs::new(app.services.plex.c.clone()).refresh_path(PLEX_MOVIES, &parent).await {
            log_warning!("library cleanup: Plex refresh of {parent} failed: {e}");
        }
    }
    log_info!("library cleanup: deleted movie {} ({} GB), watched", cand.title, gb(cand.size));
    let msg = format!("Am șters filmul {} ({} GB), l-ai văzut", cand.title, gb_ro(cand.size));
    app.notifier.send("Bibliotecă", &msg, Some(vec!["wastebasket".into()]), None, crate::services::ntfy::TIMEOUT).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::testutil::{app, fake, ok, status, temp_dir, Fake, Req};
    use serde_json::json;
    use std::sync::Mutex;

    const NOW: i64 = 1_800_000_000;
    const HOUR: i64 = 3600;
    const SIZE: i64 = 1_073_741_824;
    const PAST: &str = "2020-01-01T00:00:00Z";

    struct World {
        dir: PathBuf,
        series_path: PathBuf,
        series: Mutex<Value>,
        episodes: Mutex<Vec<Value>>,
        files: Mutex<Vec<Value>>,
        /// (path, viewCount, lastViewedAt) for Plex section 2.
        ep_views: Mutex<Vec<(String, i64, i64)>>,
        movies: Mutex<Value>,
        movie_views: Mutex<Vec<(String, i64, i64)>>,
        plex_ok: Mutex<bool>,
        /// The answer to the season's `PUT /series/4` (unmonitoring).
        put_series: Mutex<u16>,
    }

    impl World {
        fn ep_path(&self, season: i64, n: i64) -> PathBuf {
            self.series_path.join(format!("Season {season:02}")).join(format!("Show.S{season:02}E{n:02}.mkv"))
        }

        /// Adds `count` episodes with files (on disk, watched 25 h ago) to `season`.
        fn add_season(&self, season: i64, count: i64) {
            for n in 1..=count {
                let p = self.ep_path(season, n);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, b"x").unwrap();
                let (eid, fid) = (season * 10 + n, 100 + season * 10 + n);
                let ps = p.to_string_lossy().to_string();
                self.episodes.lock().unwrap().push(json!({"id": eid, "seasonNumber": season, "episodeNumber": n,
                    "episodeFileId": fid, "hasFile": true, "airDateUtc": PAST, "monitored": true}));
                self.files.lock().unwrap().push(json!({"id": fid, "seasonNumber": season, "path": ps, "size": SIZE}));
                self.ep_views.lock().unwrap().push((ps, 1, NOW - 25 * HOUR));
            }
            let mut s = self.series.lock().unwrap();
            let seasons = s[0]["seasons"].as_array_mut().unwrap();
            seasons.push(json!({"seasonNumber": season, "monitored": true, "statistics": {"episodeFileCount": count}}));
        }

        fn set_view(&self, path: &Path, count: i64, last: i64) {
            let k = path.to_string_lossy().to_string();
            for v in self.ep_views.lock().unwrap().iter_mut().chain(self.movie_views.lock().unwrap().iter_mut()) {
                if v.0 == k {
                    (v.1, v.2) = (count, last);
                }
            }
        }

        fn movie_path(&self) -> PathBuf {
            self.dir.join("Movies").join("Dune (2021)").join("Dune.mkv")
        }

        fn add_movie(&self) {
            let p = self.movie_path();
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"x").unwrap();
            let ps = p.to_string_lossy().to_string();
            *self.movies.lock().unwrap() = json!([{"id": 10, "title": "Dune", "hasFile": true,
                "path": p.parent().unwrap().to_string_lossy(), "movieFile": {"id": 5, "path": ps, "size": SIZE}}]);
            self.movie_views.lock().unwrap().push((ps, 1, NOW - 25 * HOUR));
        }
    }

    fn plex(views: &[(String, i64, i64)]) -> Value {
        let md: Vec<Value> = views
            .iter()
            .map(|(p, n, l)| json!({"viewCount": n, "lastViewedAt": l, "Media": [{"Part": [{"file": p}]}]}))
            .collect();
        json!({"MediaContainer": {"Metadata": md}})
    }

    fn world() -> Arc<World> {
        let dir = temp_dir("lc");
        let series_path = dir.join("Seriale").join("Show");
        let sp = series_path.to_string_lossy().to_string();
        Arc::new(World {
            dir,
            series_path,
            series: Mutex::new(json!([{"id": 4, "title": "Show", "path": sp, "seasons": [
                {"seasonNumber": 0, "monitored": false, "statistics": {"episodeFileCount": 0}}]}])),
            episodes: Mutex::new(Vec::new()),
            files: Mutex::new(Vec::new()),
            ep_views: Mutex::new(Vec::new()),
            movies: Mutex::new(json!([])),
            movie_views: Mutex::new(Vec::new()),
            plex_ok: Mutex::new(true),
            put_series: Mutex::new(200),
        })
    }

    async fn serve(w: Arc<World>) -> Fake {
        fake(move |r: &Req| {
            let p = r.path.as_str();
            let m = r.method.as_str();
            if p.starts_with("/library/sections/") {
                if !*w.plex_ok.lock().unwrap() {
                    return status(500);
                }
                if p.starts_with("/library/sections/2/all") {
                    return ok(plex(&w.ep_views.lock().unwrap()));
                }
                if p.starts_with("/library/sections/1/all") {
                    return ok(plex(&w.movie_views.lock().unwrap()));
                }
                if p.contains("/refresh?") {
                    return ok(json!({}));
                }
            }
            match (m, p) {
                ("GET", "/api/v3/series") => return ok(w.series.lock().unwrap().clone()),
                ("GET", "/api/v3/series/4") => return ok(w.series.lock().unwrap()[0].clone()),
                ("PUT", "/api/v3/series/4") => {
                    let code = *w.put_series.lock().unwrap();
                    return if code == 200 { ok(r.json()) } else { status(code) };
                }
                ("PUT", "/api/v3/episode/monitor") => return (202, String::new(), vec![]),
                ("GET", "/api/v3/movie") => return ok(w.movies.lock().unwrap().clone()),
                ("POST", "/") => return ok(json!({})),
                ("DELETE", "/api/v3/episodefile/bulk") => {
                    // Like Sonarr: the video files go, the Bazarr subtitles stay.
                    let ids: Vec<i64> = r.json()["episodeFileIds"].as_array().unwrap().iter().filter_map(Value::as_i64).collect();
                    let mut files = w.files.lock().unwrap();
                    for f in files.iter().filter(|f| ids.contains(&f["id"].as_i64().unwrap())) {
                        let _ = std::fs::remove_file(f["path"].as_str().unwrap());
                    }
                    files.retain(|f| !ids.contains(&f["id"].as_i64().unwrap()));
                    return ok(json!({}));
                }
                _ => {}
            }
            if m == "GET" && p.starts_with("/api/v3/episode?seriesId=4") {
                return ok(json!(w.episodes.lock().unwrap().clone()));
            }
            if m == "GET" && p.starts_with("/api/v3/episodefile?seriesId=4") {
                return ok(json!(w.files.lock().unwrap().clone()));
            }
            if m == "DELETE" && p.starts_with("/api/v3/movie/10?") {
                // Radarr removed the video; a subtitle is left behind.
                let _ = std::fs::remove_file(w.movie_path());
                return ok(json!({}));
            }
            status(404)
        })
        .await
    }

    /// The app with the job's dry-run off (unless `job_cfg` says otherwise) and the downloads
    /// root in the temp dir.
    async fn run(w: &Arc<World>, global_dry: bool, job_cfg: Value) -> (Fake, Arc<AppState>) {
        let f = serve(w.clone()).await;
        let dl = w.dir.join("Downloads");
        std::fs::create_dir_all(&dl).unwrap();
        let (a, _d) = app(&f.base, global_dry, json!({"jobs": {"seed_keeper": {"downloads_root": dl.to_string_lossy()}, "library_cleanup": job_cfg}}));
        let mut c = Cleaner::default();
        tick(&a, &mut c, NOW).await;
        (f, a)
    }

    async fn real(w: &Arc<World>) -> Fake {
        run(w, false, json!({"dry_run": false})).await.0
    }

    fn sequence(f: &Fake) -> Vec<String> {
        f.mutations().iter().map(|r| format!("{} {}", r.method, r.path.split('?').next().unwrap_or(""))).collect()
    }

    #[tokio::test]
    async fn watched_season_is_unmonitored_then_deleted_with_its_subtitles() {
        let w = world();
        w.add_season(1, 2);
        w.add_season(2, 1);
        let e1 = w.ep_path(1, 1);
        let season1 = e1.parent().unwrap().to_path_buf();
        for s in ["Show.S01E01.ro.srt", "Show.S01E01.en.srt", "Show.S01E02.ro.srt"] {
            std::fs::write(season1.join(s), b"1").unwrap();
        }
        let f = real(&w).await;
        assert_eq!(
            sequence(&f),
            ["PUT /api/v3/episode/monitor", "PUT /api/v3/series/4", "DELETE /api/v3/episodefile/bulk", "POST /"],
            "{:?}",
            f.mutations()
        );
        let muts = f.mutations();
        assert_eq!(muts[0].json(), json!({"episodeIds": [11, 12], "monitored": false}));
        let seasons = muts[1].json()["seasons"].clone();
        assert_eq!(seasons.as_array().unwrap().iter().find(|s| s["seasonNumber"] == 1).unwrap()["monitored"], json!(false));
        assert_eq!(seasons.as_array().unwrap().iter().find(|s| s["seasonNumber"] == 2).unwrap()["monitored"], json!(true));
        assert_eq!(muts[2].json(), json!({"episodeFileIds": [111, 112]}));
        assert!(muts[3].body.contains("Am șters sezonul 1 din Show (2,0 GB), l-ai văzut"), "{}", muts[3].body);
        assert!(!season1.exists(), "subtitles and the empty season folder are gone");
        assert!(w.ep_path(2, 1).exists(), "the last season stays");
        let refresh = f.find("GET", "/library/sections/2/refresh");
        assert_eq!(refresh.len(), 1);
        assert!(refresh[0].path.contains(&crate::services::arr_jobs::urlencode(&w.series_path.to_string_lossy())));
        let _ = std::fs::remove_dir_all(&w.dir);
    }

    #[tokio::test]
    async fn every_unmet_rule_keeps_the_season() {
        for case in 0..7 {
            let w = world();
            w.add_season(1, 2);
            if case != 3 {
                w.add_season(2, 1);
            }
            let e2 = w.ep_path(1, 2);
            match case {
                0 => w.set_view(&e2, 0, 0),                    // one episode unwatched
                1 => w.set_view(&e2, 1, NOW - 23 * HOUR),       // watched within the grace
                2 => {
                    // still seeding: the download copy is a hard link
                    std::fs::hard_link(&e2, w.dir.join("Downloads-copy.mkv")).unwrap();
                }
                3 => {}                                          // season 1 is the last with files
                4 => w.episodes.lock().unwrap().push(json!({"id": 13, "seasonNumber": 1, "episodeNumber": 3,
                        "episodeFileId": 0, "hasFile": false, "airDateUtc": PAST})), // aired, missing
                5 => w.set_view(&e2, 1, 0),                      // watched but no lastViewedAt
                _ => {
                    // not in Plex at all
                    let k = e2.to_string_lossy().to_string();
                    w.ep_views.lock().unwrap().retain(|v| v.0 != k);
                }
            }
            let f = real(&w).await;
            assert!(f.mutations().is_empty(), "case {case}: {:?}", f.mutations());
            assert!(w.ep_path(1, 1).exists() && e2.exists(), "case {case}");
            let _ = std::fs::remove_dir_all(&w.dir);
        }
    }

    #[tokio::test]
    async fn a_failed_unmonitor_deletes_nothing() {
        let w = world();
        w.add_season(1, 1);
        w.add_season(2, 1);
        *w.put_series.lock().unwrap() = 500;
        let f = real(&w).await;
        assert_eq!(sequence(&f), ["PUT /api/v3/episode/monitor", "PUT /api/v3/series/4"]);
        assert!(w.ep_path(1, 1).exists());
        let _ = std::fs::remove_dir_all(&w.dir);
    }

    #[tokio::test]
    async fn an_unaired_episode_without_a_file_does_not_block() {
        let w = world();
        w.add_season(1, 1);
        w.add_season(2, 1);
        w.episodes.lock().unwrap().push(json!({"id": 13, "seasonNumber": 1, "episodeNumber": 2,
            "episodeFileId": 0, "hasFile": false, "airDateUtc": "2099-01-01T00:00:00Z"}));
        let f = real(&w).await;
        assert_eq!(f.find("PUT", "/api/v3/episode/monitor")[0].json()["episodeIds"], json!([11, 13]));
        assert_eq!(f.find("DELETE", "/api/v3/episodefile/bulk").len(), 1);
        let _ = std::fs::remove_dir_all(&w.dir);
    }

    #[tokio::test]
    async fn files_under_the_downloads_root_are_never_touched() {
        let w = world();
        w.add_season(1, 1);
        w.add_season(2, 1);
        let f = serve(w.clone()).await;
        // The downloads root is the series' parent folder.
        let dl = w.series_path.parent().unwrap().to_string_lossy().to_string();
        let (a, _d) = app(&f.base, false, json!({"jobs": {"seed_keeper": {"downloads_root": dl}, "library_cleanup": {"dry_run": false}}}));
        tick(&a, &mut Cleaner::default(), NOW).await;
        assert!(f.mutations().is_empty());
        let _ = std::fs::remove_dir_all(&w.dir);
    }

    #[tokio::test]
    async fn dry_run_by_default_only_reads_and_logs() {
        crate::log::start_capture();
        let w = world();
        w.add_season(1, 2);
        w.add_season(2, 1);
        w.add_movie();
        let (f, _a) = run(&w, false, json!({})).await;
        assert!(f.mutations().is_empty(), "{:?}", f.mutations());
        assert!(f.requests().iter().all(|r| r.method == "GET"));
        let logs = crate::log::captured();
        assert!(logs.iter().any(|l| l.contains("library cleanup (dry-run): would delete season 1 of Show (2.0 GB, 2 episodes)")), "{logs:?}");
        assert!(logs.iter().any(|l| l.contains("library cleanup (dry-run): would delete movie Dune (1.0 GB)")));
        assert!(w.ep_path(1, 1).exists() && w.movie_path().exists());
        // The global dry-run wins over the job's `dry_run: false`.
        let (f, _a) = run(&w, true, json!({"dry_run": false})).await;
        assert!(f.mutations().is_empty(), "{:?}", f.mutations());
        // Disabled: not even a read.
        let (f, _a) = run(&w, false, json!({"enabled": false, "dry_run": false})).await;
        assert!(f.requests().is_empty());
        let _ = std::fs::remove_dir_all(&w.dir);
    }

    #[tokio::test]
    async fn plex_down_does_nothing() {
        let w = world();
        w.add_season(1, 1);
        w.add_season(2, 1);
        w.add_movie();
        *w.plex_ok.lock().unwrap() = false;
        let f = real(&w).await;
        assert!(f.mutations().is_empty());
        assert!(f.requests().iter().all(|r| r.path.starts_with("/library/")), "no Sonarr/Radarr call");
        let _ = std::fs::remove_dir_all(&w.dir);
    }

    #[tokio::test]
    async fn sonarr_errors_keep_everything() {
        let w = world();
        w.add_season(1, 1);
        w.add_season(2, 1);
        // An episode file without a season number: Sonarr's data is not trusted.
        w.files.lock().unwrap().push(json!({"id": 999, "path": "x.mkv", "size": 1}));
        let f = real(&w).await;
        assert!(f.mutations().is_empty());
        let _ = std::fs::remove_dir_all(&w.dir);
    }

    #[tokio::test]
    async fn one_deletion_per_tick() {
        let w = world();
        w.add_season(1, 1);
        w.add_season(2, 1);
        w.add_season(3, 1);
        w.add_movie();
        let f = real(&w).await;
        let del = f.find("DELETE", "");
        assert_eq!(del.len(), 1, "{del:?}");
        assert_eq!(del[0].json(), json!({"episodeFileIds": [111]}));
        assert_eq!(f.find("PUT", "/api/v3/episode/monitor").len(), 1);
        assert!(w.ep_path(2, 1).exists() && w.movie_path().exists());
        let _ = std::fs::remove_dir_all(&w.dir);
    }

    #[tokio::test]
    async fn watched_movie_is_deleted_with_an_import_list_exclusion() {
        let w = world();
        w.add_movie();
        let folder = w.movie_path().parent().unwrap().to_path_buf();
        std::fs::write(folder.join("Dune.ro.srt"), b"1").unwrap();
        let f = real(&w).await;
        assert_eq!(sequence(&f), ["DELETE /api/v3/movie/10", "POST /"]);
        assert_eq!(f.mutations()[0].path, "/api/v3/movie/10?deleteFiles=true&addImportListExclusion=true");
        assert!(f.mutations()[1].body.contains("Am șters filmul Dune (1,0 GB), l-ai văzut"));
        assert!(!folder.exists(), "the leftover subtitle and the folder are gone");
        assert_eq!(f.find("GET", "/library/sections/1/refresh").len(), 1);
        let _ = std::fs::remove_dir_all(&w.dir);
    }

    #[tokio::test]
    async fn unwatched_recent_or_seeding_movie_is_kept() {
        for case in 0..3 {
            let w = world();
            w.add_movie();
            let p = w.movie_path();
            match case {
                0 => w.set_view(&p, 0, 0),
                1 => w.set_view(&p, 2, NOW - HOUR),
                _ => std::fs::hard_link(&p, w.dir.join("Dune-seed.mkv")).unwrap(),
            }
            let f = real(&w).await;
            assert!(f.mutations().is_empty(), "case {case}");
            assert!(p.exists());
            let _ = std::fs::remove_dir_all(&w.dir);
        }
    }

    #[test]
    fn sidecars_of_other_videos_stay() {
        let d = temp_dir("lc-side");
        let gone = d.join("Show.S01E01.mkv");
        for n in ["Show.S01E01.ro.srt", "Show.S01E01.en.forced.ass", "Show.S01E01.idx", "Show.S01E01.nfo", "Show.S01E01.5.mkv",
                  "Show.S01E01.5.ro.srt", "Show.S01E02.mkv", "Show.S01E02.ro.srt", "other.ro.srt"] {
            std::fs::write(d.join(n), b"1").unwrap();
        }
        let mut removed: Vec<String> = remove_sidecars(&d, std::slice::from_ref(&gone))
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        removed.sort();
        assert_eq!(removed, ["Show.S01E01.en.forced.ass", "Show.S01E01.idx", "Show.S01E01.ro.srt"]);
        // A video that is still there keeps its subtitles.
        assert!(remove_sidecars(&d, &[d.join("Show.S01E02.mkv")]).is_empty());
        assert!(!remove_if_empty(&d));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn statistics_shortcut() {
        let s = |v: Value| may_have_old_season(&json!({"seasons": v}));
        let st = |n: i64, c: i64| json!({"seasonNumber": n, "statistics": {"episodeFileCount": c}});
        assert!(!s(json!([st(0, 3), st(1, 5)])));
        assert!(!s(json!([st(1, 0), st(2, 5)])));
        assert!(s(json!([st(1, 1), st(2, 5)])));
        assert!(s(json!([{"seasonNumber": 1}, st(2, 5)])));
        assert!(may_have_old_season(&json!({})));
    }

    #[tokio::test]
    async fn settings_defaults() {
        let (a, _d) = app("http://127.0.0.1:9", false, json!({}));
        assert_eq!(settings(&a), Settings { enabled: true, dry_run: true, grace_secs: 24 * 3600 });
        let (a, _d) = app("http://127.0.0.1:9", false, json!({"jobs": {"library_cleanup": {"dry_run": false, "grace_hours": 1.5}}}));
        assert_eq!(settings(&a), Settings { enabled: true, dry_run: false, grace_secs: 5400 });
        assert_eq!((gb(SIZE * 3 / 2), gb_ro(SIZE * 3 / 2)), ("1.5".to_string(), "1,5".to_string()));
    }
}
