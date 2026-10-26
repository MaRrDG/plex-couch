//! HTTP surface (spec section 3): listeners ([`server`]), routing ([`routes`]), the static
//! whitelist ([`static_files`]) and the request security rules ([`security`]).
//!
//! Every response built through [`send`] carries the security headers, HSTS on the HTTPS
//! listener, Content-Type, Content-Length, Cache-Control (`no-store` for JSON, else
//! `no-cache`, unless the caller set one), the caller's extra headers and a renewed session
//! cookie when one is pending and the caller did not set a cookie itself (spec 3.1).

pub mod routes;
pub mod security;
pub mod server;
pub mod static_files;

use bytes::Bytes;
use http_body_util::Full;
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Response, StatusCode};
use serde_json::Value;

pub type Body = Full<Bytes>;

pub const JSON: &str = "application/json; charset=utf-8";
pub const TEXT: &str = "text/plain; charset=utf-8";
pub const HTML: &str = "text/html; charset=utf-8";

/// Per-request response context.
#[derive(Default)]
pub struct ReqCtx {
    /// The HTTPS listener: HSTS on every response, `Secure` cookies.
    pub secure: bool,
    /// HEAD: same headers, no body.
    pub head: bool,
    /// `Set-Cookie` value of a renewed session for this response.
    pub renew: Option<String>,
}

fn push(resp: &mut Response<Body>, name: &str, value: &str) {
    if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
        resp.headers_mut().append(n, v);
    }
}

/// Build a response (`Handler._send`).
pub fn send(rc: &ReqCtx, code: u16, body: Vec<u8>, ctype: &str, headers: &[(&str, &str)]) -> Response<Body> {
    send_with_csp(rc, code, body, ctype, headers, security::CSP)
}

pub fn send_with_csp(
    rc: &ReqCtx,
    code: u16,
    body: Vec<u8>,
    ctype: &str,
    headers: &[(&str, &str)],
    csp: &str,
) -> Response<Body> {
    let len = body.len();
    let mut resp = Response::new(if rc.head { Full::new(Bytes::new()) } else { Full::new(Bytes::from(body)) });
    *resp.status_mut() = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    for (k, v) in security::SECURITY_HEADERS {
        push(&mut resp, k, if k == "Content-Security-Policy" { csp } else { v });
    }
    if rc.secure {
        push(&mut resp, "Strict-Transport-Security", security::HSTS);
    }
    push(&mut resp, "Content-Type", ctype);
    push(&mut resp, "Content-Length", &len.to_string());
    let has = |name: &str| headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
    if !has("Cache-Control") {
        push(&mut resp, "Cache-Control", if ctype.starts_with("application/json") { "no-store" } else { "no-cache" });
    }
    for (k, v) in headers {
        push(&mut resp, k, v);
    }
    if let Some(c) = rc.renew.as_deref() {
        if !has("Set-Cookie") {
            push(&mut resp, "Set-Cookie", c);
        }
    }
    resp
}

/// A JSON response (`Handler._json`): `json.dumps(obj, ensure_ascii=False)`.
pub fn json(rc: &ReqCtx, code: u16, v: &Value, headers: &[(&str, &str)]) -> Response<Body> {
    send(rc, code, crate::json::dumps(v), JSON, headers)
}

pub fn not_found(rc: &ReqCtx) -> Response<Body> {
    send(rc, 404, b"Not found".to_vec(), TEXT, &[])
}
