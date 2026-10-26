//! Strip Dolby Vision, every 5 minutes (replaces `strip-dv.ps1`; plan "New behaviour" 2,
//! spec 12.2; plan fix 23: `.mkv` only).
//!
//! TODO(Phase 2b): one job at a time over `D:\Media\Movies` and `D:\Media\Seriale`, same
//! ffmpeg command, checks, seen-file and log locations; then Radarr `RescanMovie` / Sonarr
//! `RescanSeries` and a Plex path refresh of section 1 (Filme) or 2 (Seriale) with the token
//! from HKCU in a header (plan fix 24).

use crate::app::AppState;
use crate::jobs::Stop;
use std::sync::Arc;
use std::time::Duration;

pub const INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Start the loop. TODO(Phase 2b): implement.
pub fn spawn(app: Arc<AppState>, stop: Stop) {
    let _ = (app, stop);
}
