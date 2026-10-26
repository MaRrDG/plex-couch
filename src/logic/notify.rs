//! Notifications: movie ready, disk low, the test notification (spec 7; logic.py:903-990,
//! app.py:386-456).
//!
//! The `notify` loop runs a tick at once, then every 60 s, skipped while the notifier is
//! disabled. A tick writes `movie_ready` / `disk_low` to the state file *before* sending, so
//! a crash never repeats a message.

use crate::app::AppState;
use crate::jobs::Stop;
use crate::logic::movies::{bazarr_ro_map, ro_of, RoMap};
use crate::logic::val;
use crate::{log_debug, log_error};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

pub const NOTIFY_POLL_SECONDS: u64 = 60;
/// Below the page's 8 s request timeout.
pub const NOTIFY_TEST_TIMEOUT: Duration = Duration::from_secs(6);
/// More new movies than this in one poll are recorded, not notified.
pub const MOVIE_READY_FLOOD: usize = 10;
pub const DISK_REARM_GB: f64 = 10.0;

/// POST /api/notify/test: `(status, body)`.
pub async fn send_test_notification(app: &Arc<AppState>) -> (u16, Value) {
    if !app.notifier.enabled() {
        return (409, json!({"error": "Notificările nu sunt pornite. Rulează plex-couch --setup-ntfy."}));
    }
    if app.demo {
        return (200, json!({"ok": true}));
    }
    let ok = app
        .notifier
        .send(&app.config.app_name(), "Test Plex Couch: notificările merg.", None, None, NOTIFY_TEST_TIMEOUT)
        .await;
    if !ok {
        return (502, json!({"error": "Notificarea nu a plecat. Verifică internetul PC-ului și reîncearcă."}));
    }
    (200, json!({"ok": true}))
}

/// `Title (Year)`, or the title without a year.
pub fn movie_label(movie: &Value) -> String {
    let title = val::s(movie, "title");
    match val::get_truthy(movie, "year") {
        Some(y) => format!("{title} ({})", crate::json::py_str(y)),
        None => title,
    }
}

/// `ro`: Some(true) RO subtitle found, Some(false) still missing, None Bazarr unknown.
pub fn movie_ready_text(movie: &Value, ro: Option<bool>) -> String {
    let label = movie_label(movie);
    match ro {
        Some(true) => format!("{label} e gata de vizionat, cu subtitrare RO."),
        Some(false) => format!("{label} e gata de vizionat. Subtitrarea RO încă lipsește."),
        None => format!("{label} e gata de vizionat."),
    }
}

/// `_id_order`: numeric ids first, in numeric order, then the rest as text.
fn id_order(a: &String, b: &String) -> std::cmp::Ordering {
    let k = |x: &String| -> (u8, u128, String) {
        if !x.is_empty() && x.bytes().all(|c| c.is_ascii_digit()) {
            (0, x.parse().unwrap_or(u128::MAX), String::new())
        } else {
            (1, 0, x.clone())
        }
    };
    k(a).cmp(&k(b))
}

fn sorted_ids(ids: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut v: Vec<String> = ids.into_iter().collect();
    v.sort_by(id_order);
    v.dedup();
    v
}

/// Which "movie ready" notifications to send (pure; the caller persists the state).
///
/// `state`: `{"primed", "done": [ids], "pending": {id: first seen with a file}}`.
/// `movies` None (Radarr down): nothing changes. `ro_map` None: Bazarr down. The first call
/// only records the movies that already have a file. A movie that newly has a file
/// notifies once: at once with a Romanian subtitle, else once `wait_s` has passed. Notified
/// ids are kept for good, so an upgrade never repeats. Returns `(new state, messages)`.
pub fn movie_ready_step(
    state: Option<&Value>,
    movies: Option<&[Value]>,
    ro_map: Option<&RoMap>,
    now: f64,
    wait_s: f64,
) -> (Value, Vec<String>) {
    let empty = json!({});
    let state = state.filter(|s| crate::json::truthy(Some(s))).unwrap_or(&empty);
    let mut done: BTreeSet<String> = val::arr(state, "done").iter().map(crate::json::py_str).collect();
    let mut pending: Vec<(String, f64)> = match state.get("pending") {
        Some(Value::Object(m)) => m.iter().map(|(k, v)| (k.clone(), val::f(Some(v)))).collect(),
        _ => Vec::new(),
    };
    let pending_json = |p: &[(String, f64)]| -> Value {
        Value::Object(p.iter().map(|(k, v)| (k.clone(), json!(v))).collect::<Map<String, Value>>())
    };
    let Some(movies) = movies else {
        let primed = crate::json::truthy(state.get("primed"));
        return (json!({"primed": primed, "done": sorted_ids(done), "pending": pending_json(&pending)}), Vec::new());
    };
    let mut by_id: Vec<(String, &Value)> = Vec::new();
    for m in movies {
        if let Some(id) = m.get("id").filter(|v| !v.is_null()) {
            let k = crate::json::py_str(id);
            by_id.retain(|(x, _)| *x != k);
            by_id.push((k, m));
        }
    }
    let movie = |id: &str| by_id.iter().find(|(k, _)| k == id).map(|(_, m)| *m);
    let with_file: BTreeSet<String> =
        by_id.iter().filter(|(_, m)| crate::json::truthy(m.get("hasFile"))).map(|(k, _)| k.clone()).collect();
    if !crate::json::truthy(state.get("primed")) {
        return (json!({"primed": true, "done": sorted_ids(with_file), "pending": {}}), Vec::new());
    }
    let new: Vec<String> = with_file
        .iter()
        .filter(|k| !done.contains(*k) && !pending.iter().any(|(p, _)| p == *k))
        .cloned()
        .collect();
    if new.len() > MOVIE_READY_FLOOD {
        done.extend(new); // a mass import: recorded, no flood
    }
    let mut messages = Vec::new();
    for mid in sorted_ids(with_file.iter().filter(|k| !done.contains(*k)).cloned()) {
        let first = match pending.iter().find(|(k, _)| *k == mid) {
            Some((_, v)) => *v,
            None => {
                pending.push((mid.clone(), now));
                now
            }
        };
        let m = movie(&mid).expect("id from by_id");
        let ro = ro_map.map(|r| ro_of(r, m.get("id")).unwrap_or(false));
        if ro == Some(true) {
            messages.push(movie_ready_text(m, Some(true)));
        } else if now - first >= wait_s {
            messages.push(movie_ready_text(m, ro));
        } else {
            continue;
        }
        done.insert(mid.clone());
        pending.retain(|(k, _)| *k != mid);
    }
    pending.retain(|(k, _)| with_file.contains(k) && !done.contains(k));
    (json!({"primed": true, "done": sorted_ids(done), "pending": pending_json(&pending)}), messages)
}

/// `ntpath.splitdrive` drive: `D:\Movies` -> `D:`, `\\server\share\Movies` ->
/// `\\server\share`; None without a drive.
pub fn drive_of(path: Option<&Value>) -> Option<String> {
    let p = match path {
        Some(v) if crate::json::truthy(Some(v)) => crate::json::py_str(v),
        _ => String::new(),
    };
    let norm = p.replace('/', "\\");
    let d: String = if norm.starts_with("\\\\") {
        let start = if norm.get(..8).is_some_and(|s| s.eq_ignore_ascii_case("\\\\?\\UNC\\")) { 8 } else { 2 };
        match norm[start..].find('\\').map(|i| i + start) {
            None => p.clone(),
            Some(i) => match norm[i + 1..].find('\\').map(|j| j + i + 1) {
                None => p.clone(),
                Some(j) => p[..j].to_string(),
            },
        }
    } else if norm.as_bytes().get(1) == Some(&b':') {
        p.chars().take(2).collect()
    } else {
        String::new()
    };
    if d.chars().count() == 2 {
        Some(d.to_uppercase())
    } else if d.is_empty() {
        None
    } else {
        Some(d)
    }
}

/// Hysteresis for the low-disk warning: `(new low, warn)`. Warn once below `min_gb`,
/// re-arm above `min_gb + 10`.
pub fn disk_low_step(low: bool, free_gb: Option<f64>, min_gb: f64) -> (bool, bool) {
    let Some(free) = free_gb else { return (low, false) };
    if !low && free < min_gb {
        return (true, true);
    }
    if low && free > min_gb + DISK_REARM_GB {
        return (false, false);
    }
    (low, false)
}

pub fn disk_low_text(drive: &str, free_gb: f64) -> String {
    format!("Mai ai {} GB liberi pe {drive}. Un film 4K are 20–30 GB.", free_gb.trunc() as i64)
}

/// The drive the disk-low check watches: the first Radarr root folder with a drive, else
/// the last one known (`D:` at first).
pub async fn radarr_drive(app: &AppState) -> String {
    match app.services.radarr.root_folders().await {
        Ok(folders) => {
            if let Some(d) = folders.iter().find_map(|f| drive_of(f.get("path"))) {
                *app.disk_drive.lock().unwrap_or_else(|e| e.into_inner()) = d;
            }
        }
        Err(e) => log_debug!("radarr root folders failed: {}", e.type_name()),
    }
    app.disk_drive.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub type Sent = Vec<(String, String, Vec<String>, Option<String>)>;

/// One round of the movie-ready and disk-low checks. Returns what was sent as
/// `(title, text, tags, priority)`.
pub async fn notify_tick(app: &Arc<AppState>, now: f64) -> Sent {
    notify_tick_with(app, now, &crate::system::info::free_gb).await
}

/// [`notify_tick`] with the free-space lookup replaced (tests).
pub async fn notify_tick_with(app: &Arc<AppState>, now: f64, free_gb: &(dyn Fn(&str) -> Option<f64> + Sync)) -> Sent {
    let cfg = match app.config.get("notifications") {
        Some(v @ Value::Object(_)) => v,
        _ => json!({}),
    };
    let movies = match app.services.radarr.movies().await {
        Ok(m) => Some(m),
        Err(e) => {
            log_debug!("notify: radarr failed: {}", e.type_name());
            None
        }
    };
    let ro_map = match app.services.bazarr.movies().await {
        Ok(b) => Some(bazarr_ro_map(&b)),
        Err(e) => {
            log_debug!("notify: bazarr failed: {}", e.type_name());
            None
        }
    };
    let state = app.state_file.load();
    let mut sent: Sent = Vec::new();
    let wait_s = val::f(cfg.get("ready_wait_minutes")) * 60.0;
    let (movie_ready, ready) = movie_ready_step(state.get("movie_ready"), movies.as_deref(), ro_map.as_ref(), now, wait_s);
    for text in ready {
        sent.push(("Film gata".into(), text, vec!["movie_camera".into()], None));
    }

    let drive = radarr_drive(app).await;
    let free = free_gb(&drive);
    let mut lows = match state.get("disk_low") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    let was_low = crate::json::truthy(lows.get(&drive));
    let (low, warn) = disk_low_step(was_low, free, val::f(cfg.get("disk_min_gb")));
    lows.insert(drive.clone(), json!(low));
    if warn {
        sent.push(("Spațiu puțin".into(), disk_low_text(&drive, free.unwrap_or(0.0)), vec!["warning".into()], Some("high".into())));
    }

    // Before sending: never twice, even after a crash. Only these keys: the file also holds
    // the login guard's known IPs (and the jobs' keys).
    let mut changes = Map::new();
    changes.insert("movie_ready".into(), movie_ready);
    changes.insert("disk_low".into(), Value::Object(lows));
    app.state_file.update(changes);
    for (title, text, tags, prio) in &sent {
        app.notifier
            .send(title, text, Some(tags.clone()), prio.clone().map(Value::from), crate::services::ntfy::TIMEOUT)
            .await;
    }
    sent
}

/// The `notify` loop. Not started in demo (jobs::spawn_all checks).
pub fn spawn_notify_loop(app: Arc<AppState>, mut stop: Stop) {
    tokio::spawn(async move {
        let mut first = true;
        loop {
            let wait = if first { Duration::ZERO } else { Duration::from_secs(NOTIFY_POLL_SECONDS) };
            if stop.sleep(wait).await {
                break;
            }
            first = false;
            if !app.notifier.enabled() {
                continue;
            }
            let a = app.clone();
            // its own task: a panic in a tick is logged and the loop goes on
            let r = tokio::spawn(async move { notify_tick(&a, crate::auth::session::now()).await }).await;
            if r.is_err() {
                log_error!("notify check failed");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logic::testutil::{self, json_answer, serve, Fake};

    const H: f64 = 3600.0;
    const WAIT: f64 = 20.0 * 60.0;
    const TOPIC: &str = "plexcouch-unit-test-topic-xyz";

    fn movie(id: i64, title: &str, year: i64, has_file: bool) -> Value {
        json!({"id": id, "title": title, "year": year, "hasFile": has_file})
    }

    fn ro(pairs: &[(i64, bool)]) -> RoMap {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn step(state: Option<&Value>, movies: Option<&[Value]>, ro_map: Option<&RoMap>, now: f64) -> (Value, Vec<String>) {
        movie_ready_step(state, movies, ro_map, now, WAIT)
    }

    fn primed() -> Value {
        let (state, msgs) =
            step(None, Some(&[movie(1, "The Godfather", 1972, true), movie(2, "Dune", 2024, false)]), Some(&ro(&[])), 0.0);
        assert!(msgs.is_empty());
        assert_eq!(state, json!({"primed": true, "done": ["1"], "pending": {}}));
        state
    }

    // py: test_notify.TestMovieReady.test_first_run_is_a_baseline
    #[test]
    fn first_run_is_a_baseline() {
        let state = primed();
        let (_s, msgs) = step(Some(&state), Some(&[movie(1, "The Godfather", 1972, true)]), Some(&ro(&[(1, false)])), 10.0 * H);
        assert!(msgs.is_empty());
    }

    // py: test_notify.TestMovieReady.test_radarr_down_changes_nothing
    #[test]
    fn radarr_down_changes_nothing() {
        let (state, msgs) = step(None, None, None, 0.0);
        assert_eq!((state["primed"].clone(), msgs.len()), (json!(false), 0));
        let state = primed();
        assert_eq!(step(Some(&state), None, None, 99.0 * H), (state, vec![]));
    }

    // py: test_notify.TestMovieReady.test_ro_present_notifies_at_once
    #[test]
    fn ro_present_notifies_at_once() {
        let state = primed();
        let both = [movie(1, "The Godfather", 1972, true), movie(2, "Dune", 2024, true)];
        let (state, msgs) = step(Some(&state), Some(&both), Some(&ro(&[(2, true)])), 100.0);
        assert_eq!(msgs, ["Dune (2024) e gata de vizionat, cu subtitrare RO."]);
        assert_eq!(state["done"], json!(["1", "2"]));
        let (_s, msgs) = step(Some(&state), Some(&[movie(2, "Dune", 2024, true)]), Some(&ro(&[(2, true)])), 200.0);
        assert!(msgs.is_empty());
    }

    // py: test_notify.TestMovieReady.test_ro_missing_waits_then_notifies
    #[test]
    fn ro_missing_waits_then_notifies() {
        let state = primed();
        let movies = [movie(1, "The Godfather", 1972, true), movie(2, "Dune", 2024, true)];
        let (state, msgs) = step(Some(&state), Some(&movies), Some(&ro(&[(2, false)])), 1000.0);
        assert_eq!((msgs.len(), state["pending"].clone()), (0, json!({"2": 1000.0})));
        let (state, msgs) = step(Some(&state), Some(&movies), Some(&ro(&[])), 1000.0 + WAIT - 1.0);
        assert!(msgs.is_empty());
        let (state, msgs) = step(Some(&state), Some(&movies), Some(&ro(&[(2, false)])), 1000.0 + WAIT);
        assert_eq!(msgs, ["Dune (2024) e gata de vizionat. Subtitrarea RO încă lipsește."]);
        assert_eq!(state["pending"], json!({}));
        assert!(step(Some(&state), Some(&movies), Some(&ro(&[(2, true)])), 5.0 * H).1.is_empty());
    }

    // py: test_notify.TestMovieReady.test_ro_arrives_during_the_wait
    #[test]
    fn ro_arrives_during_the_wait() {
        let state = primed();
        let movies = [movie(2, "Dune", 2024, true)];
        let (state, _) = step(Some(&state), Some(&movies), Some(&ro(&[(2, false)])), 0.0);
        let (_s, msgs) = step(Some(&state), Some(&movies), Some(&ro(&[(2, true)])), 300.0);
        assert_eq!(msgs, ["Dune (2024) e gata de vizionat, cu subtitrare RO."]);
    }

    // py: test_notify.TestMovieReady.test_bazarr_down_waits_full_time_generic_text
    #[test]
    fn bazarr_down_waits_full_time_generic_text() {
        let state = primed();
        let movies = [movie(2, "Dune", 2024, true)];
        let (state, msgs) = step(Some(&state), Some(&movies), None, 0.0);
        assert!(msgs.is_empty());
        let (_s, msgs) = step(Some(&state), Some(&movies), None, WAIT);
        assert_eq!(msgs, ["Dune (2024) e gata de vizionat."]);
    }

    // py: test_notify.TestMovieReady.test_no_year
    #[test]
    fn no_year() {
        assert_eq!(movie_ready_text(&json!({"title": "X"}), None), "X e gata de vizionat.");
    }

    // py: test_notify.TestMovieReady.test_flood_is_recorded_not_sent
    #[test]
    fn flood_is_recorded_not_sent() {
        let state = primed();
        let movies: Vec<Value> = (10..30).map(|i| movie(i, &format!("M{i}"), 2000, true)).collect();
        let all: Vec<(i64, bool)> = (10..30).map(|i| (i, true)).collect();
        let (state, msgs) = step(Some(&state), Some(&movies), Some(&ro(&all)), 0.0);
        assert!(msgs.is_empty());
        assert!(state["done"].as_array().unwrap().contains(&json!("29")));
    }

    // py: test_notify.TestMovieReady.test_no_duplicates_across_a_reload_of_the_state_file
    #[test]
    fn no_duplicates_across_a_reload_of_the_state_file() {
        let d = testutil::temp_dir("state");
        let sf = crate::state::StateFile::new(&d.join("state.local.json"));
        assert!(sf.load().is_empty());
        let state = primed();
        let movies = [movie(1, "The Godfather", 1972, true), movie(2, "Dune", 2024, true), movie(3, "Past Lives", 2023, true)];
        let r = ro(&[(2, true), (3, false)]);
        let (state, msgs) = step(Some(&state), Some(&movies), Some(&r), 0.0);
        assert_eq!(msgs.len(), 1);
        let save = |s: &Value| {
            let mut m = Map::new();
            m.insert("movie_ready".into(), s.clone());
            sf.save(&m);
        };
        save(&state);
        let reloaded = crate::state::StateFile::new(sf.path()).load()["movie_ready"].clone();
        let (state, msgs) = step(Some(&reloaded), Some(&movies), Some(&r), 60.0);
        assert!(msgs.is_empty()); // Dune not repeated, Past Lives still waiting
        save(&state);
        let (_s, msgs) = step(Some(&sf.load()["movie_ready"]), Some(&movies), Some(&r), WAIT);
        assert_eq!(msgs, ["Past Lives (2023) e gata de vizionat. Subtitrarea RO încă lipsește."]);
        let _ = std::fs::remove_dir_all(&d);
    }

    // py: test_notify.TestMovieReady.test_broken_state_file_reads_empty
    #[test]
    fn broken_state_file_reads_empty() {
        let d = testutil::temp_dir("state");
        let p = d.join("state.local.json");
        std::fs::write(&p, "{not json").unwrap();
        assert!(crate::state::StateFile::new(&p).load().is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    // py: test_notify.TestDiskLow.test_hysteresis
    #[test]
    fn hysteresis() {
        let mut low = false;
        let mut seq = Vec::new();
        for free in [Some(70.0), Some(55.0), Some(50.0), Some(65.0), Some(70.0), Some(71.0), Some(59.0), None, Some(58.0)] {
            let (l, warn) = disk_low_step(low, free, 60.0);
            low = l;
            seq.push(warn);
        }
        assert_eq!(seq, [false, true, false, false, false, false, true, false, false]);
    }

    // py: test_notify.TestDiskLow.test_text
    #[test]
    fn text() {
        assert_eq!(disk_low_text("D:", 55.9), "Mai ai 55 GB liberi pe D:. Un film 4K are 20–30 GB.");
    }

    // py: test_notify.TestDiskLow.test_drive_of
    #[test]
    fn drive_of_paths() {
        assert_eq!(drive_of(Some(&json!("d:\\Movies"))).as_deref(), Some("D:"));
        assert_eq!(drive_of(Some(&json!("\\\\nas\\media\\Movies"))).as_deref(), Some("\\\\nas\\media"));
        assert_eq!(drive_of(Some(&json!("/movies"))), None);
        assert_eq!(drive_of(None), None);
        assert_eq!(drive_of(Some(&json!("\\\\nas\\media"))).as_deref(), Some("\\\\nas\\media"));
        assert_eq!(drive_of(Some(&json!("D:"))).as_deref(), Some("D:"));
    }

    /// A fake Radarr (`movies`, `roots`) and Bazarr (`bazarr`) whose answers tests can change.
    pub struct Upstreams {
        pub radarr: Fake,
        pub bazarr: Fake,
        pub movies: Arc<std::sync::Mutex<Value>>,
        pub subs: Arc<std::sync::Mutex<Value>>,
    }

    pub async fn upstreams() -> Upstreams {
        let movies = Arc::new(std::sync::Mutex::new(json!([])));
        let subs = Arc::new(std::sync::Mutex::new(json!([])));
        let m2 = movies.clone();
        let radarr = serve(Arc::new(move |h| {
            if h.path.starts_with("/api/v3/rootfolder") {
                json_answer(&json!([{"path": "D:\\Movies"}]))
            } else if h.path.starts_with("/api/v3/movie") {
                json_answer(&m2.lock().unwrap())
            } else {
                testutil::status_answer(404)
            }
        }))
        .await;
        let s2 = subs.clone();
        let bazarr = serve(Arc::new(move |_| json_answer(&json!({"data": s2.lock().unwrap().clone()})))).await;
        Upstreams { radarr, bazarr, movies, subs }
    }

    /// The `AppCase` of test_notify.py: a dry-run app on a temp config whose ntfy server is
    /// a local fake (dry-run still posts to a loopback server).
    pub async fn app_case(up: &Upstreams, ntfy: &Fake) -> (Arc<AppState>, Arc<crate::config::Config>) {
        let cfg = testutil::config(json!({
            "notifications": {"topic": TOPIC, "server": ntfy.url},
            "services": {"radarr": {"url": up.radarr.url}, "bazarr": {"url": up.bazarr.url}},
        }));
        (testutil::app(cfg.clone()), cfg)
    }

    pub async fn fake_ntfy() -> Fake {
        serve(Arc::new(|_| json_answer(&json!({})))).await
    }

    // py: test_notify.TestAppNotify.test_state_file_next_to_config
    #[tokio::test]
    async fn state_file_next_to_config() {
        let (up, ntfy) = (upstreams().await, fake_ntfy().await);
        let (app, cfg) = app_case(&up, &ntfy).await;
        assert_eq!(app.state_file.path(), cfg.dir().join("state.local.json"));
    }

    // py: test_notify.TestAppNotify.test_tick_baseline_then_ready_then_no_repeat_after_restart
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tick_baseline_then_ready_then_no_repeat_after_restart() {
        let (up, ntfy) = (upstreams().await, fake_ntfy().await);
        let (app, cfg) = app_case(&up, &ntfy).await;
        let free = |_: &str| Some(500.0);
        *up.movies.lock().unwrap() = json!([movie(1, "The Godfather", 1972, true)]);
        assert!(notify_tick_with(&app, 0.0, &free).await.is_empty());
        up.movies.lock().unwrap().as_array_mut().unwrap().push(movie(2, "Dune: Part Two", 2024, true));
        *up.subs.lock().unwrap() = json!([{"radarrId": 2, "subtitles": [{"code2": "ro"}], "missing_subtitles": []}]);
        app.services.radarr.clear_cache();
        app.services.bazarr.clear_cache();
        let sent = notify_tick_with(&app, 60.0, &free).await;
        let firsts: Vec<(&str, &str)> = sent.iter().map(|s| (s.0.as_str(), s.1.as_str())).collect();
        assert_eq!(firsts, [("Film gata", "Dune: Part Two (2024) e gata de vizionat, cu subtitrare RO.")]);
        // A new app on the same config folder (a restart) does not repeat it.
        let again = testutil::app(cfg.clone());
        assert!(notify_tick_with(&again, 120.0, &free).await.is_empty());
        let titles: Vec<String> = ntfy.hits().iter().map(|h| h.json()["title"].as_str().unwrap().to_string()).collect();
        assert_eq!(titles, ["Film gata"]);
        assert_eq!(ntfy.hits()[0].json()["tags"], json!(["movie_camera"]));
    }

    // py: test_notify.TestAppNotify.test_tick_disk_low_once_on_radarr_drive
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tick_disk_low_once_on_radarr_drive() {
        let (up, ntfy) = (upstreams().await, fake_ntfy().await);
        let (app, _cfg) = app_case(&up, &ntfy).await;
        let drives = std::sync::Mutex::new(Vec::new());
        let free = |d: &str| {
            drives.lock().unwrap().push(d.to_string());
            Some(55.4)
        };
        let sent = notify_tick_with(&app, 0.0, &free).await;
        assert!(notify_tick_with(&app, 60.0, &free).await.is_empty());
        assert_eq!(*drives.lock().unwrap(), ["D:", "D:"]);
        assert_eq!(
            sent,
            vec![(
                "Spațiu puțin".to_string(),
                "Mai ai 55 GB liberi pe D:. Un film 4K are 20–30 GB.".to_string(),
                vec!["warning".to_string()],
                Some("high".to_string())
            )]
        );
        let got = ntfy.hits()[0].json();
        assert_eq!((got["title"].clone(), got["priority"].clone()), (json!("Spațiu puțin"), json!(4)));
        assert_eq!(app.state_file.load()["disk_low"], json!({"D:": true}));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tick_with_every_service_down_keeps_the_state() {
        let ntfy = fake_ntfy().await;
        let cfg = testutil::config(json!({"notifications": {"topic": TOPIC, "server": ntfy.url}}));
        let app = testutil::app(cfg);
        assert!(notify_tick_with(&app, 0.0, &|_| None).await.is_empty());
        let st = app.state_file.load();
        assert_eq!(st["movie_ready"], json!({"primed": false, "done": [], "pending": {}}));
        assert_eq!(st["disk_low"], json!({"D:": false}));
        assert!(ntfy.hits().is_empty());
    }

    /// `TestRoutes`: the new endpoints over the real listener code, logged in.
    async fn routes_case() -> (Arc<AppState>, String, String, testutil::FakePlex, Fake) {
        let plex = testutil::fake_plex().await;
        let ntfy = fake_ntfy().await;
        let cfg = testutil::config(json!({
            "notifications": {"topic": TOPIC, "server": ntfy.url},
            "services": {"plex": {"url": plex.fake.url}},
        }));
        cfg.set_pin_hash(&crate::auth::pin::hash_pin("1234", 1000)).unwrap();
        let app = testutil::app(cfg);
        let base = testutil::serve_app(app.clone()).await;
        let tok = app.new_session(None);
        (app, base, tok, plex, ntfy)
    }

    fn tv(key: &str, title: &str, progress: f64) -> Value {
        testutil::session(key, title, progress, "192.168.0.40", "playing")
    }

    // py: test_notify.TestRoutes.test_auth_and_csrf_required
    #[tokio::test]
    async fn auth_and_csrf_required() {
        let (_app, base, tok, _plex, ntfy) = routes_case().await;
        for path in ["/api/notify/test", "/api/power/after-movie", "/api/power/after-movie/cancel"] {
            assert_eq!(testutil::call(&base, "POST", path, None, true).await.0, 401, "{path}");
            assert_eq!(testutil::call(&base, "POST", path, Some(&tok), false).await.0, 403, "{path}");
        }
        assert!(ntfy.hits().is_empty());
    }

    // py: test_notify.TestRoutes.test_status_has_topic_when_logged_in
    #[tokio::test]
    async fn status_has_topic_when_logged_in() {
        let (_app, base, tok, _plex, _ntfy) = routes_case().await;
        let (code, body) = testutil::call_raw(&base, "GET", "/api/status", None, false).await;
        assert_eq!(code, 401);
        assert!(crate::http::security::find(&body, TOPIC.as_bytes()).is_none());
        let (code, body) = testutil::call(&base, "GET", "/api/status", Some(&tok), false).await;
        assert_eq!((code, body["notifications"]["topic"].clone(), body["power"].clone()), (200, json!(TOPIC), Value::Null));
        assert_eq!(body["notifications"]["enabled"], json!(true));
    }

    // py: test_notify.TestRoutes.test_notify_test_reaches_the_server
    #[tokio::test]
    async fn notify_test_reaches_the_server() {
        let (_app, base, tok, _plex, ntfy) = routes_case().await;
        assert_eq!(testutil::call(&base, "POST", "/api/notify/test", Some(&tok), true).await, (200, json!({"ok": true})));
        let got = ntfy.hits()[0].clone();
        assert_eq!(got.json(), json!({"topic": TOPIC, "title": "Plex Couch", "message": "Test Plex Couch: notificările merg."}));
        assert!(crate::http::security::find(&got.body, "notificările".as_bytes()).is_some());
    }

    // py: test_notify.TestRoutes.test_notify_test_without_topic
    #[tokio::test]
    async fn notify_test_without_topic() {
        let (app, base, tok, _plex, _ntfy) = routes_case().await;
        let mut n = app.config.get("notifications").unwrap();
        n["topic"] = Value::Null;
        app.config.set_in_memory("notifications", n);
        let (code, body) = testutil::call(&base, "POST", "/api/notify/test", Some(&tok), true).await;
        assert_eq!(code, 409);
        assert!(body["error"].as_str().unwrap().contains("--setup-ntfy"));
    }

    // py: test_notify.TestRoutes.test_after_movie_nothing_playing_409
    #[tokio::test]
    async fn after_movie_nothing_playing_409() {
        let (app, base, tok, plex, _ntfy) = routes_case().await;
        plex.set_sessions(&[testutil::session("1", "The Godfather", 10.0, "127.0.0.1", "playing")]);
        let r = testutil::call(&base, "POST", "/api/power/after-movie", Some(&tok), true).await;
        assert_eq!(r, (409, json!({"error": "Nu rulează nimic pe TV acum."})));
        assert_eq!(crate::logic::power::power_status(&app), Value::Null);
    }

    // py: test_notify.TestRoutes.test_after_movie_arm_and_cancel
    #[tokio::test]
    async fn after_movie_arm_and_cancel() {
        let (app, base, tok, plex, _ntfy) = routes_case().await;
        plex.set_sessions(&[tv("5", "The Godfather", 20.0)]);
        let (code, body) = testutil::call(&base, "POST", "/api/power/after-movie", Some(&tok), true).await;
        assert_eq!(code, 200);
        assert_eq!(body["power"]["after_movie"]["title"], json!("The Godfather"));
        assert!(body["power"].get("kind").is_none());
        let r = testutil::call(&base, "POST", "/api/power/after-movie/cancel", Some(&tok), true).await;
        assert_eq!(r, (200, json!({"ok": true, "power": null})));
        assert!(app.actions.pending().is_none());
    }

    // py: test_notify.TestStaticUi.test_ui_texts
    #[test]
    fn ui_texts() {
        let html = include_str!("../../static/index.html");
        let js = include_str!("../../static/app.js");
        for text in ["Oprește după film", "Nu mai opri", "Notificări", "Abonează-te în ntfy", "Copiază numele",
                     "Trimite un test", "Pornește notificările cu plex-couch --setup-ntfy."] {
            assert!(html.contains(text), "{text}");
        }
        for text in ["PC-ul se oprește după film.", "Oprirea după film a fost anulată.",
                     "Notificarea de test a fost trimisă.", "Dacă îl oprești mai devreme, PC-ul rămâne pornit.",
                     "/api/power/after-movie", "/api/power/after-movie/cancel", "/api/notify/test"] {
            assert!(js.contains(text), "{text}");
        }
        let rx = regex::Regex::new(r"plexcouch-[A-Za-z0-9_-]{20,}").unwrap();
        assert!(!rx.is_match(&format!("{html}{js}")));
        // New: the seeding row of a torrent kept until it is watched
        assert!(js.contains("gata, aștept să-l vezi") && js.contains("waiting_watch"));
    }
}
