//! Bazarr (spec 5.3): `GET {bazarr.url}/api/movies`, header `X-API-KEY`, timeout 4 s,
//! `.data`, cached 60 s. For `jobs::subs`: uncached `.data` reads and the `PATCH` search.

use crate::services::cache::TtlCache;
use crate::services::{Client, UpResult, UpstreamError};
use reqwest::Method;
use serde_json::Value;
use std::time::Duration;

pub const TIMEOUT: Duration = Duration::from_secs(4);
pub const MOVIES_TTL: Duration = Duration::from_secs(60);

pub struct Bazarr {
    pub c: Client,
    movies: TtlCache<Vec<Value>>,
}

impl Bazarr {
    pub fn new(c: Client) -> Bazarr {
        Bazarr { c, movies: TtlCache::new(MOVIES_TTL) }
    }

    pub fn key(&self) -> String {
        self.c.keys.get("bazarr")
    }

    /// `(data or {}).get("data") or []`, cached 60 s.
    pub async fn movies(&self) -> UpResult<Vec<Value>> {
        self.movies
            .get(|| async {
                let key = self.key();
                let url = self.c.url("bazarr", "/api/movies");
                let data = self.c.http.get_json(&url, &[("X-API-KEY", key.as_str())], TIMEOUT).await?;
                Ok(match data {
                    Some(Value::Object(mut m)) => match m.remove("data") {
                        Some(Value::Array(a)) => a,
                        _ => Vec::new(),
                    },
                    _ => Vec::new(),
                })
            })
            .await
    }

    /// Forget the cached list (tests).
    pub fn clear_cache(&self) {
        self.movies.clear();
    }

    /// Job call: `GET {bazarr.url}<path_and_query>` -> `.data` (uncached). A body without a
    /// `data` array is an error (never "nothing wanted").
    pub async fn data(&self, path_and_query: &str, timeout: Duration) -> UpResult<Vec<Value>> {
        Ok(self.data_total(path_and_query, timeout).await?.0)
    }

    /// [`Bazarr::data`] with the `total` Bazarr reports (if any), to spot a partial list.
    pub async fn data_total(&self, path_and_query: &str, timeout: Duration) -> UpResult<(Vec<Value>, Option<u64>)> {
        let key = self.key();
        let url = self.c.url("bazarr", path_and_query);
        match self.c.http.get_json(&url, &[("X-API-KEY", key.as_str())], timeout).await? {
            Some(Value::Object(mut m)) => {
                let total = m.get("total").and_then(Value::as_u64);
                match m.remove("data") {
                    Some(Value::Array(a)) => Ok((a, total)),
                    _ => Err(UpstreamError::Body),
                }
            }
            _ => Err(UpstreamError::Body),
        }
    }

    /// Job call: `PATCH {bazarr.url}<path_and_query>` (a subtitle search; Bazarr answers when
    /// the search is over).
    pub async fn patch(&self, path_and_query: &str, timeout: Duration) -> UpResult<()> {
        let key = self.key();
        let url = self.c.url("bazarr", path_and_query);
        self.c.http.request(Method::PATCH, &url, &[("X-API-KEY", key.as_str())], None, timeout).await?;
        Ok(())
    }
}
