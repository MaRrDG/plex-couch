//! Radarr v3 (spec 5.2). Header `X-Api-Key: <key>`, sent even when empty. The TTL caches
//! store successful results only.
//!
//! TODO(Phase 2a): implement the read calls used by `/api/status`, the images and the
//! notifications. Phase 2b: put job-only calls (import lists, manual import, rescans) in a
//! separate file such as `radarr_jobs.rs`.

use crate::services::{Client, UpResult, UpstreamError};
use serde_json::Value;

pub struct Radarr {
    pub c: Client,
}

impl Radarr {
    pub fn new(c: Client) -> Radarr {
        Radarr { c }
    }

    pub fn key(&self) -> String {
        self.c.keys.get("radarr")
    }

    /// `/api/v3/queue?includeMovie=true&pageSize=200` -> `.records` (not cached).
    pub async fn queue(&self) -> UpResult<Vec<Value>> {
        Err(UpstreamError::Other("radarr: not implemented"))
    }

    /// `/api/v3/movie` (cached 30 s).
    pub async fn movies(&self) -> UpResult<Vec<Value>> {
        Err(UpstreamError::Other("radarr: not implemented"))
    }

    /// `/api/v3/history?eventType=1&pageSize=200` -> `.records` (cached 60 s).
    pub async fn history(&self) -> UpResult<Vec<Value>> {
        Err(UpstreamError::Other("radarr: not implemented"))
    }

    /// `/api/v3/rootfolder` (cached 300 s).
    pub async fn root_folders(&self) -> UpResult<Vec<Value>> {
        Err(UpstreamError::Other("radarr: not implemented"))
    }

    /// `mediacover/<id>/poster-500.jpg`, then `poster.jpg`.
    pub async fn poster(&self, movie_id: &str) -> Option<(Vec<u8>, String)> {
        let _ = movie_id;
        None
    }

    /// `mediacover/<id>/fanart-360.jpg`, then `fanart.jpg`.
    pub async fn fanart(&self, movie_id: &str) -> Option<(Vec<u8>, String)> {
        let _ = movie_id;
        None
    }
}
