//! Command line and startup (spec 1.1, 1.2): the app flags, plus the
//! `make-certs` subcommand.
//!
//! ```text
//! plex-couch [--host 0.0.0.0] [--port 8765] [--https-port N] [--cert F] [--key F] [--ca-cert F]
//!            [--config <exe dir>\config.local.json] [--dry-run] [--allow-http] [--demo] [--no-tray]
//! plex-couch --set-pin 1234 [--config F]
//! plex-couch --setup-ntfy [--new-topic] [--config F]
//! plex-couch make-certs --ip 192.168.1.50 [--dns localhost] [--out certs] [--force-ca]
//! ```
//!
//! Like argparse: `--opt value` and `--opt=value`, unique prefixes (`--allow` =
//! `--allow-http`), usage errors exit 2.
//!
//! The server shows a tray icon (Windows, see [`crate::system::tray`]) unless `--no-tray`;
//! its "Ieșire" stops the app like Ctrl+C.

use crate::app::{AppOptions, AppState};
use crate::auth::pin;
use crate::config::{absolute, Config};
use crate::http::server::{self, ListenerCfg, Role};
use crate::services::ntfy;
use crate::system::tray::{self, OpenTarget};
use crate::{jobs, log_error, log_info, log_warning};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const DEFAULT_HOST: &str = "0.0.0.0";
pub const DEFAULT_PORT: u16 = 8765;
pub const DEFAULT_HTTPS_PORT: u16 = 8766;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Args {
    pub host: String,
    pub port: u16,
    pub https_port: Option<u16>,
    pub cert: Option<String>,
    pub key: Option<String>,
    pub ca_cert: Option<String>,
    pub config: Option<PathBuf>,
    pub dry_run: bool,
    pub allow_http: bool,
    pub set_pin: Option<String>,
    pub setup_ntfy: bool,
    pub new_topic: bool,
    pub demo: bool,
    pub no_tray: bool,
}

const USAGE: &str = "usage: plex-couch [-h] [--host HOST] [--port PORT] [--https-port HTTPS_PORT] [--cert CERT] \
[--key KEY] [--ca-cert CA_CERT] [--config CONFIG] [--dry-run] [--allow-http] [--set-pin DIGITS] [--setup-ntfy] \
[--new-topic] [--demo] [--no-tray]\n       plex-couch make-certs [-h] [--ip IP] [--dns DNS] [--out OUT] [--force-ca]";

const HELP: &str = "
Plex Couch

options:
  -h, --help            show this help message and exit
  --host HOST
  --port PORT           HTTP port: the whole app without a certificate, else only the install page
  --https-port HTTPS_PORT
                        HTTPS port (default 8766, or https.port in the config)
  --cert CERT           server certificate, PEM (default certs/server.pem)
  --key KEY             server private key, PEM (default certs/server.key)
  --ca-cert CA_CERT     CA certificate offered at /ca.crt, DER or PEM (default certs/ca.crt)
  --config CONFIG
  --dry-run             log monitor/power actions, never execute them
  --allow-http          without a usable certificate, serve the whole app over plain HTTP (default: only a 503
                        page saying the certificates are missing)
  --set-pin DIGITS      store the PIN hash in the config and exit
  --setup-ntfy          create an ntfy topic for phone notifications (keeps an existing one), print it, exit
  --new-topic           with --setup-ntfy: replace the existing topic
  --demo                serve the UI with fake data and no login (screenshots only; 127.0.0.1 only)
  --no-tray             no system-tray icon (default: an icon that opens the app; right click to exit)

make-certs: create the local CA and the server certificate (see `plex-couch make-certs -h`).";

#[derive(Debug, PartialEq)]
pub enum ParseOutcome {
    Run(Args),
    Help,
    /// argparse error text (exit 2).
    Error(String),
}

const VALUE_OPTS: [&str; 8] = ["--host", "--port", "--https-port", "--cert", "--key", "--ca-cert", "--config", "--set-pin"];
const FLAG_OPTS: [&str; 7] = ["--dry-run", "--allow-http", "--setup-ntfy", "--new-topic", "--demo", "--no-tray", "--help"];

fn port_value(opt: &str, v: &str) -> Result<u16, String> {
    let n: i64 = v.trim().parse().map_err(|_| format!("argument {opt}: invalid int value: '{v}'"))?;
    u16::try_from(n).map_err(|_| format!("argument {opt}: port out of range: {n}"))
}

/// Parse the app's flags (not `make-certs`).
pub fn parse(args: &[String]) -> ParseOutcome {
    let mut a = Args { host: DEFAULT_HOST.into(), port: DEFAULT_PORT, ..Default::default() };
    let all: Vec<&str> = VALUE_OPTS.iter().chain(FLAG_OPTS.iter()).copied().collect();
    let mut i = 0;
    while i < args.len() {
        let raw = &args[i];
        if raw == "-h" {
            return ParseOutcome::Help;
        }
        let (name, inline) = match raw.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_string())),
            _ => (raw.as_str(), None),
        };
        if !name.starts_with("--") {
            return ParseOutcome::Error(format!("unrecognized arguments: {raw}"));
        }
        let opt = if all.contains(&name) {
            name.to_string()
        } else {
            let m: Vec<&&str> = all.iter().filter(|o| o.starts_with(name)).collect();
            match m.len() {
                1 => m[0].to_string(),
                0 => return ParseOutcome::Error(format!("unrecognized arguments: {raw}")),
                _ => {
                    let names: Vec<&str> = m.iter().map(|s| **s).collect();
                    return ParseOutcome::Error(format!("ambiguous option: {name} could match {}", names.join(", ")));
                }
            }
        };
        if FLAG_OPTS.contains(&opt.as_str()) {
            if let Some(v) = inline {
                return ParseOutcome::Error(format!("argument {opt}: ignored explicit argument '{v}'"));
            }
            match opt.as_str() {
                "--help" => return ParseOutcome::Help,
                "--dry-run" => a.dry_run = true,
                "--allow-http" => a.allow_http = true,
                "--setup-ntfy" => a.setup_ntfy = true,
                "--new-topic" => a.new_topic = true,
                "--demo" => a.demo = true,
                "--no-tray" => a.no_tray = true,
                _ => {}
            }
            i += 1;
            continue;
        }
        let v = match inline {
            Some(v) => v,
            None => {
                i += 1;
                match args.get(i) {
                    Some(v) if !(v.starts_with("--") && v.len() > 2) => v.clone(),
                    _ => return ParseOutcome::Error(format!("argument {opt}: expected one argument")),
                }
            }
        };
        match opt.as_str() {
            "--host" => a.host = v,
            "--port" => match port_value(&opt, &v) {
                Ok(p) => a.port = p,
                Err(e) => return ParseOutcome::Error(e),
            },
            "--https-port" => match port_value(&opt, &v) {
                Ok(p) => a.https_port = Some(p),
                Err(e) => return ParseOutcome::Error(e),
            },
            "--cert" => a.cert = Some(v),
            "--key" => a.key = Some(v),
            "--ca-cert" => a.ca_cert = Some(v),
            "--config" => a.config = Some(PathBuf::from(v)),
            "--set-pin" => a.set_pin = Some(v),
            _ => {}
        }
        i += 1;
    }
    ParseOutcome::Run(a)
}

/// The folder of the executable: default config and the log file.
pub fn app_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Resolved HTTPS port and paths: command line first, then config, then defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpsSettings {
    pub port: u16,
    pub cert: String,
    pub key: String,
    pub ca_cert: String,
}

pub fn https_settings(args: &Args, config: &Config) -> Result<HttpsSettings, String> {
    let h = config.get("https").filter(|v| crate::json::truthy(Some(v))).unwrap_or(Value::Null);
    let port = match args.https_port {
        Some(p) => p,
        None => {
            let v = h.get("port");
            if crate::json::truthy(v) {
                let n = crate::json::py_int(v.unwrap()).ok_or("https.port is not a number")?;
                u16::try_from(n).map_err(|_| "https.port is out of range".to_string())?
            } else {
                DEFAULT_HTTPS_PORT
            }
        }
    };
    let cli = |v: &Option<String>| v.as_ref().filter(|s| !s.is_empty()).map(|s| absolute(Path::new(s)).to_string_lossy().into_owned());
    Ok(HttpsSettings {
        port,
        cert: cli(&args.cert).unwrap_or_else(|| config.resolve(h.get("cert"))),
        key: cli(&args.key).unwrap_or_else(|| config.resolve(h.get("key"))),
        ca_cert: cli(&args.ca_cert).unwrap_or_else(|| config.resolve(h.get("ca_cert"))),
    })
}

/// `--setup-ntfy`: make sure the config has a topic, print it and how to subscribe.
pub fn setup_ntfy(config: &Arc<Config>, replace: bool) -> i32 {
    let n = ntfy::Notifier::new(config.clone(), true, crate::services::Http::new());
    let mut topic = n.topic();
    if topic.is_none() || replace {
        let t = ntfy::new_topic();
        if let Err(e) = config.set_notification_topic(&t) {
            eprintln!("error: cannot save {}: {e}", config.path().display());
            return 1;
        }
        log_info!("ntfy topic {} via --setup-ntfy", if replace { "replaced" } else { "created" });
        println!("New ntfy topic saved to {}", config.path().display());
        topic = Some(t);
    } else {
        println!("Keeping the existing ntfy topic (use --new-topic to replace it).");
    }
    let topic = topic.unwrap_or_default();
    let server = n.server();
    println!("Topic: {topic}");
    println!("Subscribe to it in the ntfy app (server {server}), or open: {}", ntfy::subscribe_url(&server, &topic));
    println!("Anyone who knows the topic can read the notifications: keep it private.");
    println!("Restart Plex Couch to start sending notifications.");
    0
}

/// Program entry: returns the exit code.
pub fn main(argv: Vec<String>) -> i32 {
    let args: Vec<String> = argv.into_iter().skip(1).collect();
    if args.first().map(String::as_str) == Some("make-certs") {
        return crate::certs::main(&args[1..], &app_dir().join("certs"));
    }
    let a = match parse(&args) {
        ParseOutcome::Run(a) => a,
        ParseOutcome::Help => {
            println!("{USAGE}\n{HELP}");
            return 0;
        }
        ParseOutcome::Error(e) => {
            eprintln!("{USAGE}\nplex-couch: error: {e}");
            return 2;
        }
    };
    run(a)
}

/// Startup order (spec 1.2).
pub fn run(a: Args) -> i32 {
    let dir = app_dir();
    let _ = crate::log::init_file(&dir.join(crate::log::LOG_FILE));
    server::install_panic_hook();
    let config_path = a.config.clone().unwrap_or_else(|| dir.join("config.local.json"));
    let config = match Config::load(&config_path) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    if let Some(p) = &a.set_pin {
        if !pin::valid_pin_format(p) {
            println!("PIN must be 4-6 digits.");
            return 2;
        }
        if let Err(e) = config.set_pin_hash(&pin::hash_pin(p, pin::PBKDF2_ITERATIONS)) {
            eprintln!("error: cannot save {}: {e}", config.path().display());
            return 1;
        }
        log_info!("PIN changed via --set-pin");
        println!("PIN saved to {}", config.path().display());
        return 0;
    }
    if a.setup_ntfy {
        return setup_ntfy(&config, a.new_topic);
    }
    if a.demo && a.host != "127.0.0.1" {
        println!("--demo only runs with --host 127.0.0.1");
        return 2;
    }

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(16)
        .thread_name("svc")
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: cannot start the runtime: {e}");
            return 1;
        }
    };
    rt.block_on(serve(a, config))
}

async fn serve(a: Args, config: Arc<Config>) -> i32 {
    let app = match AppState::new(config.clone(), AppOptions { dry_run: a.dry_run, demo: a.demo, ..Default::default() }) {
        Ok(app) => app,
        Err(e) => {
            eprintln!("error: cannot write {}: {e}", config.path().display());
            return 1;
        }
    };
    let tls = match https_settings(&a, &config) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let allow_http = a.allow_http || a.demo; // --demo is 127.0.0.1 only and has no real data
    let host = a.host.as_str();
    let mut listeners: Vec<(String, u16, ListenerCfg)> = Vec::new();
    // what the tray icon opens: the app over HTTPS, else whatever the http port serves
    let mut open = OpenTarget { secure: false, port: a.port, bind_host: a.host.clone(), cert: None };
    let where_ = match server::load_tls(&tls.cert, &tls.key) {
        Ok(acceptor) => {
            open = OpenTarget { secure: true, port: tls.port, bind_host: a.host.clone(), cert: Some(PathBuf::from(&tls.cert)) };
            listeners.push((host.into(), tls.port, ListenerCfg::new(Role::App { secure: true }, Some(acceptor))));
            let install = Role::Install { https_port: tls.port, ca_cert: PathBuf::from(&tls.ca_cert) };
            listeners.push((host.into(), a.port, ListenerCfg::new(install, None)));
            format!("https {host}:{}, http {host}:{} (install page)", tls.port, a.port)
        }
        Err(problem) if allow_http => {
            if !a.demo {
                log_warning!("HTTPS certificate/key {problem}; serving the app over plain HTTP (--allow-http)");
            }
            listeners.push((host.into(), a.port, ListenerCfg::new(Role::App { secure: false }, None)));
            format!("http {host}:{} (no certificate, HTTPS off, --allow-http)", a.port)
        }
        Err(problem) => {
            log_error!(
                "HTTPS certificate/key {problem}: serving only an error page on http port {}. \
                 Run plex-couch make-certs (or start with --allow-http for plain HTTP).",
                a.port
            );
            println!(
                "HTTPS certificate/key {problem}. Only an error page is served on port {}; run \
                 plex-couch make-certs, or start with --allow-http to serve the app over plain HTTP.",
                a.port
            );
            listeners.push((host.into(), a.port, ListenerCfg::new(Role::NoCert, None)));
            format!("http {host}:{} (certificates missing: error page only)", a.port)
        }
    };

    // Bind everything before anything starts (a port in use stops the start-up).
    let mut bound = Vec::new();
    for (h, port, cfg) in listeners {
        match server::bind(&h, port).and_then(tokio::net::TcpListener::from_std) {
            Ok(l) => bound.push((l, Arc::new(cfg))),
            Err(e) => {
                log_error!("cannot listen on {h}:{port}: {}", crate::state::io_error_name(&e));
                eprintln!("error: cannot listen on {h}:{port}: {e}");
                return 1;
            }
        }
    }

    let (stop_handle, stop) = jobs::stop_pair();
    jobs::spawn_all(app.clone(), stop);
    for (l, cfg) in bound {
        tokio::spawn(server::serve(l, cfg, app.clone()));
    }
    let mode = if a.demo { " (demo)" } else if a.dry_run { " (dry-run)" } else { "" };
    log_info!("started on {where_}{mode}");
    if config.pin_hash().is_empty() {
        log_warning!("no PIN set; run plex-couch --set-pin <digits>");
    }
    let quit = Arc::new(tokio::sync::Notify::new());
    let tray = if a.no_tray { None } else { start_tray(&config, open, quit.clone()) };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = quit.notified() => {}
    }
    stop_handle.stop();
    if let Some(t) = tray {
        t.stop();
    }
    log_info!("stopped");
    0
}

/// The tray icon (Windows); a failure is logged and the server runs on without it.
fn start_tray(config: &Config, open: OpenTarget, quit: Arc<tokio::sync::Notify>) -> Option<tray::Tray> {
    if !cfg!(windows) {
        return None;
    }
    log_info!("tray: a click opens {}", open.url());
    let opts = tray::TrayOptions {
        app_name: config.app_name(),
        url: Box::new(move || open.url()),
        on_exit: Box::new(move || quit.notify_one()),
    };
    match tray::start(opts) {
        Ok(t) => Some(t),
        Err(e) => {
            log_warning!("no tray icon: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(v: &[&str]) -> ParseOutcome {
        parse(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn defaults_and_flags() {
        let ParseOutcome::Run(a) = p(&[]) else { panic!() };
        assert_eq!((a.host.as_str(), a.port, a.https_port), ("0.0.0.0", 8765, None));
        let ParseOutcome::Run(a) = p(&["--host", "127.0.0.1", "--port=18765", "--https-port", "18766", "--allow", "--dry-run",
                                       "--config", "x.json", "--set-pin", "1234"]) else { panic!() };
        assert_eq!(a.host, "127.0.0.1");
        assert_eq!((a.port, a.https_port), (18765, Some(18766)));
        assert!(a.allow_http && a.dry_run);
        assert_eq!(a.config, Some(PathBuf::from("x.json")));
        assert_eq!(a.set_pin.as_deref(), Some("1234"));
        assert!(!a.no_tray);
        let ParseOutcome::Run(a) = p(&["--no-tray", "--demo"]) else { panic!() };
        assert!(a.no_tray && a.demo);
        let ParseOutcome::Run(a) = p(&["--no-t"]) else { panic!() };
        assert!(a.no_tray);
        assert!(matches!(p(&["--no-tray=1"]), ParseOutcome::Error(e) if e.contains("ignored explicit argument")));
        assert!(matches!(p(&["--n"]), ParseOutcome::Error(e) if e.contains("ambiguous")));
    }

    #[test]
    fn errors() {
        assert!(matches!(p(&["--port", "abc"]), ParseOutcome::Error(e) if e.contains("invalid int value: 'abc'")));
        assert!(matches!(p(&["--nope"]), ParseOutcome::Error(_)));
        assert!(matches!(p(&["--host"]), ParseOutcome::Error(_)));
        assert!(matches!(p(&["--d"]), ParseOutcome::Error(e) if e.contains("ambiguous")));
        assert_eq!(p(&["-h"]), ParseOutcome::Help);
    }

    // py: test_web.TestHttpsSettings.test_precedence_cli_config_default
    #[test]
    fn https_precedence_cli_config_default() {
        let d = std::env::temp_dir().join(format!("pc-https-{}", crate::config::token_hex(6)));
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("cfg.json");
        std::fs::write(&path, r#"{"https": {"port": 9443, "key": "keys/k.pem"}}"#).unwrap();
        let cfg = Config::load(&path).unwrap();
        let ds = cfg.dir().to_string_lossy().into_owned();
        let none = Args { host: "x".into(), ..Default::default() };
        let h = https_settings(&none, &cfg).unwrap();
        assert_eq!(h.port, 9443);
        assert_eq!(h.key, format!("{ds}\\keys\\k.pem"));
        assert_eq!(h.cert, format!("{ds}\\certs\\server.pem"));
        assert_eq!(h.ca_cert, format!("{ds}\\certs\\ca.crt"));
        let c = format!("{ds}\\c.pem");
        let cli = Args { https_port: Some(8800), cert: Some(c.clone()), ..none };
        let h = https_settings(&cli, &cfg).unwrap();
        assert_eq!((h.port, h.cert), (8800, c));
        let _ = std::fs::remove_dir_all(&d);
    }
}
