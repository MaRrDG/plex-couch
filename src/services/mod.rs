//! Upstream clients: Plex, Radarr, Sonarr, Bazarr, Prowlarr, qBittorrent, ntfy (spec 5).
//!
//! Done (Phase 1): the shared [`Http`] client with the "never follow a redirect when a key
//! is attached" rule (spec 4.8), [`keys`] (spec 2.4: API keys and the Plex token, cached
//! 300 s, the empty string cached too), and the client structs every route and job reaches
//! through `app.services`.
//! TODO(Phase 2a): the request methods in `plex`, `radarr`, `bazarr`, `prowlarr`,
//! `qbittorrent`, `ntfy`. TODO(Phase 2b): job-only calls (import lists, manual import,
//! rescans, Sonarr) — put them in `sonarr.rs` and in new files (e.g. `radarr_jobs.rs`)
//! rather than in the 2a files, to avoid edit conflicts.
//!
//! API keys and tokens are never logged or returned; failures are logged by type only.

pub mod arr_jobs;
pub mod bazarr;
pub mod cache;
pub mod keys;
pub mod ntfy;
pub mod plex;
pub mod plex_jobs;
pub mod prowlarr;
pub mod qbit_jobs;
pub mod qbittorrent;
pub mod radarr;
pub mod sonarr;

use crate::config::Config;
use crate::{log_debug, log_warning};
use bytes::Bytes;
use reqwest::{redirect, Method};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// Default upstream timeout (`TIMEOUT = 2.5`).
pub const TIMEOUT: Duration = Duration::from_millis(2500);
pub const IMG_TIMEOUT: Duration = Duration::from_secs(6);
pub const IMG_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Request headers that carry a service key or token (compared case-insensitively).
pub const SECRET_HEADERS: [&str; 2] = ["x-api-key", "x-plex-token"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamError {
    /// A request carrying a key got a redirect (never followed).
    RedirectRefused,
    /// A non-2xx answer (after redirects, when they are allowed).
    Status(u16),
    Timeout,
    /// Connection refused/reset, DNS, TLS.
    Connect,
    /// The body could not be read or was not valid JSON.
    Body,
    /// Anything else, e.g. a stub that is not implemented yet.
    Other(&'static str),
}

impl UpstreamError {
    /// The error type name the log shows (never the URL or the message).
    pub fn type_name(&self) -> &'static str {
        match self {
            UpstreamError::RedirectRefused => "RedirectRefused",
            UpstreamError::Status(_) => "HTTPError",
            UpstreamError::Timeout => "TimeoutError",
            UpstreamError::Connect => "URLError",
            UpstreamError::Body => "JSONDecodeError",
            UpstreamError::Other(_) => "Exception",
        }
    }
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamError::Status(c) => write!(f, "HTTP {c}"),
            UpstreamError::Other(m) => f.write_str(m),
            e => f.write_str(e.type_name()),
        }
    }
}

impl std::error::Error for UpstreamError {}

pub type UpResult<T> = Result<T, UpstreamError>;

pub fn carries_secret(headers: &[(&str, &str)]) -> bool {
    headers.iter().any(|(k, _)| SECRET_HEADERS.iter().any(|s| k.eq_ignore_ascii_case(s)))
}

/// A finished upstream response.
pub struct UpResponse {
    pub status: u16,
    pub headers: reqwest::header::HeaderMap,
    pub body: Bytes,
}

/// The shared upstream client. Cheap to clone.
#[derive(Clone)]
pub struct Http {
    /// Follows redirects (requests without a key: Prowlarr ping, ntfy, qBittorrent).
    follow: reqwest::Client,
    /// Never follows (any request that carries `X-Api-Key` / `X-Plex-Token`).
    guarded: reqwest::Client,
}

impl Default for Http {
    fn default() -> Self {
        Http::new()
    }
}

fn map_err(e: reqwest::Error) -> UpstreamError {
    if e.is_timeout() {
        UpstreamError::Timeout
    } else if e.is_connect() || e.is_request() {
        UpstreamError::Connect
    } else {
        UpstreamError::Body
    }
}

impl Http {
    pub fn new() -> Http {
        // No proxies: the services are local and ntfy is reached directly.
        let base = || reqwest::Client::builder().no_proxy().pool_idle_timeout(Duration::from_secs(30));
        Http {
            follow: base().redirect(redirect::Policy::limited(10)).build().expect("http client"),
            guarded: base().redirect(redirect::Policy::none()).build().expect("http client"),
        }
    }

    /// Send a request. Non-2xx is an error; a redirect answer to a request that carries a
    /// key is refused (WARNING `upstream redirect refused (request carries a key)`).
    pub async fn request(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> UpResult<UpResponse> {
        let resp = self.send_raw(method, url, headers, body, timeout).await?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(UpstreamError::Status(status));
        }
        let hdrs = resp.headers().clone();
        let body = resp.bytes().await.map_err(map_err)?;
        Ok(UpResponse { status, headers: hdrs, body })
    }

    async fn send_raw(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> UpResult<reqwest::Response> {
        let secret = carries_secret(headers);
        let client = if secret { &self.guarded } else { &self.follow };
        let mut rb = client.request(method, url).timeout(timeout);
        for (k, v) in headers {
            rb = rb.header(*k, *v);
        }
        if let Some(b) = body {
            rb = rb.body(b);
        }
        let resp = rb.send().await.map_err(map_err)?;
        if secret && resp.status().is_redirection() {
            log_warning!("upstream redirect refused (request carries a key)");
            return Err(UpstreamError::RedirectRefused);
        }
        Ok(resp)
    }

    /// `_get_json`: GET with `Accept: application/json`; an empty body gives None.
    pub async fn get_json(&self, url: &str, headers: &[(&str, &str)], timeout: Duration) -> UpResult<Option<Value>> {
        let mut h: Vec<(&str, &str)> = headers.to_vec();
        h.push(("Accept", "application/json"));
        let r = self.request(Method::GET, url, &h, None, timeout).await?;
        if r.body.is_empty() {
            return Ok(None);
        }
        let text = std::str::from_utf8(&r.body).map_err(|_| UpstreamError::Body)?;
        serde_json::from_str(text).map(Some).map_err(|_| UpstreamError::Body)
    }

    /// `_get_image`: `(bytes, content type)` or None. At most 4 MiB; an empty or larger body
    /// is no image; a non-`image/*` type becomes `image/jpeg`. Errors are logged by type.
    pub async fn get_image(&self, url: &str, headers: &[(&str, &str)]) -> Option<(Vec<u8>, String)> {
        let mut h: Vec<(&str, &str)> = headers.to_vec();
        h.push(("Accept", "image/*"));
        let mut resp = match self.send_raw(Method::GET, url, &h, None, IMG_TIMEOUT).await {
            Ok(r) if r.status().is_success() => r,
            Ok(_) => return None,
            Err(e) => {
                log_debug!("image fetch failed: {}", e.type_name());
                return None;
            }
        };
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let mut body = Vec::new();
        loop {
            match resp.chunk().await {
                Ok(Some(c)) => {
                    body.extend_from_slice(&c);
                    if body.len() > IMG_MAX_BYTES {
                        return None;
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    log_debug!("image fetch failed: OSError");
                    return None;
                }
            }
        }
        if body.is_empty() {
            return None;
        }
        let ctype = if ctype.starts_with("image/") { ctype } else { "image/jpeg".to_string() };
        Some((body, ctype))
    }
}

/// `services.<name>.url` without a trailing `/`, plus `path`.
pub fn service_url(config: &Config, name: &str, path: &str) -> String {
    let base = config.service(name).get("url").and_then(Value::as_str).unwrap_or("").trim_end_matches('/').to_string();
    format!("{base}{path}")
}

/// Every upstream client, reached as `app.services.<name>`.
pub struct Services {
    pub keys: Arc<keys::Keys>,
    pub plex: plex::Plex,
    pub radarr: radarr::Radarr,
    pub sonarr: sonarr::Sonarr,
    pub bazarr: bazarr::Bazarr,
    pub prowlarr: prowlarr::Prowlarr,
    pub qbittorrent: qbittorrent::Qbittorrent,
}

impl Services {
    pub fn new(config: Arc<Config>, http: Http) -> Services {
        let keys = Arc::new(keys::Keys::new(config.clone()));
        let c = || Client { config: config.clone(), http: http.clone(), keys: keys.clone() };
        Services {
            plex: plex::Plex::new(c()),
            radarr: radarr::Radarr::new(c()),
            sonarr: sonarr::Sonarr::new(c()),
            bazarr: bazarr::Bazarr::new(c()),
            prowlarr: prowlarr::Prowlarr::new(c()),
            qbittorrent: qbittorrent::Qbittorrent::new(c()),
            keys,
        }
    }
}

/// What every service client holds.
#[derive(Clone)]
pub struct Client {
    pub config: Arc<Config>,
    pub http: Http,
    pub keys: Arc<keys::Keys>,
}

impl Client {
    pub fn url(&self, name: &str, path: &str) -> String {
        service_url(&self.config, name, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A tiny upstream: `/redir...` answers 302 to `/target`; anything else 200 JSON (or
    /// image/png for paths with "img"). Records the headers of every non-redirect request.
    async fn upstream() -> (String, Arc<Mutex<Vec<String>>>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let hits = Arc::new(Mutex::new(Vec::new()));
        let h2 = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                let hits = h2.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
                    let resp = if path.starts_with("/redir") {
                        "HTTP/1.1 302 Found\r\nLocation: /target\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                    } else {
                        hits.lock().unwrap().push(req.clone());
                        let ct = if path.contains("img") { "image/png" } else { "application/json" };
                        format!("HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: 12\r\nConnection: close\r\n\r\n{{\"ok\": true}}")
                    };
                    let _ = s.write_all(resp.as_bytes()).await;
                    let _ = s.shutdown().await;
                });
            }
        });
        (base, hits)
    }

    // py: test_security.TestNoRedirectWithKeys.test_redirect_refused_with_a_key
    #[tokio::test]
    async fn redirect_refused_with_a_key() {
        let (base, hits) = upstream().await;
        let http = Http::new();
        for hdr in [("X-Api-Key", "k"), ("X-Plex-Token", "t"), ("X-API-KEY", "k")] {
            let r = http.get_json(&format!("{base}/redir"), &[hdr], TIMEOUT).await;
            assert_eq!(r.unwrap_err(), UpstreamError::RedirectRefused);
            assert!(http.get_image(&format!("{base}/redir-img"), &[hdr]).await.is_none());
        }
        assert!(hits.lock().unwrap().is_empty());
    }

    // py: test_security.TestNoRedirectWithKeys.test_redirect_followed_without_a_key
    #[tokio::test]
    async fn redirect_followed_without_a_key() {
        let (base, hits) = upstream().await;
        let http = Http::new();
        let v = http.get_json(&format!("{base}/redir"), &[], TIMEOUT).await.unwrap();
        assert_eq!(v, Some(serde_json::json!({"ok": true})));
        assert_eq!(hits.lock().unwrap().len(), 1);
    }

    // py: test_security.TestNoRedirectWithKeys.test_qbittorrent_opener_has_the_handler
    // (Rust: the qBittorrent client sends through the same guarded `Http`, so a key header
    //  on any of its requests is never forwarded across a redirect.)
    #[tokio::test]
    async fn qbittorrent_client_uses_the_guarded_http() {
        let (base, hits) = upstream().await;
        let d = std::env::temp_dir().join(format!("pc-svc-{}", crate::config::token_hex(6)));
        let cfg = Arc::new(Config::load(&d.join("cfg.json")).unwrap());
        let svc = Services::new(cfg, Http::new());
        let r = svc.qbittorrent.http().get_json(&format!("{base}/redir"), &[("X-Api-Key", "k")], TIMEOUT).await;
        assert_eq!(r.unwrap_err(), UpstreamError::RedirectRefused);
        assert!(hits.lock().unwrap().is_empty());
    }
}
