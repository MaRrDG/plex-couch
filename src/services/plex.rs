//! Plex Media Server (spec 5.1). Header `X-Plex-Token` only when the token is non-empty.
//! The token comes only from `services.plex.token` or HKCU (plan fix 24: one token source,
//! always sent as a header, never in a URL).
//!
//! The session parsing (logic.py:746-785) is in [`crate::logic::plex`].

use crate::services::{Client, UpResult, TIMEOUT};
use serde_json::Value;

pub struct Plex {
    pub c: Client,
}

fn ascii_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

impl Plex {
    pub fn new(c: Client) -> Plex {
        Plex { c }
    }

    pub fn token(&self) -> String {
        self.c.keys.get("plex")
    }

    /// `GET {plex.url}<path>` as JSON with the token header (when there is one).
    pub async fn get_json(&self, path: &str) -> UpResult<Option<Value>> {
        let token = self.token();
        let url = self.c.url("plex", path);
        let headers: Vec<(&str, &str)> = if token.is_empty() { vec![] } else { vec![("X-Plex-Token", token.as_str())] };
        self.c.http.get_json(&url, &headers, TIMEOUT).await
    }

    /// `GET {plex.url}/status/sessions` (not cached).
    pub async fn sessions_raw(&self) -> UpResult<Option<Value>> {
        self.get_json("/status/sessions").await
    }

    /// The parsed sessions (internal fields `key` and `address` included).
    pub async fn sessions(&self) -> UpResult<Vec<Value>> {
        let raw = self.sessions_raw().await?;
        Ok(crate::logic::plex::parse_plex_sessions(raw.as_ref()))
    }

    async fn photo(&self, rating_key: &str, image: &str, width: u32, height: u32) -> Option<(Vec<u8>, String)> {
        if !ascii_digits(rating_key) {
            return None;
        }
        let token = self.token();
        // urlencode({"width", "height", "minSize": 1, "upscale": 1, "url": src}): "/" -> %2F
        let query = format!(
            "width={width}&height={height}&minSize=1&upscale=1&url=%2Flibrary%2Fmetadata%2F{rating_key}%2F{image}"
        );
        let url = self.c.url("plex", &format!("/photo/:/transcode?{query}"));
        let headers: Vec<(&str, &str)> = if token.is_empty() { vec![] } else { vec![("X-Plex-Token", token.as_str())] };
        self.c.http.get_image(&url, &headers).await
    }

    /// Poster through the photo transcoder, 240x360 `thumb`. `rating_key`: ASCII digits.
    pub async fn poster(&self, rating_key: &str) -> Option<(Vec<u8>, String)> {
        self.photo(rating_key, "thumb", 240, 360).await
    }

    /// Backdrop through the photo transcoder, 720x405 `art`.
    pub async fn art(&self, rating_key: &str) -> Option<(Vec<u8>, String)> {
        self.photo(rating_key, "art", 720, 405).await
    }
}
