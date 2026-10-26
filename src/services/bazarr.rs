//! Bazarr (spec 5.3): `GET {bazarr.url}/api/movies`, header `X-API-KEY`, timeout 4 s,
//! `.data`, cached 60 s.

use crate::services::cache::TtlCache;
use crate::services::{Client, UpResult};
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
}
