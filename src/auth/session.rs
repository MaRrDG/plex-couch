//! Session tokens and the session cookie (spec 4.2).
//!
//! Key: `HMAC-SHA256(key = secret, msg = pin_hash)`, so a new PIN or secret invalidates every
//! token. Token: `"<epoch>.<expiry>.<hex sig>"`, `sig = HMAC-SHA256(key, "<epoch>.<expiry>")`.

use crate::auth::pin::ct_eq;
use crate::config::hex;
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub const COOKIE: &str = "pc_session";
pub const SESSION_DAYS: i64 = 30;
/// Re-issue the cookie on an authenticated `/api/*` request when less than this is left.
pub const SESSION_RENEW_DAYS: i64 = 15;

type HmacSha256 = Hmac<Sha256>;

fn hmac(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC takes any key length");
    m.update(msg);
    m.finalize().into_bytes().into()
}

/// The signing key bound to the PIN hash.
pub fn session_key(secret: &str, pin_hash: &str) -> [u8; 32] {
    hmac(secret.as_bytes(), pin_hash.as_bytes())
}

/// Lowercase hex HMAC of a payload (`_sig`).
pub fn sig(key: &[u8], payload: &str) -> String {
    hex(&hmac(key, payload.as_bytes()))
}

/// A token valid for `days` from `now`.
pub fn sign_session(key: &[u8], now: f64, days: i64, epoch: i64) -> String {
    let expiry = (now + (days * 86400) as f64) as i64; // int(): truncation
    let payload = format!("{epoch}.{expiry}");
    let s = sig(key, &payload);
    format!("{payload}.{s}")
}

fn ascii_digits(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_digit())
}

/// The expiry of a valid, unexpired token of the current epoch, else None.
pub fn session_expiry(key: &[u8], token: Option<&str>, now: f64, epoch: i64) -> Option<i64> {
    let token = token?;
    if token.is_empty() || token.matches('.').count() != 2 {
        return None;
    }
    let mut it = token.split('.');
    let (ep_s, exp_s, sg) = (it.next()?, it.next()?, it.next()?);
    if !ascii_digits(ep_s, 9) || !ascii_digits(exp_s, 12) {
        return None;
    }
    let expected = sig(key, &format!("{ep_s}.{exp_s}"));
    if !ct_eq(sg.as_bytes(), expected.as_bytes()) {
        return None;
    }
    let ep: i64 = ep_s.parse().ok()?;
    if ep != epoch {
        return None;
    }
    let exp: i64 = exp_s.parse().ok()?;
    if (exp as f64) > now {
        Some(exp)
    } else {
        None
    }
}

pub fn verify_session(key: &[u8], token: Option<&str>, now: f64, epoch: i64) -> bool {
    session_expiry(key, token, now, epoch).is_some()
}

pub fn session_needs_renewal(expiry: i64, now: f64) -> bool {
    (expiry as f64) - now < (SESSION_RENEW_DAYS * 86400) as f64
}

/// `Set-Cookie` value; `Secure` only on the HTTPS listener.
pub fn session_cookie(name: &str, value: &str, max_age: i64, secure: bool) -> String {
    format!(
        "{name}={value}; Max-Age={max_age}; HttpOnly; SameSite=Strict; Path=/{}",
        if secure { "; Secure" } else { "" }
    )
}

/// The `pc_session` value from a Cookie header (`http.cookies.SimpleCookie` semantics for
/// the cases that matter: `;`-separated pairs, the last occurrence wins, a quoted value is
/// unquoted).
pub fn cookie_token(header: Option<&str>) -> Option<String> {
    let raw = header?;
    if raw.is_empty() {
        return None;
    }
    let mut found = None;
    for part in raw.split(';') {
        let part = part.trim();
        let Some((name, value)) = part.split_once('=') else { continue };
        if name.trim() == COOKIE {
            let v = value.trim();
            let v = if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') { &v[1..v.len() - 1] } else { v };
            found = Some(v.to_string());
        }
    }
    found
}

pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: f64 = 86400.0;

    const KEY: &[u8] = &[b'k'; 32];

    #[test]
    fn key_matches_python() {
        let k = session_key("secret", "pbkdf2_sha256$1000$aa$bb");
        assert_eq!(hex(&k), "10f2246d55afe082bc7f315144efb50bbfd3786dc99e35df7936b06c0a8bd454");
    }

    // py: test_logic.TestSessionToken.test_roundtrip
    #[test]
    fn roundtrip() {
        let t = sign_session(KEY, 1000.0, SESSION_DAYS, 1);
        assert!(verify_session(KEY, Some(&t), 1001.0, 1));
    }

    // py: test_logic.TestSessionToken.test_expiry
    #[test]
    fn expiry() {
        assert_eq!(SESSION_DAYS, 30);
        let t = sign_session(KEY, 1000.0, SESSION_DAYS, 1);
        assert!(verify_session(KEY, Some(&t), 1000.0 + 29.0 * DAY, 1));
        assert_eq!(session_expiry(KEY, Some(&t), 1001.0, 1), Some(1000 + 30 * 86400));
        assert!(!verify_session(KEY, Some(&t), 1000.0 + 30.0 * DAY, 1));
        assert!(!verify_session(KEY, Some(&t), 1000.0 + 180.0 * DAY, 1));
    }

    // py: test_logic.TestSessionToken.test_tamper
    #[test]
    fn tamper() {
        let t = sign_session(KEY, 1000.0, SESSION_DAYS, 1);
        let p: Vec<&str> = t.split('.').collect();
        let (ep, exp, sg) = (p[0], p[1], p[2]);
        assert_eq!(ep, "1");
        let bumped = format!("{}.{}.{}", ep, exp.parse::<i64>().unwrap() + 999_999, sg);
        assert!(!verify_session(KEY, Some(&bumped), 1001.0, 1));
        assert!(!verify_session(KEY, Some(&format!("2.{exp}.{sg}")), 1001.0, 2));
        let zero = format!("{ep}.{exp}.{}", "0".repeat(sg.len()));
        assert!(!verify_session(KEY, Some(&zero), 1001.0, 1));
        assert!(!verify_session(b"other", Some(&t), 1001.0, 1));
        let two = format!("{exp}.{sg}");
        let a = format!("a.{exp}.{sg}");
        let e = format!("1.{exp}.é");
        let bad: Vec<Option<&str>> = vec![
            Some(""), None, Some("abc"), Some("1.2.3"), Some(".x"), Some("x."), Some("12a.ff"),
            Some("1.é"), Some("1.2"), Some(&two), Some("1.1.2.3"), Some(&a), Some(&e),
        ];
        for b in bad {
            assert!(!verify_session(KEY, b, 1001.0, 1), "{b:?}");
        }
    }

    // py: test_logic.TestSessionToken.test_key_bound_to_pin
    #[test]
    fn key_bound_to_pin() {
        let k1 = session_key("secret", "hash1");
        let k2 = session_key("secret", "hash2");
        let t = sign_session(&k1, 1000.0, SESSION_DAYS, 1);
        assert!(verify_session(&k1, Some(&t), 1001.0, 1));
        assert!(!verify_session(&k2, Some(&t), 1001.0, 1));
    }

    #[test]
    fn renewal_window() {
        assert!(!session_needs_renewal(1000 + 16 * 86400, 1000.0));
        assert!(session_needs_renewal(1000 + 14 * 86400, 1000.0));
    }

    #[test]
    fn cookie_parse() {
        assert_eq!(cookie_token(Some("pc_session=1.2.3")).as_deref(), Some("1.2.3"));
        assert_eq!(cookie_token(Some("a=b; pc_session=x; c=d")).as_deref(), Some("x"));
        assert_eq!(cookie_token(Some("pc_session=x; pc_session=y")).as_deref(), Some("y"));
        assert_eq!(cookie_token(Some("pc_session=\"q\"")).as_deref(), Some("q"));
        assert_eq!(cookie_token(Some("other=1")), None);
        assert_eq!(cookie_token(None), None);
    }
}
