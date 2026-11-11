//! Prowlarr (spec 5.4): `GET {prowlarr.url}/ping`, no key, 2.5 s, redirects followed.
//!
//! Job-only (`jobs::watchlist_alt`): text search, `GET /api/v1/search?query=..&type=search`
//! with header `X-Api-Key` (`services.prowlarr.api_key`, else `<ApiKey>` from `config_xml`;
//! a redirect is never followed); the key is never logged.

use crate::services::arr_jobs::urlencode;
use crate::services::{Client, UpResult, UpstreamError, TIMEOUT};
use reqwest::Method;
use serde_json::Value;
use std::time::Duration;

/// A search fans out to every indexer; Prowlarr waits for the slow ones.
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Prowlarr {
    pub c: Client,
}

impl Prowlarr {
    pub fn new(c: Client) -> Prowlarr {
        Prowlarr { c }
    }

    /// Up when this is Ok (HTTP >= 400 counts as down).
    pub async fn ping(&self) -> UpResult<()> {
        let url = self.c.url("prowlarr", "/ping");
        self.c.http.request(Method::GET, &url, &[], None, TIMEOUT).await.map(|_| ())
    }

    /// The releases every indexer returns for a text query (a JSON array; an empty body is
    /// none, anything else a body error).
    pub async fn search(&self, query: &str) -> UpResult<Vec<Value>> {
        let key = self.c.keys.get("prowlarr");
        let url = self.c.url("prowlarr", &format!("/api/v1/search?query={}&type=search", urlencode(query)));
        match self.c.http.get_json(&url, &[("X-Api-Key", &key)], SEARCH_TIMEOUT).await? {
            None => Ok(Vec::new()),
            Some(Value::Array(a)) => Ok(a),
            Some(_) => Err(UpstreamError::Body),
        }
    }
}
