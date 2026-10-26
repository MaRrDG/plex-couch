//! Job-only Plex calls: library items with their file paths and `viewCount`, a path
//! refresh, and the plex.tv Watchlist. The token goes in the `X-Plex-Token` header only
//! (plan fix 24) and is never logged.

use crate::services::arr_jobs::urlencode;
use crate::services::{Client, UpResult, UpstreamError};
use reqwest::Method;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

pub const PLEX_TIMEOUT: Duration = Duration::from_secs(20);
/// The Watchlist lives on plex.tv's discover provider, not on the local server.
pub const DISCOVER_URL: &str = "https://discover.provider.plex.tv";
const PAGE: usize = 100;

#[derive(Clone)]
pub struct PlexJobs {
    pub c: Client,
}

/// One Watchlist entry: its type (`movie`/`show`) and its external ids (`tvdb://123`).
#[derive(Debug, Clone, PartialEq)]
pub struct WatchItem {
    pub kind: String,
    pub title: String,
    pub guids: Vec<String>,
}

impl WatchItem {
    /// The tvdb id from the `Guid` list (`tvdb://<n>`).
    pub fn tvdb(&self) -> Option<i64> {
        self.guids.iter().find_map(|g| g.strip_prefix("tvdb://").and_then(|n| n.trim().parse().ok()))
    }
}

/// `D:\x\y.mkv` and `d:/X/y.mkv` compare equal (Windows paths are case-insensitive).
pub fn path_key(p: &str) -> String {
    p.trim().replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

impl PlexJobs {
    pub fn new(c: Client) -> PlexJobs {
        PlexJobs { c }
    }

    fn headers(token: &str) -> Vec<(&str, &str)> {
        if token.is_empty() {
            vec![]
        } else {
            vec![("X-Plex-Token", token)]
        }
    }

    /// `GET {plex.url}/library/sections/<n>/all[?type=4]`: `path_key(file) -> viewCount`
    /// for every media part. `episodes` lists episodes (type 4) instead of top-level items.
    pub async fn view_counts(&self, section: u32, episodes: bool) -> UpResult<HashMap<String, i64>> {
        let token = self.c.keys.get("plex");
        let q = if episodes { "?type=4" } else { "" };
        let url = self.c.url("plex", &format!("/library/sections/{section}/all{q}"));
        let v = self.c.http.get_json(&url, &Self::headers(&token), PLEX_TIMEOUT).await?.ok_or(UpstreamError::Body)?;
        let mc = v.get("MediaContainer").ok_or(UpstreamError::Body)?;
        let mut out = HashMap::new();
        for m in mc.get("Metadata").and_then(Value::as_array).into_iter().flatten() {
            let views = m.get("viewCount").and_then(Value::as_i64).unwrap_or(0);
            for media in m.get("Media").and_then(Value::as_array).into_iter().flatten() {
                for part in media.get("Part").and_then(Value::as_array).into_iter().flatten() {
                    if let Some(f) = part.get("file").and_then(Value::as_str) {
                        out.insert(path_key(f), views);
                    }
                }
            }
        }
        Ok(out)
    }

    /// `GET {plex.url}/library/sections/<n>/refresh?path=<EscapeDataString(dir)>`.
    pub async fn refresh_path(&self, section: u32, dir: &str) -> UpResult<()> {
        let token = self.c.keys.get("plex");
        if token.is_empty() {
            return Err(UpstreamError::Other("no Plex token"));
        }
        let url = self.c.url("plex", &format!("/library/sections/{section}/refresh?path={}", urlencode(dir)));
        self.c.http.request(Method::GET, &url, &Self::headers(&token), None, PLEX_TIMEOUT).await?;
        Ok(())
    }

    /// Every Watchlist entry (`<discover>/library/sections/watchlist/all?includeGuids=1`,
    /// paged). An error, a missing `MediaContainer` or fewer items than `totalSize` is an
    /// error: the caller must never act on a partial list.
    pub async fn watchlist(&self, discover: &str) -> UpResult<Vec<WatchItem>> {
        let token = self.c.keys.get("plex");
        if token.is_empty() {
            return Err(UpstreamError::Other("no Plex token"));
        }
        let base = discover.trim_end_matches('/');
        let mut items = Vec::new();
        let mut start = 0usize;
        loop {
            let url = format!(
                "{base}/library/sections/watchlist/all?includeGuids=1&X-Plex-Container-Start={start}&X-Plex-Container-Size={PAGE}"
            );
            let v = self.c.http.get_json(&url, &Self::headers(&token), PLEX_TIMEOUT).await?.ok_or(UpstreamError::Body)?;
            let mc = v.get("MediaContainer").ok_or(UpstreamError::Body)?;
            let total = mc.get("totalSize").and_then(Value::as_u64).ok_or(UpstreamError::Body)? as usize;
            let page: Vec<&Value> = mc.get("Metadata").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
            for m in &page {
                let guids = m
                    .get("Guid")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|g| g.get("id").and_then(Value::as_str).map(String::from)).collect())
                    .unwrap_or_default();
                items.push(WatchItem {
                    kind: m.get("type").and_then(Value::as_str).unwrap_or("").to_string(),
                    title: m.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
                    guids,
                });
            }
            start += page.len();
            if start >= total {
                return Ok(items);
            }
            if page.is_empty() || start > 10_000 {
                return Err(UpstreamError::Other("watchlist: incomplete paging"));
            }
        }
    }
}
