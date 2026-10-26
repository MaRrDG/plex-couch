//! Notifications: movie ready, disk low, the test notification (spec 7).
//!
//! Done (Phase 1): the `POST /api/notify/test` flow ([`send_test_notification`]).
//! TODO(Phase 2a): `movie_ready_step`, `disk_low_step`, `disk_low_text`, `drive_of`,
//! [`notify_tick`] and [`spawn_notify_loop`] (first tick at once, then every 60 s, skipped
//! while the notifier is disabled; state written before sending), ported tests from
//! tests/test_notify.py.

use crate::app::AppState;
use crate::jobs::Stop;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub const NOTIFY_POLL_SECONDS: u64 = 60;
/// Below the page's 8 s request timeout.
pub const NOTIFY_TEST_TIMEOUT: Duration = Duration::from_secs(6);

/// POST /api/notify/test: `(status, body)`.
pub async fn send_test_notification(app: &Arc<AppState>) -> (u16, Value) {
    if !app.notifier.enabled() {
        return (409, json!({"error": "Notificările nu sunt pornite. Rulează app.py --setup-ntfy."}));
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

/// One round of the movie-ready and disk-low checks. Returns what was sent as
/// `(title, text, tags, priority)`.
///
/// TODO(Phase 2a): implement (app.py:410-445).
pub async fn notify_tick(app: &Arc<AppState>, now: f64) -> Vec<(String, String, Vec<String>, Option<String>)> {
    let _ = (app, now);
    Vec::new()
}

/// The `notify` loop.
///
/// TODO(Phase 2a): implement. Not started in demo (jobs::spawn_all checks).
pub fn spawn_notify_loop(app: Arc<AppState>, stop: Stop) {
    let _ = (app, stop);
}
