//! The shared application state (Python's `App`, app.py:158-178) and the auth flows that
//! sit on it: sessions, login, logout everywhere.
//!
//! Phase 2 code receives `&Arc<AppState>` and reaches everything through it: the config,
//! the state file, the upstream HTTP client and service clients, the power actions, the
//! notifier and the after-movie state.

use crate::auth::guard::LoginGuard;
use crate::auth::pin;
use crate::auth::session::{self, now as unix_now, SESSION_DAYS};
use crate::config::Config;
use crate::logic::power::AfterMovie;
use crate::services::ntfy::Notifier;
use crate::services::{Http, Services};
use crate::state::{StateFile, STATE_FILE};
use crate::system::power::{Actions, CancelOutcome};
use crate::{log_debug, log_info, log_warning};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::Semaphore;

/// Login requests checked at the same time; more get a 429.
pub const LOGIN_CONCURRENCY: usize = 4;
/// Own IPv4 addresses are re-read when older than this.
pub const OWN_IPS_TTL_S: u64 = 300;

pub type StatusFn = Arc<dyn Fn() -> Value + Send + Sync>;
pub type PosterFn = Arc<dyn Fn(&str, &str) -> Option<(Vec<u8>, String)> + Send + Sync>;
pub type CancelFn = Arc<dyn Fn() -> CancelOutcome + Send + Sync>;
pub type VerifyFn = Arc<dyn Fn(&str, &str) -> bool + Send + Sync>;
pub type OwnIpsFn = Arc<dyn Fn() -> HashSet<String> + Send + Sync>;

/// Test seams (the Python tests monkeypatch exactly these). All `None` in production.
#[derive(Default, Clone)]
pub struct Overrides {
    /// Replaces `GET /api/status`'s body.
    pub status: Option<StatusFn>,
    /// Replaces the image proxy lookup: `(kind, id) -> (bytes, content type)`.
    pub poster: Option<PosterFn>,
    /// Replaces `actions.cancel_power()` in `POST /api/power/cancel`.
    pub cancel_power: Option<CancelFn>,
    /// Replaces `verify_pin(pin, stored)` in the login.
    pub verify_pin: Option<VerifyFn>,
    /// Replaces the own-IPv4 lookup.
    pub own_ips: Option<OwnIpsFn>,
}

#[derive(Default, Clone)]
pub struct AppOptions {
    pub dry_run: bool,
    pub demo: bool,
    pub overrides: Overrides,
}

pub struct AppState {
    pub config: Arc<Config>,
    pub demo: bool,
    /// `dry_run or demo`.
    pub dry_run: bool,
    pub state_file: StateFile,
    pub guard: LoginGuard,
    pub login_slots: Arc<Semaphore>,
    /// One PBKDF2 check at a time.
    pub pin_lock: Arc<Mutex<()>>,
    /// The shared upstream HTTP client (redirects refused when a key header is attached).
    pub http: Http,
    pub services: Services,
    pub actions: Actions,
    pub notifier: Notifier,
    /// "Shut down after the movie": memory only, a restart disarms it.
    pub after_movie: Mutex<Option<AfterMovie>>,
    /// The drive the disk-low check watches (from Radarr's root folders), `D:` until known.
    pub disk_drive: Mutex<String>,
    own_ips: Mutex<Option<(Instant, HashSet<String>)>>,
    /// Process start (demo positions).
    pub started: Instant,
    pub overrides: Overrides,
}

pub fn lockout_body(seconds: i64) -> Value {
    json!({"error": format!("Prea multe încercări. Așteaptă {seconds} secunde."), "lockout_seconds": seconds})
}

impl AppState {
    /// Build the app (writes `secret` into the config on first run).
    pub fn new(config: Arc<Config>, opts: AppOptions) -> io::Result<Arc<AppState>> {
        let dry_run = opts.dry_run || opts.demo;
        config.ensure_secret()?;
        let http = Http::new();
        let services = Services::new(config.clone(), http.clone());
        let actions = Actions::new(dry_run);
        let notifier = Notifier::new(config.clone(), dry_run, http.clone());
        let state_file = StateFile::new(&config.dir().join(STATE_FILE));
        let guard = LoginGuard::new();
        guard.load_known(state_file.load().get("known_ips"), None);
        Ok(Arc::new(AppState {
            config,
            demo: opts.demo,
            dry_run,
            state_file,
            guard,
            login_slots: Arc::new(Semaphore::new(LOGIN_CONCURRENCY)),
            pin_lock: Arc::new(Mutex::new(())),
            http,
            services,
            actions,
            notifier,
            after_movie: Mutex::new(None),
            disk_drive: Mutex::new("D:".into()),
            own_ips: Mutex::new(None),
            started: Instant::now(),
            overrides: opts.overrides,
        }))
    }

    // -- auth -----------------------------------------------------------------
    pub fn session_key(&self) -> [u8; 32] {
        session::session_key(&self.config.secret(), &self.config.pin_hash())
    }

    pub fn new_session(&self, now: Option<f64>) -> String {
        session::sign_session(&self.session_key(), now.unwrap_or_else(unix_now), SESSION_DAYS, self.config.session_epoch())
    }

    /// `(ok, renewed token)`: a fresh token when the valid one has less than 15 days left.
    pub fn check_session(&self, token: Option<&str>, now: Option<f64>) -> (bool, Option<String>) {
        if self.demo {
            return (true, None);
        }
        if self.config.pin_hash().is_empty() {
            return (false, None);
        }
        let now = now.unwrap_or_else(unix_now);
        match session::session_expiry(&self.session_key(), token, now, self.config.session_epoch()) {
            None => (false, None),
            Some(exp) => {
                let fresh = session::session_needs_renewal(exp, now).then(|| self.new_session(Some(now)));
                (true, fresh)
            }
        }
    }

    pub fn session_ok(&self, token: Option<&str>) -> bool {
        self.check_session(token, None).0
    }

    /// Invalidate every session (all devices) by bumping the session epoch. No-op in demo.
    pub fn logout_all(&self) -> io::Result<()> {
        if self.demo {
            return Ok(());
        }
        let epoch = self.config.bump_session_epoch()?;
        log_info!("all sessions revoked (session epoch {epoch})");
        Ok(())
    }

    fn verifier(&self) -> VerifyFn {
        self.overrides.verify_pin.clone().unwrap_or_else(|| Arc::new(pin::verify_pin))
    }

    /// Check a PIN for `ip`: `(status, body, session token)`. `pin` is None when the request
    /// did not carry a string (that still uses up an attempt).
    ///
    /// The attempt is reserved before the check, at most [`LOGIN_CONCURRENCY`] requests are
    /// checked at once and the PBKDF2 checks run one at a time.
    pub async fn login(self: &Arc<Self>, ip: &str, pin: Option<String>) -> (u16, Value, Option<String>) {
        if self.demo {
            return (200, json!({"ok": true}), None);
        }
        let pin_hash = self.config.pin_hash();
        if pin_hash.is_empty() {
            return (503, json!({"error": "PIN-ul nu este setat. Rulează app.py --set-pin."}), None);
        }
        let (ticket, wait) = self.guard.reserve(ip, None);
        let Some(ticket) = ticket.filter(|_| wait == 0) else {
            return (429, lockout_body(wait), None);
        };
        let Ok(slot) = self.login_slots.clone().try_acquire_owned() else {
            self.guard.refund(&ticket, None);
            log_debug!("login refused from {ip}: too many at once");
            return (429, lockout_body(self.guard.busy_retry), None);
        };
        let mut ok = false;
        if let Some(pin) = pin.filter(|p| p.chars().count() <= 64) {
            let (lock, verify) = (self.pin_lock.clone(), self.verifier());
            let r = tokio::task::spawn_blocking(move || {
                let _one_at_a_time = lock.lock().unwrap_or_else(|e| e.into_inner());
                verify(&pin, &pin_hash)
            })
            .await;
            match r {
                Ok(v) => ok = v,
                Err(e) => {
                    self.guard.refund(&ticket, None);
                    drop(slot);
                    std::panic::resume_unwind(e.try_into_panic().unwrap_or_else(|_| Box::new("cancelled")));
                }
            }
        }
        drop(slot);
        if ok {
            self.guard.succeed(&ticket, None);
            let mut changes = Map::new();
            changes.insert("known_ips".into(), Value::Object(self.guard.known_snapshot(None)));
            self.state_file.update(changes);
            log_info!("login ok from {ip}");
            return (200, json!({"ok": true}), Some(self.new_session(None)));
        }
        let locked = self.guard.fail(&ticket, None);
        log_warning!("login failed from {ip}");
        if locked > 0 {
            return (429, lockout_body(locked), None);
        }
        (401, json!({"error": "PIN greșit.", "attempts_left": self.guard.attempts_left(ip)}), None)
    }

    /// This PC's IPv4 addresses, cached for 5 minutes (players on them are not the TV).
    pub fn own_ips(&self) -> HashSet<String> {
        if let Some(f) = &self.overrides.own_ips {
            return f();
        }
        let mut g = self.own_ips.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, ips)) = g.as_ref() {
            if at.elapsed().as_secs() <= OWN_IPS_TTL_S {
                return ips.clone();
            }
        }
        let ips = crate::system::info::own_ipv4s();
        *g = Some((Instant::now(), ips.clone()));
        ips
    }
}
