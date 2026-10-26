//! Radarr v3 (spec 5.2). Header `X-Api-Key: <key>`, sent even when empty. The TTL caches
//! store successful results only.
//!
//! Phase 2b: job-only calls (import lists, manual import, rescans) live in a separate file
//! such as `radarr_jobs.rs`; [`Radarr::api`] is the shared GET.

use crate::services::cache::TtlCache;
use crate::services::{Client, UpResult, TIMEOUT};
use serde_json::Value;
use std::time::Duration;

pub const MOVIES_TTL: Duration = Duration::from_secs(30);
pub const HISTORY_TTL: Duration = Duration::from_secs(60);
pub const ROOT_FOLDERS_TTL: Duration = Duration::from_secs(300);

pub struct Radarr {
    pub c: Client,
    movies: TtlCache<Vec<Value>>,
    history: TtlCache<Vec<Value>>,
    roots: TtlCache<Vec<Value>>,
}

/// `(data or {}).get("records") or []`.
pub fn records(data: Option<Value>) -> Vec<Value> {
    match data {
        Some(Value::Object(mut m)) => match m.remove("records") {
            Some(Value::Array(a)) => a,
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// `data or []` for an endpoint that answers a list.
pub fn list(data: Option<Value>) -> Vec<Value> {
    match data {
        Some(Value::Array(a)) => a,
        _ => Vec::new(),
    }
}

impl Radarr {
    pub fn new(c: Client) -> Radarr {
        Radarr {
            c,
            movies: TtlCache::new(MOVIES_TTL),
            history: TtlCache::new(HISTORY_TTL),
            roots: TtlCache::new(ROOT_FOLDERS_TTL),
        }
    }

    pub fn key(&self) -> String {
        self.c.keys.get("radarr")
    }

    /// `GET {radarr.url}/api/v3<path>` as JSON.
    pub async fn api(&self, path: &str) -> UpResult<Option<Value>> {
        let key = self.key();
        let url = self.c.url("radarr", &format!("/api/v3{path}"));
        self.c.http.get_json(&url, &[("X-Api-Key", key.as_str())], TIMEOUT).await
    }

    /// `/api/v3/queue?includeMovie=true&pageSize=200` -> `.records` (not cached).
    pub async fn queue(&self) -> UpResult<Vec<Value>> {
        Ok(records(self.api("/queue?includeMovie=true&pageSize=200").await?))
    }

    /// `/api/v3/movie` (cached 30 s).
    pub async fn movies(&self) -> UpResult<Vec<Value>> {
        self.movies.get(|| async { Ok(list(self.api("/movie").await?)) }).await
    }

    /// `/api/v3/history?eventType=1&pageSize=200` -> `.records` (cached 60 s).
    pub async fn history(&self) -> UpResult<Vec<Value>> {
        self.history.get(|| async { Ok(records(self.api("/history?eventType=1&pageSize=200").await?)) }).await
    }

    /// `/api/v3/rootfolder` (cached 300 s).
    pub async fn root_folders(&self) -> UpResult<Vec<Value>> {
        self.roots.get(|| async { Ok(list(self.api("/rootfolder").await?)) }).await
    }

    /// Forget the cached lists (tests).
    pub fn clear_cache(&self) {
        self.movies.clear();
        self.history.clear();
        self.roots.clear();
    }

    async fn cover(&self, movie_id: &str, names: [&str; 2]) -> Option<(Vec<u8>, String)> {
        if movie_id.is_empty() || !movie_id.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let key = self.key();
        for name in names {
            let url = self.c.url("radarr", &format!("/api/v3/mediacover/{movie_id}/{name}"));
            if let Some(img) = self.c.http.get_image(&url, &[("X-Api-Key", key.as_str())]).await {
                return Some(img);
            }
        }
        None
    }

    /// `mediacover/<id>/poster-500.jpg`, then `poster.jpg`.
    pub async fn poster(&self, movie_id: &str) -> Option<(Vec<u8>, String)> {
        self.cover(movie_id, ["poster-500.jpg", "poster.jpg"]).await
    }

    /// `mediacover/<id>/fanart-360.jpg`, then `fanart.jpg`.
    pub async fn fanart(&self, movie_id: &str) -> Option<(Vec<u8>, String)> {
        self.cover(movie_id, ["fanart-360.jpg", "fanart.jpg"]).await
    }
}
