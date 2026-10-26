//! qBittorrent Web API (spec 5.5): `/api/v2/torrents/info`, log in on HTTP 403 and retry
//! once; FileList filter per torrent (plan fix 13: FileList if ANY of its trackers is). The
//! cookies (SID) are kept in memory; tracker URLs are never stored, logged or returned.
//!
//! Phase 2b (seed keeper): [`Qbittorrent::api_get`] / [`Qbittorrent::api_post`] carry the
//! cookie and the log-in-on-403 retry; use them for tracker status and deletes.

use crate::logic::torrents::{compile_tracker_regex, is_filelist_tracker};
use crate::services::{Client, Http, UpResult, UpstreamError, TIMEOUT};
use regex::Regex;
use reqwest::Method;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

pub struct Qbittorrent {
    pub c: Client,
    /// Cookie name -> value (the session cookie jar).
    cookies: Mutex<Vec<(String, String)>>,
    login_lock: tokio::sync::Mutex<()>,
    /// Torrent hash -> is FileList. Never stores URLs.
    fl_hash: Mutex<HashMap<String, bool>>,
    tracker_re: Regex,
}

/// `urllib.parse.quote_plus` (form bodies): unreserved kept, space as `+`.
pub fn quote_plus(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `urllib.parse.urlencode(pairs)`.
pub fn urlencode(pairs: &[(&str, &str)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", quote_plus(k), quote_plus(v))).collect::<Vec<_>>().join("&")
}

impl Qbittorrent {
    pub fn new(c: Client) -> Qbittorrent {
        let pattern = c.config.get("filelist_tracker_regex").and_then(|v| v.as_str().map(String::from));
        let tracker_re = compile_tracker_regex(pattern.as_deref());
        Qbittorrent {
            c,
            cookies: Mutex::new(Vec::new()),
            login_lock: tokio::sync::Mutex::new(()),
            fl_hash: Mutex::new(HashMap::new()),
            tracker_re,
        }
    }

    /// The client every qBittorrent request goes through (redirect rule included).
    pub fn http(&self) -> &Http {
        &self.c.http
    }

    fn base(&self) -> String {
        self.c.url("qbittorrent", "")
    }

    fn cookie_header(&self) -> Option<String> {
        let g = self.cookies.lock().unwrap_or_else(|e| e.into_inner());
        (!g.is_empty()).then(|| g.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; "))
    }

    fn store_cookies(&self, headers: &reqwest::header::HeaderMap) {
        let mut g = self.cookies.lock().unwrap_or_else(|e| e.into_inner());
        for v in headers.get_all(reqwest::header::SET_COOKIE) {
            let Ok(v) = v.to_str() else { continue };
            let pair = v.split(';').next().unwrap_or("").trim();
            let Some((name, value)) = pair.split_once('=') else { continue };
            let name = name.trim().to_string();
            g.retain(|(k, _)| *k != name);
            g.push((name, value.trim().to_string()));
        }
    }

    async fn send(&self, method: Method, path: &str, body: Option<Vec<u8>>, form: bool) -> UpResult<Option<Value>> {
        let url = format!("{}/api/v2{path}", self.base());
        let cookie = self.cookie_header();
        let mut headers: Vec<(&str, &str)> = vec![("Accept", "application/json")];
        if let Some(c) = cookie.as_deref() {
            headers.push(("Cookie", c));
        }
        if form {
            headers.push(("Content-Type", "application/x-www-form-urlencoded"));
        }
        let r = self.c.http.request(method, &url, &headers, body, TIMEOUT).await?;
        self.store_cookies(&r.headers);
        if r.body.is_empty() {
            return Ok(None);
        }
        let parsed = std::str::from_utf8(&r.body).ok().and_then(|t| serde_json::from_str(t).ok());
        if parsed.is_none() && !form {
            return Err(UpstreamError::Body); // a GET must answer JSON (`_get_json`)
        }
        // POSTs may answer plain text ("Ok."): only JSON is returned.
        Ok(parsed)
    }

    /// `POST /api/v2/auth/login`, serialized; the answer is not checked.
    async fn login(&self) -> UpResult<()> {
        let _one = self.login_lock.lock().await;
        let svc = self.c.config.service("qbittorrent");
        let field = |k: &str, d: &str| {
            let v = svc.get(k);
            if crate::json::truthy(v) { crate::json::py_str(v.unwrap()) } else { d.to_string() }
        };
        let body = urlencode(&[("username", &field("username", "admin")), ("password", &field("password", "x"))]);
        let base = self.base();
        let headers = [
            ("Referer", base.as_str()),
            ("Origin", base.as_str()),
            ("Content-Type", "application/x-www-form-urlencoded"),
        ];
        let url = format!("{base}/api/v2/auth/login");
        let r = self.c.http.request(Method::POST, &url, &headers, Some(body.into_bytes()), TIMEOUT).await?;
        self.store_cookies(&r.headers);
        Ok(())
    }

    /// `GET /api/v2<path>` as JSON; on HTTP 403 log in and retry once.
    pub async fn api_get(&self, path: &str) -> UpResult<Option<Value>> {
        match self.send(Method::GET, path, None, false).await {
            Err(UpstreamError::Status(403)) => {}
            other => return other,
        }
        self.login().await?;
        self.send(Method::GET, path, None, false).await
    }

    /// `POST /api/v2<path>` with a form body; on HTTP 403 log in and retry once.
    pub async fn api_post(&self, path: &str, form: &[(&str, &str)]) -> UpResult<Option<Value>> {
        let body = urlencode(form).into_bytes();
        match self.send(Method::POST, path, Some(body.clone()), true).await {
            Err(UpstreamError::Status(403)) => {}
            other => return other,
        }
        self.login().await?;
        self.send(Method::POST, path, Some(body), true).await
    }

    /// FileList if the current tracker is, else if any tracker in the list is (plan fix 13;
    /// the list answer is cached per hash, a failure counts as not FileList, uncached).
    async fn is_filelist(&self, t: &Value) -> bool {
        if let Some(tr) = t.get("tracker").and_then(Value::as_str) {
            if is_filelist_tracker(Some(tr), &self.tracker_re) {
                return true;
            }
        }
        let h = t.get("hash").and_then(Value::as_str).unwrap_or("").to_string();
        if let Some(b) = self.fl_hash.lock().unwrap_or_else(|e| e.into_inner()).get(&h) {
            return *b;
        }
        let path = format!("/torrents/trackers?hash={}", quote_path(&h));
        let Ok(list) = self.api_get(&path).await else { return false };
        let result = match list {
            Some(Value::Array(a)) => {
                a.iter().any(|x| is_filelist_tracker(x.get("url").and_then(Value::as_str), &self.tracker_re))
            }
            _ => false,
        };
        self.fl_hash.lock().unwrap_or_else(|e| e.into_inner()).insert(h, result);
        result
    }

    /// FileList torrents from `/api/v2/torrents/info`.
    pub async fn filelist_torrents(&self) -> UpResult<Vec<Value>> {
        let info = match self.api_get("/torrents/info").await? {
            Some(Value::Array(a)) => a,
            _ => Vec::new(),
        };
        let mut out = Vec::new();
        for t in info {
            if self.is_filelist(&t).await {
                out.push(t);
            }
        }
        Ok(out)
    }
}

/// `urllib.parse.quote(s)` (safe `/`).
pub fn quote_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
