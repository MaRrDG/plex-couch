//! Prowlarr (spec 5.4): `GET {prowlarr.url}/ping`, no key, 2.5 s, redirects followed.
//!
//! TODO(Phase 2a): implement.

use crate::services::{Client, UpResult, UpstreamError};

pub struct Prowlarr {
    pub c: Client,
}

impl Prowlarr {
    pub fn new(c: Client) -> Prowlarr {
        Prowlarr { c }
    }

    /// Up when this is Ok.
    pub async fn ping(&self) -> UpResult<()> {
        Err(UpstreamError::Other("prowlarr: not implemented"))
    }
}
