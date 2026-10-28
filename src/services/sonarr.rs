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

    async fn cover(&self, series_id: &str, names: [&str; 2]) -> Option<(Vec<u8>, String)> {
        if series_id.is_empty() || !series_id.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let key = self.key();
        for name in names {
            let url = self.api_url(&format!("/mediacover/{series_id}/{name}"));
            if let Some(img) = self.c.http.get_image(&url, &[("X-Api-Key", key.as_str())]).await {
                return Some(img);
            }
        }
        None
    }

    /// `mediacover/<id>/poster-500.jpg`, then `poster.jpg` (the Seriale grid).
    pub async fn poster(&self, series_id: &str) -> Option<(Vec<u8>, String)> {
        self.cover(series_id, ["poster-500.jpg", "poster.jpg"]).await
    }

    /// `mediacover/<id>/fanart-360.jpg`, then `fanart.jpg` (the series header).
    pub async fn fanart(&self, series_id: &str) -> Option<(Vec<u8>, String)> {
        self.cover(series_id, ["fanart-360.jpg", "fanart.jpg"]).await
    }
}
