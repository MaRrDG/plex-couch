//! Test helpers for the jobs: a fake upstream HTTP server that records every request, and
//! an `AppState` whose services all point at it.

use crate::app::{AppOptions, AppState};
use crate::config::Config;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, Clone)]
pub struct Req {
    pub method: String,
    /// Path and query, e.g. `/api/v3/command`.
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Req {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// A response: status, body (JSON text), extra headers.
pub type Resp = (u16, String, Vec<(String, String)>);
pub type Handler = Arc<dyn Fn(&Req) -> Resp + Send + Sync>;

pub fn ok(v: Value) -> Resp {
    (200, v.to_string(), vec![])
}

pub fn status(code: u16) -> Resp {
    (code, String::new(), vec![])
}

pub struct Fake {
    pub base: String,
    pub reqs: Arc<Mutex<Vec<Req>>>,
}

impl Fake {
    pub fn requests(&self) -> Vec<Req> {
        self.reqs.lock().unwrap().clone()
    }

    /// Requests that change something upstream (anything but GET).
    pub fn mutations(&self) -> Vec<Req> {
        self.requests().into_iter().filter(|r| r.method != "GET").collect()
    }

    pub fn find(&self, method: &str, path_prefix: &str) -> Vec<Req> {
        self.requests().into_iter().filter(|r| r.method == method && r.path.starts_with(path_prefix)).collect()
    }
}

fn find_sub(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

/// Start a fake server; `handler` answers every request.
pub async fn fake(handler: impl Fn(&Req) -> Resp + Send + Sync + 'static) -> Fake {
    let handler: Handler = Arc::new(handler);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let reqs = Arc::new(Mutex::new(Vec::new()));
    let r2 = reqs.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { return };
            let (reqs, handler) = (r2.clone(), handler.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = vec![0u8; 16384];
                let head_end = loop {
                    let n = s.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = find_sub(&buf, b"\r\n\r\n") {
                        break i;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let mut lines = head.split("\r\n");
                let first: Vec<String> = lines.next().unwrap_or("").split(' ').map(String::from).collect();
                let headers: Vec<(String, String)> = lines
                    .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
                    .collect();
                let len = headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, v)| v.parse::<usize>().ok())
                    .unwrap_or(0);
                while buf.len() < head_end + 4 + len {
                    let n = s.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body = String::from_utf8_lossy(&buf[head_end + 4..]).to_string();
                let req = Req {
                    method: first.first().cloned().unwrap_or_default(),
                    path: first.get(1).cloned().unwrap_or_default(),
                    headers,
                    body,
                };
                reqs.lock().unwrap().push(req.clone());
                let (code, body, extra) = handler(&req);
                let mut out = format!(
                    "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for (k, v) in extra {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str("\r\n");
                out.push_str(&body);
                let _ = s.write_all(out.as_bytes()).await;
                let _ = s.shutdown().await;
            });
        }
    });
    Fake { base, reqs }
}

pub fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pc-{tag}-{}", crate::config::token_hex(6)));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// An app whose every service URL is `base` (plus `extra` merged into the config), keys
/// set inline, and the config in a fresh temp dir.
pub fn app(base: &str, dry_run: bool, extra: Value) -> (Arc<AppState>, PathBuf) {
    let d = temp_dir("jobs");
    let mut cfg = json!({
        "services": {
            "plex": {"url": base, "token": "plex-tok"},
            "radarr": {"url": base, "api_key": "radarr-key"},
            "sonarr": {"url": base, "api_key": "sonarr-key"},
            "qbittorrent": {"url": base},
            "bazarr": {"url": base, "api_key": "b"},
            "prowlarr": {"url": base}
        },
        "notifications": {"server": base, "topic": "plexcouch-testtopic"},
        "jobs": {"watchlist": {"discover_url": base}}
    });
    cfg = crate::config::merge(&cfg, &extra);
    let p = d.join("config.local.json");
    std::fs::write(&p, cfg.to_string()).unwrap();
    let config = Arc::new(Config::load(&p).unwrap());
    let a = AppState::new(config, AppOptions { dry_run, ..Default::default() }).unwrap();
    (a, d)
}
