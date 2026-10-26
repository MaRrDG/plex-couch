//! Test helpers: a real `AppState` on a temp config, served on an ephemeral 127.0.0.1 port by
//! the real listener code (`ServerCase`), and a tiny blocking HTTP/1.1 client
//! for the requests.

#![allow(dead_code)]

use plex_couch::app::{AppOptions, AppState};
use plex_couch::auth::pin::hash_pin;
use plex_couch::config::Config;
use plex_couch::http::server::{self, ListenerCfg, Role};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub const PIN: &str = "246810";
pub const DAY: f64 = 86400.0;

pub fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pc-it-{tag}-{}", plex_couch::config::token_hex(8)));
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub struct TestServer {
    pub port: u16,
    pub app: Arc<AppState>,
    pub dir: PathBuf,
    rt: Option<tokio::runtime::Runtime>,
}

pub struct Setup {
    pub role: Role,
    pub opts: AppOptions,
    /// Hash this PIN into the config first (1000 iterations, to keep the tests fast).
    pub pin: Option<&'static str>,
    pub max_connections: Option<usize>,
    pub timeout: Option<Duration>,
    pub tls: Option<tokio_rustls::TlsAcceptor>,
    pub dir: Option<PathBuf>,
}

impl Default for Setup {
    fn default() -> Self {
        Setup {
            role: Role::App { secure: false },
            opts: AppOptions { dry_run: true, ..Default::default() },
            pin: Some(PIN),
            max_connections: None,
            timeout: None,
            tls: None,
            dir: None,
        }
    }
}

pub fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap()
}

impl TestServer {
    pub fn start(s: Setup) -> TestServer {
        let dir = s.dir.clone().unwrap_or_else(|| temp_dir("srv"));
        let cfg = Config::load(&dir.join("cfg.json")).unwrap();
        if let Some(p) = s.pin {
            cfg.set_pin_hash(&hash_pin(p, 1000)).unwrap();
        }
        let rt = runtime();
        let _g = rt.enter();
        let app = AppState::new(Arc::new(cfg), s.opts).unwrap();
        let std_l = server::bind("127.0.0.1", 0).unwrap();
        let port = std_l.local_addr().unwrap().port();
        let mut lc = ListenerCfg::new(s.role, s.tls);
        if let Some(m) = s.max_connections {
            lc.max_connections = m;
        }
        if let Some(t) = s.timeout {
            lc.timeout = t;
        }
        let l = tokio::net::TcpListener::from_std(std_l).unwrap();
        rt.spawn(server::serve(l, Arc::new(lc), app.clone()));
        TestServer { port, app, dir, rt: Some(rt) }
    }

    pub fn req(&self, method: &str, path: &str) -> Request {
        Request::new(self.port, method, path)
    }

    /// POST /api/login with the PIN; returns (status, headers, body).
    pub fn login(&self, pin: &str) -> Resp {
        self.req("POST", "/api/login").json(serde_json::json!({ "pin": pin })).send()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub struct Request {
    port: u16,
    method: String,
    path: String,
    host: Option<String>,
    cookie: Option<String>,
    csrf: bool,
    body: Option<Value>,
    extra: Vec<(String, String)>,
}

impl Request {
    pub fn new(port: u16, method: &str, path: &str) -> Request {
        Request {
            port,
            method: method.into(),
            path: path.into(),
            host: Some(format!("127.0.0.1:{port}")),
            cookie: None,
            csrf: true,
            body: None,
            extra: Vec::new(),
        }
    }

    /// `host=""` sends no Host header at all.
    pub fn host(mut self, h: &str) -> Self {
        self.host = if h.is_empty() { None } else { Some(h.into()) };
        self
    }

    pub fn cookie(mut self, c: &str) -> Self {
        self.cookie = Some(c.into());
        self
    }

    pub fn no_csrf(mut self) -> Self {
        self.csrf = false;
        self
    }

    pub fn json(mut self, v: Value) -> Self {
        self.body = Some(v);
        self
    }

    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.extra.push((k.into(), v.into()));
        self
    }

    pub fn bytes(&self) -> Vec<u8> {
        let mut h = format!("{} {} HTTP/1.1\r\n", self.method, self.path);
        if let Some(host) = &self.host {
            h.push_str(&format!("Host: {host}\r\n"));
        }
        if let Some(c) = &self.cookie {
            h.push_str(&format!("Cookie: pc_session={c}\r\n"));
        }
        let mut body = Vec::new();
        if self.method == "POST" {
            if self.csrf {
                h.push_str("X-Requested-With: plex-couch\r\n");
            }
            h.push_str("Content-Type: application/json\r\n");
            body = serde_json::to_vec(&self.body.clone().unwrap_or(serde_json::json!({}))).unwrap();
            h.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        for (k, v) in &self.extra {
            h.push_str(&format!("{k}: {v}\r\n"));
        }
        h.push_str("Connection: close\r\n\r\n");
        let mut out = h.into_bytes();
        out.extend(body);
        out
    }

    pub fn send(self) -> Resp {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(&self.bytes()).unwrap();
        let mut raw = Vec::new();
        let _ = s.read_to_end(&mut raw);
        Resp::parse(&raw, self.method == "HEAD")
    }
}

#[derive(Debug)]
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn parse(raw: &[u8], head: bool) -> Resp {
        let end = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("a complete response head");
        let head_txt = String::from_utf8_lossy(&raw[..end]).to_string();
        let mut lines = head_txt.split("\r\n");
        let status_line = lines.next().unwrap();
        let status: u16 = status_line.split(' ').nth(1).unwrap().parse().unwrap();
        let headers: Vec<(String, String)> = lines
            .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
            .collect();
        let mut body = raw[end + 4..].to_vec();
        if head {
            body.clear();
        } else if let Some(n) = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).map(|(_, v)| v.parse::<usize>().unwrap()) {
            body.truncate(n);
        }
        Resp { status, headers, body }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8(self.body.clone()).unwrap()
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    /// The session cookie value from Set-Cookie, if any.
    pub fn cookie_value(&self) -> Option<String> {
        let sc = self.header("Set-Cookie")?;
        let first = sc.split(';').next()?;
        first.split_once('=').map(|(_, v)| v.to_string())
    }
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Send raw request bytes, read to EOF.
pub fn raw(port: u16, bytes: &[u8]) -> Resp {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(bytes).unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    Resp::parse(&out, false)
}
