//! The one-shot command line modes, run through the real exe: `--set-pin`, `--setup-ntfy`
//! (test_notify.TestSetupNtfy, ported here because the CLI is Phase 1), `--demo` host check,
//! argument errors and `make-certs` validation.

mod common;

use common::temp_dir;
use plex_couch::config::Config;
use std::process::Command;

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_plex-couch")).args(args).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn set_pin() {
    let d = temp_dir("pin");
    let cfg = d.join("cfg.json");
    let c = cfg.to_string_lossy().into_owned();
    let (code, out, _) = run(&["--set-pin", "12a4", "--config", &c]);
    assert_eq!((code, out.trim()), (2, "PIN must be 4-6 digits."));
    assert!(!cfg.exists());
    let (code, out, _) = run(&["--set-pin", "2468", "--config", &c]);
    assert_eq!(code, 0);
    assert_eq!(out.trim(), format!("PIN saved to {}", Config::load(&cfg).unwrap().path().display()));
    let conf = Config::load(&cfg).unwrap();
    assert!(conf.pin_hash().starts_with("pbkdf2_sha256$200000$"));
    assert!(plex_couch::auth::pin::verify_pin("2468", &conf.pin_hash()));
    assert_eq!(conf.secret().len(), 64);
    let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&cfg).unwrap()).unwrap();
    assert_eq!(raw["settings"], serde_json::json!({"auto_monitors_off": true}));
    let _ = std::fs::remove_dir_all(&d);
}

// py: test_notify.TestSetupNtfy.test_creates_keeps_and_replaces
#[test]
fn setup_ntfy_creates_keeps_and_replaces() {
    let d = temp_dir("ntfy");
    let cfg = d.join("cfg.json");
    let c = cfg.to_string_lossy().into_owned();
    let topic = |p: &std::path::Path| Config::load(p).unwrap().get("notifications").unwrap()["topic"].as_str().unwrap().to_string();
    let (code, out, _) = run(&["--setup-ntfy", "--config", &c]);
    assert_eq!(code, 0);
    let first = topic(&cfg);
    let rest = first.strip_prefix("plexcouch-").unwrap();
    assert!(rest.len() == 24 && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'));
    assert!(out.contains(&format!("https://ntfy.sh/{first}")));
    assert!(out.contains("New ntfy topic saved to "));
    let (_, out, _) = run(&["--setup-ntfy", "--config", &c]);
    assert!(out.contains("Keeping the existing ntfy topic (use --new-topic to replace it)."));
    assert_eq!(topic(&cfg), first);
    run(&["--setup-ntfy", "--new-topic", "--config", &c]);
    assert_ne!(topic(&cfg), first);
    let n = Config::load(&cfg).unwrap().get("notifications").unwrap();
    assert_eq!(n["disk_min_gb"], serde_json::json!(60)); // defaults still merged
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn demo_needs_localhost_and_bad_args() {
    let d = temp_dir("demo");
    let c = d.join("cfg.json").to_string_lossy().into_owned();
    let (code, out, _) = run(&["--demo", "--config", &c]);
    assert_eq!((code, out.trim()), (2, "--demo only runs with --host 127.0.0.1"));
    let (code, _, err) = run(&["--port", "abc"]);
    assert_eq!(code, 2);
    assert!(err.contains("argument --port: invalid int value: 'abc'"), "{err}");
    let (code, out, _) = run(&["--help"]);
    assert_eq!(code, 0);
    assert!(out.contains("--allow-http"));
    let _ = std::fs::remove_dir_all(&d);
}

// The validation half of test_web.TestMakeCerts: errors exit 2.
#[test]
fn make_certs_validation() {
    let d = temp_dir("mc");
    let o = d.join("certs").to_string_lossy().into_owned();
    let (code, _, err) = run(&["make-certs", "--ip", "8.8.8.8", "--dns", "example.com", "--out", &o]);
    assert_eq!(code, 2);
    assert!(err.contains("error: IP outside the CA's private ranges: 8.8.8.8"), "{err}");
    assert!(err.contains("error: DNS name must be localhost or end in .local: example.com"), "{err}");
    let (code, _, err) = run(&["make-certs", "--out", &o]);
    assert_eq!(code, 2);
    assert!(err.contains("error: give at least one --ip or --dns"));
    assert!(!d.join("certs").exists());
    let _ = std::fs::remove_dir_all(&d);
}
