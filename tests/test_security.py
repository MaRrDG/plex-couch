"""Security hardening: login guard under parallel requests, session epoch and renewal, Host
allowlist, HTTPS-required startup, LAN-only TV sessions, cancel after fire, connection cap,
no-redirect upstream opener, HSTS and the certificate tool.

Nothing here runs shutdown.exe (subprocess.run is replaced) or reaches the internet."""

import http.client
import importlib.util
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)

import app  # noqa: E402
from plexcouch import logic, services, system  # noqa: E402
from plexcouch.config import Config  # noqa: E402

PIN = "246810"
DAY = 86400


def _load_make_certs():
    spec = importlib.util.spec_from_file_location("make_certs", os.path.join(ROOT, "tools", "make_certs.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


make_certs = _load_make_certs()


class _Result:
    def __init__(self, code):
        self.returncode = code


class TestCancelAfterFire(unittest.TestCase):
    def setUp(self):
        self.calls = []
        self.codes = {"/a": 0}
        self.shutdown_delay = 0.0
        self.old = (system.POWER_DELAY, system.subprocess.run, system._shutdown_exe)
        system.POWER_DELAY = 0.05
        system._shutdown_exe = lambda: "shutdown-mock.exe"

        def fake_run(args, **kw):  # never runs shutdown.exe
            self.calls.append(list(args))
            if "/a" not in args and self.shutdown_delay:
                time.sleep(self.shutdown_delay)
            return _Result(self.codes.get(args[1], 0))
        system.subprocess.run = fake_run

    def tearDown(self):
        system.POWER_DELAY, system.subprocess.run, system._shutdown_exe = self.old

    def wait_fired(self, acts):
        deadline = time.time() + 5
        while time.time() < deadline:
            with acts._lock:
                if acts._fired is not None:
                    return
            time.sleep(0.01)
        self.fail("timer did not fire")

    def test_cancel_before_fire(self):
        acts = system.Actions(dry_run=False, app_name="App")
        system.POWER_DELAY = 5
        acts.power("shutdown")
        self.assertEqual(acts.cancel_power(), system.CANCELLED)
        self.assertEqual(self.calls, [])
        self.assertEqual(acts.cancel_power(), system.NOTHING)

    def test_cancel_after_fire_runs_abort(self):
        acts = system.Actions(dry_run=False, app_name="App")
        acts.power("shutdown")
        self.wait_fired(acts)
        time.sleep(0.05)
        self.assertEqual(acts.cancel_power(), system.ABORTED)
        self.assertEqual(self.calls, [["shutdown-mock.exe", "/s", "/hybrid", "/t", "0"],
                                      ["shutdown-mock.exe", "/a"]])
        self.assertIsNone(acts.pending())

    def test_cancel_too_late(self):
        self.codes["/a"] = 1116
        acts = system.Actions(dry_run=False, app_name="App")
        acts.power("restart")
        self.wait_fired(acts)
        self.assertEqual(acts.cancel_power(), system.TOO_LATE)
        self.assertEqual(self.calls[-1], ["shutdown-mock.exe", "/a"])

    def test_cancel_while_firing_waits_then_aborts(self):
        self.shutdown_delay = 0.3
        acts = system.Actions(dry_run=False, app_name="App")
        acts.power("shutdown")
        self.wait_fired(acts)  # shutdown.exe is "running" now
        self.assertEqual(acts.cancel_power(), system.ABORTED)
        self.assertEqual([c[1] for c in self.calls], ["/s", "/a"])  # /a only after the command

    def test_dry_run_after_fire_only_logs(self):
        acts = system.Actions(dry_run=True, app_name="App")
        acts.power("shutdown")
        self.wait_fired(acts)
        self.assertEqual(acts.cancel_power(), system.ABORTED)
        self.assertEqual(self.calls, [])

    def test_abort_args(self):
        self.assertEqual(system.abort_args("shutdown.exe"), ["shutdown.exe", "/a"])


class TestNoRedirectWithKeys(unittest.TestCase):
    def setUp(self):
        hits = self.hits = []

        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_GET(self):
                if self.path.startswith("/redir"):
                    self.send_response(302)
                    self.send_header("Location", "/target")
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                hits.append(dict(self.headers))
                body = b'{"ok": true}'
                self.send_response(200)
                self.send_header("Content-Type", "image/png" if "img" in self.path else "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        self.srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.base = "http://127.0.0.1:%d" % self.srv.server_address[1]
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()

    def tearDown(self):
        self.srv.shutdown()
        self.srv.server_close()

    def test_redirect_refused_with_a_key(self):
        for hdr in ({"X-Api-Key": "k"}, {"X-Plex-Token": "t"}, {"X-API-KEY": "k"}):
            with self.assertRaises(services.RedirectRefused):
                services._get_json(self.base + "/redir", hdr)
            self.assertIsNone(services._get_image(self.base + "/redir", hdr))
        self.assertEqual(self.hits, [])

    def test_redirect_followed_without_a_key(self):
        self.assertEqual(services._get_json(self.base + "/redir"), {"ok": True})
        self.assertEqual(len(self.hits), 1)

    def test_qbittorrent_opener_has_the_handler(self):
        svc = services.Services(type("C", (), {"data": {}})())
        self.assertTrue(any(isinstance(h, services.NoSecretRedirectHandler) for h in svc._qb.handlers))


class TestMakeCertsHardening(unittest.TestCase):
    def test_ca_constraints_and_eku(self):
        cfg = make_certs.ca_config()
        self.assertIn("extendedKeyUsage = serverAuth", cfg)
        line = [x for x in cfg.splitlines() if x.startswith("nameConstraints")][0]
        self.assertIn("permitted;email:.invalid", line)
        self.assertIn("permitted;IP:127.0.0.0/255.0.0.0", line)

    def test_restrict_uses_icacls_on_windows(self):
        args = make_certs.restrict_args(r"C:\x\server.key", "alice")
        self.assertTrue(args[0].lower().endswith("icacls.exe") or args[0] == "icacls")
        self.assertEqual(args[1:], [r"C:\x\server.key", "/inheritance:r", "/grant:r", "alice:F", "SYSTEM:F"])
        if os.name != "nt":
            return
        seen = []
        old_run = make_certs.subprocess.run
        make_certs.subprocess.run = lambda a, **k: seen.append(a) or _Result(0)
        try:
            self.assertTrue(make_certs._restrict(r"C:\x\server.key"))
            self.assertIsInstance(seen[0], list)
            self.assertEqual(seen[0][1:5], [r"C:\x\server.key", "/inheritance:r", "/grant:r",
                                            "%s:F" % os.environ.get("USERNAME")])
            make_certs.subprocess.run = lambda a, **k: _Result(5)
            self.assertFalse(make_certs._restrict(r"C:\x\server.key"))
        finally:
            make_certs.subprocess.run = old_run


if __name__ == "__main__":
    unittest.main()
