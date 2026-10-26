//! Brute-force guard for the PIN login (spec 4.3).
//!
//! Per IP: 5 wrong PINs lock that IP for 10 minutes. Every attempt is reserved under the lock
//! *before* the PIN is checked, so parallel requests cannot run more than 5 checks per
//! lockout window; a correct PIN refunds it. Globally: once 20 attempts failed within an
//! hour, IPs that did not log in during the last 30 days may try only once every 60 s in
//! total. Known IPs are not slowed down by that (their own per-IP lockout still applies).
//!
//! Use: `let (ticket, wait) = reserve(ip)`; `wait > 0` means refuse. Then exactly one of
//! `succeed(ticket)`, `fail(ticket)` or `refund(ticket)`.

use crate::auth::session::now as unix_now;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Mutex;

pub const KNOWN_IP_DAYS: i64 = 30;

#[derive(Clone, Debug, PartialEq)]
pub struct Ticket {
    pub ip: String,
    pub t: f64,
}

#[derive(Default, Clone, Copy)]
struct Entry {
    fails: u32,
    inflight: u32,
    locked_until: f64,
}

struct Inner {
    ip: HashMap<String, Entry>,
    global: Vec<f64>,
    known: HashMap<String, f64>,
}

pub struct LoginGuard {
    pub per_ip_max: u32,
    pub global_max: usize,
    pub lock_seconds: f64,
    pub window_seconds: f64,
    pub global_delay: f64,
    pub known_seconds: f64,
    pub busy_retry: i64,
    inner: Mutex<Inner>,
}

fn ceil_pos(left: f64) -> i64 {
    if left > 0.0 {
        left.ceil() as i64
    } else {
        0
    }
}

impl Default for LoginGuard {
    fn default() -> Self {
        LoginGuard::new()
    }
}

impl LoginGuard {
    pub fn new() -> LoginGuard {
        LoginGuard {
            per_ip_max: 5,
            global_max: 20,
            lock_seconds: 600.0,
            window_seconds: 3600.0,
            global_delay: 60.0,
            known_seconds: (KNOWN_IP_DAYS * 86400) as f64,
            busy_retry: 5,
            inner: Mutex::new(Inner { ip: HashMap::new(), global: Vec::new(), known: HashMap::new() }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    // -- known (successfully logged in) IPs ---------------------------------
    /// Load `known_ips` from the state file: str -> number entries younger than 30 days.
    pub fn load_known(&self, data: Option<&Value>, now: Option<f64>) {
        let now = now.unwrap_or_else(unix_now);
        let mut g = self.lock();
        g.known.clear();
        if let Some(Value::Object(m)) = data {
            for (ip, t) in m {
                if let Some(t) = t.as_f64().filter(|_| t.is_number()) {
                    if now - t < self.known_seconds {
                        g.known.insert(ip.clone(), t);
                    }
                }
            }
        }
    }

    /// `{ip: int(t)}` of the known IPs, pruned.
    pub fn known_snapshot(&self, now: Option<f64>) -> Map<String, Value> {
        let now = now.unwrap_or_else(unix_now);
        let mut g = self.lock();
        self.prune_known(&mut g, now);
        let mut ips: Vec<(&String, &f64)> = g.known.iter().collect();
        ips.sort_by(|a, b| a.0.cmp(b.0));
        ips.into_iter().map(|(ip, t)| (ip.clone(), Value::from(*t as i64))).collect()
    }

    pub fn is_known(&self, ip: &str, now: Option<f64>) -> bool {
        let now = now.unwrap_or_else(unix_now);
        let g = self.lock();
        self.is_known_locked(&g, ip, now)
    }

    fn is_known_locked(&self, g: &Inner, ip: &str, now: f64) -> bool {
        g.known.get(ip).is_some_and(|t| now - t < self.known_seconds)
    }

    fn prune_known(&self, g: &mut Inner, now: f64) {
        let ks = self.known_seconds;
        g.known.retain(|_, t| now - *t < ks);
    }

    // -- state ---------------------------------------------------------------
    fn cleanup(g: &mut Inner, ip: &str, now: f64) {
        if let Some(e) = g.ip.get(ip) {
            if e.fails == 0 && e.inflight == 0 && e.locked_until <= now {
                g.ip.remove(ip);
            }
        }
    }

    fn ip_lock_left(g: &Inner, ip: &str, now: f64) -> i64 {
        let until = g.ip.get(ip).map(|e| e.locked_until).unwrap_or(0.0);
        ceil_pos(until - now)
    }

    fn global_left(&self, g: &mut Inner, ip: &str, now: f64) -> i64 {
        let cutoff = now - self.window_seconds;
        g.global.retain(|t| *t > cutoff);
        if self.is_known_locked(g, ip, now) || g.global.len() < self.global_max {
            return 0;
        }
        let newest = g.global.iter().cloned().fold(f64::MIN, f64::max);
        ceil_pos(newest + self.global_delay - now)
    }

    /// Seconds this IP has to wait before the next attempt (0 = may try).
    pub fn remaining(&self, ip: &str, now: Option<f64>) -> i64 {
        let now = now.unwrap_or_else(unix_now);
        let mut g = self.lock();
        let a = Self::ip_lock_left(&g, ip, now);
        a.max(self.global_left(&mut g, ip, now))
    }

    pub fn attempts_left(&self, ip: &str) -> u32 {
        let g = self.lock();
        let e = g.ip.get(ip).copied().unwrap_or_default();
        self.per_ip_max.saturating_sub(e.fails + e.inflight)
    }

    // -- attempts ------------------------------------------------------------
    /// Reserve one PIN check. `(Some(ticket), 0)` or `(None, seconds to wait)`.
    pub fn reserve(&self, ip: &str, now: Option<f64>) -> (Option<Ticket>, i64) {
        let now = now.unwrap_or_else(unix_now);
        let mut g = self.lock();
        let left = Self::ip_lock_left(&g, ip, now).max(self.global_left(&mut g, ip, now));
        if left > 0 {
            return (None, left);
        }
        let e = g.ip.entry(ip.to_string()).or_default();
        if e.fails + e.inflight >= self.per_ip_max {
            return (None, self.busy_retry); // the checks still running decide the lockout
        }
        e.inflight += 1;
        g.global.push(now);
        (Some(Ticket { ip: ip.to_string(), t: now }), 0)
    }

    fn close<'a>(g: &'a mut Inner, ticket: &Ticket) -> &'a mut Entry {
        let e = g.ip.entry(ticket.ip.clone()).or_default();
        e.inflight = e.inflight.saturating_sub(1);
        e
    }

    fn drop_global(g: &mut Inner, t: f64) {
        if let Some(i) = g.global.iter().position(|x| *x == t) {
            g.global.remove(i);
        }
    }

    /// The PIN was right: refund the attempt, clear this IP's failures, remember the IP.
    pub fn succeed(&self, ticket: &Ticket, now: Option<f64>) {
        let now = now.unwrap_or_else(unix_now);
        let mut g = self.lock();
        let e = Self::close(&mut g, ticket);
        e.fails = 0;
        e.locked_until = 0.0;
        Self::drop_global(&mut g, ticket.t);
        g.known.insert(ticket.ip.clone(), now);
        self.prune_known(&mut g, now);
        Self::cleanup(&mut g, &ticket.ip, now);
    }

    /// The PIN was wrong. Returns the lockout seconds now in force for this IP (0 if none).
    pub fn fail(&self, ticket: &Ticket, now: Option<f64>) -> i64 {
        let now = now.unwrap_or_else(unix_now);
        let mut g = self.lock();
        let (per_ip_max, lock_seconds) = (self.per_ip_max, self.lock_seconds);
        let e = Self::close(&mut g, ticket);
        e.fails += 1;
        if e.fails >= per_ip_max {
            e.locked_until = now + lock_seconds;
            e.fails = 0;
        }
        let left = Self::ip_lock_left(&g, &ticket.ip, now);
        Self::cleanup(&mut g, &ticket.ip, now);
        left
    }

    /// The check never ran (server busy): give the attempt back.
    pub fn refund(&self, ticket: &Ticket, now: Option<f64>) {
        let now = now.unwrap_or_else(unix_now);
        let mut g = self.lock();
        Self::close(&mut g, ticket);
        Self::drop_global(&mut g, ticket.t);
        Self::cleanup(&mut g, &ticket.ip, now);
    }

    // -- one-shot helpers (reserved and decided at once) ----------------------
    pub fn record_failure(&self, ip: &str, now: Option<f64>) -> i64 {
        let now = now.unwrap_or_else(unix_now);
        {
            let mut g = self.lock();
            g.ip.entry(ip.to_string()).or_default().inflight += 1;
            g.global.push(now);
        }
        self.fail(&Ticket { ip: ip.to_string(), t: now }, Some(now))
    }

    pub fn record_success(&self, ip: &str, now: Option<f64>) {
        let now = now.unwrap_or_else(unix_now);
        {
            let mut g = self.lock();
            g.ip.entry(ip.to_string()).or_default().inflight += 1;
            g.global.push(now);
        }
        self.succeed(&Ticket { ip: ip.to_string(), t: now }, Some(now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Barrier};

    const DAY: f64 = 86400.0;

    // ---- test_security.TestGuardReservation ----

    // py: test_parallel_reservations_never_exceed_the_per_ip_limit
    #[test]
    fn parallel_reservations_never_exceed_the_per_ip_limit() {
        let g = Arc::new(LoginGuard::new());
        let n = 60;
        let barrier = Arc::new(Barrier::new(n));
        let handles: Vec<_> = (0..n)
            .map(|_| {
                let (g, b) = (g.clone(), barrier.clone());
                std::thread::spawn(move || {
                    b.wait();
                    g.reserve("10.0.0.7", None)
                })
            })
            .collect();
        let mut tickets = Vec::new();
        let mut waits = 0;
        for h in handles {
            match h.join().unwrap() {
                (Some(t), _) => tickets.push(t),
                (None, _) => waits += 1,
            }
        }
        assert_eq!(tickets.len(), 5);
        assert_eq!(waits, n - 5);
        assert_eq!(g.attempts_left("10.0.0.7"), 0);
        let results: Vec<i64> = tickets.iter().map(|t| g.fail(t, None)).collect();
        assert_eq!(&results[..4], &[0, 0, 0, 0]);
        assert!(results[4] > 590);
        assert!(g.reserve("10.0.0.7", None).0.is_none());
        assert!(g.remaining("10.0.0.7", None) > 590);
        assert_eq!(g.remaining("10.0.0.8", None), 0);
    }

    // py: test_success_refunds_and_resets
    #[test]
    fn success_refunds_and_resets() {
        let g = LoginGuard::new();
        let (t1, _) = g.reserve("10.0.0.7", Some(100.0));
        g.fail(&t1.unwrap(), Some(100.0));
        let (t2, _) = g.reserve("10.0.0.7", Some(101.0));
        assert_eq!(g.attempts_left("10.0.0.7"), 3);
        g.succeed(&t2.unwrap(), Some(101.0));
        assert_eq!(g.attempts_left("10.0.0.7"), 5);
        assert!(g.is_known("10.0.0.7", Some(102.0)));
    }

    // py: test_correct_pin_while_others_in_flight_unlocks
    #[test]
    fn correct_pin_while_others_in_flight_unlocks() {
        let g = LoginGuard::new();
        let tickets: Vec<Ticket> = (0..5).map(|_| g.reserve("10.0.0.7", Some(100.0)).0.unwrap()).collect();
        for t in &tickets[..4] {
            g.fail(t, Some(100.0));
        }
        g.succeed(&tickets[4], Some(100.0));
        assert_eq!(g.remaining("10.0.0.7", Some(101.0)), 0);
    }

    // py: test_refund
    #[test]
    fn refund() {
        let g = LoginGuard::new();
        let (t, _) = g.reserve("10.0.0.7", Some(100.0));
        g.refund(&t.unwrap(), Some(100.0));
        assert_eq!(g.attempts_left("10.0.0.7"), 5);
    }

    // py: test_per_ip_lockout_window
    #[test]
    fn per_ip_lockout_window() {
        let g = LoginGuard::new();
        for i in 0..5 {
            let (t, w) = g.reserve("10.0.0.7", Some(100.0 + i as f64));
            assert_eq!(w, 0);
            g.fail(&t.unwrap(), Some(100.0 + i as f64));
        }
        assert_eq!(g.reserve("10.0.0.7", Some(200.0)), (None, 504));
        assert_eq!(g.reserve("10.0.0.7", Some(704.0)).1, 0);
    }

    // ---- test_security.TestGuardKnownIps ----

    fn flood(g: &LoginGuard, now: f64) {
        for i in 0..20 {
            g.record_failure(&format!("10.0.1.{i}"), Some(now));
        }
    }

    // py: test_known_ip_is_not_slowed_down
    #[test]
    fn known_ip_is_not_slowed_down() {
        let g = LoginGuard::new();
        g.record_success("192.168.0.20", Some(900.0));
        flood(&g, 1000.0);
        assert!(g.remaining("192.168.0.99", Some(1001.0)) > 0);
        assert_eq!(g.remaining("192.168.0.20", Some(1001.0)), 0);
        let (t, w) = g.reserve("192.168.0.20", Some(1001.0));
        assert_eq!(w, 0);
        g.succeed(&t.unwrap(), Some(1001.0));
    }

    // py: test_known_ip_keeps_its_per_ip_lockout
    #[test]
    fn known_ip_keeps_its_per_ip_lockout() {
        let g = LoginGuard::new();
        g.record_success("192.168.0.20", Some(900.0));
        for i in 0..5 {
            g.record_failure("192.168.0.20", Some(1000.0 + i as f64));
        }
        assert!(g.remaining("192.168.0.20", Some(1010.0)) > 0);
    }

    // py: test_known_ips_expire_after_30_days_and_round_trip
    // (The original test also passed the int key 5; JSON object keys are always strings, so that case
    //  cannot occur in Rust and is left out.)
    #[test]
    fn known_ips_expire_after_30_days_and_round_trip() {
        let g = LoginGuard::new();
        g.load_known(
            Some(&json!({"192.168.0.20": 1000, "192.168.0.21": 1000.0 - 31.0 * DAY, "bad": "x"})),
            Some(1000.0),
        );
        let snap = g.known_snapshot(Some(1000.0));
        assert_eq!(Value::Object(snap), json!({"192.168.0.20": 1000}));
        assert!(!g.is_known("192.168.0.20", Some(1000.0 + 30.0 * DAY)));
        flood(&g, 1000.0 + 30.0 * DAY);
        assert!(g.remaining("192.168.0.20", Some(1000.0 + 30.0 * DAY + 1.0)) > 0);
        g.load_known(None, None);
        g.load_known(Some(&json!(["x"])), None);
    }

    // ---- test_logic.TestLoginGuard (the guard lives here, so its tests do too) ----

    // py: test_logic.TestLoginGuard.test_per_ip_lockout
    #[test]
    fn per_ip_lockout() {
        let g = LoginGuard::new();
        for i in 0..4 {
            assert_eq!(g.record_failure("10.0.0.2", Some(100.0 + i as f64)), 0);
        }
        assert_eq!(g.attempts_left("10.0.0.2"), 1);
        assert_eq!(g.record_failure("10.0.0.2", Some(105.0)), 600);
        assert_eq!(g.remaining("10.0.0.2", Some(105.0)), 600);
        assert_eq!(g.remaining("10.0.0.2", Some(405.0)), 300);
        assert_eq!(g.remaining("10.0.0.3", Some(405.0)), 0);
        assert_eq!(g.remaining("10.0.0.2", Some(706.0)), 0);
    }

    // py: test_logic.TestLoginGuard.test_success_resets
    #[test]
    fn success_resets() {
        let g = LoginGuard::new();
        for i in 0..4 {
            g.record_failure("10.0.0.2", Some(100.0 + i as f64));
        }
        g.record_success("10.0.0.2", None);
        assert_eq!(g.record_failure("10.0.0.2", Some(110.0)), 0);
        assert_eq!(g.attempts_left("10.0.0.2"), 4);
    }

    // py: test_logic.TestLoginGuard.test_global_slow_down_for_unknown_ips
    #[test]
    fn global_slow_down_for_unknown_ips() {
        let g = LoginGuard::new();
        for i in 0..19 {
            assert_eq!(g.record_failure(&format!("10.0.1.{i}"), Some(1000.0 + i as f64)), 0);
        }
        assert_eq!(g.remaining("10.9.9.9", Some(1019.0)), 0);
        g.record_failure("10.0.1.99", Some(1019.0));
        assert_eq!(g.remaining("10.9.9.9", Some(1020.0)), 59);
        assert_eq!(g.remaining("10.9.9.9", Some(1019.0 + 60.0)), 0);
        let (ticket, wait) = g.reserve("10.9.9.9", Some(1080.0));
        assert_eq!(wait, 0);
        g.fail(&ticket.unwrap(), Some(1080.0));
        assert_eq!(g.remaining("10.9.9.8", Some(1081.0)), 59);
    }

    // py: test_logic.TestLoginGuard.test_global_window_expires
    #[test]
    fn global_window_expires() {
        let g = LoginGuard::new();
        for i in 0..20 {
            g.record_failure(&format!("10.0.1.{i}"), Some(1000.0 + i as f64));
        }
        assert!(g.remaining("10.0.2.1", Some(1030.0)) > 0);
        assert_eq!(g.remaining("10.0.2.1", Some(1000.0 + 3700.0)), 0);
        assert_eq!(g.record_failure("10.0.2.1", Some(1000.0 + 3700.0)), 0);
    }
}
