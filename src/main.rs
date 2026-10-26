// No console window when started by Task Scheduler (pythonw equivalent).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    attach_console();
    let args: Vec<String> = std::env::args_os().map(|a| a.to_string_lossy().into_owned()).collect();
    std::process::exit(plex_couch::cli::main(args));
}

/// A GUI-subsystem exe has no console: when started from one (`--set-pin`, `make-certs`),
/// write to the parent's console. Without a parent console (Task Scheduler) this is a no-op.
#[cfg(all(windows, not(debug_assertions)))]
fn attach_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    // SAFETY: plain Win32 call; failure (no parent console) is fine.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(all(windows, not(debug_assertions))))]
fn attach_console() {}
