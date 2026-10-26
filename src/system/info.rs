//! PC info for `/api/status` and the disk-low check (spec 6.3), own IPv4s (spec 6.4).
//!
//! - [`uptime_seconds`]: `GetTickCount64() / 1000`.
//! - [`disks`]: per configured letter, `GetDiskFreeSpaceExW` on `<letter>\`;
//!   `{"drive", "free_gb", "total_gb", "used_pct"}` rounded half to even; an error skips it.
//! - [`free_gb`]: the same, unrounded, None on error.
//! - [`own_ipv4s`]: every IPv4 of the host name plus the local address of a UDP "connect"
//!   to 10.255.255.255:1 (no packet is sent).

use crate::config::Config;
use crate::json::py_round;
use serde_json::{json, Value};
use std::collections::HashSet;

/// `{"hostname", "uptime", "disks", "platform": "win32"}`.
pub fn pc_info(config: &Config) -> Value {
    let letters: Vec<String> = match config.get("disks") {
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        _ => Vec::new(),
    };
    json!({"hostname": hostname(), "uptime": uptime_seconds(), "disks": disks(&letters), "platform": "win32"})
}

/// `socket.gethostname()`: the DNS host name (not the upper-case NetBIOS name).
#[cfg(windows)]
pub fn hostname() -> String {
    use windows_sys::Win32::System::SystemInformation::{ComputerNameDnsHostname, GetComputerNameExW};
    let mut size: u32 = 0;
    // SAFETY: a null buffer asks for the size.
    unsafe { GetComputerNameExW(ComputerNameDnsHostname, std::ptr::null_mut(), &mut size) };
    if size > 0 {
        let mut buf = vec![0u16; size as usize + 1];
        let mut n = buf.len() as u32;
        // SAFETY: buf holds n UTF-16 units.
        if unsafe { GetComputerNameExW(ComputerNameDnsHostname, buf.as_mut_ptr(), &mut n) } != 0 {
            return String::from_utf16_lossy(&buf[..n as usize]);
        }
    }
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

#[cfg(not(windows))]
pub fn hostname() -> String {
    std::env::var("HOSTNAME").unwrap_or_default()
}

#[cfg(windows)]
pub fn uptime_seconds() -> Option<u64> {
    // SAFETY: no arguments, cannot fail.
    Some(unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() } / 1000)
}

#[cfg(not(windows))]
pub fn uptime_seconds() -> Option<u64> {
    None
}

/// `(total, free)` bytes of the volume holding `root` (`shutil.disk_usage`).
#[cfg(windows)]
pub fn disk_usage(root: &str) -> Option<(u64, u64)> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: NUL-terminated path, valid out pointers.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) };
    (ok != 0).then_some((total, free))
}

#[cfg(not(windows))]
pub fn disk_usage(_root: &str) -> Option<(u64, u64)> {
    None
}

fn root_of(d: &str) -> String {
    if d.ends_with('\\') { d.to_string() } else { format!("{d}\\") }
}

const GB: f64 = 1024.0 * 1024.0 * 1024.0;

pub fn disks(letters: &[String]) -> Vec<Value> {
    let mut out = Vec::new();
    for d in letters {
        let Some((total, free)) = disk_usage(&root_of(d)) else { continue };
        let used = if total > 0 { json!(py_round((total - free) as f64 / total as f64 * 100.0, 1)) } else { json!(0) };
        out.push(json!({
            "drive": d.trim_end_matches('\\'),
            "free_gb": py_round(free as f64 / GB, 1),
            "total_gb": py_round(total as f64 / GB, 1),
            "used_pct": used,
        }));
    }
    out
}

/// Free GB on the drive holding `path` (unrounded), None if it cannot be read.
pub fn free_gb(path: &str) -> Option<f64> {
    disk_usage(&root_of(path)).map(|(_, free)| free as f64 / GB)
}

/// This PC's IPv4 addresses.
pub fn own_ipv4s() -> HashSet<String> {
    use std::net::{SocketAddr, ToSocketAddrs};
    let mut ips = HashSet::new();
    if let Ok(addrs) = (hostname().as_str(), 0u16).to_socket_addrs() {
        for a in addrs {
            if let SocketAddr::V4(v4) = a {
                ips.insert(v4.ip().to_string());
            }
        }
    }
    if let Some(ip) = primary_ipv4() {
        ips.insert(ip.to_string());
    }
    ips
}

/// The IPv4 of the primary outbound interface: the local address of a UDP "connect" to
/// 10.255.255.255:1 (no packet is sent).
pub fn primary_ipv4() -> Option<std::net::Ipv4Addr> {
    use std::net::{SocketAddr, UdpSocket};
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("10.255.255.255:1").ok()?;
    match s.local_addr() {
        Ok(SocketAddr::V4(v4)) if !v4.ip().is_unspecified() => Some(*v4.ip()),
        _ => None,
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn real_values() {
        assert!(!hostname().is_empty());
        assert!(uptime_seconds().unwrap() > 0);
        let sys = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        let d = disks(&[sys.clone(), "Q:\\nope-not-a-drive".into()]);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0]["drive"], json!(sys));
        let total = d[0]["total_gb"].as_f64().unwrap();
        assert!(total > 0.0 && d[0]["free_gb"].as_f64().unwrap() <= total);
        assert!(free_gb(&sys).unwrap() > 0.0);
        assert_eq!(free_gb("Q:\\nope-not-a-drive"), None);
        assert!(own_ipv4s().iter().all(|ip| ip.parse::<std::net::Ipv4Addr>().is_ok()));
    }
}
