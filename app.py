"""Plex Couch - LAN-only phone remote for a Plex PC.

Usage:
  app.py [--host 0.0.0.0] [--port 8765] [--https-port 8766] [--cert F] [--key F] [--ca-cert F]
         [--config config.local.json] [--dry-run]
  app.py --set-pin 1234 [--config config.local.json]
  app.py --setup-ntfy [--new-topic] [--config config.local.json]

With a certificate and key (see tools/make_certs.py) the app is served over HTTPS on
--https-port, and --port only serves an install page, the CA certificate and redirects.
"""

import argparse
import base64
import hashlib
import json
import logging
import os
import re
import socket
import ssl
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from http import cookies
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from logging.handlers import RotatingFileHandler

from plexcouch import demo, logic, notify
from plexcouch.config import Config
from plexcouch.services import Services
from plexcouch.system import Actions, free_gb, own_ipv4s, pc_info

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
STATIC_DIR = os.path.join(BASE_DIR, "static")
LOG_PATH = os.path.join(BASE_DIR, "plex-couch.log")
COOKIE = "pc_session"
CSRF_HEADER = "X-Requested-With"
CSRF_VALUE = "plex-couch"
MAX_BODY = 4096
STATE_FILE = "state.local.json"  # next to the config: what was already notified
NOTIFY_POLL_SECONDS = 60
NOTIFY_TEST_TIMEOUT = 6  # below the page's 8 s request timeout

# pythonw.exe has no console: make sure nothing writes to a None stream.
if sys.stdout is None:
    sys.stdout = open(os.devnull, "w")
if sys.stderr is None:
    sys.stderr = open(os.devnull, "w")

log = logging.getLogger("plexcouch")

# Whitelist of static files: URL path -> (file in static/, content type). No directory serving.
STATIC_FILES = {
    "/": ("index.html", "text/html; charset=utf-8"),
    "/index.html": ("index.html", "text/html; charset=utf-8"),
    "/app.js": ("app.js", "text/javascript; charset=utf-8"),
    "/style.css": ("style.css", "text/css; charset=utf-8"),
    "/tokens.css": ("tokens.css", "text/css; charset=utf-8"),
    "/fonts/figtree-latin.woff2": ("fonts/figtree-latin.woff2", "font/woff2"),
    "/fonts/figtree-latin-ext.woff2": ("fonts/figtree-latin-ext.woff2", "font/woff2"),
    "/sw.js": ("sw.js", "text/javascript; charset=utf-8"),
    "/manifest.webmanifest": ("manifest.webmanifest", "application/manifest+json; charset=utf-8"),
    "/icon.svg": ("icon.svg", "image/svg+xml"),
    "/icon-192.png": ("icon-192.png", "image/png"),
    "/icon-512.png": ("icon-512.png", "image/png"),
    "/icon-maskable-512.png": ("icon-maskable-512.png", "image/png"),
    "/apple-touch-icon.png": ("apple-touch-icon.png", "image/png"),
}
TEMPLATED = {"index.html", "manifest.webmanifest"}
STATIC_HEADERS = {
    "/sw.js": {"Service-Worker-Allowed": "/"},
}
IMG_CACHE_CONTROL = "private, max-age=86400"
INSTALL_PAGE = "install.html"
CA_CONTENT_TYPE = "application/x-x509-ca-cert"

SECURITY_HEADERS = {
    "X-Content-Type-Options": "nosniff",
    "X-Frame-Options": "DENY",
    "Referrer-Policy": "no-referrer",
    "Content-Security-Policy": "default-src 'self'; img-src 'self' data:; style-src 'self'; "
                               "script-src 'self'; connect-src 'self'; frame-ancestors 'none'; "
                               "base-uri 'none'; form-action 'self'",
}


def setup_logging():
    handler = RotatingFileHandler(LOG_PATH, maxBytes=1_000_000, backupCount=2, encoding="utf-8")
    handler.setFormatter(logging.Formatter("%(asctime)s %(levelname)s %(message)s"))
    log.addHandler(handler)
    log.setLevel(logging.INFO)


def _html_escape(s):
    return (s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
            .replace('"', "&quot;"))


def _read_static(name):
    with open(os.path.join(STATIC_DIR, name), "rb") as f:
        return f.read()


def install_csp(page):
    """CSP for the install page: no scripts at all, only its own inline <style> (by hash)."""
    m = re.search(rb"<style>(.*?)</style>", page, re.S)
    style = "'none'"
    if m:
        style = "'sha256-%s'" % base64.b64encode(hashlib.sha256(m.group(1)).digest()).decode()
    return ("default-src 'none'; style-src %s; img-src 'self'; frame-ancestors 'none'; "
            "base-uri 'none'; form-action 'none'" % style)


def https_settings(args, config):
    """Resolve the HTTPS port and file paths: command line first, then config, then defaults."""
    h = config.data.get("https") or {}

    def cli(v):
        return os.path.abspath(v) if v else None

    return {
        "port": int(args.https_port if args.https_port is not None else h.get("port") or 8766),
        "cert": cli(args.cert) or config.resolve(h.get("cert")),
        "key": cli(args.key) or config.resolve(h.get("key")),
        "ca_cert": cli(args.ca_cert) or config.resolve(h.get("ca_cert")),
    }


def make_ssl_context(cert, key):
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.minimum_version = ssl.TLSVersion.TLSv1_2
    ctx.load_cert_chain(cert, key)
    return ctx


class App:
    def __init__(self, config, dry_run, demo_mode=False):
        self.config = config
        self.demo = demo_mode
        self.dry_run = dry_run or demo_mode
        config.ensure_secret()
        self.services = Services(config)
        self.actions = Actions(self.dry_run, config.app_name)
        self.guard = logic.LoginGuard()
        self.pool = ThreadPoolExecutor(max_workers=8, thread_name_prefix="svc")
        self._own_ips = set()
        self._own_ips_at = 0.0
        self.notifier = notify.Notifier(config, self.dry_run)
        self.state_file = notify.StateFile(os.path.join(os.path.dirname(config.path), STATE_FILE))
        self._disk_drive = "D:"
        # "Shut down after the movie": in memory only, a restart disarms it.
        self._after_movie = None
        self._after_movie_lock = threading.Lock()

    # -- auth ---------------------------------------------------------------
    def _session_key(self):
        return logic.session_key(self.config.data["secret"], self.config.data.get("pin_hash"))

    def new_session(self):
        return logic.sign_session(self._session_key())

    def session_ok(self, token):
        if self.demo:
            return True
        if not self.config.data.get("pin_hash"):
            return False
        return logic.verify_session(self._session_key(), token)

    def own_ips(self):
        if time.time() - self._own_ips_at > 300:
            self._own_ips = own_ipv4s()
            self._own_ips_at = time.time()
        return self._own_ips

    # -- status -------------------------------------------------------------
    def status(self):
        if self.demo:
            st = demo.demo_status(self.config.app_name, self.power_status(), self.config.settings)
            st["notifications"] = self.notifications_status()
            return st
        s = self.services
        jobs = {
            "plex": self.pool.submit(s.plex_sessions),
            "radarr_queue": self.pool.submit(s.radarr_queue),
            "radarr_movies": self.pool.submit(s.radarr_movies),
            "bazarr": self.pool.submit(s.bazarr_movies),
            "prowlarr": self.pool.submit(s.prowlarr_ping),
            "qbittorrent": self.pool.submit(s.filelist_torrents),
            "radarr_history": self.pool.submit(s.radarr_history),
        }
        results, errors = {}, {}
        deadline = time.monotonic() + 6
        for name, fut in jobs.items():
            try:
                results[name] = fut.result(timeout=max(0.1, deadline - time.monotonic()))
            except Exception as e:
                errors[name] = type(e).__name__
        for name, err in errors.items():
            log.debug("status: %s failed: %s", name, err)

        services = {
            "plex": "plex" in results,
            "radarr": "radarr_movies" in results or "radarr_queue" in results,
            "prowlarr": "prowlarr" in results,
            "bazarr": "bazarr" in results,
            "qbittorrent": "qbittorrent" in results,
        }
        queue = results.get("radarr_queue")
        movies = results.get("radarr_movies")
        titles = logic.torrent_title_map(results.get("radarr_history"), queue, movies)
        torrents = [logic.summarize_torrent(t, logic.torrent_title(t, titles))
                    for t in results.get("qbittorrent") or []]
        torrents.sort(key=lambda t: ({"pending": 0, "downloading": 1, "safe": 2}[t["hr"]], t["title"].lower()))
        ro_map = logic.bazarr_ro_map(results["bazarr"]) if "bazarr" in results else None
        queue_ids = {r.get("movieId") for r in queue or []}

        return {
            "app_name": self.config.app_name,
            "server_time": time.time(),
            "dry_run": self.dry_run,
            "pc": pc_info(self.config.data.get("disks") or []),
            "services": services,
            "now_playing": [self._public_playing(x, movies) for x in results.get("plex") or []],
            "downloads": None if queue is None else [logic.summarize_queue_record(r) for r in queue],
            "waiting": None if movies is None else logic.summarize_waiting(movies, queue_ids),
            "library": None if movies is None else logic.summarize_library(movies, ro_map),
            "torrents": None if "qbittorrent" not in results else torrents,
            "hr_warnings": logic.hr_warnings(torrents),
            "power": self.power_status(),
            "settings": self.config.settings,
            "notifications": self.notifications_status(),
        }

    def _public_playing(self, session, movies):
        out = logic.with_backdrop_fallback(logic.public_session(session), movies)
        out["on_tv"] = not logic.is_local_player(session.get("address"), self.own_ips())
        return out

    def power_status(self):
        """None, or the pending shutdown/restart ({"kind", "at"}, as before) plus "after_movie".
        Kept null when nothing is going on, so an older cached page still reads it right."""
        pending = self.actions.pending()
        with self._after_movie_lock:
            after = logic.public_after_movie(self._after_movie)
        if pending is None and after is None:
            return None
        out = dict(pending or {})
        out["after_movie"] = after
        return out

    def notifications_status(self):
        """Only ever returned to logged-in clients (/api/status); the topic is never logged."""
        n = self.notifier
        return {"enabled": n.enabled, "topic": n.topic, "server": n.server if n.enabled else None}

    # -- shut down after the movie -----------------------------------------
    def _sessions_now(self):
        if self.demo:
            return demo.demo_sessions()
        return self.services.plex_sessions()

    def arm_after_movie(self):
        """Returns (http_status, body)."""
        if self.actions.pending():
            return 409, {"error": "Există deja o oprire programată."}
        try:
            sessions = self._sessions_now()
        except Exception as e:
            log.debug("after-movie: plex poll failed: %s", type(e).__name__)
            return 503, {"error": "Plex nu răspunde. Repornește PC-ul sau verifică serviciul."}
        state = logic.after_movie_arm(sessions, self.own_ips(), time.time())
        if state is None:
            return 409, {"error": "Nu rulează nimic pe TV acum."}
        with self._after_movie_lock:
            self._after_movie = state
        log.info("after-movie shutdown armed")
        return 200, {"ok": True, "power": self.power_status()}

    def cancel_after_movie(self):
        with self._after_movie_lock:
            was = self._after_movie is not None
            self._after_movie = None
        if was:
            log.info("after-movie shutdown disarmed")
        return self.power_status()

    def after_movie_poll(self, sessions, now=None):
        """Called with every successful Plex poll. Starts the normal 60 s shutdown once."""
        now = time.time() if now is None else now
        with self._after_movie_lock:
            if self._after_movie is None:
                return None
            self._after_movie, action = logic.after_movie_step(self._after_movie, sessions,
                                                               self.own_ips(), now)
        if action == "expire":
            log.info("after-movie shutdown expired (nothing played for 3 h)")
        elif action == "fire":
            log.info("after-movie: the movie ended, shutting down")
            if self.actions.power("shutdown") is not None:
                self.notifier.send_async("Oprire după film",
                                         "PC-ul se oprește în 60 de secunde. Anulează din Plex Couch.",
                                         priority="high")
        return action

    # -- notifications -----------------------------------------------------
    def send_test_notification(self):
        """Returns (http_status, body)."""
        if not self.notifier.enabled:
            return 409, {"error": "Notificările nu sunt pornite. Rulează app.py --setup-ntfy."}
        if self.demo:
            return 200, {"ok": True}
        ok = self.notifier.send(self.config.app_name, "Test Plex Couch: notificările merg.",
                                timeout=NOTIFY_TEST_TIMEOUT)
        if not ok:
            return 502, {"error": "Notificarea nu a plecat. Verifică internetul PC-ului și reîncearcă."}
        return 200, {"ok": True}

    def _radarr_drive(self):
        try:
            for folder in self.services.radarr_root_folders() or []:
                d = logic.drive_of(folder.get("path"))
                if d:
                    self._disk_drive = d
                    break
        except Exception as e:
            log.debug("radarr root folders failed: %s", type(e).__name__)
        return self._disk_drive

    def notify_tick(self, now=None):
        """One round of the movie-ready and disk-low checks. Returns what was sent."""
        now = time.time() if now is None else now
        cfg = self.config.data.get("notifications") or {}
        try:
            movies = self.services.radarr_movies()
        except Exception as e:
            log.debug("notify: radarr failed: %s", type(e).__name__)
            movies = None
        try:
            ro_map = logic.bazarr_ro_map(self.services.bazarr_movies())
        except Exception as e:
            log.debug("notify: bazarr failed: %s", type(e).__name__)
            ro_map = None
        state = self.state_file.load()
        sent = []
        wait_s = float(cfg.get("ready_wait_minutes") or 0) * 60
        state["movie_ready"], ready = logic.movie_ready_step(state.get("movie_ready"), movies, ro_map,
                                                             now, wait_s)
        for text in ready:
            sent.append(("Film gata", text, ["movie_camera"], None))

        drive = self._radarr_drive()
        free = free_gb(drive)
        lows = state.get("disk_low") if isinstance(state.get("disk_low"), dict) else {}
        lows[drive], warn = logic.disk_low_step(lows.get(drive), free, float(cfg.get("disk_min_gb") or 0))
        state["disk_low"] = lows
        if warn:
            sent.append(("Spațiu puțin", logic.disk_low_text(drive, free), ["warning"], "high"))

        self.state_file.save(state)  # before sending: never twice, even after a crash
        for title, text, tags, prio in sent:
            self.notifier.send(title, text, tags=tags, priority=prio)
        return sent

    def notify_loop(self, stop):
        first = True
        while not stop.wait(0 if first else NOTIFY_POLL_SECONDS):
            first = False
            if not self.notifier.enabled:
                continue
            try:
                self.notify_tick()
            except Exception:
                log.exception("notify check failed")

    # -- posters ------------------------------------------------------------
    def poster(self, kind, ident):
        """(bytes, content_type) for /img/<kind>/<ident>, or None. ident is digits only."""
        if self.demo:
            return demo.poster(kind, ident)
        fetch = {
            "movie": self.services.radarr_poster,
            "plex": self.services.plex_poster,
            "movie-art": self.services.radarr_fanart,
            "plex-art": self.services.plex_art,
        }.get(kind)
        return fetch(ident) if fetch else None

    # -- background: auto monitors off -------------------------------------
    def auto_monitor_loop(self, stop):
        handled = set()
        primed = False
        interval = max(3, int(self.config.data.get("plex_poll_seconds") or 10))
        while not stop.wait(0 if not primed else interval):
            try:
                sessions = self.services.plex_sessions()
            except Exception as e:
                log.debug("auto-monitor poll failed: %s", type(e).__name__)
                if not primed:
                    primed = True  # wait an interval before retrying
                continue
            # First successful poll only records what is already playing (no trigger at startup).
            enabled = primed and bool(self.config.settings.get("auto_monitors_off"))
            handled, trigger = logic.auto_monitor_decision(handled, sessions, self.own_ips(), enabled)
            primed = True
            try:
                self.after_movie_poll(sessions)
            except Exception:
                log.exception("after-movie check failed")
            if trigger:
                try:
                    self.actions.monitors_off(reason="auto: film pornit pe TV")
                except Exception:
                    log.exception("auto monitors off failed")


class Handler(BaseHTTPRequestHandler):
    server_version = "PlexCouch"
    sys_version = ""
    protocol_version = "HTTP/1.1"
    timeout = 60
    app = None  # set in main()
    secure = False  # True on the HTTPS listener

    # quiet default logging (pythonw has no stderr); errors go to the log file
    def log_message(self, fmt, *args):
        pass

    @property
    def client_ip(self):
        return self.client_address[0]

    def _send(self, code, body=b"", ctype="application/json; charset=utf-8", headers=None):
        self.send_response(code)
        for k, v in SECURITY_HEADERS.items():
            self.send_header(k, v)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        headers = headers or {}
        if "Cache-Control" not in headers:
            self.send_header("Cache-Control", "no-store" if ctype.startswith("application/json") else "no-cache")
        for k, v in headers.items():
            self.send_header(k, v)
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def _json(self, code, obj, headers=None):
        self._send(code, json.dumps(obj, ensure_ascii=False).encode("utf-8"), headers=headers)

    def _token(self):
        raw = self.headers.get("Cookie")
        if not raw:
            return None
        try:
            c = cookies.SimpleCookie(raw)
        except cookies.CookieError:
            return None
        return c[COOKIE].value if COOKIE in c else None

    def _body(self):
        try:
            n = int(self.headers.get("Content-Length") or 0)
        except ValueError:
            n = 0
        if n <= 0:
            return {}
        if n > MAX_BODY:
            raise ValueError("body too large")
        data = json.loads(self.rfile.read(n).decode("utf-8") or "{}")
        return data if isinstance(data, dict) else {}

    def _allowed_ip(self):
        if logic.is_private_client(self.client_ip):
            return True
        log.warning("rejected non-private client %s", self.client_ip)
        self._json(403, {"error": "Acces permis doar din rețeaua locală."})
        return False

    # -- GET ------------------------------------------------------------------
    def do_HEAD(self):
        self.do_GET()

    def do_GET(self):
        if not self._allowed_ip():
            return
        path = self.path.split("?", 1)[0]
        if path.startswith("/api/"):
            if not self.app.session_ok(self._token()):
                return self._json(401, {"error": "Autentificare necesară."})
            if path == "/api/status":
                return self._json(200, self.app.status())
            return self._json(404, {"error": "Necunoscut."})
        if path.startswith("/img/"):
            return self._image(path)
        entry = STATIC_FILES.get(path)
        if not entry:
            return self._send(404, b"Not found", "text/plain; charset=utf-8")
        name, ctype = entry
        body = _read_static(name)
        if name in TEMPLATED:
            app_name = self.app.config.app_name
            safe = _html_escape(app_name) if name.endswith(".html") else json.dumps(app_name)[1:-1]
            body = body.replace(b"{{APP_NAME}}", safe.encode("utf-8"))
        self._send(200, body, ctype, headers=STATIC_HEADERS.get(path))

    def _image(self, path):
        """Poster proxy: same login and LAN checks as /api; the service keys stay server-side."""
        if not self.app.session_ok(self._token()):
            return self._json(401, {"error": "Autentificare necesară."})
        parsed = logic.parse_img_path(path)
        img = self.app.poster(*parsed) if parsed else None
        if not img:
            return self._send(404, b"Not found", "text/plain; charset=utf-8")
        body, ctype = img
        self._send(200, body, ctype, headers={"Cache-Control": IMG_CACHE_CONTROL})

    # -- POST -----------------------------------------------------------------
    def do_POST(self):
        if not self._allowed_ip():
            return
        path = self.path.split("?", 1)[0]
        if self.headers.get(CSRF_HEADER) != CSRF_VALUE:
            log.warning("csrf header missing on %s from %s", path, self.client_ip)
            return self._json(403, {"error": "Cerere respinsă (CSRF)."})
        try:
            body = self._body()
        except (ValueError, UnicodeDecodeError):
            return self._json(400, {"error": "Cerere invalidă."})

        if path == "/api/login":
            return self._login(body)
        if not self.app.session_ok(self._token()):
            return self._json(401, {"error": "Autentificare necesară."})

        a = self.app.actions
        if path == "/api/logout":
            return self._json(200, {"ok": True}, headers={
                "Set-Cookie": logic.session_cookie(COOKIE, "", 0, self.secure)})
        if path == "/api/monitors/off":
            log.info("action monitors/off from %s", self.client_ip)
            return self._json(200, {"ok": a.monitors_off(), "dry_run": self.app.dry_run})
        if path == "/api/monitors/on":
            log.info("action monitors/on from %s", self.client_ip)
            return self._json(200, {"ok": a.monitors_on(), "dry_run": self.app.dry_run})
        if path in ("/api/power/shutdown", "/api/power/restart"):
            kind = path.rsplit("/", 1)[1]
            log.info("action power/%s from %s", kind, self.client_ip)
            pending = a.power(kind)
            if pending is None:
                return self._json(500, {"error": "Comanda de oprire a eșuat (poate există deja una programată)."})
            return self._json(200, {"ok": True, "power": pending, "dry_run": self.app.dry_run})
        if path == "/api/power/cancel":
            log.info("action power/cancel from %s", self.client_ip)
            a.cancel_power()
            return self._json(200, {"ok": True, "power": None})
        if path == "/api/power/after-movie":
            log.info("action power/after-movie from %s", self.client_ip)
            code, out = self.app.arm_after_movie()
            return self._json(code, out)
        if path == "/api/power/after-movie/cancel":
            log.info("action power/after-movie/cancel from %s", self.client_ip)
            return self._json(200, {"ok": True, "power": self.app.cancel_after_movie()})
        if path == "/api/notify/test":
            log.info("action notify/test from %s", self.client_ip)
            code, out = self.app.send_test_notification()
            return self._json(code, out)
        if path == "/api/settings":
            changes = {}
            if isinstance(body.get("auto_monitors_off"), bool):
                changes["auto_monitors_off"] = body["auto_monitors_off"]
            if not changes:
                return self._json(400, {"error": "Setare invalidă."})
            if self.app.demo:
                self.app.config.data["settings"].update(changes)
                return self._json(200, {"ok": True, "settings": self.app.config.settings})
            self.app.config.update_settings(changes)
            log.info("settings changed %s from %s", changes, self.client_ip)
            return self._json(200, {"ok": True, "settings": self.app.config.settings})
        return self._json(404, {"error": "Necunoscut."})

    def _login(self, body):
        ip = self.client_ip
        guard = self.app.guard
        left = guard.remaining(ip)
        if left:
            return self._json(429, {"error": "Prea multe încercări. Așteaptă %d secunde." % left,
                                    "lockout_seconds": left})
        if self.app.demo:
            return self._json(200, {"ok": True})
        pin_hash = self.app.config.data.get("pin_hash")
        if not pin_hash:
            return self._json(503, {"error": "PIN-ul nu este setat. Rulează app.py --set-pin."})
        pin = body.get("pin")
        if isinstance(pin, str) and len(pin) <= 64 and logic.verify_pin(pin, pin_hash):
            guard.record_success(ip)
            log.info("login ok from %s", ip)
            token = self.app.new_session()
            cookie = logic.session_cookie(COOKIE, token, logic.SESSION_DAYS * 86400, self.secure)
            return self._json(200, {"ok": True}, headers={"Set-Cookie": cookie})
        locked = guard.record_failure(ip)
        log.warning("login failed from %s", ip)
        if locked:
            return self._json(429, {"error": "Prea multe încercări. Așteaptă %d secunde." % locked,
                                    "lockout_seconds": locked})
        return self._json(401, {"error": "PIN greșit.", "attempts_left": guard.attempts_left(ip)})


class SecureHandler(Handler):
    secure = True


class InstallHandler(Handler):
    """Plain-HTTP listener while HTTPS is on: install page, CA certificate, redirects. Nothing else."""
    https_port = 8766
    ca_cert = ""

    def _https_url(self, path):
        local = self.connection.getsockname()[0]
        return logic.https_url(self.headers.get("Host"), self.https_port, path, fallback_host=local)

    def do_GET(self):
        if not self._allowed_ip():
            return
        path = self.path.split("?", 1)[0]
        if path == "/":
            return self._install_page()
        if path == "/ca.crt":
            try:
                with open(self.ca_cert, "rb") as f:
                    der = logic.cert_der(f.read())
            except (OSError, ValueError):
                return self._send(404, b"Not found", "text/plain; charset=utf-8")
            return self._send(200, der, CA_CONTENT_TYPE, headers={
                "Content-Disposition": 'attachment; filename="plex-couch-ca.crt"'})
        self._redirect()

    def do_POST(self):
        if not self._allowed_ip():
            return
        self._redirect()

    def _install_page(self):
        page = _read_static(INSTALL_PAGE)
        page = (page.replace(b"{{APP_NAME}}", _html_escape(self.app.config.app_name).encode("utf-8"))
                .replace(b"{{HTTPS_URL}}", _html_escape(self._https_url("/")).encode("utf-8")))
        self.send_response(200)
        for k, v in SECURITY_HEADERS.items():
            if k != "Content-Security-Policy":
                self.send_header(k, v)
        self.send_header("Content-Security-Policy", install_csp(page))
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(page)))
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(page)

    def _redirect(self):
        self._send(301, b"", "text/plain; charset=utf-8", headers={"Location": self._https_url(self.path)})


class Server(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = False

    def handle_error(self, request, client_address):
        exc = sys.exc_info()[1]
        if isinstance(exc, (ssl.SSLError, ConnectionError, socket.timeout)):
            # Clients dropping keep-alive connections or failing TLS: not worth a traceback.
            log.debug("connection error from %s: %s", client_address[0], type(exc).__name__)
            return
        log.exception("request error from %s", client_address[0])


class TLSServer(Server):
    """HTTPS server. The TLS handshake runs in the request thread, not in the accept loop,
    so a slow or broken client cannot stall other connections."""

    def __init__(self, address, handler, context):
        self.context = context
        super().__init__(address, handler)

    def finish_request(self, request, client_address):
        request.settimeout(Handler.timeout)
        try:
            tls = self.context.wrap_socket(request, server_side=True)
        except (ssl.SSLError, OSError) as e:
            log.debug("tls handshake failed from %s: %s", client_address[0], type(e).__name__)
            return
        try:
            self.RequestHandlerClass(tls, client_address, self)
        finally:
            try:
                tls.close()
            except OSError:
                pass


def setup_ntfy(config, replace=False):
    """--setup-ntfy: make sure the config has a topic, print it and how to subscribe."""
    notifier = notify.Notifier(config, dry_run=True)
    topic = notifier.topic
    if topic is None or replace:
        topic = notify.new_topic()
        config.set_notification_topic(topic)
        log.info("ntfy topic %s via --setup-ntfy", "replaced" if replace else "created")
        print("New ntfy topic saved to %s" % config.path)
    else:
        print("Keeping the existing ntfy topic (use --new-topic to replace it).")
    print("Topic: %s" % topic)
    print("Subscribe to it in the ntfy app (server %s), or open: %s"
          % (notifier.server, notify.subscribe_url(notifier.server, topic)))
    print("Anyone who knows the topic can read the notifications: keep it private.")
    print("Restart Plex Couch to start sending notifications.")
    return 0


def main(argv=None):
    p = argparse.ArgumentParser(description="Plex Couch")
    p.add_argument("--host", default="0.0.0.0")
    p.add_argument("--port", type=int, default=8765,
                   help="HTTP port: the whole app without a certificate, else only the install page")
    p.add_argument("--https-port", type=int, help="HTTPS port (default 8766, or https.port in the config)")
    p.add_argument("--cert", help="server certificate, PEM (default certs/server.pem)")
    p.add_argument("--key", help="server private key, PEM (default certs/server.key)")
    p.add_argument("--ca-cert", help="CA certificate offered at /ca.crt, DER or PEM (default certs/ca.crt)")
    p.add_argument("--config", default=os.path.join(BASE_DIR, "config.local.json"))
    p.add_argument("--dry-run", action="store_true", help="log monitor/power actions, never execute them")
    p.add_argument("--set-pin", metavar="DIGITS", help="store the PIN hash in the config and exit")
    p.add_argument("--setup-ntfy", action="store_true",
                   help="create an ntfy topic for phone notifications (keeps an existing one), print it, exit")
    p.add_argument("--new-topic", action="store_true", help="with --setup-ntfy: replace the existing topic")
    p.add_argument("--demo", action="store_true",
                   help="serve the UI with fake data and no login (screenshots only; 127.0.0.1 only)")
    args = p.parse_args(argv)

    setup_logging()
    config = Config(args.config)

    if args.set_pin is not None:
        if not logic.valid_pin_format(args.set_pin):
            print("PIN must be 4-6 digits.")
            return 2
        config.set_pin_hash(logic.hash_pin(args.set_pin))
        log.info("PIN changed via --set-pin")
        print("PIN saved to %s" % config.path)
        return 0

    if args.setup_ntfy:
        return setup_ntfy(config, args.new_topic)

    if args.demo and args.host != "127.0.0.1":
        print("--demo only runs with --host 127.0.0.1")
        return 2
    app = App(config, args.dry_run, demo_mode=args.demo)
    Handler.app = app
    tls = https_settings(args, config)
    servers = []
    if os.path.isfile(tls["cert"] or "") and os.path.isfile(tls["key"] or ""):
        try:
            context = make_ssl_context(tls["cert"], tls["key"])
        except (ssl.SSLError, OSError) as e:
            log.error("cannot load the HTTPS certificate/key: %s", type(e).__name__)
            print("Cannot load the HTTPS certificate/key: %s" % e)
            return 2
        InstallHandler.https_port = tls["port"]
        InstallHandler.ca_cert = tls["ca_cert"]
        servers.append(TLSServer((args.host, tls["port"]), SecureHandler, context))
        servers.append(Server((args.host, args.port), InstallHandler))
        where = "https %s:%d, http %s:%d (install page)" % (args.host, tls["port"], args.host, args.port)
    else:
        servers.append(Server((args.host, args.port), Handler))
        where = "http %s:%d (no certificate, HTTPS off)" % (args.host, args.port)

    stop = threading.Event()
    if not args.demo:
        threading.Thread(target=app.auto_monitor_loop, args=(stop,), name="auto-monitor", daemon=True).start()
        threading.Thread(target=app.notify_loop, args=(stop,), name="notify", daemon=True).start()
    for srv in servers[1:]:
        threading.Thread(target=srv.serve_forever, name="http", daemon=True).start()
    log.info("started on %s%s", where, " (demo)" if args.demo else (" (dry-run)" if args.dry_run else ""))
    if not config.data.get("pin_hash"):
        log.warning("no PIN set; run app.py --set-pin <digits>")
    try:
        servers[0].serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        stop.set()
        for srv in servers[1:]:
            srv.shutdown()
        for srv in servers:
            srv.server_close()
        log.info("stopped")
    return 0


if __name__ == "__main__":
    sys.exit(main())
