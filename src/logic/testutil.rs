//! Test helpers (compiled only for unit tests): a fake upstream HTTP server on an ephemeral
//! 127.0.0.1 port that records every request, a temp config, and an `AppState` on it.

use crate::app::{AppOptions, AppState};
use crate::config::Config;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One request the fake received.
#[derive(Clone, Debug)]
pub struct Hit {
    pub method: String,
    /// The request target (path and query).
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Hit {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

/// `(status, content type, body)`.
pub type Answer = (u16, String, Vec<u8>);
pub type Handler = Arc<dyn Fn(&Hit) -> Answer + Send + Sync>;

pub fn json_answer(v: &Value) -> Answer {
    (200, "application/json".into(), serde_json::to_vec(v).unwrap())
}

pub fn status_answer(code: u16) -> Answer {
    (code, "text/plain".into(), b"x".to_vec())
}

pub struct Fake {
    pub url: String,
    pub hits: Arc<Mutex<Vec<Hit>>>,
}

impl Fake {
    pub fn hits(&self) -> Vec<Hit> {
        self.hits.lock().unwrap().clone()
    }

    pub fn paths(&self) -> Vec<String> {
        self.hits().into_iter().map(|h| h.path).collect()
    }
}

async fn read_request(s: &mut tokio::net::TcpStream) -> Option<Hit> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = s.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = crate::http::security::find(&buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).to_string();
            let mut lines = head.split("\r\n");
            let first = lines.next().unwrap_or("");
            let mut parts = first.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let path = parts.next().unwrap_or("").to_string();
            let headers: Vec<(String, String)> = lines
                .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
                .collect();
            let len: usize = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, v)| v.parse().ok())
                .unwrap_or(0);
            while buf.len() < i + 4 + len {
                let n = s.read(&mut tmp).await.ok()?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let body = buf[i + 4..].to_vec();
            return Some(Hit { method, path, headers, body });
        }
    }
}

/// Start a fake upstream; `handler` answers every request (one per connection).
pub async fn serve(handler: Handler) -> Fake {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let hits = Arc::new(Mutex::new(Vec::new()));
    let h2 = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { return };
            let (hits, handler) = (h2.clone(), handler.clone());
            tokio::spawn(async move {
                let Some(hit) = read_request(&mut s).await else { return };
                hits.lock().unwrap().push(hit.clone());
                let (code, ctype, body) = handler(&hit);
                let head = format!(
                    "HTTP/1.1 {code} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes()).await;
                let _ = s.write_all(&body).await;
                let _ = s.shutdown().await;
            });
        }
    });
    Fake { url, hits }
}

/// A URL nobody listens on (connections are refused at once).
pub fn closed_url() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    format!("http://127.0.0.1:{port}")
}

pub fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pc-ut-{tag}-{}", crate::config::token_hex(6)));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A config in a fresh temp folder: every service on a closed port with a dummy key (never
/// the real keys of this PC), merged with `extra` (top-level keys; `services` per service).
pub fn config(extra: Value) -> Arc<Config> {
    let dir = temp_dir("cfg");
    let mut services = json!({
        "plex": {"url": closed_url(), "token": "plex-test-token"},
        "radarr": {"url": closed_url(), "api_key": "radarr-test-key"},
        "sonarr": {"url": closed_url(), "api_key": "sonarr-test-key"},
        "bazarr": {"url": closed_url(), "api_key": "bazarr-test-key"},
        "prowlarr": {"url": closed_url()},
        "qbittorrent": {"url": closed_url()},
    });
    let mut top = serde_json::Map::new();
    if let Value::Object(m) = extra {
        for (k, v) in m {
            if k == "services" {
                if let Value::Object(s) = v {
                    for (name, over) in s {
                        if let (Some(Value::Object(base)), Value::Object(o)) = (services.get_mut(&name), over) {
                            for (kk, vv) in o {
                                base.insert(kk, vv);
                            }
                        }
                    }
                }
            } else {
                top.insert(k, v);
            }
        }
    }
    top.insert("services".into(), services.take());
    top.entry("disks").or_insert(json!([]));
    let p = dir.join("cfg.json");
    std::fs::write(&p, crate::json::dumps(&Value::Object(top))).unwrap();
    Arc::new(Config::load(&p).unwrap())
}

/// A dry-run app on `config`, own IPs fixed to 192.168.0.10.
pub fn app(config: Arc<Config>) -> Arc<AppState> {
    app_with(config, AppOptions { dry_run: true, ..Default::default() })
}

pub fn app_with(config: Arc<Config>, mut opts: AppOptions) -> Arc<AppState> {
    if opts.overrides.own_ips.is_none() {
        opts.overrides.own_ips = Some(Arc::new(|| ["192.168.0.10".to_string()].into_iter().collect()));
    }
    AppState::new(config, opts).unwrap()
}

/// A Plex session as the logic sees it.
pub fn session(key: &str, title: &str, progress: f64, address: &str, state: &str) -> Value {
    json!({"key": key, "title": title, "progress": progress, "address": address, "state": state})
}

/// A fake Plex whose `/status/sessions` answer tests can change.
pub struct FakePlex {
    pub fake: Fake,
    pub answer: Arc<Mutex<Answer>>,
}

impl FakePlex {
    /// Answer with these parsed-shape sessions (see [`plex_meta`]).
    pub fn set_sessions(&self, sessions: &[Value]) {
        let meta: Vec<Value> = sessions.iter().map(plex_meta).collect();
        *self.answer.lock().unwrap() = json_answer(&json!({"MediaContainer": {"size": meta.len(), "Metadata": meta}}));
    }

    /// Answer every request with this HTTP status.
    pub fn set_status(&self, code: u16) {
        *self.answer.lock().unwrap() = status_answer(code);
    }
}

pub async fn fake_plex() -> FakePlex {
    let answer = Arc::new(Mutex::new(json_answer(&json!({"MediaContainer": {"size": 0}}))));
    let a2 = answer.clone();
    let fake = serve(Arc::new(move |_| a2.lock().unwrap().clone())).await;
    FakePlex { fake, answer }
}

/// Raw Plex metadata that parses back to `s` (`key`, `title`, `progress`, `address`, `state`).
pub fn plex_meta(s: &Value) -> Value {
    let progress = s.get("progress").and_then(Value::as_f64).unwrap_or(0.0);
    json!({
        "sessionKey": s["key"], "title": s["title"], "type": "movie",
        "duration": 100000, "viewOffset": progress * 1000.0,
        "Player": {"state": s["state"], "address": s["address"], "title": "TV"},
    })
}

/// Serve `app` on an ephemeral port with the real HTTP listener code; returns the base URL.
pub async fn serve_app(app: Arc<AppState>) -> String {
    use crate::http::server::{self, ListenerCfg, Role};
    let std_l = server::bind("127.0.0.1", 0).unwrap();
    let port = std_l.local_addr().unwrap().port();
    let l = tokio::net::TcpListener::from_std(std_l).unwrap();
    tokio::spawn(server::serve(l, Arc::new(ListenerCfg::new(Role::App { secure: false }, None)), app));
    format!("http://127.0.0.1:{port}")
}

/// One request to the app (`req` of test_notify.TestRoutes): `(status, JSON body)`.
pub async fn call(base: &str, method: &str, path: &str, token: Option<&str>, csrf: bool) -> (u16, Value) {
    let (code, body) = call_raw(base, method, path, token, csrf).await;
    (code, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// Like [`call`], with the raw body.
pub async fn call_raw(base: &str, method: &str, path: &str, token: Option<&str>, csrf: bool) -> (u16, Vec<u8>) {
    let c = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut rb = c.request(method.parse().unwrap(), format!("{base}{path}"));
    if let Some(t) = token {
        rb = rb.header("Cookie", format!("pc_session={t}"));
    }
    if csrf {
        rb = rb.header("X-Requested-With", "plex-couch");
    }
    if method == "POST" {
        rb = rb.header("Content-Type", "application/json").body("{}");
    }
    let r = rb.send().await.unwrap();
    let code = r.status().as_u16();
    (code, r.bytes().await.unwrap().to_vec())
}

/// Wait until `cond` holds (up to 5 s).
pub async fn wait_for(cond: impl Fn() -> bool) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("condition not reached in 5 s");
}
