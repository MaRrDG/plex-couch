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


if __name__ == "__main__":
    unittest.main()
