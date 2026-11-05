//! Job-only Plex calls: library items with their file paths and `viewCount`, a path
//! refresh, a part's subtitle streams and their selection, and the plex.tv Watchlist. The token goes in the `X-Plex-Token` header only
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

/// A media part as the library listing shows it: its item's `ratingKey` and the part `id`.
#[derive(Debug, Clone, PartialEq)]
pub struct PartRef {
    pub rating_key: String,
    pub part_id: i64,
}

/// One subtitle stream of a part (`/library/metadata/<ratingKey>`).
#[derive(Debug, Clone, PartialEq)]
pub struct SubStream {
    pub id: i64,
    /// `languageCode` (ISO 639-2, e.g. `ron`, `eng`), empty if Plex gives none.
    pub language: String,
    /// The one Plex plays.
    pub selected: bool,
    /// A sidecar file (it has a `key`), e.g. our `<video>.ro.srt`.
    pub external: bool,
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
        let mut out = HashMap::new();
        self.each_part(section, episodes, |m, file, _| {
            let views = m.get("viewCount").and_then(Value::as_i64).unwrap_or(0);
            let last = m.get("lastViewedAt").and_then(Value::as_i64).unwrap_or(0);
            out.insert(path_key(file), (views, last));
        })
        .await?;
        Ok(out)
    }

    /// The same listing as [`PlexJobs::view_info`]: `path_key(file) -> (ratingKey, part id)`
    /// for every media part (a part without a ratingKey or an id is left out).
    pub async fn part_refs(&self, section: u32, episodes: bool) -> UpResult<HashMap<String, PartRef>> {
        let mut out = HashMap::new();
        self.each_part(section, episodes, |m, file, part| {
            let rating_key = m.get("ratingKey").and_then(|v| v.as_str().map(String::from).or_else(|| v.as_i64().map(|n| n.to_string())));
            if let (Some(rating_key), Some(part_id)) = (rating_key.filter(|k| !k.is_empty()), part.get("id").and_then(Value::as_i64)) {
                out.insert(path_key(file), PartRef { rating_key, part_id });
            }
        })
        .await?;
        Ok(out)
    }

    /// `GET {plex.url}/library/sections/<n>/all[?type=4]`, then `f(item, file, part)` for
    /// every media part with a file. `episodes` lists episodes (type 4) instead of top-level
    /// items.
    async fn each_part(&self, section: u32, episodes: bool, mut f: impl FnMut(&Value, &str, &Value)) -> UpResult<()> {
        let token = self.c.keys.get("plex");
        let q = if episodes { "?type=4" } else { "" };
        let url = self.c.url("plex", &format!("/library/sections/{section}/all{q}"));
        let v = self.c.http.get_json(&url, &Self::headers(&token), PLEX_TIMEOUT).await?.ok_or(UpstreamError::Body)?;
        let mc = v.get("MediaContainer").ok_or(UpstreamError::Body)?;
        for m in mc.get("Metadata").and_then(Value::as_array).into_iter().flatten() {
            for media in m.get("Media").and_then(Value::as_array).into_iter().flatten() {
                for part in media.get("Part").and_then(Value::as_array).into_iter().flatten() {
                    if let Some(file) = part.get("file").and_then(Value::as_str) {
                        f(m, file, part);
                    }
                }
            }
        }
        Ok(())
    }

    /// `GET {plex.url}/library/metadata/<ratingKey>`: the subtitle streams (`streamType` 3)
    /// of the part `part_id`; None if the item has no such part.
    pub async fn part_subtitles(&self, rating_key: &str, part_id: i64) -> UpResult<Option<Vec<SubStream>>> {
        let token = self.c.keys.get("plex");
        if token.is_empty() {
            return Err(UpstreamError::Other("no Plex token"));
        }
        let url = self.c.url("plex", &format!("/library/metadata/{}", urlencode(rating_key)));
        let v = self.c.http.get_json(&url, &Self::headers(&token), PLEX_TIMEOUT).await?.ok_or(UpstreamError::Body)?;
        let mc = v.get("MediaContainer").ok_or(UpstreamError::Body)?;
        let arr = |v: &Value, k: &str| v.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
        let part = arr(mc, "Metadata")
            .iter()
            .flat_map(|m| arr(m, "Media"))
            .flat_map(|media| arr(&media, "Part"))
            .find(|p| p.get("id").and_then(Value::as_i64) == Some(part_id));
        let Some(part) = part else { return Ok(None) };
        let subs = arr(&part, "Stream")
            .iter()
            .filter(|s| s.get("streamType").and_then(Value::as_i64) == Some(3))
            .filter_map(|s| {
                Some(SubStream {
                    id: s.get("id")?.as_i64()?,
                    language: s.get("languageCode").and_then(Value::as_str).unwrap_or("").to_string(),
                    selected: s.get("selected").and_then(Value::as_bool).unwrap_or(false),
                    external: s.get("key").and_then(Value::as_str).is_some_and(|k| !k.is_empty()),
                })
            })
            .collect();
        Ok(Some(subs))
    }

    /// `PUT {plex.url}/library/parts/<id>?subtitleStreamID=<stream>&allParts=1`: the subtitle
    /// Plex plays for that part (for the token's account).
    pub async fn select_subtitle(&self, part_id: i64, stream_id: i64) -> UpResult<()> {
        let token = self.c.keys.get("plex");
        if token.is_empty() {
            return Err(UpstreamError::Other("no Plex token"));
        }
        let url = self.c.url("plex", &format!("/library/parts/{part_id}?subtitleStreamID={stream_id}&allParts=1"));
        self.c.http.request(Method::PUT, &url, &Self::headers(&token), None, PLEX_TIMEOUT).await?;
        Ok(())
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
