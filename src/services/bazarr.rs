//! Bazarr (spec 5.3): `GET {bazarr.url}/api/movies`, header `X-API-KEY`, timeout 4 s,
//! `.data`, cached 60 s.
//!
//! TODO(Phase 2a): implement.

use crate::services::{Client, UpResult, UpstreamError};
use serde_json::Value;

pub struct Bazarr {
    pub c: Client,
}

impl Bazarr {
    pub fn new(c: Client) -> Bazarr {
        Bazarr { c }
    }

    pub fn key(&self) -> String {
        self.c.keys.get("bazarr")
    }

    pub async fn movies(&self) -> UpResult<Vec<Value>> {
        Err(UpstreamError::Other("bazarr: not implemented"))
    }
}
