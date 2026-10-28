//! Job-only qBittorrent calls for the seed keeper and the TV conversion: all torrents,
//! tracker statuses, file lists, delete with `deleteFiles=true`. Logs in on HTTP 403 and
//! retries once (spec 5.5); the SID cookie is kept in memory. Tracker URLs are never stored,
//! logged or returned: only "is FileList" and the status leave this module.

use crate::services::arr_jobs::urlencode;
use crate::services::{Client, UpResult, UpstreamError};
use regex::Regex;
use reqwest::Method;
use serde_json::Value;
use std::sync::Mutex;
use std::time::Duration;

pub const QBIT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct QbitJobs {
    pub c: Client,
    sid: Mutex<Option<String>>,
}

/// One tracker of a torrent, without its URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerStatus {
    pub filelist: bool,
    /// qBittorrent: 0 disabled, 1 not contacted, 2 working, 3 updating, 4 not working.
    pub status: i64,
}

impl QbitJobs {
    pub fn new(c: Client) -> QbitJobs {
        QbitJobs { c, sid: Mutex::new(None) }
    }

    fn base(&self) -> String {
        self.c.url("qbittorrent", "")
    }

    fn cookie(&self) -> Option<String> {
        self.sid.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    async fn login(&self) {
        let svc = self.c.config.service("qbittorrent");
        let s = |k: &str, d: &str| match svc.get(k).and_then(Value::as_str) {
            Some(v) if !v.is_empty() => v.to_string(),
            _ => d.to_string(),
        };
        let base = self.base();
        let form = format!("username={}&password={}", urlencode(&s("username", "admin")), urlencode(&s("password", "x")));
        let h = [
            ("Content-Type", "application/x-www-form-urlencoded"),
            ("Referer", base.as_str()),
            ("Origin", base.as_str()),
        ];
        let url = format!("{base}/api/v2/auth/login");
        if let Ok(r) = self.c.http.request(Method::POST, &url, &h, Some(form.into_bytes()), QBIT_TIMEOUT).await {
            for v in r.headers.get_all(reqwest::header::SET_COOKIE) {
                if let Some(sid) = v.to_str().ok().and_then(|c| c.split(';').next()).filter(|c| c.starts_with("SID=")) {
                    *self.sid.lock().unwrap_or_else(|e| e.into_inner()) = Some(sid.to_string());
                }
            }
        }
    }

    /// One request; on HTTP 403 log in and retry once.
    async fn call(&self, method: Method, path: &str, form: Option<String>) -> UpResult<Vec<u8>> {
        let url = format!("{}{path}", self.base());
        for attempt in 0..2 {
            let cookie = self.cookie();
            let mut h: Vec<(&str, &str)> = Vec::new();
            if let Some(c) = cookie.as_deref() {
                h.push(("Cookie", c));
            }
            if form.is_some() {
                h.push(("Content-Type", "application/x-www-form-urlencoded"));
            }
            let body = form.clone().map(String::into_bytes);
            match self.c.http.request(method.clone(), &url, &h, body, QBIT_TIMEOUT).await {
                Err(UpstreamError::Status(403)) if attempt == 0 => self.login().await,
                Err(e) => return Err(e),
                Ok(r) => return Ok(r.body.to_vec()),
            }
        }
        Err(UpstreamError::Status(403))
    }

    /// `GET /api/v2/torrents/info` (every torrent, every category).
    pub async fn torrents(&self) -> UpResult<Vec<Value>> {
        let b = self.call(Method::GET, "/api/v2/torrents/info", None).await?;
        match serde_json::from_slice::<Value>(&b) {
            Ok(Value::Array(a)) => Ok(a),
            _ => Err(UpstreamError::Body),
        }
    }

    /// `GET /api/v2/torrents/trackers?hash=<h>`, reduced to (FileList?, status).
    pub async fn trackers(&self, hash: &str, filelist: &Regex) -> UpResult<Vec<TrackerStatus>> {
        let b = self.call(Method::GET, &format!("/api/v2/torrents/trackers?hash={}", urlencode(hash)), None).await?;
        let Ok(Value::Array(a)) = serde_json::from_slice::<Value>(&b) else { return Err(UpstreamError::Body) };
        Ok(a.iter()
            .map(|t| TrackerStatus {
                filelist: t.get("url").and_then(Value::as_str).is_some_and(|u| filelist.is_match(u)),
                status: t.get("status").and_then(Value::as_i64).unwrap_or(-1),
            })
            .collect())
    }

    /// `GET /api/v2/torrents/files?hash=<h>`: the file names, relative to `save_path`.
    pub async fn files(&self, hash: &str) -> UpResult<Vec<String>> {
        let b = self.call(Method::GET, &format!("/api/v2/torrents/files?hash={}", urlencode(hash)), None).await?;
        let Ok(Value::Array(a)) = serde_json::from_slice::<Value>(&b) else { return Err(UpstreamError::Body) };
        a.iter().map(|f| f.get("name").and_then(Value::as_str).map(String::from).ok_or(UpstreamError::Body)).collect()
    }

    /// [`QbitJobs::files`] with each file's size in bytes.
    pub async fn files_sized(&self, hash: &str) -> UpResult<Vec<(String, u64)>> {
        let b = self.call(Method::GET, &format!("/api/v2/torrents/files?hash={}", urlencode(hash)), None).await?;
        let Ok(Value::Array(a)) = serde_json::from_slice::<Value>(&b) else { return Err(UpstreamError::Body) };
        a.iter()
            .map(|f| {
                let name = f.get("name").and_then(Value::as_str).ok_or(UpstreamError::Body)?;
                Ok((name.to_string(), f.get("size").and_then(Value::as_u64).unwrap_or(0)))
            })
            .collect()
    }

    /// `POST /api/v2/torrents/delete` `hashes=<h>&deleteFiles=true`.
    pub async fn delete_with_files(&self, hash: &str) -> UpResult<()> {
        let form = format!("hashes={}&deleteFiles=true", urlencode(hash));
        self.call(Method::POST, "/api/v2/torrents/delete", Some(form)).await?;
        Ok(())
    }
}
