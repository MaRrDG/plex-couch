//! Ports of the server-level cases of tests/test_security.py (the pure ones live next to
//! their code: auth::guard, http::security, system::power, logic::power, logic::status,
//! services, certs). Each test carries `// py: <Class>.<test>`.

mod common;

use common::{free_port, temp_dir, Request, Setup, TestServer, DAY, PIN};
use plex_couch::app::{AppOptions, Overrides, LOGIN_CONCURRENCY};
use plex_couch::auth::session::{now, session_expiry, sig};
use plex_couch::http::server::{self, Role, CONNECTION_TIMEOUT, MAX_CONNECTIONS};
use plex_couch::system::power::CancelOutcome;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn ok_status() -> Overrides {
    Overrides {
        status: Some(Arc::new(|| json!({"ok": true}))),
        poster: Some(Arc::new(|_k: &str, _i: &str| Some((b"img".to_vec(), "image/jpeg".to_string())))),
        ..Default::default()
    }
}

fn app_with(overrides: Overrides) -> TestServer {
    TestServer::start(Setup { opts: AppOptions { dry_run: true, overrides, ..Default::default() }, ..Default::default() })
}

// ---------------------------------------------------------------------------
// TestLoginBurst
// ---------------------------------------------------------------------------

struct Counting {
    checks: Arc<Mutex<Vec<String>>>,
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
}

fn counting_server() -> (TestServer, Counting) {
    let c = Counting {
        checks: Arc::new(Mutex::new(Vec::new())),
        active: Arc::new(AtomicUsize::new(0)),
        max_active: Arc::new(AtomicUsize::new(0)),
    };
    let (checks, active, max_active) = (c.checks.clone(), c.active.clone(), c.max_active.clone());
    let verify = Arc::new(move |pin: &str, stored: &str| {
        checks.lock().unwrap().push(pin.to_string());
        let n = active.fetch_add(1, Ordering::SeqCst) + 1;
        max_active.fetch_max(n, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(20));
        let r = plex_couch::auth::pin::verify_pin(pin, stored);
        active.fetch_sub(1, Ordering::SeqCst);
        r
    });
    let srv = app_with(Overrides { verify_pin: Some(verify), ..Default::default() });
    (srv, c)
}

// py: TestLoginBurst.test_parallel_burst_checks_at_most_five_pins_and_hides_the_right_one
#[test]
fn parallel_burst_checks_at_most_five_pins_and_hides_the_right_one() {
    let (srv, c) = counting_server();
    let n = 40;
    let mut socks: Vec<TcpStream> = (0..n).map(|_| TcpStream::connect(("127.0.0.1", srv.port)).unwrap()).collect();
    let reqs: Vec<Vec<u8>> = (0..n)
        .map(|i| {
            let pin = if i == n - 1 { PIN.to_string() } else { format!("{:06}", 100000 + i) };
            let body = serde_json::to_vec(&json!({"pin": pin})).unwrap();
            let mut r = format!(
                "POST /api/login HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Requested-With: plex-couch\r\n\
                 Content-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .into_bytes();
            r.extend(body);
            r
        })
        .collect();
    for (s, r) in socks.iter_mut().zip(&reqs) {
        s.write_all(&r[..r.len() - 1]).unwrap();
    }
    std::thread::sleep(Duration::from_millis(300));
    for (s, r) in socks.iter_mut().zip(&reqs) {
        s.write_all(&r[r.len() - 1..]).unwrap();
    }
    let mut codes = Vec::new();
    for mut s in socks {
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut data = Vec::new();
        let _ = s.read_to_end(&mut data);
        let t = String::from_utf8_lossy(&data).to_string();
        codes.push(t.split(' ').nth(1).unwrap_or("").to_string());
    }
    let checks = c.checks.lock().unwrap().clone();
    assert!(checks.len() <= 5, "{} checks", checks.len());
    assert_eq!(c.max_active.load(Ordering::SeqCst), 1); // one PBKDF2 at a time
    assert!(!codes.contains(&"200".to_string()));
    assert!(!checks.contains(&PIN.to_string()));
    assert_eq!(codes.iter().filter(|c| *c == "401" || *c == "429").count(), n);
    // whatever is left of the 5 attempts, then the IP is locked: even the right PIN is refused
    let used = checks.len();
    for _ in 0..(5 - used) {
        srv.login("000000");
    }
    assert_eq!(c.checks.lock().unwrap().len(), 5);
    let r = srv.login(PIN);
    assert_eq!(r.status, 429);
    assert!(r.json().get("lockout_seconds").is_some());
    assert_eq!(c.checks.lock().unwrap().len(), 5);
}

// py: TestLoginBurst.test_login_slots_refuse_beyond_the_cap
#[test]
fn login_slots_refuse_beyond_the_cap() {
    let (srv, c) = counting_server();
    {
        let held = srv.app.login_slots.try_acquire_many(LOGIN_CONCURRENCY as u32).unwrap();
        let r = srv.login(PIN);
        assert_eq!(r.status, 429);
        assert!(c.checks.lock().unwrap().is_empty());
        assert_eq!(srv.app.guard.attempts_left("127.0.0.1"), 5); // refunded
        drop(held);
    }
    assert_eq!(srv.login(PIN).status, 200);
}

// py: TestLoginBurst.test_success_remembers_the_ip_in_the_state_file
#[test]
fn success_remembers_the_ip_in_the_state_file() {
    let (srv, _c) = counting_server();
    assert_eq!(srv.login(PIN).status, 200);
    let state = srv.app.state_file.load();
    assert!(state["known_ips"].get("127.0.0.1").is_some());
    // the notification checks keep it (they only write their own keys)
    let mut m = serde_json::Map::new();
    m.insert("movie_ready".into(), json!({"primed": true}));
    srv.app.state_file.update(m);
    assert!(srv.app.state_file.load()["known_ips"].get("127.0.0.1").is_some());
    // and a restart loads it
    let again = plex_couch::app::AppState::new(srv.app.config.clone(), AppOptions { dry_run: true, ..Default::default() }).unwrap();
    assert!(again.guard.is_known("127.0.0.1", None));
}

// ---------------------------------------------------------------------------
// TestSessions
// ---------------------------------------------------------------------------

fn login_cookie(srv: &TestServer) -> String {
    let r = srv.login(PIN);
    assert_eq!(r.status, 200);
    assert!(r.header("Set-Cookie").unwrap().contains(&format!("Max-Age={}", 30 * 86400)));
    r.cookie_value().unwrap()
}

// py: TestSessions.test_logout_all_invalidates_every_token
#[test]
fn logout_all_invalidates_every_token() {
    let srv = app_with(ok_status());
    let (a, b) = (login_cookie(&srv), login_cookie(&srv));
    assert_eq!(srv.req("GET", "/api/status").cookie(&a).send().status, 200);
    assert_eq!(srv.req("POST", "/api/logout-all").cookie(&a).no_csrf().send().status, 403); // CSRF
    assert_eq!(srv.req("POST", "/api/logout-all").send().status, 401); // auth
    let r = srv.req("POST", "/api/logout-all").cookie(&a).send();
    assert_eq!(r.status, 200);
    assert!(r.header("Set-Cookie").unwrap().contains("Max-Age=0"));
    assert_eq!(srv.req("GET", "/api/status").cookie(&a).send().status, 401);
    assert_eq!(srv.req("GET", "/api/status").cookie(&b).send().status, 401);
    assert_eq!(srv.app.config.session_epoch(), 2);
    let file: Value = serde_json::from_slice(&std::fs::read(srv.app.config.path()).unwrap()).unwrap();
    assert_eq!(file["session_epoch"], json!(2));
    let c = login_cookie(&srv);
    assert!(c.starts_with("2."));
    assert_eq!(srv.req("GET", "/api/status").cookie(&c).send().status, 200);
}

// py: TestSessions.test_logout_keeps_other_devices
#[test]
fn logout_keeps_other_devices() {
    let srv = app_with(ok_status());
    let (a, b) = (login_cookie(&srv), login_cookie(&srv));
    assert_eq!(srv.req("POST", "/api/logout").cookie(&a).send().status, 200);
    assert_eq!(srv.req("GET", "/api/status").cookie(&b).send().status, 200);
}

// py: TestSessions.test_sliding_renewal
#[test]
fn sliding_renewal() {
    let srv = app_with(ok_status());
    let fresh = srv.app.new_session(None);
    let r = srv.req("GET", "/api/status").cookie(&fresh).send();
    assert_eq!(r.status, 200);
    assert!(r.header("Set-Cookie").is_none());
    let old = srv.app.new_session(Some(now() - 16.0 * DAY)); // 14 days left
    let r = srv.req("GET", "/api/status").cookie(&old).send();
    assert_eq!(r.status, 200);
    let renewed = r.cookie_value().unwrap();
    assert!(r.header("Set-Cookie").unwrap().contains("HttpOnly"));
    let exp = session_expiry(&srv.app.session_key(), Some(&renewed), now(), 1).unwrap();
    assert!(exp as f64 > now() + 29.0 * DAY);
    // POST actions renew too; poster images do not (they are cached)
    let r = srv.req("POST", "/api/monitors/on").cookie(&old).send();
    assert_eq!(r.status, 200);
    assert!(r.cookie_value().is_some());
    let r = srv.req("GET", "/img/movie/1").cookie(&old).send();
    assert_eq!(r.status, 200);
    assert!(r.header("Set-Cookie").is_none());
}

// py: TestSessions.test_expired_after_30_days
#[test]
fn expired_after_30_days() {
    let srv = app_with(ok_status());
    let old = srv.app.new_session(Some(now() - 30.0 * DAY - 5.0));
    assert_eq!(srv.req("GET", "/api/status").cookie(&old).send().status, 401);
    let (ok, fresh) = srv.app.check_session(Some(&srv.app.new_session(Some(now() - 29.0 * DAY))), None);
    assert!(ok);
    assert!(fresh.is_some());
}

// py: TestSessions.test_ui_has_the_logout_all_action
#[test]
fn ui_has_the_logout_all_action() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("static");
    let html = std::fs::read_to_string(root.join("index.html")).unwrap();
    let js = std::fs::read_to_string(root.join("app.js")).unwrap();
    assert!(html.contains(
        "<button type=\"button\" class=\"btn-text body-strong\" id=\"logout-all\">Ieși de pe toate dispozitivele</button>"
    ));
    assert!(html.find("id=\"logout\"").unwrap() < html.find("id=\"logout-all\"").unwrap());
    assert!(js.contains("'/api/logout-all'"));
    assert!(js.contains("Ai ieșit de pe toate dispozitivele."));
}

// py: TestSessions.test_old_two_part_tokens_are_rejected
#[test]
fn old_two_part_tokens_are_rejected() {
    let srv = app_with(ok_status());
    let key = srv.app.session_key();
    let exp = (now() as i64 + 100 * 86400).to_string();
    let old_style = format!("{exp}.{}", sig(&key, &exp));
    assert!(!srv.app.session_ok(Some(&old_style)));
}

// ---------------------------------------------------------------------------
// Host allowlist over HTTP
// ---------------------------------------------------------------------------

// py: TestHostOverHttp.test_rebind_host_refused_everywhere
#[test]
fn rebind_host_refused_everywhere() {
    let srv = TestServer::start(Setup::default());
    for (method, path) in [("GET", "/"), ("GET", "/api/status"), ("POST", "/api/login"), ("GET", "/app.js")] {
        let mut rq = srv.req(method, path).host("rebind.evil");
        if method == "POST" {
            rq = rq.json(json!({"pin": PIN}));
        }
        let r = rq.send();
        assert_eq!(r.status, 421, "{path}");
        assert_eq!(r.text(), "Gazdă nepermisă.");
        assert!(r.header("Content-Type").unwrap().starts_with("text/plain"));
    }
}

// py: TestHostOverHttp.test_missing_host_refused
#[test]
fn missing_host_refused() {
    let srv = TestServer::start(Setup::default());
    assert_eq!(srv.req("GET", "/").host("").send().status, 421);
}

// py: TestHostOverHttp.test_allowed_hosts
#[test]
fn allowed_hosts() {
    let srv = TestServer::start(Setup::default());
    assert_eq!(srv.req("GET", "/").host(&format!("127.0.0.1:{}", srv.port)).send().status, 200);
    assert_eq!(srv.req("GET", "/").host("localhost").send().status, 200);
    assert_eq!(srv.req("GET", "/").host("plexpc.local").send().status, 200);
    assert_eq!(srv.req("GET", "/").host("plexpc.lan").send().status, 421);
    srv.app.config.set_in_memory("allowed_hosts", json!(["plexpc.lan"]));
    assert_eq!(srv.req("GET", "/").host("plexpc.lan").send().status, 200);
}

// py: TestHostOverHttp.test_no_hsts_on_plain_http
#[test]
fn no_hsts_on_plain_http() {
    let srv = TestServer::start(Setup::default());
    assert!(srv.req("GET", "/").send().header("Strict-Transport-Security").is_none());
}

// py: TestInstallHandlerHost.test_install_listener_checks_host_and_has_no_hsts
#[test]
fn install_listener_checks_host_and_has_no_hsts() {
    let srv = TestServer::start(Setup {
        role: Role::Install { https_port: 8766, ca_cert: "".into() },
        ..Default::default()
    });
    assert_eq!(srv.req("GET", "/").host("rebind.evil").send().status, 421);
    assert_eq!(srv.req("GET", "/api/status").host("rebind.evil").send().status, 421);
    let r = srv.req("GET", "/").send();
    assert_eq!(r.status, 200);
    assert!(r.header("Strict-Transport-Security").is_none());
    let r = srv.req("GET", "/api/status").send();
    assert_eq!(r.status, 301);
    assert!(r.header("Strict-Transport-Security").is_none());
}

// py: TestHstsOnSecureHandler.test_hsts_on_every_response
#[test]
fn hsts_on_every_response() {
    // the HTTPS listener's handler (TLS itself does not matter here)
    let srv = TestServer::start(Setup { role: Role::App { secure: true }, ..Default::default() });
    for path in ["/", "/api/status", "/nope"] {
        let r = srv.req("GET", path).send();
        assert_eq!(r.header("Strict-Transport-Security"), Some("max-age=31536000"), "{path}");
    }
    let r = srv.req("GET", "/").host("rebind.evil").send();
    assert_eq!(r.header("Strict-Transport-Security"), Some("max-age=31536000"));
}

// ---------------------------------------------------------------------------
// HTTPS required
// ---------------------------------------------------------------------------

// py: TestNoCertHandler.test_only_a_503_page
#[test]
fn only_a_503_page() {
    let srv = TestServer::start(Setup { role: Role::NoCert, ..Default::default() });
    for (method, path) in [("GET", "/"), ("GET", "/api/status"), ("POST", "/api/login"), ("GET", "/app.js"), ("GET", "/ca.crt"), ("HEAD", "/")] {
        let mut rq = srv.req(method, path);
        if method == "POST" {
            rq = rq.json(json!({"pin": PIN}));
        }
        let r = rq.send();
        assert_eq!(r.status, 503, "{path}");
        assert!(r.header("Content-Type").unwrap().starts_with("text/html"));
        if method != "HEAD" {
            assert!(r.text().contains("make_certs.py"));
            assert!(r.text().contains("Certificatele HTTPS lipsesc"));
        }
    }
    assert_eq!(srv.req("GET", "/").host("rebind.evil").send().status, 421);
}

// py: TestLoadTls.test_missing_and_unreadable
#[test]
fn load_tls_missing_and_unreadable() {
    let d = temp_dir("tls");
    let (cert, key) = (d.join("server.pem"), d.join("server.key"));
    let (c, k) = (cert.to_string_lossy().into_owned(), key.to_string_lossy().into_owned());
    assert_eq!(server::load_tls(&c, &k).err().as_deref(), Some("missing"));
    for p in [&cert, &key] {
        std::fs::write(p, "not a certificate").unwrap();
    }
    let problem = server::load_tls(&c, &k).err().unwrap();
    assert!(problem.starts_with("unreadable"), "{problem}");
    assert_eq!(server::load_tls("", "").err().as_deref(), Some("missing"));
    let _ = std::fs::remove_dir_all(&d);
}

// ---------------------------------------------------------------------------
// TestStartupWithoutCertificates: the real exe in a child process
// ---------------------------------------------------------------------------

struct Child {
    proc: std::process::Child,
    port: u16,
    dir: std::path::PathBuf,
}

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.proc.kill();
        let _ = self.proc.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start_child(extra: &[&str]) -> Child {
    let dir = temp_dir("child");
    let cfg = dir.join("cfg.json");
    // services on a closed port: the child never talks to a real Plex/Radarr
    let closed = "http://127.0.0.1:9";
    let services: serde_json::Map<String, Value> = ["plex", "radarr", "sonarr", "bazarr", "prowlarr", "qbittorrent"]
        .iter()
        .map(|n| (n.to_string(), json!({"url": closed})))
        .collect();
    let body = json!({"pin_hash": plex_couch::auth::pin::hash_pin(PIN, 1000), "secret": "s".repeat(64),
                      "notifications": {"topic": null}, "services": services});
    std::fs::write(&cfg, serde_json::to_vec(&body).unwrap()).unwrap();
    let (port, https_port) = (free_port(), free_port());
    let mut args: Vec<String> = ["--host", "127.0.0.1", "--port", &port.to_string(), "--https-port", &https_port.to_string(),
                                 "--config", &cfg.to_string_lossy(), "--dry-run"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    args.extend(extra.iter().map(|s| s.to_string()));
    let proc = std::process::Command::new(env!("CARGO_BIN_EXE_plex-couch"))
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let child = Child { proc, port, dir };
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(500)).is_ok() {
            return child;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("plex-couch did not start");
}

// py: TestStartupWithoutCertificates.test_missing_certificates_serve_only_503
#[test]
fn missing_certificates_serve_only_503() {
    let c = start_child(&[]);
    let r = Request::new(c.port, "GET", "/").send();
    assert_eq!(r.status, 503);
    assert!(r.text().contains("make_certs.py"));
    assert_eq!(Request::new(c.port, "GET", "/api/status").send().status, 503);
}

// py: TestStartupWithoutCertificates.test_allow_http_serves_the_app
#[test]
fn allow_http_serves_the_app() {
    let c = start_child(&["--allow-http"]);
    assert_eq!(Request::new(c.port, "GET", "/").send().status, 200);
    assert_eq!(Request::new(c.port, "GET", "/api/status").send().status, 401);
}

// ---------------------------------------------------------------------------
// TestCancelRoute
// ---------------------------------------------------------------------------

// py: TestCancelRoute.test_too_late_is_409
#[test]
fn too_late_is_409() {
    let outcome = Arc::new(Mutex::new(CancelOutcome::TooLate));
    let o2 = outcome.clone();
    let srv = app_with(Overrides { cancel_power: Some(Arc::new(move || *o2.lock().unwrap())), ..Default::default() });
    let tok = srv.app.new_session(None);
    let r = srv.req("POST", "/api/power/cancel").cookie(&tok).send();
    assert_eq!(r.status, 409);
    assert!(r.json()["error"].as_str().unwrap().contains("Prea târziu"));
    *outcome.lock().unwrap() = CancelOutcome::Aborted;
    let r = srv.req("POST", "/api/power/cancel").cookie(&tok).send();
    assert_eq!((r.status, r.json()["outcome"].clone()), (200, json!("aborted")));
}

// ---------------------------------------------------------------------------
// TestConnectionCap
// ---------------------------------------------------------------------------

// py: TestConnectionCap.test_extra_connections_are_closed
#[test]
fn extra_connections_are_closed() {
    let srv = TestServer::start(Setup { pin: None, max_connections: Some(2), ..Default::default() });
    let idle: Vec<TcpStream> = (0..2).map(|_| TcpStream::connect(("127.0.0.1", srv.port)).unwrap()).collect();
    std::thread::sleep(Duration::from_millis(200));
    let mut extra = TcpStream::connect(("127.0.0.1", srv.port)).unwrap();
    extra.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut buf = [0u8; 100];
    let n = match extra.read(&mut buf) {
        Ok(n) => n,
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset || e.kind() == std::io::ErrorKind::ConnectionAborted => 0,
        Err(e) => panic!("the extra connection was not closed: {e}"),
    };
    assert_eq!(n, 0); // closed without an answer
    drop(extra);
    drop(idle);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(srv.req("GET", "/").send().status, 200);
}

// py: TestConnectionCap.test_limits
#[test]
fn connection_limits() {
    assert_eq!(MAX_CONNECTIONS, 64);
    assert_eq!(CONNECTION_TIMEOUT, Duration::from_secs(15));
}

// Extra (not in the Python suite): the connection timeout closes an idle connection, but
// never a request whose handler is still working.
#[test]
fn idle_connections_time_out_but_slow_handlers_finish() {
    let slow = Overrides {
        status: Some(Arc::new(|| {
            std::thread::sleep(Duration::from_millis(1500));
            json!({"ok": true})
        })),
        ..Default::default()
    };
    let srv = TestServer::start(Setup {
        timeout: Some(Duration::from_secs(1)),
        opts: AppOptions { dry_run: true, overrides: slow, ..Default::default() },
        ..Default::default()
    });
    // idle: closed after about 1 s without a byte sent
    let mut idle = TcpStream::connect(("127.0.0.1", srv.port)).unwrap();
    idle.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let t0 = Instant::now();
    let mut buf = [0u8; 64];
    let _ = idle.read(&mut buf);
    let waited = t0.elapsed();
    assert!(waited >= Duration::from_millis(800) && waited < Duration::from_secs(4), "{waited:?}");
    // a 1.5 s handler with a 1 s timeout still answers
    let tok = srv.app.new_session(None);
    let r = srv.req("GET", "/api/status").cookie(&tok).send();
    assert_eq!(r.status, 200);
    // keep-alive: a second request on the same connection works
    let mut s = TcpStream::connect(("127.0.0.1", srv.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    for _ in 0..2 {
        s.write_all(b"GET /tokens.css HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
        let mut got = Vec::new();
        loop {
            let n = s.read(&mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&got).to_string();
            if let Some(i) = text.find("\r\n\r\n") {
                let len: usize = text.lines().find_map(|l| l.strip_prefix("Content-Length: ")).unwrap().trim().parse().unwrap();
                if got.len() >= i + 4 + len {
                    assert!(text.starts_with("HTTP/1.1 200"));
                    break;
                }
            }
        }
    }
}
