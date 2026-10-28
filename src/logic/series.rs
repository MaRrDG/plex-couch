//! The Seriale tab and the movie sheet in Filme: what is in the library, what is watched,
//! what the TV cannot direct-play, and the Romanian subtitle of every episode.
//!
//! - `GET /api/series` ([`api_list`]): the poster grid. Every Sonarr series with episode
//!   files; per series the unwatched files (Plex `viewCount` by path, section 2) and how many of
//!   them need converting ([`crate::jobs::convert::tv_needs_conversion`], not converted yet).
//!   Built by [`grid`], cached and loaded in the background by `convert::Control::listing`
//!   (60 s, a failure 15 s).
//! - `GET /api/series/<id>` ([`api_detail`]): seasons and episodes with a file: number, title,
//!   watched, file name, size, codec, the video state (direct / needs converting / converted /
//!   converting with its percent / queued), the RO subtitle (Bazarr
//!   `GET /api/episodes?seriesid[]=<id>`: `subtitles[].code2`, `missing_subtitles[].code2`; a
//!   running Bazarr translation job (`GET /api/system/jobs`, `job_name` with "Translating",
//!   `progress_message` the `.en.srt` path) for that file is "translating"). Cached 20 s; the
//!   running batch is merged in on every request.
//! - `GET /api/movies/<id>` ([`api_movie`]): the same for one Radarr movie (the Filme sheet).
//!
//! A service that is down blanks only its part (Plex: watched unknown, nothing can start;
//! Bazarr: no subtitle state). Sonarr/Radarr down is an error. Demo: fixed data built through
//! the same functions ([`demo_grid`], [`demo_detail`], [`demo_movie`]).

use crate::app::AppState;
use crate::jobs::common::id_of;
use crate::jobs::convert::{
    candidates, converted_keys, file_needs_conversion, ledger_error, ledger_path, load_ledger, plex_movie_views, plex_views, Settings,
    DEFAULT_COUNT, MAX_COUNT, RADARR_DOWN, SONARR_DOWN,
};
use crate::logic::images::poster_url;
use crate::logic::movies::{bazarr_ro_map, ro_of};
use crate::services::arr_jobs::Arr;
use crate::services::plex_jobs::path_key;
use crate::services::UpstreamError;
use crate::log_warning;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

const BAZARR_TIMEOUT: Duration = Duration::from_secs(10);

// -- pure helpers --------------------------------------------------------------------------

/// `H.264, 10 biți` from a Sonarr/Radarr `mediaInfo` (`""` when unknown).
pub fn codec_label(media_info: Option<&Value>) -> String {
    let codec = media_info.and_then(|m| m.get("videoCodec")).and_then(Value::as_str).unwrap_or("").trim();
    let depth = media_info.and_then(|m| m.get("videoBitDepth")).and_then(Value::as_i64).unwrap_or(0);
    let name = match codec.to_ascii_lowercase().as_str() {
        "" => return String::new(),
        "x264" | "h264" | "avc" => "H.264".to_string(),
        "x265" | "h265" | "hevc" => "HEVC".to_string(),
        "av1" => "AV1".to_string(),
        _ => codec.to_string(),
    };
    if depth > 0 {
        format!("{name}, {depth} biți")
    } else {
        name
    }
}

/// The file name of a Windows or POSIX path.
pub fn basename(path: &str) -> String {
    path.rsplit(['\\', '/']).next().unwrap_or("").to_string()
}

fn has_ro(list: Option<&Value>) -> bool {
    list.and_then(Value::as_array).is_some_and(|a| {
        a.iter().any(|s| {
            s.get("code2").and_then(Value::as_str).is_some_and(|c| c.eq_ignore_ascii_case("ro")) && !s.get("forced").and_then(Value::as_bool).unwrap_or(false)
        })
    })
}

/// The RO subtitle of one Bazarr record: `ro` (there), `translating`, `missing`, or None
/// (Bazarr does not know the file or does not want RO for it).
pub fn ro_status(rec: Option<&Value>, translating: bool) -> Option<&'static str> {
    if rec.is_some_and(|r| has_ro(r.get("subtitles"))) {
        return Some("ro");
    }
    if translating {
        return Some("translating");
    }
    rec.filter(|r| has_ro(r.get("missing_subtitles"))).map(|_| "missing")
}

/// The texts of Bazarr's translation jobs that still run (lower-case, `\` separators).
pub fn translation_texts(jobs: &[Value]) -> Vec<String> {
    let s = |j: &Value, k: &str| j.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    jobs.iter()
        .filter(|j| s(j, "job_name").to_lowercase().contains("translat"))
        .filter(|j| !matches!(s(j, "status").to_lowercase().as_str(), "completed" | "failed" | "done" | "cancelled" | "canceled"))
        .map(|j| format!("{} {}", s(j, "job_name"), s(j, "progress_message")).to_lowercase().replace('/', "\\"))
        .collect()
}

/// A translation job is working on the subtitle of `path` (`<stem>.en.srt` names it).
pub fn translating(texts: &[String], path: &str) -> bool {
    let name = basename(path).to_lowercase();
    let stem = name.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or(name);
    !stem.is_empty() && texts.iter().any(|t| t.contains(&format!("{stem}.")))
}

fn sort_key(s: &Value) -> String {
    s.get("sortTitle").or_else(|| s.get("title")).and_then(Value::as_str).unwrap_or("").to_lowercase()
}

fn watched_in(views: Option<&HashMap<String, i64>>, path: &str) -> Option<bool> {
    views.map(|v| v.get(&path_key(path)).is_some_and(|n| *n > 0))
}

/// The grid rows: `{"id", "title", "year", "poster", "files", "unwatched", "convert"}` by
/// title. `views` None (Plex down): `unwatched` null and `convert` counts every file that
/// needs it.
pub fn grid_rows(list: &[(Value, Vec<Value>)], views: Option<&HashMap<String, i64>>, converted: &HashSet<String>) -> Vec<Value> {
    let mut rows: Vec<(String, Value)> = list
        .iter()
        .filter_map(|(s, files)| {
            let id = id_of(s.get("id"))?;
            let path = |f: &Value| f.get("path").and_then(Value::as_str).unwrap_or("").to_string();
            let seen = |f: &Value| watched_in(views, &path(f)) == Some(true);
            let unwatched = views.map(|_| files.iter().filter(|f| !seen(f)).count());
            let convert = files.iter().filter(|f| file_needs_conversion(f) && !converted.contains(&path_key(&path(f))) && !seen(f)).count();
            let row = json!({
                "id": id, "title": s.get("title").and_then(Value::as_str).unwrap_or(""), "year": s.get("year").cloned().unwrap_or(Value::Null),
                "poster": poster_url("series", &id.to_string()), "files": files.len(), "unwatched": unwatched, "convert": convert,
            });
            Some((sort_key(s), row))
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.into_iter().map(|(_, r)| r).collect()
}

/// What one series screen is built from.
pub struct DetailIn<'a> {
    pub series: &'a Value,
    pub episodes: &'a [Value],
    pub files: &'a [Value],
    /// Plex `viewCount` by path key (None: Plex down).
    pub views: Option<&'a HashMap<String, i64>>,
    pub converted: &'a HashSet<String>,
    /// Bazarr's episode records (None: Bazarr down).
    pub bazarr: Option<&'a [Value]>,
    /// Bazarr's job list (None: unknown).
    pub jobs: Option<&'a [Value]>,
}

fn season_order(n: i64) -> (bool, i64) {
    (n == 0, n)
}

/// The series screen (see the module docs), without the running batch ([`merge_job`]).
pub fn detail(d: &DetailIn) -> Value {
    let id = id_of(d.series.get("id")).unwrap_or(0);
    let by_id: HashMap<i64, &Value> = d.files.iter().filter_map(|f| Some((id_of(f.get("id"))?, f))).collect();
    let cands = d.views.map(|v| candidates(d.series, d.episodes, d.files, v, d.converted));
    let cand_ids: HashSet<i64> = cands.iter().flatten().map(|e| e.episode_id).collect();
    let ro_recs: HashMap<i64, &Value> = d.bazarr.unwrap_or(&[]).iter().filter_map(|r| Some((id_of(r.get("sonarrEpisodeId"))?, r))).collect();
    let texts = translation_texts(d.jobs.unwrap_or(&[]));
    let mut rows: Vec<(i64, i64, Value)> = Vec::new();
    for e in d.episodes {
        let Some(f) = id_of(e.get("episodeFileId")).and_then(|fid| by_id.get(&fid)) else { continue };
        let Some(path) = f.get("path").and_then(Value::as_str) else { continue };
        let eid = id_of(e.get("id")).unwrap_or(0);
        let (season, number) = (e.get("seasonNumber").and_then(Value::as_i64).unwrap_or(0), e.get("episodeNumber").and_then(Value::as_i64).unwrap_or(0));
        let converted = d.converted.contains(&path_key(path));
        let ro = d.bazarr.and_then(|_| ro_status(ro_recs.get(&eid).copied(), translating(&texts, path)));
        rows.push((
            season,
            number,
            json!({
                "id": eid, "season": season, "episode": number, "label": crate::jobs::convert::label(season, number),
                "title": e.get("title").and_then(Value::as_str).unwrap_or(""),
                "watched": watched_in(d.views, path), "file": basename(path), "size": f.get("size").and_then(Value::as_u64).unwrap_or(0),
                "codec": codec_label(f.get("mediaInfo")), "needs_conversion": file_needs_conversion(f) && !converted,
                "converted": converted, "can_convert": cand_ids.contains(&eid), "ro_sub": ro, "converting": null, "queued": false,
            }),
        ));
    }
    rows.sort_by_key(|(s, n, _)| (season_order(*s), *n));
    let mut seasons: Vec<(i64, Vec<Value>)> = Vec::new();
    for (s, _, row) in &rows {
        match seasons.last_mut() {
            Some((n, list)) if *n == *s => list.push(row.clone()),
            _ => seasons.push((*s, vec![row.clone()])),
        }
    }
    let unwatched = d.views.map(|_| rows.iter().filter(|(_, _, r)| r["watched"] == json!(false)).count());
    let next_unwatched = rows.iter().find(|(s, _, r)| *s != 0 && r["watched"] == json!(false)).map(|(s, _, _)| *s);
    let last_regular = seasons.iter().rev().find(|(s, _)| *s != 0).map(|(s, _)| *s);
    let default_season = next_unwatched
        .or(if d.views.is_some() { last_regular } else { None })
        .or_else(|| seasons.first().map(|(s, _)| *s));
    json!({
        "id": id,
        "title": d.series.get("title").and_then(Value::as_str).unwrap_or(""),
        "year": d.series.get("year").cloned().unwrap_or(Value::Null),
        "poster": poster_url("series", &id.to_string()),
        "backdrop": poster_url("series-art", &id.to_string()),
        "seasons": seasons.iter().map(|(n, eps)| json!({"number": n, "label": if *n == 0 { "Speciale".to_string() } else { format!("S{n}") }, "episodes": eps})).collect::<Vec<_>>(),
        "default_season": default_season,
        "pending": cands.as_ref().map(Vec::len),
        "next": cands.as_ref().and_then(|c| c.first()).map(|e| e.label()),
        "unwatched": unwatched,
        "plex": d.views.is_some(),
        "bazarr": d.bazarr.is_some(),
    })
}

/// The running (or just finished) batch into a series screen: the episode converting and
/// its percent, the queued ones, the ones converted by this batch.
pub fn merge_job(detail: &mut Value, job: &Value) {
    let id = detail["id"].as_i64().unwrap_or(0);
    if job.is_null() || job["movie_id"].as_i64().unwrap_or(0) != 0 || job["series_id"].as_i64() != Some(id) {
        return;
    }
    let running = job["running"].as_bool().unwrap_or(false);
    let current = job["current"]["episode_id"].as_i64().filter(|_| running);
    let pct = job["current"]["percent"].clone();
    let queued: HashSet<i64> = if running { job["queued"].as_array().into_iter().flatten().filter_map(Value::as_i64).collect() } else { HashSet::new() };
    let done: HashSet<i64> = job["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["result"] == json!("ok"))
        .filter_map(|i| i["episode_id"].as_i64())
        .collect();
    let mut converted_now = 0;
    for s in detail["seasons"].as_array_mut().into_iter().flatten() {
        for e in s["episodes"].as_array_mut().into_iter().flatten() {
            let eid = e["id"].as_i64().unwrap_or(0);
            if Some(eid) == current {
                e["converting"] = json!({"percent": pct});
            }
            if queued.contains(&eid) {
                e["queued"] = json!(true);
            }
            if done.contains(&eid) && e["converted"] != json!(true) {
                e["converted"] = json!(true);
                e["needs_conversion"] = json!(false);
                if e["can_convert"] == json!(true) {
                    e["can_convert"] = json!(false);
                    converted_now += 1;
                }
            }
        }
    }
    if let Some(p) = detail["pending"].as_u64() {
        detail["pending"] = json!(p.saturating_sub(converted_now));
    }
}

/// The movie sheet: `{"id", "title", "year", "poster", "file", "size", "codec", "watched",
/// "needs_conversion", "converted", "can_convert", "ro_sub", "converting", "plex"}`.
pub fn movie_detail(movie: &Value, views: Option<&HashMap<String, i64>>, converted: &HashSet<String>, ro: Option<Option<bool>>, texts: &[String]) -> Value {
    let id = id_of(movie.get("id")).unwrap_or(0);
    let file = movie.get("movieFile").filter(|f| f.is_object());
    let path = file.and_then(|f| f.get("path")).and_then(Value::as_str).unwrap_or("");
    let has = !path.trim().is_empty();
    let conv = has && converted.contains(&path_key(path));
    let needs = has && file.is_some_and(file_needs_conversion) && !conv;
    let watched = if has { watched_in(views, path) } else { None };
    let ro_sub = if has && translating(texts, path) && ro != Some(Some(true)) {
        Some("translating")
    } else {
        match ro {
            Some(Some(true)) => Some("ro"),
            Some(Some(false)) => Some("missing"),
            _ => None,
        }
    };
    json!({
        "id": id, "title": movie.get("title").and_then(Value::as_str).unwrap_or(""), "year": movie.get("year").cloned().unwrap_or(Value::Null),
        "poster": poster_url("movie", &id.to_string()), "file": if has { Value::from(basename(path)) } else { Value::Null },
        "size": file.and_then(|f| f.get("size")).and_then(Value::as_u64).unwrap_or(0), "codec": codec_label(file.and_then(|f| f.get("mediaInfo"))),
        "watched": watched, "needs_conversion": needs, "converted": conv, "can_convert": needs && watched == Some(false),
        "ro_sub": ro_sub, "converting": null, "plex": views.is_some(),
    })
}

/// The running batch into a movie sheet.
pub fn merge_movie_job(m: &mut Value, job: &Value) {
    let id = m["id"].as_i64().unwrap_or(0);
    if job.is_null() || job["movie_id"].as_i64() != Some(id) || id == 0 {
        return;
    }
    if job["running"].as_bool().unwrap_or(false) && !job["current"].is_null() {
        m["converting"] = json!({"percent": job["current"]["percent"].clone()});
    }
    if job["items"].as_array().into_iter().flatten().any(|i| i["result"] == json!("ok")) {
        m["converted"] = json!(true);
        m["needs_conversion"] = json!(false);
        m["can_convert"] = json!(false);
    }
}

// -- upstream ------------------------------------------------------------------------------

/// Bazarr `GET /api/system/jobs` (`{"data": [...]}` or a bare list); None if it fails.
async fn bazarr_jobs(app: &AppState) -> Option<Vec<Value>> {
    let bz = &app.services.bazarr;
    let key = bz.key();
    let url = bz.c.url("bazarr", "/api/system/jobs");
    match bz.c.http.get_json(&url, &[("X-API-KEY", key.as_str())], BAZARR_TIMEOUT).await {
        Ok(Some(Value::Array(a))) => Some(a),
        Ok(Some(Value::Object(mut m))) => match m.remove("data") {
            Some(Value::Array(a)) => Some(a),
            _ => None,
        },
        _ => None,
    }
}

/// The Seriale grid ([`grid_rows`]) from Sonarr, Plex and the ledger. `Err`: a friendly message.
pub async fn grid(app: &AppState) -> Result<Vec<Value>, String> {
    let arr = app.services.sonarr.arr();
    let series = arr.items().await.map_err(|e| {
        log_warning!("series: Sonarr series failed: {}", e.type_name());
        SONARR_DOWN.to_string()
    })?;
    let converted = converted_keys(&load_ledger(&ledger_path(app)).map_err(|t| ledger_error(&t))?);
    let mut list = Vec::new();
    for s in series {
        let Some(id) = id_of(s.get("id")) else { continue };
        if s.get("statistics").and_then(|x| x.get("episodeFileCount")).and_then(Value::as_i64) == Some(0) {
            continue;
        }
        let files = arr.episode_files(id).await.map_err(|_| SONARR_DOWN.to_string())?;
        if !files.is_empty() {
            list.push((s, files));
        }
    }
    let views = if list.is_empty() { None } else { plex_views(app, Settings::from_app(app).section).await.ok() };
    Ok(grid_rows(&list, views.as_ref(), &converted))
}

/// `GET /api/series`.
pub async fn api_list(app: &Arc<AppState>) -> Value {
    if app.demo {
        return json!({"series": demo_grid(), "error": null, "loading": false, "demo": true, "dry_run": true});
    }
    let (series, error, loading) = app.convert.listing(app).await;
    json!({"series": series, "error": error, "loading": loading, "demo": false, "dry_run": app.dry_run})
}

async fn load_detail(app: &AppState, id: i64) -> Result<Value, (u16, String)> {
    let arr = app.services.sonarr.arr();
    let down = || (502, SONARR_DOWN.to_string());
    let series = match arr.series(id).await {
        Ok(s) => s,
        Err(UpstreamError::Status(404)) => return Err((404, "Serialul nu există în Sonarr.".into())),
        Err(e) => {
            log_warning!("series: Sonarr series {id} failed: {}", e.type_name());
            return Err(down());
        }
    };
    if id_of(series.get("id")) != Some(id) {
        return Err((404, "Serialul nu există în Sonarr.".into()));
    }
    let episodes = arr.episodes(id).await.map_err(|_| down())?;
    let files = arr.episode_files(id).await.map_err(|_| down())?;
    let converted = converted_keys(&load_ledger(&ledger_path(app)).map_err(|t| (502, ledger_error(&t)))?);
    let views = plex_views(app, Settings::from_app(app).section).await.ok();
    let bazarr = app.services.bazarr.data(&format!("/api/episodes?seriesid%5B%5D={id}"), BAZARR_TIMEOUT).await.ok();
    let jobs = if bazarr.is_some() { bazarr_jobs(app).await } else { None };
    Ok(detail(&DetailIn {
        series: &series,
        episodes: &episodes,
        files: &files,
        views: views.as_ref(),
        converted: &converted,
        bazarr: bazarr.as_deref(),
        jobs: jobs.as_deref(),
    }))
}

fn with_status(mut v: Value, job: Value, dry_run: bool, demo: bool) -> Value {
    if let Value::Object(m) = &mut v {
        m.insert("job".into(), job);
        m.insert("dry_run".into(), json!(dry_run));
        m.insert("demo".into(), json!(demo));
        m.insert("max".into(), json!(MAX_COUNT));
        m.insert("default".into(), json!(DEFAULT_COUNT));
    }
    v
}

/// `GET /api/series/<id>` -> `(status, body)`.
pub async fn api_detail(app: &Arc<AppState>, id: i64) -> (u16, Value) {
    if app.demo {
        let job = crate::jobs::convert::demo_job(app.started.elapsed().as_secs_f64());
        return match demo_detail(id) {
            Some(mut v) => {
                merge_job(&mut v, &job);
                (200, with_status(v, job, true, true))
            }
            None => (404, json!({"error": "Serialul nu există în Sonarr."})),
        };
    }
    let base = match app.convert.cached_detail(id) {
        Some(v) => v,
        None => match load_detail(app, id).await {
            Ok(v) => {
                app.convert.store_detail(id, v.clone());
                v
            }
            Err((code, m)) => return (code, json!({"error": m})),
        },
    };
    let job = app.convert.status_json();
    let mut v = base;
    merge_job(&mut v, &job);
    (200, with_status(v, job, app.dry_run, false))
}

/// `GET /api/movies/<id>` -> `(status, body)`.
pub async fn api_movie(app: &Arc<AppState>, id: i64) -> (u16, Value) {
    if app.demo {
        let job = crate::jobs::convert::demo_job(app.started.elapsed().as_secs_f64());
        return match demo_movie(id) {
            Some(v) => (200, with_status(v, job, true, true)),
            None => (404, json!({"error": "Filmul nu există în Radarr."})),
        };
    }
    let movie = match Arr::radarr(app.services.radarr.c.clone()).movie(id).await {
        Ok(m) if id_of(m.get("id")) == Some(id) => m,
        Ok(_) | Err(UpstreamError::Status(404)) => return (404, json!({"error": "Filmul nu există în Radarr."})),
        Err(e) => {
            log_warning!("series: Radarr movie {id} failed: {}", e.type_name());
            return (502, json!({"error": RADARR_DOWN}));
        }
    };
    let converted = match load_ledger(&ledger_path(app)) {
        Ok(l) => converted_keys(&l),
        Err(t) => return (502, json!({"error": ledger_error(&t)})),
    };
    let views = plex_movie_views(app, Settings::from_app(app).movie_section).await.ok();
    let ro = app.services.bazarr.movies().await.ok().map(|m| ro_of(&bazarr_ro_map(&m), movie.get("id")));
    let jobs = if ro.is_some() { bazarr_jobs(app).await.unwrap_or_default() } else { Vec::new() };
    let mut v = movie_detail(&movie, views.as_ref(), &converted, ro, &translation_texts(&jobs));
    let job = app.convert.status_json();
    merge_movie_job(&mut v, &job);
    (200, with_status(v, job, app.dry_run, false))
}

// -- demo ----------------------------------------------------------------------------------

/// A demo episode id: series * 10000 + season * 100 + episode.
pub fn demo_episode_id(series: i64, season: i64, episode: i64) -> i64 {
    series * 10000 + season * 100 + episode
}

/// Per demo series: (season, episodes, watched up to, codec, bit depth).
fn demo_seasons(sid: i64) -> &'static [(i64, i64, i64, &'static str, i64)] {
    match sid {
        1 => &[(1, 12, 4, "x264", 10), (2, 10, 0, "x265", 10), (0, 1, 0, "x264", 10)],
        2 => &[(1, 6, 6, "x264", 8), (2, 6, 3, "x264", 10)],
        3 => &[(1, 9, 9, "x265", 10), (2, 10, 6, "x265", 10)],
        4 => &[(1, 10, 0, "x265", 10)],
        5 => &[(1, 8, 8, "h264", 8)],
        _ => &[],
    }
}

struct DemoWorld {
    series: Value,
    episodes: Vec<Value>,
    files: Vec<Value>,
    views: HashMap<String, i64>,
    converted: HashSet<String>,
    bazarr: Vec<Value>,
    jobs: Vec<Value>,
}

fn demo_world(sid: i64) -> Option<DemoWorld> {
    let &(_, title, year, ..) = crate::logic::demo::series(u32::try_from(sid).ok()?)?;
    let folder = title.replace(':', "");
    let mut w = DemoWorld {
        series: json!({"id": sid, "title": title, "year": year}),
        episodes: vec![],
        files: vec![],
        views: HashMap::new(),
        converted: HashSet::new(),
        bazarr: vec![],
        jobs: vec![],
    };
    for &(season, count, watched, codec, depth) in demo_seasons(sid) {
        for n in 1..=count {
            let id = demo_episode_id(sid, season, n);
            let path = format!("D:\\Media\\Seriale\\{folder}\\Season {season:02}\\{folder} - S{season:02}E{n:02}.mkv");
            let size: u64 = if codec == "x264" { 1_450_000_000 } else { 2_150_000_000 } + n as u64 * 7_000_000;
            w.files.push(json!({"id": id, "path": path, "size": size, "mediaInfo": {"videoCodec": codec, "videoBitDepth": depth}}));
            w.episodes.push(json!({"id": id, "seasonNumber": season, "episodeNumber": n, "title": format!("Episodul {n}"), "episodeFileId": id, "hasFile": true}));
            w.views.insert(path_key(&path), i64::from(n <= watched));
            let ro = |has: bool| {
                let list = json!([{"code2": "ro", "forced": false}]);
                if has {
                    json!({"sonarrEpisodeId": id, "subtitles": list, "missing_subtitles": []})
                } else {
                    json!({"sonarrEpisodeId": id, "subtitles": [], "missing_subtitles": list})
                }
            };
            match (sid, season) {
                (_, 0) => {}
                (1, 1) => w.bazarr.push(ro(n <= 5)),
                (1, 2) => w.bazarr.push(ro(n <= 3)),
                _ => w.bazarr.push(ro(n <= watched || n % 2 == 0)),
            }
            if (sid, season, n) == (1, 1, 5) {
                w.converted.insert(path_key(&path));
            }
            if (sid, season, n) == (1, 1, 6) {
                let srt = path.replace(".mkv", ".en.srt");
                w.jobs.push(json!({"job_name": "Translating from English to Romanian", "status": "running", "progress_message": srt}));
            }
        }
    }
    Some(w)
}

/// The demo grid (through [`grid_rows`]).
pub fn demo_grid() -> Vec<Value> {
    let worlds: Vec<DemoWorld> = crate::logic::demo::SERIES.iter().filter_map(|s| demo_world(s.0 as i64)).collect();
    let mut views = HashMap::new();
    let mut converted = HashSet::new();
    for w in &worlds {
        views.extend(w.views.clone());
        converted.extend(w.converted.clone());
    }
    let list: Vec<(Value, Vec<Value>)> = worlds.into_iter().map(|w| (w.series, w.files)).collect();
    grid_rows(&list, Some(&views), &converted)
}

/// A demo series screen (through [`detail`]), without the batch.
pub fn demo_detail(sid: i64) -> Option<Value> {
    let w = demo_world(sid)?;
    Some(detail(&DetailIn {
        series: &w.series,
        episodes: &w.episodes,
        files: &w.files,
        views: Some(&w.views),
        converted: &w.converted,
        bazarr: Some(&w.bazarr),
        jobs: Some(&w.jobs),
    }))
}

/// A demo movie sheet: The Godfather is Hi10P (the button shows), the others are HEVC.
pub fn demo_movie(rid: i64) -> Option<Value> {
    let &(_, title, year, ..) = crate::logic::demo::MOVIES.iter().find(|m| i64::from(m.0) == rid)?;
    let (codec, depth, size, ro, watched) = match rid {
        1 => ("x264", 10, 19_000_000_000u64, true, 0),
        2 => ("x265", 10, 61_800_000_000, true, 1),
        4 => ("x265", 10, 19_100_000_000, false, 0),
        _ => return Some(movie_detail(&json!({"id": rid, "title": title, "year": year}), Some(&HashMap::new()), &HashSet::new(), None, &[])),
    };
    let path = format!("D:\\Media\\Movies\\{title} ({year})\\{} ({year}).mkv", title.replace(':', ""));
    let movie = json!({"id": rid, "title": title, "year": year,
                       "movieFile": {"path": path, "size": size, "mediaInfo": {"videoCodec": codec, "videoBitDepth": depth}}});
    let views: HashMap<String, i64> = [(path_key(&path), watched)].into_iter().collect();
    Some(movie_detail(&movie, Some(&views), &HashSet::new(), Some(Some(ro)), &[]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::testutil::{app, fake, ok, status, Req};

    fn file(id: i64, path: &str, codec: &str, depth: i64) -> Value {
        json!({"id": id, "path": path, "size": 1_500_000_000u64, "mediaInfo": {"videoCodec": codec, "videoBitDepth": depth}})
    }

    fn ep(id: i64, season: i64, number: i64, file_id: i64) -> Value {
        json!({"id": id, "seasonNumber": season, "episodeNumber": number, "title": format!("T{number}"), "episodeFileId": file_id})
    }

    #[test]
    fn helpers() {
        assert_eq!(codec_label(Some(&json!({"videoCodec": "x264", "videoBitDepth": 10}))), "H.264, 10 biți");
        assert_eq!(codec_label(Some(&json!({"videoCodec": "HEVC", "videoBitDepth": 0}))), "HEVC");
        assert_eq!(codec_label(Some(&json!({"videoCodec": "VC1", "videoBitDepth": 8}))), "VC1, 8 biți");
        assert_eq!(codec_label(None), "");
        assert_eq!(basename("D:\\S\\Show - S01E01.mkv"), "Show - S01E01.mkv");
        assert_eq!(basename("/m/x.mkv"), "x.mkv");
        let ro = json!([{"code2": "ro"}]);
        assert_eq!(ro_status(Some(&json!({"subtitles": ro, "missing_subtitles": []})), true), Some("ro"));
        assert_eq!(ro_status(Some(&json!({"subtitles": [], "missing_subtitles": ro})), false), Some("missing"));
        assert_eq!(ro_status(Some(&json!({"subtitles": [], "missing_subtitles": ro})), true), Some("translating"));
        assert_eq!(ro_status(Some(&json!({"subtitles": [{"code2": "ro", "forced": true}], "missing_subtitles": []})), false), None);
        assert_eq!(ro_status(None, false), None);
        let jobs = vec![
            json!({"job_name": "Translating from English to Romanian", "status": "running", "progress_message": "D:/S/Show - S01E04.en.srt"}),
            json!({"job_name": "Translating", "status": "completed", "progress_message": "D:\\S\\Show - S01E05.en.srt"}),
            json!({"job_name": "Searching subtitles", "status": "running", "progress_message": "D:\\S\\Show - S01E06.en.srt"}),
        ];
        let t = translation_texts(&jobs);
        assert_eq!(t.len(), 1);
        assert!(translating(&t, "D:\\S\\Show - S01E04.mkv"));
        assert!(!translating(&t, "D:\\S\\Show - S01E05.mkv"));
        assert!(!translating(&t, "D:\\S\\Show - S01E0.mkv"));
        assert!(!translating(&t, ""));
    }

    fn show() -> Value {
        json!({"id": 4, "title": "Show", "year": 2020})
    }

    #[test]
    fn grid_counts() {
        let files = vec![file(1, "D:\\S\\e1.mkv", "x264", 10), file(2, "D:\\S\\e2.mkv", "x264", 10), file(3, "D:\\S\\e3.mkv", "x264", 10), file(4, "D:\\S\\e4.mkv", "x265", 10)];
        let list = vec![(show(), files), (json!({"id": 5, "title": "Another", "sortTitle": "another"}), vec![file(9, "D:\\A\\x.mkv", "h264", 8)])];
        let views: HashMap<String, i64> = [(path_key("D:\\S\\e1.mkv"), 1)].into_iter().collect();
        let converted: HashSet<String> = [path_key("D:\\S\\e2.mkv")].into_iter().collect();
        let rows = grid_rows(&list, Some(&views), &converted);
        assert_eq!(rows[0]["title"], json!("Another"));
        assert_eq!(rows[1], json!({"id": 4, "title": "Show", "year": 2020, "poster": "/img/series/4", "files": 4, "unwatched": 3, "convert": 1}));
        // Plex down: watched unknown, every file that needs it counts.
        let rows = grid_rows(&list, None, &converted);
        assert_eq!((rows[1]["unwatched"].clone(), rows[1]["convert"].clone()), (Value::Null, json!(2)));
    }

    /// files, episodes, views, converted, Bazarr records, Bazarr jobs.
    type World = (Vec<Value>, Vec<Value>, HashMap<String, i64>, HashSet<String>, Vec<Value>, Vec<Value>);

    fn detail_world() -> World {
        let files = vec![
            file(1, "D:\\S\\e1.mkv", "x264", 10),
            file(2, "D:\\S\\e2.mkv", "x264", 10),
            file(3, "D:\\S\\e3.mkv", "x264", 10),
            file(4, "D:\\S\\e4-5.mkv", "x264", 10),
            file(6, "D:\\S\\s2e1.mkv", "x265", 10),
            file(7, "D:\\S\\sp.mkv", "x264", 10),
        ];
        let eps = vec![ep(11, 1, 1, 1), ep(12, 1, 2, 2), ep(13, 1, 3, 3), ep(14, 1, 4, 4), ep(15, 1, 5, 4), ep(21, 2, 1, 6), ep(31, 0, 1, 7), ep(16, 1, 6, 0)];
        let views: HashMap<String, i64> = [(path_key("D:\\S\\e1.mkv"), 2), (path_key("D:\\S\\e2.mkv"), 0)].into_iter().collect();
        let converted: HashSet<String> = [path_key("D:\\S\\e2.mkv")].into_iter().collect();
        let ro = json!([{"code2": "ro", "forced": false}]);
        let bazarr = vec![
            json!({"sonarrEpisodeId": 11, "subtitles": ro, "missing_subtitles": []}),
            json!({"sonarrEpisodeId": 12, "subtitles": [], "missing_subtitles": ro}),
            json!({"sonarrEpisodeId": 13, "subtitles": [], "missing_subtitles": ro}),
        ];
        let jobs = vec![json!({"job_name": "Translating", "status": "running", "progress_message": "D:\\S\\e3.en.srt"})];
        (files, eps, views, converted, bazarr, jobs)
    }

    #[test]
    fn detail_rows() {
        let (files, eps, views, converted, bazarr, jobs) = detail_world();
        let s = show();
        let d = detail(&DetailIn { series: &s, episodes: &eps, files: &files, views: Some(&views), converted: &converted, bazarr: Some(&bazarr), jobs: Some(&jobs) });
        let seasons: Vec<i64> = d["seasons"].as_array().unwrap().iter().map(|s| s["number"].as_i64().unwrap()).collect();
        assert_eq!(seasons, [1, 2, 0], "specials last, episodes without a file left out");
        assert_eq!(d["seasons"][2]["label"], json!("Speciale"));
        let s1 = d["seasons"][0]["episodes"].as_array().unwrap();
        assert_eq!(s1.len(), 5);
        // e1 watched; e2 converted; e3 needs it, translating; e4-5 one file (only E04 starts it).
        assert_eq!((s1[0]["watched"].clone(), s1[0]["ro_sub"].clone(), s1[0]["can_convert"].clone()), (json!(true), json!("ro"), json!(false)));
        assert_eq!((s1[1]["converted"].clone(), s1[1]["needs_conversion"].clone(), s1[1]["ro_sub"].clone()), (json!(true), json!(false), json!("missing")));
        assert_eq!((s1[2]["can_convert"].clone(), s1[2]["ro_sub"].clone(), s1[2]["file"].clone()), (json!(true), json!("translating"), json!("e3.mkv")));
        assert_eq!((s1[3]["can_convert"].clone(), s1[4]["can_convert"].clone(), s1[4]["needs_conversion"].clone()), (json!(true), json!(false), json!(true)));
        assert_eq!((s1[3]["codec"].clone(), s1[3]["label"].clone(), s1[3]["ro_sub"].clone()), (json!("H.264, 10 biți"), json!("S01E04"), Value::Null));
        assert_eq!(d["seasons"][1]["episodes"][0]["needs_conversion"], json!(false));
        assert_eq!((d["pending"].clone(), d["next"].clone(), d["unwatched"].clone()), (json!(3), json!("S01E03"), json!(6)));
        assert_eq!((d["default_season"].clone(), d["plex"].clone(), d["bazarr"].clone()), (json!(1), json!(true), json!(true)));
        assert_eq!((d["poster"].clone(), d["backdrop"].clone()), (json!("/img/series/4"), json!("/img/series-art/4")));
        // Plex and Bazarr down: nothing can start, no subtitle state.
        let d = detail(&DetailIn { series: &s, episodes: &eps, files: &files, views: None, converted: &converted, bazarr: None, jobs: None });
        assert_eq!((d["pending"].clone(), d["unwatched"].clone(), d["default_season"].clone()), (Value::Null, Value::Null, json!(1)));
        assert!(d["seasons"][0]["episodes"].as_array().unwrap().iter().all(|e| e["can_convert"] == json!(false) && e["watched"].is_null() && e["ro_sub"].is_null()));
        // Everything watched: the last regular season.
        let all: HashMap<String, i64> = files.iter().map(|f| (path_key(f["path"].as_str().unwrap()), 1)).collect();
        let d = detail(&DetailIn { series: &s, episodes: &eps, files: &files, views: Some(&all), converted: &converted, bazarr: None, jobs: None });
        assert_eq!((d["default_season"].clone(), d["pending"].clone()), (json!(2), json!(0)));
    }

    #[test]
    fn the_batch_is_merged() {
        let (files, eps, views, converted, bazarr, jobs) = detail_world();
        let s = show();
        let mut d = detail(&DetailIn { series: &s, episodes: &eps, files: &files, views: Some(&views), converted: &converted, bazarr: Some(&bazarr), jobs: Some(&jobs) });
        let job = json!({"series_id": 4, "movie_id": 0, "running": true, "current": {"label": "S01E04", "percent": 45.5, "episode_id": 14},
                         "queued": [31], "items": [{"label": "S01E03", "episode_id": 13, "result": "ok"}]});
        merge_job(&mut d, &job);
        let s1 = &d["seasons"][0]["episodes"];
        assert_eq!(s1[3]["converting"], json!({"percent": 45.5}));
        assert_eq!((s1[2]["converted"].clone(), s1[2]["can_convert"].clone()), (json!(true), json!(false)));
        assert_eq!(d["seasons"][2]["episodes"][0]["queued"], json!(true));
        assert_eq!(d["pending"], json!(2));
        // Another series' batch changes nothing.
        let mut other = detail(&DetailIn { series: &s, episodes: &eps, files: &files, views: Some(&views), converted: &converted, bazarr: None, jobs: None });
        let before = other.clone();
        merge_job(&mut other, &json!({"series_id": 9, "movie_id": 0, "running": true, "current": {"episode_id": 14, "percent": 1}}));
        merge_job(&mut other, &Value::Null);
        assert_eq!(other, before);
    }

    #[test]
    fn movie_sheet() {
        let path = "D:\\Media\\Movies\\Film (2020)\\Film.mkv";
        let m = json!({"id": 7, "title": "Film", "year": 2020, "movieFile": {"path": path, "size": 9, "mediaInfo": {"videoCodec": "x264", "videoBitDepth": 10}}});
        let views: HashMap<String, i64> = HashMap::new();
        let v = movie_detail(&m, Some(&views), &HashSet::new(), Some(Some(false)), &[]);
        assert_eq!((v["can_convert"].clone(), v["watched"].clone(), v["ro_sub"].clone(), v["file"].clone()), (json!(true), json!(false), json!("missing"), json!("Film.mkv")));
        let texts = translation_texts(&[json!({"job_name": "Translating", "progress_message": "D:\\Media\\Movies\\Film (2020)\\Film.en.srt"})]);
        assert_eq!(movie_detail(&m, Some(&views), &HashSet::new(), Some(Some(false)), &texts)["ro_sub"], json!("translating"));
        let watched: HashMap<String, i64> = [(path_key(path), 1)].into_iter().collect();
        let v = movie_detail(&m, Some(&watched), &HashSet::new(), None, &[]);
        assert_eq!((v["needs_conversion"].clone(), v["can_convert"].clone(), v["ro_sub"].clone()), (json!(true), json!(false), Value::Null));
        let conv: HashSet<String> = [path_key(path)].into_iter().collect();
        let v = movie_detail(&m, None, &conv, None, &[]);
        assert_eq!((v["converted"].clone(), v["needs_conversion"].clone(), v["plex"].clone()), (json!(true), json!(false), json!(false)));
        let mut v = movie_detail(&m, Some(&views), &HashSet::new(), None, &[]);
        merge_movie_job(&mut v, &json!({"movie_id": 7, "running": true, "current": {"percent": 12.0}, "items": []}));
        assert_eq!(v["converting"], json!({"percent": 12.0}));
        let no_file = movie_detail(&json!({"id": 8, "title": "X"}), Some(&views), &HashSet::new(), None, &[]);
        assert_eq!((no_file["file"].clone(), no_file["can_convert"].clone()), (Value::Null, json!(false)));
    }

    #[test]
    fn demo_data() {
        let g = demo_grid();
        assert_eq!(g.len(), 5);
        let frieren = g.iter().find(|r| r["id"] == json!(1)).unwrap();
        assert_eq!((frieren["convert"].clone(), frieren["unwatched"].clone()), (json!(8), json!(19)));
        let mut d = demo_detail(1).unwrap();
        assert_eq!(d["default_season"], json!(1));
        let job = crate::jobs::convert::demo_job(90.0);
        merge_job(&mut d, &job);
        let s1 = &d["seasons"][0]["episodes"];
        assert_eq!((s1[4]["converted"].clone(), s1[5]["converting"]["percent"].clone(), s1[6]["queued"].clone()), (json!(true), json!(10.0), json!(true)));
        assert_eq!(s1[5]["ro_sub"], json!("translating"));
        assert!(demo_detail(99).is_none());
        assert_eq!(demo_movie(1).unwrap()["can_convert"], json!(true));
        assert_eq!(demo_movie(2).unwrap()["can_convert"], json!(false));
        assert!(demo_movie(5).is_some() && demo_movie(42).is_none());
    }

    // -- against fake services -----------------------------------------------------------

    async fn upstream() -> crate::jobs::testutil::Fake {
        fake(|r: &Req| {
            let p = r.path.as_str();
            let ro = json!([{"code2": "ro", "forced": false}]);
            match p {
                "/api/v3/series" => ok(json!([show(), {"id": 5, "title": "Empty", "statistics": {"episodeFileCount": 0}}])),
                "/api/v3/series/4" => ok(show()),
                "/api/v3/series/6" => status(404),
                "/api/v3/movie/7" => ok(json!({"id": 7, "title": "Film", "year": 2020, "hasFile": true,
                                               "movieFile": {"path": "D:\\M\\Film.mkv", "size": 5, "mediaInfo": {"videoCodec": "x264", "videoBitDepth": 10}}})),
                "/api/v3/movie/8" => status(404),
                "/api/movies" => ok(json!({"data": [{"radarrId": 7, "subtitles": ro, "missing_subtitles": []}]})),
                "/api/system/jobs" => ok(json!({"data": [{"job_name": "Translating", "status": "running", "progress_message": "D:\\S\\e2.en.srt"}]})),
                _ if p.starts_with("/api/v3/episodefile?seriesId=4") => ok(json!([file(1, "D:\\S\\e1.mkv", "x264", 10), file(2, "D:\\S\\e2.mkv", "x264", 10)])),
                _ if p.starts_with("/api/v3/episode?seriesId=4") => ok(json!([ep(11, 1, 1, 1), ep(12, 1, 2, 2)])),
                _ if p.starts_with("/library/sections/2/all") => {
                    ok(json!({"MediaContainer": {"Metadata": [{"viewCount": 1, "Media": [{"Part": [{"file": "D:\\S\\e1.mkv"}]}]}]}}))
                }
                _ if p.starts_with("/library/sections/1/all") => ok(json!({"MediaContainer": {"Metadata": []}})),
                _ if p.starts_with("/api/episodes?seriesid%5B%5D=4") => {
                    ok(json!({"data": [{"sonarrEpisodeId": 11, "subtitles": ro, "missing_subtitles": []}, {"sonarrEpisodeId": 12, "subtitles": [], "missing_subtitles": ro}]}))
                }
                _ => status(404),
            }
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn endpoints_against_fakes() {
        let f = upstream().await;
        let (a, _d) = app(&f.base, true, json!({}));
        let v = api_list(&a).await;
        assert_eq!(v["series"], json!([{"id": 4, "title": "Show", "year": 2020, "poster": "/img/series/4", "files": 2, "unwatched": 1, "convert": 1}]));
        assert_eq!((v["error"].clone(), v["loading"].clone()), (Value::Null, json!(false)));
        let (code, d) = api_detail(&a, 4).await;
        assert_eq!(code, 200, "{d}");
        let s1 = &d["seasons"][0]["episodes"];
        assert_eq!((s1[0]["watched"].clone(), s1[0]["ro_sub"].clone()), (json!(true), json!("ro")));
        assert_eq!((s1[1]["can_convert"].clone(), s1[1]["ro_sub"].clone()), (json!(true), json!("translating")));
        assert_eq!((d["pending"].clone(), d["job"].clone(), d["max"].clone()), (json!(1), Value::Null, json!(MAX_COUNT)));
        // Cached: a second request asks nothing upstream.
        let n = f.requests().len();
        assert_eq!(api_detail(&a, 4).await.0, 200);
        assert_eq!(f.requests().len(), n);
        a.convert.invalidate_list();
        assert_eq!(api_detail(&a, 4).await.0, 200);
        assert!(f.requests().len() > n);
        assert_eq!(api_detail(&a, 6).await, (404, json!({"error": "Serialul nu există în Sonarr."})));
        let (code, m) = api_movie(&a, 7).await;
        assert_eq!(code, 200, "{m}");
        assert_eq!((m["can_convert"].clone(), m["ro_sub"].clone(), m["codec"].clone()), (json!(true), json!("ro"), json!("H.264, 10 biți")));
        assert_eq!(api_movie(&a, 8).await.0, 404);
        assert!(f.mutations().is_empty(), "reads only");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sonarr_down_is_a_message() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let (a, _d) = app(&format!("http://127.0.0.1:{port}"), true, json!({}));
        assert_eq!(grid(&a).await, Err(SONARR_DOWN.to_string()));
        assert_eq!(api_detail(&a, 4).await, (502, json!({"error": SONARR_DOWN})));
        assert_eq!(api_movie(&a, 7).await, (502, json!({"error": RADARR_DOWN})));
        let v = api_list(&a).await;
        assert_eq!((v["series"].clone(), v["error"].clone()), (Value::Null, json!(SONARR_DOWN)));
    }
}
