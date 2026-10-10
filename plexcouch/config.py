"""Config file handling. The config holds the PIN hash, the cookie-signing secret,
user settings and (overridable) service locations. Never log its contents."""

import copy
import json
import os
import secrets
import threading

DEFAULTS = {
    "app_name": "Plex Couch",
    "pin_hash": "",
    "secret": "",
    "settings": {
        "auto_monitors_off": True,
    },
    "filelist_tracker_regex": r"thefl\.org|filelist\.(io|ro)|flro\.org",
    "disks": ["C:", "D:"],
    "plex_poll_seconds": 10,
    # Extra Host names the app answers to, besides IP addresses, localhost and *.local
    # (protection against DNS rebinding). Example: ["plexpc.lan"].
    "allowed_hosts": [],
    # Part of every session token; "Ieși de pe toate dispozitivele" increments it, which
    # invalidates every session at once. Written by the app.
    "session_epoch": 1,
    # Phone notifications through ntfy. No topic = off; app.py --setup-ntfy creates one.
    "notifications": {
        "server": "https://ntfy.sh",
        "topic": None,
        "disk_min_gb": 60,
        "ready_wait_minutes": 20,
    },
    # HTTPS for the installable PWA. Relative paths are resolved against the config file's
    # folder. If the certificate or key is missing or unreadable, only a 503 page is served
    # (the whole app over plain HTTP needs app.py --allow-http).
    "https": {
        "port": 8766,
        "cert": "certs/server.pem",
        "key": "certs/server.key",
        "ca_cert": "certs/ca.crt",
    },
    "services": {
        "plex": {
            "url": "http://127.0.0.1:32400",
            "token": "",
            "token_registry_key": r"Software\Plex, Inc.\Plex Media Server",
            "token_registry_value": "PlexOnlineToken",
        },
        "radarr": {
            "url": "http://127.0.0.1:7878",
            "api_key": "",
            "config_xml": r"C:\ProgramData\Radarr\config.xml",
        },
        "bazarr": {
            "url": "http://127.0.0.1:6767",
            "api_key": "",
            "config_yaml": r"C:\ProgramData\Bazarr\config\config.yaml",
        },
        "prowlarr": {
            "url": "http://127.0.0.1:9696",
        },
        "qbittorrent": {
            "url": "http://127.0.0.1:8080",
            "username": "",
            "password": "",
        },
    },
}

# Keys the app writes itself. Everything else is user configuration.
SETTINGS_KEYS = {"auto_monitors_off": bool}


def _merge(base, override):
    out = copy.deepcopy(base)
    for k, v in (override or {}).items():
        if isinstance(v, dict) and isinstance(out.get(k), dict):
            out[k] = _merge(out[k], v)
        else:
            out[k] = v
    return out


class Config:
    def __init__(self, path):
        self.path = os.path.abspath(path)
        self._lock = threading.Lock()
        self._raw = {}
        self.data = copy.deepcopy(DEFAULTS)
        self.load()

    def load(self):
        raw = {}
        if os.path.exists(self.path):
            with open(self.path, "r", encoding="utf-8-sig") as f:
                raw = json.load(f)
        self._raw = raw
        self.data = _merge(DEFAULTS, raw)

    def _save_locked(self):
        tmp = self.path + ".tmp"
        d = os.path.dirname(self.path)
        if d:
            os.makedirs(d, exist_ok=True)
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(self._raw, f, indent=2, ensure_ascii=False)
            f.write("\n")
        os.replace(tmp, self.path)

    def ensure_secret(self):
        """Generate the cookie-signing secret on first run."""
        with self._lock:
            if not self._raw.get("secret"):
                self._raw["secret"] = secrets.token_hex(32)
                self._save_locked()
            self.data["secret"] = self._raw["secret"]
        return self.data["secret"]

    def set_pin_hash(self, pin_hash):
        with self._lock:
            self._raw["pin_hash"] = pin_hash
            if not self._raw.get("secret"):
                self._raw["secret"] = secrets.token_hex(32)
            self._raw.setdefault("settings", {}).setdefault("auto_monitors_off", True)
            self._save_locked()
            self.data = _merge(DEFAULTS, self._raw)

    def set_notification_topic(self, topic):
        with self._lock:
            n = self._raw.get("notifications")
            if not isinstance(n, dict):
                n = self._raw["notifications"] = {}
            n["topic"] = topic
            self._save_locked()
            self.data = _merge(DEFAULTS, self._raw)

    @property
    def session_epoch(self):
        try:
            return max(1, int(self.data.get("session_epoch") or 1))
        except (TypeError, ValueError):
            return 1

    def bump_session_epoch(self):
        """Invalidate every session token. Returns the new epoch."""
        with self._lock:
            try:
                cur = max(1, int(self._raw.get("session_epoch") or 1))
            except (TypeError, ValueError):
                cur = 1
            self._raw["session_epoch"] = cur + 1
            self._save_locked()
            self.data = _merge(DEFAULTS, self._raw)
            return cur + 1

    @property
    def allowed_hosts(self):
        h = self.data.get("allowed_hosts")
        return [x for x in h if isinstance(x, str)] if isinstance(h, list) else []

    def update_settings(self, changes):
        with self._lock:
            s = self._raw.setdefault("settings", {})
            for k, v in changes.items():
                if k in SETTINGS_KEYS:
                    s[k] = SETTINGS_KEYS[k](v)
            self._save_locked()
            self.data = _merge(DEFAULTS, self._raw)

    @property
    def settings(self):
        return dict(self.data["settings"])

    @property
    def app_name(self):
        return str(self.data.get("app_name") or DEFAULTS["app_name"])

    def resolve(self, value):
        """Resolve a path from the config relative to the config file's folder."""
        if not value:
            return ""
        value = os.path.expandvars(os.path.expanduser(str(value)))
        if os.path.isabs(value):
            return value
        return os.path.normpath(os.path.join(os.path.dirname(self.path), value))

    def service(self, name):
        return self.data["services"].get(name, {})
