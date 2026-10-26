//! Monitors off/on (spec 6.1). Must run in the interactive user session.
//!
//! Done (Phase 1): the dry-run path. TODO(Phase 2a): the real calls:
//! - off: `PostMessageW(HWND_BROADCAST=0xFFFF, WM_SYSCOMMAND=0x0112, SC_MONITORPOWER=0xF170, 2)`
//!   (PostMessage, never SendMessage), returns its BOOL; log `monitors off (<reason>)`.
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
    // TODO(Phase 2a): PostMessageW(...)
    false
}

pub fn monitors_on(dry_run: bool) -> bool {
    if dry_run {
        log_info!("dry-run: monitors on");
        return true;
    }
    // TODO(Phase 2a): SendInput + SetThreadExecutionState
    false
}
