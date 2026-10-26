//! Shut down after the movie, TV sessions and auto monitors off (spec 6.4, 6.5).
//!
//! Sessions are the parsed Plex sessions as JSON objects (`parse_plex_sessions` output:
//! `key`, `title`, `state`, `progress`, `address`, ...).
//!
//! The TV/local player rules, `tv_sessions`, `auto_monitor_decision`, arm/step/public
//! after-movie, the `power_status` shape, the arm/cancel route flows, `after_movie_poll` and
//! the `auto-monitor` poll loop ([`spawn_auto_monitor`], spec 6.5; plan fix 4: `primed` is
//! set only after a successful poll).

use crate::app::AppState;
use crate::http::security::{is_loopback_str, is_private_client};
use crate::jobs::Stop;
use crate::{log_debug, log_info};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;

pub const AFTER_MOVIE_DONE_PCT: f64 = 90.0;
pub const AFTER_MOVIE_IDLE_S: f64 = 3.0 * 3600.0;

/// The armed state (memory only).
#[derive(Clone, Debug, PartialEq)]
pub struct AfterMovie {
    pub key: Option<String>,
    pub title: String,
    pub since: f64,
    pub progress: f64,
    pub seen: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterMovieAction {
    Fire,
    Expire,
}

fn s_str<'a>(s: &'a Value, k: &str) -> Option<&'a str> {
    s.get(k).and_then(Value::as_str)
}

/// `str(x)` of a session key (Plex keys are strings; tolerate numbers).
fn s_key(s: &Value) -> Option<String> {
    match s.get("key") {
        Some(Value::String(k)) if !k.is_empty() => Some(k.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn s_progress(s: &Value) -> f64 {
    s.get("progress").and_then(Value::as_f64).unwrap_or(0.0)
}

const LOCAL_ADDRESSES: [&str; 4] = ["127.0.0.1", "::1", "localhost", "::ffff:127.0.0.1"];

/// A Plex player on this PC: loopback, `localhost` or one of our own IPv4s.
pub fn is_local_player(address: Option<&str>, own_ips: &HashSet<String>) -> bool {
    let Some(a) = address.filter(|a| !a.is_empty()) else { return false };
    let a = a.trim().to_lowercase();
    LOCAL_ADDRESSES.contains(&a.as_str()) || own_ips.contains(&a) || is_loopback_str(&a)
}

/// The TV at home: a private/LAN address that is not this PC. Remote viewers never count.
pub fn is_tv_player(address: Option<&str>, own_ips: &HashSet<String>) -> bool {
    let Some(addr) = address.filter(|a| !a.is_empty()) else { return false };
    if is_local_player(Some(addr), own_ips) {
        return false;
    }
    let a = addr.trim();
    is_private_client(Some(a)) && !is_local_player(a.split('%').next(), own_ips)
}

/// Sessions on the TV (with a key).
pub fn tv_sessions(sessions: &[Value], own_ips: &HashSet<String>) -> Vec<Value> {
    sessions
        .iter()
        .filter(|s| s_key(s).is_some() && is_tv_player(s_str(s, "address"), own_ips))
        .cloned()
        .collect()
}

/// Auto monitors off: `(new handled set, trigger)`. A session is decided the first time it
/// is seen playing; it triggers only if enabled and it plays on the TV.
pub fn auto_monitor_decision(
    handled: &HashSet<String>,
    sessions: &[Value],
    own_ips: &HashSet<String>,
    enabled: bool,
) -> (HashSet<String>, bool) {
    let mut handled = handled.clone();
    let mut current = HashSet::new();
    let mut trigger = false;
    for s in sessions {
        let Some(key) = s_key(s) else { continue };
        current.insert(key.clone());
        if handled.contains(&key) {
            continue;
        }
        if s_str(s, "state") == Some("playing") {
            handled.insert(key);
            if enabled && is_tv_player(s_str(s, "address"), own_ips) {
                trigger = true;
            }
        }
    }
    (handled.intersection(&current).cloned().collect(), trigger)
}

/// The session to follow: the same title if it is back, else one that plays, else any.
pub fn pick_session<'a>(sessions: &'a [Value], title: Option<&str>) -> Option<&'a Value> {
    let mut pool: Vec<&Value> = sessions.iter().collect();
    if let Some(t) = title.filter(|t| !t.is_empty()) {
        let same: Vec<&Value> = pool.iter().copied().filter(|s| s_str(s, "title") == Some(t)).collect();
        if !same.is_empty() {
            pool = same;
        }
    }
    pool.iter().copied().find(|s| s_str(s, "state") == Some("playing")).or_else(|| pool.first().copied())
}

/// Arm on what plays on the TV; None if nothing does.
pub fn after_movie_arm(sessions: &[Value], own_ips: &HashSet<String>, now: f64) -> Option<AfterMovie> {
    let tv = tv_sessions(sessions, own_ips);
    let s = pick_session(&tv, None)?;
    Some(AfterMovie {
        key: s_key(s),
        title: s_str(s, "title").unwrap_or("").to_string(),
        since: now,
        progress: s_progress(s),
        seen: now,
    })
}

/// One successful Plex poll while armed: `(new state, action)`.
pub fn after_movie_step(
    state: Option<&AfterMovie>,
    sessions: &[Value],
    own_ips: &HashSet<String>,
    now: f64,
) -> (Option<AfterMovie>, Option<AfterMovieAction>) {
    let Some(state) = state else { return (None, None) };
    let mut st = state.clone();
    let tv = tv_sessions(sessions, own_ips);
    if let Some(key) = st.key.clone() {
        if let Some(cur) = tv.iter().find(|s| s_key(s).as_deref() == Some(key.as_str())) {
            st.progress = s_progress(cur);
            st.title = s_str(cur, "title").filter(|t| !t.is_empty()).map(String::from).unwrap_or(st.title);
            st.seen = now;
            return (Some(st), None);
        }
        if st.progress >= AFTER_MOVIE_DONE_PCT {
            return (None, Some(AfterMovieAction::Fire));
        }
        st.key = None; // stopped early: stay armed, wait for the next session
        st.seen = now;
    }
    if let Some(next) = pick_session(&tv, Some(&st.title)) {
        st.key = s_key(next);
        st.title = s_str(next, "title").filter(|t| !t.is_empty()).map(String::from).unwrap_or(st.title);
        st.progress = s_progress(next);
        st.seen = now;
        return (Some(st), None);
    }
    if now - st.seen >= AFTER_MOVIE_IDLE_S {
        return (None, Some(AfterMovieAction::Expire));
    }
    (Some(st), None)
}

/// The armed state as `/api/status` shows it.
pub fn public_after_movie(state: Option<&AfterMovie>) -> Value {
    match state {
        None => Value::Null,
        Some(s) => json!({"armed": true, "title": s.title, "since": s.since as i64}),
    }
}

/// `power_status()`: null, or the pending countdown (`kind`, `at`) plus `after_movie`.
pub fn power_status(app: &AppState) -> Value {
    let pending = app.actions.pending();
    let after = {
        let g = app.after_movie.lock().unwrap_or_else(|e| e.into_inner());
        public_after_movie(g.as_ref())
    };
    if pending.is_none() && after.is_null() {
        return Value::Null;
    }
    let mut out = match pending {
        Some(p) => p.to_json(),
        None => json!({}),
    };
    out["after_movie"] = after;
    out
}

/// Plex sessions now (demo: the fake TV session).
async fn sessions_now(app: &Arc<AppState>) -> Result<Vec<Value>, crate::services::UpstreamError> {
    if app.demo {
        return Ok(crate::logic::status::demo_sessions(app));
    }
    app.services.plex.sessions().await
}

/// POST /api/power/after-movie: `(status, body)`.
pub async fn arm_after_movie(app: &Arc<AppState>) -> (u16, Value) {
    if app.actions.pending().is_some() {
        return (409, json!({"error": "Există deja o oprire programată."}));
    }
    let sessions = match sessions_now(app).await {
        Ok(s) => s,
        Err(_) => return (503, json!({"error": "Plex nu răspunde. Repornește PC-ul sau verifică serviciul."})),
    };
    let Some(state) = after_movie_arm(&sessions, &app.own_ips(), crate::auth::session::now()) else {
        return (409, json!({"error": "Nu rulează nimic pe TV acum."}));
    };
    *app.after_movie.lock().unwrap_or_else(|e| e.into_inner()) = Some(state);
    log_info!("after-movie shutdown armed");
    (200, json!({"ok": true, "power": power_status(app)}))
}

/// POST /api/power/after-movie/cancel: disarm; returns the new `power_status`.
pub fn cancel_after_movie(app: &AppState) -> Value {
    let was = app.after_movie.lock().unwrap_or_else(|e| e.into_inner()).take().is_some();
    if was {
        log_info!("after-movie shutdown disarmed");
    }
    power_status(app)
}

/// Called with every successful Plex poll: starts the normal 60 s shutdown once.
pub fn after_movie_poll(app: &Arc<AppState>, sessions: &[Value], now: f64) -> Option<AfterMovieAction> {
    let action = {
        let mut g = app.after_movie.lock().unwrap_or_else(|e| e.into_inner());
        g.as_ref()?;
        let (st, action) = after_movie_step(g.as_ref(), sessions, &app.own_ips(), now);
        *g = st;
        action
    };
    match action {
        Some(AfterMovieAction::Expire) => log_info!("after-movie shutdown expired (nothing played for 3 h)"),
        Some(AfterMovieAction::Fire) => {
            log_info!("after-movie: the movie ended, shutting down");
            if app.actions.power("shutdown").is_some() {
                app.notifier.send_async(
                    "Oprire după film",
                    "PC-ul se oprește în 60 de secunde. Anulează din Plex Couch.",
                    None,
                    Some(json!("high")),
                );
            }
        }
        None => {}
    }
    action
}

/// The `auto-monitor` loop state.
#[derive(Default, Debug)]
pub struct AutoMonitor {
    pub handled: HashSet<String>,
    /// Set after the first *successful* poll (plan fix 4): until then nothing triggers, so
    /// what already plays at startup never turns the monitors off.
    pub primed: bool,
}

/// One poll of the loop: auto monitors off, then the after-movie step. Returns true when
/// the monitors were turned off.
pub fn auto_monitor_tick(
    app: &Arc<AppState>,
    st: &mut AutoMonitor,
    polled: Result<Vec<Value>, crate::services::UpstreamError>,
    now: f64,
) -> bool {
    let sessions = match polled {
        Ok(s) => s,
        Err(e) => {
            log_debug!("auto-monitor poll failed: {}", e.type_name());
            return false;
        }
    };
    // The first successful poll only records what is already playing.
    let enabled = st.primed && app.config.setting("auto_monitors_off");
    let (handled, trigger) = auto_monitor_decision(&st.handled, &sessions, &app.own_ips(), enabled);
    st.handled = handled;
    st.primed = true;
    after_movie_poll(app, &sessions, now);
    if trigger {
        crate::system::monitors::monitors_off(app.dry_run, "auto: film pornit pe TV");
    }
    trigger
}

/// `max(3, int(plex_poll_seconds or 10))` seconds.
pub fn poll_interval(app: &AppState) -> std::time::Duration {
    let v = app.config.get("plex_poll_seconds");
    let n = if crate::json::truthy(v.as_ref()) { v.as_ref().and_then(crate::json::py_int).unwrap_or(10) } else { 10 };
    std::time::Duration::from_secs(n.max(3) as u64)
}

/// The `auto-monitor` loop (§6.5): poll Plex now and then every [`poll_interval`]; auto
/// monitors off + `after_movie_poll` on every successful poll. Not started in demo
/// (jobs::spawn_all checks).
pub fn spawn_auto_monitor(app: Arc<AppState>, mut stop: Stop) {
    tokio::spawn(async move {
        let mut st = AutoMonitor::default();
        let mut first = true;
        loop {
            let wait = if first { std::time::Duration::ZERO } else { poll_interval(&app) };
            if stop.sleep(wait).await {
                break;
            }
            first = false;
            let polled = app.services.plex.sessions().await;
            let tick = std::panic::AssertUnwindSafe(|| auto_monitor_tick(&app, &mut st, polled, crate::auth::session::now()));
            if std::panic::catch_unwind(tick).is_err() {
                crate::log_error!("after-movie check failed");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own() -> HashSet<String> {
        ["192.168.0.10".to_string()].into_iter().collect()
    }

    // py: test_security.TestTvSessions.test_is_tv_player
    #[test]
    fn tv_player() {
        for a in ["192.168.0.40", "10.0.0.129", "172.16.5.5", "fd00::5", "::ffff:192.168.0.40"] {
            assert!(is_tv_player(Some(a), &own()), "{a}");
        }
        for a in ["192.168.0.10", "127.0.0.1", "::1", "localhost", "", "8.8.8.8", "2a00:1450:4001::1",
                  "85.120.10.1", "::ffff:8.8.8.8", "garbage"] {
            assert!(!is_tv_player(Some(a), &own()), "{a}");
        }
        assert!(!is_tv_player(None, &own()));
    }

    // py: test_security.TestTvSessions.test_remote_viewers_are_not_the_tv
    #[test]
    fn remote_viewers_are_not_the_tv() {
        let remote = json!({"key": "1", "state": "playing", "address": "85.120.10.1", "title": "X", "progress": 99});
        let tv = json!({"key": "2", "state": "playing", "address": "192.168.0.40", "title": "Y", "progress": 10});
        assert_eq!(tv_sessions(&[remote.clone(), tv.clone()], &own()), vec![tv.clone()]);
        let (_h, trig) = auto_monitor_decision(&HashSet::new(), std::slice::from_ref(&remote), &own(), true);
        assert!(!trig);
        let (_h, trig) = auto_monitor_decision(&HashSet::new(), std::slice::from_ref(&tv), &own(), true);
        assert!(trig);
        assert!(after_movie_arm(std::slice::from_ref(&remote), &own(), 0.0).is_none());
        let st = after_movie_arm(&[remote, tv], &own(), 0.0).unwrap();
        assert_eq!(st.key.as_deref(), Some("2"));
    }

    use crate::logic::status::public_session;
    use crate::logic::testutil::{self, fake_plex, session};

    const H: f64 = 3600.0;

    fn own131() -> HashSet<String> {
        ["10.0.0.131".to_string()].into_iter().collect()
    }

    fn s(key: &str, state: &str, addr: &str) -> Value {
        json!({"key": key, "state": state, "address": addr})
    }

    fn keys(h: &HashSet<String>) -> Vec<String> {
        let mut v: Vec<String> = h.iter().cloned().collect();
        v.sort();
        v
    }

    // py: test_logic.TestAutoMonitor.test_new_remote_playing_triggers_once
    #[test]
    fn new_remote_playing_triggers_once() {
        let (h, trig) = auto_monitor_decision(&HashSet::new(), &[s("1", "playing", "10.0.0.129")], &own131(), true);
        assert!(trig);
        let (h, trig) = auto_monitor_decision(&h, &[s("1", "playing", "10.0.0.129")], &own131(), true);
        assert!(!trig); // the same session never re-triggers
        let (h, _) = auto_monitor_decision(&h, &[s("1", "paused", "10.0.0.129")], &own131(), true);
        let (_h, trig) = auto_monitor_decision(&h, &[s("1", "playing", "10.0.0.129")], &own131(), true);
        assert!(!trig);
    }

    // py: test_logic.TestAutoMonitor.test_buffering_then_playing
    #[test]
    fn buffering_then_playing() {
        let (h, trig) = auto_monitor_decision(&HashSet::new(), &[s("2", "buffering", "10.0.0.129")], &own131(), true);
        assert!(!trig);
        let (_h, trig) = auto_monitor_decision(&h, &[s("2", "playing", "10.0.0.129")], &own131(), true);
        assert!(trig);
    }

    // py: test_logic.TestAutoMonitor.test_local_player_skipped
    #[test]
    fn local_player_skipped() {
        for addr in ["127.0.0.1", "::1", "10.0.0.131"] {
            let (h, trig) = auto_monitor_decision(&HashSet::new(), &[s("3", "playing", addr)], &own131(), true);
            assert!(!trig, "{addr}");
            assert_eq!(keys(&h), ["3"]);
        }
    }

    // py: test_logic.TestAutoMonitor.test_disabled
    #[test]
    fn disabled() {
        let (h, trig) = auto_monitor_decision(&HashSet::new(), &[s("4", "playing", "10.0.0.129")], &own131(), false);
        assert!(!trig);
        // enabling later does not fire for an already-seen session
        let (_h, trig) = auto_monitor_decision(&h, &[s("4", "playing", "10.0.0.129")], &own131(), true);
        assert!(!trig);
    }

    // py: test_logic.TestAutoMonitor.test_ended_sessions_forgotten
    #[test]
    fn ended_sessions_forgotten() {
        let (h, _) = auto_monitor_decision(&HashSet::new(), &[s("5", "playing", "10.0.0.129")], &own131(), true);
        let (h, _) = auto_monitor_decision(&h, &[], &own131(), true);
        assert!(h.is_empty());
        let (_h, trig) = auto_monitor_decision(&h, &[s("6", "playing", "10.0.0.129")], &own131(), true);
        assert!(trig);
    }

    /// The test TV at 192.168.0.40, playing "The Godfather".
    fn tv(key: &str, progress: f64) -> Value {
        session(key, "The Godfather", progress, "192.168.0.40", "playing")
    }

    fn arm(sessions: &[Value], now: f64) -> Option<AfterMovie> {
        after_movie_arm(sessions, &own(), now)
    }

    fn step(st: Option<&AfterMovie>, sessions: &[Value], now: f64) -> (Option<AfterMovie>, Option<AfterMovieAction>) {
        after_movie_step(st, sessions, &own(), now)
    }

    // py: test_notify.TestAfterMovie.test_nothing_playing_on_the_tv
    #[test]
    fn nothing_playing_on_the_tv() {
        assert!(arm(&[], 0.0).is_none());
        let local = [
            session("1", "The Godfather", 10.0, "127.0.0.1", "playing"),
            session("2", "The Godfather", 10.0, "192.168.0.10", "playing"),
        ];
        assert!(arm(&local, 0.0).is_none());
    }

    // py: test_notify.TestAfterMovie.test_arm_follows_the_tv_session
    #[test]
    fn arm_follows_the_tv_session() {
        let st = arm(&[session("9", "Local", 10.0, "127.0.0.1", "playing"), tv("5", 12.5)], 100.0).unwrap();
        assert_eq!((st.key.as_deref(), st.title.as_str(), st.since, st.progress), (Some("5"), "The Godfather", 100.0, 12.5));
        assert_eq!(public_after_movie(Some(&st)), json!({"armed": true, "title": "The Godfather", "since": 100}));
        assert_eq!(public_after_movie(None), Value::Null);
    }

    // py: test_notify.TestAfterMovie.test_ended_after_90_percent_fires_once
    #[test]
    fn ended_after_90_percent_fires_once() {
        let st = arm(&[tv("5", 10.0)], 0.0);
        let (st, act) = step(st.as_ref(), &[tv("5", 95.2)], 60.0);
        assert_eq!((act, st.as_ref().unwrap().progress), (None, 95.2));
        let (st, act) = step(st.as_ref(), &[], 70.0);
        assert_eq!((st.clone(), act), (None, Some(AfterMovieAction::Fire)));
        assert_eq!(step(st.as_ref(), &[], 80.0), (None, None));
    }

    // py: test_notify.TestAfterMovie.test_exactly_90_fires
    #[test]
    fn exactly_90_fires() {
        let st = arm(&[tv("5", 90.0)], 0.0);
        assert_eq!(step(st.as_ref(), &[], 10.0).1, Some(AfterMovieAction::Fire));
    }

    // py: test_notify.TestAfterMovie.test_stopped_early_does_not_fire_and_stays_armed
    #[test]
    fn stopped_early_does_not_fire_and_stays_armed() {
        let st = arm(&[tv("5", 40.0)], 0.0);
        let (st, _) = step(st.as_ref(), &[tv("5", 89.9)], 10.0);
        let (st, act) = step(st.as_ref(), &[], 20.0);
        assert_eq!(act, None);
        assert!(st.as_ref().unwrap().key.is_none());
        // The user starts it again (new session key): it is followed, and fires at the end.
        let (st, act) = step(st.as_ref(), &[tv("8", 89.95)], 30.0);
        assert_eq!((st.as_ref().unwrap().key.as_deref(), act), (Some("8"), None));
        let (st, _) = step(st.as_ref(), &[tv("8", 97.0)], 40.0);
        assert_eq!(step(st.as_ref(), &[], 50.0), (None, Some(AfterMovieAction::Fire)));
    }

    // py: test_notify.TestAfterMovie.test_same_title_preferred_when_it_comes_back
    #[test]
    fn same_title_preferred_when_it_comes_back() {
        let st = arm(&[tv("5", 30.0)], 0.0);
        let (st, _) = step(st.as_ref(), &[], 10.0);
        let both = [
            session("6", "Other", 10.0, "192.168.0.40", "playing"),
            session("7", "The Godfather", 10.0, "192.168.0.40", "paused"),
        ];
        let (st, _) = step(st.as_ref(), &both, 20.0);
        assert_eq!(st.unwrap().key.as_deref(), Some("7"));
    }

    // py: test_notify.TestAfterMovie.test_local_sessions_ignored
    #[test]
    fn local_sessions_ignored() {
        let st = arm(&[tv("5", 95.0)], 0.0);
        let (st, act) = step(st.as_ref(), &[session("5", "The Godfather", 95.0, "127.0.0.1", "playing")], 10.0);
        assert_eq!((st, act), (None, Some(AfterMovieAction::Fire)));
    }

    // py: test_notify.TestAfterMovie.test_expires_after_3_hours_with_nothing_on_the_tv
    #[test]
    fn expires_after_3_hours_with_nothing_on_the_tv() {
        let st = arm(&[tv("5", 30.0)], 0.0);
        let (st, _) = step(st.as_ref(), &[], 100.0); // stopped early at t=100
        let (st, act) = step(st.as_ref(), &[], 100.0 + 3.0 * H - 1.0);
        assert_eq!((act, st.is_some()), (None, true));
        assert_eq!(step(st.as_ref(), &[], 100.0 + 3.0 * H), (None, Some(AfterMovieAction::Expire)));
    }

    // py: test_notify.TestAfterMovie.test_long_movie_does_not_expire_while_it_plays
    #[test]
    fn long_movie_does_not_expire_while_it_plays() {
        let st = arm(&[tv("5", 1.0)], 0.0);
        let (st, act) = step(st.as_ref(), &[tv("5", 80.0)], 4.0 * H);
        assert_eq!(act, None);
        let (st, _) = step(st.as_ref(), &[tv("5", 99.0)], 4.0 * H + 60.0);
        assert_eq!(step(st.as_ref(), &[], 4.0 * H + 70.0), (None, Some(AfterMovieAction::Fire)));
    }

    const TOPIC: &str = "plexcouch-unit-test-topic-xyz";

    /// `AppCase`: a dry-run app with a fake Plex and a fake ntfy server.
    async fn app_case() -> (Arc<AppState>, testutil::FakePlex, testutil::Fake) {
        let plex = fake_plex().await;
        let ntfy = testutil::serve(Arc::new(|_| testutil::json_answer(&json!({})))).await;
        let cfg = testutil::config(json!({
            "notifications": {"topic": TOPIC, "server": ntfy.url},
            "settings": {"auto_monitors_off": true},
            "services": {"plex": {"url": plex.fake.url}},
        }));
        (testutil::app(cfg), plex, ntfy)
    }

    // py: test_notify.TestAppNotify.test_after_movie_poll_fires_power_once_and_notifies
    #[tokio::test]
    async fn after_movie_poll_fires_power_once_and_notifies() {
        let (app, plex, ntfy) = app_case().await;
        plex.set_sessions(&[tv("5", 50.0)]);
        assert_eq!(arm_after_movie(&app).await.0, 200);
        assert_eq!(after_movie_poll(&app, &[tv("5", 96.0)], 10.0), None);
        assert_eq!(after_movie_poll(&app, &[], 20.0), Some(AfterMovieAction::Fire));
        assert_eq!(after_movie_poll(&app, &[], 30.0), None);
        // power("shutdown") was called once: the (dry-run) countdown is pending
        assert_eq!(app.actions.pending().map(|p| p.kind), Some("shutdown".to_string()));
        testutil::wait_for(|| !ntfy.hits().is_empty()).await;
        let got = ntfy.hits().last().unwrap().json();
        assert_eq!(
            (got["title"].clone(), got["message"].clone()),
            (json!("Oprire după film"), json!("PC-ul se oprește în 60 de secunde. Anulează din Plex Couch."))
        );
        assert_eq!(got["priority"], json!(4));
        assert_eq!(power_status(&app)["after_movie"], Value::Null);
        app.actions.cancel_power();
    }

    // py: test_notify.TestAppNotify.test_after_movie_stopped_early_no_power
    #[tokio::test]
    async fn after_movie_stopped_early_no_power() {
        let (app, plex, _ntfy) = app_case().await;
        plex.set_sessions(&[tv("5", 50.0)]);
        arm_after_movie(&app).await;
        assert_eq!(after_movie_poll(&app, &[], 10.0), None);
        assert!(app.actions.pending().is_none());
        assert_eq!(power_status(&app)["after_movie"]["armed"], json!(true));
    }

    // py: test_notify.TestAppNotify.test_arm_errors
    #[tokio::test]
    async fn arm_errors() {
        let (app, plex, _ntfy) = app_case().await;
        assert_eq!(arm_after_movie(&app).await, (409, json!({"error": "Nu rulează nimic pe TV acum."})));
        plex.set_status(503);
        assert_eq!(arm_after_movie(&app).await.0, 503);
        plex.set_sessions(&[tv("5", 10.0)]);
        app.actions.power("shutdown").unwrap();
        assert_eq!(arm_after_movie(&app).await, (409, json!({"error": "Există deja o oprire programată."})));
        app.actions.cancel_power();
    }

    // py: test_notify.TestAppNotify.test_power_status_shape
    #[tokio::test]
    async fn power_status_shape() {
        let (app, _plex, _ntfy) = app_case().await;
        assert_eq!(power_status(&app), Value::Null);
        let p = app.actions.power("shutdown").unwrap();
        assert_eq!(power_status(&app), json!({"kind": "shutdown", "at": p.at, "after_movie": null}));
        let keys: Vec<String> = power_status(&app).as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["kind", "at", "after_movie"]);
        app.actions.cancel_power();
    }

    // New (plan fix 4): a failed first poll does not prime the loop, so a movie already
    // playing at the first successful poll never turns the monitors off.
    #[tokio::test]
    async fn failed_first_poll_does_not_prime() {
        crate::log::start_capture();
        let (app, _plex, _ntfy) = app_case().await;
        let mut st = AutoMonitor::default();
        assert!(!auto_monitor_tick(&app, &mut st, Err(crate::services::UpstreamError::Connect), 0.0));
        assert!(!st.primed);
        let playing = [json!({"key": "1", "state": "playing", "address": "192.168.0.40"})];
        assert!(!auto_monitor_tick(&app, &mut st, Ok(playing.to_vec()), 1.0));
        assert!(st.primed);
        assert!(!auto_monitor_tick(&app, &mut st, Ok(playing.to_vec()), 2.0));
        let more = [playing[0].clone(), json!({"key": "2", "state": "playing", "address": "192.168.0.40"})];
        assert!(auto_monitor_tick(&app, &mut st, Ok(more.to_vec()), 3.0));
        assert!(crate::log::captured().concat().contains("dry-run: monitors off (auto: film pornit pe TV)"));
        // the setting is read live each tick
        app.config.update_settings_in_memory(&[("auto_monitors_off", false)]);
        let third = [json!({"key": "3", "state": "playing", "address": "192.168.0.40"})];
        assert!(!auto_monitor_tick(&app, &mut st, Ok(third.to_vec()), 4.0));
    }

    #[tokio::test]
    async fn the_loop_polls_the_fake_plex_and_stops() {
        let (app, plex, _ntfy) = app_case().await;
        plex.set_sessions(&[tv("5", 10.0)]);
        let (h, stop) = crate::jobs::stop_pair();
        spawn_auto_monitor(app.clone(), stop);
        testutil::wait_for(|| plex.fake.paths().iter().any(|p| p == "/status/sessions")).await;
        let hit = plex.fake.hits()[0].clone();
        assert_eq!(hit.header("X-Plex-Token"), Some("plex-test-token"));
        assert_eq!(hit.header("Accept"), Some("application/json"));
        h.stop();
        assert_eq!(poll_interval(&app), std::time::Duration::from_secs(10));
        app.config.set_in_memory("plex_poll_seconds", json!(1));
        assert_eq!(poll_interval(&app), std::time::Duration::from_secs(3));
        // the sessions parse back to what the fake was given
        let parsed = app.services.plex.sessions().await.unwrap();
        assert_eq!(public_session(&parsed[0])["progress"], json!(10.0));
        assert_eq!(parsed[0]["address"], json!("192.168.0.40"));
    }
}
