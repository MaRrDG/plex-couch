//! Paced Romanian subtitle search, every 3 minutes, first run 75 s after start.
//!
//! When Sonarr imports a whole season pack, Bazarr searches every episode at once: the
//! providers rate-limit it (regielive TooManyRequests, subtitrari-noi timeouts, both then
//! throttled by Bazarr) and most episodes stay "missing" until Bazarr's own wanted search,
//! every 6 h. This job asks for ONE search per tick, for the episode you will watch next.
//!
//! Per tick: Bazarr's wanted episodes (`GET /api/episodes/wanted?start=0&length=-1`) with a
//! missing non-forced `ro` subtitle; for each series that has one not in backoff, all its
//! episodes (`GET /api/episodes?seriesid[]=N`: season, episode, path) and Plex's `viewCount`
//! / `lastViewedAt` per file (section 2). Watched episodes are left to Bazarr. The order
//! ([`order`]): first the next unwatched episode of every series (after the last watched
//! one), the most recently watched series first; then the one after it; and so on. The first
//! one not in backoff is searched with `PATCH /api/episodes/subtitles?seriesid=..&episodeid=..
//! &language=ro&hi=False&forced=False`. With no episode to search, movies get the same
//! (`/api/movies/wanted`, newest Radarr id first, `PATCH /api/movies/subtitles?radarrid=..`).
//!
//! Backoff per episode/movie after each search: 30 min, then 2 h, then every 6 h
//! ([`backoff_seconds`]), kept in `state.local.json` under `subs_search`. An entry is dropped
//! once Bazarr no longer lists it as wanted (the subtitle was found, or the file is gone);
//! never by an empty or partial list ([`complete_list`]). Specials (season 0) never set
//! "where you are" and come last.
//! Bazarr or Plex down: logged once, nothing searched (Plex down: the order is plain episode
//! order). Dry-run: the search is only logged and the backoff stays in memory.

use crate::app::AppState;
use crate::jobs::common::{id_of, unix_now, Quiet};
use crate::jobs::Stop;
use crate::services::plex_jobs::{path_key, PlexJobs};
use crate::{log_info, log_warning};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

pub const INTERVAL: Duration = Duration::from_secs(3 * 60);
pub const FIRST_RUN: Duration = Duration::from_secs(75);
/// `state.local.json` key: `{"ep:<sonarrEpisodeId>" | "movie:<radarrId>": {"tries", "next"}}`.
pub const STATE_KEY: &str = "subs_search";
pub const LANGUAGE: &str = "ro";
/// Waits after the 1st, 2nd and every later search of the same item.
pub const BACKOFF: [f64; 3] = [30.0 * 60.0, 2.0 * 3600.0, 6.0 * 3600.0];
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// Bazarr answers the PATCH when its search is over (several providers).
const SEARCH_TIMEOUT: Duration = Duration::from_secs(180);

/// The wait after the `tries`-th search: 30 min, 2 h, then 6 h.
pub fn backoff_seconds(tries: u32) -> f64 {
    BACKOFF[(tries.max(1) as usize - 1).min(BACKOFF.len() - 1)]
}

/// A wanted record misses a non-forced Romanian subtitle (`missing_subtitles[].code2`).
pub fn missing_ro(rec: &Value) -> bool {
    rec.get("missing_subtitles").and_then(Value::as_array).is_some_and(|a| {
        a.iter().any(|m| {
            m.get("code2").and_then(Value::as_str).is_some_and(|c| c.eq_ignore_ascii_case(LANGUAGE))
                && !m.get("forced").and_then(Value::as_bool).unwrap_or(false)
        })
    })
}

pub fn episode_key(id: i64) -> String {
    format!("ep:{id}")
}

pub fn movie_key(id: i64) -> String {
    format!("movie:{id}")
}

/// One backoff entry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Attempt {
    pub tries: u32,
    /// Unix time before which it is not searched again.
    pub next: f64,
}

pub fn parse_attempts(v: Option<&Value>) -> HashMap<String, Attempt> {
    v.and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, a)| {
                    Some((k.clone(), Attempt { tries: a.get("tries")?.as_u64()? as u32, next: a.get("next")?.as_f64()? }))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn attempts_json(a: &HashMap<String, Attempt>) -> Value {
    let m: Map<String, Value> = a.iter().map(|(k, x)| (k.clone(), json!({"tries": x.tries, "next": x.next}))).collect();
    Value::Object(m)
}

/// Searchable now: never tried, or its backoff is over.
pub fn eligible(a: &HashMap<String, Attempt>, key: &str, now: f64) -> bool {
    a.get(key).is_none_or(|x| x.next <= now)
}

/// An episode (or movie) to search.
#[derive(Debug, Clone, PartialEq)]
pub struct Cand {
    pub key: String,
    pub series_id: i64,
    pub episode_id: i64,
    /// Radarr id (movies only, else 0).
    pub movie_id: i64,
    pub title: String,
    pub season: i64,
    pub episode: i64,
}

impl Cand {
    /// `Show S01E05` / the movie title, for the log.
    pub fn name(&self) -> String {
        if self.movie_id != 0 {
            self.title.clone()
        } else {
            format!("{} S{:02}E{:02}", self.title, self.season, self.episode)
        }
    }

    pub fn search_path(&self) -> String {
        if self.movie_id != 0 {
            format!("/api/movies/subtitles?radarrid={}&language={LANGUAGE}&hi=False&forced=False", self.movie_id)
        } else {
            format!("/api/episodes/subtitles?seriesid={}&episodeid={}&language={LANGUAGE}&hi=False&forced=False", self.series_id, self.episode_id)
        }
    }
}

/// `1x02` -> (1, 2) (Bazarr's wanted `episode_number`).
fn season_episode(s: &str) -> Option<(i64, i64)> {
    let (a, b) = s.trim().split_once(['x', 'X'])?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

fn num(v: &Value, k: &str) -> Option<i64> {
    v.get(k).and_then(|x| x.as_i64().or_else(|| x.as_str().and_then(|s| s.trim().parse().ok())))
}

/// One series: its id, title and every episode Bazarr knows (`season`, `episode`, `path`,
/// `sonarrEpisodeId`; a wanted record with `episode_number` and no path works too).
pub struct SeriesEps {
    pub id: i64,
    pub title: String,
    pub episodes: Vec<Value>,
}

/// Season, episode, Sonarr episode id, path key.
type Ep = (i64, i64, i64, String);

/// The episodes to search, best first (see the module docs): watched ones (Plex
/// `viewCount > 0`) are left out; `wanted` holds the keys Bazarr lists as missing RO.
pub fn order(series: &[SeriesEps], wanted: &HashSet<String>, views: &HashMap<String, (i64, i64)>) -> Vec<Cand> {
    let mut ranked: Vec<(usize, i64, Cand)> = Vec::new();
    for s in series {
        let mut eps: Vec<Ep> = s
            .episodes
            .iter()
            .filter_map(|e| {
                let id = id_of(e.get("sonarrEpisodeId"))?;
                let (season, number) = match (num(e, "season"), num(e, "episode")) {
                    (Some(a), Some(b)) => (a, b),
                    _ => season_episode(e.get("episode_number").and_then(Value::as_str).unwrap_or(""))?,
                };
                Some((season, number, id, path_key(e.get("path").and_then(Value::as_str).unwrap_or(""))))
            })
            .collect();
        eps.sort_by_key(|(season, number, id, _)| (*season == 0, *season, *number, *id));
        let view = |p: &str| if p.is_empty() { (0, 0) } else { views.get(p).copied().unwrap_or((0, 0)) };
        let last_seen = eps.iter().map(|e| view(&e.3).1).max().unwrap_or(0);
        // Specials (season 0) never set where you are; they come after everything else.
        let last_watched = eps.iter().rposition(|e| e.0 != 0 && view(&e.3).0 > 0);
        let todo: Vec<(usize, &Ep)> =
            eps.iter().enumerate().filter(|(_, e)| wanted.contains(&episode_key(e.2)) && view(&e.3).0 == 0).collect();
        // After the last watched episode first (where you are), then the ones skipped before
        // it, then the specials.
        let regular = |e: &&(usize, &Ep)| e.1 .0 != 0;
        let after = todo.iter().filter(regular).filter(|(i, _)| last_watched.is_none_or(|w| *i > w));
        let before = todo.iter().filter(regular).filter(|(i, _)| last_watched.is_some_and(|w| *i < w));
        let specials = todo.iter().filter(|e| e.1 .0 == 0);
        for (pos, (_, e)) in after.chain(before).chain(specials).enumerate() {
            let c = Cand { key: episode_key(e.2), series_id: s.id, episode_id: e.2, movie_id: 0, title: s.title.clone(), season: e.0, episode: e.1 };
            ranked.push((pos, last_seen, c));
        }
    }
    ranked.sort_by_key(|(pos, seen, c)| (*pos, std::cmp::Reverse(*seen), c.series_id, c.season == 0, c.season, c.episode));
    ranked.into_iter().map(|(_, _, c)| c).collect()
}

/// Wanted movies missing RO, newest Radarr id first.
pub fn order_movies(wanted: &[Value]) -> Vec<Cand> {
    let mut out: Vec<Cand> = wanted
        .iter()
        .filter(|m| missing_ro(m))
        .filter_map(|m| {
            let id = id_of(m.get("radarrId"))?;
            let title = m.get("title").and_then(Value::as_str).unwrap_or("").to_string();
            Some(Cand { key: movie_key(id), series_id: 0, episode_id: 0, movie_id: id, title, season: 0, episode: 0 })
        })
        .collect();
    out.sort_by_key(|c| std::cmp::Reverse(c.movie_id));
    out
}

/// A wanted list to prune the backoff by: not empty (an empty answer may be Bazarr still
/// starting up) and as long as the `total` Bazarr reports.
pub fn complete_list(len: usize, total: Option<u64>) -> bool {
    len > 0 && total.is_none_or(|t| t as usize <= len)
}

/// Drop the `prefix` entries `still` says no longer apply; true if any went.
pub fn prune(a: &mut HashMap<String, Attempt>, prefix: &str, still: impl Fn(&str) -> bool) -> bool {
    let before = a.len();
    a.retain(|k, _| !k.starts_with(prefix) || still(k));
    a.len() != before
}

/// The job's memory between ticks.
#[derive(Default)]
pub struct Subs {
    pub attempts: HashMap<String, Attempt>,
    loaded: bool,
    quiet: Quiet,
}

fn section(app: &AppState) -> u32 {
    match crate::jobs::common::job_cfg(app, "subs", "section").and_then(|v| v.as_u64()) {
        Some(n) => n as u32,
        None => crate::jobs::convert::Settings::from_app(app).section,
    }
}

/// One tick: at most one search. Returns the key searched (or only logged in dry-run).
pub async fn tick(app: &AppState, s: &mut Subs, now: f64) -> Option<String> {
    if !s.loaded {
        match app.state_file.try_load() {
            Ok(m) => {
                s.attempts = parse_attempts(m.get(STATE_KEY));
                s.loaded = true;
                s.quiet.clear("state");
            }
            Err(t) => {
                s.quiet.warn("state", format!("subs: the state file is unreadable ({t}), nothing searched"));
                return None;
            }
        }
    }
    let bz = &app.services.bazarr;
    let (wanted, total) = match bz.data_total("/api/episodes/wanted?start=0&length=-1", LIST_TIMEOUT).await {
        Ok(w) => {
            if s.quiet.clear("bazarr") {
                log_info!("subs: Bazarr answers again");
            }
            w
        }
        Err(e) => {
            s.quiet.warn("bazarr", format!("subs: Bazarr wanted episodes failed ({e}), nothing searched"));
            return None;
        }
    };
    let complete = complete_list(wanted.len(), total);
    let wanted: Vec<&Value> = wanted.iter().filter(|w| missing_ro(w)).collect();
    let keys: HashSet<String> = wanted.iter().filter_map(|w| id_of(w.get("sonarrEpisodeId"))).map(episode_key).collect();
    let mut changed = complete && prune(&mut s.attempts, "ep:", |k| keys.contains(k));

    // The series that have an episode to search now, with the wanted records as a fallback.
    let open: HashSet<i64> = wanted
        .iter()
        .filter(|w| id_of(w.get("sonarrEpisodeId")).is_some_and(|e| eligible(&s.attempts, &episode_key(e), now)))
        .filter_map(|w| id_of(w.get("sonarrSeriesId")))
        .collect();
    let mut series: Vec<SeriesEps> = Vec::new();
    for w in &wanted {
        let Some(sid) = id_of(w.get("sonarrSeriesId")).filter(|id| open.contains(id)) else { continue };
        match series.iter_mut().find(|x| x.id == sid) {
            Some(x) => x.episodes.push((*w).clone()),
            None => {
                let title = w.get("seriesTitle").and_then(Value::as_str).unwrap_or("").to_string();
                series.push(SeriesEps { id: sid, title, episodes: vec![(*w).clone()] });
            }
        }
    }
    let mut chosen = None;
    if !series.is_empty() {
        let views = match PlexJobs::new(app.services.plex.c.clone()).view_info(section(app), true).await {
            Ok(v) => {
                s.quiet.clear("plex");
                v
            }
            Err(e) => {
                s.quiet.warn("plex", format!("subs: Plex failed ({}), plain episode order", e.type_name()));
                HashMap::new()
            }
        };
        for x in series.iter_mut() {
            // Every episode of the series (paths for "watched"); the wanted records if that fails.
            if let Ok(all) = bz.data(&format!("/api/episodes?seriesid%5B%5D={}", x.id), LIST_TIMEOUT).await {
                if !all.is_empty() {
                    x.episodes = all;
                }
            }
        }
        chosen = order(&series, &keys, &views).into_iter().find(|c| eligible(&s.attempts, &c.key, now));
    }
    if chosen.is_none() {
        match bz.data_total("/api/movies/wanted?start=0&length=-1", LIST_TIMEOUT).await {
            Ok((m, total)) => {
                let movies = order_movies(&m);
                let mkeys: HashSet<&str> = movies.iter().map(|c| c.key.as_str()).collect();
                if complete_list(m.len(), total) {
                    changed |= prune(&mut s.attempts, "movie:", |k| mkeys.contains(k));
                }
                chosen = movies.into_iter().find(|c| eligible(&s.attempts, &c.key, now));
            }
            Err(e) => s.quiet.warn("bazarr-movies", format!("subs: Bazarr wanted movies failed ({e})")),
        }
    }
    let Some(c) = chosen else {
        if changed {
            save(app, s);
        }
        return None;
    };
    let tries = s.attempts.get(&c.key).map_or(0, |a| a.tries) + 1;
    let name = c.name().chars().map(|ch| if ch.is_ascii() { ch } else { '?' }).collect::<String>();
    if app.dry_run {
        log_info!("subs (dry-run): would search RO subtitles for {name} (try {tries})");
    } else {
        log_info!("subs: searching RO subtitles for {name} (try {tries})");
        match bz.patch(&c.search_path(), SEARCH_TIMEOUT).await {
            Ok(()) => log_info!("subs: search for {name} done"),
            Err(e) => log_warning!("subs: search for {name} failed: {e}"),
        }
    }
    s.attempts.insert(c.key.clone(), Attempt { tries, next: now + backoff_seconds(tries) });
    save(app, s);
    Some(c.key)
}

/// The backoff to the state file (not in dry-run: memory only).
fn save(app: &AppState, s: &Subs) {
    if app.dry_run {
        return;
    }
    let mut m = Map::new();
    m.insert(STATE_KEY.into(), attempts_json(&s.attempts));
    app.state_file.update(m);
}

/// Start the loop. A panic in a tick is logged and the loop goes on.
pub fn spawn(app: Arc<AppState>, mut stop: Stop) {
    tokio::spawn(async move {
        if stop.sleep(FIRST_RUN).await {
            return;
        }
        let st = Arc::new(tokio::sync::Mutex::new(Subs::default()));
        loop {
            let (a, st2) = (app.clone(), st.clone());
            let r = tokio::spawn(async move {
                let mut g = st2.lock().await;
                tick(&a, &mut g, unix_now()).await
            })
            .await;
            if r.is_err() {
                log_warning!("subs: tick failed (panic)");
            }
            if stop.sleep(INTERVAL).await {
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::testutil::{app, fake, ok, status, Fake, Req};
    use std::sync::Mutex;

    fn ro() -> Value {
        json!([{"name": "Romanian", "code2": "ro", "code3": "ron", "forced": false, "hi": false}])
    }

    #[test]
    fn backoff_and_missing() {
        assert_eq!(backoff_seconds(0), 1800.0);
        assert_eq!(backoff_seconds(1), 1800.0);
        assert_eq!(backoff_seconds(2), 7200.0);
        assert_eq!(backoff_seconds(3), 21600.0);
        assert_eq!(backoff_seconds(9), 21600.0);
        assert!(missing_ro(&json!({"missing_subtitles": ro()})));
        assert!(!missing_ro(&json!({"missing_subtitles": [{"code2": "en", "forced": false}]})));
        assert!(!missing_ro(&json!({"missing_subtitles": [{"code2": "ro", "forced": true}]})));
        assert!(!missing_ro(&json!({})));
        let mut a = HashMap::new();
        a.insert("ep:1".to_string(), Attempt { tries: 1, next: 100.0 });
        assert!(!eligible(&a, "ep:1", 99.0) && eligible(&a, "ep:1", 100.0) && eligible(&a, "ep:2", 0.0));
        assert_eq!(parse_attempts(Some(&attempts_json(&a))), a);
        assert_eq!(season_episode("1x02"), Some((1, 2)));
        assert_eq!(season_episode("nope"), None);
    }

    fn e(id: i64, season: i64, n: i64, path: &str) -> Value {
        json!({"sonarrEpisodeId": id, "season": season, "episode": n, "path": path, "missing_subtitles": ro()})
    }

    #[test]
    fn next_episode_first_most_recent_series_first() {
        // A: E01-E02 watched (E01 an hour ago, E02 just now), E03-E04 and a special missing.
        // B: never watched, E01-E02 missing. C: watched yesterday, E02 missing, E01 skipped.
        let a = SeriesEps { id: 1, title: "A".into(), episodes: vec![
            e(11, 1, 1, "D:\\A\\1.mkv"), e(12, 1, 2, "D:\\A\\2.mkv"), e(14, 1, 4, "D:\\A\\4.mkv"), e(13, 1, 3, "D:\\A\\3.mkv"), e(10, 0, 1, "D:\\A\\0.mkv")] };
        let b = SeriesEps { id: 2, title: "B".into(), episodes: vec![e(21, 1, 1, "D:\\B\\1.mkv"), e(22, 1, 2, "D:\\B\\2.mkv")] };
        let c = SeriesEps { id: 3, title: "C".into(), episodes: vec![e(31, 1, 1, "D:\\C\\1.mkv"), e(32, 1, 2, "D:\\C\\2.mkv"), e(33, 1, 3, "D:\\C\\3.mkv")] };
        let mut views = HashMap::new();
        views.insert(path_key("D:\\A\\1.mkv"), (1, 1000));
        views.insert(path_key("D:\\A\\2.mkv"), (1, 5000));
        views.insert(path_key("d:/c/2.MKV"), (2, 3000));
        let wanted: HashSet<String> = [11, 13, 14, 10, 21, 22, 31, 33].into_iter().map(episode_key).collect();
        let got: Vec<String> = order(&[b, c, a], &wanted, &views).iter().map(Cand::name).collect();
        assert_eq!(got, ["A S01E03", "C S01E03", "B S01E01", "A S01E04", "C S01E01", "B S01E02", "A S00E01"]);
        // The wanted record alone (no path, `episode_number`) still sorts.
        let w = SeriesEps { id: 9, title: "W".into(), episodes: vec![
            json!({"sonarrEpisodeId": 92, "episode_number": "1x02"}), json!({"sonarrEpisodeId": 91, "episode_number": "1x01"})] };
        let wanted: HashSet<String> = [91, 92].into_iter().map(episode_key).collect();
        let got: Vec<String> = order(&[w], &wanted, &HashMap::new()).iter().map(Cand::name).collect();
        assert_eq!(got, ["W S01E01", "W S01E02"]);
        assert_eq!(
            order_movies(&[json!({"radarrId": 3, "title": "Old", "missing_subtitles": ro()}), json!({"radarrId": 7, "title": "New", "missing_subtitles": ro()}),
                           json!({"radarrId": 9, "title": "En", "missing_subtitles": [{"code2": "en"}]})])
                .iter().map(|c| c.title.as_str()).collect::<Vec<_>>(),
            ["New", "Old"]
        );
    }

    #[test]
    fn specials_never_set_the_cursor() {
        // A watched special must not push S01E03/E04 behind as "skipped before it".
        let a = SeriesEps { id: 1, title: "A".into(), episodes: vec![
            e(11, 1, 1, "D:\\A\\1.mkv"), e(12, 1, 2, "D:\\A\\2.mkv"), e(13, 1, 3, "D:\\A\\3.mkv"), e(10, 0, 1, "D:\\A\\0.mkv"), e(9, 0, 2, "D:\\A\\00.mkv")] };
        let mut views = HashMap::new();
        views.insert(path_key("D:\\A\\1.mkv"), (1, 100));
        views.insert(path_key("D:\\A\\0.mkv"), (1, 200));
        let wanted: HashSet<String> = [12, 13, 9].into_iter().map(episode_key).collect();
        let got: Vec<String> = order(&[a], &wanted, &views).iter().map(Cand::name).collect();
        assert_eq!(got, ["A S01E02", "A S01E03", "A S00E02"]);
    }

    #[test]
    fn only_complete_lists_prune() {
        assert!(complete_list(3, Some(3)) && complete_list(3, None));
        assert!(!complete_list(0, Some(0)) && !complete_list(0, None), "empty: maybe Bazarr is starting");
        assert!(!complete_list(2, Some(5)), "partial");
        let mut a: HashMap<String, Attempt> =
            [("ep:1", 1), ("ep:2", 1), ("movie:3", 1)].into_iter().map(|(k, t)| (k.to_string(), Attempt { tries: t, next: 0.0 })).collect();
        assert!(prune(&mut a, "ep:", |k| k == "ep:2"));
        assert!(!a.contains_key("ep:1") && a.contains_key("ep:2") && a.contains_key("movie:3"));
        assert!(!prune(&mut a, "ep:", |_| true));
    }

    struct World {
        wanted: Mutex<Vec<i64>>,
        movies: Mutex<Value>,
        bazarr_up: Mutex<bool>,
    }

    async fn serve(w: Arc<World>) -> Fake {
        fake(move |r: &Req| {
            let p = r.path.as_str();
            if p.starts_with("/api/") && !*w.bazarr_up.lock().unwrap() {
                return status(500);
            }
            let path = |n: i64| format!("D:\\Seriale\\Show\\S01E0{n}.mkv");
            if p.starts_with("/api/episodes/wanted") {
                let recs: Vec<Value> = w.wanted.lock().unwrap().iter()
                    .map(|n| json!({"seriesTitle": "Show", "episode_number": format!("1x0{n}"), "sonarrSeriesId": 4, "sonarrEpisodeId": 100 + n, "missing_subtitles": ro()}))
                    .collect();
                return ok(json!({"data": recs, "total": recs.len()}));
            }
            if p.starts_with("/api/episodes?seriesid%5B%5D=4") {
                return ok(json!({"data": (1..=5).map(|n| e(100 + n, 1, n, &path(n))).collect::<Vec<_>>()}));
            }
            if p.starts_with("/api/movies/wanted") {
                return ok(json!({"data": w.movies.lock().unwrap().clone()}));
            }
            if p.starts_with("/api/episodes/subtitles") || p.starts_with("/api/movies/subtitles") {
                return status(204);
            }
            if p.starts_with("/library/sections/2/all") {
                return ok(json!({"MediaContainer": {"Metadata": [{"viewCount": 1, "lastViewedAt": 10, "Media": [{"Part": [{"file": path(1)}]}]}]}}));
            }
            status(404)
        })
        .await
    }

    fn world() -> Arc<World> {
        Arc::new(World { wanted: Mutex::new(vec![1, 2, 3, 4]), movies: Mutex::new(json!([])), bazarr_up: Mutex::new(true) })
    }

    fn patches(f: &Fake) -> Vec<String> {
        f.find("PATCH", "/api/").into_iter().map(|r| r.path).collect()
    }

    #[tokio::test]
    async fn one_search_per_tick_with_backoff() {
        let w = world();
        let f = serve(w.clone()).await;
        let (a, _d) = app(&f.base, false, json!({}));
        let mut s = Subs::default();
        let t0 = 1_000_000.0;
        // E1 is watched (and missing): the next one is E2.
        assert_eq!(tick(&a, &mut s, t0).await.as_deref(), Some("ep:102"));
        assert_eq!(patches(&f), ["/api/episodes/subtitles?seriesid=4&episodeid=102&language=ro&hi=False&forced=False"]);
        assert_eq!(f.find("PATCH", "/")[0].header("x-api-key"), Some("b"));
        // Still missing: E2 waits 30 min, E3 goes next.
        assert_eq!(tick(&a, &mut s, t0 + 180.0).await.as_deref(), Some("ep:103"));
        // E2 got its subtitle: its entry is dropped.
        w.wanted.lock().unwrap().retain(|n| *n != 2);
        assert_eq!(tick(&a, &mut s, t0 + 360.0).await.as_deref(), Some("ep:104"));
        assert!(!s.attempts.contains_key("ep:102"));
        // Everything in backoff and no movies: nothing.
        assert_eq!(tick(&a, &mut s, t0 + 540.0).await, None);
        // After 30 min E3 again (2nd try, then 2 h).
        assert_eq!(tick(&a, &mut s, t0 + 180.0 + 1800.0).await.as_deref(), Some("ep:103"));
        assert_eq!(s.attempts["ep:103"], Attempt { tries: 2, next: t0 + 1980.0 + 7200.0 });
        // Persisted: a new job instance reads the backoff back.
        let saved = parse_attempts(a.state_file.load().get(STATE_KEY));
        assert_eq!(saved, s.attempts);
        let mut s2 = Subs::default();
        assert_eq!(tick(&a, &mut s2, t0 + 2100.0).await, None);
        // Movies once no episode is due.
        *w.movies.lock().unwrap() = json!([{"radarrId": 7, "title": "Film", "missing_subtitles": ro()}]);
        assert_eq!(tick(&a, &mut s2, t0 + 2150.0).await.as_deref(), Some("movie:7"));
        assert_eq!(patches(&f).last().unwrap(), "/api/movies/subtitles?radarrid=7&language=ro&hi=False&forced=False");
        assert_eq!(patches(&f).len(), 5);
    }

    #[tokio::test]
    async fn dry_run_and_bazarr_down() {
        let w = world();
        let f = serve(w.clone()).await;
        let (a, _d) = app(&f.base, true, json!({}));
        let mut s = Subs::default();
        assert_eq!(tick(&a, &mut s, 1000.0).await.as_deref(), Some("ep:102"));
        assert!(patches(&f).is_empty(), "dry-run: no search");
        assert!(a.state_file.load().get(STATE_KEY).is_none(), "dry-run: backoff in memory only");
        assert_eq!(tick(&a, &mut s, 1100.0).await.as_deref(), Some("ep:103"));
        // An empty wanted list (Bazarr starting up) prunes nothing either.
        let saved = std::mem::take(&mut *w.wanted.lock().unwrap());
        assert_eq!(tick(&a, &mut s, 1150.0).await, None);
        assert_eq!(s.attempts.len(), 2, "an empty list prunes nothing");
        *w.wanted.lock().unwrap() = saved;
        *w.bazarr_up.lock().unwrap() = false;
        assert_eq!(tick(&a, &mut s, 1200.0).await, None);
        assert_eq!(s.attempts.len(), 2, "a failed fetch prunes nothing");
    }
}
