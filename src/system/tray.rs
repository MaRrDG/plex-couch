//! The notification-area (system tray) icon: tooltip `app_name`, a left click or double
//! click opens the app in the default browser, the right-click menu has "Deschide …" and
//! "Ieșire" (the same graceful stop as Ctrl+C).
//!
//! - [`OpenTarget::url`]: the address to open, worked out on every click (the LAN IP can
//!   change): `https://<ip>:<https port>/` with the server certificate's private IPv4 that
//!   this PC still has, else this PC's private IPv4 (the primary interface first), else
//!   127.0.0.1; `http://<ip>:<port>/` when HTTPS is off. A specific `--host` wins.
//! - [`start`] (Windows): a dedicated thread with a hidden top-level window (not a
//!   message-only one: those miss the "TaskbarCreated" broadcast that says Explorer
//!   restarted and the icon has to be added again) running `Shell_NotifyIconW`.
//!   [`Tray::stop`] removes the icon (no ghost icon) and ends the thread.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The icon, embedded (the same file build.rs puts into the exe's resources).
pub const ICON: &[u8] = include_bytes!("../../assets/plex-couch.ico");

/// A click within this long of the previous open does not open another tab (a double
/// click is a click, a double-click message and another click).
pub const DEBOUNCE: Duration = Duration::from_millis(1500);

/// Where the app is served, for the "open" action.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenTarget {
    /// HTTPS (`port` is the HTTPS port) or plain HTTP.
    pub secure: bool,
    pub port: u16,
    /// `--host`: a specific address is used as is, `0.0.0.0` means "find the LAN IP".
    pub bind_host: String,
    /// The server certificate (PEM), whose IP SANs are preferred.
    pub cert: Option<PathBuf>,
}

impl OpenTarget {
    /// The URL to open now.
    pub fn url(&self) -> String {
        let cert_ips = match (&self.cert, self.secure) {
            (Some(p), true) => std::fs::read_to_string(p).map(|pem| cert_ipv4s(&pem)).unwrap_or_default(),
            _ => Vec::new(),
        };
        let host = pick_host(&self.bind_host, &cert_ips, &own_ipv4s_ordered());
        open_url(self.secure, &host, self.port)
    }
}

/// `https://host:port/` or `http://host:port/` (IPv6 hosts in brackets).
pub fn open_url(secure: bool, host: &str, port: u16) -> String {
    let scheme = if secure { "https" } else { "http" };
    if host.contains(':') {
        format!("{scheme}://[{host}]:{port}/")
    } else {
        format!("{scheme}://{host}:{port}/")
    }
}

fn is_wildcard(host: &str) -> bool {
    matches!(host.trim(), "" | "0.0.0.0" | "::" | "[::]")
}

/// The host for the URL: a specific bind address; else the first private IPv4 of the
/// certificate that this PC has (any, when this PC's addresses are unknown); else this
/// PC's first private IPv4; else 127.0.0.1.
pub fn pick_host(bind_host: &str, cert_ips: &[Ipv4Addr], own: &[Ipv4Addr]) -> String {
    if !is_wildcard(bind_host) {
        return bind_host.trim().trim_start_matches('[').trim_end_matches(']').to_string();
    }
    let from_cert = cert_ips.iter().find(|ip| ip.is_private() && (own.is_empty() || own.contains(ip)));
    from_cert
        .or_else(|| own.iter().find(|ip| ip.is_private()))
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "127.0.0.1".into())
}

/// The IPv4 subject alternative names of the first certificate in `pem` (empty if it
/// cannot be read).
pub fn cert_ipv4s(pem: &str) -> Vec<Ipv4Addr> {
    let Ok(params) = rcgen::CertificateParams::from_ca_cert_pem(pem) else {
        return Vec::new();
    };
    params
        .subject_alt_names
        .into_iter()
        .filter_map(|san| match san {
            rcgen::SanType::IpAddress(IpAddr::V4(v4)) => Some(v4),
            _ => None,
        })
        .collect()
}

/// This PC's IPv4s: the primary interface first, then the rest in address order.
pub fn own_ipv4s_ordered() -> Vec<Ipv4Addr> {
    let primary = super::info::primary_ipv4();
    let mut rest: Vec<Ipv4Addr> =
        super::info::own_ipv4s().iter().filter_map(|s| s.parse().ok()).filter(|ip| Some(*ip) != primary).collect();
    rest.sort();
    primary.into_iter().chain(rest).collect()
}

/// Whether a click at `now` opens the browser, given the last open.
pub fn should_open(last_open: Option<Instant>, now: Instant) -> bool {
    last_open.is_none_or(|t| now.saturating_duration_since(t) >= DEBOUNCE)
}

/// `(offset, length)` of the image in an `.ico` file that suits a `want` pixel icon: the
/// smallest one at least that big, else the biggest.
pub fn pick_icon_image(ico: &[u8], want: u32) -> Option<(usize, usize)> {
    if ico.len() < 6 || ico[0..4] != [0, 0, 1, 0] {
        return None;
    }
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    let mut images = Vec::new();
    for n in 0..count {
        let e = ico.get(6 + 16 * n..6 + 16 * (n + 1))?;
        let size = if e[0] == 0 { 256 } else { e[0] as u32 };
        let len = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as usize;
        let off = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as usize;
        if off.checked_add(len).is_some_and(|end| end <= ico.len()) {
            images.push((size, off, len));
        }
    }
    let fits = images.iter().filter(|i| i.0 >= want).min_by_key(|i| i.0);
    fits.or_else(|| images.iter().max_by_key(|i| i.0)).map(|&(_, off, len)| (off, len))
}

/// What the tray does: the tooltip and menu name, the URL to open and what "Ieșire" does.
pub struct TrayOptions {
    pub app_name: String,
    pub url: Box<dyn Fn() -> String + Send>,
    pub on_exit: Box<dyn Fn() + Send>,
}

#[cfg(windows)]
pub use win::{start, Tray};

#[cfg(not(windows))]
pub struct Tray;

#[cfg(not(windows))]
pub fn start(_opts: TrayOptions) -> Result<Tray, String> {
    Err("the tray icon is Windows only".into())
}

#[cfg(not(windows))]
impl Tray {
    pub fn stop(self) {}
}

#[cfg(windows)]
mod win {
    use super::{pick_icon_image, should_open, TrayOptions, ICON};
    use crate::{log_info, log_warning};
    use std::cell::RefCell;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::ptr::{null, null_mut};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, ChangeWindowMessageFilterEx, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
        DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, KillTimer,
        PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, SetMenuDefaultItem,
        SetTimer, TrackPopupMenu, HICON, LR_DEFAULTCOLOR, MF_SEPARATOR, MF_STRING, MSG, MSGFLT_ALLOW, SM_CXSMICON,
        SW_SHOWNORMAL, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY,
        WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
    };

    const WM_TRAY: u32 = WM_APP + 1;
    /// Tests only: run the "Ieșire" action without showing the menu.
    #[cfg(test)]
    const WM_TEST_EXIT: u32 = WM_APP + 2;
    const ICON_ID: u32 = 1;
    const ID_OPEN: usize = 1;
    const ID_EXIT: usize = 2;
    const RETRY_TIMER: usize = 1;
    /// At log-on the taskbar may not exist yet: retry adding every 3 s for 5 minutes (and
    /// "TaskbarCreated" adds it whenever Explorer comes up).
    const RETRY_MS: u32 = 3000;
    const RETRY_MAX: u32 = 100;
    const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    struct State {
        hwnd: HWND,
        icon: HICON,
        tip: String,
        open_label: Vec<u16>,
        url: Box<dyn Fn() -> String>,
        on_exit: Box<dyn Fn()>,
        taskbar_created: u32,
        last_open: Option<Instant>,
        added: bool,
        retries: u32,
    }

    thread_local! {
        static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    }

    /// Run `f` on the state without holding the borrow across anything that pumps messages
    /// (menus, ShellExecute, DestroyWindow re-enter the window procedure).
    fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
        STATE.with(|s| s.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
    }

    fn notify_data(s: &State) -> NOTIFYICONDATAW {
        let mut nid = NOTIFYICONDATAW { cbSize: size_of::<NOTIFYICONDATAW>() as u32, ..Default::default() };
        nid.hWnd = s.hwnd;
        nid.uID = ICON_ID;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = s.icon;
        for (d, c) in nid.szTip.iter_mut().zip(s.tip.encode_utf16().take(127)) {
            *d = c;
        }
        nid
    }

    /// Add the icon (or update it if Explorer still has it); false if the taskbar is not there.
    fn add_icon(s: &mut State) -> bool {
        let nid = notify_data(s);
        // SAFETY: nid is fully initialised and cbSize is set.
        let ok = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) != 0 || Shell_NotifyIconW(NIM_MODIFY, &nid) != 0 };
        if ok && !s.added {
            log_info!("tray icon added");
        }
        s.added = ok;
        ok
    }

    fn delete_icon(s: &mut State) {
        if s.added {
            let nid = notify_data(s);
            // SAFETY: as above.
            unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
            s.added = false;
            log_info!("tray icon removed");
        }
    }

    fn add_or_retry(s: &mut State) {
        if !add_icon(s) {
            s.retries = 0;
            // SAFETY: our window; a timer id of our own.
            unsafe { SetTimer(s.hwnd, RETRY_TIMER, RETRY_MS, None) };
        }
    }

    fn open(hwnd: HWND) {
        let Some(url) = with_state(|s| {
            let now = Instant::now();
            if !should_open(s.last_open, now) {
                return None;
            }
            s.last_open = Some(now);
            Some((s.url)())
        })
        .flatten() else {
            return;
        };
        let (verb, file) = (wide("open"), wide(&url));
        // SAFETY: NUL-terminated strings; the result is an HINSTANCE-typed status code.
        let r = unsafe { ShellExecuteW(hwnd, verb.as_ptr(), file.as_ptr(), null(), null(), SW_SHOWNORMAL) } as isize;
        if r > 32 {
            log_info!("tray: opened {url}");
        } else {
            log_warning!("tray: cannot open {url} (ShellExecute error {r})");
        }
    }

    fn exit(hwnd: HWND) {
        log_info!("tray: exit chosen");
        with_state(|s| (s.on_exit)());
        // SAFETY: our window, on its own thread; WM_DESTROY removes the icon.
        unsafe { DestroyWindow(hwnd) };
    }

    fn menu(hwnd: HWND) {
        let Some(label) = with_state(|s| s.open_label.clone()) else { return };
        let exit_label = wide("Ieșire");
        // SAFETY: a popup menu owned and destroyed here; strings outlive the calls.
        let cmd = unsafe {
            let m = CreatePopupMenu();
            if m.is_null() {
                return;
            }
            AppendMenuW(m, MF_STRING, ID_OPEN, label.as_ptr());
            AppendMenuW(m, MF_SEPARATOR, 0, null());
            AppendMenuW(m, MF_STRING, ID_EXIT, exit_label.as_ptr());
            SetMenuDefaultItem(m, ID_OPEN as u32, 0);
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            // the menu closes on a click elsewhere only if our window is in front (KB135788)
            SetForegroundWindow(hwnd);
            let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, pt.x, pt.y, 0, hwnd, null());
            PostMessageW(hwnd, WM_NULL, 0, 0);
            DestroyMenu(m);
            cmd as usize
        };
        match cmd {
            ID_OPEN => open(hwnd),
            ID_EXIT => exit(hwnd),
            _ => {}
        }
    }

    fn handle(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        if msg == WM_TRAY {
            match lparam as u32 & 0xFFFF {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => open(hwnd),
                WM_RBUTTONUP | WM_CONTEXTMENU => menu(hwnd),
                _ => {}
            }
            return Some(0);
        }
        #[cfg(test)]
        if msg == WM_TEST_EXIT {
            exit(hwnd);
            return Some(0);
        }
        if with_state(|s| msg == s.taskbar_created) == Some(true) {
            log_info!("tray: Explorer restarted, adding the icon again");
            with_state(|s| {
                s.added = false;
                add_or_retry(s)
            });
            return Some(0);
        }
        match msg {
            WM_TIMER if wparam == RETRY_TIMER => {
                with_state(|s| {
                    s.retries += 1;
                    if add_icon(s) || s.retries >= RETRY_MAX {
                        // SAFETY: our window and timer.
                        unsafe { KillTimer(hwnd, RETRY_TIMER) };
                        if !s.added {
                            log_warning!("tray: the taskbar is not there, no tray icon until Explorer starts");
                        }
                    }
                });
                Some(0)
            }
            WM_CLOSE => {
                // SAFETY: our window, on its own thread.
                unsafe { DestroyWindow(hwnd) };
                Some(0)
            }
            WM_DESTROY => {
                with_state(delete_icon);
                // SAFETY: ends this thread's message loop.
                unsafe { PostQuitMessage(0) };
                Some(0)
            }
            _ => None,
        }
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // A panic must not unwind into Windows (that aborts the whole server).
        match catch_unwind(AssertUnwindSafe(|| handle(hwnd, msg, wparam, lparam))) {
            Ok(Some(r)) => r,
            // SAFETY: default handling of a message for our window.
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    /// The embedded icon at the small-icon size of this (DPI-aware) thread.
    fn load_icon() -> HICON {
        // SAFETY: plain metric query.
        let want = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
        let Some((off, len)) = pick_icon_image(ICON, want as u32) else { return null_mut() };
        let bits = &ICON[off..off + len];
        // SAFETY: bits is one icon image (DIB or PNG) from the .ico, len bytes long.
        unsafe { CreateIconFromResourceEx(bits.as_ptr(), len as u32, 1, 0x0003_0000, want, want, LR_DEFAULTCOLOR) }
    }

    /// The running tray icon: [`Tray::stop`] removes it.
    pub struct Tray {
        hwnd: isize,
        done: mpsc::Receiver<()>,
    }

    impl Tray {
        /// Remove the icon and end the tray thread (waits up to 2 s).
        pub fn stop(self) {
            // SAFETY: PostMessage is thread-safe; a window that is already gone just fails.
            unsafe { PostMessageW(self.hwnd as HWND, WM_CLOSE, 0, 0) };
            let _ = self.done.recv_timeout(Duration::from_secs(2));
        }

        /// Tests only: choose "Ieșire"; true when the tray thread ended.
        #[cfg(test)]
        pub fn test_exit(&self) -> bool {
            // SAFETY: as in stop.
            unsafe { PostMessageW(self.hwnd as HWND, WM_TEST_EXIT, 0, 0) };
            self.done.recv_timeout(Duration::from_secs(2)).is_ok()
        }
    }

    /// Start the tray thread; Err if the window cannot be made. The icon itself may come a
    /// bit later when the taskbar is not up yet.
    pub fn start(opts: TrayOptions) -> Result<Tray, String> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<isize, String>>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        std::thread::Builder::new()
            .name("tray".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                run(opts, ready_tx);
                let _ = done_tx.send(());
            })
            .map_err(|e| format!("cannot start the tray thread: {e}"))?;
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(hwnd)) => Ok(Tray { hwnd, done: done_rx }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("the tray thread did not start".into()),
        }
    }

    fn run(opts: TrayOptions, ready: mpsc::Sender<Result<isize, String>>) {
        // SAFETY: plain Win32 calls on this thread; every pointer is valid for the call.
        unsafe {
            // crisp icon and menu at any scaling, for this thread only
            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            // ShellExecute may use COM
            let com = CoInitializeEx(null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32) >= 0;
            let hinst = GetModuleHandleW(null());
            let class = wide("PlexCouchTray");
            let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst, lpszClassName: class.as_ptr(), ..Default::default() };
            if RegisterClassW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                let _ = ready.send(Err(format!("RegisterClassW failed ({})", GetLastError())));
                return;
            }
            let title = wide(&opts.app_name);
            // hidden top-level window: message-only windows do not get "TaskbarCreated"
            let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW, class.as_ptr(), title.as_ptr(), WS_OVERLAPPED, 0, 0, 0, 0,
                                       null_mut(), null_mut(), hinst, null());
            if hwnd.is_null() {
                let _ = ready.send(Err(format!("CreateWindowExW failed ({})", GetLastError())));
                return;
            }
            let taskbar_created = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
            // when the app runs elevated, let Explorer's messages through
            ChangeWindowMessageFilterEx(hwnd, taskbar_created, MSGFLT_ALLOW, null_mut());
            ChangeWindowMessageFilterEx(hwnd, WM_TRAY, MSGFLT_ALLOW, null_mut());
            let icon = load_icon();
            if icon.is_null() {
                log_warning!("tray: cannot load the icon ({})", GetLastError());
            }
            STATE.with(|s| {
                *s.borrow_mut() = Some(State {
                    hwnd,
                    icon,
                    tip: opts.app_name.clone(),
                    open_label: wide(&format!("Deschide {}", opts.app_name)),
                    url: opts.url,
                    on_exit: opts.on_exit,
                    taskbar_created,
                    last_open: None,
                    added: false,
                    retries: 0,
                })
            });
            with_state(add_or_retry);
            let _ = ready.send(Ok(hwnd as isize));
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                DispatchMessageW(&msg);
            }
            if let Ok(Some(mut s)) = STATE.with(|s| s.try_borrow_mut().map(|mut g| g.take())) {
                delete_icon(&mut s);
            }
            if !icon.is_null() {
                DestroyIcon(icon);
            }
            if com {
                CoUninitialize();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    #[test]
    fn urls() {
        assert_eq!(open_url(true, "192.168.1.50", 8766), "https://192.168.1.50:8766/");
        assert_eq!(open_url(false, "127.0.0.1", 8765), "http://127.0.0.1:8765/");
        assert_eq!(open_url(true, "::1", 8766), "https://[::1]:8766/");
    }

    #[test]
    fn host_choice() {
        let own = [ip("172.20.0.1"), ip("192.168.1.50"), ip("10.0.0.7")];
        // the certificate's IP wins when this PC has it
        assert_eq!(pick_host("0.0.0.0", &[ip("8.8.8.8"), ip("192.168.1.50")], &own), "192.168.1.50");
        // a certificate IP this PC no longer has is skipped
        assert_eq!(pick_host("0.0.0.0", &[ip("192.168.1.99")], &own), "172.20.0.1");
        // no certificate: the first private own IP (the primary interface comes first)
        assert_eq!(pick_host("", &[], &[ip("100.64.0.1"), ip("192.168.1.50")]), "192.168.1.50");
        // own IPs unknown: the certificate's private IP
        assert_eq!(pick_host("0.0.0.0", &[ip("192.168.1.99")], &[]), "192.168.1.99");
        // nothing private: localhost
        assert_eq!(pick_host("0.0.0.0", &[ip("8.8.8.8")], &[ip("100.64.0.1")]), "127.0.0.1");
        assert_eq!(pick_host("::", &[], &[]), "127.0.0.1");
        // a specific --host is used as is (--demo is 127.0.0.1)
        assert_eq!(pick_host("127.0.0.1", &[ip("192.168.1.50")], &own), "127.0.0.1");
        assert_eq!(pick_host("[::1]", &[], &own), "::1");
    }

    #[test]
    fn certificate_sans() {
        let key = rcgen::KeyPair::generate().unwrap();
        let params = crate::certs::server_params(&["192.168.1.50".into(), "127.0.0.1".into()], &["localhost".into()], &key).unwrap();
        let pem = params.self_signed(&key).unwrap().pem();
        assert_eq!(cert_ipv4s(&pem), vec![ip("192.168.1.50"), ip("127.0.0.1")]);
        assert!(cert_ipv4s("not a certificate").is_empty());

        let d = std::env::temp_dir().join(format!("pc-tray-{}", crate::config::token_hex(6)));
        std::fs::create_dir_all(&d).unwrap();
        let cert = d.join("server.pem");
        std::fs::write(&cert, &pem).unwrap();
        let t = OpenTarget { secure: true, port: 8766, bind_host: "127.0.0.1".into(), cert: Some(cert.clone()) };
        assert_eq!(t.url(), "https://127.0.0.1:8766/");
        let t = OpenTarget { secure: false, port: 8765, bind_host: "0.0.0.0".into(), cert: Some(cert) };
        let url = t.url();
        assert!(url.starts_with("http://") && url.ends_with(":8765/"), "{url}");
        let t = OpenTarget { secure: true, port: 8766, bind_host: "0.0.0.0".into(), cert: Some(d.join("missing.pem")) };
        assert!(t.url().starts_with("https://"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn debounce() {
        let t = Instant::now();
        assert!(should_open(None, t));
        assert!(!should_open(Some(t), t + Duration::from_millis(400)));
        assert!(should_open(Some(t), t + DEBOUNCE));
    }

    #[test]
    fn icon_images() {
        let sizes: Vec<usize> = [16, 20, 24, 32, 40, 48, 64, 300]
            .iter()
            .map(|&w| {
                let (off, len) = pick_icon_image(ICON, w).unwrap();
                assert!(off + len <= ICON.len());
                off
            })
            .collect();
        // 16 -> 16, 20 -> 24, 24 -> 24, 32 -> 32, 40 -> 48, 48 -> 48, 64 -> 256, 300 -> 256
        assert_eq!(sizes[1], sizes[2]);
        assert_eq!(sizes[4], sizes[5]);
        assert_eq!(sizes[6], sizes[7]);
        assert_ne!(sizes[0], sizes[1]);
        assert_eq!(&ICON[sizes[0]..sizes[0] + 4], &40u32.to_le_bytes()); // a DIB header
        assert_eq!(&ICON[sizes[7] + 1..sizes[7] + 4], b"PNG");
        assert_eq!(pick_icon_image(b"nope", 16), None);
    }

    /// Adds a real icon to the taskbar for a moment: `cargo test -- --ignored tray_smoke`.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn tray_smoke() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let opts = |exited: Arc<AtomicBool>| TrayOptions {
            app_name: "Plex Couch (test)".into(),
            url: Box::new(|| "http://127.0.0.1:1/".into()),
            on_exit: Box::new(move || exited.store(true, Ordering::SeqCst)),
        };
        let exited = Arc::new(AtomicBool::new(false));
        let tray = start(opts(exited.clone())).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        tray.stop();
        assert!(!exited.load(Ordering::SeqCst));
        // "Ieșire" calls on_exit, removes the icon and ends the thread
        let tray = start(opts(exited.clone())).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        assert!(tray.test_exit());
        assert!(exited.load(Ordering::SeqCst));
    }
}
