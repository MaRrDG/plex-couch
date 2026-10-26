//! ntfy notifications (spec 7.1). The topic is the only secret: kept in the config,
//! returned only to logged-in clients, never logged.
//!
//! [`Notifier::send`]: POST `<server>/` with the JSON body `topic,title,message[,tags,
//! priority,click]` as raw UTF-8; in dry-run it only logs unless the server is on this
//! machine; true on 2xx; failures log `notify '<title>' failed: HTTP <code>` / `<ExcType>`
//! (never the URL); success logs `notify sent: <title>`. It never panics.

use crate::config::Config;
use crate::services::{Http, UpResult, UpstreamError};
use crate::{log_info, log_warning};
use base64::Engine;
use reqwest::Method;
use serde_json::{json, Map, Value};
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

/// ntfy priority names: 1 min, 2 low, 3 default, 4 high, 5 max/urgent.
fn priority_of(p: &Value) -> Option<i64> {
    let n = match p {
        Value::String(s) => match s.as_str() {
            "min" => 1,
            "low" => 2,
            "default" => 3,
            "high" => 4,
            "max" | "urgent" => 5,
            _ => return None,
        },
        Value::Number(n) => n.as_i64()?,
        _ => return None,
    };
    (1..=5).contains(&n).then_some(n)
}

/// The JSON body for ntfy's JSON publish API. Optional fields only when given; an unknown
/// priority is dropped.
pub fn build_payload(
    topic: &str,
    title: &str,
    message: &str,
    tags: Option<&[String]>,
    priority: Option<&Value>,
    click: Option<&str>,
) -> Value {
    let mut body = Map::new();
    body.insert("topic".into(), json!(topic));
    body.insert("title".into(), json!(title));
    body.insert("message".into(), json!(message));
    if let Some(t) = tags.filter(|t| !t.is_empty()) {
        body.insert("tags".into(), json!(t));
    }
    if let Some(p) = priority.and_then(priority_of) {
        body.insert("priority".into(), json!(p));
    }
    if let Some(c) = click.filter(|c| !c.is_empty()) {
        body.insert("click".into(), json!(c));
    }
    Value::Object(body)
}

/// The server is this machine (`localhost` or a loopback IP).
pub fn is_loopback_url(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else { return false };
    let host = u.host_str().unwrap_or("").trim_start_matches('[').trim_end_matches(']').to_lowercase();
    host == "localhost" || crate::http::security::is_loopback_str(&host)
}

/// Python `repr(str)` (the log quotes titles like `'%r'`).
pub fn py_repr_str(s: &str) -> String {
    let q = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(q);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == q => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(q);
    out
}

/// Replaces the HTTP POST (tests): `(url, body, timeout) -> HTTP status`.
pub type PostFn = Arc<dyn Fn(&str, &[u8], Duration) -> UpResult<u16> + Send + Sync>;

#[derive(Clone)]
pub struct Notifier {
    pub config: Arc<Config>,
    pub dry_run: bool,
    pub http: Http,
    pub post_override: Option<PostFn>,
}

impl Notifier {
    pub fn new(config: Arc<Config>, dry_run: bool, http: Http) -> Notifier {
        Notifier { config, dry_run, http, post_override: None }
    }

    /// With a replaced POST (tests).
    pub fn with_post(mut self, f: PostFn) -> Notifier {
        self.post_override = Some(f);
        self
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

    async fn post(&self, url: &str, body: Vec<u8>, timeout: Duration) -> UpResult<u16> {
        if let Some(f) = &self.post_override {
            return f(url, &body, timeout);
        }
        let headers = [("Content-Type", "application/json; charset=utf-8")];
        self.http.request(Method::POST, url, &headers, Some(body), timeout).await.map(|r| r.status)
    }

    /// Send one notification; true if accepted (or only logged in dry-run). Never panics.
    /// `priority`: a string (`min`..`urgent`) or an int 1-5.
    pub async fn send(
        &self,
        title: &str,
        message: &str,
        tags: Option<Vec<String>>,
        priority: Option<Value>,
        timeout: Duration,
    ) -> bool {
        self.send_click(title, message, tags, priority, None, timeout).await
    }

    /// [`Notifier::send`] with a `click` URL.
    pub async fn send_click(
        &self,
        title: &str,
        message: &str,
        tags: Option<Vec<String>>,
        priority: Option<Value>,
        click: Option<&str>,
        timeout: Duration,
    ) -> bool {
        let Some(topic) = self.topic() else { return false };
        let server = self.server();
        if self.dry_run && !is_loopback_url(&server) {
            log_info!("dry-run: notify {title}");
            return true;
        }
        let body = build_payload(&topic, title, message, tags.as_deref(), priority.as_ref(), click);
        match self.post(&format!("{server}/"), crate::json::dumps(&body), timeout).await {
            Ok(status) => {
                log_info!("notify sent: {title}");
                (200..300).contains(&status)
            }
            Err(UpstreamError::Status(code)) => {
                log_warning!("notify {} failed: HTTP {code}", py_repr_str(title));
                false
            }
            Err(e) => {
                log_warning!("notify {} failed: {}", py_repr_str(title), e.type_name());
                false
            }
        }
    }

    /// Fire and forget (`send_async`): the after-movie notification.
    pub fn send_async(&self, title: &str, message: &str, tags: Option<Vec<String>>, priority: Option<Value>) {
        let me = self.clone();
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
    use std::sync::Mutex;

    const TOPIC: &str = "plexcouch-unit-test-topic-xyz";

    fn cfg(notifications: Value) -> Arc<Config> {
        let d = std::env::temp_dir().join(format!("pc-ntfy-{}", crate::config::token_hex(6)));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("cfg.json");
        std::fs::write(&p, crate::json::dumps(&json!({ "notifications": notifications }))).unwrap();
        let c = Arc::new(Config::load(&p).unwrap());
        let _ = std::fs::remove_dir_all(&d);
        c
    }

    type Posts = Arc<Mutex<Vec<(String, Vec<u8>, Duration)>>>;

    /// `RecordingNotifier`: records the POSTs instead of sending them.
    fn recording(notifications: Value, dry_run: bool) -> (Notifier, Posts) {
        let posts: Posts = Arc::new(Mutex::new(Vec::new()));
        let p2 = posts.clone();
        let n = Notifier::new(cfg(notifications), dry_run, Http::new()).with_post(Arc::new(move |u, b, t| {
            p2.lock().unwrap().push((u.to_string(), b.to_vec(), t));
            Ok(200)
        }));
        (n, posts)
    }

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

    // py: test_notify.TestNotifier.test_json_payload_keeps_diacritics
    #[tokio::test]
    async fn json_payload_keeps_diacritics() {
        let (n, posts) = recording(json!({"topic": TOPIC, "server": "https://ntfy.example/"}), false);
        let text = "Mai ai 55 GB liberi pe D:. Un film 4K are 20–30 GB.";
        assert!(n.send("Spațiu puțin", text, Some(vec!["warning".into()]), Some(json!("high")), TIMEOUT).await);
        let (url, data, timeout) = posts.lock().unwrap()[0].clone();
        assert_eq!((url.as_str(), timeout), ("https://ntfy.example/", Duration::from_secs(10)));
        assert!(crate::http::security::find(&data, "Spațiu puțin".as_bytes()).is_some());
        let v: Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(
            v,
            json!({"topic": TOPIC, "title": "Spațiu puțin", "message": text, "tags": ["warning"], "priority": 4})
        );
    }

    // py: test_notify.TestNotifier.test_payload_optional_fields
    #[test]
    fn payload_optional_fields() {
        assert_eq!(build_payload("t", "a", "b", None, None, None), json!({"topic": "t", "title": "a", "message": "b"}));
        let p = build_payload("t", "a", "b", Some(&["x".to_string()]), Some(&json!(9)), Some("https://x"));
        assert_eq!(p, json!({"topic": "t", "title": "a", "message": "b", "tags": ["x"], "click": "https://x"}));
        let keys: Vec<String> = p.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["topic", "title", "message", "tags", "click"]);
        assert_eq!(build_payload("t", "a", "b", None, Some(&json!(5)), None)["priority"], json!(5));
        assert_eq!(build_payload("t", "a", "b", None, Some(&json!("urgent")), None)["priority"], json!(5));
        assert!(build_payload("t", "a", "b", None, Some(&json!("loud")), None).get("priority").is_none());
    }

    // py: test_notify.TestNotifier.test_topic_never_logged
    #[tokio::test]
    async fn topic_never_logged() {
        crate::log::start_capture();
        let notif = json!({"topic": TOPIC, "server": "https://ntfy.example"});
        let failing = Notifier::new(cfg(notif.clone()), false, Http::new())
            .with_post(Arc::new(|_, _, _| Err(UpstreamError::Connect)));
        assert!(!failing.send("Film gata", "x", None, None, TIMEOUT).await);
        assert!(recording(notif.clone(), false).0.send("Film gata", "x", None, None, TIMEOUT).await);
        assert!(recording(notif, true).0.send("Film gata", "x", None, None, TIMEOUT).await);
        let out = crate::log::captured().concat();
        assert!(out.contains("notify 'Film gata' failed: URLError"));
        assert!(out.contains("dry-run: notify Film gata"));
        assert!(!out.contains(TOPIC));
        assert!(!out.contains("ntfy.example"));
    }

    // py: test_notify.TestNotifier.test_disabled_without_topic
    #[tokio::test]
    async fn disabled_without_topic() {
        for topic in [Value::Null, json!(""), json!("   ")] {
            let (n, posts) = recording(json!({ "topic": topic }), false);
            assert!(!n.enabled());
            assert!(!n.send("a", "b", None, None, TIMEOUT).await);
            assert!(posts.lock().unwrap().is_empty());
        }
        let n = Notifier::new(cfg(json!({"topic": TOPIC, "server": null})), false, Http::new());
        assert_eq!(n.server(), "https://ntfy.sh");
    }

    // py: test_notify.TestNotifier.test_dry_run_never_sends_to_a_real_server
    #[tokio::test]
    async fn dry_run_never_sends_to_a_real_server() {
        let (n, posts) = recording(json!({"topic": TOPIC, "server": null}), true);
        assert!(n.send("Film gata", "x", None, None, TIMEOUT).await);
        assert!(posts.lock().unwrap().is_empty());
        let (n, posts) = recording(json!({"topic": TOPIC, "server": "http://192.168.0.5:80"}), true);
        n.send("a", "b", None, None, TIMEOUT).await;
        assert!(posts.lock().unwrap().is_empty());
    }

    // py: test_notify.TestNotifier.test_dry_run_sends_to_a_local_test_server
    #[tokio::test]
    async fn dry_run_sends_to_a_local_test_server() {
        for server in ["http://127.0.0.1:9", "http://localhost:9", "http://[::1]:9"] {
            let (n, posts) = recording(json!({"topic": TOPIC, "server": server}), true);
            n.send("a", "b", None, None, TIMEOUT).await;
            assert_eq!(posts.lock().unwrap().len(), 1, "{server}");
        }
    }

    /// `FakeNtfy`: a local stand-in for ntfy.sh that records each POST.
    async fn fake_ntfy(status: u16) -> crate::logic::testutil::Fake {
        crate::logic::testutil::serve(Arc::new(move |_| (status, "application/json".into(), b"{}".to_vec()))).await
    }

    // py: test_notify.TestNotifier.test_real_post_to_fake_server
    #[tokio::test]
    async fn real_post_to_fake_server() {
        let fake = fake_ntfy(200).await;
        let n = Notifier::new(cfg(json!({"topic": TOPIC, "server": fake.url})), false, Http::new());
        let text = "Dune (2024) e gata de vizionat. Subtitrarea RO încă lipsește.";
        assert!(n.send("Film gata", text, Some(vec!["movie_camera".into()]), None, TIMEOUT).await);
        let hit = fake.hits()[0].clone();
        assert_eq!((hit.method.as_str(), hit.path.as_str()), ("POST", "/"));
        assert!(hit.header("Content-Type").unwrap().starts_with("application/json"));
        let v = hit.json();
        assert_eq!(v["message"], json!(text));
        assert_eq!(v["tags"], json!(["movie_camera"]));
    }

    // py: test_notify.TestNotifier.test_http_error_returns_false
    #[tokio::test]
    async fn http_error_returns_false() {
        crate::log::start_capture();
        let fake = fake_ntfy(429).await;
        let n = Notifier::new(cfg(json!({"topic": TOPIC, "server": fake.url})), false, Http::new());
        assert!(!n.send("a'b", "b", None, None, TIMEOUT).await);
        assert_eq!(fake.hits().len(), 1);
        assert!(crate::log::captured().concat().contains("notify \"a'b\" failed: HTTP 429"));
    }

    #[test]
    fn loopback_urls() {
        for u in ["http://127.0.0.1:9", "http://localhost:9", "http://[::1]:9", "http://LOCALHOST", "http://127.5.0.1"] {
            assert!(is_loopback_url(u), "{u}");
        }
        for u in ["https://ntfy.sh", "http://192.168.0.5:80", "garbage", ""] {
            assert!(!is_loopback_url(u), "{u}");
        }
    }
}
