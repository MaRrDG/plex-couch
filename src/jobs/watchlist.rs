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
//!    Watchlist and is no longer on it. A candidate carries the Sonarr tag
//!    `plex-watchlist` (set on the import list; looked up by label each tick, no tag ->
//!    nothing deleted) AND its tvdb id was seen on the Watchlist at least once
//!    (`watchlist.seen_tvdb` in the state file). Still on the list = same tvdb, tmdb or
//!    imdb id, or the same title without a trailing `(...)`. Guards: a failed, partial,
//!    shifting or empty Watchlist fetch, a show without a tvdb id, a failed Sonarr fetch or
//!    an unreadable state file delete nothing; a series is deleted only after it was absent
//!    in 2+ successful fetches at least 5 min apart (`watchlist.missing`); more than 2
//!    deletions in one tick delete none and send one ntfy warning.
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
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
            tick(&app, &mut mem, unix_now()).await;
            if stop.sleep(INTERVAL).await {
                return;
            }
        }
    });
}

/// One run of every step (errors are logged and never stop the loop).
pub async fn tick(app: &AppState, mem: &mut Mem, now: f64) {
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
    let kept = sonarr_cleanup(app, &sonarr, watch.as_deref(), &series, now, &mut mem.quiet).await;
    if verbose {
        let (items, shows) = watch.as_deref().map(|w| (w.len(), w.iter().filter(|i| i.kind == "show").count())).unwrap_or((0, 0));
        log_info!(
            "watchlist: Plex Watchlist {items} item(s), {shows} show(s); Sonarr {} series, {} from the Watchlist",
            series.len(),
            kept.len()
        );
    }
    if let Some(w) = watch.as_deref() {
        next_season(app, &sonarr, &plex, w, &kept, now as i64, &mut mem.quiet).await;
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
#[derive(Debug, Default, Clone, PartialEq)]
pub struct WState {
    /// tvdb ids ever seen on the Watchlist.
    pub seen: BTreeSet<i64>,
    /// `tvdb:season` keys of next seasons already started.
    pub done: BTreeSet<String>,
    /// tvdb id -> (first missing at, successful fetches it was missing in, last of them).
    pub missing: BTreeMap<i64, (f64, u32, f64)>,
    /// The "too many deletions" warning was sent and the condition has not cleared.
    pub cap_warned: bool,
}

impl WState {
    /// Only a missing file is empty; an unreadable state file is an error (skip the step).
    pub fn load(app: &AppState) -> Result<WState, &'static str> {
        let st = app.state_file.try_load()?;
        let w = st.get(STATE_KEY);
        let arr = |k: &str| w.and_then(|w| w.get(k)).and_then(Value::as_array).cloned().unwrap_or_default();
        let missing = w
            .and_then(|w| w.get("missing"))
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| {
                        let n = |f: &str| v.get(f).and_then(Value::as_f64);
                        Some((k.parse().ok()?, (n("since")?, n("count")? as u32, n("last")?)))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(WState {
            seen: arr("seen_tvdb").iter().filter_map(Value::as_i64).collect(),
            done: arr("next_season").iter().filter_map(|x| x.as_str().map(String::from)).collect(),
            missing,
            cap_warned: w.and_then(|w| w.get("cap_warned")).and_then(Value::as_bool).unwrap_or(false),
        })
    }

    pub fn save(&self, app: &AppState) {
        let missing: Map<String, Value> = self
            .missing
            .iter()
            .map(|(k, (since, count, last))| (k.to_string(), json!({"since": since, "count": count, "last": last})))
            .collect();
        let mut m = Map::new();
        m.insert(
            STATE_KEY.into(),
            json!({"seen_tvdb": self.seen, "next_season": self.done, "missing": missing, "cap_warned": self.cap_warned}),
        );
        app.state_file.update(m);
    }
}

/// Lower-case, collapsed spaces, trailing `(...)` groups (year, country) removed.
pub fn norm_title(t: &str) -> String {
    let mut s = t.trim().to_lowercase();
    while s.ends_with(')') {
        match s.rfind('(') {
            Some(i) if i > 0 => s = s[..i].trim_end().to_string(),
            _ => break,
        }
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A Sonarr series the cleanup may consider: from the list (tag) and seen on the Watchlist.
#[derive(Debug, Clone, PartialEq)]
pub struct Ours {
    pub id: i64,
    pub title: String,
    pub tvdb: i64,
    pub on_list: bool,
}

/// Which of our series are still on the Watchlist (by tvdb, tmdb, imdb or normalized
/// title). Err (abort the whole cleanup) when the Watchlist is empty or any show on it has
/// no parseable tvdb id.
pub fn classify(series: &[Value], watch: &[WatchItem], seen: &BTreeSet<i64>, tag: i64) -> Result<Vec<Ours>, String> {
    if watch.is_empty() {
        return Err("the Plex Watchlist is empty".into());
    }
    let shows: Vec<&WatchItem> = watch.iter().filter(|i| i.kind == "show").collect();
    if let Some(s) = shows.iter().find(|s| s.tvdb().is_none()) {
        return Err(format!("the Watchlist show '{}' has no tvdb id", s.title));
    }
    let tvdbs: HashSet<i64> = shows.iter().filter_map(|s| s.tvdb()).collect();
    let tmdbs: HashSet<i64> = shows.iter().filter_map(|s| s.tmdb()).collect();
    let imdbs: HashSet<String> = shows.iter().filter_map(|s| s.imdb()).collect();
    let titles: HashSet<String> = shows.iter().map(|s| norm_title(&s.title)).filter(|t| !t.is_empty()).collect();
    Ok(series
        .iter()
        .filter_map(|s| {
            let id = id_of(s.get("id"))?;
            let tvdb = id_of(s.get("tvdbId"))?;
            let tagged = s.get("tags").and_then(Value::as_array).is_some_and(|t| t.iter().any(|x| x.as_i64() == Some(tag)));
            if !tagged || !seen.contains(&tvdb) {
                return None;
            }
            let title = s.get("title").and_then(Value::as_str).unwrap_or("").to_string();
            let imdb = s.get("imdbId").and_then(Value::as_str).map(|x| x.trim().to_lowercase()).filter(|x| !x.is_empty());
            let on_list = tvdbs.contains(&tvdb)
                || id_of(s.get("tmdbId")).is_some_and(|t| tmdbs.contains(&t))
                || imdb.is_some_and(|i| imdbs.contains(&i))
                || titles.contains(&norm_title(&title));
            Some(Ours { id, title, tvdb, on_list })
        })
        .collect())
}

/// Grace rule: missing in at least this many successful complete fetches...
pub const GRACE_FETCHES: u32 = 2;
/// ... spanning at least this long.
pub const GRACE_SECONDS: f64 = 300.0;
/// More deletions than this in one tick: delete none and warn.
pub const MAX_DELETES: usize = 2;
pub const LIST_TAG: &str = "plex-watchlist";

/// Update the missing counters for one successful complete fetch; returns the series due.
pub fn due_for_delete(st: &mut WState, ours: &[Ours], now: f64) -> Vec<Ours> {
    let absent: HashSet<i64> = ours.iter().filter(|o| !o.on_list).map(|o| o.tvdb).collect();
    st.missing.retain(|t, _| absent.contains(t));
    let mut due = Vec::new();
    for o in ours.iter().filter(|o| !o.on_list) {
        let e = st.missing.entry(o.tvdb).or_insert((now, 0, now));
        e.1 += 1;
        e.2 = now;
        if e.1 >= GRACE_FETCHES && now - e.0 >= GRACE_SECONDS {
            due.push(o.clone());
        }
    }
    due
}

/// Returns the series ids that stay (from the list, still on the Watchlist), for the
/// next-season step. Any doubt deletes nothing.
async fn sonarr_cleanup(
    app: &AppState,
    sonarr: &Arr,
    watch: Option<&[WatchItem]>,
    series: &[Value],
    now: f64,
    quiet: &mut Quiet,
) -> Vec<i64> {
    let mut st = match WState::load(app) {
        Ok(s) => s,
        Err(t) => {
            quiet.warn("wl-state", format!("watchlist: the state file is unreadable ({t}), Sonarr cleanup skipped"));
            return Vec::new();
        }
    };
    let Some(w) = watch.filter(|w| !w.is_empty()) else {
        quiet.warn("wl-clean", "watchlist: Plex Watchlist unavailable or empty, Sonarr cleanup skipped".into());
        return Vec::new();
    };
    let tag = match sonarr.tag_id(LIST_TAG).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            quiet.warn("wl-clean", format!("watchlist: Sonarr has no '{LIST_TAG}' tag, Sonarr cleanup skipped"));
            return Vec::new();
        }
        Err(e) => {
            quiet.warn("wl-clean", format!("watchlist: Sonarr tags failed, Sonarr cleanup skipped: {e}"));
            return Vec::new();
        }
    };
    let shows: Vec<&WatchItem> = w.iter().filter(|i| i.kind == "show").collect();
    if shows.iter().all(|s| s.tvdb().is_some()) {
        let before = st.seen.len();
        st.seen.extend(shows.iter().filter_map(|s| s.tvdb()));
        if st.seen.len() != before {
            st.save(app);
        }
    }
    let ours = match classify(series, w, &st.seen, tag) {
        Ok(o) => o,
        Err(r) => {
            quiet.warn("wl-clean", format!("watchlist: {r}, Sonarr cleanup skipped"));
            return Vec::new();
        }
    };
    quiet.clear("wl-clean");
    let due = due_for_delete(&mut st, &ours, now);
    if due.len() > MAX_DELETES {
        let names: Vec<&str> = due.iter().map(|o| o.title.as_str()).collect();
        log_info!("watchlist: {} series would be deleted at once ({}), deleting none", due.len(), names.join(", "));
        if !st.cap_warned {
            st.cap_warned = true;
            let msg = format!(
                "{} seriale au dispărut deodată din Plex Watchlist; nu am șters nimic din Sonarr. Verifică lista.",
                due.len()
            );
            if app.dry_run {
                log_info!("dry-run: would notify 'Watchlist': {msg}");
            } else {
                let tags = Some(vec!["warning".to_string()]);
                app.notifier.send("Watchlist", &msg, tags, Some(json!("high")), crate::services::ntfy::TIMEOUT).await;
            }
        }
        st.save(app);
        return ours.iter().filter(|o| o.on_list).map(|o| o.id).collect();
    }
    st.cap_warned = false;
    for o in &due {
        if app.dry_run {
            log_info!("dry-run: would delete Sonarr series '{}' ({}) with its files: no longer on the Plex Watchlist", o.title, o.id);
            continue;
        }
        match sonarr.delete_series(o.id).await {
            Ok(()) => {
                log_info!("watchlist: deleted Sonarr series '{}' ({}) with its files: no longer on the Plex Watchlist", o.title, o.id);
                st.missing.remove(&o.tvdb);
                st.seen.remove(&o.tvdb);
            }
            Err(e) => log_info!("watchlist: deleting Sonarr series '{}' ({}) failed: {e}", o.title, o.id),
        }
    }
    for o in ours.iter().filter(|o| !o.on_list && !due.contains(o)) {
        let (since, count, _) = st.missing.get(&o.tvdb).copied().unwrap_or((now, 0, now));
        log_info!(
            "watchlist: Sonarr series '{}' is off the Plex Watchlist ({count} check(s), first {:.0} s ago), waiting before deleting",
            o.title,
            now - since
        );
    }
    st.save(app);
    ours.iter().filter(|o| o.on_list).map(|o| o.id).collect()
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
    let season_of = |e: &&Value| e.get("seasonNumber").and_then(Value::as_i64);
    let ep_monitored = |e: &&Value| e.get("monitored").and_then(Value::as_bool).unwrap_or(false);
    // The list's "first season" option monitors only the season 1 episodes but leaves every season
    // flag on, so the current season is the highest one with monitored episodes; the season flags
    // are only a fallback when Sonarr sent no episode flags at all.
    let cur = episodes
        .iter()
        .filter(ep_monitored)
        .filter_map(|e| season_of(&e))
        .filter(|n| *n > 0)
        .max()
        .or_else(|| {
            seasons
                .iter()
                .filter(|s| s.get("monitored").and_then(Value::as_bool).unwrap_or(false))
                .filter_map(|s| s.get("seasonNumber").and_then(Value::as_i64))
                .max()
        })?;
    let next = seasons.iter().filter_map(|s| s.get("seasonNumber").and_then(Value::as_i64)).filter(|n| *n > cur).min()?;
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
        .unwrap_or(false)
        && aired.iter().all(ep_monitored);
    let missing = aired.iter().any(|e| !e.get("hasFile").and_then(Value::as_bool).unwrap_or(false));
    let ids = next_eps.iter().filter_map(|e| id_of(e.get("id"))).collect();
    Some((next, ids, !monitored, missing))
}

async fn next_season(app: &AppState, sonarr: &Arr, plex: &PlexJobs, watch: &[WatchItem], kept: &[i64], now: i64, quiet: &mut Quiet) {
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
        let Ok(mut st) = WState::load(app) else {
            quiet.warn("wl-state", "watchlist: the state file is unreadable, next season check skipped".into());
            return;
        };
        if st.done.contains(&key) {
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
        st.done.insert(key);
        st.save(app);
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
        json!({"type": "show", "title": title, "ratingKey": format!("s{tvdb}"),
               "Guid": [{"id": format!("imdb://tt{tvdb}")}, {"id": format!("tvdb://{tvdb}")}]})
    }

    fn movie(title: &str) -> Value {
        json!({"type": "movie", "title": title, "ratingKey": format!("m{title}"), "Guid": [{"id": "tmdb://1"}, {"id": "tvdb://5000"}]})
    }

    /// A world for one test: answers by path; `watchlist` may be swapped.
    struct World {
        watchlist: Mutex<Resp>,
        series: Mutex<Value>,
        tags: Mutex<Value>,
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
            tags: Mutex::new(json!([{"id": 3, "label": "other"}, {"id": 1, "label": "plex-watchlist"}])),
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
            if p == "/api/v3/tag" {
                return ok(w.tags.lock().unwrap().clone());
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

    const T0: f64 = 1_793_000_000.0;

    async fn run(w: World, dry: bool) -> (Fake, Arc<AppState>, Arc<World>) {
        let w = Arc::new(w);
        let f = serve(w.clone()).await;
        let (a, _d) = app(&f.base, dry, json!({}));
        tick(&a, &mut Mem::default(), T0).await;
        (f, a, w)
    }

    fn sonarr_series_json(id: i64, title: &str, tvdb: i64) -> Value {
        json!({"id": id, "title": title, "tvdbId": tvdb, "tags": [1], "seasons": [{"seasonNumber": 1, "monitored": true}]})
    }

    fn deleted(f: &Fake) -> Vec<String> {
        f.find("DELETE", "/api/v3/series").iter().map(|r| r.path.clone()).collect()
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
    async fn sonarr_cleanup_waits_two_fetches_five_minutes_apart_and_needs_the_tag() {
        let w = world();
        let mut untagged = sonarr_series_json(4, "Untagged", 200);
        untagged["tags"] = json!([3]);
        *w.series.lock().unwrap() = json!([
            sonarr_series_json(1, "Show A", 100),
            sonarr_series_json(2, "Show B", 200),
            sonarr_series_json(3, "Manual", 300),
            untagged
        ]);
        let w = Arc::new(w);
        let f = serve(w.clone()).await;
        let (a, _d) = app(&f.base, false, json!({}));
        let mut m = Mem::default();
        *w.watchlist.lock().unwrap() = ok(wl(json!([show("Show A", 100), show("Show B", 200)])));
        tick(&a, &mut m, T0).await; // seen: A and B
        *w.watchlist.lock().unwrap() = ok(wl(json!([movie("Dune"), show("Show A", 100)])));
        tick(&a, &mut m, T0 + 180.0).await; // B missing once
        assert!(deleted(&f).is_empty());
        tick(&a, &mut m, T0 + 360.0).await; // twice, but only 3 min apart
        assert!(deleted(&f).is_empty());
        tick(&a, &mut m, T0 + 540.0).await; // 3 times over 6 min: delete B only
        assert_eq!(deleted(&f), vec!["/api/v3/series/2?deleteFiles=true&addImportListExclusion=false".to_string()]);
        assert_eq!(f.find("DELETE", "")[0].header("x-api-key"), Some("sonarr-key"));
        let st = WState::load(&a).unwrap();
        assert_eq!(st.seen, BTreeSet::from([100]));
        assert!(st.missing.is_empty());
    }

    #[tokio::test]
    async fn seen_again_resets_the_grace() {
        let w = world();
        *w.series.lock().unwrap() = json!([sonarr_series_json(1, "Show A", 100), sonarr_series_json(2, "Show B", 200)]);
        let w = Arc::new(w);
        let f = serve(w.clone()).await;
        let (a, _d) = app(&f.base, false, json!({}));
        let mut m = Mem::default();
        let both = ok(wl(json!([show("Show A", 100), show("Show B", 200)])));
        let only_a = ok(wl(json!([show("Show A", 100)])));
        for (t, list) in [(0.0, &both), (180.0, &only_a), (360.0, &both), (540.0, &only_a), (720.0, &only_a)] {
            *w.watchlist.lock().unwrap() = list.clone();
            tick(&a, &mut m, T0 + t).await;
        }
        assert!(deleted(&f).is_empty(), "the second absence started over at +540");
        tick(&a, &mut m, T0 + 900.0).await;
        assert_eq!(deleted(&f).len(), 1);
    }

    /// Seen at T0, then `bad` answers for the next ticks: nothing may be deleted.
    async fn never_deletes(seed: Value, bad: Resp, tags: Value) {
        let w = world();
        *w.series.lock().unwrap() = json!([sonarr_series_json(1, "Show A", 100), sonarr_series_json(2, "Show B", 200)]);
        let w = Arc::new(w);
        let f = serve(w.clone()).await;
        let (a, _d) = app(&f.base, false, json!({}));
        let mut m = Mem::default();
        *w.watchlist.lock().unwrap() = ok(wl(seed));
        tick(&a, &mut m, T0).await;
        *w.watchlist.lock().unwrap() = bad.clone();
        *w.tags.lock().unwrap() = tags;
        for t in 1..6 {
            tick(&a, &mut m, T0 + 400.0 * t as f64).await;
        }
        assert!(f.find("DELETE", "").is_empty(), "case {:?} {}", bad.0, bad.1);
    }

    fn tag() -> Value {
        json!([{"id": 1, "label": "plex-watchlist"}])
    }

    #[tokio::test]
    async fn outage_guard_deletes_nothing() {
        let seed = json!([show("Show A", 100), show("Show B", 200)]);
        for bad in [status(500), ok(wl(json!([]))), ok(json!({"nope": 1})), (200, String::new(), vec![])] {
            never_deletes(seed.clone(), bad, tag()).await;
        }
        // totalSize says 5 but the pages end after nothing.
        never_deletes(seed.clone(), ok(json!({"MediaContainer": {"totalSize": 5, "size": 0, "Metadata": []}})), tag()).await;
    }

    #[tokio::test]
    async fn a_show_without_tvdb_aborts_the_cleanup() {
        let seed = json!([show("Show A", 100), show("Show B", 200)]);
        let broken = json!({"type": "show", "title": "Odd", "ratingKey": "x", "Guid": [{"id": "tvdb://"}]});
        never_deletes(seed, ok(wl(json!([show("Show A", 100), broken]))), tag()).await;
    }

    #[tokio::test]
    async fn no_list_tag_deletes_nothing() {
        let seed = json!([show("Show A", 100), show("Show B", 200)]);
        never_deletes(seed.clone(), ok(wl(json!([show("Show A", 100)]))), json!([{"id": 3, "label": "other"}])).await;
        never_deletes(seed, ok(wl(json!([show("Show A", 100)]))), json!({"bad": true})).await;
    }

    #[test]
    fn still_on_the_list_by_any_id_or_title() {
        let seen: BTreeSet<i64> = [100, 200, 300, 400].into();
        let mut s = vec![
            sonarr_series_json(1, "Show A", 100),
            sonarr_series_json(2, "The Office (US)", 200),
            sonarr_series_json(3, "By Imdb", 300),
            sonarr_series_json(4, "By Tmdb", 400),
        ];
        s[2]["imdbId"] = json!("TT777");
        s[3]["tmdbId"] = json!(55);
        let w = |title: &str, guids: &[&str]| WatchItem { kind: "show".into(), title: title.into(), guids: guids.iter().map(|g| g.to_string()).collect() };
        let watch = vec![
            w("the office", &["tvdb://999"]),
            w("Something", &["tvdb://998", "imdb://tt777"]),
            w("Else", &["tvdb://997", "tmdb://55"]),
        ];
        let v = classify(&s, &watch, &seen, 1).unwrap();
        let on: Vec<(i64, bool)> = v.iter().map(|o| (o.id, o.on_list)).collect();
        assert_eq!(on, vec![(1, false), (2, true), (3, true), (4, true)]);
        assert_eq!(norm_title("  The Office (US) (2005) "), "the office");
        assert_eq!(norm_title("Dark"), "dark");
    }

    #[tokio::test]
    async fn more_than_two_deletions_delete_none_and_warn_once() {
        let w = world();
        *w.series.lock().unwrap() = json!([
            sonarr_series_json(1, "A", 100), sonarr_series_json(2, "B", 200),
            sonarr_series_json(3, "C", 300), sonarr_series_json(4, "D", 400)
        ]);
        let w = Arc::new(w);
        let f = serve(w.clone()).await;
        let (a, _d) = app(&f.base, false, json!({}));
        let mut m = Mem::default();
        *w.watchlist.lock().unwrap() = ok(wl(json!([show("A", 100), show("B", 200), show("C", 300), show("D", 400)])));
        tick(&a, &mut m, T0).await;
        *w.watchlist.lock().unwrap() = ok(wl(json!([show("A", 100)])));
        for t in 1..6 {
            tick(&a, &mut m, T0 + 400.0 * t as f64).await;
        }
        assert!(f.find("DELETE", "").is_empty());
        let warn: Vec<Req> = f.requests().into_iter().filter(|r| r.method == "POST" && r.path == "/").collect();
        assert_eq!(warn.len(), 1, "one ntfy warning");
        assert!(warn[0].body.contains("nu am șters nimic"));
        assert!(WState::load(&a).unwrap().cap_warned);
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
        std::fs::write(d.join("state.local.json"), r#"{"watchlist": {"seen_tvdb": [200], "next_season": [], "missing": {"200": {"since": 0.0, "count": 5, "last": 0.0}}}}"#).unwrap();
        tick(&a, &mut Mem::default(), T0).await;
        assert!(f.mutations().is_empty(), "{:?}", f.mutations());
    }

    // -- next season ------------------------------------------------------------------

    fn ep(id: i64, season: i64, n: i64, file: i64, aired: &str, has_file: bool) -> Value {
        json!({"id": id, "seasonNumber": season, "episodeNumber": n, "episodeFileId": file, "airDateUtc": aired, "hasFile": has_file})
    }

    fn two_seasons(next_monitored: bool) -> Value {
        json!({"id": 1, "title": "Show A", "tvdbId": 100, "tags": [1], "seasons": [
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

    #[test]
    fn current_season_comes_from_the_monitored_episodes() {
        // What the Plex list's "first season" option really leaves behind: every season flag on,
        // only the season 1 episodes monitored.
        let now = parse_iso_utc("2026-10-26T00:00:00Z").unwrap();
        let s = json!({"id": 1, "title": "Show A", "tvdbId": 100, "tags": [1], "seasons": [
            {"seasonNumber": 0, "monitored": false},
            {"seasonNumber": 1, "monitored": true},
            {"seasonNumber": 2, "monitored": true},
            {"seasonNumber": 3, "monitored": true}]});
        let mut e: Vec<Value> = eps().into_iter().map(|mut x| { x["monitored"] = json!(x["seasonNumber"] == 1); x }).collect();
        e.push(json!({"id": 31, "seasonNumber": 3, "episodeNumber": 1, "episodeFileId": 0, "airDateUtc": "2022-01-01T00:00:00Z", "hasFile": false, "monitored": false}));
        // Season 1 is current, so season 2 is next; its flag is on but its episodes are not, so monitor them.
        assert_eq!(next_season_plan(&s, &e, &files(), &views(&[1, 2, 3]), now), Some((2, vec![21, 22, 23], true, true)));
        assert_eq!(next_season_plan(&s, &e, &files(), &views(&[1, 2]), now), None);
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
        tick(&a, &mut Mem::default(), T0).await;
        tick(&a, &mut Mem::default(), T0 + 180.0).await;
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
