//! Seed keeper, every 5 minutes: the only thing that deletes torrents (plan "New
//! behaviour" 3).
//!
//! TODO(Phase 2b): the seeded-seconds ledger (state file key or `seed-ledger.json`), the
//! H&R + watched-or-gone rule, deletion with `deleteFiles=true`, and the `watched` /
//! `waiting_watch` fields for `/api/status` seeding items (expose a function Phase 2a's
//! status code can call, e.g. `pub fn ledger_seconds(app, hash) -> Option<u64>`).

use crate::app::AppState;
use crate::jobs::Stop;
use std::sync::Arc;
use std::time::Duration;

pub const INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Start the loop. TODO(Phase 2b): implement.
pub fn spawn(app: Arc<AppState>, stop: Stop) {
    let _ = (app, stop);
}
