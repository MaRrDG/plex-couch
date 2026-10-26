//! The static whitelist (app.py:61-83), embedded at build time. Exact path match only; no
//! directory serving. `install.html` is only for the install listener, `fonts/OFL.txt` is
//! never served.

use crate::http::security::html_escape;
use crate::json::json_str_ascii_body;

pub struct StaticFile {
    pub url: &'static str,
    pub name: &'static str,
    pub content_type: &'static str,
    pub body: &'static [u8],
}

macro_rules! file {
    ($url:literal, $name:literal, $ctype:literal) => {
        StaticFile {
            url: $url,
            name: $name,
            content_type: $ctype,
            body: include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/static/", $name)),
        }
    };
}

pub static STATIC_FILES: &[StaticFile] = &[
    file!("/", "index.html", "text/html; charset=utf-8"),
    file!("/index.html", "index.html", "text/html; charset=utf-8"),
    file!("/app.js", "app.js", "text/javascript; charset=utf-8"),
    file!("/style.css", "style.css", "text/css; charset=utf-8"),
    file!("/tokens.css", "tokens.css", "text/css; charset=utf-8"),
    file!("/fonts/figtree-latin.woff2", "fonts/figtree-latin.woff2", "font/woff2"),
    file!("/fonts/figtree-latin-ext.woff2", "fonts/figtree-latin-ext.woff2", "font/woff2"),
    file!("/sw.js", "sw.js", "text/javascript; charset=utf-8"),
    file!("/manifest.webmanifest", "manifest.webmanifest", "application/manifest+json; charset=utf-8"),
    file!("/icon.svg", "icon.svg", "image/svg+xml"),
    file!("/icon-192.png", "icon-192.png", "image/png"),
    file!("/icon-512.png", "icon-512.png", "image/png"),
    file!("/icon-maskable-512.png", "icon-maskable-512.png", "image/png"),
    file!("/apple-touch-icon.png", "apple-touch-icon.png", "image/png"),
];

/// Files whose `{{APP_NAME}}` is replaced.
pub const TEMPLATED: &[&str] = &["index.html", "manifest.webmanifest"];

/// Extra headers per URL (`STATIC_HEADERS`).
pub fn static_headers(url: &str) -> &'static [(&'static str, &'static str)] {
    match url {
        "/sw.js" => &[("Service-Worker-Allowed", "/")],
        _ => &[],
    }
}

pub static INSTALL_PAGE: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/static/install.html"));

pub fn lookup(url: &str) -> Option<&'static StaticFile> {
    STATIC_FILES.iter().find(|f| f.url == url)
}

pub fn replace_bytes(hay: &[u8], needle: &[u8], with: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(hay.len());
    let mut i = 0;
    while i < hay.len() {
        if hay[i..].starts_with(needle) {
            out.extend_from_slice(with);
            i += needle.len();
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}

/// The body to serve: templated files get the app name (HTML-escaped for `.html`, a JSON
/// string body with ASCII escapes for the manifest).
pub fn render(f: &StaticFile, app_name: &str) -> Vec<u8> {
    if !TEMPLATED.contains(&f.name) {
        return f.body.to_vec();
    }
    let safe = if f.name.ends_with(".html") { html_escape(app_name) } else { json_str_ascii_body(app_name) };
    replace_bytes(f.body, b"{{APP_NAME}}", safe.as_bytes())
}

/// The install page with the app name and the HTTPS URL filled in.
pub fn install_page(app_name: &str, https_url: &str) -> Vec<u8> {
    let page = replace_bytes(INSTALL_PAGE, b"{{APP_NAME}}", html_escape(app_name).as_bytes());
    replace_bytes(&page, b"{{HTTPS_URL}}", html_escape(https_url).as_bytes())
}

pub const NO_CERT_PAGE: &str = "<!doctype html>
<html lang=\"ro\">
<head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">
<title>{name}</title></head>
<body>
<h1>Certificatele HTTPS lipsesc</h1>
<p>{name} nu pornește fără HTTPS: certificatul sau cheia lipsesc ori nu pot fi citite.</p>
<p>Pe PC, rulează <code>plex-couch make-certs --ip &lt;IP-ul PC-ului&gt;</code> și repornește {name}.</p>
</body>
</html>
";

pub fn no_cert_page(app_name: &str) -> Vec<u8> {
    NO_CERT_PAGE.replace("{name}", &html_escape(app_name)).into_bytes()
}

/// DER bytes of a certificate given as PEM text or DER bytes (`logic.cert_der`, with
/// `ssl.PEM_cert_to_DER_cert`'s rules: the text must start with the header and end, once
/// stripped, with the footer). Err for anything that is not a usable certificate file.
pub fn cert_der(data: &[u8]) -> Result<Vec<u8>, &'static str> {
    const HEADER: &str = "-----BEGIN CERTIFICATE-----";
    const FOOTER: &str = "-----END CERTIFICATE-----";
    let trimmed = data.iter().position(|b| !b.is_ascii_whitespace()).map(|i| &data[i..]).unwrap_or(&[]);
    if !trimmed.starts_with(b"-----BEGIN") {
        return Ok(data.to_vec());
    }
    let text = std::str::from_utf8(data).map_err(|_| "not ascii")?;
    if !text.is_ascii() {
        return Err("not ascii");
    }
    if !text.starts_with(HEADER) {
        return Err("Invalid PEM encoding");
    }
    let t = text.trim();
    if !t.ends_with(FOOTER) {
        return Err("Invalid PEM encoding");
    }
    let body: String = t[HEADER.len()..t.len() - FOOTER.len()]
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '/' || *c == '=')
        .collect();
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(body).map_err(|_| "Incorrect padding")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn static_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("static")
    }

    fn read(name: &str) -> String {
        String::from_utf8(std::fs::read(static_dir().join(name)).unwrap()).unwrap()
    }

    // py: test_web.TestStatic.test_whitelist_has_pwa_files
    #[test]
    fn whitelist_has_pwa_files() {
        for p in ["/sw.js", "/manifest.webmanifest", "/icon-192.png", "/icon-512.png", "/icon-maskable-512.png",
                  "/apple-touch-icon.png", "/icon.svg"] {
            assert!(lookup(p).is_some(), "{p}");
        }
        assert_eq!(lookup("/sw.js").unwrap().content_type, "text/javascript; charset=utf-8");
        assert!(lookup("/manifest.webmanifest").unwrap().content_type.starts_with("application/manifest+json"));
        assert_eq!(static_headers("/sw.js"), &[("Service-Worker-Allowed", "/")]);
    }

    // py: test_web.TestStatic.test_whitelisted_files_exist_and_nothing_else_is_served
    #[test]
    fn whitelisted_files_exist_and_nothing_else_is_served() {
        for f in STATIC_FILES {
            assert!(static_dir().join(f.name).is_file(), "{}", f.url);
            assert!(!f.name.contains(".."));
            assert!(!f.name.contains('\\'));
            if f.name.contains('/') {
                let rest = f.name.strip_prefix("fonts/").expect("only fonts/ is nested");
                let stem = rest.strip_suffix(".woff2").expect("woff2 only");
                assert!(stem.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'));
                assert_eq!(f.url, format!("/{}", f.name));
            }
            // the embedded copy is the file on disk
            assert_eq!(f.body, std::fs::read(static_dir().join(f.name)).unwrap().as_slice(), "{}", f.name);
        }
        assert!(lookup("/install.html").is_none());
        assert!(lookup("/fonts/OFL.txt").is_none());
    }

    // py: test_web.TestStatic.test_whitelist_has_design_files
    #[test]
    fn whitelist_has_design_files() {
        let t = lookup("/tokens.css").unwrap();
        assert_eq!((t.name, t.content_type), ("tokens.css", "text/css; charset=utf-8"));
        for sub in ["latin", "latin-ext"] {
            let path = format!("/fonts/figtree-{sub}.woff2");
            let f = lookup(&path).unwrap();
            assert_eq!((f.name, f.content_type), (&path[1..], "font/woff2"));
            assert_eq!(&f.body[..4], b"wOF2");
        }
        assert!(static_dir().join("fonts").join("OFL.txt").is_file());
        let css = read("style.css");
        assert!(css.contains("/fonts/figtree-latin.woff2"));
        assert!(css.contains("/fonts/figtree-latin-ext.woff2"));
        let html = read("index.html");
        assert!(html.find("/tokens.css").unwrap() < html.find("/style.css").unwrap());
    }

    // py: test_web.TestStatic.test_style_uses_only_token_colors
    #[test]
    fn style_uses_only_token_colors() {
        // re.search(r"#[0-9a-fA-F]{3,8}\b|rgba?\(|hsla?\(", css) is None
        let css = read("style.css");
        let b = css.as_bytes();
        for (i, _) in css.match_indices('#') {
            let n = b[i + 1..].iter().take_while(|c| c.is_ascii_hexdigit()).count();
            if n >= 3 {
                // \b after 3..=8 hex digits: some k in 3..=min(n,8) is followed by a non-word char
                let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
                let hit = (3..=n.min(8)).any(|k| b.get(i + 1 + k).is_none_or(|c| !word(*c)));
                assert!(!hit, "hex colour at {i}");
            }
        }
        for f in ["rgb(", "rgba(", "hsl(", "hsla("] {
            assert!(!css.contains(f), "{f}");
        }
    }

    // py: test_web.TestStatic.test_manifest_icons
    #[test]
    fn manifest_icons() {
        let raw = read("manifest.webmanifest").replace("{{APP_NAME}}", "Plex Couch");
        let m: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!((m["start_url"].as_str(), m["scope"].as_str(), m["display"].as_str()),
                   (Some("/"), Some("/"), Some("standalone")));
        let purposes: Vec<(String, String)> = m["icons"].as_array().unwrap().iter()
            .map(|i| (i["sizes"].as_str().unwrap().into(), i["purpose"].as_str().unwrap().into()))
            .collect();
        for want in [("192x192", "any"), ("512x512", "any"), ("512x512", "maskable")] {
            assert!(purposes.contains(&(want.0.into(), want.1.into())), "{want:?}");
        }
        for i in m["icons"].as_array().unwrap() {
            assert!(lookup(i["src"].as_str().unwrap()).is_some());
        }
    }

    // py: test_web.TestStatic.test_sw_precaches_whitelisted_files_only
    #[test]
    fn sw_precaches_whitelisted_files_only() {
        let sw = read("sw.js");
        assert!(sw.contains("'/api/'"));
        for p in ["/", "/app.js", "/style.css", "/tokens.css", "/manifest.webmanifest", "/icon-192.png",
                  "/fonts/figtree-latin.woff2", "/fonts/figtree-latin-ext.woff2"] {
            assert!(sw.contains(&format!("'{p}'")), "{p}");
        }
        assert!(sw.contains("'/img/'"));
    }

    #[test]
    fn templating() {
        let idx = render(lookup("/").unwrap(), "A&B <\"x\">");
        assert!(!idx.windows(12).any(|w| w == b"{{APP_NAME}}"));
        let s = String::from_utf8(idx).unwrap();
        assert!(s.contains("A&amp;B &lt;&quot;x&quot;&gt;"));
        let man = String::from_utf8(render(lookup("/manifest.webmanifest").unwrap(), "Plex \"ș\"")).unwrap();
        assert!(man.contains("\"name\": \"Plex \\\"\\u0219\\\"\""), "{man}");
        assert_eq!(render(lookup("/app.js").unwrap(), "x"), lookup("/app.js").unwrap().body);
    }

    // py: test_web.TestCertDer.test_pem_and_der
    #[test]
    fn pem_and_der() {
        let pem = "-----BEGIN CERTIFICATE-----\nMAMCAQA=\n-----END CERTIFICATE-----\n";
        let der = cert_der(pem.as_bytes()).unwrap();
        // ssl.PEM_cert_to_DER_cert(PEM) == b"0\x03\x02\x01\x00"
        assert_eq!(der, vec![0x30, 0x03, 0x02, 0x01, 0x00]);
        assert_eq!(cert_der(&der).unwrap(), der);
        assert!(cert_der(b"  -----BEGIN CERTIFICATE-----\nMAMCAQA=\n-----END CERTIFICATE-----\n").is_err());
        assert!(cert_der(b"-----BEGIN CERTIFICATE-----\nMAMCAQA=\n").is_err());
    }
}
