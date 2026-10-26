//! `state.local.json` next to the config (spec 2.5): what was already notified and the IPs
//! that logged in. A missing, broken or non-object file reads as `{}`. Writes are
//! `json.dump(indent=2, ensure_ascii=False, sort_keys=True)` + `"\n"`, atomic via `.tmp`.
//!
//! Keys: `known_ips` (login guard), `movie_ready` and `disk_low` (notifications, Phase 2a),
//! and whatever the jobs add (Phase 2b, e.g. a seed ledger): [`StateFile::update`] merges
//! top-level keys under one lock, so writers never clobber each other.

use crate::json::{dump_pretty, sorted};
use crate::log_warning;
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const STATE_FILE: &str = "state.local.json";

pub struct StateFile {
    path: PathBuf,
    lock: Mutex<()>,
}

impl StateFile {
    pub fn new(path: &Path) -> StateFile {
        StateFile { path: crate::config::absolute(path), lock: Mutex::new(()) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Map<String, Value> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.load_locked()
    }

    fn load_locked(&self) -> Map<String, Value> {
        // Python opens it as plain utf-8: a BOM makes json.load fail -> {}.
        self.try_load_locked().unwrap_or_default()
    }

    /// Like [`StateFile::load`], but only a missing file is empty: an unreadable or
    /// unparsable file (or a non-object) is an error naming its type. The background jobs
    /// use this so a broken file never makes them forget what they already did.
    pub fn try_load(&self) -> Result<Map<String, Value>, &'static str> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.try_load_locked()
    }

    fn try_load_locked(&self) -> Result<Map<String, Value>, &'static str> {
        let bytes = match fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
            Err(e) => return Err(io_error_name(&e)),
        };
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(m)) => Ok(m),
            Ok(_) => Err("TypeError"),
            Err(_) => Err("JSONDecodeError"),
        }
    }

    pub fn save(&self, data: &Map<String, Value>) {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.save_locked(data);
    }

    /// Merge the given top-level keys into the file (load + save under one lock). An
    /// existing file that cannot be read or parsed is left alone (WARNING), so one bad read
    /// never wipes the other keys.
    pub fn update(&self, changes: Map<String, Value>) {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut data = match self.try_load_locked() {
            Ok(d) => d,
            Err(t) => {
                log_warning!("cannot update the state file, it is unreadable: {t}");
                return;
            }
        };
        for (k, v) in changes {
            data.insert(k, v);
        }
        self.save_locked(&data);
    }

    fn save_locked(&self, data: &Map<String, Value>) {
        let mut bytes = dump_pretty(&sorted(&Value::Object(data.clone())));
        bytes.push(b'\n');
        let tmp = crate::config::tmp_path(&self.path);
        let r = fs::write(&tmp, &bytes).and_then(|_| fs::rename(&tmp, &self.path));
        if let Err(e) = r {
            log_warning!("cannot save the state file: {}", io_error_name(&e));
        }
    }
}

/// The Python exception class name for an io error (logs name the type, never paths).
pub fn io_error_name(e: &std::io::Error) -> &'static str {
    use std::io::ErrorKind::*;
    match e.kind() {
        NotFound => "FileNotFoundError",
        PermissionDenied => "PermissionError",
        AlreadyExists => "FileExistsError",
        TimedOut => "TimeoutError",
        ConnectionRefused => "ConnectionRefusedError",
        ConnectionReset => "ConnectionResetError",
        ConnectionAborted => "ConnectionAbortedError",
        BrokenPipe => "BrokenPipeError",
        IsADirectory => "IsADirectoryError",
        NotADirectory => "NotADirectoryError",
        _ => "OSError",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorted_atomic_and_merging() {
        let d = std::env::temp_dir().join(format!("pc-state-{}", crate::config::token_hex(6)));
        fs::create_dir_all(&d).unwrap();
        let s = StateFile::new(&d.join(STATE_FILE));
        assert!(s.load().is_empty());
        let mut m = Map::new();
        m.insert("known_ips".into(), json!({"127.0.0.1": 5}));
        s.update(m);
        let mut m = Map::new();
        m.insert("disk_low".into(), json!({"D:": false}));
        m.insert("movie_ready".into(), json!({"primed": true, "done": [], "pending": {"2": 1000.0}}));
        s.update(m);
        let text = fs::read_to_string(s.path()).unwrap();
        assert_eq!(
            text,
            "{\n  \"disk_low\": {\n    \"D:\": false\n  },\n  \"known_ips\": {\n    \"127.0.0.1\": 5\n  },\n  \"movie_ready\": {\n    \"done\": [],\n    \"pending\": {\n      \"2\": 1000.0\n    },\n    \"primed\": true\n  }\n}\n"
        );
        fs::write(s.path(), "[1, 2]").unwrap();
        assert!(s.load().is_empty());
        fs::write(s.path(), "{broken").unwrap();
        assert!(s.load().is_empty());
        assert_eq!(s.try_load(), Err("JSONDecodeError"));
        // update() never overwrites a file it cannot parse.
        let mut m = Map::new();
        m.insert("k".into(), json!(1));
        s.update(m);
        assert_eq!(fs::read_to_string(s.path()).unwrap(), "{broken");
        fs::remove_file(s.path()).unwrap();
        assert_eq!(s.try_load(), Ok(Map::new()));
        let _ = fs::remove_dir_all(&d);
    }
}
