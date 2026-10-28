//! Server-level and tooling web tests (pure ones live next to
//! their code: http::security, http::static_files, cli, certs), plus end-to-end checks of
//! the HTTPS listener with certificates from `make-certs`.

mod common;

use common::{temp_dir, Setup, TestServer};
use plex_couch::app::{AppOptions, Overrides};
use plex_couch::auth::session::session_cookie;
use plex_couch::http::server::{self, Role};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// TestCookie
// ---------------------------------------------------------------------------

// py: TestCookie.test_secure_flag_only_over_https
#[test]
fn secure_flag_only_over_https() {
    assert_eq!(session_cookie("pc_session", "tok", 60, true), "pc_session=tok; Max-Age=60; HttpOnly; SameSite=Strict; Path=/; Secure");
    let c = session_cookie("pc_session", "tok", 60, false);
    assert!(!c.contains("Secure"));
    assert!(c.contains("HttpOnly"));
    assert!(c.contains("SameSite=Strict"));
}

// py: TestCookie.test_handlers
#[test]
fn handlers() {
    assert!(!Role::App { secure: false }.secure());
    assert!(Role::App { secure: true }.secure());
}

// ---------------------------------------------------------------------------
// TestImageRoutes
// ---------------------------------------------------------------------------

type Calls = Arc<Mutex<Vec<(String, String)>>>;

fn image_server() -> (TestServer, Calls) {
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let c2 = calls.clone();
    let poster = Arc::new(move |kind: &str, ident: &str| {
        c2.lock().unwrap().push((kind.to_string(), ident.to_string()));
        (ident == "2").then(|| (b"\xff\xd8\xff\xe0jpeg".to_vec(), "image/jpeg".to_string()))
    });
    let srv = TestServer::start(Setup {
        pin: Some("1234"),
        opts: AppOptions { dry_run: true, overrides: Overrides { poster: Some(poster), ..Default::default() }, ..Default::default() },
        ..Default::default()
    });
    (srv, calls)
}

// py: TestImageRoutes.test_requires_login
#[test]
fn image_requires_login() {
    let (srv, calls) = image_server();
    for path in ["/img/movie/2", "/img/plex/1", "/img/plex-art/2", "/img/movie-art/2", "/img/movie/abc"] {
        assert_eq!(srv.req("GET", path).send().status, 401, "{path}");
        assert_eq!(srv.req("GET", path).cookie("123.bad").send().status, 401, "{path}");
    }
    assert!(calls.lock().unwrap().is_empty());
}

// py: TestImageRoutes.test_digits_only_and_cache_headers
#[test]
fn image_digits_only_and_cache_headers() {
    let (srv, calls) = image_server();
    let tok = srv.app.new_session(None);
    let r = srv.req("GET", "/img/movie/2").cookie(&tok).send();
    assert_eq!((r.status, r.header("Content-Type"), r.body.as_slice()), (200, Some("image/jpeg"), &b"\xff\xd8\xff\xe0jpeg"[..]));
    assert_eq!(r.header("Cache-Control"), Some("private, max-age=86400"));
    assert_eq!(r.header("X-Content-Type-Options"), Some("nosniff"));
    for path in ["/img/plex-art/2", "/img/movie-art/2", "/img/plex/2"] {
        let r = srv.req("GET", path).cookie(&tok).send();
        assert_eq!((r.status, r.header("Cache-Control")), (200, Some("private, max-age=86400")), "{path}");
    }
    let last3: Vec<(String, String)> = calls.lock().unwrap().iter().rev().take(3).rev().cloned().collect();
    let want: Vec<(String, String)> =
        [("plex-art", "2"), ("movie-art", "2"), ("plex", "2")].iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    assert_eq!(last3, want);
    calls.lock().unwrap().clear();
    for path in ["/img/movie/abc", "/img/movie/2a", "/img/movie/..%2f1", "/img/plex/%31", "/img/x/1", "/img/movie/",
                 "/img/movie/12345678901", "/img/plex-art/a", "/img/movie-art/"] {
        assert_eq!(srv.req("GET", path).cookie(&tok).send().status, 404, "{path}");
    }
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(srv.req("GET", "/img/movie/7").cookie(&tok).send().status, 404);
    assert_eq!(*calls.lock().unwrap(), vec![("movie".to_string(), "7".to_string())]);
}

// ---------------------------------------------------------------------------
// TestTokens (the generator is plex_couch::tokens, run by `cargo run --example build_tokens`)
// ---------------------------------------------------------------------------

fn py_str(v: &Value) -> String {
    plex_couch::json::py_str(v)
}

fn render_tokens(t: &Value) -> String {
    plex_couch::tokens::render(t)
}

fn load_tokens() -> Value {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/design-system/tokens.json");
    serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
}

// py: TestTokens.test_tokens_css_matches_tokens_json
#[test]
fn tokens_css_matches_tokens_json() {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("static/tokens.css");
    let on_disk = std::fs::read_to_string(p).unwrap().replace("\r\n", "\n"); // universal newlines
    assert_eq!(on_disk, render_tokens(&load_tokens()), "run cargo run --example build_tokens");
}

// py: TestTokens.test_every_token_is_a_variable
#[test]
fn every_token_is_a_variable() {
    let t = load_tokens();
    let css = render_tokens(&t);
    for c in t["color"]["tokens"].as_array().unwrap() {
        assert!(css.contains(&format!("  --{}: {};", c["name"].as_str().unwrap(), py_str(&c["value"]["dark"]))));
    }
    for group in ["spacing", "radius", "shadow", "size"] {
        for x in t[group]["tokens"].as_array().unwrap() {
            assert!(css.contains(&format!("  --{}: {};", x["name"].as_str().unwrap(), py_str(&x["value"]))));
        }
    }
    for s in t["type"]["groups"][0]["styles"].as_array().unwrap() {
        assert!(css.contains(&format!("\n.{} {{\n", s["name"].as_str().unwrap())));
    }
    assert!(css.contains("--font-sans: \"Figtree\""));
}

// ---------------------------------------------------------------------------
// Extra (not in the original suite): response rules the routes rely on
// ---------------------------------------------------------------------------

#[test]
fn static_and_security_headers() {
    let srv = TestServer::start(Setup::default());
    let r = srv.req("GET", "/").send();
    assert_eq!(r.status, 200);
    assert_eq!(r.header("Content-Type"), Some("text/html; charset=utf-8"));
    assert_eq!(r.header("Cache-Control"), Some("no-cache"));
    assert_eq!(r.header("X-Frame-Options"), Some("DENY"));
    assert_eq!(r.header("Referrer-Policy"), Some("no-referrer"));
    assert!(r.header("Content-Security-Policy").unwrap().starts_with("default-src 'self'; img-src 'self' data:"));
    assert_eq!(r.header("Server"), Some("PlexCouch"));
    assert!(r.header("Date").is_some());
    assert!(!r.text().contains("{{APP_NAME}}"));
    let sw = srv.req("GET", "/sw.js").send();
    assert_eq!(sw.header("Service-Worker-Allowed"), Some("/"));
    let nf = srv.req("GET", "/install.html").send();
    assert_eq!((nf.status, nf.text().as_str()), (404, "Not found"));
    // HEAD: same headers, no body
    let h = srv.req("HEAD", "/app.js").send();
    assert_eq!(h.status, 200);
    let g = srv.req("GET", "/app.js").send();
    assert_eq!(h.header("Content-Length"), Some(g.body.len().to_string().as_str()));
    // JSON: no-store, ", " and ": " separators
    let r = srv.req("GET", "/api/status").send();
    assert_eq!(r.status, 401);
    assert_eq!(r.header("Cache-Control"), Some("no-store"));
    assert_eq!(r.text(), "{\"error\": \"Autentificare necesară.\"}");
    // other methods: 405 with the security headers (plan fix 6)
    let r = srv.req("PUT", "/").send();
    assert_eq!(r.status, 405);
    assert_eq!(r.header("X-Content-Type-Options"), Some("nosniff"));
    let r = srv.req("DELETE", "/").host("rebind.evil").send();
    assert_eq!(r.status, 421);
}

#[test]
fn post_rules() {
    let srv = TestServer::start(Setup::default());
    // CSRF before anything, login too
    let r = srv.req("POST", "/api/login").no_csrf().json(json!({"pin": "1"})).send();
    assert_eq!((r.status, r.json()), (403, json!({"error": "Cerere respinsă (CSRF)."})));
    // wrong PIN
    let r = srv.login("000000");
    assert_eq!((r.status, r.json()), (401, json!({"error": "PIN greșit.", "attempts_left": 4})));
    // a non-string PIN still uses up an attempt
    let r = srv.req("POST", "/api/login").json(json!({"pin": 1234})).send();
    assert_eq!(r.json()["attempts_left"], json!(3));
    // a body over 4096 bytes: 400 and the connection is closed (plan fix 5)
    let big = json!({"pin": "x".repeat(5000)});
    let r = srv.req("POST", "/api/login").json(big).send();
    assert_eq!((r.status, r.json()), (400, json!({"error": "Cerere invalidă."})));
    assert_eq!(r.header("Connection"), Some("close"));
    // invalid JSON
    let r = common::raw(srv.port, b"POST /api/login HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Requested-With: plex-couch\r\nContent-Length: 5\r\nConnection: close\r\n\r\n{nope");
    assert_eq!(r.status, 400);
    // settings: auth, then validation
    let tok = srv.app.new_session(None);
    let r = srv.req("POST", "/api/settings").cookie(&tok).json(json!({"auto_monitors_off": "yes"})).send();
    assert_eq!((r.status, r.json()), (400, json!({"error": "Setare invalidă."})));
    let r = srv.req("POST", "/api/settings").cookie(&tok).json(json!({"auto_monitors_off": false})).send();
    assert_eq!((r.status, r.json()), (200, json!({"ok": true, "settings": {"auto_monitors_off": false}})));
    let file: Value = serde_json::from_slice(&std::fs::read(srv.app.config.path()).unwrap()).unwrap();
    assert_eq!(file["settings"]["auto_monitors_off"], json!(false));
    let r = srv.req("POST", "/api/nope").cookie(&tok).send();
    assert_eq!((r.status, r.json()), (404, json!({"error": "Necunoscut."})));
    let r = srv.req("GET", "/api/nope").cookie(&tok).send();
    assert_eq!((r.status, r.json()), (404, json!({"error": "Necunoscut."})));
    let r = srv.req("POST", "/api/logout").cookie(&tok).send();
    assert_eq!(r.header("Set-Cookie"), Some("pc_session=; Max-Age=0; HttpOnly; SameSite=Strict; Path=/"));
}

#[test]
fn no_pin_and_demo_login() {
    let srv = TestServer::start(Setup { pin: None, ..Default::default() });
    let r = srv.login("1234");
    assert_eq!((r.status, r.json()), (503, json!({"error": "PIN-ul nu este setat. Rulează plex-couch --set-pin."})));
    let demo = TestServer::start(Setup { pin: None, opts: AppOptions { demo: true, ..Default::default() }, ..Default::default() });
    let r = demo.login("whatever");
    assert_eq!((r.status, r.json()), (200, json!({"ok": true})));
    assert!(r.header("Set-Cookie").is_none());
    assert_eq!(demo.req("GET", "/api/status").send().status, 200);
    let r = demo.req("POST", "/api/settings").json(json!({"auto_monitors_off": false})).send();
    assert_eq!(r.json()["settings"], json!({"auto_monitors_off": false}));
    assert!(!demo.app.config.path().exists() || {
        let f: Value = serde_json::from_slice(&std::fs::read(demo.app.config.path()).unwrap()).unwrap();
        f.get("settings").is_none()
    });
}

/// An app whose services all sit on a closed port (keys set inline, nothing real is read).
fn closed_services_server(demo: bool) -> TestServer {
    let dir = temp_dir("conv");
    let base = format!("http://127.0.0.1:{}", common::free_port());
    let cfg = json!({"services": {
        "plex": {"url": base, "token": "t"}, "radarr": {"url": base, "api_key": "k"}, "sonarr": {"url": base, "api_key": "k"},
        "bazarr": {"url": base, "api_key": "k"}, "prowlarr": {"url": base}, "qbittorrent": {"url": base}}});
    std::fs::write(dir.join("cfg.json"), cfg.to_string()).unwrap();
    let opts = AppOptions { dry_run: true, demo, ..Default::default() };
    TestServer::start(Setup { dir: Some(dir), opts, ..Default::default() })
}

#[test]
fn convert_routes() {
    let srv = closed_services_server(false);
    // login and CSRF like every other route
    assert_eq!(srv.req("GET", "/api/convert").send().status, 401);
    let start = json!({"seriesId": 4, "count": 2});
    assert_eq!(srv.req("POST", "/api/convert/start").json(start.clone()).send().status, 401);
    assert_eq!(srv.req("POST", "/api/convert/stop").send().status, 401);
    let tok = srv.app.new_session(None);
    let r = srv.req("POST", "/api/convert/start").cookie(&tok).no_csrf().json(start.clone()).send();
    assert_eq!((r.status, r.json()), (403, json!({"error": "Cerere respinsă (CSRF)."})));
    assert_eq!(srv.req("POST", "/api/convert/stop").cookie(&tok).no_csrf().send().status, 403);
    // validation before any upstream call
    for body in [json!({}), json!({"seriesId": 4, "count": 0}), json!({"seriesId": 4, "count": 21}), json!({"seriesId": "x", "count": 2})] {
        let r = srv.req("POST", "/api/convert/start").cookie(&tok).json(body).send();
        assert_eq!((r.status, r.json()), (400, json!({"error": "Cerere invalidă."})));
    }
    for body in [json!({"seriesId": 4, "episodeIds": []}), json!({"seriesId": 4, "episodeIds": [1, 1]}), json!({"movieId": "7"}),
                 json!({"movieId": 7, "seriesId": 4}), json!({"seriesId": 4, "count": 2, "episodeIds": [3]})] {
        let r = srv.req("POST", "/api/convert/start").cookie(&tok).json(body.clone()).send();
        assert_eq!((r.status, r.json()), (400, json!({"error": "Cerere invalidă."})), "{body}");
    }
    // Sonarr / Radarr down: a message, never a crash
    let r = srv.req("POST", "/api/convert/start").cookie(&tok).json(start).send();
    assert_eq!((r.status, r.json()), (502, json!({"error": "Sonarr nu răspunde."})));
    let r = srv.req("POST", "/api/convert/start").cookie(&tok).json(json!({"seriesId": 4, "episodeIds": [11]})).send();
    assert_eq!(r.status, 502);
    let r = srv.req("POST", "/api/convert/start").cookie(&tok).json(json!({"movieId": 7})).send();
    assert_eq!((r.status, r.json()), (502, json!({"error": "Radarr nu răspunde."})));
    let r = srv.req("GET", "/api/convert").cookie(&tok).send();
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!((v["job"].clone(), v["available"].clone()), (Value::Null, json!(true)));
    assert_eq!((v["max"].clone(), v["default"].clone()), (json!(20), json!(2)));
    let r = srv.req("POST", "/api/convert/stop").cookie(&tok).send();
    assert_eq!((r.status, r.json()), (409, json!({"error": "Nicio conversie nu rulează."})));
    // the page has the Seriale tab, the PC tab no longer has the conversion section
    let html = srv.req("GET", "/").send().text();
    assert!(html.contains("data-tab=\"seriale\"") && html.contains("id=\"view-seriale\""));
    assert!(!html.contains("id=\"h-convert\""));
}

#[test]
fn series_routes() {
    let srv = closed_services_server(false);
    // login like every other route
    for path in ["/api/series", "/api/series/4", "/api/movies/7", "/img/series/4", "/img/series-art/4"] {
        assert_eq!(srv.req("GET", path).send().status, 401, "{path}");
    }
    let tok = srv.app.new_session(None);
    // ids: 1-10 ASCII digits, never 0
    for path in ["/api/series/", "/api/series/x", "/api/series/0", "/api/series/4/", "/api/series/12345678901", "/api/series/-1",
                 "/api/movies/abc", "/api/movies/1.5", "/api/series/%34"] {
        let r = srv.req("GET", path).cookie(&tok).send();
        assert_eq!((r.status, r.json()), (404, json!({"error": "Necunoscut."})), "{path}");
    }
    // Sonarr / Radarr down: a message
    let v = srv.req("GET", "/api/series").cookie(&tok).send().json();
    assert_eq!((v["series"].clone(), v["error"].clone(), v["demo"].clone()), (Value::Null, json!("Sonarr nu răspunde."), json!(false)));
    let r = srv.req("GET", "/api/series/4").cookie(&tok).send();
    assert_eq!((r.status, r.json()), (502, json!({"error": "Sonarr nu răspunde."})));
    let r = srv.req("GET", "/api/movies/7").cookie(&tok).send();
    assert_eq!((r.status, r.json()), (502, json!({"error": "Radarr nu răspunde."})));
    // a GET never starts anything
    assert!(!srv.app.convert.running());
}

#[test]
fn subs_routes() {
    let srv = closed_services_server(false);
    let ep = json!({"seriesId": 4, "episodeIds": [11]});
    // login and CSRF like every other route
    assert_eq!(srv.req("GET", "/api/subs").send().status, 401);
    for path in ["/api/subs/translate", "/api/subs/translate/cancel"] {
        assert_eq!(srv.req("POST", path).json(ep.clone()).send().status, 401, "{path}");
    }
    let tok = srv.app.new_session(None);
    for path in ["/api/subs/translate", "/api/subs/translate/cancel"] {
        let r = srv.req("POST", path).cookie(&tok).no_csrf().json(ep.clone()).send();
        assert_eq!((r.status, r.json()), (403, json!({"error": "Cerere respinsă (CSRF)."})), "{path}");
    }
    // validation before any upstream call
    for body in [json!({}), json!({"seriesId": 4}), json!({"seriesId": 4, "episodeIds": []}), json!({"seriesId": 4, "episodeIds": [1, 1]}),
                 json!({"seriesId": 4, "season": 1}), json!({"seriesId": 4, "season": 1, "missingOnly": false}), json!({"movieId": "7"}),
                 json!({"movieId": 7, "seriesId": 4}), json!({"seriesId": 4, "season": 1, "missingOnly": true, "episodeIds": [3]})] {
        for path in ["/api/subs/translate", "/api/subs/translate/cancel"] {
            let r = srv.req("POST", path).cookie(&tok).json(body.clone()).send();
            assert_eq!((r.status, r.json()), (400, json!({"error": "Cerere invalidă."})), "{path} {body}");
        }
    }
    // Bazarr down: a message, never a crash; nothing queued
    let r = srv.req("POST", "/api/subs/translate").cookie(&tok).json(ep.clone()).send();
    assert_eq!((r.status, r.json()), (502, json!({"error": "Bazarr nu răspunde."})));
    let r = srv.req("POST", "/api/subs/translate/cancel").cookie(&tok).json(ep).send();
    assert_eq!((r.status, r.json()["removed"].clone()), (200, json!(0)));
    let v = srv.req("GET", "/api/subs").cookie(&tok).send().json();
    assert_eq!((v["translator"].clone(), v["queue_len"].clone(), v["per_day"].clone(), v["auto"].clone()), (Value::Null, json!(0), json!(40), json!(false)));
    assert!(!srv.app.subs.has_queue());
}

#[test]
fn subs_in_demo() {
    let demo = closed_services_server(true);
    let v = demo.req("GET", "/api/subs").send().json();
    assert_eq!((v["demo"].clone(), v["translator"].clone(), v["queue_len"].clone()), (json!(true), json!(true), json!(2)));
    let d = demo.req("GET", "/api/series/1").send().json();
    let s1 = &d["seasons"][0]["episodes"];
    assert_eq!((s1[5]["ro"].clone(), s1[6]["ro"].clone(), s1[7]["ro_pos"].clone()), (json!("translating"), json!("queued"), json!(2)));
    assert_eq!(d["subs"]["queue_len"], json!(2));
    let r = demo.req("POST", "/api/subs/translate").json(json!({"seriesId": 1, "season": 1, "missingOnly": true})).send();
    assert_eq!((r.status, r.json()["queued"].clone()), (200, json!(4)));
    let r = demo.req("POST", "/api/subs/translate/cancel").json(json!({"seriesId": 1, "episodeIds": [10107]})).send();
    assert_eq!((r.status, r.json()["removed"].clone()), (200, json!(1)));
    let m = demo.req("GET", "/api/movies/4").send().json();
    assert_eq!((m["ro"].clone(), m["subs"]["translator"].clone()), (json!("missing"), json!(true)));
    let r = demo.req("POST", "/api/subs/translate").json(json!({"movieId": 4})).send();
    assert_eq!((r.status, r.json()["message"].clone()), (200, json!("Filmul e în coadă pentru traducere.")));
    assert_eq!(demo.req("GET", "/api/movies/4").send().json()["ro"], json!("queued"));
}

#[test]
fn convert_in_demo() {
    let demo = closed_services_server(true);
    let v = demo.req("GET", "/api/convert").send().json();
    assert_eq!((v["demo"].clone(), v["job"]["running"].clone()), (json!(true), json!(true)));
    let r = demo.req("POST", "/api/convert/start").json(json!({"seriesId": 1, "count": 2})).send();
    assert_eq!((r.status, r.json()["demo"].clone()), (200, json!(true)));
    let r = demo.req("POST", "/api/convert/start").json(json!({"movieId": 1})).send();
    assert_eq!((r.status, r.json()["demo"].clone()), (200, json!(true)));
    let r = demo.req("POST", "/api/convert/start").json(json!({"seriesId": 1, "episodeIds": [10107]})).send();
    assert_eq!(r.status, 200);
    assert!(!demo.app.convert.running());
    assert_eq!(demo.req("POST", "/api/convert/stop").send().status, 200);
    // the Seriale grid, a series screen with the running batch, a movie sheet
    let g = demo.req("GET", "/api/series").send().json();
    assert_eq!(g["series"].as_array().unwrap().len(), 5);
    let d = demo.req("GET", "/api/series/1").send().json();
    assert_eq!((d["title"].clone(), d["job"]["series_id"].clone()), (json!("Frieren: Beyond Journey's End"), json!(1)));
    assert!(d["seasons"][0]["episodes"][5]["converting"]["percent"].is_number());
    assert_eq!(demo.req("GET", "/api/series/77").send().status, 404);
    let m = demo.req("GET", "/api/movies/1").send().json();
    assert_eq!(m["can_convert"], json!(true));
    let r = demo.req("GET", "/img/series/1").send();
    assert_eq!((r.status, r.header("Content-Type")), (200, Some("image/svg+xml")));
}

#[test]
fn install_page_redirects_and_ca() {
    let d = temp_dir("inst");
    let ca = d.join("ca.crt");
    std::fs::write(&ca, "-----BEGIN CERTIFICATE-----\nMAMCAQA=\n-----END CERTIFICATE-----\n").unwrap();
    let srv = TestServer::start(Setup { role: Role::Install { https_port: 8766, ca_cert: ca.clone() }, ..Default::default() });
    let r = srv.req("GET", "/").host("192.168.1.50:8765").send();
    assert_eq!(r.status, 200);
    assert!(r.text().contains("https://192.168.1.50:8766/"));
    let csp = r.header("Content-Security-Policy").unwrap();
    assert!(csp.starts_with("default-src 'none'; style-src 'sha256-"), "{csp}");
    assert_eq!(r.header("Cache-Control"), Some("no-cache"));
    let r = srv.req("GET", "/foo?x=1").host("PlexPC.local:8765").send();
    assert_eq!((r.status, r.header("Location")), (301, Some("https://plexpc.local:8766/foo?x=1")));
    let r = srv.req("POST", "/api/login").host("10.0.0.2").json(json!({"pin": "1"})).send();
    assert_eq!((r.status, r.header("Location")), (301, Some("https://10.0.0.2:8766/api/login")));
    let r = srv.req("GET", "//evil.com/x").host("10.0.0.2").send();
    assert_eq!(r.header("Location"), Some("https://10.0.0.2:8766/evil.com/x"));
    let r = srv.req("GET", "/ca.crt").send();
    assert_eq!((r.status, r.header("Content-Type")), (200, Some("application/x-x509-ca-cert")));
    assert_eq!(r.header("Content-Disposition"), Some("attachment; filename=\"plex-couch-ca.crt\""));
    assert_eq!(r.body, vec![0x30, 0x03, 0x02, 0x01, 0x00]);
    std::fs::remove_file(&ca).unwrap();
    assert_eq!(srv.req("GET", "/ca.crt").send().status, 404);
    let _ = std::fs::remove_dir_all(&d);
}

/// `make-certs` output served by the HTTPS listener, verified by a rustls client that
/// trusts only ca.pem (chain, name constraints, EKU and SAN checked by webpki). Also checks
/// that a re-run keeps the CA and that the new server certificate still chains to it.
#[test]
fn https_with_make_certs_output() {
    let d = temp_dir("certs");
    let out = d.join("certs");
    let ips = vec!["127.0.0.1".to_string()];
    let dns = vec!["localhost".to_string()];
    let first = plex_couch::certs::make_all(&out, &ips, &dns, false, (2048, 2048)).unwrap();
    assert!(first.created_ca);
    let ca_before = std::fs::read(out.join("ca.pem")).unwrap();
    let again = plex_couch::certs::make_all(&out, &ips, &dns, false, (2048, 2048)).unwrap();
    assert!(!again.created_ca);
    assert_eq!(std::fs::read(out.join("ca.pem")).unwrap(), ca_before);
    for f in ["ca.key", "ca.pem", "ca.crt", "server.key", "server.pem"] {
        assert!(out.join(f).is_file(), "{f}");
    }
    assert!(std::fs::read_to_string(out.join("server.key")).unwrap().starts_with("-----BEGIN PRIVATE KEY-----"));

    // the certificate as x509 sees it
    let pem = std::fs::read(out.join("ca.pem")).unwrap();
    let der = plex_couch::http::static_files::cert_der(&pem).unwrap();
    assert_eq!(std::fs::read(out.join("ca.crt")).unwrap(), der);
    let (_, ca) = x509_parser::parse_x509_certificate(&der).unwrap();
    assert_eq!(ca.subject().to_string(), "CN=Plex Couch Local CA");
    let bc = ca.basic_constraints().unwrap().unwrap();
    assert!(bc.critical && bc.value.ca && bc.value.path_len_constraint == Some(0));
    let nc = ca.name_constraints().unwrap().unwrap();
    assert!(nc.critical);
    let srv_pem = std::fs::read(out.join("server.pem")).unwrap();
    let srv_der = plex_couch::http::static_files::cert_der(&srv_pem).unwrap();
    let (_, sc) = x509_parser::parse_x509_certificate(&srv_der).unwrap();
    assert_eq!(sc.subject().to_string(), "CN=Plex Couch");
    let days = (sc.validity().not_after.timestamp() - sc.validity().not_before.timestamp()) / 86400;
    assert_eq!(days, 397);

    let acceptor = server::load_tls(&out.join("server.pem").to_string_lossy(), &out.join("server.key").to_string_lossy())
        .unwrap();
    let srv = TestServer::start(Setup { role: Role::App { secure: true }, tls: Some(acceptor), ..Default::default() });

    let rt = common::runtime();
    let (status_line, hsts) = rt.block_on(async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio_rustls::rustls;
        let mut roots = rustls::RootCertStore::empty();
        roots.add(rustls::pki_types::CertificateDer::from(der.clone())).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cfg = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let conn = tokio_rustls::TlsConnector::from(Arc::new(cfg));
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", srv.port)).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap();
        let mut tls = conn.connect(name, tcp).await.expect("the chain verifies");
        tls.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").await.unwrap();
        let mut raw = Vec::new();
        let _ = tls.read_to_end(&mut raw).await;
        let r = common::Resp::parse(&raw, false);
        (r.status, r.header("Strict-Transport-Security").map(String::from))
    });
    assert_eq!(status_line, 200);
    assert_eq!(hsts.as_deref(), Some("max-age=31536000"));
    rt.shutdown_background();
    drop(srv);
    let _ = std::fs::remove_dir_all(&d);
}
