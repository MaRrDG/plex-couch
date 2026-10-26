//! Generate assets/plex-couch.ico from static/icon.svg's shapes.
//!
//! ```text
//! cargo run --example make_icon            # writes assets/plex-couch.ico
//! cargo run --example make_icon -- --check # exits 1 if assets/plex-couch.ico is out of date
//! ```
//!
//! See [`plex_couch::icon`] for what is generated.

use plex_couch::icon::{ico, ICO_PATH};
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut check = false;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--check" => check = true,
            "-h" | "--help" => {
                println!("usage: make_icon [-h] [--check]\n\nGenerate {ICO_PATH} from static/icon.svg.\n\n  --check  only verify {ICO_PATH} is up to date");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("usage: make_icon [-h] [--check]\nmake_icon: error: unrecognized arguments: {other}");
                return ExitCode::from(2);
            }
        }
    }
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join(ICO_PATH);
    let data = ico();
    if check {
        if std::fs::read(&out).ok().as_deref() != Some(data.as_slice()) {
            println!("{ICO_PATH} is out of date; run cargo run --example make_icon");
            return ExitCode::FAILURE;
        }
        println!("{ICO_PATH} is up to date");
        return ExitCode::SUCCESS;
    }
    if let Some(dir) = out.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&out, &data) {
        eprintln!("error: cannot write {ICO_PATH}: {e}");
        return ExitCode::FAILURE;
    }
    println!("wrote {ICO_PATH} ({} bytes)", data.len());
    ExitCode::SUCCESS
}
