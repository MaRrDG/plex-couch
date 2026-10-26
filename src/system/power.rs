//! Shutdown/restart with our own 60 s countdown (spec 6.2, system.py:27-176).
//!
//! `shutdown /s /hybrid /t 0` or `shutdown /r /t 0`, never `/f` (`/t` above 0 would imply
//! it). One request at a time; it stays pending until 10 s after `at`. Cancel before it
//! fires stops the timer; after it fired, waits for shutdown.exe and runs `shutdown /a`.
//!
//! Implemented in Phase 1 because tests/test_security.py exercises it (TestCancelAfterFire);
//! test_logic.TestPower / TestPowerArgs / TestPowerTimer are ported here too.

use crate::{log_info, log_warning};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const POWER_DELAY: Duration = Duration::from_secs(60);
pub const POWER_GRACE_S: f64 = 10.0;
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

/// `cancel_power()` outcomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The countdown was stopped before shutdown.exe ran.
    Cancelled,
    /// shutdown.exe had already run; `shutdown /a` stopped it.
    Aborted,
    /// shutdown.exe had already run and `shutdown /a` failed.
    TooLate,
    /// Nothing was pending.
    Nothing,
}

impl CancelOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            CancelOutcome::Cancelled => "cancelled",
            CancelOutcome::Aborted => "aborted",
            CancelOutcome::TooLate => "too_late",
            CancelOutcome::Nothing => "nothing",
        }
    }
}

/// A pending countdown as the API shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingPower {
    pub kind: String,
    pub at: i64,
}

impl PendingPower {
    pub fn to_json(&self) -> Value {
        json!({"kind": self.kind, "at": self.at})
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Scheduled {
    id: u64,
    kind: String,
    at: f64,
}

/// `pending_power(state, now, grace=10)`.
pub fn pending_power(kind: &str, at: f64, now: f64) -> Option<PendingPower> {
    if at + POWER_GRACE_S < now {
        return None;
    }
    Some(PendingPower { kind: kind.to_string(), at: at as i64 })
}

/// `%SystemRoot%\System32\shutdown.exe` if it exists, else `shutdown`.
pub fn shutdown_exe() -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
    let exe = std::path::Path::new(&root).join("System32").join("shutdown.exe");
    if exe.exists() {
        exe.to_string_lossy().into_owned()
    } else {
        "shutdown".into()
    }
}

/// The shutdown.exe argv for an immediate shutdown/restart. None for an unknown kind.
pub fn power_args(kind: &str, exe: &str) -> Option<Vec<String>> {
    let flags: &[&str] = match kind {
        "shutdown" => &["/s", "/hybrid"],
        "restart" => &["/r"],
        _ => return None,
    };
    let mut v = vec![exe.to_string()];
    v.extend(flags.iter().map(|s| s.to_string()));
    v.extend(["/t".to_string(), "0".to_string()]);
    Some(v)
}

pub fn abort_args(exe: &str) -> Vec<String> {
    vec![exe.to_string(), "/a".to_string()]
}

/// Runs an argv, returns its exit code (tests replace it).
pub type Runner = Arc<dyn Fn(&[String]) -> std::io::Result<i32> + Send + Sync>;

/// The real runner: hidden window, output captured, 15 s timeout.
pub fn run_hidden(args: &[String]) -> std::io::Result<i32> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn()?;
    let deadline = std::time::Instant::now() + COMMAND_TIMEOUT;
    loop {
        if let Some(st) = child.try_wait()? {
            return Ok(st.code().unwrap_or(-1));
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct State {
    power: Option<Scheduled>,
    /// The scheduled action whose shutdown.exe ran (or is running).
    fired: Option<Scheduled>,
    next_id: u64,
}

struct Inner {
    dry_run: bool,
    delay: Duration,
    exe: Box<dyn Fn() -> String + Send + Sync>,
    runner: Runner,
    state: Mutex<State>,
    /// Held while shutdown.exe runs. `fire` takes it before it releases `state`, so a
    /// cancel that sees the fired state waits for the command to finish before `/a`.
    exec: Mutex<()>,
}

/// Power actions. Cheap to clone (shared state).
#[derive(Clone)]
pub struct Actions {
    inner: Arc<Inner>,
}

fn now() -> f64 {
    crate::auth::session::now()
}

impl Actions {
    pub fn new(dry_run: bool) -> Actions {
        Actions::with(dry_run, POWER_DELAY, Arc::new(run_hidden), Box::new(shutdown_exe))
    }

    /// Everything injectable (tests).
    pub fn with(dry_run: bool, delay: Duration, runner: Runner, exe: Box<dyn Fn() -> String + Send + Sync>) -> Actions {
        Actions {
            inner: Arc::new(Inner {
                dry_run,
                delay,
                exe,
                runner,
                state: Mutex::new(State { power: None, fired: None, next_id: 0 }),
                exec: Mutex::new(()),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn dry(&self) -> &'static str {
        if self.inner.dry_run {
            "dry-run: "
        } else {
            ""
        }
    }

    /// Schedule `shutdown`/`restart` in 60 s. None if one is already pending (or the kind
    /// is unknown).
    pub fn power(&self, kind: &str) -> Option<PendingPower> {
        let args = power_args(kind, &(self.inner.exe)())?;
        let mut g = self.lock();
        if let Some(p) = &g.power {
            if pending_power(&p.kind, p.at, now()).is_some() {
                return None; // one at a time
            }
        }
        g.next_id += 1;
        let sched = Scheduled { id: g.next_id, kind: kind.to_string(), at: now() + self.inner.delay.as_secs_f64() };
        g.power = Some(sched.clone());
        g.fired = None;
        let me = self.clone();
        let delay = self.inner.delay;
        let s2 = sched.clone();
        std::thread::Builder::new()
            .name("power".into())
            .spawn(move || {
                std::thread::sleep(delay);
                me.fire(&s2, &args);
            })
            .ok()?;
        log_info!("{}power {kind} scheduled in {}s", self.dry(), self.inner.delay.as_secs());
        pending_power(&sched.kind, sched.at, now())
    }

    fn fire(&self, scheduled: &Scheduled, args: &[String]) {
        let g = self.lock();
        if g.power.as_ref().map(|p| p.id) != Some(scheduled.id) {
            return; // cancelled or replaced
        }
        let mut g = g;
        g.fired = Some(scheduled.clone());
        let exec = self.inner.exec.lock().unwrap_or_else(|e| e.into_inner());
        drop(g);
        let kind = &scheduled.kind;
        if self.inner.dry_run {
            log_info!("dry-run: power {kind} (would run shutdown {})", args[1..].join(" "));
            drop(exec);
            return;
        }
        log_info!("power {kind} now");
        let code = match (self.inner.runner)(args) {
            Ok(c) => c,
            Err(e) => {
                log_warning!("shutdown {kind} failed: {}", crate::state::io_error_name(&e));
                -1
            }
        };
        if code != 0 {
            log_warning!("shutdown {kind} failed with code {code}");
            let mut g = self.lock();
            if g.power.as_ref().map(|p| p.id) == Some(scheduled.id) {
                g.power = None;
            }
            if g.fired.as_ref().map(|p| p.id) == Some(scheduled.id) {
                g.fired = None;
            }
        }
        drop(exec);
    }

    /// Cancel the pending shutdown/restart (blocking: may wait for shutdown.exe and run `/a`).
    pub fn cancel_power(&self) -> CancelOutcome {
        let (fired, pending) = {
            let mut g = self.lock();
            let fired = g.fired.take();
            let pending = g.power.take().is_some();
            (fired, pending)
        };
        let Some(fired) = fired else {
            log_info!("{}power cancelled", self.dry());
            return if pending { CancelOutcome::Cancelled } else { CancelOutcome::Nothing };
        };
        let _exec = self.inner.exec.lock().unwrap_or_else(|e| e.into_inner());
        self.abort(&fired.kind)
    }

    fn abort(&self, kind: &str) -> CancelOutcome {
        if self.inner.dry_run {
            log_info!("dry-run: power {kind} already started, would run shutdown /a");
            return CancelOutcome::Aborted;
        }
        let code = match (self.inner.runner)(&abort_args(&(self.inner.exe)())) {
            Ok(c) => c,
            Err(e) => {
                log_warning!("shutdown /a failed: {}", crate::state::io_error_name(&e));
                -1
            }
        };
        if code == 0 {
            log_info!("power {kind} aborted with shutdown /a");
            return CancelOutcome::Aborted;
        }
        log_warning!("power {kind} could not be aborted (shutdown /a code {code})");
        CancelOutcome::TooLate
    }

    /// The pending countdown, or None (an expired one is cleared).
    pub fn pending(&self) -> Option<PendingPower> {
        let mut g = self.lock();
        let p = g.power.as_ref().and_then(|p| pending_power(&p.kind, p.at, now()));
        if p.is_none() {
            g.power = None;
        }
        p
    }

    /// True once the timer fired (tests wait for it).
    pub fn has_fired(&self) -> bool {
        self.lock().fired.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Instant;

    struct Fake {
        calls: Arc<Mutex<Vec<Vec<String>>>>,
        codes: Arc<Mutex<HashMap<String, i32>>>,
        delay: Arc<Mutex<Duration>>,
    }

    fn fake() -> (Fake, Runner) {
        let f = Fake {
            calls: Arc::new(Mutex::new(Vec::new())),
            codes: Arc::new(Mutex::new([("/a".to_string(), 0)].into_iter().collect())),
            delay: Arc::new(Mutex::new(Duration::ZERO)),
        };
        let (calls, codes, delay) = (f.calls.clone(), f.codes.clone(), f.delay.clone());
        let runner: Runner = Arc::new(move |args: &[String]| {
            calls.lock().unwrap().push(args.to_vec());
            let d = *delay.lock().unwrap();
            if !args.iter().any(|a| a == "/a") && !d.is_zero() {
                std::thread::sleep(d);
            }
            Ok(*codes.lock().unwrap().get(&args[1]).unwrap_or(&0))
        });
        (f, runner)
    }

    fn acts(dry_run: bool, delay: Duration, runner: Runner) -> Actions {
        Actions::with(dry_run, delay, runner, Box::new(|| "shutdown-mock.exe".to_string()))
    }

    fn wait_fired(a: &Actions) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if a.has_fired() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timer did not fire");
    }

    const FAST: Duration = Duration::from_millis(50);

    // py: test_security.TestCancelAfterFire.test_cancel_before_fire
    #[test]
    fn cancel_before_fire() {
        let (f, r) = fake();
        let a = acts(false, Duration::from_secs(5), r);
        a.power("shutdown").unwrap();
        assert_eq!(a.cancel_power(), CancelOutcome::Cancelled);
        assert!(f.calls.lock().unwrap().is_empty());
        assert_eq!(a.cancel_power(), CancelOutcome::Nothing);
    }

    // py: test_security.TestCancelAfterFire.test_cancel_after_fire_runs_abort
    #[test]
    fn cancel_after_fire_runs_abort() {
        let (f, r) = fake();
        let a = acts(false, FAST, r);
        a.power("shutdown").unwrap();
        wait_fired(&a);
        std::thread::sleep(FAST);
        assert_eq!(a.cancel_power(), CancelOutcome::Aborted);
        let want: Vec<Vec<String>> = vec![
            ["shutdown-mock.exe", "/s", "/hybrid", "/t", "0"].iter().map(|s| s.to_string()).collect(),
            ["shutdown-mock.exe", "/a"].iter().map(|s| s.to_string()).collect(),
        ];
        assert_eq!(*f.calls.lock().unwrap(), want);
        assert!(a.pending().is_none());
    }

    // py: test_security.TestCancelAfterFire.test_cancel_too_late
    #[test]
    fn cancel_too_late() {
        let (f, r) = fake();
        f.codes.lock().unwrap().insert("/a".into(), 1116);
        let a = acts(false, FAST, r);
        a.power("restart").unwrap();
        wait_fired(&a);
        assert_eq!(a.cancel_power(), CancelOutcome::TooLate);
        assert_eq!(f.calls.lock().unwrap().last().unwrap(), &vec!["shutdown-mock.exe".to_string(), "/a".to_string()]);
    }

    // py: test_security.TestCancelAfterFire.test_cancel_while_firing_waits_then_aborts
    #[test]
    fn cancel_while_firing_waits_then_aborts() {
        let (f, r) = fake();
        *f.delay.lock().unwrap() = Duration::from_millis(300);
        let a = acts(false, FAST, r);
        a.power("shutdown").unwrap();
        wait_fired(&a);
        assert_eq!(a.cancel_power(), CancelOutcome::Aborted);
        let flags: Vec<String> = f.calls.lock().unwrap().iter().map(|c| c[1].clone()).collect();
        assert_eq!(flags, vec!["/s".to_string(), "/a".to_string()]);
    }

    // py: test_security.TestCancelAfterFire.test_dry_run_after_fire_only_logs
    #[test]
    fn dry_run_after_fire_only_logs() {
        let (f, r) = fake();
        let a = acts(true, FAST, r);
        a.power("shutdown").unwrap();
        wait_fired(&a);
        assert_eq!(a.cancel_power(), CancelOutcome::Aborted);
        assert!(f.calls.lock().unwrap().is_empty());
    }

    // py: test_security.TestCancelAfterFire.test_abort_args
    #[test]
    fn abort_args_shape() {
        assert_eq!(abort_args("shutdown.exe"), vec!["shutdown.exe".to_string(), "/a".to_string()]);
    }

    #[test]
    fn one_at_a_time_and_args() {
        let (_f, r) = fake();
        let a = acts(true, Duration::from_secs(5), r);
        assert!(a.power("shutdown").is_some());
        assert!(a.power("restart").is_none());
        assert!(a.power("nope").is_none());
        assert_eq!(power_args("restart", "x").unwrap(), vec!["x", "/r", "/t", "0"]);
        assert_eq!(pending_power("shutdown", 1060.4, 1000.0), Some(PendingPower { kind: "shutdown".into(), at: 1060 }));
        assert_eq!(pending_power("shutdown", 1060.0, 1100.0), None);
        a.cancel_power();
    }

    // py: test_logic.TestPower.test_pending
    #[test]
    fn pending() {
        // pending_power(None) is None: an Actions with nothing scheduled
        let (_f, r) = fake();
        assert!(acts(true, Duration::from_secs(5), r).pending().is_none());
        assert_eq!(pending_power("shutdown", 1060.4, 1000.0), Some(PendingPower { kind: "shutdown".into(), at: 1060 }));
        assert_eq!(pending_power("shutdown", 1060.4, 1000.0).unwrap().to_json(), serde_json::json!({"kind": "shutdown", "at": 1060}));
        assert_eq!(pending_power("shutdown", 1060.0, 1100.0), None);
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // py: test_logic.TestPowerArgs.test_shutdown_is_hybrid_and_immediate
    #[test]
    fn shutdown_is_hybrid_and_immediate() {
        assert_eq!(power_args("shutdown", "shutdown.exe").unwrap(), strs(&["shutdown.exe", "/s", "/hybrid", "/t", "0"]));
    }

    // py: test_logic.TestPowerArgs.test_restart_has_no_hybrid
    #[test]
    fn restart_has_no_hybrid() {
        assert_eq!(power_args("restart", "shutdown.exe").unwrap(), strs(&["shutdown.exe", "/r", "/t", "0"]));
    }

    // py: test_logic.TestPowerArgs.test_never_forced
    #[test]
    fn never_forced() {
        // /t above 0 implies /f; /t 0 lets unsaved work stop it, like Start > Shut down
        for kind in ["shutdown", "restart"] {
            assert!(!power_args(kind, "shutdown.exe").unwrap().contains(&"/f".to_string()));
        }
    }

    // py: test_logic.TestPowerArgs.test_unknown_kind_raises
    #[test]
    fn unknown_kind_raises() {
        // Python raises KeyError; here the kind is refused (None) and nothing is scheduled
        assert_eq!(power_args("hibernate", "shutdown.exe"), None);
        let (f, r) = fake();
        let a = acts(false, FAST, r);
        assert!(a.power("hibernate").is_none());
        assert!(a.pending().is_none());
        std::thread::sleep(FAST * 3);
        assert!(f.calls.lock().unwrap().is_empty());
    }

    // py: test_logic.TestPowerTimer.test_cancel_stops_it
    #[test]
    fn cancel_stops_it() {
        let (f, r) = fake();
        let a = acts(true, Duration::from_millis(200), r);
        assert!(a.power("shutdown").is_some());
        assert!(a.power("restart").is_none()); // one at a time
        a.cancel_power();
        std::thread::sleep(Duration::from_millis(400));
        assert!(a.pending().is_none());
        assert!(!a.has_fired());
        assert!(f.calls.lock().unwrap().is_empty());
    }

    // py: test_logic.TestPowerTimer.test_dry_run_never_executes
    #[test]
    fn dry_run_never_executes() {
        let (f, r) = fake();
        let a = acts(true, Duration::from_millis(200), r);
        a.power("shutdown");
        std::thread::sleep(Duration::from_millis(400));
        assert!(a.has_fired());
        assert!(f.calls.lock().unwrap().is_empty());
    }
}
