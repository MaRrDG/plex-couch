//! Monitors off/on (spec 6.1). Must run in the interactive user session.
//!
//! - off: `PostMessageW(HWND_BROADCAST, WM_SYSCOMMAND, SC_MONITORPOWER, 2)` (PostMessage,
//!   never SendMessage, which can hang), returns its BOOL; log `monitors off (<reason>)`.
//! - on: `SendInput` two relative mouse moves (+1, -1), then
//!   `SetThreadExecutionState(ES_DISPLAY_REQUIRED)`; ok = SendInput returned 2; log
//!   `monitors on`.

use crate::log_info;

pub const HWND_BROADCAST: isize = 0xFFFF;
pub const WM_SYSCOMMAND: u32 = 0x0112;
pub const SC_MONITORPOWER: usize = 0xF170;
pub const MONITOR_OFF: isize = 2;

/// `reason` is `manual` or `auto: film pornit pe TV`.
pub fn monitors_off(dry_run: bool, reason: &str) -> bool {
    if dry_run {
        log_info!("dry-run: monitors off ({reason})");
        return true;
    }
    log_info!("monitors off ({reason})");
    post_monitor_off()
}

pub fn monitors_on(dry_run: bool) -> bool {
    if dry_run {
        log_info!("dry-run: monitors on");
        return true;
    }
    log_info!("monitors on");
    let sent = mouse_nudge();
    keep_display_on();
    sent == 2
}

#[cfg(windows)]
fn post_monitor_off() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;
    // SAFETY: a broadcast PostMessage takes no pointers.
    unsafe { PostMessageW(HWND_BROADCAST as _, WM_SYSCOMMAND, SC_MONITORPOWER, MONITOR_OFF) != 0 }
}

/// A 1-pixel relative mouse move and back; wakes the displays. Returns the events sent.
#[cfg(windows)]
fn mouse_nudge() -> u32 {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT};
    let ev = |dx: i32| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy: 0, mouseData: 0, dwFlags: MOUSEEVENTF_MOVE, time: 0, dwExtraInfo: 0 } },
    };
    let inputs = [ev(1), ev(-1)];
    // SAFETY: two initialised INPUT records of the size passed.
    unsafe { SendInput(2, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) }
}

#[cfg(windows)]
fn keep_display_on() {
    use windows_sys::Win32::System::Power::{SetThreadExecutionState, ES_DISPLAY_REQUIRED};
    // SAFETY: flag-only call.
    unsafe { SetThreadExecutionState(ES_DISPLAY_REQUIRED) };
}

#[cfg(not(windows))]
fn post_monitor_off() -> bool {
    false
}

#[cfg(not(windows))]
fn mouse_nudge() -> u32 {
    0
}

#[cfg(not(windows))]
fn keep_display_on() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_only_logs() {
        crate::log::start_capture();
        assert!(monitors_off(true, "manual-test-reason"));
        assert!(monitors_on(true));
        let out = crate::log::captured().concat();
        assert!(out.contains("dry-run: monitors off (manual-test-reason)"));
        assert!(out.contains("dry-run: monitors on"));
    }
}
