//! Manual RO translations: the queue the user fills from Seriale and Filme, processed by the
//! subs job ([`crate::jobs::subs`]) one at a time, before anything else.
//!
//! - `POST /api/subs/translate` ([`api_translate`]): `{seriesId, episodeIds: [...]}`,
//!   `{seriesId, season, missingOnly: true}` (every episode of that season without RO) or
//!   `{movieId}`. Refused when the daily cap is reached (409, "Limita zilnică ..."), when
//!   Bazarr's translator is not Gemini with a key (409) or Bazarr does not answer (502).
//!   Items that have RO, are queued or translating already are skipped (dedupe by key).
//! - `POST /api/subs/translate/cancel` ([`api_cancel`]): the same bodies; removes queued
//!   items, never the one in flight.
//! - `GET /api/subs` ([`api_status`]): the translator (configured or not), the one in flight,
//!   the queue, today's count and the cap, the last results. `GET /api/series/<id>` and
//!   `GET /api/movies/<id>` carry the same object as `subs` and, per episode / movie,
//!   `ro`: `ok` | `missing` | `queued` (with `ro_pos`, 1-based) | `translating` | null, and
//!   `ro_note` (the last failed attempt) ([`merge_series`], [`merge_movie`]).
//!
//! The queue (FIFO) is persisted in `state.local.json` under `subs_translate.queue`, the
//! last results under `subs_translate.recent`, next to the job's own state ([`Manual`] holds
//! all of it and writes it as one key, so the page and the job never clobber each other). An
//! enqueue wakes the job at once ([`Manual::wake`]); while something is queued or in flight
//! the job looks every 15 s.
//!
//! Processing ([`step`]): the cap and Bazarr's gate as for the automatic translation, no
//! provider search needed. The video path comes from Bazarr (its episode / movie record);
//! a record with a Romanian subtitle, or a non-empty `.ro.srt` next to the video, ends the
//! item as "avea deja RO". A failure is reported (the page, ntfy) and the item leaves the
//! queue; there is no backoff, the user may ask again.
//!
//! Demo: a fixed queue (Frieren S01E07, S01E08) and S01E06 in flight, kept in memory; the
//! routes work on it, nothing is ever processed.

use crate::app::AppState;
use crate::jobs::common::{id_of, unix_now, Quiet};
use crate::jobs::subs::{episode_key, movie_key, Cand};
use crate::jobs::subs_translate::{
    bazarr_gate, local_today, manual_gate, prepare, translator_ready, InFlight, Prep, SubTools, TrSettings, TrState, When, TR_KEY,
};
use crate::log_info;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// At most this many episode ids in one request.
pub const MAX_IDS: usize = 100;
/// The last results kept (page, state file).
pub const RECENT: usize = 20;
/// How long Bazarr's translator answer is reused by the page (a failure: [`TRANSLATOR_RETRY`]).
pub const TRANSLATOR_TTL: Duration = Duration::from_secs(60);
pub const TRANSLATOR_RETRY: Duration = Duration::from_secs(15);
const BAZARR_TIMEOUT: Duration = Duration::from_secs(10);
/// Items that end at once (RO there already, a failure) handled in one go.
const BURST: usize = 50;

pub const BAZARR_DOWN: &str = "Bazarr nu răspunde.";
pub const NOT_CONFIGURED: &str = "Configurează Gemini în Bazarr: traducătorul Gemini, cu o cheie API.";
pub const STATE_ERROR: &str = "Starea aplicației (state.local.json) nu se poate citi.";
pub const HAS_RO: &str = "avea deja RO";

/// `Limita zilnică de traduceri (40) atinsă, reîncearcă mâine.`
pub fn cap_text(per_day: u32) -> String {
    format!("Limita zilnică de traduceri ({per_day}) atinsă, reîncearcă mâine.")
}

/// `1 episod`, `5 episoade`, `20 de episoade`.
pub fn ep_text(n: usize) -> String {
    match n {
        1 => "1 episod".to_string(),
        _ if n % 100 >= 20 || (n > 0 && n.is_multiple_of(100)) => format!("{n} de episoade"),
        _ => format!("{n} episoade"),
    }
}

// -- the queue -----------------------------------------------------------------------------

/// One request: an episode (Sonarr series and episode ids) or a movie (Radarr id).
#[derive(Debug, Clone, PartialEq)]
pub struct QueueItem {
    pub series_id: i64,
    pub episode_id: i64,
    pub movie_id: i64,
    /// The series or movie title.
    pub title: String,
    pub season: i64,
    pub episode: i64,
    pub requested_at: f64,
}

impl QueueItem {
    pub fn episode(series_id: i64, episode_id: i64, title: &str, season: i64, episode: i64, at: f64) -> QueueItem {
        QueueItem { series_id, episode_id, movie_id: 0, title: title.to_string(), season, episode, requested_at: at }
    }

    pub fn movie(movie_id: i64, title: &str, at: f64) -> QueueItem {
        QueueItem { series_id: 0, episode_id: 0, movie_id, title: title.to_string(), season: 0, episode: 0, requested_at: at }
    }

    pub fn is_movie(&self) -> bool {
        self.movie_id != 0
    }

    pub fn key(&self) -> String {
        if self.is_movie() {
            movie_key(self.movie_id)
        } else {
            episode_key(self.episode_id)
        }
    }

    pub fn cand(&self) -> Cand {
        Cand {
            key: self.key(),
            series_id: self.series_id,
            episode_id: self.episode_id,
            movie_id: self.movie_id,
            title: self.title.clone(),
            season: self.season,
            episode: self.episode,
        }
    }

    /// `Show S01E02` / the movie title.
    pub fn label(&self) -> String {
        self.cand().name()
    }

    pub fn to_json(&self) -> Value {
        json!({"kind": if self.is_movie() { "movie" } else { "episode" }, "key": self.key(), "series_id": self.series_id,
               "episode_id": self.episode_id, "movie_id": self.movie_id, "title": self.title, "season": self.season,
               "episode": self.episode, "label": self.label(), "requested_at": self.requested_at})
    }

    pub fn from_json(v: &Value) -> Option<QueueItem> {
        let i = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
        let title = v.get("title").and_then(Value::as_str).unwrap_or("");
        let at = v.get("requested_at").and_then(Value::as_f64).unwrap_or(0.0);
        match v.get("kind").and_then(Value::as_str)? {
            "episode" if i("series_id") > 0 && i("episode_id") > 0 => {
                Some(QueueItem::episode(i("series_id"), i("episode_id"), title, i("season"), i("episode"), at))
            }
            "movie" if i("movie_id") > 0 => Some(QueueItem::movie(i("movie_id"), title, at)),
            _ => None,
        }
    }
}

/// What became of a manual request.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub key: String,
    pub label: String,
    pub ok: bool,
    /// `tradusă`, `avea deja RO`, or why it failed (Romanian, for the page and ntfy).
    pub text: String,
    pub at: f64,
}

impl Outcome {
    pub fn new(key: &str, label: &str, ok: bool, text: &str, at: f64) -> Outcome {
        Outcome { key: key.to_string(), label: label.to_string(), ok, text: text.to_string(), at }
    }

    pub fn to_json(&self) -> Value {
        json!({"key": self.key, "label": self.label, "ok": self.ok, "text": self.text, "at": self.at})
    }

    pub fn from_json(v: &Value) -> Option<Outcome> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
        Some(Outcome { key: s("key")?, label: s("label").unwrap_or_default(), ok: v.get("ok")?.as_bool()?, text: s("text").unwrap_or_default(), at: v.get("at")?.as_f64()? })
    }
}

/// A copy of the shared state, for the page.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub tr: TrState,
    pub queue: Vec<QueueItem>,
    pub recent: Vec<Outcome>,
}

impl Snapshot {
    /// 1-based position of `key` in the queue.
    pub fn position(&self, key: &str) -> Option<usize> {
        self.queue.iter().position(|q| q.key() == key).map(|i| i + 1)
    }

    pub fn inflight_key(&self) -> Option<&str> {
        self.tr.inflight.as_ref().map(|f| f.key.as_str())
    }

    /// The live RO state of `key` from its `base` (`ok` / `missing` / `translating` / null,
    /// what Bazarr says): `(ro, ro_pos, ro_note)`.
    pub fn ro_of(&self, key: &str, base: &Value) -> (Value, Value, Value) {
        if self.inflight_key() == Some(key) {
            return (json!("translating"), Value::Null, Value::Null);
        }
        if let Some(p) = self.position(key) {
            return (json!("queued"), json!(p), Value::Null);
        }
        let note = match self.recent.iter().rev().find(|o| o.key == key) {
            Some(o) if !o.ok && base == &json!("missing") => json!(format!("Ultima traducere a eșuat: {}.", o.text)),
            _ => Value::Null,
        };
        (base.clone(), Value::Null, note)
    }
}

#[derive(Default)]
struct Shared {
    loaded: bool,
    tr: TrState,
    queue: Vec<QueueItem>,
    recent: Vec<Outcome>,
}

impl Shared {
    fn json(&self) -> Value {
        let mut v = self.tr.to_json();
        v["queue"] = Value::Array(self.queue.iter().map(QueueItem::to_json).collect());
        v["recent"] = Value::Array(self.recent.iter().map(Outcome::to_json).collect());
        v
    }

    fn read(&mut self, v: Option<&Value>) {
        self.tr = TrState::from_json(v);
        let list = |k: &str| v.and_then(|v| v.get(k)).and_then(Value::as_array).cloned().unwrap_or_default();
        let mut seen = HashSet::new();
        self.queue = list("queue").iter().filter_map(QueueItem::from_json).filter(|q| seen.insert(q.key())).collect();
        self.recent = list("recent").iter().filter_map(Outcome::from_json).collect();
        self.loaded = true;
    }

    fn record(&mut self, o: Outcome) {
        self.recent.push(o);
        let extra = self.recent.len().saturating_sub(RECENT);
        self.recent.drain(..extra);
    }

    /// Demo: S01E06 of Frieren in flight, S01E07 and S01E08 queued, 3 done today.
    fn demo(&mut self, now: f64) {
        let id = |e: i64| crate::logic::series::demo_episode_id(1, 1, e);
        let title = crate::logic::demo::series(1).map_or("Frieren", |s| s.1);
        self.tr.day = local_today();
        self.tr.count = 3;
        self.tr.inflight = Some(InFlight {
            key: episode_key(id(6)),
            series_id: 1,
            episode_id: id(6),
            movie_id: 0,
            name: format!("{title} S01E06"),
            en: String::new(),
            ro: String::new(),
            started: now - 90.0,
            tries: 0,
            manual: true,
        });
        self.queue = vec![QueueItem::episode(1, id(7), title, 1, 7, now - 60.0), QueueItem::episode(1, id(8), title, 1, 8, now - 60.0)];
        self.recent = vec![Outcome::new(&episode_key(id(5)), &format!("{title} S01E05"), true, "tradusă", now - 600.0)];
        self.loaded = true;
    }
}

/// Held by `AppState` (`app.subs`): the queue, the last results and a copy of the job's
/// translation state, the job's wake-up, Bazarr's translator answer.
#[derive(Default)]
pub struct Manual {
    inner: Mutex<Shared>,
    /// Notified on every enqueue; the subs job selects on it.
    pub wake: tokio::sync::Notify,
    translator: Mutex<Option<(Instant, Option<bool>)>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What [`Manual::enqueue`] did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Enqueued {
    pub added: usize,
    /// Queued or in flight already.
    pub already: usize,
    /// The queue length after it.
    pub queue: usize,
}

impl Manual {
    fn ensure_locked(app: &AppState, sh: &mut Shared) -> Result<(), &'static str> {
        if sh.loaded {
            return Ok(());
        }
        if app.demo {
            sh.demo(unix_now());
            return Ok(());
        }
        let m = app.state_file.try_load()?;
        sh.read(m.get(TR_KEY));
        Ok(())
    }

    /// The whole key to the state file (not in dry-run: memory only).
    fn persist(app: &AppState, sh: &Shared, mut extra: Map<String, Value>) {
        if app.dry_run {
            return;
        }
        extra.insert(TR_KEY.into(), sh.json());
        app.state_file.update(extra);
    }

    /// The job's translation state, read once from `file` (the state file the job loaded).
    pub fn load_from(&self, app: &AppState, file: &Map<String, Value>) -> TrState {
        let mut sh = lock(&self.inner);
        if !sh.loaded {
            if app.demo {
                sh.demo(unix_now());
            } else {
                sh.read(file.get(TR_KEY));
            }
        }
        sh.tr.clone()
    }

    /// The job saves its translation state (plus `extra` top-level keys) with the queue.
    pub fn save(&self, app: &AppState, tr: &TrState, extra: Map<String, Value>) {
        let mut sh = lock(&self.inner);
        if !sh.loaded {
            if let Ok(m) = app.state_file.try_load() {
                sh.read(m.get(TR_KEY));
            }
            sh.loaded = true;
        }
        sh.tr = tr.clone();
        Self::persist(app, &sh, extra);
    }

    /// The shared state now; `Err` if it was never loaded and the state file is unreadable.
    pub fn snapshot(&self, app: &AppState) -> Result<Snapshot, &'static str> {
        let mut sh = lock(&self.inner);
        Self::ensure_locked(app, &mut sh)?;
        Ok(Snapshot { tr: sh.tr.clone(), queue: sh.queue.clone(), recent: sh.recent.clone() })
    }

    /// Add `items` at the end (FIFO), skipping the queued and the one in flight; saved, and
    /// the job is woken.
    pub fn enqueue(&self, app: &AppState, items: Vec<QueueItem>) -> Result<Enqueued, &'static str> {
        let mut sh = lock(&self.inner);
        Self::ensure_locked(app, &mut sh)?;
        let mut keys: HashSet<String> = sh.queue.iter().map(QueueItem::key).collect();
        if let Some(f) = &sh.tr.inflight {
            keys.insert(f.key.clone());
        }
        let (mut added, mut already) = (0, 0);
        for it in items {
            if keys.insert(it.key()) {
                sh.queue.push(it);
                added += 1;
            } else {
                already += 1;
            }
        }
        if added > 0 {
            Self::persist(app, &sh, Map::new());
        }
        let queue = sh.queue.len();
        drop(sh);
        if added > 0 {
            self.wake.notify_one();
        }
        Ok(Enqueued { added, already, queue })
    }

    /// Remove the queued items `pick` chooses (never the one in flight); saved. Returns how many.
    pub fn cancel(&self, app: &AppState, pick: impl Fn(&QueueItem) -> bool) -> Result<usize, &'static str> {
        let mut sh = lock(&self.inner);
        Self::ensure_locked(app, &mut sh)?;
        let before = sh.queue.len();
        sh.queue.retain(|q| !pick(q));
        let removed = before - sh.queue.len();
        if removed > 0 {
            Self::persist(app, &sh, Map::new());
        }
        Ok(removed)
    }

    /// The next request (the job; memory only, the job saves).
    pub fn front(&self) -> Option<QueueItem> {
        lock(&self.inner).queue.first().cloned()
    }

    /// `key` leaves the queue (the job saves).
    pub fn finish(&self, key: &str) {
        lock(&self.inner).queue.retain(|q| q.key() != key);
    }

    /// A result for the page (the job saves).
    pub fn record(&self, o: Outcome) {
        lock(&self.inner).record(o);
    }

    pub fn has_queue(&self) -> bool {
        !lock(&self.inner).queue.is_empty()
    }

    /// Something queued or in flight: the job looks often.
    pub fn busy(&self) -> bool {
        let sh = lock(&self.inner);
        !sh.queue.is_empty() || sh.tr.inflight.is_some()
    }

    /// The job read Bazarr's translator settings.
    pub fn note_translator(&self, ready: bool) {
        *lock(&self.translator) = Some((Instant::now(), Some(ready)));
    }

    /// Bazarr's translator is Gemini with a key (None: Bazarr does not answer). Cached.
    pub async fn translator(&self, app: &AppState) -> Option<bool> {
        if app.demo {
            return Some(true);
        }
        if let Some((at, v)) = *lock(&self.translator) {
            let ttl = if v.is_some() { TRANSLATOR_TTL } else { TRANSLATOR_RETRY };
            if at.elapsed() < ttl {
                return v;
            }
        }
        let v = app.services.bazarr.get("/api/system/settings", BAZARR_TIMEOUT).await.ok().map(|s| translator_ready(&s));
        *lock(&self.translator) = Some((Instant::now(), v));
        v
    }
}

// -- processing (the subs job) -------------------------------------------------------------

/// A failed manual request: kept for the page, ntfy (`jobs.subs.translate_notify`).
pub async fn report_failure(app: &AppState, cfg: &TrSettings, key: &str, label: &str, why: &str, now: f64) {
    app.subs.record(Outcome::new(key, label, false, why, now));
    if cfg.notify {
        let msg = format!("Traducere RO eșuată: {label} ({why})");
        app.notifier.send("Subtitrări RO", &msg, Some(vec!["warning".into()]), None, crate::services::ntfy::TIMEOUT).await;
    }
}

/// Bazarr's record of a queued item. `Err(None)`: Bazarr failed (wait); `Err(Some(why))`:
/// it does not know the item.
async fn record_of(app: &AppState, it: &QueueItem, cache: &mut HashMap<i64, Vec<Value>>) -> Result<Value, Option<&'static str>> {
    let bz = &app.services.bazarr;
    if it.is_movie() {
        let recs = bz.data(&format!("/api/movies?radarrid%5B%5D={}", it.movie_id), BAZARR_TIMEOUT).await.map_err(|_| None)?;
        return recs.into_iter().find(|r| id_of(r.get("radarrId")) == Some(it.movie_id)).ok_or(Some("Bazarr nu cunoaște filmul"));
    }
    if let std::collections::hash_map::Entry::Vacant(slot) = cache.entry(it.series_id) {
        slot.insert(bz.data(&format!("/api/episodes?seriesid%5B%5D={}", it.series_id), BAZARR_TIMEOUT).await.map_err(|_| None)?);
    }
    cache[&it.series_id].iter().find(|r| id_of(r.get("sonarrEpisodeId")) == Some(it.episode_id)).cloned().ok_or(Some("Bazarr nu cunoaște episodul"))
}

/// Start the next manual request(s): items that end at once (RO there, a failure) are
/// handled and the next one is tried, until one is in flight. Returns true: the state changed.
pub async fn step(app: &AppState, tr: &mut TrState, quiet: &mut Quiet, tools: Option<Arc<dyn SubTools>>, cfg: &TrSettings, when: When<'_>) -> bool {
    let When { now, today } = when;
    let m = &app.subs;
    if tr.inflight.is_some() || !m.has_queue() {
        return false;
    }
    if manual_gate(tr, cfg, today).is_err() {
        quiet.warn("tr-manual-cap", format!("subs: the daily translation cap ({}) is reached, the manual queue waits", cfg.per_day));
        return false;
    }
    quiet.clear("tr-manual-cap");
    if !bazarr_gate(app, quiet).await {
        return false;
    }
    let mut changed = false;
    let mut cache = HashMap::new();
    for _ in 0..BURST {
        if tr.used_today(today) >= cfg.per_day {
            break;
        }
        let Some(it) = m.front() else { break };
        let (key, label) = (it.key(), it.label());
        let rec = match record_of(app, &it, &mut cache).await {
            Ok(r) => {
                quiet.clear("tr-manual-bazarr");
                r
            }
            Err(None) => {
                quiet.warn("tr-manual-bazarr", "subs: Bazarr failed, the manual translation queue waits".into());
                break;
            }
            Err(Some(why)) => {
                m.finish(&key);
                report_failure(app, cfg, &key, &label, why, now).await;
                changed = true;
                continue;
            }
        };
        changed = true;
        if crate::logic::series::has_ro(rec.get("subtitles")) {
            log_info!("subs: {} has a RO subtitle in Bazarr already, not translated", label.chars().map(|c| if c.is_ascii() { c } else { '?' }).collect::<String>());
            m.finish(&key);
            m.record(Outcome::new(&key, &label, true, HAS_RO, now));
            continue;
        }
        let video = rec.get("path").and_then(Value::as_str).unwrap_or("").to_string();
        let prep = prepare(app, quiet, tools.clone(), &it.cand(), &video, now, 0).await;
        m.finish(&key);
        match prep {
            Prep::Started(mut f) => {
                f.manual = true;
                tr.inflight = Some(f);
                tr.note_started(today, it.is_movie());
                break;
            }
            Prep::DryRun => {
                tr.note_started(today, it.is_movie());
                m.record(Outcome::new(&key, &label, true, "mod de probă: doar în jurnal", now));
            }
            Prep::HasRo => m.record(Outcome::new(&key, &label, true, HAS_RO, now)),
            Prep::NoFile => report_failure(app, cfg, &key, &label, "fișierul video lipsește", now).await,
            Prep::NoSource(why) | Prep::Failed(why) => report_failure(app, cfg, &key, &label, why, now).await,
        }
    }
    changed
}

// -- routes --------------------------------------------------------------------------------

/// A valid translate / cancel body.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// `{seriesId, episodeIds}`: these (1..=100 distinct ids).
    Episodes { series_id: i64, ids: Vec<i64> },
    /// `{seriesId, season, missingOnly: true}`: the season's episodes without RO.
    Season { series_id: i64, season: i64 },
    /// `{movieId}`.
    Movie { movie_id: i64 },
}

/// The body, strictly: positive JSON integers (a season may be 0, the specials); `movieId`
/// alone, or `seriesId` with exactly one of `episodeIds` / `season` (+ `missingOnly: true`).
pub fn parse_target(body: &Map<String, Value>) -> Option<Target> {
    let pos = |k: &str| match body.get(k) {
        None => Ok(None),
        Some(v) => v.as_i64().filter(|n| *n > 0).map(Some).ok_or(()),
    };
    let (movie, series) = (pos("movieId").ok()?, pos("seriesId").ok()?);
    let (ids, season, missing) = (body.get("episodeIds"), body.get("season"), body.get("missingOnly"));
    if let Some(movie_id) = movie {
        return (series.is_none() && ids.is_none() && season.is_none() && missing.is_none()).then_some(Target::Movie { movie_id });
    }
    let series_id = series?;
    match (ids, season) {
        (Some(Value::Array(a)), None) if missing.is_none() => {
            let ids: Vec<i64> = a.iter().map(|v| v.as_i64().filter(|n| *n > 0)).collect::<Option<_>>()?;
            let distinct: HashSet<i64> = ids.iter().copied().collect();
            (!ids.is_empty() && ids.len() <= MAX_IDS && distinct.len() == ids.len()).then_some(Target::Episodes { series_id, ids })
        }
        (None, Some(s)) if missing == Some(&Value::Bool(true)) => {
            let season = s.as_i64().filter(|n| (0..=1000).contains(n))?;
            Some(Target::Season { series_id, season })
        }
        _ => None,
    }
}

fn err(code: u16, msg: &str) -> (u16, Value) {
    (code, json!({"error": msg}))
}

/// `GET /api/subs` (also `subs` in the series screen and the movie sheet).
pub async fn api_status(app: &AppState) -> Value {
    let cfg = TrSettings::from_app(app);
    let translator = app.subs.translator(app).await;
    let snap = app.subs.snapshot(app).unwrap_or_default();
    let today = local_today();
    let inflight = snap.tr.inflight.as_ref().map(|f| {
        json!({"key": f.key, "label": f.name, "series_id": f.series_id, "episode_id": f.episode_id, "movie_id": f.movie_id,
               "started": f.started, "manual": f.manual})
    });
    let queue: Vec<Value> = snap
        .queue
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let mut v = q.to_json();
            v["position"] = json!(i + 1);
            v
        })
        .collect();
    let recent: Vec<Value> = snap.recent.iter().rev().take(10).map(Outcome::to_json).collect();
    json!({"translator": translator, "auto": cfg.auto, "used": snap.tr.used_today(&today), "per_day": cfg.per_day,
           "inflight": inflight, "queue": queue, "queue_len": snap.queue.len(), "recent": recent,
           "demo": app.demo, "dry_run": app.dry_run})
}

/// `ro`, `ro_pos`, `ro_note` of every episode of a series screen, live.
pub fn merge_series(d: &mut Value, snap: &Snapshot) {
    for s in d["seasons"].as_array_mut().into_iter().flatten() {
        for e in s["episodes"].as_array_mut().into_iter().flatten() {
            let key = episode_key(e["id"].as_i64().unwrap_or(0));
            let (ro, pos, note) = snap.ro_of(&key, &e["ro"]);
            e["ro"] = ro;
            e["ro_pos"] = pos;
            e["ro_note"] = note;
        }
    }
}

/// The same for a movie sheet.
pub fn merge_movie(m: &mut Value, snap: &Snapshot) {
    let key = movie_key(m["id"].as_i64().unwrap_or(0));
    let (ro, pos, note) = snap.ro_of(&key, &m["ro"]);
    m["ro"] = ro;
    m["ro_pos"] = pos;
    m["ro_note"] = note;
}

/// `POST /api/subs/translate` (see [`parse_target`]) -> `(status, body)`.
pub async fn api_translate(app: &Arc<AppState>, body: &Map<String, Value>) -> (u16, Value) {
    let Some(target) = parse_target(body) else {
        return err(400, "Cerere invalidă.");
    };
    let cfg = TrSettings::from_app(app);
    let Ok(snap) = app.subs.snapshot(app) else { return err(500, STATE_ERROR) };
    let used = snap.tr.used_today(&local_today());
    if used >= cfg.per_day {
        return (409, json!({"error": cap_text(cfg.per_day), "cap": true}));
    }
    match app.subs.translator(app).await {
        None => return err(502, BAZARR_DOWN),
        Some(false) => return (409, json!({"error": NOT_CONFIGURED, "translator": false})),
        Some(true) => {}
    }
    let now = unix_now();
    // The items from the (live) series screen or movie sheet: what has RO is skipped.
    let (items, has_ro, movie) = match &target {
        Target::Movie { movie_id } => {
            let (code, m) = crate::logic::series::api_movie(app, *movie_id).await;
            if code != 200 {
                return (code, json!({"error": m["error"].clone()}));
            }
            if m["file"].is_null() {
                return err(409, "Filmul nu are încă un fișier.");
            }
            if m["ro"] == json!("ok") {
                (vec![], 1, true)
            } else {
                (vec![QueueItem::movie(*movie_id, m["title"].as_str().unwrap_or(""), now)], 0, true)
            }
        }
        Target::Episodes { series_id, .. } | Target::Season { series_id, .. } => {
            let (code, d) = crate::logic::series::api_detail(app, *series_id).await;
            if code != 200 {
                return (code, json!({"error": d["error"].clone()}));
            }
            let title = d["title"].as_str().unwrap_or("").to_string();
            let eps: Vec<&Value> = d["seasons"].as_array().into_iter().flatten().flat_map(|s| s["episodes"].as_array().into_iter().flatten()).collect();
            let item = |e: &Value| {
                let n = |k: &str| e[k].as_i64().unwrap_or(0);
                QueueItem::episode(*series_id, n("id"), &title, n("season"), n("episode"), now)
            };
            match &target {
                Target::Episodes { ids, .. } => {
                    let mut picked = Vec::new();
                    for id in ids {
                        match eps.iter().find(|e| e["id"].as_i64() == Some(*id)) {
                            Some(e) => picked.push(*e),
                            None => return err(404, "Episodul nu există sau nu are încă un fișier."),
                        }
                    }
                    let has_ro = picked.iter().filter(|e| e["ro"] == json!("ok")).count();
                    (picked.into_iter().filter(|e| e["ro"] != json!("ok")).map(item).collect(), has_ro, false)
                }
                Target::Season { season, .. } => {
                    let Some(s) = d["seasons"].as_array().into_iter().flatten().find(|s| s["number"].as_i64() == Some(*season)) else {
                        return err(404, "Sezonul nu există.");
                    };
                    // The queued ones count as "already" (dedupe).
                    let todo = |e: &&Value| e["ro"] == json!("missing") || e["ro"] == json!("queued");
                    let list: Vec<QueueItem> = s["episodes"].as_array().into_iter().flatten().filter(todo).map(item).collect();
                    (list, 0, false)
                }
                Target::Movie { .. } => unreachable!(),
            }
        }
    };
    let asked = items.len();
    let q = match app.subs.enqueue(app, items) {
        Ok(q) => q,
        Err(_) => return err(500, STATE_ERROR),
    };
    let message = if q.added > 0 {
        let head = if movie {
            "Filmul e în coadă pentru traducere.".to_string()
        } else if q.added == 1 {
            "Episodul e în coadă pentru traducere.".to_string()
        } else {
            format!("{} în coadă pentru traducere.", ep_text(q.added))
        };
        let left = cfg.per_day.saturating_sub(used) as usize;
        if q.queue > left {
            format!("{head} Azi se mai pot traduce {left}; restul rămân în coadă până mâine.")
        } else {
            head
        }
    } else if q.already > 0 {
        if movie || asked == 1 { "E deja în coadă sau se traduce acum." } else { "Sunt deja în coadă sau se traduc acum." }.to_string()
    } else if has_ro > 0 {
        "Are deja subtitrare RO.".to_string()
    } else {
        "Niciun episod fără RO în acest sezon.".to_string()
    };
    if q.added > 0 {
        log_info!("subs: {} queued for RO translation ({} in the queue)", q.added, q.queue);
    }
    (200, json!({"ok": true, "queued": q.added, "already": q.already, "has_ro": has_ro, "queue": q.queue, "message": message,
                 "demo": app.demo, "subs": api_status(app).await}))
}

/// `POST /api/subs/translate/cancel` (the same bodies) -> `(status, body)`.
pub async fn api_cancel(app: &Arc<AppState>, body: &Map<String, Value>) -> (u16, Value) {
    let Some(target) = parse_target(body) else {
        return err(400, "Cerere invalidă.");
    };
    let Ok(snap) = app.subs.snapshot(app) else { return err(500, STATE_ERROR) };
    let pick = |q: &QueueItem| match &target {
        Target::Movie { movie_id } => q.movie_id == *movie_id,
        Target::Episodes { series_id, ids } => !q.is_movie() && q.series_id == *series_id && ids.contains(&q.episode_id),
        Target::Season { series_id, season } => !q.is_movie() && q.series_id == *series_id && q.season == *season,
    };
    let running = snap.tr.inflight.as_ref().is_some_and(|f| match &target {
        Target::Movie { movie_id } => f.movie_id == *movie_id,
        Target::Episodes { ids, .. } => f.movie_id == 0 && ids.contains(&f.episode_id),
        Target::Season { series_id, .. } => f.movie_id == 0 && f.series_id == *series_id,
    });
    let removed = match app.subs.cancel(app, pick) {
        Ok(n) => n,
        Err(_) => return err(500, STATE_ERROR),
    };
    let message = match (removed, running) {
        (0, true) => "Traducerea a pornit deja și nu se mai poate anula.".to_string(),
        (0, false) => "Nu era în coadă.".to_string(),
        (1, _) => "Traducerea a fost scoasă din coadă.".to_string(),
        (n, _) => format!("{} scoase din coadă.", ep_text(n)),
    };
    if removed > 0 {
        log_info!("subs: {removed} removed from the RO translation queue");
    }
    (200, json!({"ok": true, "removed": removed, "message": message, "demo": app.demo, "subs": api_status(app).await}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn targets() {
        assert_eq!(parse_target(&body(json!({"movieId": 7}))), Some(Target::Movie { movie_id: 7 }));
        assert_eq!(parse_target(&body(json!({"seriesId": 4, "episodeIds": [11, 12]}))), Some(Target::Episodes { series_id: 4, ids: vec![11, 12] }));
        assert_eq!(parse_target(&body(json!({"seriesId": 4, "season": 0, "missingOnly": true}))), Some(Target::Season { series_id: 4, season: 0 }));
        for bad in [
            json!({}),
            json!({"movieId": 0}),
            json!({"movieId": "7"}),
            json!({"movieId": 7, "seriesId": 4}),
            json!({"seriesId": 4}),
            json!({"seriesId": 4, "episodeIds": []}),
            json!({"seriesId": 4, "episodeIds": [1, 1]}),
            json!({"seriesId": 4, "episodeIds": [1, -2]}),
            json!({"seriesId": 4, "episodeIds": "1"}),
            json!({"seriesId": 4, "season": 1}),
            json!({"seriesId": 4, "season": 1, "missingOnly": false}),
            json!({"seriesId": 4, "season": -1, "missingOnly": true}),
            json!({"seriesId": 4, "season": 1, "missingOnly": true, "episodeIds": [3]}),
            json!({"seriesId": 4, "episodeIds": [3], "missingOnly": true}),
            json!({"seriesId": "4", "episodeIds": [3]}),
        ] {
            assert_eq!(parse_target(&body(bad.clone())), None, "{bad}");
        }
        let many: Vec<i64> = (1..=101).collect();
        assert_eq!(parse_target(&body(json!({"seriesId": 4, "episodeIds": many}))), None);
        assert_eq!(cap_text(40), "Limita zilnică de traduceri (40) atinsă, reîncearcă mâine.");
        assert_eq!((ep_text(1), ep_text(5), ep_text(20), ep_text(101)), ("1 episod".into(), "5 episoade".into(), "20 de episoade".into(), "101 episoade".into()));
    }

    #[test]
    fn items_and_live_state() {
        let ep = QueueItem::episode(4, 12, "Show", 1, 2, 100.0);
        let mv = QueueItem::movie(7, "Film", 101.0);
        assert_eq!((ep.key(), ep.label(), mv.key(), mv.label()), ("ep:12".into(), "Show S01E02".into(), "movie:7".into(), "Film".into()));
        assert_eq!(QueueItem::from_json(&ep.to_json()), Some(ep.clone()));
        assert_eq!(QueueItem::from_json(&mv.to_json()), Some(mv.clone()));
        assert_eq!(QueueItem::from_json(&json!({"kind": "episode", "series_id": 4})), None);
        assert_eq!(QueueItem::from_json(&json!({"kind": "show", "movie_id": 4})), None);
        let o = Outcome::new("ep:13", "Show S01E03", false, "job eșuat în Bazarr", 5.0);
        assert_eq!(Outcome::from_json(&o.to_json()), Some(o.clone()));
        let tr = TrState { inflight: Some(InFlight { key: "ep:11".into(), series_id: 4, episode_id: 11, movie_id: 0, name: "Show S01E01".into(),
                                                     en: "a".into(), ro: "b".into(), started: 1.0, tries: 0, manual: true }), ..Default::default() };
        let snap = Snapshot { tr, queue: vec![mv.clone(), ep.clone()], recent: vec![o] };
        assert_eq!(snap.ro_of("ep:11", &json!("missing")), (json!("translating"), Value::Null, Value::Null));
        assert_eq!(snap.ro_of("ep:12", &json!("missing")), (json!("queued"), json!(2), Value::Null));
        assert_eq!(snap.ro_of("movie:7", &Value::Null), (json!("queued"), json!(1), Value::Null));
        assert_eq!(snap.ro_of("ep:13", &json!("missing")).2, json!("Ultima traducere a eșuat: job eșuat în Bazarr."));
        assert_eq!(snap.ro_of("ep:13", &json!("ok")), (json!("ok"), Value::Null, Value::Null), "translated since: no note");
        // Merged into a series screen and a movie sheet.
        let mut d = json!({"seasons": [{"number": 1, "episodes": [{"id": 11, "ro": "missing"}, {"id": 12, "ro": "missing"}, {"id": 14, "ro": "ok"}]}]});
        merge_series(&mut d, &snap);
        let eps = &d["seasons"][0]["episodes"];
        assert_eq!((eps[0]["ro"].clone(), eps[1]["ro"].clone(), eps[1]["ro_pos"].clone(), eps[2]["ro"].clone()), (json!("translating"), json!("queued"), json!(2), json!("ok")));
        let mut m = json!({"id": 7, "ro": "missing"});
        merge_movie(&mut m, &snap);
        assert_eq!((m["ro"].clone(), m["ro_pos"].clone()), (json!("queued"), json!(1)));
    }

    #[test]
    fn queue_persists_dedupes_and_cancels() {
        let (a, _d) = crate::jobs::testutil::app("http://127.0.0.1:9", false, json!({}));
        let m = &a.subs;
        let e = |id: i64| QueueItem::episode(4, id, "Show", 1, id - 10, 100.0);
        let q = m.enqueue(&a, vec![e(12), e(13), e(12)]).unwrap();
        assert_eq!(q, Enqueued { added: 2, already: 1, queue: 2 });
        let q = m.enqueue(&a, vec![e(13), QueueItem::movie(7, "Film", 101.0)]).unwrap();
        assert_eq!(q, Enqueued { added: 1, already: 1, queue: 3 });
        // Persisted under subs_translate.queue, FIFO, the job state next to it.
        let saved = a.state_file.load();
        let keys: Vec<String> = saved[TR_KEY]["queue"].as_array().unwrap().iter().map(|v| v["key"].as_str().unwrap().to_string()).collect();
        assert_eq!(keys, ["ep:12", "ep:13", "movie:7"]);
        assert!(saved[TR_KEY].get("items").is_some() && saved[TR_KEY].get("inflight").is_some());
        // A new instance (a restart) reads it back.
        let fresh = Manual::default();
        let snap = fresh.snapshot(&a).unwrap();
        assert_eq!(snap.queue.iter().map(QueueItem::key).collect::<Vec<_>>(), keys);
        assert_eq!(fresh.front().unwrap().key(), "ep:12");
        // The job's save keeps the queue; the in-flight item is not queued again.
        let tr = TrState { inflight: Some(InFlight { key: "ep:14".into(), series_id: 4, episode_id: 14, movie_id: 0, name: "Show S01E04".into(),
                                                     en: "a".into(), ro: "b".into(), started: 1.0, tries: 0, manual: true }), ..Default::default() };
        m.save(&a, &tr, Map::new());
        assert_eq!(a.state_file.load()[TR_KEY]["queue"].as_array().unwrap().len(), 3);
        assert_eq!(m.enqueue(&a, vec![e(14)]).unwrap().added, 0);
        assert!(m.busy());
        // Cancel: queued ones only.
        assert_eq!(m.cancel(&a, |q| q.episode_id == 13 || q.episode_id == 14).unwrap(), 1);
        assert_eq!(a.state_file.load()[TR_KEY]["queue"].as_array().unwrap().len(), 2);
        m.finish("ep:12");
        m.finish("movie:7");
        assert!(!m.has_queue());
        // Results: the last RECENT kept.
        for i in 0..(RECENT + 5) {
            m.record(Outcome::new("ep:1", "x", true, "tradusă", i as f64));
        }
        m.save(&a, &TrState::default(), Map::new());
        let recent = a.state_file.load()[TR_KEY]["recent"].as_array().unwrap().clone();
        assert_eq!((recent.len(), recent[0]["at"].clone()), (RECENT, json!(5.0)));
        assert!(!m.busy());
    }

    #[tokio::test]
    async fn enqueue_wakes_the_job() {
        let (a, _d) = crate::jobs::testutil::app("http://127.0.0.1:9", false, json!({}));
        let woke = tokio::time::timeout(Duration::from_millis(50), a.subs.wake.notified()).await;
        assert!(woke.is_err(), "nothing queued yet");
        a.subs.enqueue(&a, vec![QueueItem::movie(7, "Film", 1.0)]).unwrap();
        // The permit is kept for a job that is busy right now.
        tokio::time::timeout(Duration::from_secs(1), a.subs.wake.notified()).await.expect("woken");
        a.subs.enqueue(&a, vec![QueueItem::movie(7, "Film", 1.0)]).unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(50), a.subs.wake.notified()).await.is_err(), "a duplicate wakes nothing");
    }

    use crate::jobs::testutil::{app, fake, ok, status, Fake, Req};

    fn gemini(ready: bool) -> Value {
        json!({"translator": {"translator_type": if ready { "gemini" } else { "google_translate" }, "gemini_keys": ["k"]}})
    }

    /// Show (Sonarr 4): S01E01 has RO, E02 and E03 do not, S02E01 does not.
    async fn upstream(ready: bool) -> Fake {
        fake(move |r: &Req| {
            let p = r.path.as_str();
            let ro = json!([{"code2": "ro", "forced": false}]);
            let file = |id: i64| json!({"id": id, "path": format!("D:\\S\\e{id}.mkv"), "size": 5, "mediaInfo": {"videoCodec": "x265", "videoBitDepth": 10}});
            let ep = |id: i64, s: i64, n: i64, f: i64| json!({"id": id, "seasonNumber": s, "episodeNumber": n, "title": "T", "episodeFileId": f});
            let rec = |id: i64, has: bool| if has { json!({"sonarrEpisodeId": id, "subtitles": ro, "missing_subtitles": []}) }
                                           else { json!({"sonarrEpisodeId": id, "subtitles": [], "missing_subtitles": ro}) };
            match p {
                "/api/v3/series/4" => ok(json!({"id": 4, "title": "Show", "year": 2020})),
                "/api/v3/movie/7" => ok(json!({"id": 7, "title": "Film", "movieFile": {"path": "D:\\M\\Film.mkv", "size": 5}})),
                "/api/movies" => ok(json!({"data": [{"radarrId": 7, "subtitles": [], "missing_subtitles": ro}]})),
                "/api/system/settings" => ok(gemini(ready)),
                "/api/system/jobs" => ok(json!({"data": []})),
                _ if p.starts_with("/api/v3/episodefile?seriesId=4") => ok(json!([file(1), file(2), file(3), file(6)])),
                _ if p.starts_with("/api/v3/episode?seriesId=4") => ok(json!([ep(11, 1, 1, 1), ep(12, 1, 2, 2), ep(13, 1, 3, 3), ep(21, 2, 1, 6)])),
                _ if p.starts_with("/library/sections/") => ok(json!({"MediaContainer": {"Metadata": []}})),
                _ if p.starts_with("/api/episodes?seriesid%5B%5D=4") => ok(json!({"data": [rec(11, true), rec(12, false), rec(13, false), rec(21, false)]})),
                _ => status(404),
            }
        })
        .await
    }

    fn eps(d: &Value, season: usize) -> Vec<(i64, Value, Value)> {
        d["seasons"][season]["episodes"].as_array().unwrap().iter().map(|e| (e["id"].as_i64().unwrap(), e["ro"].clone(), e["ro_pos"].clone())).collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn season_expansion_dedupe_and_cancel() {
        let f = upstream(true).await;
        let (a, _d) = app(&f.base, false, json!({}));
        let season = json!({"seriesId": 4, "season": 1, "missingOnly": true});
        let (code, v) = api_translate(&a, &body(season.clone())).await;
        assert_eq!((code, v["queued"].clone(), v["message"].clone()), (200, json!(2), json!("2 episoade în coadă pentru traducere.")), "{v}");
        assert_eq!(v["subs"]["queue_len"], json!(2));
        // The series screen: E01 ok, E02 #1, E03 #2; season 2 untouched.
        let (_, d) = crate::logic::series::api_detail(&a, 4).await;
        assert_eq!(eps(&d, 0), [(11, json!("ok"), Value::Null), (12, json!("queued"), json!(1)), (13, json!("queued"), json!(2))]);
        assert_eq!(eps(&d, 1), [(21, json!("missing"), Value::Null)]);
        assert_eq!((d["subs"]["translator"].clone(), d["subs"]["per_day"].clone(), d["subs"]["auto"].clone()), (json!(true), json!(40), json!(false)));
        // Again: nothing new.
        let (_, v) = api_translate(&a, &body(season.clone())).await;
        assert_eq!((v["queued"].clone(), v["already"].clone(), v["message"].clone()), (json!(0), json!(2), json!("Sunt deja în coadă sau se traduc acum.")));
        // An episode with RO, an unknown one, an unknown season.
        let (_, v) = api_translate(&a, &body(json!({"seriesId": 4, "episodeIds": [11]}))).await;
        assert_eq!((v["queued"].clone(), v["message"].clone()), (json!(0), json!("Are deja subtitrare RO.")));
        assert_eq!(api_translate(&a, &body(json!({"seriesId": 4, "episodeIds": [99]}))).await.0, 404);
        assert_eq!(api_translate(&a, &body(json!({"seriesId": 4, "season": 7, "missingOnly": true}))).await.0, 404);
        // One episode, then the movie.
        let (_, v) = api_translate(&a, &body(json!({"seriesId": 4, "episodeIds": [21]}))).await;
        assert_eq!((v["queued"].clone(), v["message"].clone()), (json!(1), json!("Episodul e în coadă pentru traducere.")));
        let (_, v) = api_translate(&a, &body(json!({"movieId": 7}))).await;
        assert_eq!((v["queued"].clone(), v["message"].clone()), (json!(1), json!("Filmul e în coadă pentru traducere.")));
        let (_, m) = crate::logic::series::api_movie(&a, 7).await;
        assert_eq!((m["ro"].clone(), m["ro_pos"].clone()), (json!("queued"), json!(4)));
        // Cancel the season: the two queued ones go, S02E01 and the movie stay.
        let (code, v) = api_cancel(&a, &body(season)).await;
        assert_eq!((code, v["removed"].clone(), v["message"].clone()), (200, json!(2), json!("2 episoade scoase din coadă.")));
        let (_, d) = crate::logic::series::api_detail(&a, 4).await;
        assert_eq!(eps(&d, 0)[1], (12, json!("missing"), Value::Null));
        assert_eq!(eps(&d, 1)[0], (21, json!("queued"), json!(1)));
        let (_, v) = api_cancel(&a, &body(json!({"movieId": 7}))).await;
        assert_eq!((v["removed"].clone(), v["message"].clone()), (json!(1), json!("Traducerea a fost scoasă din coadă.")));
        let (_, v) = api_cancel(&a, &body(json!({"movieId": 7}))).await;
        assert_eq!((v["removed"].clone(), v["message"].clone()), (json!(0), json!("Nu era în coadă.")));
        assert!(f.mutations().is_empty(), "the routes never touch Bazarr");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn translate_refusals() {
        // Not Gemini with a key: refused.
        let f = upstream(false).await;
        let (a, _d) = app(&f.base, false, json!({"jobs": {"subs": {"translate_per_day": 2}}}));
        let (code, v) = api_translate(&a, &body(json!({"movieId": 7}))).await;
        assert_eq!((code, v["error"].clone(), v["translator"].clone()), (409, json!(NOT_CONFIGURED), json!(false)));
        assert_eq!(api_status(&a).await["translator"], json!(false));
        // The daily cap counts manual requests too, before anything is asked upstream.
        let mut tr = TrState::default();
        tr.note_started(&local_today(), false);
        tr.note_started(&local_today(), true);
        a.subs.save(&a, &tr, Map::new());
        let n = f.requests().len();
        let (code, v) = api_translate(&a, &body(json!({"movieId": 7}))).await;
        assert_eq!((code, v["error"].clone(), v["cap"].clone()), (409, json!(cap_text(2)), json!(true)));
        assert_eq!(f.requests().len(), n);
        assert_eq!(api_translate(&a, &body(json!({"movieId": "7"}))).await.0, 400);
        assert_eq!(api_cancel(&a, &body(json!({}))).await.0, 400);
        // Bazarr down.
        let (a, _d) = app("http://127.0.0.1:9", false, json!({}));
        let (code, v) = api_translate(&a, &body(json!({"movieId": 7}))).await;
        assert_eq!((code, v["error"].clone()), (502, json!(BAZARR_DOWN)));
        assert_eq!(api_status(&a).await["translator"], Value::Null);
        // A queue beyond what is left today says so.
        let f = upstream(true).await;
        let (a, _d) = app(&f.base, false, json!({"jobs": {"subs": {"translate_per_day": 1}}}));
        let (_, v) = api_translate(&a, &body(json!({"seriesId": 4, "season": 1, "missingOnly": true}))).await;
        assert_eq!(v["message"], json!("2 episoade în coadă pentru traducere. Azi se mai pot traduce 1; restul rămân în coadă până mâine."));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn demo_queue() {
        let (a, _d) = crate::jobs::testutil::app("http://127.0.0.1:9", true, json!({}));
        let a = AppState::new(a.config.clone(), crate::app::AppOptions { demo: true, ..Default::default() }).unwrap();
        let s = api_status(&a).await;
        assert_eq!((s["translator"].clone(), s["queue_len"].clone(), s["used"].clone()), (json!(true), json!(2), json!(3)));
        assert_eq!(s["inflight"]["label"], json!("Frieren: Beyond Journey's End S01E06"));
        let (_, d) = crate::logic::series::api_detail(&a, 1).await;
        let s1 = eps(&d, 0);
        assert_eq!((s1[4].1.clone(), s1[5].1.clone(), s1[6].clone(), s1[8].1.clone()),
                   (json!("ok"), json!("translating"), (10107, json!("queued"), json!(1)), json!("missing")));
        // The routes work on the demo queue (memory only).
        let (code, v) = api_translate(&a, &body(json!({"seriesId": 1, "season": 1, "missingOnly": true}))).await;
        assert_eq!((code, v["queued"].clone(), v["already"].clone()), (200, json!(4), json!(2)));
        let (_, v) = api_cancel(&a, &body(json!({"seriesId": 1, "episodeIds": [10109]}))).await;
        assert_eq!(v["removed"], json!(1));
        let (_, v) = api_translate(&a, &body(json!({"movieId": 4}))).await;
        assert_eq!(v["queued"], json!(1));
        let (_, v) = api_translate(&a, &body(json!({"movieId": 1}))).await;
        assert_eq!((v["queued"].clone(), v["message"].clone()), (json!(0), json!("Are deja subtitrare RO.")));
        assert!(a.state_file.load().get(TR_KEY).is_none());
    }

    #[test]
    fn demo_and_dry_run_stay_in_memory() {
        let (a, _d) = crate::jobs::testutil::app("http://127.0.0.1:9", true, json!({}));
        a.subs.enqueue(&a, vec![QueueItem::movie(7, "Film", 1.0)]).unwrap();
        assert!(a.state_file.load().get(TR_KEY).is_none(), "dry-run: memory only");
        assert!(a.subs.has_queue());
    }
}
