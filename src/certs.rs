//! `plex-couch make-certs`: the local, name-constrained CA and the server certificate
//! (spec 9.1). Pure Rust (rcgen + rsa); no openssl needed.
//!
//! ```text
//! plex-couch make-certs --ip 192.168.1.50 [--ip 127.0.0.1] [--dns localhost] [--out certs]
//!                       [--force-ca] [--openssl PATH (accepted, unused)]
//! ```
//!
//! Files in the output folder: `ca.key`/`ca.pem` (RSA 3072, 3650 days; created once, kept on
//! re-runs unless `--force-ca`), `ca.crt` (DER of `ca.pem`, rewritten every run),
//! `server.key`/`server.pem` (RSA 2048, 397 days, signed by the CA). Private keys are
//! PKCS#8 PEM like `openssl genpkey`, restricted to the current user with icacls.
//!
//! The CA carries critical name constraints (private IPv4 ranges, `localhost`, `*.local`,
//! no e-mail names) and is limited to TLS servers (EKU serverAuth). Key identifiers are the
//! SHA-1 of the public key, like openssl's `subjectKeyIdentifier = hash`, so a CA made by
//! a CA made earlier with openssl keeps working with server certificates made here.

use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType, ExtendedKeyUsagePurpose,
    GeneralSubtree, IsCa, KeyIdMethod, KeyPair, KeyUsagePurpose, NameConstraints, SanType, SerialNumber,
};
use sha1::Digest;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

pub const CA_NAME: &str = "Plex Couch Local CA";
pub const SERVER_NAME: &str = "Plex Couch";
pub const CA_DAYS: i64 = 3650;
pub const SERVER_DAYS: i64 = 397;
pub const CA_KEY_BITS: usize = 3072;
pub const SERVER_KEY_BITS: usize = 2048;

/// (network, mask) of the IPv4 ranges the CA may vouch for (RFC 1918 + loopback).
pub const PERMITTED_V4: [([u8; 4], [u8; 4]); 4] = [
    ([192, 168, 0, 0], [255, 255, 0, 0]),
    ([10, 0, 0, 0], [255, 0, 0, 0]),
    ([172, 16, 0, 0], [255, 240, 0, 0]),
    ([127, 0, 0, 0], [255, 0, 0, 0]),
];

/// `permitted;IP:...` x4, `permitted;DNS:.local`, `permitted;DNS:localhost`,
/// `permitted;email:.invalid` (no real address ever matches), no excluded subtrees.
pub fn name_constraints() -> NameConstraints {
    let mut permitted: Vec<GeneralSubtree> =
        PERMITTED_V4.iter().map(|(n, m)| GeneralSubtree::IpAddress(CidrSubnet::V4(*n, *m))).collect();
    permitted.push(GeneralSubtree::DnsName(".local".into()));
    permitted.push(GeneralSubtree::DnsName("localhost".into()));
    permitted.push(GeneralSubtree::Rfc822Name(".invalid".into()));
    NameConstraints { permitted_subtrees: permitted, excluded_subtrees: Vec::new() }
}

fn dn(cn: &str) -> DistinguishedName {
    let mut d = DistinguishedName::new();
    d.push(DnType::CommonName, cn);
    d
}

fn validity(p: &mut CertificateParams, days: i64) {
    let now = time::OffsetDateTime::now_utc();
    p.not_before = now;
    p.not_after = now + time::Duration::days(days);
}

/// `0x<32 random hex>` like `-set_serial`, kept positive.
pub fn random_serial() -> SerialNumber {
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).expect("OS random source");
    b[0] &= 0x7f;
    if b[0] == 0 {
        b[0] = 1;
    }
    SerialNumber::from_slice(&b)
}

/// openssl `subjectKeyIdentifier = hash`: SHA-1 of the subjectPublicKey bits.
pub fn key_id(key: &KeyPair) -> Vec<u8> {
    sha1::Sha1::digest(key.public_key_raw()).to_vec()
}

/// The CA (`[v3_ca]`): CA:TRUE pathlen:0, keyCertSign+cRLSign, EKU serverAuth, name
/// constraints, CN = Plex Couch Local CA.
pub fn ca_params(key: &KeyPair) -> CertificateParams {
    let mut p = CertificateParams::default();
    p.distinguished_name = dn(CA_NAME);
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    p.name_constraints = Some(name_constraints());
    p.key_identifier_method = KeyIdMethod::PreSpecified(key_id(key));
    p.serial_number = Some(random_serial());
    validity(&mut p, CA_DAYS);
    p
}

/// The server certificate (`[v3_srv]`): CA:FALSE, digitalSignature+keyEncipherment, EKU
/// serverAuth, SKI/AKI, SAN = the IPs then the DNS names as given; CN = Plex Couch (not a
/// host name, on purpose).
pub fn server_params(ips: &[String], dns_names: &[String], key: &KeyPair) -> Result<CertificateParams, String> {
    let mut p = CertificateParams::default();
    p.distinguished_name = dn(SERVER_NAME);
    p.is_ca = IsCa::ExplicitNoCa;
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
    p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    p.use_authority_key_identifier_extension = true;
    p.key_identifier_method = KeyIdMethod::PreSpecified(key_id(key));
    p.serial_number = Some(random_serial());
    let mut sans = Vec::new();
    for ip in ips {
        sans.push(SanType::IpAddress(ip.parse::<IpAddr>().map_err(|_| format!("not an IP address: {ip}"))?));
    }
    for d in dns_names {
        let s = rcgen::Ia5String::try_from(d.clone()).map_err(|_| format!("not a DNS name: {d}"))?;
        sans.push(SanType::DnsName(s));
    }
    p.subject_alt_names = sans;
    validity(&mut p, SERVER_DAYS);
    Ok(p)
}

/// `^(?=.{1,253}$)[A-Za-z0-9](?:[A-Za-z0-9-]{0,62})(?:\.[A-Za-z0-9-]{1,63})*$`
fn dns_ok(d: &str) -> bool {
    if d.is_empty() || d.len() > 253 {
        return false;
    }
    let labels: Vec<&str> = d.split('.').collect();
    let ok = |l: &str| l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let first = labels[0];
    if first.is_empty() || first.len() > 63 || !first.as_bytes()[0].is_ascii_alphanumeric() || !ok(first) {
        return false;
    }
    labels[1..].iter().all(|l| !l.is_empty() && l.len() <= 63 && ok(l))
}

/// Check the SANs against the CA's name constraints. Returns the error lines.
pub fn validate_names(ips: &[String], dns_names: &[String]) -> Vec<String> {
    let mut errors = Vec::new();
    for ip in ips {
        match crate::http::security::parse_ip(ip) {
            None => errors.push(format!("not an IP address: {ip}")),
            Some(IpAddr::V4(a)) if permitted_v4(a) => {}
            Some(_) => errors.push(format!("IP outside the CA's private ranges: {ip}")),
        }
    }
    for d in dns_names {
        let dl = d.to_lowercase();
        let dl = dl.trim_end_matches('.');
        if !dns_ok(dl) {
            errors.push(format!("not a DNS name: {d}"));
        } else if !(dl == "localhost" || dl.ends_with(".local")) {
            errors.push(format!("DNS name must be localhost or end in .local: {d}"));
        }
    }
    if ips.is_empty() && dns_names.is_empty() {
        errors.push("give at least one --ip or --dns".into());
    }
    errors
}

fn permitted_v4(a: Ipv4Addr) -> bool {
    let x = u32::from(a);
    PERMITTED_V4.iter().any(|(n, m)| x & u32::from_be_bytes(*m) == u32::from_be_bytes(*n))
}

// ---------------------------------------------------------------------------
// Private key files
// ---------------------------------------------------------------------------

fn icacls_exe() -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
    let exe = Path::new(&root).join("System32").join("icacls.exe");
    if exe.is_file() {
        exe.to_string_lossy().into_owned()
    } else {
        "icacls".into()
    }
}

/// icacls argv that leaves only `user` and SYSTEM with access to `path`. Pure.
pub fn restrict_args(path: &str, user: &str) -> Vec<String> {
    vec![
        icacls_exe(),
        path.to_string(),
        "/inheritance:r".into(),
        "/grant:r".into(),
        format!("{user}:F"),
        "SYSTEM:F".into(),
    ]
}

fn run_icacls(args: &[String]) -> std::io::Result<i32> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(crate::system::power::CREATE_NO_WINDOW);
    }
    Ok(cmd.status()?.code().unwrap_or(-1))
}

fn current_user() -> Option<String> {
    ["USERNAME", "USER", "LOGNAME", "LNAME"].iter().find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
}

/// Make a private key readable only by the current user (icacls; chmod 600 elsewhere).
pub fn restrict_with(path: &str, user: Option<&str>, run: &dyn Fn(&[String]) -> std::io::Result<i32>) -> bool {
    if !cfg!(windows) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        return true;
    }
    let ok = match user {
        Some(u) => matches!(run(&restrict_args(path, u)), Ok(0)),
        None => false,
    };
    if !ok {
        eprintln!("warning: could not restrict access to {path}; do it by hand with icacls.");
    }
    ok
}

pub fn restrict(path: &Path) -> bool {
    restrict_with(&path.to_string_lossy(), current_user().as_deref(), &run_icacls)
}

/// A new RSA key as PKCS#8 PEM (what `openssl genpkey -algorithm RSA` writes).
pub fn new_rsa_key_pem(bits: usize) -> Result<String, String> {
    use rsa::pkcs8::{EncodePrivateKey, LineEnding};
    let key = rsa::RsaPrivateKey::new(&mut rand_core::OsRng, bits).map_err(|e| format!("RSA key generation failed: {e}"))?;
    let pem = key.to_pkcs8_pem(LineEnding::LF).map_err(|e| format!("RSA key encoding failed: {e}"))?;
    Ok(pem.to_string())
}

fn load_key(pem: &str) -> Result<KeyPair, String> {
    KeyPair::from_pem_and_sign_algo(pem, &rcgen::PKCS_RSA_SHA256)
        .or_else(|_| KeyPair::from_pem(pem))
        .map_err(|e| format!("cannot use the private key: {e}"))
}

fn write_file(path: &Path, data: &[u8]) -> Result<(), String> {
    let mut f = std::fs::File::create(path).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    f.write_all(data).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Write a private key, then lock it down (`_restrict` right after `genpkey`).
fn write_key(path: &Path, pem: &str) -> Result<(), String> {
    write_file(path, pem.as_bytes())?;
    restrict(path);
    Ok(())
}

pub struct Outcome {
    pub created_ca: bool,
}

/// Create (or keep) the CA, write `ca.crt`, create the server certificate.
pub fn make_all(out: &Path, ips: &[String], dns_names: &[String], force_ca: bool, bits: (usize, usize)) -> Result<Outcome, String> {
    std::fs::create_dir_all(out).map_err(|e| format!("cannot create {}: {e}", out.display()))?;
    let (ca_key_path, ca_pem_path) = (out.join("ca.key"), out.join("ca.pem"));
    let have_ca = ca_key_path.is_file() && ca_pem_path.is_file();
    let created_ca = !have_ca || force_ca;
    if created_ca {
        let pem = new_rsa_key_pem(bits.0)?;
        write_key(&ca_key_path, &pem)?;
        let key = load_key(&pem)?;
        let cert = ca_params(&key).self_signed(&key).map_err(|e| format!("cannot create the CA: {e}"))?;
        write_file(&ca_pem_path, cert.pem().as_bytes())?;
    }
    // The CA as the issuer: subject name and key identifier taken from ca.pem itself.
    let ca_pem = std::fs::read_to_string(&ca_pem_path).map_err(|e| format!("cannot read {}: {e}", ca_pem_path.display()))?;
    let ca_key_pem =
        std::fs::read_to_string(&ca_key_path).map_err(|e| format!("cannot read {}: {e}", ca_key_path.display()))?;
    let ca_key = load_key(&ca_key_pem)?;
    let ca_der = crate::http::static_files::cert_der(ca_pem.as_bytes()).map_err(|e| format!("bad ca.pem: {e}"))?;
    write_file(&out.join("ca.crt"), &ca_der)?;
    let mut issuer_params =
        CertificateParams::from_ca_cert_pem(&ca_pem).map_err(|e| format!("cannot read the CA certificate: {e}"))?;
    if !matches!(issuer_params.key_identifier_method, KeyIdMethod::PreSpecified(ref v) if !v.is_empty()) {
        issuer_params.key_identifier_method = KeyIdMethod::PreSpecified(key_id(&ca_key));
    }
    let issuer = issuer_params.self_signed(&ca_key).map_err(|e| format!("cannot use the CA: {e}"))?;

    let server_pem = new_rsa_key_pem(bits.1)?;
    write_key(&out.join("server.key"), &server_pem)?;
    let server_key = load_key(&server_pem)?;
    let cert = server_params(ips, dns_names, &server_key)?
        .signed_by(&server_key, &issuer, &ca_key)
        .map_err(|e| format!("cannot sign the server certificate: {e}"))?;
    write_file(&out.join("server.pem"), cert.pem().as_bytes())?;
    Ok(Outcome { created_ca })
}

const USAGE: &str = "usage: plex-couch make-certs [-h] [--ip IP] [--dns DNS] [--out OUT] [--force-ca] [--openssl OPENSSL]";

/// `make-certs` entry point. Exit codes: 0 ok, 1 failure, 2 bad arguments.
pub fn main(args: &[String], default_out: &Path) -> i32 {
    let mut ips = Vec::new();
    let mut dns = Vec::new();
    let mut out: Option<PathBuf> = None;
    let mut force_ca = false;
    let opts = ["--ip", "--dns", "--out", "--force-ca", "--openssl", "--help"];
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let (name, inline) = match a.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (a.clone(), None),
        };
        let name = if name == "-h" { "--help".to_string() } else { name };
        let matches: Vec<&&str> = opts.iter().filter(|o| o.starts_with(name.as_str()) && name.starts_with("--")).collect();
        let opt = if opts.contains(&name.as_str()) { name.clone() } else if matches.len() == 1 { matches[0].to_string() } else {
            eprintln!("{USAGE}\nplex-couch make-certs: error: unrecognized arguments: {a}");
            return 2;
        };
        let mut value = || -> Option<String> {
            if let Some(v) = inline.clone() {
                return Some(v);
            }
            i += 1;
            args.get(i).cloned()
        };
        match opt.as_str() {
            "--help" => {
                println!("{USAGE}\n\nCreate the Plex Couch local CA and server certificate.");
                return 0;
            }
            "--force-ca" => force_ca = true,
            o => {
                let Some(v) = value() else {
                    eprintln!("{USAGE}\nplex-couch make-certs: error: argument {o}: expected one argument");
                    return 2;
                };
                match o {
                    "--ip" => ips.push(v),
                    "--dns" => dns.push(v),
                    "--out" => out = Some(PathBuf::from(v)),
                    _ => {} // --openssl: accepted for compatibility, not needed
                }
            }
        }
        i += 1;
    }
    let errors = validate_names(&ips, &dns);
    if !errors.is_empty() {
        for e in errors {
            eprintln!("error: {e}");
        }
        return 2;
    }
    let out = crate::config::absolute(&out.unwrap_or_else(|| default_out.to_path_buf()));
    let have_ca = out.join("ca.key").is_file() && out.join("ca.pem").is_file();
    if have_ca && !force_ca {
        println!("Keeping the existing CA in {} (use --force-ca to replace it).", out.display());
    }
    match make_all(&out, &ips, &dns, force_ca, (CA_KEY_BITS, SERVER_KEY_BITS)) {
        Ok(o) => {
            if o.created_ca {
                println!("Created CA \"{CA_NAME}\" ({CA_DAYS} days).");
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    }
    let names: Vec<String> = ips.iter().chain(dns.iter()).cloned().collect();
    println!("Created server certificate for {} ({SERVER_DAYS} days).", names.join(", "));
    println!("Files in {}: ca.crt (install on the phone), ca.pem, server.pem, server.key, ca.key.", out.display());
    println!("Keep ca.key and server.key private; never commit the certs folder.");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    // py: test_web.TestMakeCerts.test_ca_config_is_name_constrained
    #[test]
    fn ca_is_name_constrained() {
        let key = KeyPair::generate().unwrap();
        let p = ca_params(&key);
        assert_eq!(p.is_ca, IsCa::Ca(BasicConstraints::Constrained(0)));
        assert_eq!(p.key_usages, vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign]);
        let nc = p.name_constraints.clone().unwrap();
        assert!(nc.excluded_subtrees.is_empty());
        let permitted = &nc.permitted_subtrees;
        for want in [
            GeneralSubtree::IpAddress(CidrSubnet::V4([192, 168, 0, 0], [255, 255, 0, 0])),
            GeneralSubtree::IpAddress(CidrSubnet::V4([10, 0, 0, 0], [255, 0, 0, 0])),
            GeneralSubtree::IpAddress(CidrSubnet::V4([172, 16, 0, 0], [255, 240, 0, 0])),
            GeneralSubtree::DnsName(".local".into()),
            GeneralSubtree::DnsName("localhost".into()),
        ] {
            assert!(permitted.contains(&want), "{want:?}");
        }
        let cn = p.distinguished_name.get(&DnType::CommonName).unwrap();
        assert_eq!(cn, &rcgen::DnValue::Utf8String(CA_NAME.into()));
    }

    // py: test_web.TestMakeCerts.test_server_config
    #[test]
    fn server_config() {
        let key = KeyPair::generate().unwrap();
        let p = server_params(&s(&["192.168.1.50", "127.0.0.1"]), &s(&["localhost"]), &key).unwrap();
        let sans: Vec<String> = p
            .subject_alt_names
            .iter()
            .map(|x| match x {
                SanType::IpAddress(ip) => format!("IP:{ip}"),
                SanType::DnsName(d) => format!("DNS:{}", d.as_str()),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(sans.join(","), "IP:192.168.1.50,IP:127.0.0.1,DNS:localhost");
        assert_eq!(p.extended_key_usages, vec![ExtendedKeyUsagePurpose::ServerAuth]);
        assert_eq!(p.key_usages, vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment]);
        assert_eq!(p.is_ca, IsCa::ExplicitNoCa);
        assert_eq!(SERVER_DAYS, 397);
        assert_eq!(CA_DAYS, 3650);
    }

    // py: test_web.TestMakeCerts.test_validate_names
    #[test]
    fn validate() {
        assert!(validate_names(&s(&["192.168.1.50", "10.1.2.3", "172.20.0.1", "127.0.0.1"]), &s(&["localhost", "plexpc.local"]))
            .is_empty());
        assert!(!validate_names(&s(&["8.8.8.8"]), &[]).is_empty());
        assert!(!validate_names(&s(&["172.32.0.1"]), &[]).is_empty());
        assert!(!validate_names(&s(&["fe80::1"]), &[]).is_empty());
        assert!(!validate_names(&[], &s(&["example.com"])).is_empty());
        assert!(!validate_names(&[], &[]).is_empty());
    }

    // py: test_security.TestMakeCertsHardening.test_ca_constraints_and_eku
    #[test]
    fn ca_constraints_and_eku() {
        let key = KeyPair::generate().unwrap();
        let p = ca_params(&key);
        assert_eq!(p.extended_key_usages, vec![ExtendedKeyUsagePurpose::ServerAuth]);
        let nc = p.name_constraints.unwrap();
        assert!(nc.permitted_subtrees.contains(&GeneralSubtree::Rfc822Name(".invalid".into())));
        assert!(nc.permitted_subtrees.contains(&GeneralSubtree::IpAddress(CidrSubnet::V4([127, 0, 0, 0], [255, 0, 0, 0]))));
    }

    // py: test_security.TestMakeCertsHardening.test_restrict_uses_icacls_on_windows
    #[test]
    fn restrict_uses_icacls_on_windows() {
        let args = restrict_args(r"C:\x\server.key", "alice");
        assert!(args[0].to_lowercase().ends_with("icacls.exe") || args[0] == "icacls");
        assert_eq!(&args[1..], &s(&[r"C:\x\server.key", "/inheritance:r", "/grant:r", "alice:F", "SYSTEM:F"]));
        if !cfg!(windows) {
            return;
        }
        let seen = std::cell::RefCell::new(Vec::new());
        let ok = restrict_with(r"C:\x\server.key", Some("bob"), &|a: &[String]| {
            seen.borrow_mut().push(a.to_vec());
            Ok(0)
        });
        assert!(ok);
        assert_eq!(&seen.borrow()[0][1..5], &s(&[r"C:\x\server.key", "/inheritance:r", "/grant:r", "bob:F"]));
        assert!(!restrict_with(r"C:\x\server.key", Some("bob"), &|_a: &[String]| Ok(5)));
    }
}
