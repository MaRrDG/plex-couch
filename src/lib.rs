//! Plex Couch: a LAN-only phone remote for a Plex PC (ported from the former Python app).
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
//! - [`json`]: the JSON output format and value coercions.

pub mod app;
pub mod auth;
pub mod certs;
pub mod cli;
pub mod config;
pub mod http;
pub mod jobs;
pub mod json;
pub mod log;
pub mod logic;
pub mod services;
pub mod state;
pub mod system;
pub mod tokens;
