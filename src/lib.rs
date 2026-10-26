//! Plex Couch: a LAN-only phone remote for a Plex PC (Rust rewrite of app.py).
//!
//! Behaviour reference: plans/rust-rewrite-spec.md. Module map:
//! - [`cli`]: flags, startup order, `--set-pin`, `--setup-ntfy`, `make-certs` ([`certs`]).
//! - [`config`], [`state`]: `config.local.json`, `state.local.json`.
//! - [`log`]: the rotating `plex-couch.log` (`log_info!` & co.).
//! - [`http`]: listeners, routes, static files, security rules.
//! - [`auth`]: PIN hashes, session tokens, the login guard; the login flow is on [`app`].
//! - [`app`]: `AppState`, shared by everything.
//! - [`services`], [`logic`], [`system`], [`jobs`]: integrations, app logic, Windows
//!   actions, background jobs (mostly Phase 2 stubs; see each module's TODOs).
//! - [`json`]: Python-compatible JSON output and coercions.

pub mod app;
pub mod auth;
pub mod config;
pub mod http;
pub mod jobs;
pub mod json;
pub mod log;
pub mod logic;
pub mod services;
pub mod state;
pub mod system;
