//! Seed keeper, every 5 minutes: the only thing that deletes torrents (plan "New
//! behaviour" 3).
//!
//! ## For `/api/status` (Phase 2a)
//! ```ignore
//! pub fn snapshot(app: &AppState) -> HashMap<String, SeedInfo>
//! pub struct SeedInfo { pub seconds: f64, pub watched: Option<bool>, pub waiting_watch: bool }
//! ```
//! Keyed by the torrent hash in lowercase (as qBittorrent reports it). `seconds` is the
//! ledger's seeded time (use it for the H&R progress instead of qBittorrent's
//! `seeding_time`), `watched` is `Some(true/false)` once the torrent is mapped to Radarr /
//! Sonarr and Plex, `None` when unknown; `waiting_watch` is "H&R done, not watched yet" (the
//! PC tab shows "gata, aștept să-l vezi"). Cheap: an in-memory copy of the last tick, read
//! from `seed-ledger.json` on first use. A torrent missing from the map has no ledger entry
//! yet (fall back to qBittorrent's numbers, `watched: null`, `waiting_watch: false`).
//!
//! TODO(Phase 2b, in progress): the tick (accrual, H&R + watched-or-gone, deletion).

use crate::app::AppState;
use crate::jobs::Stop;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub const INTERVAL: Duration = Duration::from_secs(5 * 60);
/// The ledger file, next to `state.local.json`.
pub const LEDGER_FILE: &str = "seed-ledger.json";

/// What `/api/status` shows for one seeding torrent.
#[derive(Debug, Clone, PartialEq)]
pub struct SeedInfo {
    /// Seconds seeded according to the ledger.
    pub seconds: f64,
    /// Watched in Plex (`None`: unknown mapping or not checked yet).
    pub watched: Option<bool>,
    /// H&R satisfied but not watched yet.
    pub waiting_watch: bool,
}

/// `<state dir>\seed-ledger.json`.
pub fn ledger_path(app: &AppState) -> PathBuf {
    app.state_file.path().parent().map(Path::to_path_buf).unwrap_or_default().join(LEDGER_FILE)
}

fn snapshots() -> &'static Mutex<HashMap<PathBuf, HashMap<String, SeedInfo>>> {
    static S: OnceLock<Mutex<HashMap<PathBuf, HashMap<String, SeedInfo>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The seed info of every torrent in the ledger (see the module docs).
pub fn snapshot(app: &AppState) -> HashMap<String, SeedInfo> {
    let path = ledger_path(app);
    let mut g = snapshots().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = g.get(&path) {
        return s.clone();
    }
    let s = infos_of(&load_ledger(&path));
    g.insert(path, s.clone());
    s
}

fn publish(path: &Path, infos: HashMap<String, SeedInfo>) {
    snapshots().lock().unwrap_or_else(|e| e.into_inner()).insert(path.to_path_buf(), infos);
}

/// `{"torrents": {"<hash>": {...}}}`; a missing or broken file is empty.
pub fn load_ledger(path: &Path) -> Map<String, Value> {
    let Ok(bytes) = std::fs::read(path) else { return Map::new() };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(m)) => match m.get("torrents") {
            Some(Value::Object(t)) => t.clone(),
            _ => Map::new(),
        },
        _ => Map::new(),
    }
}

fn infos_of(torrents: &Map<String, Value>) -> HashMap<String, SeedInfo> {
    torrents
        .iter()
        .map(|(h, e)| {
            let info = SeedInfo {
                seconds: e.get("seconds").and_then(Value::as_f64).unwrap_or(0.0),
                watched: e.get("watched").and_then(Value::as_bool),
                waiting_watch: e.get("waiting_watch").and_then(Value::as_bool).unwrap_or(false),
            };
            (h.to_ascii_lowercase(), info)
        })
        .collect()
}

/// Start the loop. TODO(Phase 2b): implement.
pub fn spawn(app: Arc<AppState>, stop: Stop) {
    let _ = (app, stop, publish as fn(&Path, HashMap<String, SeedInfo>));
}
