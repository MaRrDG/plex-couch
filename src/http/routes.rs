//! Request routing for the three listener roles (spec 3.2-3.7).
//!
//! Order on every listener: client IP rule (403) -> Host allowlist (421) -> method
//! (GET/HEAD/POST; anything else 405 with the normal headers, plan fix 6) -> role.
//! A POST body over 4096 bytes gets 400 and the connection is closed (plan fix 5); so does
//! any response that leaves a request body unread.

use crate::app::AppState;
use crate::auth::session::{cookie_token, session_cookie, COOKIE, SESSION_DAYS};
use crate::http::security::{host_allowed, https_url, install_csp, is_private_client};
use crate::http::server::{ConnCtx, Role};
use crate::http::static_files::{self, cert_der, install_page, no_cert_page};
use crate::http::{json, not_found, send, send_with_csp, Body, ReqCtx, HTML, TEXT};
use crate::json::py_repr_bool_map;
use crate::logic::images::parse_img_path;
use crate::system::power::CancelOutcome;
use crate::{log_error, log_info, log_warning};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::header::{HeaderValue, CONNECTION, CONTENT_LENGTH, HOST, TRANSFER_ENCODING};
use hyper::{Method, Request, Response};
use serde_json::{json, Map, Value};
use std::sync::Arc;

pub const CSRF_HEADER: &str = "X-Requested-With";
pub const CSRF_VALUE: &str = "plex-couch";
pub const MAX_BODY: i64 = 4096;
pub const IMG_CACHE_CONTROL: &str = "private, max-age=86400";
pub const CA_CONTENT_TYPE: &str = "application/x-x509-ca-cert";

/// What the routes need from a request, taken apart once.
pub struct Req {
    pub method: Method,
    /// The request target as sent (`self.path`): path plus query, no decoding.
    pub target: String,
    /// `target.split("?", 1)[0]`.
    pub path: String,
    pub host: Option<String>,
    pub headers: hyper::HeaderMap,
    pub body: Option<Incoming>,
    pub has_body: bool,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    fn token(&self) -> Option<String> {
        cookie_token(self.header("Cookie"))
    }

    /// `Content-Length`: missing or not an int counts as 0.
    fn content_length(&self) -> i64 {
        self.header(CONTENT_LENGTH.as_str()).and_then(|v| v.trim().parse::<i64>().ok()).unwrap_or(0)
    }
}

/// Raw target: origin-form `path?query`; absolute-form keeps the whole URI.
fn raw_target(uri: &hyper::Uri) -> String {
    if uri.scheme().is_some() {
        return uri.to_string();
    }
    uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| uri.path().to_string())
}

/// Entry point for one request. `None` drops the connection without an answer (a body that
/// never arrived, a failed config write).
pub async fn handle(req: Request<Incoming>, ctx: Arc<ConnCtx>) -> Option<Response<Body>> {
    let (parts, body) = req.into_parts();
    let target = raw_target(&parts.uri);
    let path = target.split('?').next().unwrap_or("").to_string();
    let host = parts.headers.get(HOST).and_then(|v| v.to_str().ok()).map(String::from);
    let has_body = parts.headers.contains_key(TRANSFER_ENCODING)
        || parts
            .headers
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .is_some_and(|n| n > 0);
    let mut r = Req { method: parts.method, target, path, host, headers: parts.headers, body: Some(body), has_body };
    let mut rc = ReqCtx { secure: ctx.cfg.role.secure(), head: r.method == Method::HEAD, renew: None };

    let mut resp = match preflight(&r, &ctx, &rc) {
        Some(refused) => refused,
        None => match r.method {
            Method::GET | Method::HEAD | Method::POST => match ctx.cfg.role.clone() {
                Role::App { .. } => {
                    if r.method == Method::POST {
                        app_post(&mut r, &ctx, &mut rc).await?
                    } else {
                        app_get(&r, &ctx, &mut rc).await
                    }
                }
                Role::Install { https_port, ca_cert } => install(&r, &ctx, &rc, https_port, &ca_cert).await,
                Role::NoCert => {
                    let page = no_cert_page(&ctx.app.config.app_name());
                    send(&rc, 503, page, HTML, &[("Retry-After", "300"), ("Connection", "close")])
                }
            },
            _ => send(&rc, 405, b"Method not allowed".to_vec(), TEXT, &[("Allow", "GET, HEAD, POST")]),
        },
    };
    // A body we did not read must not be parsed as the next request.
    if r.has_body && r.body.is_some() {
        resp.headers_mut().insert(CONNECTION, HeaderValue::from_static("close"));
    }
    resp.headers_mut().insert("Server", HeaderValue::from_static("PlexCouch"));
    Some(resp)
}

/// IP rule, then Host allowlist. `Some(response)` when refused.
fn preflight(r: &Req, ctx: &ConnCtx, rc: &ReqCtx) -> Option<Response<Body>> {
    if !is_private_client(Some(&ctx.peer_ip)) {
        log_warning!("rejected non-private client {}", ctx.peer_ip);
        return Some(json(rc, 403, &json!({"error": "Acces permis doar din rețeaua locală."}), &[("Connection", "close")]));
    }
    if !host_allowed(r.host.as_deref(), &ctx.app.config.allowed_hosts()) {
        log_warning!("rejected Host header from {}", ctx.peer_ip); // the value is never logged
        return Some(send(rc, 421, "Gazdă nepermisă.".as_bytes().to_vec(), TEXT, &[("Connection", "close")]));
    }
    None
}

/// Session check; with `renew`, a soon-to-expire session gets a fresh cookie on this response.
fn auth(r: &Req, app: &AppState, rc: &mut ReqCtx, renew: bool) -> bool {
    let (ok, fresh) = app.check_session(r.token().as_deref(), None);
    if ok && renew {
        if let Some(t) = fresh {
            rc.renew = Some(session_cookie(COOKIE, &t, SESSION_DAYS * 86400, rc.secure));
        }
    }
    ok
}

fn unauthorized(rc: &ReqCtx) -> Response<Body> {
    json(rc, 401, &json!({"error": "Autentificare necesară."}), &[])
}

fn unknown(rc: &ReqCtx) -> Response<Body> {
    json(rc, 404, &json!({"error": "Necunoscut."}), &[])
}

// ---------------------------------------------------------------------------
// App listener: GET / HEAD
// ---------------------------------------------------------------------------

async fn app_get(r: &Req, ctx: &ConnCtx, rc: &mut ReqCtx) -> Response<Body> {
    let app = &ctx.app;
    let path = r.path.as_str();
    if path.starts_with("/api/") {
        if !auth(r, app, rc, true) {
            return unauthorized(rc);
        }
        if path == "/api/status" {
            let status = match &app.overrides.status {
                Some(f) => f(),
                None => crate::logic::status::status(app).await,
            };
            return json(rc, 200, &status, &[]);
        }
        if path == "/api/convert" {
            return json(rc, 200, &crate::jobs::convert::api_status(app), &[]);
        }
        if path == "/api/series" {
            return json(rc, 200, &crate::logic::series::api_list(app).await, &[]);
        }
        if path == "/api/subs" {
            return json(rc, 200, &crate::jobs::subs_manual::api_status(app).await, &[]);
        }
        if let Some(id) = path_id(path, "/api/series/") {
            let (code, out) = crate::logic::series::api_detail(app, id).await;
            return json(rc, code, &out, &[]);
        }
        if let Some(id) = path_id(path, "/api/movies/") {
            let (code, out) = crate::logic::series::api_movie(app, id).await;
            return json(rc, code, &out, &[]);
        }
        return unknown(rc);
    }
    if path.starts_with("/img/") {
        return image(r, app, rc).await;
    }
    let Some(f) = static_files::lookup(path) else { return not_found(rc) };
    let body = static_files::render(f, &app.config.app_name());
    send(rc, 200, body, f.content_type, static_files::static_headers(path))
}

/// `<prefix><id>` with an id of 1-10 ASCII digits, not 0 (`/api/series/12`).
pub fn path_id(path: &str, prefix: &str) -> Option<i64> {
    let s = path.strip_prefix(prefix)?;
    ((1..=10).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse::<i64>().ok()).flatten().filter(|n| *n > 0)
}

/// Poster proxy: same login and LAN checks as /api, no cookie renewal (cached responses).
async fn image(r: &Req, app: &Arc<AppState>, rc: &mut ReqCtx) -> Response<Body> {
    if !auth(r, app, rc, false) {
        return unauthorized(rc);
    }
    let Some((kind, ident)) = parse_img_path(Some(&r.path)) else { return not_found(rc) };
    let img = match &app.overrides.poster {
        Some(f) => f(kind, &ident),
        None => crate::logic::images::poster(app, kind, &ident).await,
    };
    match img {
        Some((bytes, ctype)) => send(rc, 200, bytes, &ctype, &[("Cache-Control", IMG_CACHE_CONTROL)]),
        None => not_found(rc),
    }
}

// ---------------------------------------------------------------------------
// App listener: POST
// ---------------------------------------------------------------------------

enum BodyErr {
    /// 400: too large or not JSON.
    Invalid,
    /// The body never arrived (timeout, reset): drop the connection.
    Broken,
}

/// `_body`: `{}` for no body, 400 for > 4096 bytes or bad JSON, `{}` for a non-object.
async fn read_body(r: &mut Req, timeout: std::time::Duration) -> Result<Map<String, Value>, BodyErr> {
    let n = r.content_length();
    if n <= 0 {
        return Ok(Map::new());
    }
    if n > MAX_BODY {
        return Err(BodyErr::Invalid); // unread: the connection is closed after the 400
    }
    let body = r.body.take().ok_or(BodyErr::Broken)?;
    let bytes = tokio::time::timeout(timeout, body.collect())
        .await
        .map_err(|_| BodyErr::Broken)?
        .map_err(|_| BodyErr::Broken)?
        .to_bytes();
    let text = std::str::from_utf8(&bytes).map_err(|_| BodyErr::Invalid)?;
    let text = if text.is_empty() { "{}" } else { text };
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Ok(Map::new()),
        Err(_) => Err(BodyErr::Invalid),
    }
}

async fn app_post(r: &mut Req, ctx: &ConnCtx, rc: &mut ReqCtx) -> Option<Response<Body>> {
    let app = &ctx.app;
    let ip = ctx.peer_ip.as_str();
    let path = r.path.clone();
    if r.header(CSRF_HEADER) != Some(CSRF_VALUE) {
        log_warning!("csrf header missing on {path} from {ip}");
        return Some(json(rc, 403, &json!({"error": "Cerere respinsă (CSRF)."}), &[]));
    }
    let body = match read_body(r, ctx.cfg.timeout).await {
        Ok(b) => b,
        Err(BodyErr::Invalid) => return Some(json(rc, 400, &json!({"error": "Cerere invalidă."}), &[("Connection", "close")])),
        Err(BodyErr::Broken) => return None,
    };

    if path == "/api/login" {
        let pin = match body.get("pin") {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        };
        let (code, out, token) = app.login(ip, pin).await;
        let cookie = token.map(|t| session_cookie(COOKIE, &t, SESSION_DAYS * 86400, rc.secure));
        let headers: Vec<(&str, &str)> = cookie.as_deref().map(|c| vec![("Set-Cookie", c)]).unwrap_or_default();
        return Some(json(rc, code, &out, &headers));
    }
    if !auth(r, app, rc, true) {
        return Some(unauthorized(rc));
    }

    let clear = session_cookie(COOKIE, "", 0, rc.secure);
    let resp = match path.as_str() {
        "/api/logout" => json(rc, 200, &json!({"ok": true}), &[("Set-Cookie", &clear)]),
        "/api/logout-all" => {
            log_info!("action logout-all from {ip}");
            if let Err(e) = app.logout_all() {
                log_error!("request error from {ip}: cannot save the config ({})", crate::state::io_error_name(&e));
                return None;
            }
            json(rc, 200, &json!({"ok": true}), &[("Set-Cookie", &clear)])
        }
        "/api/monitors/off" => {
            log_info!("action monitors/off from {ip}");
            let ok = crate::system::monitors::monitors_off(app.dry_run, "manual");
            json(rc, 200, &json!({"ok": ok, "dry_run": app.dry_run}), &[])
        }
        "/api/monitors/on" => {
            log_info!("action monitors/on from {ip}");
            let ok = crate::system::monitors::monitors_on(app.dry_run);
            json(rc, 200, &json!({"ok": ok, "dry_run": app.dry_run}), &[])
        }
        "/api/power/shutdown" | "/api/power/restart" => {
            let kind = path.rsplit('/').next().unwrap_or("");
            log_info!("action power/{kind} from {ip}");
            match app.actions.power(kind) {
                None => json(rc, 500, &json!({"error": "Comanda de oprire a eșuat (poate există deja una programată)."}), &[]),
                Some(p) => json(rc, 200, &json!({"ok": true, "power": p.to_json(), "dry_run": app.dry_run}), &[]),
            }
        }
        "/api/power/cancel" => {
            log_info!("action power/cancel from {ip}");
            let outcome = match &app.overrides.cancel_power {
                Some(f) => f(),
                None => {
                    // may wait for shutdown.exe and run `shutdown /a`: off the async workers
                    let a = ctx.app.clone();
                    tokio::task::spawn_blocking(move || a.actions.cancel_power()).await.unwrap_or(CancelOutcome::Nothing)
                }
            };
            if outcome == CancelOutcome::TooLate {
                json(
                    rc,
                    409,
                    &json!({"error": "Prea târziu: comanda pornise deja și nu a mai putut fi anulată.", "outcome": outcome.as_str()}),
                    &[],
                )
            } else {
                json(rc, 200, &json!({"ok": true, "power": null, "outcome": outcome.as_str()}), &[])
            }
        }
        "/api/power/after-movie" => {
            log_info!("action power/after-movie from {ip}");
            let (code, out) = crate::logic::power::arm_after_movie(app).await;
            json(rc, code, &out, &[])
        }
        "/api/power/after-movie/cancel" => {
            log_info!("action power/after-movie/cancel from {ip}");
            let power = crate::logic::power::cancel_after_movie(app);
            json(rc, 200, &json!({"ok": true, "power": power}), &[])
        }
        "/api/notify/test" => {
            log_info!("action notify/test from {ip}");
            let (code, out) = crate::logic::notify::send_test_notification(app).await;
            json(rc, code, &out, &[])
        }
        "/api/convert/start" => {
            let num = |k: &str| body.get(k).and_then(Value::as_i64).unwrap_or(0);
            let ids = body.get("episodeIds").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
            log_info!("action convert/start series {} x{} episodes {ids} movie {} from {ip}", num("seriesId"), num("count"), num("movieId"));
            let (code, out) = crate::jobs::convert::api_start(app, &body).await;
            json(rc, code, &out, &[])
        }
        "/api/convert/stop" => {
            log_info!("action convert/stop from {ip}");
            let (code, out) = crate::jobs::convert::api_stop(app);
            json(rc, code, &out, &[])
        }
        "/api/subs/translate" | "/api/subs/translate/cancel" => {
            let num = |k: &str| body.get(k).and_then(Value::as_i64).unwrap_or(0);
            let ids = body.get("episodeIds").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
            let season = body.get("season").and_then(Value::as_i64).map_or("-".to_string(), |n| n.to_string());
            let what = if path.ends_with("/cancel") { "subs/translate/cancel" } else { "subs/translate" };
            log_info!("action {what} series {} episodes {ids} season {season} movie {} from {ip}", num("seriesId"), num("movieId"));
            let (code, out) = if path.ends_with("/cancel") {
                crate::jobs::subs_manual::api_cancel(app, &body).await
            } else {
                crate::jobs::subs_manual::api_translate(app, &body).await
            };
            json(rc, code, &out, &[])
        }
        "/api/settings" => {
            let changes: Vec<(&str, bool)> = match body.get("auto_monitors_off") {
                Some(Value::Bool(b)) => vec![("auto_monitors_off", *b)],
                _ => vec![],
            };
            if changes.is_empty() {
                json(rc, 400, &json!({"error": "Setare invalidă."}), &[])
            } else if app.demo {
                app.config.update_settings_in_memory(&changes);
                json(rc, 200, &json!({"ok": true, "settings": app.config.settings()}), &[])
            } else {
                if let Err(e) = app.config.update_settings(&changes) {
                    log_error!("request error from {ip}: cannot save the config ({})", crate::state::io_error_name(&e));
                    return None;
                }
                log_info!("settings changed {} from {ip}", py_repr_bool_map(&changes));
                json(rc, 200, &json!({"ok": true, "settings": app.config.settings()}), &[])
            }
        }
        _ => unknown(rc),
    };
    Some(resp)
}

// ---------------------------------------------------------------------------
// Install listener
// ---------------------------------------------------------------------------

async fn install(r: &Req, ctx: &ConnCtx, rc: &ReqCtx, https_port: u16, ca_cert: &std::path::Path) -> Response<Body> {
    let redirect = |rc: &ReqCtx| {
        let loc = https_url(r.host.as_deref(), https_port, &r.target, &ctx.local_ip);
        send(rc, 301, Vec::new(), TEXT, &[("Location", &loc)])
    };
    if r.method == Method::POST {
        return redirect(rc); // the body is never read (the connection is closed after it)
    }
    match r.path.as_str() {
        "/" => {
            let url = https_url(r.host.as_deref(), https_port, "/", &ctx.local_ip);
            let page = install_page(&ctx.app.config.app_name(), &url);
            let csp = install_csp(&page);
            // the security headers with the install CSP; no HSTS, no session renewal
            let plain = ReqCtx { secure: false, head: rc.head, renew: None };
            send_with_csp(&plain, 200, page, HTML, &[("Cache-Control", "no-cache")], &csp)
        }
        "/ca.crt" => match tokio::fs::read(ca_cert).await.map_err(|_| ()).and_then(|d| cert_der(&d).map_err(|_| ())) {
            Ok(der) => send(
                rc,
                200,
                der,
                CA_CONTENT_TYPE,
                &[("Content-Disposition", "attachment; filename=\"plex-couch-ca.crt\"")],
            ),
            Err(()) => not_found(rc),
        },
        _ => redirect(rc),
    }
}
