//! Request-level security rules (spec 3.1, 3.2, 4.6, 4.7): the response security headers,
//! the LAN-only client IP rule with Python's exact `ipaddress.is_private` semantics, the
//! Host allowlist (DNS rebinding), the HTTPS redirect URL and the install page CSP.

use base64::Engine;
use sha2::{Digest, Sha256};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub const CSP: &str = "default-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; \
connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";
pub const HSTS: &str = "max-age=31536000";

/// Sent on every response (`SECURITY_HEADERS`), in this order.
pub const SECURITY_HEADERS: [(&str, &str); 4] = [
    ("X-Content-Type-Options", "nosniff"),
    ("X-Frame-Options", "DENY"),
    ("Referrer-Policy", "no-referrer"),
    ("Content-Security-Policy", CSP),
];

// ---------------------------------------------------------------------------
// Client IP rule
// ---------------------------------------------------------------------------

fn v4_in(a: Ipv4Addr, net: [u8; 4], prefix: u32) -> bool {
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    (u32::from(a) & mask) == (u32::from(Ipv4Addr::from(net)) & mask)
}

fn v6_in(a: Ipv6Addr, net: &str, prefix: u32) -> bool {
    let n: Ipv6Addr = net.parse().expect("static network");
    let mask = if prefix == 0 { 0 } else { u128::MAX << (128 - prefix) };
    (u128::from(a) & mask) == (u128::from(n) & mask)
}

/// Python 3.12+ `IPv4Address.is_private`.
pub fn v4_is_private(a: Ipv4Addr) -> bool {
    const PRIVATE: [([u8; 4], u32); 14] = [
        ([0, 0, 0, 0], 8),
        ([10, 0, 0, 0], 8),
        ([127, 0, 0, 0], 8),
        ([169, 254, 0, 0], 16),
        ([172, 16, 0, 0], 12),
        ([192, 0, 0, 0], 29),
        ([192, 0, 0, 170], 31),
        ([192, 0, 2, 0], 24),
        ([192, 168, 0, 0], 16),
        ([198, 18, 0, 0], 15),
        ([198, 51, 100, 0], 24),
        ([203, 0, 113, 0], 24),
        ([240, 0, 0, 0], 4),
        ([255, 255, 255, 255], 32),
    ];
    const EXCEPTIONS: [([u8; 4], u32); 2] = [([192, 0, 0, 9], 32), ([192, 0, 0, 10], 32)];
    PRIVATE.iter().any(|(n, p)| v4_in(a, *n, *p)) && !EXCEPTIONS.iter().any(|(n, p)| v4_in(a, *n, *p))
}

/// Python 3.12+ `IPv6Address.is_private` (for an address that is not IPv4-mapped).
pub fn v6_is_private(a: Ipv6Addr) -> bool {
    const PRIVATE: [(&str, u32); 11] = [
        ("::1", 128),
        ("::", 128),
        ("::ffff:0:0", 96),
        ("64:ff9b:1::", 48),
        ("100::", 64),
        ("2001::", 23),
        ("2001:db8::", 32),
        ("2002::", 16),
        ("3fff::", 20),
        ("fc00::", 7),
        ("fe80::", 10),
    ];
    const EXCEPTIONS: [(&str, u32); 6] = [
        ("2001:1::1", 128),
        ("2001:1::2", 128),
        ("2001:3::", 32),
        ("2001:4:112::", 48),
        ("2001:20::", 28),
        ("2001:30::", 28),
    ];
    PRIVATE.iter().any(|(n, p)| v6_in(a, n, *p)) && !EXCEPTIONS.iter().any(|(n, p)| v6_in(a, n, *p))
}

/// Python `ipaddress.ip_address(s)`: a strict dotted quad (no leading zeros) or IPv6.
pub fn parse_ip(s: &str) -> Option<IpAddr> {
    if s.contains(':') {
        s.parse::<Ipv6Addr>().ok().map(IpAddr::V6)
    } else {
        parse_v4_strict(s).map(IpAddr::V4)
    }
}

/// `ipaddress.IPv4Address(s)`: four decimal octets 0-255, no leading zeros.
pub fn parse_v4_strict(s: &str) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut o = [0u8; 4];
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if p.len() > 1 && p.starts_with('0') {
            return None;
        }
        o[i] = p.parse::<u16>().ok().filter(|v| *v <= 255)? as u8;
    }
    Some(Ipv4Addr::from(o))
}

/// True for private (LAN) or loopback addresses; false for anything else or garbage.
/// A `%zone` suffix is dropped and an IPv4-mapped IPv6 address is unwrapped first.
pub fn is_private_client(ip: Option<&str>) -> bool {
    let s = ip.unwrap_or("");
    let s = s.split('%').next().unwrap_or("");
    let Some(addr) = parse_ip(s) else { return false };
    match addr {
        IpAddr::V4(a) => v4_is_private(a) || a.is_loopback(),
        IpAddr::V6(a) => match a.to_ipv4_mapped() {
            Some(m) => v4_is_private(m) || m.is_loopback(),
            None => v6_is_private(a) || a.is_loopback(),
        },
    }
}

/// `ipaddress.ip_address(a).is_loopback` (no mapped unwrapping, like Python).
pub fn is_loopback_str(s: &str) -> bool {
    match parse_ip(s) {
        Some(IpAddr::V4(a)) => a.is_loopback(),
        Some(IpAddr::V6(a)) => a.is_loopback(),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Host header
// ---------------------------------------------------------------------------

/// `^[A-Za-z0-9](?:[A-Za-z0-9-]{0,62})(?:\.[A-Za-z0-9-]{1,63})*\.?$`
fn hostname_ok(h: &str) -> bool {
    let h = h.strip_suffix('.').unwrap_or(h);
    let labels: Vec<&str> = h.split('.').collect();
    let alnum_dash = |l: &str| l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let first = labels[0];
    if first.is_empty() || first.len() > 63 || !first.as_bytes()[0].is_ascii_alphanumeric() || !alnum_dash(first) {
        return false;
    }
    labels[1..].iter().all(|l| !l.is_empty() && l.len() <= 63 && alnum_dash(l))
}

fn port_ok(p: &str) -> bool {
    (1..=5).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit())
}

/// The host part of a Host header (`"192.168.1.50:8765"` -> `"192.168.1.50"`,
/// `"[fe80::1]:8765"` -> `"[fe80::1]"`), lowercased; None if missing or not a sane host.
pub fn split_host_header(host_header: Option<&str>) -> Option<String> {
    let h = host_header.unwrap_or("").trim();
    if h.is_empty() || h.chars().count() > 255 {
        return None;
    }
    if let Some(inner_rest) = h.strip_prefix('[') {
        let end = inner_rest.find(']')?;
        let (inner, rest) = (&inner_rest[..end], &inner_rest[end + 1..]);
        if !rest.is_empty() {
            let p = rest.strip_prefix(':')?;
            if !port_ok(p) {
                return None;
            }
        }
        let addr = inner.split('%').next().unwrap_or("");
        addr.parse::<Ipv6Addr>().ok()?;
        return Some(format!("[{inner}]"));
    }
    let host = if h.matches(':').count() == 1 {
        let (host, port) = h.rsplit_once(':').unwrap();
        if !port_ok(port) {
            return None;
        }
        host
    } else {
        h
    };
    if host.is_empty() || !hostname_ok(host) {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// Host allowlist: an IP literal, `localhost`, a `*.local` name or an `allowed_hosts` entry.
pub fn host_allowed(host_header: Option<&str>, extra_hosts: &[String]) -> bool {
    let Some(host) = split_host_header(host_header) else { return false };
    if host.starts_with('[') {
        return true; // only valid IPv6 literals get here
    }
    let host = host.trim_end_matches('.');
    if parse_v4_strict(host).is_some() {
        return true;
    }
    if host == "localhost" || (host.ends_with(".local") && host.len() > ".local".len()) {
        return true;
    }
    extra_hosts
        .iter()
        .map(|h| h.trim())
        .filter(|h| !h.is_empty())
        .any(|h| h.to_lowercase().trim_end_matches('.') == host)
}

/// The HTTPS URL for a plain-HTTP request: the host the browser used, the HTTPS port.
pub fn https_url(host_header: Option<&str>, https_port: u16, path: &str, fallback_host: &str) -> String {
    let host = match split_host_header(host_header) {
        Some(h) => h,
        None => {
            let fb = if fallback_host.is_empty() { "127.0.0.1" } else { fallback_host };
            if fb.contains(':') && !fb.starts_with('[') {
                format!("[{fb}]")
            } else {
                fb.to_string()
            }
        }
    };
    let mut path = path.to_string();
    if path.is_empty() || !path.starts_with('/') || path.chars().any(|c| (c as u32) < 33 || c as u32 == 127) {
        path = "/".into();
    }
    if path.starts_with("//") {
        path = format!("/{}", path.trim_start_matches('/'));
    }
    if https_port == 443 {
        format!("https://{host}{path}")
    } else {
        format!("https://{host}:{https_port}{path}")
    }
}

/// CSP for the install page: no scripts, only its own inline `<style>` (by hash).
pub fn install_csp(page: &[u8]) -> String {
    let mut style = "'none'".to_string();
    if let Some(start) = find(page, b"<style>") {
        let body = &page[start + 7..];
        if let Some(end) = find(body, b"</style>") {
            let digest = Sha256::digest(&body[..end]);
            style = format!("'sha256-{}'", base64::engine::general_purpose::STANDARD.encode(digest));
        }
    }
    format!(
        "default-src 'none'; style-src {style}; img-src 'self'; frame-ancestors 'none'; \
base-uri 'none'; form-action 'none'"
    )
}

pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `_html_escape`: `&`, `<`, `>`, `"`.
pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    // py: test_logic.TestPrivateIp.test_allowed
    #[test]
    fn private_allowed() {
        for a in ["127.0.0.1", "::1", "10.0.0.5", "172.16.3.4", "::ffff:10.1.2.3", "fd00::1", "169.254.1.1",
                  "192.168.0.1", "fe80::1%eth0"] {
            assert!(is_private_client(Some(a)), "{a}");
        }
    }

    // py: test_logic.TestPrivateIp.test_rejected
    #[test]
    fn private_rejected() {
        for a in ["8.8.8.8", "1.1.1.1", "2001:4860:4860::8888", "::ffff:8.8.8.8", "", "garbage", "100.64.0.1",
                  "224.0.0.1", "010.0.0.1", "192.0.0.9"] {
            assert!(!is_private_client(Some(a)), "{a}");
        }
        assert!(!is_private_client(None));
    }

    #[test]
    fn python_is_private_table() {
        for a in ["0.1.2.3", "192.0.0.5", "192.0.0.171", "192.0.2.9", "198.19.1.1", "198.51.100.3",
                  "203.0.113.7", "240.0.0.1", "255.255.255.255"] {
            assert!(is_private_client(Some(a)), "{a}");
        }
        for a in ["::", "64:ff9b:1::1", "100::1", "2001::1", "2001:db8::1", "2002::1", "3fff::1", "fc00::1",
                  "fe80::1"] {
            assert!(is_private_client(Some(a)), "{a}");
        }
        for a in ["2001:1::1", "2001:1::2", "2001:3::1", "2001:4:112::1", "2001:20::1", "2001:30::1",
                  "2a00:1450:4001::1", "192.0.0.10"] {
            assert!(!is_private_client(Some(a)), "{a}");
        }
    }

    // py: test_security.TestHostAllowed.test_allowed
    #[test]
    fn host_allowed_cases() {
        for h in ["127.0.0.1", "127.0.0.1:8800", "192.168.0.5:8766", "10.1.2.3", "[::1]", "[::1]:8766",
                  "[fe80::1%25eth0]:1", "localhost", "localhost:8765", "LOCALHOST", "localhost.",
                  "plexpc.local", "PlexPC.local:8766", "a.b.local", "plexpc.local."] {
            assert!(host_allowed(Some(h), &[]), "{h}");
        }
    }

    // py: test_security.TestHostAllowed.test_rejected
    #[test]
    fn host_rejected_cases() {
        let long = "x".repeat(300);
        for h in ["", " ", "rebind.evil", "rebind.evil:8766", "evil.com", "local", ".local",
                  "plexpc.local.evil.com", "localhost.evil.com", "127.0.0.1.nip.io", "2130706433",
                  "a b", &long, "[nonsense]", "plexpc.lan"] {
            assert!(!host_allowed(Some(h), &[]), "{h}");
        }
        assert!(!host_allowed(None, &[]));
    }

    // py: test_security.TestHostAllowed.test_configured_names
    #[test]
    fn host_configured_names() {
        assert!(host_allowed(Some("plexpc.lan:8766"), &["PlexPC.lan".into()]));
        assert!(host_allowed(Some("plexpc.lan"), &["plexpc.lan.".into()]));
        assert!(!host_allowed(Some("other.lan"), &["plexpc.lan".into()]));
        // Python also passes None and 5: Config::allowed_hosts drops non-strings before this.
        assert!(!host_allowed(Some("plexpc.lan"), &["".into()]));
    }

    // py: test_web.TestHttpsUrl.test_swaps_port_keeps_host
    #[test]
    fn https_url_swaps_port_keeps_host() {
        let f = "127.0.0.1";
        assert_eq!(https_url(Some("192.168.1.50:8765"), 8766, "/", f), "https://192.168.1.50:8766/");
        assert_eq!(https_url(Some("192.168.1.50"), 8766, "/foo?x=1", f), "https://192.168.1.50:8766/foo?x=1");
        assert_eq!(https_url(Some("PlexPC.local:8765"), 8766, "/a", f), "https://plexpc.local:8766/a");
        assert_eq!(https_url(Some("localhost:8765"), 443, "/", f), "https://localhost/");
    }

    // py: test_web.TestHttpsUrl.test_ipv6_host
    #[test]
    fn https_url_ipv6_host() {
        assert_eq!(https_url(Some("[fe80::1]:8765"), 8766, "/", "127.0.0.1"), "https://[fe80::1]:8766/");
        assert_eq!(https_url(Some("[::1]"), 8766, "/", "127.0.0.1"), "https://[::1]:8766/");
    }

    // py: test_web.TestHttpsUrl.test_bad_host_uses_fallback
    #[test]
    fn https_url_bad_host_uses_fallback() {
        let long = "x".repeat(300);
        let bad: [Option<&str>; 9] = [None, Some(""), Some("evil.com/x"), Some("a b"), Some("host:abc"),
                                      Some("[nonsense]:1"), Some(&long), Some("h@st"), Some("a:1:2")];
        for b in bad {
            assert_eq!(https_url(b, 8766, "/", "192.168.1.50"), "https://192.168.1.50:8766/", "{b:?}");
        }
        assert_eq!(https_url(Some(""), 8766, "/", "::1"), "https://[::1]:8766/");
    }

    // py: test_web.TestHttpsUrl.test_path_sanitised
    #[test]
    fn https_url_path_sanitised() {
        let f = "127.0.0.1";
        assert_eq!(https_url(Some("10.0.0.2"), 8766, "//evil.com/x", f), "https://10.0.0.2:8766/evil.com/x");
        assert_eq!(https_url(Some("10.0.0.2"), 8766, "/a\r\nSet-Cookie: x", f), "https://10.0.0.2:8766/");
        assert_eq!(https_url(Some("10.0.0.2"), 8766, "foo", f), "https://10.0.0.2:8766/");
        assert_eq!(https_url(Some("10.0.0.2"), 8766, "", f), "https://10.0.0.2:8766/");
    }

    // py: test_web.TestStatic.test_install_csp_hashes_inline_style
    #[test]
    fn install_csp_hashes_inline_style() {
        let csp = install_csp(b"<html><style>body{color:red}</style></html>");
        assert!(csp.contains("style-src 'sha256-"));
        assert!(csp.contains("default-src 'none'"));
        assert!(!csp.contains("script-src"));
        // python: base64.b64encode(hashlib.sha256(b"body{color:red}").digest())
        assert!(csp.contains("'sha256-FcQqt3aNlV7AZnGV4zkQRVeCeJOxbMPnQSx258L803E='"), "{csp}");
        assert!(install_csp(b"<html></html>").contains("style-src 'none';"));
    }
}
