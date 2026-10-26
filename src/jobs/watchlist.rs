//! Watchlist sync, every 3 minutes (replaces `radarr-watchlist-sync.ps1`; plan "New
//! behaviour" 1, spec 12.1).
//!
//! TODO(Phase 2b):
//! - Radarr: `ImportListSync` with `definitionId` for every enabled import list.
//! - Sonarr: the same for its enabled lists; then delete (`deleteFiles=true`) list series
//!   no longer on the Plex Watchlist (guard: a failed or empty Watchlist fetch deletes
//!   nothing).
//! - Auto import of downloads blocked with "matched to movie by ID" / "matched to series by
//!   ID": manualimport for that downloadId, import the files without rejections.

use crate::app::AppState;
use crate::jobs::Stop;
use std::sync::Arc;
use std::time::Duration;

pub const INTERVAL: Duration = Duration::from_secs(3 * 60);

/// Start the loop. TODO(Phase 2b): implement.
pub fn spawn(app: Arc<AppState>, stop: Stop) {
    let _ = (app, stop);
}
