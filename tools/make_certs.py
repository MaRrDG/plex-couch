"""Create a local, name-constrained CA and a server certificate for Plex Couch (HTTPS / PWA).

Standard library + the `openssl` command line tool. Nothing is downloaded and private keys are
never printed.

    python tools/make_certs.py --ip 192.168.1.50 [--ip 127.0.0.1] [--dns localhost] [--out certs]

Files written to the output folder (git-ignored):
    ca.key / ca.pem       CA private key and certificate (PEM). Created once, kept on re-runs.
    ca.crt                the CA certificate as DER, to install on the phone.
    server.key / server.pem   server key and certificate signed by the CA (397 days).

Re-running renews only the server certificate. Use --force-ca to replace the CA (every phone
then has to install the new ca.crt).

The CA carries critical name constraints, so even if its key leaked it could only vouch for
private-network IP addresses, `localhost` and `*.local` names, never for public web sites; it
permits no e-mail names and is limited to TLS server use (extendedKeyUsage = serverAuth).
The private keys are made readable only by the current user (icacls on Windows, chmod 600
elsewhere).
"""

import argparse
import ipaddress
import os
import re
import secrets
import shutil
import subprocess
import sys
import tempfile

CA_NAME = "Plex Couch Local CA"
SERVER_NAME = "Plex Couch"
CA_DAYS = 3650
SERVER_DAYS = 397

# Private IPv4 ranges (RFC 1918), loopback, mDNS names and localhost.
NAME_CONSTRAINTS = ",".join([
    "permitted;IP:192.168.0.0/255.255.0.0",
    "permitted;IP:10.0.0.0/255.0.0.0",
    "permitted;IP:172.16.0.0/255.240.0.0",
    "permitted;IP:127.0.0.0/255.0.0.0",
    "permitted;DNS:.local",
    "permitted;DNS:localhost",
    # No e-mail (S/MIME) names: every rfc822Name must be under the reserved .invalid domain,
    # so no real address ever matches. ('excluded;email:.' is accepted by openssl but matches
    # nothing when certificates are verified, so it would exclude nothing.)
    "permitted;email:.invalid",
])
# The CA may only be used for TLS server certificates (verifiers that check EKU along the chain).
CA_EKU = "serverAuth"

_DNS_RE = re.compile(r"^(?=.{1,253}$)[A-Za-z0-9](?:[A-Za-z0-9-]{0,62})(?:\.[A-Za-z0-9-]{1,63})*$")


def ca_config():
    """OpenSSL config text for the self-signed CA."""
    return "\n".join([
        "[req]",
        "prompt = no",
        "distinguished_name = dn",
        "x509_extensions = v3_ca",
        "[dn]",
        "CN = %s" % CA_NAME,
        "[v3_ca]",
        "basicConstraints = critical,CA:TRUE,pathlen:0",
        "keyUsage = critical,keyCertSign,cRLSign",
        "extendedKeyUsage = %s" % CA_EKU,
        "subjectKeyIdentifier = hash",
        "nameConstraints = critical,%s" % NAME_CONSTRAINTS,
        "",
    ])


def server_config(ips, dns_names):
    """OpenSSL config text for the server CSR and its extensions."""
    sans = ["IP:%s" % ip for ip in ips] + ["DNS:%s" % d for d in dns_names]
    return "\n".join([
        "[req]",
        "prompt = no",
        "distinguished_name = dn",
        "[dn]",
        # A CN that is not a host name, so no verifier treats it as one.
        "CN = %s" % SERVER_NAME,
        "[v3_srv]",
        "basicConstraints = critical,CA:FALSE",
        "keyUsage = critical,digitalSignature,keyEncipherment",
        "extendedKeyUsage = serverAuth",
        "subjectKeyIdentifier = hash",
        "authorityKeyIdentifier = keyid,issuer",
        "subjectAltName = %s" % ",".join(sans),
        "",
    ])


def validate_names(ips, dns_names):
    """Check SANs against the CA's name constraints. Returns a list of error strings."""
    errors = []
    allowed = [ipaddress.ip_network(n) for n in
               ("192.168.0.0/16", "10.0.0.0/8", "172.16.0.0/12", "127.0.0.0/8")]
    for ip in ips:
        try:
            addr = ipaddress.ip_address(ip)
        except ValueError:
            errors.append("not an IP address: %s" % ip)
            continue
        if addr.version != 4 or not any(addr in n for n in allowed):
            errors.append("IP outside the CA's private ranges: %s" % ip)
    for d in dns_names:
        dl = d.lower().rstrip(".")
        if not _DNS_RE.match(dl):
            errors.append("not a DNS name: %s" % d)
        elif not (dl == "localhost" or dl.endswith(".local")):
            errors.append("DNS name must be localhost or end in .local: %s" % d)
    if not ips and not dns_names:
        errors.append("give at least one --ip or --dns")
    return errors


def find_openssl(explicit=None):
    """Locate openssl: --openssl, $OPENSSL, PATH, then the copy that ships with Git for Windows."""
    cands = [explicit, os.environ.get("OPENSSL"), shutil.which("openssl")]
    for root in (os.environ.get("ProgramFiles"), os.environ.get("ProgramW6432"),
                 os.environ.get("ProgramFiles(x86)"),
                 os.path.join(os.environ.get("LOCALAPPDATA") or "", "Programs")):
        if root:
            cands += [os.path.join(root, "Git", "usr", "bin", "openssl.exe"),
                      os.path.join(root, "Git", "mingw64", "bin", "openssl.exe")]
    for c in cands:
        if c and os.path.isfile(c):
            return c
    return None


def _run(openssl, args):
    r = subprocess.run([openssl] + args, capture_output=True, text=True)
    if r.returncode != 0:
        # openssl error output never contains key material.
        raise RuntimeError("openssl %s failed:\n%s" % (args[0], (r.stderr or "").strip()))
    return r.stdout


def _serial():
    return "0x" + secrets.token_hex(16)


def _write(path, text):
    with open(path, "w", encoding="ascii", newline="\n") as f:
        f.write(text)


def restrict_args(path, user):
    """icacls argv that leaves only the current user and SYSTEM with access to path. Pure."""
    return [_icacls_exe(), path, "/inheritance:r", "/grant:r", "%s:F" % user, "SYSTEM:F"]


def _icacls_exe():
    exe = os.path.join(os.environ.get("SystemRoot", r"C:\Windows"), "System32", "icacls.exe")
    return exe if os.path.isfile(exe) else "icacls"


def _restrict(path):
    """Make a private key readable only by the current user. chmod does nothing useful on
    Windows, so there the file's ACL is replaced with icacls (no inherited entries)."""
    if os.name != "nt":
        try:
            os.chmod(path, 0o600)
        except OSError:
            pass
        return True
    user = os.environ.get("USERNAME")
    if not user:
        import getpass
        user = getpass.getuser()
    try:
        r = subprocess.run(restrict_args(path, user), capture_output=True, text=True)
        ok = r.returncode == 0
    except OSError:
        ok = False
    if not ok:
        print("warning: could not restrict access to %s; do it by hand with icacls." % path,
              file=sys.stderr)
    return ok


def make_ca(openssl, out, tmp):
    key, pem = os.path.join(out, "ca.key"), os.path.join(out, "ca.pem")
    cnf = os.path.join(tmp, "ca.cnf")
    _write(cnf, ca_config())
    _run(openssl, ["genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:3072", "-out", key])
    _restrict(key)
    _run(openssl, ["req", "-x509", "-new", "-key", key, "-sha256", "-days", str(CA_DAYS),
                   "-set_serial", _serial(), "-config", cnf, "-out", pem])


def make_server(openssl, out, tmp, ips, dns_names):
    key, pem = os.path.join(out, "server.key"), os.path.join(out, "server.pem")
    cnf, csr = os.path.join(tmp, "server.cnf"), os.path.join(tmp, "server.csr")
    _write(cnf, server_config(ips, dns_names))
    _run(openssl, ["genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048", "-out", key])
    _restrict(key)
    _run(openssl, ["req", "-new", "-key", key, "-config", cnf, "-out", csr])
    _run(openssl, ["x509", "-req", "-in", csr, "-CA", os.path.join(out, "ca.pem"),
                   "-CAkey", os.path.join(out, "ca.key"), "-set_serial", _serial(),
                   "-days", str(SERVER_DAYS), "-sha256", "-extfile", cnf, "-extensions", "v3_srv",
                   "-out", pem])


def main(argv=None):
    here = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    p = argparse.ArgumentParser(description="Create the Plex Couch local CA and server certificate.")
    p.add_argument("--ip", action="append", default=[], help="server IP address SAN (repeatable)")
    p.add_argument("--dns", action="append", default=[], help="server DNS SAN: localhost or *.local (repeatable)")
    p.add_argument("--out", default=os.path.join(here, "certs"), help="output folder (default: certs/)")
    p.add_argument("--force-ca", action="store_true", help="replace an existing CA")
    p.add_argument("--openssl", help="path to the openssl binary")
    args = p.parse_args(argv)

    errors = validate_names(args.ip, args.dns)
    if errors:
        for e in errors:
            print("error: " + e, file=sys.stderr)
        return 2
    openssl = find_openssl(args.openssl)
    if not openssl:
        print("error: openssl not found. Install Git for Windows or pass --openssl <path>.", file=sys.stderr)
        return 2

    out = os.path.abspath(args.out)
    os.makedirs(out, exist_ok=True)
    have_ca = os.path.isfile(os.path.join(out, "ca.key")) and os.path.isfile(os.path.join(out, "ca.pem"))
    with tempfile.TemporaryDirectory(prefix="plexcouch-certs-") as tmp:
        try:
            if have_ca and not args.force_ca:
                print("Keeping the existing CA in %s (use --force-ca to replace it)." % out)
            else:
                make_ca(openssl, out, tmp)
                print("Created CA \"%s\" (%d days)." % (CA_NAME, CA_DAYS))
            _run(openssl, ["x509", "-in", os.path.join(out, "ca.pem"), "-outform", "DER",
                           "-out", os.path.join(out, "ca.crt")])
            make_server(openssl, out, tmp, args.ip, args.dns)
        except RuntimeError as e:
            print("error: %s" % e, file=sys.stderr)
            return 1
    print("Created server certificate for %s (%d days)." % (", ".join(args.ip + args.dns), SERVER_DAYS))
    print("Files in %s: ca.crt (install on the phone), ca.pem, server.pem, server.key, ca.key." % out)
    print("Keep ca.key and server.key private; never commit the certs folder.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
