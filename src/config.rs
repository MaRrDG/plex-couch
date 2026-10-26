//! `config.local.json` (spec section 2): loaded once, deep-merged over [`defaults`], written
//! back only from the raw user dict (never the defaults), atomically via `<path>.tmp`.
//! Never log its contents.

use crate::json::{dump_pretty, py_int, py_str, truthy};
use serde_json::{json, Map, Value};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

pub const DEFAULT_APP_NAME: &str = "Plex Couch";
pub const DEFAULT_FILELIST_REGEX: &str = r"thefl\.org|filelist\.(io|ro)|flro\.org";
/// Keys of `settings` the app writes itself (`SETTINGS_KEYS`); all are bools.
pub const SETTINGS_KEYS: &[&str] = &["auto_monitors_off"];

/// The defaults (config.py:10-68). Key order matters only for display.
pub fn defaults() -> Value {
    json!({
        "app_name": DEFAULT_APP_NAME,
        "pin_hash": "",
        "secret": "",
        "settings": {"auto_monitors_off": true},
        "filelist_tracker_regex": DEFAULT_FILELIST_REGEX,
        "disks": ["C:", "D:"],
        "plex_poll_seconds": 10,
        "allowed_hosts": [],
        "session_epoch": 1,
        "notifications": {
            "server": "https://ntfy.sh",
            "topic": null,
            "disk_min_gb": 60,
            "ready_wait_minutes": 20
        },
        "https": {
            "port": 8766,
            "cert": "certs/server.pem",
            "key": "certs/server.key",
            "ca_cert": "certs/ca.crt"
        },
        "services": {
            "plex": {
                "url": "http://127.0.0.1:32400",
                "token": "",
                "token_registry_key": "Software\\Plex, Inc.\\Plex Media Server",
                "token_registry_value": "PlexOnlineToken"
            },
            "radarr": {
                "url": "http://127.0.0.1:7878",
                "api_key": "",
                "config_xml": "C:\\ProgramData\\Radarr\\config.xml"
            },
            "bazarr": {
                "url": "http://127.0.0.1:6767",
                "api_key": "",
                "config_yaml": "C:\\ProgramData\\Bazarr\\config\\config.yaml"
            },
            // New in the Rust app (watchlist sync jobs); not in the Python defaults.
            "sonarr": {
                "url": "http://127.0.0.1:8989",
                "api_key": "",
                "config_xml": "C:\\ProgramData\\Sonarr\\config.xml"
            },
            "prowlarr": {"url": "http://127.0.0.1:9696"},
            "qbittorrent": {"url": "http://127.0.0.1:8080", "username": "", "password": ""}
        }
    })
}

/// `_merge`: dicts merge recursively; any other value (lists included) replaces the default.
pub fn merge(base: &Value, over: &Value) -> Value {
    let mut out = base.clone();
    if let (Value::Object(o), Value::Object(ov)) = (&mut out, over) {
        for (k, v) in ov {
            match (o.get(k), v) {
                (Some(b @ Value::Object(_)), Value::Object(_)) => {
                    let m = merge(b, v);
                    o.insert(k.clone(), m);
                }
                _ => {
                    o.insert(k.clone(), v.clone());
                }
            }
        }
    }
    out
}

#[derive(Debug)]
pub enum ConfigError {
    Io(io::Error),
    Json(serde_json::Error),
    NotAnObject,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "cannot read the config file: {e}"),
            ConfigError::Json(e) => write!(f, "the config file is not valid JSON: {e}"),
            ConfigError::NotAnObject => write!(f, "the config file must hold a JSON object"),
        }
    }
}

impl std::error::Error for ConfigError {}

struct Inner {
    raw: Map<String, Value>,
    data: Value,
}

pub struct Config {
    path: PathBuf,
    inner: RwLock<Inner>,
}

/// A secret: 32 random bytes as 64 lowercase hex chars (`secrets.token_hex(32)`).
pub fn token_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::getrandom(&mut b).expect("OS random source");
    hex(&b)
}

pub fn hex(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 15) as usize] as char);
    }
    s
}

impl Config {
    /// Load `path` (UTF-8, optional BOM). A missing file is an empty config.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let path = absolute(path);
        let raw = if path.exists() {
            let bytes = fs::read(&path).map_err(ConfigError::Io)?;
            let text = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
            match serde_json::from_slice::<Value>(text).map_err(ConfigError::Json)? {
                Value::Object(m) => m,
                _ => return Err(ConfigError::NotAnObject),
            }
        } else {
            Map::new()
        };
        let data = merge(&defaults(), &Value::Object(raw.clone()));
        Ok(Config { path, inner: RwLock::new(Inner { raw, data }) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The folder holding the config (and `state.local.json`).
    pub fn dir(&self) -> PathBuf {
        self.path.parent().map(Path::to_path_buf).unwrap_or_default()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }

    /// A snapshot of the merged config (defaults + file).
    pub fn data(&self) -> Value {
        self.read().data.clone()
    }

    /// One top-level value of the merged config.
    pub fn get(&self, key: &str) -> Option<Value> {
        self.read().data.get(key).cloned()
    }

    /// `config.data[key] = value` in memory only (demo settings, tests).
    pub fn set_in_memory(&self, key: &str, value: Value) {
        if let Value::Object(m) = &mut self.write().data {
            m.insert(key.to_string(), value);
        }
    }

    fn save_locked(&self, inner: &mut Inner) -> io::Result<()> {
        if let Some(d) = self.path.parent() {
            if !d.as_os_str().is_empty() {
                fs::create_dir_all(d)?;
            }
        }
        let mut bytes = dump_pretty(&Value::Object(inner.raw.clone()));
        bytes.push(b'\n');
        let tmp = tmp_path(&self.path);
        fs::write(&tmp, &bytes)?;
        fs::rename(&tmp, &self.path)?;
        inner.data = merge(&defaults(), &Value::Object(inner.raw.clone()));
        Ok(())
    }

    /// Generate the cookie-signing secret on first run (writes the file).
    pub fn ensure_secret(&self) -> io::Result<String> {
        let mut g = self.write();
        if !truthy(g.raw.get("secret")) {
            g.raw.insert("secret".into(), Value::String(token_hex(32)));
            self.save_locked(&mut g)?;
        }
        let s = g.raw.get("secret").cloned().unwrap_or(Value::Null);
        if let Value::Object(m) = &mut g.data {
            m.insert("secret".into(), s.clone());
        }
        Ok(py_str(&s))
    }

    /// `--set-pin`: store the hash, add a secret if missing, default `auto_monitors_off`.
    pub fn set_pin_hash(&self, pin_hash: &str) -> io::Result<()> {
        let mut g = self.write();
        g.raw.insert("pin_hash".into(), Value::String(pin_hash.into()));
        if !truthy(g.raw.get("secret")) {
            g.raw.insert("secret".into(), Value::String(token_hex(32)));
        }
        // setdefault("settings", {}).setdefault("auto_monitors_off", True)
        let settings = g.raw.entry("settings").or_insert_with(|| json!({}));
        if let Value::Object(s) = settings {
            s.entry("auto_monitors_off").or_insert(Value::Bool(true));
        }
        self.save_locked(&mut g)
    }

    /// `--setup-ntfy`: set `notifications.topic` (replacing a non-dict `notifications`).
    pub fn set_notification_topic(&self, topic: &str) -> io::Result<()> {
        let mut g = self.write();
        if !matches!(g.raw.get("notifications"), Some(Value::Object(_))) {
            g.raw.insert("notifications".into(), json!({}));
        }
        if let Some(Value::Object(n)) = g.raw.get_mut("notifications") {
            n.insert("topic".into(), Value::String(topic.into()));
        }
        self.save_locked(&mut g)
    }

    /// `max(1, int(session_epoch or 1))`, 1 when it is not a number.
    pub fn session_epoch(&self) -> i64 {
        epoch_of(self.read().data.get("session_epoch"))
    }

    /// Logout everywhere: bump and persist the epoch. Returns the new epoch.
    pub fn bump_session_epoch(&self) -> io::Result<i64> {
        let mut g = self.write();
        let next = epoch_of(g.raw.get("session_epoch")) + 1;
        g.raw.insert("session_epoch".into(), json!(next));
        self.save_locked(&mut g)?;
        Ok(next)
    }

    /// Extra Host names (`allowed_hosts`), strings only.
    pub fn allowed_hosts(&self) -> Vec<String> {
        match self.read().data.get("allowed_hosts") {
            Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
            _ => Vec::new(),
        }
    }

    /// POST /api/settings: `settings[k] = bool(v)` for known keys, persisted.
    pub fn update_settings(&self, changes: &[(&str, bool)]) -> io::Result<()> {
        let mut g = self.write();
        if !matches!(g.raw.get("settings"), Some(Value::Object(_))) {
            // setdefault("settings", {}); Python would fail on a non-dict, we replace it.
            g.raw.insert("settings".into(), json!({}));
        }
        if let Some(Value::Object(s)) = g.raw.get_mut("settings") {
            for (k, v) in changes {
                if SETTINGS_KEYS.contains(k) {
                    s.insert((*k).to_string(), Value::Bool(*v));
                }
            }
        }
        self.save_locked(&mut g)
    }

    /// Demo mode: `config.data["settings"].update(changes)` in memory only.
    pub fn update_settings_in_memory(&self, changes: &[(&str, bool)]) {
        let mut g = self.write();
        if let Value::Object(d) = &mut g.data {
            let s = d.entry("settings").or_insert_with(|| json!({}));
            if let Value::Object(s) = s {
                for (k, v) in changes {
                    s.insert((*k).to_string(), Value::Bool(*v));
                }
            }
        }
    }

    /// `dict(data["settings"])`.
    pub fn settings(&self) -> Value {
        match self.read().data.get("settings") {
            Some(Value::Object(m)) => Value::Object(m.clone()),
            _ => json!({}),
        }
    }

    /// One bool setting (`bool(settings.get(k))`).
    pub fn setting(&self, key: &str) -> bool {
        truthy(self.read().data.get("settings").and_then(|s| s.get(key)))
    }

    /// `str(app_name or "Plex Couch")`.
    pub fn app_name(&self) -> String {
        let g = self.read();
        let v = g.data.get("app_name");
        if truthy(v) {
            py_str(v.unwrap())
        } else {
            DEFAULT_APP_NAME.to_string()
        }
    }

    pub fn pin_hash(&self) -> String {
        match self.read().data.get("pin_hash") {
            Some(Value::String(s)) => s.clone(),
            Some(v) if truthy(Some(v)) => py_str(v),
            _ => String::new(),
        }
    }

    pub fn secret(&self) -> String {
        match self.read().data.get("secret") {
            Some(v) if truthy(Some(v)) => py_str(v),
            _ => String::new(),
        }
    }

    /// `services.<name>` (an empty object if missing).
    pub fn service(&self, name: &str) -> Value {
        self.read()
            .data
            .get("services")
            .and_then(|s| s.get(name))
            .cloned()
            .unwrap_or_else(|| json!({}))
    }

    /// Resolve a path from the config: `~` and `%VAR%`/`$VAR` expanded, relative paths
    /// against the config file's folder, normalized. Empty for a falsy value.
    pub fn resolve(&self, value: Option<&Value>) -> String {
        if !truthy(value) {
            return String::new();
        }
        let v = expand_vars(&expand_user(&py_str(value.unwrap())));
        if is_abs(&v) {
            return v;
        }
        let base = self.dir();
        normpath(&join(&base.to_string_lossy(), &v))
    }
}

fn epoch_of(v: Option<&Value>) -> i64 {
    if !truthy(v) {
        return 1;
    }
    py_int(v.unwrap()).map(|n| n.max(1)).unwrap_or(1)
}

pub fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

/// `os.path.abspath`.
pub fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// `os.path.expanduser` (Windows: `~` or `~\...` with USERPROFILE).
pub fn expand_user(s: &str) -> String {
    if !s.starts_with('~') {
        return s.to_string();
    }
    let rest_at = s.find(['/', '\\']).unwrap_or(s.len());
    if rest_at != 1 {
        return s.to_string(); // ~user forms are left alone
    }
    match std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
        Ok(home) => format!("{}{}", home, &s[1..]),
        Err(_) => s.to_string(),
    }
}

/// `ntpath.expandvars`: `$VAR`, `${VAR}` and `%VAR%`; unknown variables stay as written,
/// `'...'` is copied literally, `%%` and `$$` give one char.
pub fn expand_vars(s: &str) -> String {
    if !s.contains('$') && !s.contains('%') {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let is_var = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            match chars[i + 1..].iter().position(|&x| x == '\'') {
                Some(j) => {
                    out.extend(&chars[i..i + j + 2]);
                    i += j + 2;
                }
                None => {
                    out.extend(&chars[i..]);
                    i = chars.len();
                }
            }
        } else if c == '%' {
            if chars.get(i + 1) == Some(&'%') {
                out.push('%');
                i += 2;
            } else {
                match chars[i + 1..].iter().position(|&x| x == '%') {
                    Some(j) => {
                        let name: String = chars[i + 1..i + 1 + j].iter().collect();
                        match std::env::var(&name) {
                            Ok(v) => out.push_str(&v),
                            Err(_) => out.push_str(&format!("%{name}%")),
                        }
                        i += j + 2;
                    }
                    None => {
                        out.extend(&chars[i..]);
                        i = chars.len();
                    }
                }
            }
        } else if c == '$' {
            if chars.get(i + 1) == Some(&'$') {
                out.push('$');
                i += 2;
            } else if chars.get(i + 1) == Some(&'{') {
                match chars[i + 2..].iter().position(|&x| x == '}') {
                    Some(j) => {
                        let name: String = chars[i + 2..i + 2 + j].iter().collect();
                        match std::env::var(&name) {
                            Ok(v) => out.push_str(&v),
                            Err(_) => out.push_str(&format!("${{{name}}}")),
                        }
                        i += j + 3;
                    }
                    None => {
                        out.extend(&chars[i..]);
                        i = chars.len();
                    }
                }
            } else {
                let mut j = i + 1;
                while j < chars.len() && is_var(chars[j]) {
                    j += 1;
                }
                let name: String = chars[i + 1..j].iter().collect();
                if name.is_empty() {
                    out.push('$');
                } else {
                    match std::env::var(&name) {
                        Ok(v) => out.push_str(&v),
                        Err(_) => out.push_str(&format!("${name}")),
                    }
                }
                i = j;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

fn is_sep(c: char) -> bool {
    c == '\\' || c == '/'
}

/// (drive, rest): `C:` or `\\server\share`.
fn split_drive(p: &str) -> (&str, &str) {
    let b = p.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        return (&p[..2], &p[2..]);
    }
    if b.len() >= 2 && is_sep(b[0] as char) && is_sep(b[1] as char) {
        // UNC: \\server\share
        let rest = &p[2..];
        if let Some(i) = rest.find(is_sep) {
            let after = &rest[i + 1..];
            let j = after.find(is_sep).map(|j| j + i + 1).unwrap_or(rest.len());
            return (&p[..j + 2], &p[j + 2..]);
        }
        return (p, "");
    }
    ("", p)
}

/// `ntpath.isabs` (Python 3.13+): a drive or UNC share followed by a separator.
pub fn is_abs(p: &str) -> bool {
    let (drive, rest) = split_drive(p);
    if drive.starts_with("\\\\") || drive.starts_with("//") {
        return true;
    }
    !drive.is_empty() && rest.starts_with(is_sep)
}

/// `ntpath.join(a, b)` for the cases the config needs.
pub fn join(a: &str, b: &str) -> String {
    let (bd, brest) = split_drive(b);
    if !bd.is_empty() {
        return b.to_string();
    }
    if brest.starts_with(is_sep) {
        let (ad, _) = split_drive(a);
        return format!("{ad}{b}");
    }
    if a.is_empty() || a.ends_with(is_sep) {
        format!("{a}{b}")
    } else {
        format!("{a}\\{b}")
    }
}

/// `ntpath.normpath`.
pub fn normpath(p: &str) -> String {
    let p = p.replace('/', "\\");
    let (drive, rest) = split_drive(&p);
    let rooted = rest.starts_with('\\');
    let mut parts: Vec<&str> = Vec::new();
    for comp in rest.split('\\') {
        match comp {
            "" | "." => {}
            ".." => {
                if matches!(parts.last(), Some(&x) if x != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                }
            }
            c => parts.push(c),
        }
    }
    let mut out = String::from(drive);
    if rooted {
        out.push('\\');
    }
    out.push_str(&parts.join("\\"));
    if out.is_empty() {
        ".".into()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_dicts_recursively_lists_replace() {
        let m = merge(&defaults(), &json!({"disks": ["E:"], "notifications": {"topic": "t"}}));
        assert_eq!(m["disks"], json!(["E:"]));
        assert_eq!(m["notifications"]["topic"], json!("t"));
        assert_eq!(m["notifications"]["disk_min_gb"], json!(60));
    }

    #[test]
    fn paths() {
        assert_eq!(normpath("D:\\a\\b\\..\\c/./d"), "D:\\a\\c\\d");
        assert_eq!(join("D:\\cfg", "certs/server.pem"), "D:\\cfg\\certs/server.pem");
        assert_eq!(join("D:\\cfg", "\\x"), "D:\\x");
        assert!(is_abs("C:\\x"));
        assert!(is_abs("\\\\srv\\share\\x"));
        assert!(!is_abs("\\x"));
        assert!(!is_abs("certs\\a"));
        std::env::set_var("PC_TEST_VAR", "V");
        assert_eq!(expand_vars("%PC_TEST_VAR%\\a\\$PC_TEST_VAR\\${PC_TEST_VAR}\\%NOPE_X%"), "V\\a\\V\\V\\%NOPE_X%");
    }

    #[test]
    fn epoch_coercion() {
        assert_eq!(epoch_of(None), 1);
        assert_eq!(epoch_of(Some(&json!(0))), 1);
        assert_eq!(epoch_of(Some(&json!("abc"))), 1);
        assert_eq!(epoch_of(Some(&json!(3))), 3);
        assert_eq!(epoch_of(Some(&json!(-4))), 1);
        assert_eq!(epoch_of(Some(&json!("7"))), 7);
    }

    #[test]
    fn writes_only_the_raw_dict() {
        let d = tempdir();
        let p = d.join("cfg.json");
        fs::write(&p, "\u{feff}{\"pin_hash\": \"x\", \"z\": 1.0}").unwrap();
        let c = Config::load(&p).unwrap();
        c.ensure_secret().unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("{\n  \"pin_hash\": \"x\",\n  \"z\": 1.0,\n  \"secret\": \""), "{text}");
        assert!(text.ends_with("\"\n}\n"));
        assert!(!text.contains("services"));
        assert_eq!(c.secret().len(), 64);
        assert_eq!(c.bump_session_epoch().unwrap(), 2);
        assert_eq!(c.session_epoch(), 2);
        let _ = fs::remove_dir_all(&d);
    }

    pub(crate) fn tempdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("pc-cfg-{}", token_hex(8)));
        fs::create_dir_all(&d).unwrap();
        d
    }
}
