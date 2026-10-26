//! PC info for `/api/status` and the disk-low check (spec 6.3), own IPv4s (spec 6.4).
//!
//! TODO(Phase 2a): implement:
//! - [`uptime_seconds`]: `GetTickCount64() / 1000`, None on error.
//! - [`disks`]: per configured letter, `GetDiskFreeSpaceExW` on `<letter>\`;
//!   `{"drive", "free_gb", "total_gb", "used_pct"}` rounded to 1 decimal (Python
//!   round-half-even: `crate::json::py_round`); an error skips the disk.
//! - [`free_gb`]: the same, unrounded, None on error.
//! - [`own_ipv4s`]: every IPv4 of the host name plus the local address of a UDP "connect"
//!   to 10.255.255.255:1 (no packet is sent).

use crate::config::Config;
use serde_json::{json, Value};
use std::collections::HashSet;

/// `{"hostname", "uptime", "disks", "platform": "win32"}`.
pub fn pc_info(config: &Config) -> Value {
    let letters: Vec<String> = match config.get("disks") {
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        _ => Vec::new(),
    };
    json!({"hostname": hostname(), "uptime": uptime_seconds(), "disks": disks(&letters), "platform": "win32"})
}

/// TODO(Phase 2a): the real host name (`socket.gethostname()`).
pub fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

/// TODO(Phase 2a).
pub fn uptime_seconds() -> Option<u64> {
    None
}

/// TODO(Phase 2a).
pub fn disks(letters: &[String]) -> Vec<Value> {
    let _ = letters;
    Vec::new()
}

/// TODO(Phase 2a).
pub fn free_gb(drive: &str) -> Option<f64> {
    let _ = drive;
    None
}

/// TODO(Phase 2a).
pub fn own_ipv4s() -> HashSet<String> {
    HashSet::new()
}
