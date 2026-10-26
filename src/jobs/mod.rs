//! Background jobs, started once at startup by [`spawn_all`] (never in demo).
//!
//! - `auto-monitor` loop (Phase 2a): `logic::power::spawn_auto_monitor`.
//! - `notify` loop (Phase 2a): `logic::notify::spawn_notify_loop`.
//! - [`watchlist`] sync every 3 min, [`strip_dv`] every 5 min, [`seed_keeper`] every 5 min
//!   (Phase 2b; plan "New behaviour").
//!
//! Rules for every job: honour `app.dry_run` (never delete, import, remux or rescan in
//! dry-run; only log what would happen), use the service URLs from the config (tests run
//! the exe with every service on a closed port), and stop when [`Stop`] fires.

pub mod common;
pub mod seed_keeper;
pub mod strip_dv;
pub mod watchlist;
#[cfg(test)]
pub mod testutil;

use crate::app::AppState;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// Shutdown signal for the loops (Python's `stop` Event). Clone one per job.
#[derive(Clone)]
pub struct Stop {
    rx: watch::Receiver<bool>,
}

/// The sending side, kept by `cli` to stop everything.
pub struct StopHandle {
    tx: watch::Sender<bool>,
}

pub fn stop_pair() -> (StopHandle, Stop) {
    let (tx, rx) = watch::channel(false);
    (StopHandle { tx }, Stop { rx })
}

impl StopHandle {
    pub fn stop(&self) {
        let _ = self.tx.send(true);
    }
}

impl Stop {
    pub fn is_stopped(&self) -> bool {
        *self.rx.borrow()
    }

    /// `stop.wait(timeout)`: sleep up to `d`; true if the app is stopping.
    pub async fn sleep(&mut self, d: Duration) -> bool {
        if self.is_stopped() {
            return true;
        }
        let woke = tokio::select! {
            _ = tokio::time::sleep(d) => false,
            r = self.rx.wait_for(|v| *v) => r.is_ok(),
        };
        woke || self.is_stopped()
    }
}

/// Start every background job. Nothing runs in demo mode.
pub fn spawn_all(app: Arc<AppState>, stop: Stop) {
    if app.demo {
        return;
    }
    crate::logic::power::spawn_auto_monitor(app.clone(), stop.clone());
    crate::logic::notify::spawn_notify_loop(app.clone(), stop.clone());
    watchlist::spawn(app.clone(), stop.clone());
    strip_dv::spawn(app.clone(), stop.clone());
    seed_keeper::spawn(app, stop);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stop_wakes_sleepers() {
        let (h, mut s) = stop_pair();
        assert!(!s.sleep(Duration::from_millis(1)).await);
        let mut s2 = s.clone();
        let t = tokio::spawn(async move { s2.sleep(Duration::from_secs(30)).await });
        h.stop();
        assert!(t.await.unwrap());
        assert!(s.sleep(Duration::from_secs(30)).await);
    }
}
