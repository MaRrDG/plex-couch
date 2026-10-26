//! Plex Media Server (spec 5.1). Header `X-Plex-Token` only when the token is non-empty.
//!
//! TODO(Phase 2a): implement the methods (and `parse_plex_sessions`, logic.py:746-785,
//! wherever 2a puts the pure parsing).

use crate::services::{Client, UpResult, UpstreamError};
use serde_json::Value;

pub struct Plex {
    pub c: Client,
}

impl Plex {
    pub fn new(c: Client) -> Plex {
        Plex { c }
    }

    pub fn token(&self) -> String {
        self.c.keys.get("plex")
    }

    /// `GET {plex.url}/status/sessions` (not cached).
    pub async fn sessions_raw(&self) -> UpResult<Option<Value>> {
        Err(UpstreamError::Other("plex: not implemented"))
    }

    /// The parsed sessions (internal fields `key` and `address` included).
    pub async fn sessions(&self) -> UpResult<Vec<Value>> {
        Err(UpstreamError::Other("plex: not implemented"))
    }

    /// Poster through the photo transcoder, 240x360 `thumb`. `rating_key`: ASCII digits.
    pub async fn poster(&self, rating_key: &str) -> Option<(Vec<u8>, String)> {
        let _ = rating_key;
        None
    }

    /// Backdrop through the photo transcoder, 720x405 `art`.
    pub async fn art(&self, rating_key: &str) -> Option<(Vec<u8>, String)> {
        let _ = rating_key;
        None
    }
}
