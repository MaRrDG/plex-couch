"""Plex Couch - LAN-only phone remote for a Plex PC.

Usage:
  app.py [--host 0.0.0.0] [--port 8765] [--config config.local.json] [--dry-run]
  app.py --set-pin 1234 [--config config.local.json]
"""

import argparse
import json
import logging
import os
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from http import cookies
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from logging.handlers import RotatingFileHandler

from plexcouch import demo, logic
from plexcouch.config import Config
from plexcouch.services import Services
from plexcouch.system import Actions, own_ipv4s, pc_info

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
STATIC_DIR = os.path.join(BASE_DIR, "static")
LOG_PATH = os.path.join(BASE_DIR, "plex-couch.log")
COOKIE = "pc_session"
CSRF_HEADER = "X-Requested-With"
CSRF_VALUE = "plex-couch"
MAX_BODY = 4096

# pythonw.exe has no console: make sure nothing writes to a None stream.
if sys.stdout is None:
    sys.stdout = open(os.devnull, "w")
if sys.stderr is None:
    sys.stderr = open(os.devnull, "w")

log = logging.getLogger("plexcouch")

STATIC_FILES = {
    "/": ("index.html", "text/html; charset=utf-8"),
    "/index.html": ("index.html", "text/html; charset=utf-8"),
    "/app.js": ("app.js", "text/javascript; charset=utf-8"),
    "/style.css": ("style.css", "text/css; charset=utf-8"),
    "/manifest.webmanifest": ("manifest.webmanifest", "application/manifest+json; charset=utf-8"),
    "/icon.svg": ("icon.svg", "image/svg+xml"),
}
TEMPLATED = {"index.html", "manifest.webmanifest"}

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
            return demo.demo_status(self.config.app_name, self.actions.pending(), self.config.settings)
        s = self.services
        jobs = {
            "plex": self.pool.submit(s.plex_sessions),
            "radarr_queue": self.pool.submit(s.radarr_queue),
            "radarr_movies": self.pool.submit(s.radarr_movies),
            "bazarr": self.pool.submit(s.bazarr_movies),
            "prowlarr": self.pool.submit(s.prowlarr_ping),
            "qbittorrent": self.pool.submit(s.filelist_torrents),
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
        torrents = [logic.summarize_torrent(t) for t in results.get("qbittorrent") or []]
        torrents.sort(key=lambda t: ({"pending": 0, "downloading": 1, "safe": 2}[t["hr"]], t["name"].lower()))
        ro_map = logic.bazarr_ro_map(results["bazarr"]) if "bazarr" in results else None
        queue_ids = {r.get("movieId") for r in queue or []}

        return {
            "app_name": self.config.app_name,
            "server_time": time.time(),
            "dry_run": self.dry_run,
            "pc": pc_info(self.config.data.get("disks") or []),
            "services": services,
            "now_playing": [logic.public_session(x) for x in results.get("plex") or []],
            "downloads": None if queue is None else [logic.summarize_queue_record(r) for r in queue],
            "waiting": None if movies is None else logic.summarize_waiting(movies, queue_ids),
            "library": None if movies is None else logic.summarize_library(movies, ro_map),
            "torrents": None if "qbittorrent" not in results else torrents,
            "hr_warnings": logic.hr_warnings(torrents),
            "power": self.actions.pending(),
            "settings": self.config.settings,
        }

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
            if trigger:
                try:
                    self.actions.monitors_off(reason="auto: film pornit pe TV")
                except Exception:
                    log.exception("auto monitors off failed")


class Handler(BaseHTTPRequestHandler):
    server_version = "PlexCouch"
    sys_version = ""
    protocol_version = "HTTP/1.1"
    app = None  # set in main()

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
        self.send_header("Cache-Control", "no-store" if ctype.startswith("application/json") else "no-cache")
        for k, v in (headers or {}).items():
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
        entry = STATIC_FILES.get(path)
        if not entry:
            return self._send(404, b"Not found", "text/plain; charset=utf-8")
        name, ctype = entry
        with open(os.path.join(STATIC_DIR, name), "rb") as f:
            body = f.read()
        if name in TEMPLATED:
            app_name = self.app.config.app_name
            safe = _html_escape(app_name) if name.endswith(".html") else json.dumps(app_name)[1:-1]
            body = body.replace(b"{{APP_NAME}}", safe.encode("utf-8"))
        self._send(200, body, ctype)

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
                "Set-Cookie": "%s=; Max-Age=0; HttpOnly; SameSite=Strict; Path=/" % COOKIE})
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
            cookie = "%s=%s; Max-Age=%d; HttpOnly; SameSite=Strict; Path=/" % (
                COOKIE, token, logic.SESSION_DAYS * 86400)
            return self._json(200, {"ok": True}, headers={"Set-Cookie": cookie})
        locked = guard.record_failure(ip)
        log.warning("login failed from %s", ip)
        if locked:
            return self._json(429, {"error": "Prea multe încercări. Așteaptă %d secunde." % locked,
                                    "lockout_seconds": locked})
        return self._json(401, {"error": "PIN greșit.", "attempts_left": guard.attempts_left(ip)})


class Server(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = False

    def handle_error(self, request, client_address):
        log.exception("request error from %s", client_address[0])


def main(argv=None):
    p = argparse.ArgumentParser(description="Plex Couch")
    p.add_argument("--host", default="0.0.0.0")
    p.add_argument("--port", type=int, default=8765)
    p.add_argument("--config", default=os.path.join(BASE_DIR, "config.local.json"))
    p.add_argument("--dry-run", action="store_true", help="log monitor/power actions, never execute them")
    p.add_argument("--set-pin", metavar="DIGITS", help="store the PIN hash in the config and exit")
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

    if args.demo and args.host != "127.0.0.1":
        print("--demo only runs with --host 127.0.0.1")
        return 2
    app = App(config, args.dry_run, demo_mode=args.demo)
    Handler.app = app
    stop = threading.Event()
    if not args.demo:
        threading.Thread(target=app.auto_monitor_loop, args=(stop,), name="auto-monitor", daemon=True).start()
    server = Server((args.host, args.port), Handler)
    log.info("started on %s:%d%s", args.host, args.port,
             " (demo)" if args.demo else (" (dry-run)" if args.dry_run else ""))
    if not config.data.get("pin_hash"):
        log.warning("no PIN set; run app.py --set-pin <digits>")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        stop.set()
        server.server_close()
        log.info("stopped")
    return 0


if __name__ == "__main__":
    sys.exit(main())
