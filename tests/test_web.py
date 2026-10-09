import importlib.util
import json
import os
import ssl
import sys
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)

import app  # noqa: E402
from plexcouch import logic  # noqa: E402


def _load_make_certs():
    spec = importlib.util.spec_from_file_location("make_certs", os.path.join(ROOT, "tools", "make_certs.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


make_certs = _load_make_certs()


def _read(name):
    with open(os.path.join(app.STATIC_DIR, name), encoding="utf-8") as f:
        return f.read()


class TestHttpsUrl(unittest.TestCase):
    def test_swaps_port_keeps_host(self):
        self.assertEqual(logic.https_url("192.168.1.50:8765", 8766, "/"), "https://192.168.1.50:8766/")
        self.assertEqual(logic.https_url("192.168.1.50", 8766, "/foo?x=1"), "https://192.168.1.50:8766/foo?x=1")
        self.assertEqual(logic.https_url("PlexPC.local:8765", 8766, "/a"), "https://plexpc.local:8766/a")
        self.assertEqual(logic.https_url("localhost:8765", 443, "/"), "https://localhost/")

    def test_ipv6_host(self):
        self.assertEqual(logic.https_url("[fe80::1]:8765", 8766, "/"), "https://[fe80::1]:8766/")
        self.assertEqual(logic.https_url("[::1]", 8766, "/"), "https://[::1]:8766/")

    def test_bad_host_uses_fallback(self):
        for bad in (None, "", "evil.com/x", "a b", "host:abc", "[nonsense]:1", "x" * 300, "h@st", "a:1:2"):
            self.assertEqual(logic.https_url(bad, 8766, "/", fallback_host="192.168.1.50"),
                             "https://192.168.1.50:8766/", bad)
        self.assertEqual(logic.https_url("", 8766, "/", fallback_host="::1"), "https://[::1]:8766/")

    def test_path_sanitised(self):
        self.assertEqual(logic.https_url("10.0.0.2", 8766, "//evil.com/x"), "https://10.0.0.2:8766/evil.com/x")
        self.assertEqual(logic.https_url("10.0.0.2", 8766, "/a\r\nSet-Cookie: x"), "https://10.0.0.2:8766/")
        self.assertEqual(logic.https_url("10.0.0.2", 8766, "foo"), "https://10.0.0.2:8766/")
        self.assertEqual(logic.https_url("10.0.0.2", 8766, ""), "https://10.0.0.2:8766/")


class TestCookie(unittest.TestCase):
    def test_secure_flag_only_over_https(self):
        c = logic.session_cookie("pc_session", "tok", 60, True)
        self.assertEqual(c, "pc_session=tok; Max-Age=60; HttpOnly; SameSite=Strict; Path=/; Secure")
        c = logic.session_cookie("pc_session", "tok", 60, False)
        self.assertNotIn("Secure", c)
        self.assertIn("HttpOnly", c)
        self.assertIn("SameSite=Strict", c)

    def test_handlers(self):
        self.assertFalse(app.Handler.secure)
        self.assertTrue(app.SecureHandler.secure)


class TestStatic(unittest.TestCase):
    def test_whitelist_has_pwa_files(self):
        for path in ("/sw.js", "/manifest.webmanifest", "/icon-192.png", "/icon-512.png",
                     "/icon-maskable-512.png", "/apple-touch-icon.png", "/icon.svg"):
            self.assertIn(path, app.STATIC_FILES)
        self.assertEqual(app.STATIC_FILES["/sw.js"][1], "text/javascript; charset=utf-8")
        self.assertTrue(app.STATIC_FILES["/manifest.webmanifest"][1].startswith("application/manifest+json"))
        self.assertEqual(app.STATIC_HEADERS["/sw.js"], {"Service-Worker-Allowed": "/"})

    def test_whitelisted_files_exist_and_nothing_else_is_served(self):
        for path, (name, _ctype) in app.STATIC_FILES.items():
            self.assertTrue(os.path.isfile(os.path.join(app.STATIC_DIR, name)), path)
            self.assertNotIn("/", name)
        self.assertNotIn("/install.html", app.STATIC_FILES)

    def test_manifest_icons(self):
        raw = _read("manifest.webmanifest")
        m = json.loads(raw.replace("{{APP_NAME}}", "Plex Couch"))
        self.assertEqual((m["start_url"], m["scope"], m["display"]), ("/", "/", "standalone"))
        purposes = {(i["sizes"], i["purpose"]) for i in m["icons"]}
        self.assertTrue({("192x192", "any"), ("512x512", "any"), ("512x512", "maskable")} <= purposes)
        for i in m["icons"]:
            self.assertIn(i["src"], app.STATIC_FILES)

    def test_sw_precaches_whitelisted_files_only(self):
        sw = _read("sw.js")
        self.assertIn("'/api/'", sw)
        for path in ("/", "/app.js", "/style.css", "/manifest.webmanifest", "/icon-192.png"):
            self.assertIn("'%s'" % path, sw)

    def test_install_csp_hashes_inline_style(self):
        page = b"<html><style>body{color:red}</style></html>"
        csp = app.install_csp(page)
        self.assertIn("style-src 'sha256-", csp)
        self.assertIn("default-src 'none'", csp)
        self.assertNotIn("script-src", csp)


class TestHttpsSettings(unittest.TestCase):
    def test_precedence_cli_config_default(self):
        import argparse
        import tempfile
        from plexcouch.config import Config
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "cfg.json")
            with open(path, "w", encoding="utf-8") as f:
                json.dump({"https": {"port": 9443, "key": "keys/k.pem"}}, f)
            cfg = Config(path)
            none = argparse.Namespace(https_port=None, cert=None, key=None, ca_cert=None)
            h = app.https_settings(none, cfg)
            self.assertEqual(h["port"], 9443)
            self.assertEqual(h["key"], os.path.join(d, "keys", "k.pem"))
            self.assertEqual(h["cert"], os.path.join(d, "certs", "server.pem"))
            self.assertEqual(h["ca_cert"], os.path.join(d, "certs", "ca.crt"))
            cli = argparse.Namespace(https_port=8800, cert=os.path.join(d, "c.pem"), key=None, ca_cert=None)
            h = app.https_settings(cli, cfg)
            self.assertEqual((h["port"], h["cert"]), (8800, os.path.join(d, "c.pem")))


class TestCertDer(unittest.TestCase):
    PEM = ("-----BEGIN CERTIFICATE-----\nMAMCAQA=\n-----END CERTIFICATE-----\n")

    def test_pem_and_der(self):
        der = logic.cert_der(self.PEM.encode())
        self.assertEqual(der, ssl.PEM_cert_to_DER_cert(self.PEM))
        self.assertEqual(logic.cert_der(der), der)


class TestMakeCerts(unittest.TestCase):
    def test_ca_config_is_name_constrained(self):
        cfg = make_certs.ca_config()
        self.assertIn("basicConstraints = critical,CA:TRUE,pathlen:0", cfg)
        self.assertIn("keyUsage = critical,keyCertSign,cRLSign", cfg)
        line = [x for x in cfg.splitlines() if x.startswith("nameConstraints")][0]
        self.assertTrue(line.startswith("nameConstraints = critical,"))
        for part in ("permitted;IP:192.168.0.0/255.255.0.0", "permitted;IP:10.0.0.0/255.0.0.0",
                     "permitted;IP:172.16.0.0/255.240.0.0", "permitted;DNS:.local", "permitted;DNS:localhost"):
            self.assertIn(part, line)
        self.assertNotIn("excluded", line)
        self.assertIn("CN = Plex Couch Local CA", cfg)

    def test_server_config(self):
        cfg = make_certs.server_config(["192.168.1.50", "127.0.0.1"], ["localhost"])
        self.assertIn("subjectAltName = IP:192.168.1.50,IP:127.0.0.1,DNS:localhost", cfg)
        self.assertIn("extendedKeyUsage = serverAuth", cfg)
        self.assertIn("keyUsage = critical,digitalSignature,keyEncipherment", cfg)
        self.assertIn("CA:FALSE", cfg)
        self.assertEqual(make_certs.SERVER_DAYS, 397)
        self.assertEqual(make_certs.CA_DAYS, 3650)

    def test_validate_names(self):
        self.assertEqual(make_certs.validate_names(["192.168.1.50", "10.1.2.3", "172.20.0.1", "127.0.0.1"],
                                                   ["localhost", "plexpc.local"]), [])
        self.assertTrue(make_certs.validate_names(["8.8.8.8"], []))
        self.assertTrue(make_certs.validate_names(["172.32.0.1"], []))
        self.assertTrue(make_certs.validate_names(["fe80::1"], []))
        self.assertTrue(make_certs.validate_names([], ["example.com"]))
        self.assertTrue(make_certs.validate_names([], []))


if __name__ == "__main__":
    unittest.main()
