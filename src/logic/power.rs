//! Shut down after the movie, TV sessions and auto monitors off (spec 6.4, 6.5;
//! logic.py:806-905, app.py:320-384).
//!
//! Sessions are the parsed Plex sessions as JSON objects (`parse_plex_sessions` output:
//! `key`, `title`, `state`, `progress`, `address`, ...).
//!
//! Done (Phase 1, needed by the ported security tests and the routes): the TV/local player
//! rules, `tv_sessions`, `auto_monitor_decision`, arm/step/public after-movie, the
//! `power_status` shape, the arm/cancel route flows and `after_movie_poll`.
//! TODO(Phase 2a): [`spawn_auto_monitor`] (the poll loop, §6.5) and its tests in
//! test_logic.TestAutoMonitor / TestAfterMovie / TestAppNotify / TestRoutes.

use crate::app::AppState;
use crate::http::security::{is_loopback_str, is_private_client};
use crate::jobs::Stop;
use crate::log_info;
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

/// One successful Plex poll while armed: `(new state, action)` (logic.py:863-893).
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

/// The `auto-monitor` loop (§6.5): poll Plex now and then every `max(3, plex_poll_seconds)`
/// seconds; auto monitors off + `after_movie_poll` on every successful poll.
///
/// TODO(Phase 2a): implement. Not started in demo (jobs::spawn_all checks).
pub fn spawn_auto_monitor(app: Arc<AppState>, stop: Stop) {
    let _ = (app, stop);
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
}
