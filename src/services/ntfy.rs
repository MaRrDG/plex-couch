//! ntfy notifications (spec 7.1). The topic is the only secret: kept in the config,
//! returned only to logged-in clients, never logged.
//!
//! Done (Phase 1): topic/server/enabled, [`new_topic`], [`subscribe_url`] (for
//! `--setup-ntfy` and `/api/status`).
//! TODO(Phase 2a): [`Notifier::send`] (POST `<server>/` JSON body `topic,title,message[,tags,
//! priority,click]`, raw UTF-8; dry-run only logs unless the server is loopback; 2xx = true;
//! `notify '<title>' failed: HTTP <code>` / `<ExcType>`; `notify sent: <title>`; never
//! panics), `build_payload`, `is_loopback_url`.

use crate::config::Config;
use crate::services::Http;
use base64::Engine;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_SERVER: &str = "https://ntfy.sh";
pub const TIMEOUT: Duration = Duration::from_secs(10);
pub const TOPIC_PREFIX: &str = "plexcouch-";

/// `plexcouch-` + `token_urlsafe(18)` (24 chars of `[A-Za-z0-9_-]`).
pub fn new_topic() -> String {
    let mut b = [0u8; 18];
    getrandom::getrandom(&mut b).expect("OS random source");
    format!("{TOPIC_PREFIX}{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b))
}

pub fn subscribe_url(server: &str, topic: &str) -> String {
    let s = if server.is_empty() { DEFAULT_SERVER } else { server };
    format!("{}/{}", s.trim_end_matches('/'), topic)
}

pub struct Notifier {
    pub config: Arc<Config>,
    pub dry_run: bool,
    pub http: Http,
}

impl Notifier {
    pub fn new(config: Arc<Config>, dry_run: bool, http: Http) -> Notifier {
        Notifier { config, dry_run, http }
    }

    fn settings(&self) -> Value {
        match self.config.get("notifications") {
            Some(v) if crate::json::truthy(Some(&v)) => v,
            _ => Value::Object(Default::default()),
        }
    }

    /// The topic if it is a non-blank string.
    pub fn topic(&self) -> Option<String> {
        match self.settings().get("topic") {
            Some(Value::String(t)) if !t.trim().is_empty() => Some(t.clone()),
            _ => None,
        }
    }

    /// `str(server or "https://ntfy.sh").rstrip("/")`.
    pub fn server(&self) -> String {
        let s = self.settings();
        let v = s.get("server");
        let raw = if crate::json::truthy(v) { crate::json::py_str(v.unwrap()) } else { DEFAULT_SERVER.to_string() };
        raw.trim_end_matches('/').to_string()
    }

    pub fn enabled(&self) -> bool {
        self.topic().is_some()
    }

    /// Send one notification; true if accepted (or only logged in dry-run). Never panics.
    ///
    /// TODO(Phase 2a): implement. `priority`: a string (`min`..`urgent`) or an int 1-5.
    pub async fn send(
        &self,
        title: &str,
        message: &str,
        tags: Option<Vec<String>>,
        priority: Option<Value>,
        timeout: Duration,
    ) -> bool {
        let _ = (title, message, tags, priority, timeout);
        false
    }

    /// Fire and forget (`send_async`): the after-movie notification.
    pub fn send_async(&self, title: &str, message: &str, tags: Option<Vec<String>>, priority: Option<Value>) {
        let me = Notifier { config: self.config.clone(), dry_run: self.dry_run, http: self.http.clone() };
        let (title, message) = (title.to_string(), message.to_string());
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                me.send(&title, &message, tags, priority, TIMEOUT).await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // py: test_notify.TestNotifier.test_new_topic (needed by --setup-ntfy)
    #[test]
    fn topic_shape() {
        let t = new_topic();
        assert!(t.starts_with("plexcouch-"));
        let rest = &t["plexcouch-".len()..];
        assert_eq!(rest.len(), 24);
        assert!(rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'));
        assert_ne!(new_topic(), t);
        assert_eq!(subscribe_url("https://ntfy.sh/", "x"), "https://ntfy.sh/x");
    }
}
