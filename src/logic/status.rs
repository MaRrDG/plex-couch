//! `GET /api/status` (spec 3.5) and the demo data (spec 8).
//!
//! Done (Phase 1): the route, `public_session`, `with_backdrop_fallback`, `public_playing`
//! (the `on_tv` flag), `notifications_status`, `demo_sessions`.
//! TODO(Phase 2a): [`status`] (the 7 upstream jobs with one shared 6 s deadline, then the
//! summaries; plan fix 14: per-upstream timeouts with cancellation; fix 17: a bad
//! `timeleft` becomes null for that item) and [`demo_status`] (demo.py:101-152).

use crate::app::AppState;
use crate::logic::images::poster_url;
use crate::logic::power::{is_tv_player, power_status};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::sync::Arc;

/// The full status JSON.
///
/// TODO(Phase 2a): replace the placeholder (every service down) with the real build.
pub async fn status(app: &Arc<AppState>) -> Value {
    if app.demo {
        let mut st = demo_status(app);
        st["notifications"] = notifications_status(app);
        return st;
    }
    json!({
        "app_name": app.config.app_name(),
        "server_time": crate::auth::session::now(),
        "dry_run": app.dry_run,
        "pc": crate::system::info::pc_info(&app.config),
        "services": {"plex": false, "radarr": false, "prowlarr": false, "bazarr": false, "qbittorrent": false},
        "now_playing": [],
        "downloads": null,
        "waiting": null,
        "library": null,
        "torrents": null,
        "hr_warnings": [],
        "power": power_status(app),
        "settings": app.config.settings(),
        "notifications": notifications_status(app),
    })
}

/// Demo status (fixed data, demo.py:101-152).
///
/// TODO(Phase 2a): implement with the real summarize / title-map / sort functions.
pub fn demo_status(app: &Arc<AppState>) -> Value {
    json!({
        "app_name": app.config.app_name(),
        "server_time": crate::auth::session::now(),
        "dry_run": true,
        "demo": true,
        "pc": {"hostname": "DEMO-PC", "uptime": 3 * 86400 + 4 * 3600, "platform": "win32", "disks": []},
        "services": {"plex": true, "radarr": true, "prowlarr": true, "bazarr": true, "qbittorrent": true},
        "now_playing": [],
        "downloads": [],
        "waiting": [],
        "library": [],
        "torrents": [],
        "hr_warnings": [],
        "power": power_status(app),
        "settings": app.config.settings(),
    })
}

/// The demo movie as an internal Plex session (for arming "shut down after the movie").
pub fn demo_sessions(app: &AppState) -> Vec<Value> {
    let played = 2843 + app.started.elapsed().as_secs() as i64;
    let progress = crate::json::py_round(played as f64 / 9960.0 * 100.0, 1);
    vec![json!({"key": "demo", "title": "Dune: Part Two", "year": 2024, "state": "playing",
                "progress": progress, "address": "10.0.0.20"})]
}

/// Only ever returned to logged-in clients; the topic is never logged.
pub fn notifications_status(app: &AppState) -> Value {
    let n = &app.notifier;
    let enabled = n.enabled();
    json!({"enabled": enabled, "topic": n.topic(), "server": if enabled { Value::from(n.server()) } else { Value::Null }})
}

/// Drop the internal fields (`address`, `key`).
pub fn public_session(s: &Value) -> Value {
    match s {
        Value::Object(m) => {
            let out: Map<String, Value> =
                m.iter().filter(|(k, _)| *k != "address" && *k != "key").map(|(k, v)| (k.clone(), v.clone())).collect();
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// No Plex art for a playing movie: use Radarr's fanart of the same movie (title casefold +
/// year).
pub fn with_backdrop_fallback(mut session: Value, movies: Option<&[Value]>) -> Value {
    let has = crate::json::truthy(session.get("backdrop"));
    let Some(movies) = movies.filter(|m| !m.is_empty()) else { return session };
    if has {
        return session;
    }
    let title = session.get("title").and_then(Value::as_str).unwrap_or("").to_lowercase();
    for m in movies {
        let mt = m.get("title").and_then(Value::as_str).unwrap_or("").to_lowercase();
        if mt == title && m.get("year") == session.get("year") {
            let id = match m.get("id") {
                Some(Value::Number(n)) => n.to_string(),
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            };
            session["backdrop"] = poster_url("movie-art", &id).map(Value::from).unwrap_or(Value::Null);
            break;
        }
    }
    session
}

/// A `now_playing` entry: public fields, backdrop fallback, `on_tv`.
pub fn public_playing(session: &Value, movies: Option<&[Value]>, own_ips: &HashSet<String>) -> Value {
    let mut out = with_backdrop_fallback(public_session(session), movies);
    out["on_tv"] = Value::Bool(is_tv_player(session.get("address").and_then(Value::as_str), own_ips));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // py: test_security.TestTvSessions.test_on_tv_flag_in_status
    #[test]
    fn on_tv_flag_in_status() {
        let own: HashSet<String> = ["192.168.0.10".to_string()].into_iter().collect();
        let remote = public_playing(&json!({"title": "X", "address": "85.120.10.1"}), Some(&[]), &own);
        let tv = public_playing(&json!({"title": "Y", "address": "192.168.0.40"}), Some(&[]), &own);
        assert_eq!((remote["on_tv"].clone(), tv["on_tv"].clone()), (json!(false), json!(true)));
        assert!(tv.get("address").is_none());
    }
}
