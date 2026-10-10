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


# ---------------------------------------------------------------------------
# Login guard
# ---------------------------------------------------------------------------

class TestGuardReservation(unittest.TestCase):
    def test_parallel_reservations_never_exceed_the_per_ip_limit(self):
        g = logic.LoginGuard()
        n = 60
        barrier = threading.Barrier(n)
        tickets, waits = [], []
        lock = threading.Lock()

        def go():
            barrier.wait()
            t, w = g.reserve("10.0.0.7")
            with lock:
                (tickets if t else waits).append(t or w)

        threads = [threading.Thread(target=go) for _ in range(n)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        self.assertEqual(len(tickets), 5)
        self.assertEqual(len(waits), n - 5)
        self.assertEqual(g.attempts_left("10.0.0.7"), 0)
        # the five checks fail: the IP is locked for 10 minutes, nothing more gets through
        results = [g.fail(t) for t in tickets]
        self.assertEqual(results[:4], [0, 0, 0, 0])
        self.assertGreater(results[4], 590)
        self.assertEqual(g.reserve("10.0.0.7")[0], None)
        self.assertGreater(g.remaining("10.0.0.7"), 590)
        self.assertEqual(g.remaining("10.0.0.8"), 0)  # other IPs unaffected

    def test_success_refunds_and_resets(self):
        g = logic.LoginGuard()
        t1, _ = g.reserve("10.0.0.7", now=100)
        g.fail(t1, now=100)
        t2, _ = g.reserve("10.0.0.7", now=101)
        self.assertEqual(g.attempts_left("10.0.0.7"), 3)
        g.succeed(t2, now=101)
        self.assertEqual(g.attempts_left("10.0.0.7"), 5)
        self.assertTrue(g.is_known("10.0.0.7", now=102))

    def test_correct_pin_while_others_in_flight_unlocks(self):
        g = logic.LoginGuard()
        tickets = [g.reserve("10.0.0.7", now=100)[0] for _ in range(5)]
        for t in tickets[:4]:
            g.fail(t, now=100)
        g.succeed(tickets[4], now=100)
        self.assertEqual(g.remaining("10.0.0.7", now=101), 0)

    def test_refund(self):
        g = logic.LoginGuard()
        t, _ = g.reserve("10.0.0.7", now=100)
        g.refund(t, now=100)
        self.assertEqual(g.attempts_left("10.0.0.7"), 5)

    def test_per_ip_lockout_window(self):
        g = logic.LoginGuard()
        for i in range(5):
            t, w = g.reserve("10.0.0.7", now=100 + i)
            self.assertEqual(w, 0)
            g.fail(t, now=100 + i)
        self.assertEqual(g.reserve("10.0.0.7", now=200), (None, 504))
        self.assertEqual(g.reserve("10.0.0.7", now=704)[1], 0)


class TestGuardKnownIps(unittest.TestCase):
    def flood(self, g, now=1000):
        for i in range(20):
            g.record_failure("10.0.1.%d" % i, now=now)

    def test_known_ip_is_not_slowed_down(self):
        g = logic.LoginGuard()
        g.record_success("192.168.0.20", now=900)
        self.flood(g)
        self.assertGreater(g.remaining("192.168.0.99", now=1001), 0)  # unknown: slowed down
        self.assertEqual(g.remaining("192.168.0.20", now=1001), 0)    # the owner: not
        t, w = g.reserve("192.168.0.20", now=1001)
        self.assertEqual(w, 0)
        g.succeed(t, now=1001)

    def test_known_ip_keeps_its_per_ip_lockout(self):
        g = logic.LoginGuard()
        g.record_success("192.168.0.20", now=900)
        for i in range(5):
            g.record_failure("192.168.0.20", now=1000 + i)
        self.assertGreater(g.remaining("192.168.0.20", now=1010), 0)

    def test_known_ips_expire_after_30_days_and_round_trip(self):
        g = logic.LoginGuard()
        g.load_known({"192.168.0.20": 1000, "192.168.0.21": 1000 - 31 * DAY, "bad": "x", 5: 1}, now=1000)
        self.assertEqual(g.known_snapshot(now=1000), {"192.168.0.20": 1000})
        self.assertFalse(g.is_known("192.168.0.20", now=1000 + 30 * DAY))
        self.flood(g, now=1000 + 30 * DAY)
        self.assertGreater(g.remaining("192.168.0.20", now=1000 + 30 * DAY + 1), 0)
        g.load_known(None)
        g.load_known(["x"])


# ---------------------------------------------------------------------------
# A real App on a temp config, served by a local test server
# ---------------------------------------------------------------------------

class ServerCase(unittest.TestCase):
    handler = app.Handler
    iterations = 1000

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        cfg = Config(os.path.join(self.tmp.name, "cfg.json"))
        cfg.set_pin_hash(logic.hash_pin(PIN, iterations=self.iterations))
        self.app = app.App(cfg, dry_run=True)
        self.old_app = app.Handler.app
        app.Handler.app = self.app
        self.srv = app.Server(("127.0.0.1", 0), self.handler)
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()
        self.port = self.srv.server_address[1]

    def tearDown(self):
        self.srv.shutdown()
        self.srv.server_close()
        app.Handler.app = self.old_app
        self.app.pool.shutdown(wait=False)
        self.tmp.cleanup()

    def req(self, method, path, body=None, cookie=None, host=None, csrf=True):
        c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        headers = {}
        if cookie:
            headers["Cookie"] = "pc_session=%s" % cookie
        if csrf and method == "POST":
            headers["X-Requested-With"] = "plex-couch"
        data = None
        if method == "POST":
            headers["Content-Type"] = "application/json"
            data = json.dumps(body or {}).encode()
        c.putrequest(method, path, skip_host=host is not None)
        if host is not None and host != "":
            c.putheader("Host", host)
        for k, v in headers.items():
            c.putheader(k, v)
        if data is not None:
            c.putheader("Content-Length", str(len(data)))
        c.endheaders(data)
        r = c.getresponse()
        raw = r.read()
        c.close()
        return r.status, r.headers, raw

    def cookie_value(self, headers):
        sc = headers.get("Set-Cookie") or ""
        return sc.split(";", 1)[0].split("=", 1)[1] if "=" in sc else None


class TestLoginBurst(ServerCase):
    def setUp(self):
        super().setUp()
        self.checks = []
        self.active = 0
        self.max_active = 0
        self.lock = threading.Lock()
        real = logic.verify_pin

        def counting(pin, stored):
            with self.lock:
                self.checks.append(pin)
                self.active += 1
                self.max_active = max(self.max_active, self.active)
            time.sleep(0.02)
            try:
                return real(pin, stored)
            finally:
                with self.lock:
                    self.active -= 1
        self.old_verify = logic.verify_pin
        logic.verify_pin = counting

    def tearDown(self):
        logic.verify_pin = self.old_verify
        super().tearDown()

    def test_parallel_burst_checks_at_most_five_pins_and_hides_the_right_one(self):
        n = 40
        socks = [socket.create_connection(("127.0.0.1", self.port)) for _ in range(n)]
        reqs = []
        for i in range(n):
            pin = PIN if i == n - 1 else "%06d" % (100000 + i)
            body = json.dumps({"pin": pin}).encode()
            reqs.append(b"POST /api/login HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Requested-With: plex-couch\r\n"
                        b"Content-Type: application/json\r\nConnection: close\r\nContent-Length: %d\r\n\r\n"
                        % len(body) + body)
        for s, r in zip(socks, reqs):
            s.sendall(r[:-1])
        time.sleep(0.3)
        for s, r in zip(socks, reqs):
            s.sendall(r[-1:])
        codes = []
        for s in socks:
            data = b""
            s.settimeout(10)
            while True:
                chunk = s.recv(65536)
                if not chunk:
                    break
                data += chunk
            s.close()
            codes.append(data.split(b" ", 2)[1].decode())
        self.assertLessEqual(len(self.checks), 5)
        self.assertEqual(self.max_active, 1)  # one PBKDF2 at a time
        self.assertNotIn("200", codes)
        self.assertNotIn(PIN, self.checks)
        self.assertEqual(codes.count("401") + codes.count("429"), n)
        # whatever is left of the 5 attempts, then the IP is locked: even the right PIN is refused
        used = len(self.checks)
        for i in range(5 - used):
            self.req("POST", "/api/login", {"pin": "000000"})
        self.assertEqual(len(self.checks), 5)
        status, _h, body = self.req("POST", "/api/login", {"pin": PIN})
        self.assertEqual(status, 429)
        self.assertIn("lockout_seconds", json.loads(body))
        self.assertEqual(len(self.checks), 5)

    def test_login_slots_refuse_beyond_the_cap(self):
        for _ in range(app.LOGIN_CONCURRENCY):
            self.assertTrue(self.app._login_slots.acquire(blocking=False))
        try:
            status, _h, body = self.req("POST", "/api/login", {"pin": PIN})
            self.assertEqual(status, 429)
            self.assertEqual(self.checks, [])
            self.assertEqual(self.app.guard.attempts_left("127.0.0.1"), 5)  # refunded
        finally:
            for _ in range(app.LOGIN_CONCURRENCY):
                self.app._login_slots.release()
        status, h, _b = self.req("POST", "/api/login", {"pin": PIN})
        self.assertEqual(status, 200)

    def test_success_remembers_the_ip_in_the_state_file(self):
        status, _h, _b = self.req("POST", "/api/login", {"pin": PIN})
        self.assertEqual(status, 200)
        state = self.app.state_file.load()
        self.assertIn("127.0.0.1", state["known_ips"])
        # the notification checks keep it (they only write their own keys)
        self.app.state_file.update({"movie_ready": {"primed": True}})
        self.assertIn("127.0.0.1", self.app.state_file.load()["known_ips"])
        # and a restart loads it
        again = app.App(self.app.config, dry_run=True)
        again.pool.shutdown(wait=False)
        self.assertTrue(again.guard.is_known("127.0.0.1"))


class TestSessions(ServerCase):
    def login(self):
        status, h, _b = self.req("POST", "/api/login", {"pin": PIN})
        self.assertEqual(status, 200)
        self.assertIn("Max-Age=%d" % (30 * DAY), h["Set-Cookie"])
        return self.cookie_value(h)

    def test_logout_all_invalidates_every_token(self):
        self.app.status = lambda: {"ok": True}
        a, b = self.login(), self.login()
        self.assertEqual(self.req("GET", "/api/status", cookie=a)[0], 200)
        self.assertEqual(self.req("POST", "/api/logout-all", cookie=a, csrf=False)[0], 403)  # CSRF
        self.assertEqual(self.req("POST", "/api/logout-all")[0], 401)  # auth
        status, h, _b = self.req("POST", "/api/logout-all", cookie=a)
        self.assertEqual(status, 200)
        self.assertIn("Max-Age=0", h["Set-Cookie"])
        self.assertEqual(self.req("GET", "/api/status", cookie=a)[0], 401)
        self.assertEqual(self.req("GET", "/api/status", cookie=b)[0], 401)
        self.assertEqual(self.app.config.session_epoch, 2)
        with open(self.app.config.path, encoding="utf-8") as f:
            self.assertEqual(json.load(f)["session_epoch"], 2)
        c = self.login()
        self.assertTrue(c.startswith("2."))
        self.assertEqual(self.req("GET", "/api/status", cookie=c)[0], 200)

    def test_logout_keeps_other_devices(self):
        self.app.status = lambda: {"ok": True}
        a, b = self.login(), self.login()
        self.assertEqual(self.req("POST", "/api/logout", cookie=a)[0], 200)
        self.assertEqual(self.req("GET", "/api/status", cookie=b)[0], 200)

    def test_sliding_renewal(self):
        self.app.status = lambda: {"ok": True}
        fresh = self.app.new_session()
        status, h, _b = self.req("GET", "/api/status", cookie=fresh)
        self.assertEqual(status, 200)
        self.assertIsNone(h.get("Set-Cookie"))
        old = self.app.new_session(now=time.time() - 16 * DAY)  # 14 days left
        status, h, _b = self.req("GET", "/api/status", cookie=old)
        self.assertEqual(status, 200)
        renewed = self.cookie_value(h)
        self.assertIn("HttpOnly", h["Set-Cookie"])
        exp = logic.session_expiry(self.app._session_key(), renewed, epoch=1)
        self.assertGreater(exp, time.time() + 29 * DAY)
        # POST actions renew too; poster images do not (they are cached)
        status, h, _b = self.req("POST", "/api/monitors/on", cookie=old)
        self.assertEqual(status, 200)
        self.assertIsNotNone(self.cookie_value(h))
        self.app.poster = lambda kind, ident: (b"img", "image/jpeg")
        status, h, _b = self.req("GET", "/img/movie/1", cookie=old)
        self.assertEqual(status, 200)
        self.assertIsNone(h.get("Set-Cookie"))

    def test_expired_after_30_days(self):
        self.app.status = lambda: {"ok": True}
        old = self.app.new_session(now=time.time() - 30 * DAY - 5)
        self.assertEqual(self.req("GET", "/api/status", cookie=old)[0], 401)
        ok, fresh = self.app.check_session(self.app.new_session(now=time.time() - 29 * DAY))
        self.assertTrue(ok)
        self.assertIsNotNone(fresh)

    def test_ui_has_the_logout_all_action(self):
        with open(os.path.join(app.STATIC_DIR, "index.html"), encoding="utf-8") as f:
            html = f.read()
        with open(os.path.join(app.STATIC_DIR, "app.js"), encoding="utf-8") as f:
            js = f.read()
        self.assertIn('<button type="button" class="btn-text body-strong" id="logout-all">'
                      'Ieși de pe toate dispozitivele</button>', html)
        self.assertLess(html.index('id="logout"'), html.index('id="logout-all"'))
        self.assertIn("'/api/logout-all'", js)
        self.assertIn("Ai ieșit de pe toate dispozitivele.", js)

    def test_old_two_part_tokens_are_rejected(self):
        key = self.app._session_key()
        exp = str(int(time.time()) + 100 * DAY)
        old_style = "%s.%s" % (exp, logic._sig(key, exp))
        self.assertFalse(self.app.session_ok(old_style))


# ---------------------------------------------------------------------------
# Host allowlist
# ---------------------------------------------------------------------------

class TestHostAllowed(unittest.TestCase):
    def test_allowed(self):
        for h in ("127.0.0.1", "127.0.0.1:8800", "192.168.0.5:8766", "10.1.2.3", "[::1]", "[::1]:8766",
                  "[fe80::1%25eth0]:1", "localhost", "localhost:8765", "LOCALHOST", "localhost.",
                  "plexpc.local", "PlexPC.local:8766", "a.b.local", "plexpc.local."):
            self.assertTrue(logic.host_allowed(h), h)

    def test_rejected(self):
        for h in (None, "", " ", "rebind.evil", "rebind.evil:8766", "evil.com", "local", ".local",
                  "plexpc.local.evil.com", "localhost.evil.com", "127.0.0.1.nip.io", "2130706433",
                  "a b", "x" * 300, "[nonsense]", "plexpc.lan"):
            self.assertFalse(logic.host_allowed(h), h)

    def test_configured_names(self):
        self.assertTrue(logic.host_allowed("plexpc.lan:8766", ["PlexPC.lan"]))
        self.assertTrue(logic.host_allowed("plexpc.lan", ["plexpc.lan."]))
        self.assertFalse(logic.host_allowed("other.lan", ["plexpc.lan"]))
        self.assertFalse(logic.host_allowed("plexpc.lan", [None, 5, ""]))


class TestHostOverHttp(ServerCase):
    def test_rebind_host_refused_everywhere(self):
        for method, path in (("GET", "/"), ("GET", "/api/status"), ("POST", "/api/login"), ("GET", "/app.js")):
            status, h, body = self.req(method, path, {"pin": PIN} if method == "POST" else None,
                                       host="rebind.evil")
            self.assertEqual(status, 421, path)
            self.assertEqual(body.decode("utf-8"), "Gazdă nepermisă.")
            self.assertTrue(h["Content-Type"].startswith("text/plain"))

    def test_missing_host_refused(self):
        self.assertEqual(self.req("GET", "/", host="")[0], 421)

    def test_allowed_hosts(self):
        self.assertEqual(self.req("GET", "/", host="127.0.0.1:%d" % self.port)[0], 200)
        self.assertEqual(self.req("GET", "/", host="localhost")[0], 200)
        self.assertEqual(self.req("GET", "/", host="plexpc.local")[0], 200)
        self.assertEqual(self.req("GET", "/", host="plexpc.lan")[0], 421)
        self.app.config.data["allowed_hosts"] = ["plexpc.lan"]
        self.assertEqual(self.req("GET", "/", host="plexpc.lan")[0], 200)

    def test_no_hsts_on_plain_http(self):
        _s, h, _b = self.req("GET", "/")
        self.assertIsNone(h.get("Strict-Transport-Security"))


class TestInstallHandlerHost(ServerCase):
    handler = app.InstallHandler

    def test_install_listener_checks_host_and_has_no_hsts(self):
        self.assertEqual(self.req("GET", "/", host="rebind.evil")[0], 421)
        self.assertEqual(self.req("GET", "/api/status", host="rebind.evil")[0], 421)
        status, h, _b = self.req("GET", "/")
        self.assertEqual(status, 200)
        self.assertIsNone(h.get("Strict-Transport-Security"))
        status, h, _b = self.req("GET", "/api/status")
        self.assertEqual(status, 301)
        self.assertIsNone(h.get("Strict-Transport-Security"))


class TestHstsOnSecureHandler(ServerCase):
    handler = app.SecureHandler  # the HTTPS listener's handler (TLS itself does not matter here)

    def test_hsts_on_every_response(self):
        for path in ("/", "/api/status", "/nope"):
            _s, h, _b = self.req("GET", path)
            self.assertEqual(h.get("Strict-Transport-Security"), "max-age=31536000", path)
        _s, h, _b = self.req("GET", "/", host="rebind.evil")
        self.assertEqual(h.get("Strict-Transport-Security"), "max-age=31536000")


# ---------------------------------------------------------------------------
# HTTPS required
# ---------------------------------------------------------------------------

class TestNoCertHandler(ServerCase):
    handler = app.NoCertHandler

    def test_only_a_503_page(self):
        for method, path in (("GET", "/"), ("GET", "/api/status"), ("POST", "/api/login"),
                             ("GET", "/app.js"), ("GET", "/ca.crt"), ("HEAD", "/")):
            status, h, body = self.req(method, path, {"pin": PIN} if method == "POST" else None)
            self.assertEqual(status, 503, path)
            self.assertTrue(h["Content-Type"].startswith("text/html"))
            if method != "HEAD":
                self.assertIn("make_certs.py", body.decode("utf-8"))
                self.assertIn("Certificatele HTTPS lipsesc", body.decode("utf-8"))
        self.assertEqual(self.req("GET", "/", host="rebind.evil")[0], 421)


class TestLoadTls(unittest.TestCase):
    def test_missing_and_unreadable(self):
        with tempfile.TemporaryDirectory() as d:
            cert, key = os.path.join(d, "server.pem"), os.path.join(d, "server.key")
            tls = {"cert": cert, "key": key}
            self.assertEqual(app.load_tls(tls), (None, "missing"))
            for p in (cert, key):
                with open(p, "w") as f:
                    f.write("not a certificate")
            ctx, problem = app.load_tls(tls)
            self.assertIsNone(ctx)
            self.assertTrue(problem.startswith("unreadable"))
            self.assertEqual(app.load_tls({"cert": "", "key": None}), (None, "missing"))


def _free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


class TestStartupWithoutCertificates(unittest.TestCase):
    """app.py main() in a child process, on a temp config whose certificates do not exist."""

    def start(self, *extra):
        self.tmp = tempfile.TemporaryDirectory()
        cfg = os.path.join(self.tmp.name, "cfg.json")
        with open(cfg, "w", encoding="utf-8") as f:
            # services on a closed port: the child never talks to a real Plex/Radarr
            closed = "http://127.0.0.1:9"
            json.dump({"pin_hash": logic.hash_pin(PIN, iterations=1000), "secret": "s" * 64,
                       "notifications": {"topic": None},
                       "services": {n: {"url": closed} for n in
                                    ("plex", "radarr", "bazarr", "prowlarr", "qbittorrent")}}, f)
        self.port, https_port = _free_port(), _free_port()
        cmd = [sys.executable, "-X", "utf8", os.path.join(ROOT, "app.py"), "--host", "127.0.0.1",
               "--port", str(self.port), "--https-port", str(https_port), "--config", cfg, "--dry-run"]
        self.proc = subprocess.Popen(cmd + list(extra), stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        self.addCleanup(self.stop)
        deadline = time.time() + 15
        while time.time() < deadline:
            try:
                socket.create_connection(("127.0.0.1", self.port), timeout=0.5).close()
                return
            except OSError:
                time.sleep(0.1)
        self.fail("app.py did not start")

    def stop(self):
        self.proc.kill()
        self.proc.wait(10)
        self.proc.stdout.close()
        self.tmp.cleanup()

    def get(self, path):
        c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=5)
        c.request("GET", path)
        r = c.getresponse()
        body = r.read()
        c.close()
        return r.status, body

    def test_missing_certificates_serve_only_503(self):
        self.start()
        status, body = self.get("/")
        self.assertEqual(status, 503)
        self.assertIn(b"make_certs.py", body)
        self.assertEqual(self.get("/api/status")[0], 503)

    def test_allow_http_serves_the_app(self):
        self.start("--allow-http")
        self.assertEqual(self.get("/")[0], 200)
        self.assertEqual(self.get("/api/status")[0], 401)


# ---------------------------------------------------------------------------
# LAN-only TV sessions
# ---------------------------------------------------------------------------

class TestTvSessions(unittest.TestCase):
    OWN = {"192.168.0.10"}

    def test_is_tv_player(self):
        for a in ("192.168.0.40", "10.0.0.129", "172.16.5.5", "fd00::5", "::ffff:192.168.0.40"):
            self.assertTrue(logic.is_tv_player(a, self.OWN), a)
        for a in ("192.168.0.10", "127.0.0.1", "::1", "localhost", "", None, "8.8.8.8", "2a00:1450:4001::1",
                  "85.120.10.1", "::ffff:8.8.8.8", "garbage"):
            self.assertFalse(logic.is_tv_player(a, self.OWN), a)

    def test_remote_viewers_are_not_the_tv(self):
        remote = {"key": "1", "state": "playing", "address": "85.120.10.1", "title": "X", "progress": 99}
        tv = {"key": "2", "state": "playing", "address": "192.168.0.40", "title": "Y", "progress": 10}
        self.assertEqual(logic.tv_sessions([remote, tv], self.OWN), [tv])
        _h, trig = logic.auto_monitor_decision(set(), [remote], self.OWN, True)
        self.assertFalse(trig)
        _h, trig = logic.auto_monitor_decision(set(), [tv], self.OWN, True)
        self.assertTrue(trig)
        self.assertIsNone(logic.after_movie_arm([remote], self.OWN, 0))
        st = logic.after_movie_arm([remote, tv], self.OWN, 0)
        self.assertEqual(st["key"], "2")

    def test_on_tv_flag_in_status(self):
        with tempfile.TemporaryDirectory() as d:
            a = app.App(Config(os.path.join(d, "cfg.json")), dry_run=True)
            a.pool.shutdown(wait=False)
            a.own_ips = lambda: self.OWN
            remote = a._public_playing({"title": "X", "address": "85.120.10.1"}, [])
            tv = a._public_playing({"title": "Y", "address": "192.168.0.40"}, [])
            self.assertEqual((remote["on_tv"], tv["on_tv"]), (False, True))
            self.assertNotIn("address", tv)


# ---------------------------------------------------------------------------
# Cancel vs. fire
# ---------------------------------------------------------------------------

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


class TestCancelRoute(ServerCase):
    def test_too_late_is_409(self):
        tok = self.app.new_session()
        self.app.actions.cancel_power = lambda: system.TOO_LATE
        status, _h, body = self.req("POST", "/api/power/cancel", cookie=tok)
        self.assertEqual(status, 409)
        self.assertIn("Prea târziu", json.loads(body)["error"])
        self.app.actions.cancel_power = lambda: system.ABORTED
        status, _h, body = self.req("POST", "/api/power/cancel", cookie=tok)
        self.assertEqual((status, json.loads(body)["outcome"]), (200, "aborted"))


# ---------------------------------------------------------------------------
# Connection cap and timeout
# ---------------------------------------------------------------------------

class SmallServer(app.Server):
    max_connections = 2


class TestConnectionCap(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.app = app.App(Config(os.path.join(self.tmp.name, "cfg.json")), dry_run=True)
        self.old_app = app.Handler.app
        app.Handler.app = self.app
        self.srv = SmallServer(("127.0.0.1", 0), app.Handler)
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()
        self.port = self.srv.server_address[1]

    def tearDown(self):
        self.srv.shutdown()
        self.srv.server_close()
        app.Handler.app = self.old_app
        self.app.pool.shutdown(wait=False)
        self.tmp.cleanup()

    def test_extra_connections_are_closed(self):
        idle = [socket.create_connection(("127.0.0.1", self.port)) for _ in range(2)]
        time.sleep(0.2)
        extra = socket.create_connection(("127.0.0.1", self.port))
        extra.settimeout(3)
        try:
            data = extra.recv(100)
        except ConnectionError:
            data = b""
        self.assertEqual(data, b"")  # closed without an answer
        extra.close()
        for s in idle:
            s.close()
        time.sleep(0.3)
        c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=5)
        c.request("GET", "/")
        self.assertEqual(c.getresponse().status, 200)
        c.close()

    def test_limits(self):
        self.assertEqual(app.Server.max_connections, 64)
        self.assertEqual(app.Handler.timeout, 15)
        self.assertEqual(app.CONNECTION_TIMEOUT, 15)


# ---------------------------------------------------------------------------
# Upstream redirects
# ---------------------------------------------------------------------------

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


# ---------------------------------------------------------------------------
# tools/make_certs.py
# ---------------------------------------------------------------------------

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
