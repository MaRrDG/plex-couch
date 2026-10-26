//! Generate static/tokens.css from docs/design-system/tokens.json.
//!
//! ```text
//! cargo run --example build_tokens            # writes static/tokens.css
//! cargo run --example build_tokens -- --check # exits 1 if static/tokens.css is out of date
//! ```
//!
//! See [`plex_couch::tokens`] for what is generated.

use plex_couch::tokens::{render, TOKENS_CSS, TOKENS_JSON};
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut check = false;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--check" => check = true,
            "-h" | "--help" => {
                println!("usage: build_tokens [-h] [--check]\n\nGenerate {TOKENS_CSS} from {TOKENS_JSON}.\n\n  --check  only verify {TOKENS_CSS} is up to date");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("usage: build_tokens [-h] [--check]\nbuild_tokens: error: unrecognized arguments: {other}");
                return ExitCode::from(2);
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tokens = match std::fs::read(root.join(TOKENS_JSON)).map_err(|e| e.to_string()).and_then(|b| {
        serde_json::from_slice::<serde_json::Value>(&b).map_err(|e| e.to_string())
    }) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: cannot read {TOKENS_JSON}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let css = render(&tokens);
    let out = root.join(TOKENS_CSS);
    if check {
        // Universal newlines: a CRLF checkout passes.
        let current = std::fs::read_to_string(&out).ok().map(|s| s.replace("\r\n", "\n"));
        if current.as_deref() != Some(css.as_str()) {
            println!("{TOKENS_CSS} is out of date; run cargo run --example build_tokens");
            return ExitCode::FAILURE;
        }
        println!("{TOKENS_CSS} is up to date");
        return ExitCode::SUCCESS;
    }
    if let Err(e) = std::fs::write(&out, css.as_bytes()) {
        eprintln!("error: cannot write {TOKENS_CSS}: {e}");
        return ExitCode::FAILURE;
    }
    println!("wrote {TOKENS_CSS}");
    ExitCode::SUCCESS
}
