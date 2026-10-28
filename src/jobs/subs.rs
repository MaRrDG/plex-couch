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
//!
//! An item still missing `ro` after a search may then be translated from English by Bazarr's
//! Gemini translator, one at a time: see [`crate::jobs::subs_translate`] (state under
//! `subs_translate`).

use crate::app::AppState;
use crate::jobs::common::{id_of, unix_now, Quiet};
use crate::jobs::subs_translate::{self as translate, bazarr_gate, local_gate, order_movie_cands, pick_kind, SubTools, TrSettings, TrState, TR_KEY};
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
    /// The translation fallback ([`crate::jobs::subs_translate`]).
    pub tr: TrState,
    /// Tests: fake ffprobe/ffmpeg (else the installed ffmpeg, looked up when needed).
    pub tools: Option<Arc<dyn SubTools>>,
    loaded: bool,
    quiet: Quiet,
}

const MOVIES_WANTED: &str = "/api/movies/wanted?start=0&length=-1";
/// At most this many movie candidates are looked up per tick (newest first).
const MOVIE_LOOKUPS: usize = 50;

fn section(app: &AppState) -> u32 {
    match crate::jobs::common::job_cfg(app, "subs", "section").and_then(|v| v.as_u64()) {
        Some(n) => n as u32,
        None => crate::jobs::convert::Settings::from_app(app).section,
    }
}

/// One tick: at most one search and at most one translation started. Returns the key
/// searched (or only logged in dry-run).
pub async fn tick(app: &AppState, s: &mut Subs, now: f64) -> Option<String> {
    let today = crate::log::asctime().get(..10).unwrap_or("").to_string();
    tick_on(app, s, now, &today).await
}

/// [`tick`] on the local day `today` (`YYYY-MM-DD`, for the daily translation cap).
pub async fn tick_on(app: &AppState, s: &mut Subs, now: f64, today: &str) -> Option<String> {
    if !s.loaded {
        match app.state_file.try_load() {
            Ok(m) => {
                s.attempts = parse_attempts(m.get(STATE_KEY));
                s.tr = TrState::from_json(m.get(TR_KEY));
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
    if complete {
        changed |= s.tr.prune("ep:", |k| keys.contains(k));
    }
    let tcfg = TrSettings::from_app(app);
    if s.tr.inflight.is_some() {
        changed |= translate::poll(app, &mut s.tr, &tcfg, section(app), now).await;
    }

    // May a translation start this tick? Local checks first (off, in flight, daily cap), then
    // is anything due (searched at least once), then Bazarr's side (translator, its jobs).
    let tr_local = local_gate(&s.tr, &tcfg, today).is_ok();
    let mut movies_wanted = if tr_local { Some(bz.data_total(MOVIES_WANTED, LIST_TIMEOUT).await) } else { None };
    let tr_due = |k: &str| s.attempts.contains_key(k) && s.tr.eligible(k, now);
    let tr_series: HashSet<i64> = if tr_local {
        wanted
            .iter()
            .filter(|w| id_of(w.get("sonarrEpisodeId")).is_some_and(|e| tr_due(&episode_key(e))))
            .filter_map(|w| id_of(w.get("sonarrSeriesId")))
            .collect()
    } else {
        HashSet::new()
    };
    let tr_movies = matches!(&movies_wanted, Some(Ok((m, _))) if order_movies(m).iter().any(|c| tr_due(&c.key)));
    let tr_go = (!tr_series.is_empty() || tr_movies) && bazarr_gate(app, &mut s.quiet).await;

    // The series that have an episode to search (or translate) now, with the wanted records
    // as a fallback.
    let mut open: HashSet<i64> = wanted
        .iter()
        .filter(|w| id_of(w.get("sonarrEpisodeId")).is_some_and(|e| eligible(&s.attempts, &episode_key(e), now)))
        .filter_map(|w| id_of(w.get("sonarrSeriesId")))
        .collect();
    if tr_go {
        open.extend(tr_series);
    }
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
    let mut ranked: Vec<Cand> = Vec::new();
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
        ranked = order(&series, &keys, &views);
    }
    // Episode key -> video path (Bazarr's full episode records).
    let ep_paths: HashMap<String, String> = series
        .iter()
        .flat_map(|x| x.episodes.iter())
        .filter_map(|e| Some((episode_key(id_of(e.get("sonarrEpisodeId"))?), e.get("path")?.as_str()?.to_string())))
        .filter(|(_, p)| !p.is_empty())
        .collect();
    let mut chosen = ranked.iter().find(|c| eligible(&s.attempts, &c.key, now)).cloned();
    let mut movies: Vec<Cand> = Vec::new();
    if chosen.is_none() || tr_go {
        let fetched = match movies_wanted.take() {
            Some(r) => r,
            None => bz.data_total(MOVIES_WANTED, LIST_TIMEOUT).await,
        };
        match fetched {
            Ok((m, total)) => {
                movies = order_movies(&m);
                let mkeys: HashSet<&str> = movies.iter().map(|c| c.key.as_str()).collect();
                if complete_list(m.len(), total) {
                    changed |= prune(&mut s.attempts, "movie:", |k| mkeys.contains(k));
                    changed |= s.tr.prune("movie:", |k| mkeys.contains(k));
                }
                if chosen.is_none() {
                    chosen = movies.iter().find(|c| eligible(&s.attempts, &c.key, now)).cloned();
                }
                s.quiet.clear("bazarr-movies");
            }
            Err(e) => s.quiet.warn("bazarr-movies", format!("subs: Bazarr wanted movies failed ({e})")),
        }
    }

    // The search.
    let searched = chosen.as_ref().map(|c| c.key.clone());
    if let Some(c) = &chosen {
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
        changed = true;
    }

    // The translation: an episode (search order) or a movie (unwatched, newest first), in
    // turns; never the item searched just now.
    if tr_go {
        let skip = searched.as_deref();
        let (ep, movie_cands) = {
            let was_searched = |k: &str| s.attempts.contains_key(k);
            let ep = translate::candidates(&ranked, &was_searched, &s.tr, now, skip).into_iter().find(|c| ep_paths.contains_key(&c.key));
            let mut mc = translate::candidates(&movies, &was_searched, &s.tr, now, skip);
            mc.truncate(MOVIE_LOOKUPS);
            (ep, mc)
        };
        let mut movie_paths: HashMap<i64, String> = HashMap::new();
        let mut movie = None;
        if !movie_cands.is_empty() {
            let q = movie_cands.iter().map(|c| format!("radarrid%5B%5D={}", c.movie_id)).collect::<Vec<_>>().join("&");
            match bz.data(&format!("/api/movies?{q}"), LIST_TIMEOUT).await {
                Ok(recs) => {
                    movie_paths = recs
                        .iter()
                        .filter_map(|m| Some((id_of(m.get("radarrId"))?, m.get("path")?.as_str()?.to_string())))
                        .collect();
                    let views = match PlexJobs::new(app.services.plex.c.clone()).view_counts(tcfg.movie_section, false).await {
                        Ok(v) => {
                            s.quiet.clear("plex-movies");
                            v
                        }
                        Err(e) => {
                            s.quiet.warn("plex-movies", format!("subs: Plex movies failed ({}), every movie counts as unwatched", e.type_name()));
                            HashMap::new()
                        }
                    };
                    movie = order_movie_cands(&movie_cands, &movie_paths, &views).into_iter().next();
                }
                Err(e) => s.quiet.warn("bazarr-movie", format!("subs: Bazarr movie details failed ({e})")),
            }
        }
        if let Some(c) = pick_kind(ep, movie, &s.tr.last) {
            let video = if c.movie_id != 0 { movie_paths.get(&c.movie_id) } else { ep_paths.get(&c.key) }.cloned().unwrap_or_default();
            let tools = s.tools.clone().or_else(|| translate::real_tools(app));
            changed |= translate::start(app, &mut s.tr, &mut s.quiet, tools, &c, &video, translate::When { now, today }).await;
        }
    }
    if changed {
        save(app, s);
    }
    searched
}

/// The backoff and the translation state to the state file (not in dry-run: memory only).
fn save(app: &AppState, s: &Subs) {
    if app.dry_run {
        return;
    }
    let mut m = Map::new();
    m.insert(STATE_KEY.into(), attempts_json(&s.attempts));
    m.insert(TR_KEY.into(), s.tr.to_json());
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

    // -- the translation fallback ----------------------------------------------------------

    use crate::jobs::subs_translate::{InFlight, TrItem};
    use crate::services::arr_jobs::urlencode;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TrWorld {
        root: PathBuf,
        wanted: Mutex<Vec<i64>>,
        /// Radarr ids missing RO; 9 is watched in Plex.
        movies: Mutex<Vec<i64>>,
        settings: Mutex<Value>,
        jobs: Mutex<Value>,
    }

    impl TrWorld {
        fn ep(&self, n: i64) -> PathBuf {
            self.root.join("Show").join(format!("S01E0{n}.mkv"))
        }

        fn movie(&self, id: i64) -> PathBuf {
            self.root.join("Movies").join(format!("Film{id}.mkv"))
        }

        fn job(&self, status: &str, en: &Path) {
            *self.jobs.lock().unwrap() = json!([{"job_name": "Translating from EN to RO using Gemini", "status": status,
                                                  "progress_value": 1, "progress_max": 1, "progress_message": en.display().to_string()}]);
        }
    }

    fn gemini() -> Value {
        json!({"general": {}, "translator": {"translator_type": "gemini", "gemini_keys": ["secret"], "gemini_model": "m"}})
    }

    fn tr_world() -> Arc<TrWorld> {
        let root = crate::jobs::testutil::temp_dir("subs-tr");
        let w = TrWorld { root, wanted: Mutex::new(vec![2, 3, 4]), movies: Mutex::new(vec![]), settings: Mutex::new(gemini()), jobs: Mutex::new(json!([])) };
        fs::create_dir_all(w.root.join("Show")).unwrap();
        fs::create_dir_all(w.root.join("Movies")).unwrap();
        for n in 1..=5 {
            fs::write(w.ep(n), b"video").unwrap();
        }
        for id in [3, 7, 9] {
            fs::write(w.movie(id), b"video").unwrap();
        }
        Arc::new(w)
    }

    async fn tr_serve(w: Arc<TrWorld>) -> Fake {
        fake(move |r: &Req| {
            let p = r.path.as_str();
            if r.method == "PATCH" && p.starts_with("/api/") {
                return status(204);
            }
            if r.method == "POST" && p == "/" {
                return ok(json!({})); // ntfy
            }
            if p.starts_with("/api/episodes/wanted") {
                let recs: Vec<Value> = w.wanted.lock().unwrap().iter()
                    .map(|n| json!({"seriesTitle": "Show", "episode_number": format!("1x0{n}"), "sonarrSeriesId": 4, "sonarrEpisodeId": 100 + n, "missing_subtitles": ro()}))
                    .collect();
                return ok(json!({"data": recs, "total": recs.len()}));
            }
            if p.starts_with("/api/episodes?seriesid%5B%5D=4") {
                return ok(json!({"data": (1..=5).map(|n| e(100 + n, 1, n, &w.ep(n).display().to_string())).collect::<Vec<_>>()}));
            }
            if p.starts_with("/api/movies/wanted") {
                let recs: Vec<Value> = w.movies.lock().unwrap().iter().map(|id| json!({"radarrId": id, "title": format!("Film {id}"), "missing_subtitles": ro()})).collect();
                return ok(json!({"data": recs, "total": recs.len()}));
            }
            if p.starts_with("/api/movies?radarrid%5B%5D=") {
                let recs: Vec<Value> = [3, 7, 9].iter().filter(|id| p.contains(&format!("radarrid%5B%5D={id}")))
                    .map(|id| json!({"radarrId": id, "title": format!("Film {id}"), "path": w.movie(*id).display().to_string()})).collect();
                return ok(json!({"data": recs}));
            }
            if p.starts_with("/api/system/settings") {
                return ok(w.settings.lock().unwrap().clone());
            }
            if p.starts_with("/api/system/jobs") {
                return ok(json!({"data": w.jobs.lock().unwrap().clone()}));
            }
            if p.starts_with("/library/sections/2/all") {
                return ok(json!({"MediaContainer": {"Metadata": [{"viewCount": 1, "lastViewedAt": 10, "Media": [{"Part": [{"file": w.ep(1).display().to_string()}]}]}]}}));
            }
            if p.starts_with("/library/sections/1/all") {
                return ok(json!({"MediaContainer": {"Metadata": [{"viewCount": 1, "Media": [{"Part": [{"file": w.movie(9).display().to_string()}]}]}]}}));
            }
            if p.contains("/refresh?path=") {
                return ok(json!({}));
            }
            status(404)
        })
        .await
    }

    /// An English ASS track (index 2) with 60 dialogue lines and some karaoke.
    #[derive(Default)]
    struct FakeTools {
        probes: AtomicUsize,
        extracts: AtomicUsize,
    }

    impl SubTools for FakeTools {
        fn probe(&self, _file: &Path) -> Result<Value, String> {
            self.probes.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"streams": [{"index": 0, "codec_type": "video", "codec_name": "hevc"},
                                  {"index": 2, "codec_type": "subtitle", "codec_name": "ass", "tags": {"language": "eng"}}]}))
        }

        fn extract(&self, _file: &Path, index: i64, ass: bool) -> Result<String, String> {
            self.extracts.fetch_add(1, Ordering::SeqCst);
            assert_eq!((index, ass), (2, true));
            let mut s = String::from("[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n");
            for i in 0..60 {
                s.push_str(&format!("Dialogue: 0,0:01:{i:02}.50,0:01:{i:02}.90,Default,,0,0,0,,Line {i}\n"));
                s.push_str(&format!("Dialogue: 0,0:00:{i:02}.00,0:00:{i:02}.50,OP1 - ENG A,,0,0,0,,{{\\k20}}Sing {i}\n"));
            }
            Ok(s)
        }
    }

    fn translations(f: &Fake) -> Vec<String> {
        patches(f).into_iter().filter(|p| p.starts_with("/api/subtitles?action=translate")).collect()
    }

    fn translate_path(kind: &str, id: i64, en: &Path) -> String {
        format!("/api/subtitles?action=translate&language=ro&path={}&type={kind}&id={id}", urlencode(&en.display().to_string()))
    }

    const DAY: &str = "2026-10-28";
    const HEADER_SRT: &str = "1\r\n00:00:01,000 --> 00:00:05,000\r\n# Subtitles translated with gemini-3.5-flash-lite #\r\n\r\n\
                              2\r\n00:00:06,000 --> 00:00:07,000\r\nSalut\r\n\r\n3\r\n00:00:08,000 --> 00:00:09,000\r\nPa\r\n\r\n";

    fn searched_once(s: &mut Subs, keys: &[&str], next: f64) {
        for k in keys {
            s.attempts.insert(k.to_string(), Attempt { tries: 1, next });
        }
    }

    #[tokio::test]
    async fn translates_after_a_search_one_at_a_time() {
        let w = tr_world();
        let f = tr_serve(w.clone()).await;
        let (a, _d) = app(&f.base, false, json!({"jobs": {"subs": {"translate_per_day": 2}}}));
        let tools = Arc::new(FakeTools::default());
        let mut s = Subs { tools: Some(tools.clone()), ..Default::default() };
        let t0 = 1_000_000.0;
        // E2 was searched before (still missing, in its backoff); E3 and E4 never were. E3 has
        // its own English file.
        assert_eq!(tick_on(&a, &mut s, t0 - 10.0, DAY).await.as_deref(), Some("ep:102"));
        assert!(translations(&f).is_empty(), "never translated before a search");
        fs::write(w.root.join("Show").join("S01E03.en.srt"), "1\n00:00:01,000 --> 00:00:02,000\nHi\n\n").unwrap();

        // E3 is searched now (so not translated); E2 is translated from its embedded track.
        assert_eq!(tick_on(&a, &mut s, t0, DAY).await.as_deref(), Some("ep:103"));
        let en2 = w.root.join("Show").join("S01E02.en.srt");
        assert_eq!(translations(&f), [translate_path("episode", 102, &en2)]);
        assert!(patches(&f).contains(&"/api/series?seriesid=4&action=scan-disk".to_string()));
        let pos = |needle: &str| patches(&f).iter().position(|p| p.starts_with(needle)).unwrap();
        assert!(pos("/api/series?seriesid=4&action=scan-disk") < pos("/api/subtitles?action=translate"), "index first");
        let srt = fs::read_to_string(&en2).unwrap();
        assert!(srt.starts_with("1\n00:01:00,500 --> 00:01:00,900\nLine 0\n\n") && !srt.contains("Sing"), "{srt}");
        assert_eq!(srt.matches(" --> ").count(), 60);
        assert_eq!(fs::read(w.ep(2)).unwrap(), b"video", "the video is untouched");
        assert_eq!(s.tr.inflight.as_ref().map(|x| x.key.as_str()), Some("ep:102"));
        let ro2 = w.root.join("Show").join("S01E02.ro.srt");
        fs::write(&ro2, b"").unwrap(); // Bazarr's empty placeholder

        // Running: nothing new starts although E3 is due now.
        w.job("running", &en2);
        tick_on(&a, &mut s, t0 + 180.0, DAY).await;
        assert_eq!(translations(&f).len(), 1);

        // Done: the header cue goes, Bazarr rescans, Plex refreshes, ntfy. Then E3 starts,
        // from its external file (no ffmpeg).
        w.job("completed", &en2);
        fs::write(&ro2, HEADER_SRT).unwrap();
        tick_on(&a, &mut s, t0 + 360.0, DAY).await;
        assert_eq!(fs::read_to_string(&ro2).unwrap(), "1\r\n00:00:06,000 --> 00:00:07,000\r\nSalut\r\n\r\n2\r\n00:00:08,000 --> 00:00:09,000\r\nPa\r\n\r\n");
        assert_eq!(s.tr.items["ep:102"].why, "done");
        assert_eq!(f.find("GET", "/library/sections/2/refresh").len(), 1);
        let ntfy: Vec<Value> = f.find("POST", "/").iter().filter(|q| q.path == "/").map(Req::json).collect();
        assert_eq!(ntfy.len(), 1);
        assert_eq!(ntfy[0]["message"], json!("Subtitrare RO tradusă: Show S01E02"));
        let en3 = w.root.join("Show").join("S01E03.en.srt");
        assert_eq!(translations(&f).last().unwrap(), &translate_path("episode", 103, &en3));
        assert_eq!((tools.probes.load(Ordering::SeqCst), tools.extracts.load(Ordering::SeqCst)), (1, 1), "external file: no ffmpeg");

        // E3 fails: Bazarr's empty file goes (it would count as a subtitle), 1 h backoff. The
        // daily cap (2) stops E4 today.
        let ro3 = w.root.join("Show").join("S01E03.ro.srt");
        fs::write(&ro3, b"").unwrap();
        w.job("failed", &en3);
        tick_on(&a, &mut s, t0 + 540.0, DAY).await;
        assert!(!ro3.exists());
        assert_eq!(s.tr.items["ep:103"], TrItem { tries: 1, next: t0 + 540.0 + 3600.0, why: "failed".into() });
        assert!(s.tr.inflight.is_none());
        assert_eq!(translations(&f).len(), 2, "daily cap");
        assert_eq!(s.tr.used_today(DAY), 2);
        // Persisted.
        assert_eq!(crate::jobs::subs_translate::TrState::from_json(a.state_file.load().get(TR_KEY)), s.tr);

        // The next day E4 (searched at t0 + 180) goes; E3 waits for its backoff.
        tick_on(&a, &mut s, t0 + 720.0, "2026-10-29").await;
        let en4 = w.root.join("Show").join("S01E04.en.srt");
        assert_eq!(translations(&f).last().unwrap(), &translate_path("episode", 104, &en4));
        // A timeout gives up too.
        tick_on(&a, &mut s, t0 + 720.0 + 16.0 * 60.0, "2026-10-29").await;
        assert_eq!(s.tr.items["ep:104"].why, "failed");
        // E2 got its subtitle in Bazarr's eyes: its entry is dropped.
        w.wanted.lock().unwrap().retain(|n| *n != 2);
        tick_on(&a, &mut s, t0 + 2000.0, "2026-10-29").await;
        assert!(!s.tr.items.contains_key("ep:102"));
        let _ = fs::remove_dir_all(&w.root);
    }

    #[tokio::test]
    async fn movies_take_turns_with_episodes() {
        let w = tr_world();
        *w.wanted.lock().unwrap() = vec![2];
        *w.movies.lock().unwrap() = vec![3, 7, 9];
        let f = tr_serve(w.clone()).await;
        let (a, _d) = app(&f.base, false, json!({}));
        let mut s = Subs { tools: Some(Arc::new(FakeTools::default())), ..Default::default() };
        let t0 = 1_000_000.0;
        s.loaded = true;
        searched_once(&mut s, &["ep:102", "movie:3", "movie:7", "movie:9"], t0 + 9999.0);
        s.tr.last = "ep".into();
        for id in [3, 7, 9] {
            fs::write(w.root.join("Movies").join(format!("Film{id}.en.srt")), "1\n00:00:01,000 --> 00:00:02,000\nHi\n\n").unwrap();
        }
        // A movie's turn: 9 is watched, so the newest unwatched one, 7.
        assert_eq!(tick_on(&a, &mut s, t0, DAY).await, None, "everything in its search backoff");
        let en7 = w.root.join("Movies").join("Film7.en.srt");
        assert_eq!(translations(&f), [translate_path("movie", 7, &en7)]);
        assert!(patches(&f).contains(&"/api/movies?radarrid=7&action=scan-disk".to_string()));
        // Done: Plex refreshes the movies section; then the episode's turn.
        w.job("completed", &en7);
        fs::write(w.root.join("Movies").join("Film7.ro.srt"), HEADER_SRT).unwrap();
        tick_on(&a, &mut s, t0 + 180.0, DAY).await;
        assert_eq!(f.find("GET", "/library/sections/1/refresh").len(), 1);
        let ntfy: Vec<Value> = f.find("POST", "/").iter().filter(|q| q.path == "/").map(Req::json).collect();
        assert_eq!(ntfy[0]["message"], json!("Subtitrare RO tradusă: Film 7"));
        let en2 = w.root.join("Show").join("S01E02.en.srt");
        assert_eq!(translations(&f).last().unwrap(), &translate_path("episode", 102, &en2));
        // Then a movie again (3; 7 is done, 9 watched).
        w.job("completed", &en2);
        fs::write(w.root.join("Show").join("S01E02.ro.srt"), HEADER_SRT).unwrap();
        tick_on(&a, &mut s, t0 + 360.0, DAY).await;
        let en3 = w.root.join("Movies").join("Film3.en.srt");
        assert_eq!(translations(&f).last().unwrap(), &translate_path("movie", 3, &en3));
        assert_eq!(translations(&f).len(), 3);
        // Movie 7 left the wanted list: its entry goes.
        *w.movies.lock().unwrap() = vec![3, 9];
        tick_on(&a, &mut s, t0 + 540.0, DAY).await;
        assert!(!s.tr.items.contains_key("movie:7") && s.tr.items.contains_key("ep:102"));
        let _ = fs::remove_dir_all(&w.root);
    }

    #[tokio::test]
    async fn no_translation_without_gemini_in_dry_run_or_over_a_subtitle() {
        let w = tr_world();
        *w.wanted.lock().unwrap() = vec![2];
        let f = tr_serve(w.clone()).await;
        let t0 = 1_000_000.0;
        let primed = |tools: &Arc<FakeTools>| {
            let mut s = Subs { tools: Some(tools.clone()), ..Default::default() };
            s.loaded = true;
            searched_once(&mut s, &["ep:102"], t0 + 9999.0);
            s
        };
        let tools = Arc::new(FakeTools::default());
        // Google Translate: skipped.
        *w.settings.lock().unwrap() = json!({"translator": {"translator_type": "google_translate", "gemini_keys": ["k"]}});
        let (a, _d) = app(&f.base, false, json!({}));
        let mut s = primed(&tools);
        tick_on(&a, &mut s, t0, DAY).await;
        tick_on(&a, &mut s, t0 + 180.0, DAY).await;
        assert!(f.mutations().is_empty() && s.tr.items.is_empty());
        assert_eq!(tools.probes.load(Ordering::SeqCst), 0);
        // Switched off: Bazarr's settings are not even read.
        *w.settings.lock().unwrap() = gemini();
        let before = f.find("GET", "/api/system/settings").len();
        let (a_off, _d2) = app(&f.base, false, json!({"jobs": {"subs": {"translate": false}}}));
        let mut s = primed(&tools);
        tick_on(&a_off, &mut s, t0, DAY).await;
        assert_eq!(f.find("GET", "/api/system/settings").len(), before);
        // Some other translation is running in Bazarr: wait.
        w.job("running", Path::new("D:\\x\\Other.en.srt"));
        let mut s = primed(&tools);
        tick_on(&a, &mut s, t0, DAY).await;
        assert!(f.mutations().is_empty());
        *w.jobs.lock().unwrap() = json!([]);
        // Dry-run: probed and logged, nothing written, no PATCH, the state file untouched.
        let (a_dry, _d3) = app(&f.base, true, json!({}));
        let mut s = primed(&tools);
        tick_on(&a_dry, &mut s, t0, DAY).await;
        assert!(f.mutations().is_empty());
        assert!(!w.root.join("Show").join("S01E02.en.srt").exists());
        assert!(a_dry.state_file.load().get(TR_KEY).is_none());
        assert_eq!((s.tr.used_today(DAY), tools.extracts.load(Ordering::SeqCst)), (1, 0));
        // A Romanian file already there (non-empty): never overwritten.
        let ro2 = w.root.join("Show").join("S01E02.ro.srt");
        fs::write(&ro2, "1\n00:00:01,000 --> 00:00:02,000\nDeja\n\n").unwrap();
        let mut s = primed(&tools);
        tick_on(&a, &mut s, t0, DAY).await;
        assert!(translations(&f).is_empty());
        assert_eq!(s.tr.items["ep:102"].why, "has_ro");
        assert_eq!(fs::read_to_string(&ro2).unwrap(), "1\n00:00:01,000 --> 00:00:02,000\nDeja\n\n");
        // No English source at all: 24 h.
        fs::remove_file(&ro2).unwrap();
        struct NoSubs;
        impl SubTools for NoSubs {
            fn probe(&self, _f: &Path) -> Result<Value, String> {
                Ok(json!({"streams": [{"index": 2, "codec_type": "subtitle", "codec_name": "ass", "tags": {"language": "eng", "title": "Signs & Songs"}}]}))
            }
            fn extract(&self, _f: &Path, _i: i64, _a: bool) -> Result<String, String> {
                unreachable!()
            }
        }
        let mut s = primed(&tools);
        s.tools = Some(Arc::new(NoSubs));
        tick_on(&a, &mut s, t0, DAY).await;
        assert_eq!(s.tr.items["ep:102"], TrItem { tries: 0, next: t0 + 86400.0, why: "no_source".into() });
        assert!(translations(&f).is_empty());
        // A restart with a translation in flight keeps following it.
        let mut st = Subs { tools: Some(tools.clone()), ..Default::default() };
        st.tr.inflight = Some(InFlight { key: "ep:102".into(), series_id: 4, episode_id: 102, movie_id: 0, name: "Show S01E02".into(),
                                         en: "x.en.srt".into(), ro: "x.ro.srt".into(), started: t0, tries: 0 });
        save(&a, &st);
        let mut s2 = Subs::default();
        tick_on(&a, &mut s2, t0 + 60.0, DAY).await;
        assert_eq!(s2.tr.inflight, st.tr.inflight);
        let _ = fs::remove_dir_all(&w.root);
    }
}
