//! API keys and tokens (spec 2.4, services.py:104-163).
//!
//! - Plex: `services.plex.token`, else `HKCU\<token_registry_key>` value
//!   `<token_registry_value>`.
//! - Radarr / Sonarr: `api_key`, else `<ApiKey>...</ApiKey>` from `config_xml`.
//! - Bazarr: `api_key`, else the `apikey:` line of the top-level `auth:` block of
//!   `config_yaml`.
//!
//! Cached per service for 300 s; a failed read caches `""` too and logs
//! WARNING `cannot read <name> key: <ExcType>`. Values are never logged.

use crate::config::Config;
use crate::log_warning;
use serde_json::Value;
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const KEY_TTL: Duration = Duration::from_secs(300);

pub struct Keys {
    config: Arc<Config>,
    cache: Mutex<HashMap<String, (Instant, String)>>,
}

#[derive(Debug)]
pub struct KeyError(pub &'static str);

impl From<io::Error> for KeyError {
    fn from(e: io::Error) -> Self {
        KeyError(crate::state::io_error_name(&e))
    }
}

impl Keys {
    pub fn new(config: Arc<Config>) -> Keys {
        Keys { config, cache: Mutex::new(HashMap::new()) }
    }

    /// The key or token for `name` (`plex`, `radarr`, `sonarr`, `bazarr`), `""` if none.
    pub fn get(&self, name: &str) -> String {
        {
            let g = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, v)) = g.get(name) {
                if at.elapsed() < KEY_TTL {
                    return v.clone();
                }
            }
        }
        let v = self.load(name);
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).insert(name.to_string(), (Instant::now(), v.clone()));
        v
    }

    fn load(&self, name: &str) -> String {
        let svc = self.config.service(name);
        let s = |k: &str| svc.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let r: Result<String, KeyError> = match name {
            "plex" if !s("token").is_empty() => Ok(s("token")),
            "plex" => read_plex_token(&s("token_registry_key"), &s("token_registry_value")).map_err(KeyError::from),
            "radarr" | "sonarr" if !s("api_key").is_empty() => Ok(s("api_key")),
            "radarr" | "sonarr" => read_xml_api_key(&s("config_xml")),
            "bazarr" if !s("api_key").is_empty() => Ok(s("api_key")),
            "bazarr" => read_bazarr_key(&s("config_yaml")),
            _ => Ok(String::new()),
        };
        match r {
            Ok(v) => v,
            Err(KeyError(t)) => {
                log_warning!("cannot read {name} key: {t}");
                String::new()
            }
        }
    }
}

fn read_text(path: &str) -> Result<String, KeyError> {
    let bytes = std::fs::read(path)?;
    let b = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
    String::from_utf8(b.to_vec()).map_err(|_| KeyError("UnicodeDecodeError"))
}

/// `re.search(r"<ApiKey>\s*([^<\s]+)\s*</ApiKey>", text)`.
pub fn parse_xml_api_key(text: &str) -> String {
    let mut from = 0;
    while let Some(i) = text[from..].find("<ApiKey>") {
        let start = from + i + "<ApiKey>".len();
        let rest = text[start..].trim_start();
        let end = rest.find(|c: char| c == '<' || c.is_whitespace()).unwrap_or(rest.len());
        let (key, tail) = rest.split_at(end);
        if !key.is_empty() && tail.trim_start().starts_with("</ApiKey>") {
            return key.to_string();
        }
        from = start;
    }
    String::new()
}

/// Radarr/Sonarr `config.xml`.
pub fn read_xml_api_key(path: &str) -> Result<String, KeyError> {
    Ok(parse_xml_api_key(&read_text(path)?))
}

/// The `apikey:` line directly under the top-level `auth:` block (no yaml parser).
pub fn parse_bazarr_apikey(text: &str) -> String {
    let mut in_auth = false;
    for raw in text.split('\n') {
        let line = raw.trim_end_matches(['\r', '\n']);
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if !line.chars().next().is_some_and(char::is_whitespace) {
            in_auth = line.split('#').next().unwrap_or("").trim() == "auth:";
            continue;
        }
        if in_auth {
            let s = line.trim();
            if let Some(v) = s.strip_prefix("apikey:") {
                return v.trim().trim_matches(['\'', '"']).to_string();
            }
        }
    }
    String::new()
}

pub fn read_bazarr_key(path: &str) -> Result<String, KeyError> {
    Ok(parse_bazarr_apikey(&read_text(path)?))
}

/// `HKEY_CURRENT_USER\<key>` value `<value>`, stringified.
#[cfg(windows)]
pub fn read_plex_token(key: &str, value: &str) -> io::Result<String> {
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, REG_DWORD, REG_EXPAND_SZ, REG_QWORD, REG_SZ, RRF_RT_ANY,
    };
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (k, v) = (wide(key), wide(value));
    let mut ty: u32 = 0;
    let mut size: u32 = 0;
    // SAFETY: valid NUL-terminated strings; first call only asks for the size.
    let rc = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr(), RRF_RT_ANY, &mut ty, std::ptr::null_mut(), &mut size)
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    let mut buf = vec![0u8; size as usize + 2];
    let mut size2 = buf.len() as u32;
    // SAFETY: buf holds size2 bytes.
    let rc = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr(), RRF_RT_ANY, &mut ty, buf.as_mut_ptr().cast(), &mut size2)
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    buf.truncate(size2 as usize);
    Ok(match ty {
        REG_SZ | REG_EXPAND_SZ => {
            let units: Vec<u16> = buf.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
            let end = units.iter().position(|u| *u == 0).unwrap_or(units.len());
            String::from_utf16_lossy(&units[..end])
        }
        REG_DWORD if buf.len() >= 4 => u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]).to_string(),
        REG_QWORD if buf.len() >= 8 => u64::from_le_bytes(buf[..8].try_into().unwrap()).to_string(),
        _ => String::from_utf8_lossy(&buf).into_owned(),
    })
}

#[cfg(not(windows))]
pub fn read_plex_token(_key: &str, _value: &str) -> io::Result<String> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "no registry"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // py: test_logic.TestBazarrKey.test_parse
    #[test]
    fn bazarr_key() {
        let yaml = "general:\n  apikey: wrong\nauth:\n  password: x\n  apikey: 'abc123'\n  type: form\nother:\n  apikey: nope\n";
        assert_eq!(parse_bazarr_apikey(yaml), "abc123");
        assert_eq!(parse_bazarr_apikey("auth:\n  type: x\n"), "");
        assert_eq!(parse_bazarr_apikey("auth:\n  # apikey: commented\n  apikey: ok\n"), "ok");
        assert_eq!(parse_bazarr_apikey("auth:  # c\r\n  apikey: \"q\"\r\n"), "q");
        assert_eq!(parse_bazarr_apikey("general:\n  apikey: x\n"), "");
    }

    #[test]
    fn xml_key() {
        assert_eq!(parse_xml_api_key("<Config>\n  <ApiKey> abc </ApiKey>\n</Config>"), "abc");
        assert_eq!(parse_xml_api_key("<ApiKey>a b</ApiKey><ApiKey>c</ApiKey>"), "c");
        assert_eq!(parse_xml_api_key("<Config></Config>"), "");
    }

    #[test]
    fn missing_files_cache_the_empty_key() {
        let d = std::env::temp_dir().join(format!("pc-keys-{}", crate::config::token_hex(6)));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("cfg.json");
        std::fs::write(&p, r#"{"services": {"radarr": {"config_xml": "Z:\\nope\\config.xml"}}}"#).unwrap();
        let k = Keys::new(Arc::new(Config::load(&p).unwrap()));
        assert_eq!(k.get("radarr"), "");
        assert_eq!(k.get("radarr"), "");
        let _ = std::fs::remove_dir_all(&d);
    }
}
