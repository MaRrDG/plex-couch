//! Sonarr v3 (new; `services.sonarr` in the config defaults: `http://127.0.0.1:8989`, key
//! from `C:\ProgramData\Sonarr\config.xml`). Header `X-Api-Key`.
//!
//! Only the background jobs use Sonarr; their calls (import lists, `ImportListSync`, queue,
//! manual import, series, history, episodes / episode files, `RescanSeries`, delete with
//! `deleteFiles=true`) live in [`crate::services::arr_jobs::Arr`], shared with Radarr.

use crate::services::arr_jobs::Arr;
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

    /// The job API client.
    pub fn arr(&self) -> Arr {
        Arr::sonarr(self.c.clone())
    }
}
