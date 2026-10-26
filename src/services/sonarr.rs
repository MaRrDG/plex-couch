//! Sonarr v3 (new; `services.sonarr` in the config defaults: `http://127.0.0.1:8989`, key
//! from `C:\ProgramData\Sonarr\config.xml`). Header `X-Api-Key`.
//!
//! TODO(Phase 2b): the calls the jobs need (import lists, `ImportListSync`, series, history,
//! episodes/episode files, manual import, `RescanSeries`, delete with `deleteFiles=true`).

use crate::services::Client;

pub struct Sonarr {
    pub c: Client,
}

impl Sonarr {
    pub fn new(c: Client) -> Sonarr {
        Sonarr { c }
    }

    pub fn key(&self) -> String {
        self.c.keys.get("sonarr")
    }

    /// `{sonarr.url}/api/v3<path>`.
    pub fn api_url(&self, path: &str) -> String {
        self.c.url("sonarr", &format!("/api/v3{path}"))
    }
}
