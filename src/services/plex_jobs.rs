//! Job-only Plex calls: library items with their file paths and `viewCount`, a path
//! refresh, and the plex.tv Watchlist. The token goes in the `X-Plex-Token` header only
//! (plan fix 24) and is never logged.

use crate::services::arr_jobs::urlencode;
use crate::services::{Client, UpResult, UpstreamError};
use reqwest::Method;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
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
        self.guid_num("tvdb")
    }

    pub fn tmdb(&self) -> Option<i64> {
        self.guid_num("tmdb")
    }

    /// `imdb://tt0088763` -> `tt0088763` (lower-case).
    pub fn imdb(&self) -> Option<String> {
        self.guids.iter().find_map(|g| {
            let rest = g.trim().strip_prefix("imdb://")?;
            let id: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
            (id.len() > 2).then(|| id.to_lowercase())
        })
    }

    /// `<scheme>://<digits>[anything]` -> the digits (suffixes and queries ignored).
    fn guid_num(&self, scheme: &str) -> Option<i64> {
        let pre = format!("{scheme}://");
        self.guids.iter().find_map(|g| {
            let rest = g.trim().strip_prefix(&pre)?;
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok().filter(|n: &i64| *n > 0)
        })
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
        Ok(self.view_info(section, episodes).await?.into_iter().map(|(k, (v, _))| (k, v)).collect())
    }

    /// [`PlexJobs::view_counts`] with `lastViewedAt` (unix seconds, 0 if never):
    /// `path_key(file) -> (viewCount, lastViewedAt)`.
    pub async fn view_info(&self, section: u32, episodes: bool) -> UpResult<HashMap<String, (i64, i64)>> {
        let token = self.c.keys.get("plex");
        let q = if episodes { "?type=4" } else { "" };
        let url = self.c.url("plex", &format!("/library/sections/{section}/all{q}"));
        let v = self.c.http.get_json(&url, &Self::headers(&token), PLEX_TIMEOUT).await?.ok_or(UpstreamError::Body)?;
        let mc = v.get("MediaContainer").ok_or(UpstreamError::Body)?;
        let mut out = HashMap::new();
        for m in mc.get("Metadata").and_then(Value::as_array).into_iter().flatten() {
            let views = m.get("viewCount").and_then(Value::as_i64).unwrap_or(0);
            let last = m.get("lastViewedAt").and_then(Value::as_i64).unwrap_or(0);
            for media in m.get("Media").and_then(Value::as_array).into_iter().flatten() {
                for part in media.get("Part").and_then(Value::as_array).into_iter().flatten() {
                    if let Some(f) = part.get("file").and_then(Value::as_str) {
                        out.insert(path_key(f), (views, last));
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
        let mut keys: HashSet<String> = HashSet::new();
        let mut first_total: Option<usize> = None;
        let mut start = 0usize;
        loop {
            let url = format!(
                "{base}/library/sections/watchlist/all?includeGuids=1&X-Plex-Container-Start={start}&X-Plex-Container-Size={PAGE}"
            );
            let v = self.c.http.get_json(&url, &Self::headers(&token), PLEX_TIMEOUT).await?.ok_or(UpstreamError::Body)?;
            let mc = v.get("MediaContainer").ok_or(UpstreamError::Body)?;
            let total = mc.get("totalSize").and_then(Value::as_u64).ok_or(UpstreamError::Body)? as usize;
            if *first_total.get_or_insert(total) != total {
                return Err(UpstreamError::Other("watchlist: totalSize changed while paging"));
            }
            let page: Vec<&Value> = mc.get("Metadata").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
            for m in &page {
                let guids: Vec<String> = m
                    .get("Guid")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|g| g.get("id").and_then(Value::as_str).map(String::from)).collect())
                    .unwrap_or_default();
                let key = ["ratingKey", "guid"]
                    .iter()
                    .find_map(|k| m.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from))
                    .ok_or(UpstreamError::Other("watchlist: item without ratingKey/guid"))?;
                if !keys.insert(key) {
                    continue;
                }
                items.push(WatchItem {
                    kind: m.get("type").and_then(Value::as_str).unwrap_or("").to_string(),
                    title: m.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
                    guids,
                });
            }
            start += page.len();
            if start >= total {
                // Duplicates across pages (the list shifted while paging) or a short list:
                // never act on it.
                if items.len() != total {
                    return Err(UpstreamError::Other("watchlist: item count does not match totalSize"));
                }
                return Ok(items);
            }
            if page.is_empty() || start > 10_000 {
                return Err(UpstreamError::Other("watchlist: incomplete paging"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::testutil::{app, fake, ok, Req};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn item(rk: &str, tvdb: &str) -> Value {
        json!({"type": "show", "ratingKey": rk, "title": rk, "Guid": [{"id": tvdb}]})
    }

    #[test]
    fn guid_parsing() {
        let w = WatchItem { kind: "show".into(), title: "x".into(), guids: vec!["imdb://tt0088763".into(), "tmdb://105?lang=en".into(), "tvdb://299/extra".into()] };
        assert_eq!((w.tvdb(), w.tmdb(), w.imdb()), (Some(299), Some(105), Some("tt0088763".to_string())));
        let bad = WatchItem { kind: "show".into(), title: "x".into(), guids: vec!["tvdb://".into(), "tvdb://abc".into()] };
        assert_eq!(bad.tvdb(), None);
    }

    async fn pages(p: Vec<Value>) -> Result<Vec<WatchItem>, UpstreamError> {
        let p = Arc::new(Mutex::new(p));
        let f = fake(move |r: &Req| {
            let start: usize = r.path.split("X-Plex-Container-Start=").nth(1).and_then(|s| s.split('&').next()).and_then(|s| s.parse().ok()).unwrap_or(0);
            let pages = p.lock().unwrap();
            ok(pages.iter().find(|pg| pg["start"] == start).cloned().unwrap_or(json!({}))["body"].clone())
        })
        .await;
        let (a, _d) = app(&f.base, false, json!({}));
        PlexJobs::new(a.services.plex.c.clone()).watchlist(&f.base).await
    }

    fn page(start: usize, total: usize, items: Vec<Value>) -> Value {
        json!({"start": start, "body": {"MediaContainer": {"totalSize": total, "size": items.len(), "Metadata": items}}})
    }

    #[tokio::test]
    async fn paging_dedupes_and_checks_the_total() {
        // Two clean pages.
        let ok2 = pages(vec![page(0, 2, vec![item("1", "tvdb://1")]), page(1, 2, vec![item("2", "tvdb://2")])]).await;
        assert_eq!(ok2.unwrap().len(), 2);
        // The list shifted: item 1 repeats on page 2, so only 1 unique of 2.
        let dup = pages(vec![page(0, 2, vec![item("1", "tvdb://1")]), page(1, 2, vec![item("1", "tvdb://1")])]).await;
        assert!(dup.is_err());
        // totalSize changed between pages.
        let moved = pages(vec![page(0, 3, vec![item("1", "tvdb://1")]), page(1, 2, vec![item("2", "tvdb://2"), item("3", "tvdb://3")])]).await;
        assert!(moved.is_err());
        // An item without ratingKey or guid.
        let anon = pages(vec![page(0, 1, vec![json!({"type": "show"})])]).await;
        assert!(anon.is_err());
    }
}
