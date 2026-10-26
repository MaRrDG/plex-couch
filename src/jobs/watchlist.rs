//! Watchlist sync, every 3 minutes, first run 30 s after start (replaces
//! `radarr-watchlist-sync.ps1`; plan "New behaviour" 1 and 4, spec 12.1).
//!
//! Each tick:
//! 1. Radarr and Sonarr: `ImportListSync` with `definitionId` for every enabled import list
//!    (Radarr `enabled`; Sonarr v4 lists have no `enabled`, `enableAutomaticAdd` instead).
//! 2. Auto import of queue items blocked with "matched to movie by ID" (Radarr) / "matched
//!    to series by ID" (Sonarr) and `importBlocked`: GET manualimport for that download,
//!    import the files without rejections (`ManualImport`, `importMode: auto`).
//! 3. Sonarr cleanup: delete (`deleteFiles=true`) every series that came from the Plex
//!    Watchlist and is no longer on it. Sonarr keeps no "added by list" marker on a
//!    series, so a series counts as "from the Watchlist" when its tvdb id was seen on the
//!    Watchlist at least once (`state.local.json` key `watchlist.seen_tvdb`). Guards: a
//!    failed, partial or empty Watchlist fetch, or a failed Sonarr series fetch, deletes
//!    nothing; a series whose title is still on the Watchlist is kept.
//! 4. Next season: for those series still on the Watchlist, when every episode but the
//!    last 2 of the highest monitored season is watched in Plex (section 2, matched by file
//!    path, `viewCount > 0`), monitor the next season (if it has aired episodes), run
//!    `SeasonSearch` and send "Sezonul N din <serial> se descarcă" once
//!    (`watchlist.next_season` in the state file).
//!
//! Dry-run: only GETs; every would-be action is logged.

use crate::app::AppState;
use crate::jobs::common::{id_of, job_str, parse_iso_utc, unix_now, Quiet};
use crate::jobs::Stop;
use crate::log_info;
use crate::services::arr_jobs::Arr;
use crate::services::plex_jobs::{path_key, PlexJobs, WatchItem, DISCOVER_URL};
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

pub const INTERVAL: Duration = Duration::from_secs(3 * 60);
pub const FIRST_RUN: Duration = Duration::from_secs(30);
/// Routine sync lines are logged on the first tick and then once an hour.
const VERBOSE_EVERY: u64 = 20;
pub const STATE_KEY: &str = "watchlist";
const PLEX_SERIES_SECTION: u32 = 2;

/// In-memory state of the loop.
#[derive(Default)]
pub struct Mem {
    pub ticks: u64,
    pub quiet: Quiet,
}

/// Start the loop.
pub fn spawn(app: Arc<AppState>, mut stop: Stop) {
    tokio::spawn(async move {
        if stop.sleep(FIRST_RUN).await {
            return;
        }
        let mut mem = Mem::default();
        loop {
            tick(&app, &mut mem).await;
            if stop.sleep(INTERVAL).await {
                return;
            }
        }
    });
}

/// One run of every step (errors are logged and never stop the loop).
pub async fn tick(app: &AppState, mem: &mut Mem) {
    let verbose = app.dry_run || mem.ticks.is_multiple_of(VERBOSE_EVERY);
    mem.ticks += 1;
    let radarr = Arr::radarr(app.services.radarr.c.clone());
    let sonarr = app.services.sonarr.arr();
    for arr in [&radarr, &sonarr] {
        force_sync(app, arr, verbose, &mut mem.quiet).await;
    }
    auto_import(app, &radarr, &mut mem.quiet).await;
    auto_import(app, &sonarr, &mut mem.quiet).await;
    let plex = PlexJobs::new(app.services.plex.c.clone());
    let discover = job_str(app, "watchlist", "discover_url", DISCOVER_URL);
    let watch = match plex.watchlist(&discover).await {
        Ok(w) => {
            if mem.quiet.clear("watchlist") {
                log_info!("watchlist: Plex Watchlist reachable again");
            }
            Some(w)
        }
        Err(e) => {
            mem.quiet.warn("watchlist", format!("watchlist: cannot read the Plex Watchlist: {e}"));
            None
        }
    };
    let Some(series) = sonarr_series(&sonarr, &mut mem.quiet).await else { return };
    let kept = sonarr_cleanup(app, &sonarr, watch.as_deref(), &series, verbose).await;
    if verbose {
        let (items, shows) = watch.as_deref().map(|w| (w.len(), w.iter().filter(|i| i.kind == "show").count())).unwrap_or((0, 0));
        log_info!(
            "watchlist: Plex Watchlist {items} item(s), {shows} show(s); Sonarr {} series, {} from the Watchlist",
            series.len(),
            kept.len()
        );
    }
    if let Some(w) = watch.as_deref() {
        next_season(app, &sonarr, &plex, w, &kept, &mut mem.quiet).await;
    }
}

/// Radarr `enabled`; Sonarr v4 has `enableAutomaticAdd` and no `enabled`.
pub fn list_enabled(l: &Value) -> bool {
    match l.get("enabled") {
        Some(v) => v.as_bool().unwrap_or(false),
        None => l.get("enableAutomaticAdd").and_then(Value::as_bool).unwrap_or(false),
    }
}

async fn force_sync(app: &AppState, arr: &Arr, verbose: bool, quiet: &mut Quiet) {
    let key = format!("{}-lists", arr.name);
    let lists = match arr.import_lists().await {
        Ok(l) => {
            quiet.clear(&key);
            l
        }
        Err(e) => {
            quiet.warn(&key, format!("watchlist: {} import lists failed: {e}", arr.label()));
            return;
        }
    };
    for l in lists.iter().filter(|l| list_enabled(l)) {
        let Some(id) = id_of(l.get("id")) else { continue };
        let name = l.get("name").and_then(Value::as_str).unwrap_or("");
        if app.dry_run {
            log_info!("dry-run: would force {} ImportListSync for list '{name}' ({id})", arr.label());
            continue;
        }
        match arr.command(&json!({"name": "ImportListSync", "definitionId": id})).await {
            Ok(_) if verbose => log_info!("watchlist: {} ImportListSync forced for list '{name}' ({id})", arr.label()),
            Ok(_) => {}
            Err(e) => quiet.warn(&format!("{key}-{id}"), format!("watchlist: {} ImportListSync '{name}' failed: {e}", arr.label())),
        }
    }
}

/// Every `statusMessages[].messages[]` joined with a space.
fn status_text(q: &Value) -> String {
    let mut parts = Vec::new();
    for s in q.get("statusMessages").and_then(Value::as_array).into_iter().flatten() {
        for m in s.get("messages").and_then(Value::as_array).into_iter().flatten() {
            if let Some(t) = m.as_str() {
                parts.push(t.to_string());
            }
        }
    }
    parts.join(" ")
}

fn no_rejections(item: &Value) -> bool {
    match item.get("rejections") {
        None | Some(Value::Null) => true,
        Some(Value::Array(a)) => a.is_empty(),
        Some(_) => false,
    }
}

/// The queue items to import: `(downloadId, item id, title)`, once per download.
pub fn blocked_by_id(arr_name: &str, queue: &[Value]) -> Vec<(String, i64, String)> {
    let (id_field, needle) =
        if arr_name == "radarr" { ("movieId", "matched to movie by id") } else { ("seriesId", "matched to series by id") };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for q in queue {
        if q.get("trackedDownloadState").and_then(Value::as_str) != Some("importBlocked") {
            continue;
        }
        let Some(id) = id_of(q.get(id_field)) else { continue };
        if !status_text(q).to_lowercase().contains(needle) {
            continue;
        }
        let Some(dl) = q.get("downloadId").and_then(Value::as_str).filter(|d| !d.is_empty()) else { continue };
        if seen.insert(dl.to_string()) {
            out.push((dl.to_string(), id, q.get("title").and_then(Value::as_str).unwrap_or("").to_string()));
        }
    }
    out
}

/// The `ManualImport` files for one download: items without rejections (Sonarr: with
/// episodes), fields as `radarr-watchlist-sync.ps1` sends them.
pub fn import_files(arr_name: &str, items: &[Value], download_id: &str, id: i64) -> Vec<Value> {
    let g = |it: &Value, k: &str| it.get(k).cloned().unwrap_or(Value::Null);
    items
        .iter()
        .filter(|it| no_rejections(it))
        .filter_map(|it| {
            if arr_name == "radarr" {
                return Some(json!({
                    "path": g(it, "path"), "movieId": id, "quality": g(it, "quality"), "languages": g(it, "languages"),
                    "releaseGroup": g(it, "releaseGroup"), "downloadId": download_id, "indexerFlags": g(it, "indexerFlags"),
                }));
            }
            let eps: Vec<i64> = it
                .get("episodes")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|e| id_of(e.get("id"))).collect())
                .unwrap_or_default();
            if eps.is_empty() {
                return None;
            }
            let mut f = json!({
                "path": g(it, "path"), "seriesId": id, "episodeIds": eps, "quality": g(it, "quality"),
                "languages": g(it, "languages"), "releaseGroup": g(it, "releaseGroup"), "downloadId": download_id,
                "indexerFlags": g(it, "indexerFlags"),
            });
            if let Some(rt) = it.get("releaseType") {
                f["releaseType"] = rt.clone();
            }
            Some(f)
        })
        .collect()
}

async fn auto_import(app: &AppState, arr: &Arr, quiet: &mut Quiet) {
    let key = format!("{}-queue", arr.name);
    let queue = match arr.queue().await {
        Ok(q) => {
            quiet.clear(&key);
            q
        }
        Err(e) => {
            quiet.warn(&key, format!("watchlist: {} queue failed: {e}", arr.label()));
            return;
        }
    };
    let id_param = if arr.name == "radarr" { "movieId" } else { "seriesId" };
    for (dl, id, title) in blocked_by_id(arr.name, &queue) {
        let items = match arr.manual_import(&dl, id_param, id).await {
            Ok(i) => i,
            Err(e) => {
                quiet.warn(&format!("{key}-{dl}"), format!("watchlist: {} manualimport for '{title}' failed: {e}", arr.label()));
                continue;
            }
        };
        let files = import_files(arr.name, &items, &dl, id);
        if files.is_empty() {
            continue;
        }
        if app.dry_run {
            log_info!("dry-run: would {} ManualImport {} file(s) of '{title}'", arr.label(), files.len());
            continue;
        }
        match arr.command(&json!({"name": "ManualImport", "importMode": "auto", "files": files})).await {
            Ok(_) => log_info!("watchlist: {} ManualImport of {} file(s) of '{title}' (matched by ID)", arr.label(), files.len()),
            Err(e) => quiet.warn(&format!("{key}-{dl}"), format!("watchlist: {} ManualImport '{title}' failed: {e}", arr.label())),
        }
    }
}

async fn sonarr_series(sonarr: &Arr, quiet: &mut Quiet) -> Option<Vec<Value>> {
    match sonarr.items().await {
        Ok(s) => {
            quiet.clear("sonarr-series");
            Some(s)
        }
        Err(e) => {
            quiet.warn("sonarr-series", format!("watchlist: Sonarr series failed: {e}"));
            None
        }
    }
}

/// `watchlist` in the state file.
fn load_state(app: &AppState) -> (BTreeSet<i64>, BTreeSet<String>) {
    let st = app.state_file.load();
    let w = st.get(STATE_KEY);
    let seen = w
        .and_then(|w| w.get("seen_tvdb"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    let done = w
        .and_then(|w| w.get("next_season"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    (seen, done)
}

fn save_state(app: &AppState, seen: &BTreeSet<i64>, done: &BTreeSet<String>) {
    let mut m = Map::new();
    m.insert(STATE_KEY.into(), json!({"seen_tvdb": seen, "next_season": done}));
    app.state_file.update(m);
}

/// What the cleanup decided for one Sonarr series.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Still on the Watchlist (or never seen there: not ours).
    Keep,
    Delete,
}

/// Pure decision: `shows` = tvdb ids of the shows now on the Watchlist (None: the fetch
/// failed or returned nothing at all), `seen` = tvdb ids ever seen there.
pub fn cleanup_verdicts(
    series: &[Value],
    watch: Option<&[WatchItem]>,
    seen: &BTreeSet<i64>,
) -> Option<Vec<(i64, String, Verdict)>> {
    let w = watch.filter(|w| !w.is_empty())?;
    let shows: Vec<&WatchItem> = w.iter().filter(|i| i.kind == "show").collect();
    let on_list: HashSet<i64> = shows.iter().filter_map(|s| s.tvdb()).collect();
    let titles: HashSet<String> = shows.iter().map(|s| s.title.trim().to_lowercase()).collect();
    Some(
        series
            .iter()
            .filter_map(|s| {
                let id = id_of(s.get("id"))?;
                let tvdb = id_of(s.get("tvdbId"))?;
                let title = s.get("title").and_then(Value::as_str).unwrap_or("").to_string();
                let gone = seen.contains(&tvdb) && !on_list.contains(&tvdb) && !titles.contains(&title.trim().to_lowercase());
                Some((id, title, if gone { Verdict::Delete } else { Verdict::Keep }))
            })
            .collect(),
    )
}

/// Returns the series ids that stay (and came from the Watchlist), for the next-season step.
async fn sonarr_cleanup(app: &AppState, sonarr: &Arr, watch: Option<&[WatchItem]>, series: &[Value], verbose: bool) -> Vec<i64> {
    let (mut seen, done) = load_state(app);
    if let Some(w) = watch.filter(|w| !w.is_empty()) {
        let before = seen.len();
        seen.extend(w.iter().filter(|i| i.kind == "show").filter_map(WatchItem::tvdb));
        if seen.len() != before {
            save_state(app, &seen, &done);
        }
    }
    let Some(verdicts) = cleanup_verdicts(series, watch, &seen) else {
        let ours = series.iter().filter(|s| id_of(s.get("tvdbId")).is_some_and(|t| seen.contains(&t))).count();
        if ours > 0 && verbose {
            log_info!("watchlist: Plex Watchlist unavailable or empty, Sonarr cleanup skipped ({ours} list series kept)");
        }
        return Vec::new();
    };
    let mut kept = Vec::new();
    for (id, title, v) in verdicts {
        let tvdb = series.iter().find(|s| id_of(s.get("id")) == Some(id)).and_then(|s| id_of(s.get("tvdbId")));
        match v {
            Verdict::Keep => {
                if tvdb.is_some_and(|t| seen.contains(&t)) {
                    kept.push(id);
                }
            }
            Verdict::Delete if app.dry_run => {
                log_info!("dry-run: would delete Sonarr series '{title}' ({id}) with its files: no longer on the Plex Watchlist");
            }
            Verdict::Delete => match sonarr.delete_series(id).await {
                Ok(()) => {
                    log_info!("watchlist: deleted Sonarr series '{title}' ({id}) with its files: no longer on the Plex Watchlist");
                    if let Some(t) = tvdb {
                        let (mut s2, d2) = load_state(app);
                        s2.remove(&t);
                        save_state(app, &s2, &d2);
                        seen.remove(&t);
                    }
                }
                Err(e) => log_info!("watchlist: deleting Sonarr series '{title}' ({id}) failed: {e}"),
            },
        }
    }
    kept
}

/// The decision for one series: `Some((next season, aired episode ids, needs monitoring,
/// needs search))` when the next season should start downloading.
pub fn next_season_plan(
    series: &Value,
    episodes: &[Value],
    files: &HashMap<i64, String>,
    views: &HashMap<String, i64>,
    now: i64,
) -> Option<(i64, Vec<i64>, bool, bool)> {
    let seasons: Vec<&Value> = series
        .get("seasons")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter(|s| s.get("seasonNumber").and_then(Value::as_i64).unwrap_or(0) > 0).collect())
        .unwrap_or_default();
    let cur = seasons
        .iter()
        .filter(|s| s.get("monitored").and_then(Value::as_bool).unwrap_or(false))
        .filter_map(|s| s.get("seasonNumber").and_then(Value::as_i64))
        .max()?;
    let next = seasons.iter().filter_map(|s| s.get("seasonNumber").and_then(Value::as_i64)).filter(|n| *n > cur).min()?;
    let season_of = |e: &&Value| e.get("seasonNumber").and_then(Value::as_i64);
    let mut cur_eps: Vec<&Value> = episodes.iter().filter(|e| season_of(e) == Some(cur)).collect();
    cur_eps.sort_by_key(|e| e.get("episodeNumber").and_then(Value::as_i64).unwrap_or(0));
    if cur_eps.is_empty() {
        return None;
    }
    let need = cur_eps.len().saturating_sub(2).max(1);
    let watched = |e: &Value| {
        id_of(e.get("episodeFileId"))
            .and_then(|f| files.get(&f))
            .and_then(|p| views.get(&path_key(p)))
            .is_some_and(|v| *v > 0)
    };
    if !cur_eps[..need].iter().all(|e| watched(e)) {
        return None;
    }
    let next_eps: Vec<&Value> = episodes.iter().filter(|e| season_of(e) == Some(next)).collect();
    let aired: Vec<&Value> = next_eps
        .iter()
        .copied()
        .filter(|e| e.get("airDateUtc").and_then(Value::as_str).and_then(parse_iso_utc).is_some_and(|t| t <= now))
        .collect();
    if aired.is_empty() {
        return None;
    }
    let monitored = seasons
        .iter()
        .find(|s| s.get("seasonNumber").and_then(Value::as_i64) == Some(next))
        .and_then(|s| s.get("monitored").and_then(Value::as_bool))
        .unwrap_or(false);
    let missing = aired.iter().any(|e| !e.get("hasFile").and_then(Value::as_bool).unwrap_or(false));
    let ids = next_eps.iter().filter_map(|e| id_of(e.get("id"))).collect();
    Some((next, ids, !monitored, missing))
}

async fn next_season(app: &AppState, sonarr: &Arr, plex: &PlexJobs, watch: &[WatchItem], kept: &[i64], quiet: &mut Quiet) {
    if kept.is_empty() {
        return;
    }
    let on_list: HashSet<i64> = watch.iter().filter(|i| i.kind == "show").filter_map(WatchItem::tvdb).collect();
    let views = match plex.view_counts(PLEX_SERIES_SECTION, true).await {
        Ok(v) => {
            quiet.clear("plex-episodes");
            v
        }
        Err(e) => {
            quiet.warn("plex-episodes", format!("watchlist: Plex episodes failed, next season check skipped: {e}"));
            return;
        }
    };
    let now = unix_now() as i64;
    for &id in kept {
        let series = match sonarr.series(id).await {
            Ok(s) => s,
            Err(e) => {
                quiet.warn(&format!("series-{id}"), format!("watchlist: Sonarr series {id} failed: {e}"));
                continue;
            }
        };
        let Some(tvdb) = id_of(series.get("tvdbId")).filter(|t| on_list.contains(t)) else { continue };
        let (Ok(eps), Ok(efs)) = (sonarr.episodes(id).await, sonarr.episode_files(id).await) else {
            quiet.warn(&format!("eps-{id}"), format!("watchlist: Sonarr episodes of series {id} failed"));
            continue;
        };
        let files: HashMap<i64, String> = efs
            .iter()
            .filter_map(|f| Some((id_of(f.get("id"))?, f.get("path").and_then(Value::as_str)?.to_string())))
            .collect();
        let Some((season, ep_ids, monitor, search)) = next_season_plan(&series, &eps, &files, &views, now) else { continue };
        let key = format!("{tvdb}:{season}");
        let (seen, mut done) = load_state(app);
        if done.contains(&key) {
            continue;
        }
        let title = series.get("title").and_then(Value::as_str).unwrap_or("").to_string();
        if app.dry_run {
            log_info!(
                "dry-run: would start season {season} of '{title}' (monitor: {monitor}, SeasonSearch: {search}) and notify"
            );
            continue;
        }
        if monitor {
            let mut s = series.clone();
            if let Some(Value::Array(a)) = s.get_mut("seasons") {
                for x in a.iter_mut() {
                    if x.get("seasonNumber").and_then(Value::as_i64) == Some(season) {
                        x["monitored"] = Value::Bool(true);
                    }
                }
            }
            if let Err(e) = sonarr.put_series(id, &s).await {
                log_info!("watchlist: monitoring season {season} of '{title}' failed: {e}");
                continue;
            }
            if let Err(e) = sonarr.monitor_episodes(&ep_ids).await {
                log_info!("watchlist: monitoring the episodes of season {season} of '{title}' failed: {e}");
                continue;
            }
        }
        if search {
            if let Err(e) = sonarr.command(&json!({"name": "SeasonSearch", "seriesId": id, "seasonNumber": season})).await {
                log_info!("watchlist: SeasonSearch for season {season} of '{title}' failed: {e}");
                continue;
            }
        }
        // Remember before notifying, so a crash never repeats the message.
        done.insert(key);
        save_state(app, &seen, &done);
        log_info!("watchlist: next season {season} of '{title}' started (monitored: {monitor}, SeasonSearch: {search})");
        if monitor || search {
            let msg = format!("Sezonul {season} din {title} se descarcă");
            app.notifier.send("Sezon nou", &msg, Some(vec!["tv".into()]), None, crate::services::ntfy::TIMEOUT).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::testutil::{app, fake, ok, status, Fake, Req, Resp};
    use std::sync::Mutex;

    fn wl(items: Value) -> Value {
        let n = items.as_array().map(|a| a.len()).unwrap_or(0);
        json!({"MediaContainer": {"totalSize": n, "size": n, "offset": 0, "Metadata": items}})
    }

    fn show(title: &str, tvdb: i64) -> Value {
        json!({"type": "show", "title": title, "Guid": [{"id": "imdb://tt1"}, {"id": format!("tvdb://{tvdb}")}]})
    }

    fn movie(title: &str) -> Value {
        json!({"type": "movie", "title": title, "Guid": [{"id": "tmdb://1"}, {"id": "tvdb://5000"}]})
    }

    /// A world for one test: answers by path; `watchlist` may be swapped.
    struct World {
        watchlist: Mutex<Resp>,
        series: Mutex<Value>,
        radarr_queue: Value,
        sonarr_queue: Value,
        manual: Value,
        episodes: Value,
        episode_files: Value,
        plex_eps: Value,
    }

    fn world() -> World {
        World {
            watchlist: Mutex::new(ok(wl(json!([movie("Dune"), show("Show A", 100)])))),
            series: Mutex::new(json!([])),
            radarr_queue: json!({"records": []}),
            sonarr_queue: json!({"records": []}),
            manual: json!([]),
            episodes: json!([]),
            episode_files: json!([]),
            plex_eps: json!({"MediaContainer": {"Metadata": []}}),
        }
    }

    async fn serve(w: Arc<World>) -> Fake {
        fake(move |r: &Req| {
            let p = r.path.as_str();
            let radarr = r.header("x-api-key") == Some("radarr-key");
            if p.starts_with("/library/sections/watchlist/all") {
                return w.watchlist.lock().unwrap().clone();
            }
            if p.starts_with("/library/sections/2/all") {
                return ok(w.plex_eps.clone());
            }
            if p == "/api/v3/importlist" {
                return if radarr {
                    ok(json!([{"id": 1, "name": "Plex Watchlist", "enabled": true}, {"id": 2, "name": "Off", "enabled": false}]))
                } else {
                    ok(json!([{"id": 7, "name": "Plex Watchlist", "enableAutomaticAdd": true}, {"id": 8, "name": "Off", "enableAutomaticAdd": false}]))
                };
            }
            if p.starts_with("/api/v3/queue") {
                return ok(if radarr { w.radarr_queue.clone() } else { w.sonarr_queue.clone() });
            }
            if p.starts_with("/api/v3/manualimport") {
                return ok(w.manual.clone());
            }
            if p == "/api/v3/series" {
                return ok(w.series.lock().unwrap().clone());
            }
            if let Some(id) = p.strip_prefix("/api/v3/series/") {
                if r.method == "GET" {
                    let id: i64 = id.parse().unwrap_or(0);
                    let s = w.series.lock().unwrap().as_array().unwrap().iter().find(|s| s["id"] == id).cloned();
                    return s.map(ok).unwrap_or(status(404));
                }
                return ok(json!({}));
            }
            if p.starts_with("/api/v3/episodefile") {
                return ok(w.episode_files.clone());
            }
            if p.starts_with("/api/v3/episode?") {
                return ok(w.episodes.clone());
            }
            ok(json!({}))
        })
        .await
    }

    async fn run(w: World, dry: bool) -> (Fake, Arc<AppState>, Arc<World>) {
        let w = Arc::new(w);
        let f = serve(w.clone()).await;
        let (a, _d) = app(&f.base, dry, json!({}));
        tick(&a, &mut Mem::default()).await;
        (f, a, w)
    }

    fn sonarr_series_json(id: i64, title: &str, tvdb: i64) -> Value {
        json!({"id": id, "title": title, "tvdbId": tvdb, "seasons": [{"seasonNumber": 1, "monitored": true}]})
    }

    #[tokio::test]
    async fn forced_syncs_for_enabled_lists_only() {
        let (f, _a, _w) = run(world(), false).await;
        let cmds: Vec<(String, Value)> = f
            .find("POST", "/api/v3/command")
            .iter()
            .map(|r| (r.header("x-api-key").unwrap().to_string(), r.json()))
            .collect();
        assert_eq!(
            cmds,
            vec![
                ("radarr-key".to_string(), json!({"name": "ImportListSync", "definitionId": 1})),
                ("sonarr-key".to_string(), json!({"name": "ImportListSync", "definitionId": 7})),
            ]
        );
    }

    #[tokio::test]
    async fn sonarr_cleanup_deletes_only_seen_series_gone_from_the_list() {
        let w = world();
        *w.series.lock().unwrap() = json!([
            sonarr_series_json(1, "Show A", 100),
            sonarr_series_json(2, "Show B", 200),
            sonarr_series_json(3, "Manual", 300)
        ]);
        let w = Arc::new(w);
        let f = serve(w.clone()).await;
        let (a, _d) = app(&f.base, false, json!({}));
        // Seen: A and B on the Watchlist.
        *w.watchlist.lock().unwrap() = ok(wl(json!([show("Show A", 100), show("Show B", 200)])));
        tick(&a, &mut Mem::default()).await;
        assert!(f.find("DELETE", "/api/v3/series").is_empty());
        // B leaves the Watchlist: only B is deleted (series 3 was never on it).
        *w.watchlist.lock().unwrap() = ok(wl(json!([movie("Dune"), show("Show A", 100)])));
        tick(&a, &mut Mem::default()).await;
        let del = f.find("DELETE", "/api/v3/series");
        assert_eq!(del.len(), 1);
        assert_eq!(del[0].path, "/api/v3/series/2?deleteFiles=true&addImportListExclusion=false");
        assert_eq!(del[0].header("x-api-key"), Some("sonarr-key"));
        let st = a.state_file.load();
        assert_eq!(st["watchlist"]["seen_tvdb"], json!([100]));
    }

    #[tokio::test]
    async fn outage_guard_deletes_nothing() {
        for outage in [status(500), ok(wl(json!([]))), ok(json!({"nope": 1})), (200, String::new(), vec![])] {
            let w = world();
            *w.series.lock().unwrap() = json!([sonarr_series_json(1, "Show A", 100)]);
            let w = Arc::new(w);
            let f = serve(w.clone()).await;
            let (a, _d) = app(&f.base, false, json!({}));
            tick(&a, &mut Mem::default()).await; // seen
            *w.watchlist.lock().unwrap() = outage.clone();
            tick(&a, &mut Mem::default()).await;
            assert!(f.find("DELETE", "").is_empty(), "outage {:?}", outage.0);
        }
    }

    #[tokio::test]
    async fn partial_watchlist_page_is_an_error() {
        let w = world();
        *w.series.lock().unwrap() = json!([sonarr_series_json(1, "Show A", 100)]);
        let w = Arc::new(w);
        let f = serve(w.clone()).await;
        let (a, _d) = app(&f.base, false, json!({}));
        tick(&a, &mut Mem::default()).await;
        // totalSize says 5 but every page is empty after the first item.
        *w.watchlist.lock().unwrap() =
            ok(json!({"MediaContainer": {"totalSize": 5, "size": 0, "Metadata": []}}));
        tick(&a, &mut Mem::default()).await;
        assert!(f.find("DELETE", "").is_empty());
    }

    #[tokio::test]
    async fn title_still_on_the_list_is_kept() {
        let seen: BTreeSet<i64> = [100].into();
        let series = [sonarr_series_json(1, "Show A", 100)];
        let watch = [WatchItem { kind: "show".into(), title: "show a".into(), guids: vec![] }];
        let v = cleanup_verdicts(&series, Some(&watch), &seen).unwrap();
        assert_eq!(v[0].2, Verdict::Keep);
    }

    fn queue_rec(state: &str, msg: &str, id_field: &str) -> Value {
        json!({"trackedDownloadState": state, id_field: 5, "downloadId": "ABC", "title": "Rel",
               "statusMessages": [{"title": "x", "messages": [msg]}]})
    }

    #[tokio::test]
    async fn auto_import_only_matched_by_id_without_rejections() {
        let mut w = world();
        w.radarr_queue = json!({"records": [
            queue_rec("importBlocked", "Found matching movie via grab history, but release was matched to movie by ID. Manual Import required.", "movieId"),
            queue_rec("importBlocked", "Not an upgrade", "movieId"),
            queue_rec("importPending", "matched to movie by ID", "movieId")
        ]});
        w.manual = json!([
            {"path": "D:\\Media\\Downloads\\radarr\\a.mkv", "quality": {"quality": {"id": 19}}, "languages": [{"id": 1}],
             "releaseGroup": "FLUX", "indexerFlags": 0, "rejections": []},
            {"path": "D:\\Media\\Downloads\\radarr\\sample.mkv", "rejections": [{"reason": "Sample", "type": "permanent"}]}
        ]);
        let (f, _a, _w) = run(w, false).await;
        let imports: Vec<Value> =
            f.find("POST", "/api/v3/command").iter().map(Req::json).filter(|j| j["name"] == "ManualImport").collect();
        assert_eq!(imports.len(), 1);
        assert_eq!(
            imports[0],
            json!({"name": "ManualImport", "importMode": "auto", "files": [{
                "path": "D:\\Media\\Downloads\\radarr\\a.mkv", "movieId": 5, "quality": {"quality": {"id": 19}},
                "languages": [{"id": 1}], "releaseGroup": "FLUX", "downloadId": "ABC", "indexerFlags": 0}]})
        );
        let mi = f.find("GET", "/api/v3/manualimport");
        assert_eq!(mi.len(), 1);
        assert_eq!(mi[0].path, "/api/v3/manualimport?downloadId=ABC&movieId=5&filterExistingFiles=false");
    }

    #[tokio::test]
    async fn auto_import_skips_when_every_file_is_rejected() {
        let mut w = world();
        w.radarr_queue = json!({"records": [queue_rec("importBlocked", "matched to movie by ID", "movieId")]});
        w.manual = json!([{"path": "x", "rejections": [{"reason": "r"}]}]);
        let (f, _a, _w) = run(w, false).await;
        assert!(f.find("POST", "/api/v3/command").iter().all(|r| r.json()["name"] != "ManualImport"));
    }

    #[test]
    fn sonarr_import_files_need_episodes() {
        let q = [queue_rec("importBlocked", "release was matched to series by ID. Manual Import required.", "seriesId"),
                 queue_rec("importBlocked", "matched to series by ID", "seriesId")];
        assert_eq!(blocked_by_id("sonarr", &q), vec![("ABC".to_string(), 5, "Rel".to_string())]);
        assert!(blocked_by_id("radarr", &q).is_empty());
        let items = [
            json!({"path": "a", "episodes": [{"id": 11}, {"id": 12}], "quality": {}, "languages": [], "releaseType": "seasonPack", "rejections": null}),
            json!({"path": "b", "episodes": [], "rejections": []}),
        ];
        let f = import_files("sonarr", &items, "ABC", 5);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0]["episodeIds"], json!([11, 12]));
        assert_eq!(f[0]["seriesId"], json!(5));
        assert_eq!(f[0]["releaseType"], json!("seasonPack"));
    }

    #[tokio::test]
    async fn dry_run_makes_no_mutating_calls() {
        let mut w = world();
        w.radarr_queue = json!({"records": [queue_rec("importBlocked", "matched to movie by ID", "movieId")]});
        w.manual = json!([{"path": "a", "rejections": []}]);
        *w.series.lock().unwrap() = json!([sonarr_series_json(2, "Show B", 200)]);
        let w = Arc::new(w);
        let f = serve(w.clone()).await;
        let (a, d) = app(&f.base, true, json!({}));
        // B was seen earlier (state file), now gone from the list.
        std::fs::write(d.join("state.local.json"), r#"{"watchlist": {"seen_tvdb": [200], "next_season": []}}"#).unwrap();
        tick(&a, &mut Mem::default()).await;
        assert!(f.mutations().is_empty(), "{:?}", f.mutations());
    }

    // -- next season ------------------------------------------------------------------

    fn ep(id: i64, season: i64, n: i64, file: i64, aired: &str, has_file: bool) -> Value {
        json!({"id": id, "seasonNumber": season, "episodeNumber": n, "episodeFileId": file, "airDateUtc": aired, "hasFile": has_file})
    }

    fn two_seasons(next_monitored: bool) -> Value {
        json!({"id": 1, "title": "Show A", "tvdbId": 100, "seasons": [
            {"seasonNumber": 0, "monitored": false},
            {"seasonNumber": 1, "monitored": true},
            {"seasonNumber": 2, "monitored": next_monitored}]})
    }

    /// Season 1 has 5 episodes (files 1..5), season 2 has 2 aired + 1 future.
    fn eps() -> Vec<Value> {
        let mut v: Vec<Value> = (1..=5).map(|n| ep(n, 1, n, n, "2020-01-01T00:00:00Z", true)).collect();
        v.push(ep(21, 2, 1, 0, "2021-01-01T00:00:00Z", false));
        v.push(ep(22, 2, 2, 0, "2021-01-08T00:00:00Z", false));
        v.push(ep(23, 2, 3, 0, "2099-01-01T00:00:00Z", false));
        v
    }

    fn files() -> HashMap<i64, String> {
        (1..=5).map(|n| (n, format!("D:\\Media\\Seriale\\Show A\\Season 01\\e{n}.mkv"))).collect()
    }

    fn views(watched: &[i64]) -> HashMap<String, i64> {
        (1..=5).map(|n| (path_key(&format!("d:/media/seriale/show a/season 01/E{n}.mkv")), i64::from(watched.contains(&n)))).collect()
    }

    #[test]
    fn next_season_thresholds() {
        let now = parse_iso_utc("2026-10-26T00:00:00Z").unwrap();
        let s = two_seasons(false);
        // 2 of 5 watched: not yet.
        assert_eq!(next_season_plan(&s, &eps(), &files(), &views(&[1, 2]), now), None);
        // 3 of 5 (all but the last 2): go, with the 3 episode ids of season 2.
        assert_eq!(next_season_plan(&s, &eps(), &files(), &views(&[1, 2, 3]), now), Some((2, vec![21, 22, 23], true, true)));
        // A gap (episode 2 unwatched) blocks it.
        assert_eq!(next_season_plan(&s, &eps(), &files(), &views(&[1, 3, 4, 5]), now), None);
        // Next season not aired yet.
        let future: Vec<Value> = eps().into_iter().map(|mut e| {
            if e["seasonNumber"] == 2 { e["airDateUtc"] = json!("2099-01-01T00:00:00Z"); }
            e
        }).collect();
        assert_eq!(next_season_plan(&s, &future, &files(), &views(&[1, 2, 3]), now), None);
        // Next season already on disk: monitor it, no search.
        let have: Vec<Value> = eps().into_iter().map(|mut e| { e["hasFile"] = json!(true); e }).collect();
        assert_eq!(next_season_plan(&s, &have, &files(), &views(&[1, 2, 3]), now), Some((2, vec![21, 22, 23], true, false)));
        // Season 2 monitored: it is the current one and there is nothing after it.
        assert_eq!(next_season_plan(&two_seasons(true), &eps(), &files(), &views(&[1, 2, 3, 4, 5]), now).map(|p| p.0), None);
    }

    #[tokio::test]
    async fn next_season_fires_once() {
        let mut w = world();
        *w.series.lock().unwrap() = json!([two_seasons(false)]);
        w.episodes = Value::Array(eps());
        w.episode_files = json!((1..=5).map(|n| json!({"id": n, "path": format!("D:\\Media\\Seriale\\Show A\\Season 01\\e{n}.mkv")})).collect::<Vec<_>>());
        w.plex_eps = json!({"MediaContainer": {"Metadata": (1..=3).map(|n| json!({"viewCount": 1, "Media": [{"Part": [{"file": format!("D:\\Media\\Seriale\\Show A\\Season 01\\e{n}.mkv")}]}]})).collect::<Vec<_>>()}});
        let w = Arc::new(w);
        let f = serve(w.clone()).await;
        let (a, _d) = app(&f.base, false, json!({}));
        tick(&a, &mut Mem::default()).await;
        tick(&a, &mut Mem::default()).await;
        let puts = f.find("PUT", "/api/v3/series/1");
        assert_eq!(puts.len(), 1);
        assert_eq!(puts[0].json()["seasons"][2], json!({"seasonNumber": 2, "monitored": true}));
        let mon = f.find("PUT", "/api/v3/episode/monitor");
        assert_eq!(mon.len(), 1);
        assert_eq!(mon[0].json(), json!({"episodeIds": [21, 22, 23], "monitored": true}));
        let searches: Vec<Value> =
            f.find("POST", "/api/v3/command").iter().map(Req::json).filter(|j| j["name"] == "SeasonSearch").collect();
        assert_eq!(searches, vec![json!({"name": "SeasonSearch", "seriesId": 1, "seasonNumber": 2})]);
        assert_eq!(a.state_file.load()["watchlist"]["next_season"], json!(["100:2"]));
    }

    #[tokio::test]
    async fn next_season_dry_run_changes_nothing() {
        let mut w = world();
        *w.series.lock().unwrap() = json!([two_seasons(false)]);
        w.episodes = Value::Array(eps());
        w.episode_files = json!((1..=5).map(|n| json!({"id": n, "path": format!("D:\\Media\\Seriale\\Show A\\Season 01\\e{n}.mkv")})).collect::<Vec<_>>());
        w.plex_eps = json!({"MediaContainer": {"Metadata": (1..=5).map(|n| json!({"viewCount": 2, "Media": [{"Part": [{"file": format!("D:\\Media\\Seriale\\Show A\\Season 01\\e{n}.mkv")}]}]})).collect::<Vec<_>>()}});
        let (f, a, _w) = run(w, true).await;
        assert!(f.mutations().is_empty(), "{:?}", f.mutations());
        assert_eq!(a.state_file.load()["watchlist"]["next_season"], json!([]));
    }
}
