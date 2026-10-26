//! Job-only calls to Radarr and Sonarr (both speak the same v3 API): import lists,
//! commands, queue, manual import, history by download id, series/episodes. Used by
//! `jobs::watchlist`, `jobs::strip_dv` and `jobs::seed_keeper`. Header `X-Api-Key`
//! (sent even when empty); the key is never logged.

use crate::services::{Client, UpResult, UpstreamError};
use reqwest::Method;
use serde_json::{json, Value};
use std::time::Duration;

/// Jobs are not latency-critical; manual import scans the download folder.
pub const ARR_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct Arr {
    pub c: Client,
    /// `radarr` or `sonarr`.
    pub name: &'static str,
}

/// A JSON array, or `[]` for an empty body; anything else is a body error.
fn as_array(v: Option<Value>) -> UpResult<Vec<Value>> {
    match v {
        None => Ok(Vec::new()),
        Some(Value::Array(a)) => Ok(a),
        Some(_) => Err(UpstreamError::Body),
    }
}

impl Arr {
    pub fn radarr(c: Client) -> Arr {
        Arr { c, name: "radarr" }
    }

    pub fn sonarr(c: Client) -> Arr {
        Arr { c, name: "sonarr" }
    }

    /// `Radarr` / `Sonarr` for log lines.
    pub fn label(&self) -> &'static str {
        if self.name == "radarr" {
            "Radarr"
        } else {
            "Sonarr"
        }
    }

    fn url(&self, path: &str) -> String {
        self.c.url(self.name, &format!("/api/v3{path}"))
    }

    pub async fn get(&self, path: &str) -> UpResult<Option<Value>> {
        let key = self.c.keys.get(self.name);
        self.c.http.get_json(&self.url(path), &[("X-Api-Key", &key)], ARR_TIMEOUT).await
    }

    async fn send(&self, method: Method, path: &str, body: Option<&Value>) -> UpResult<Option<Value>> {
        let key = self.c.keys.get(self.name);
        let mut h: Vec<(&str, &str)> = vec![("X-Api-Key", &key), ("Accept", "application/json")];
        let bytes = body.map(|b| serde_json::to_vec(b).unwrap_or_default());
        if bytes.is_some() {
            h.push(("Content-Type", "application/json"));
        }
        let r = self.c.http.request(method, &self.url(path), &h, bytes, ARR_TIMEOUT).await?;
        if r.body.is_empty() {
            return Ok(None);
        }
        Ok(serde_json::from_slice(&r.body).ok())
    }

    /// `GET /importlist`.
    pub async fn import_lists(&self) -> UpResult<Vec<Value>> {
        as_array(self.get("/importlist").await?)
    }

    /// `POST /command` with `body` (`{"name": ..., ...}`).
    pub async fn command(&self, body: &Value) -> UpResult<Option<Value>> {
        self.send(Method::POST, "/command", Some(body)).await
    }

    /// `GET /queue?pageSize=100` -> `.records`.
    pub async fn queue(&self) -> UpResult<Vec<Value>> {
        let v = self.get("/queue?pageSize=100").await?;
        Ok(v.and_then(|v| v.get("records").and_then(Value::as_array).cloned()).unwrap_or_default())
    }

    /// `GET /manualimport?downloadId=..&<movieId|seriesId>=..&filterExistingFiles=false`.
    pub async fn manual_import(&self, download_id: &str, id_param: &str, id: i64) -> UpResult<Vec<Value>> {
        let dl = urlencode(download_id);
        as_array(self.get(&format!("/manualimport?downloadId={dl}&{id_param}={id}&filterExistingFiles=false")).await?)
    }

    /// `GET /history?downloadId=<UPPER>&pageSize=200` -> `.records` (the filter is
    /// case-sensitive and the *arr apps store the hash upper-case).
    pub async fn history_for_download(&self, download_id: &str) -> UpResult<Vec<Value>> {
        let dl = urlencode(&download_id.trim().to_ascii_uppercase());
        let v = self.get(&format!("/history?downloadId={dl}&pageSize=200")).await?;
        match v {
            Some(v) => match v.get("records") {
                Some(Value::Array(a)) => Ok(a.clone()),
                _ => Err(UpstreamError::Body),
            },
            None => Err(UpstreamError::Body),
        }
    }

    /// `GET /movie` (Radarr) or `GET /series` (Sonarr), uncached.
    pub async fn items(&self) -> UpResult<Vec<Value>> {
        let path = if self.name == "radarr" { "/movie" } else { "/series" };
        match self.get(path).await? {
            Some(Value::Array(a)) => Ok(a),
            _ => Err(UpstreamError::Body),
        }
    }

    /// Sonarr `GET /series/<id>`.
    pub async fn series(&self, id: i64) -> UpResult<Value> {
        self.get(&format!("/series/{id}")).await?.ok_or(UpstreamError::Body)
    }

    /// Sonarr `PUT /series/<id>` with the full series object.
    pub async fn put_series(&self, id: i64, series: &Value) -> UpResult<Option<Value>> {
        self.send(Method::PUT, &format!("/series/{id}"), Some(series)).await
    }

    /// Sonarr `DELETE /series/<id>?deleteFiles=true&addImportListExclusion=false`.
    pub async fn delete_series(&self, id: i64) -> UpResult<()> {
        self.send(Method::DELETE, &format!("/series/{id}?deleteFiles=true&addImportListExclusion=false"), None).await?;
        Ok(())
    }

    /// Sonarr `GET /episode?seriesId=<id>`.
    pub async fn episodes(&self, series_id: i64) -> UpResult<Vec<Value>> {
        match self.get(&format!("/episode?seriesId={series_id}")).await? {
            Some(Value::Array(a)) => Ok(a),
            _ => Err(UpstreamError::Body),
        }
    }

    /// Sonarr `GET /episodefile?seriesId=<id>`.
    pub async fn episode_files(&self, series_id: i64) -> UpResult<Vec<Value>> {
        match self.get(&format!("/episodefile?seriesId={series_id}")).await? {
            Some(Value::Array(a)) => Ok(a),
            _ => Err(UpstreamError::Body),
        }
    }

    /// Sonarr `PUT /episode/monitor` `{"episodeIds": [...], "monitored": true}`.
    pub async fn monitor_episodes(&self, ids: &[i64]) -> UpResult<()> {
        self.send(Method::PUT, "/episode/monitor", Some(&json!({"episodeIds": ids, "monitored": true}))).await?;
        Ok(())
    }
}

/// Percent-encode a query value (RFC 3986 unreserved characters stay).
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
