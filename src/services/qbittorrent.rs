//! qBittorrent Web API (spec 5.5): `/api/v2/torrents/info`, log in on HTTP 403 and retry
//! once; FileList filter per torrent (plan fix 13: FileList if ANY of its trackers is). The
//! SID cookie is kept in memory; tracker URLs are never stored, logged or returned.
//!
//! TODO(Phase 2a): implement (login serialized by a lock, `Referer`/`Origin` headers,
//! per-hash tracker cache). Phase 2b (seed keeper) also needs: tracker status, delete with
//! `deleteFiles=true` — add those in a separate file or impl block.

use crate::services::{Client, Http, UpResult, UpstreamError};
use serde_json::Value;

pub struct Qbittorrent {
    pub c: Client,
}

impl Qbittorrent {
    pub fn new(c: Client) -> Qbittorrent {
        Qbittorrent { c }
    }

    /// The client every qBittorrent request goes through (redirect rule included).
    pub fn http(&self) -> &Http {
        &self.c.http
    }

    /// FileList torrents from `/api/v2/torrents/info`.
    pub async fn filelist_torrents(&self) -> UpResult<Vec<Value>> {
        Err(UpstreamError::Other("qbittorrent: not implemented"))
    }
}
