"""Phone notifications through ntfy (https://ntfy.sh or a self-hosted server), plus the small
state file that remembers what was already notified.

The topic name is the only "secret": anyone who knows it can read the notifications. It is
kept in the config file, returned only to logged-in clients and never logged."""

import ipaddress
import json
import logging
import os
import secrets
import threading
import urllib.error
import urllib.parse
import urllib.request

log = logging.getLogger("plexcouch")

DEFAULT_SERVER = "https://ntfy.sh"
TIMEOUT = 10
TOPIC_PREFIX = "plexcouch-"
# ntfy priorities: 1 min, 2 low, 3 default, 4 high, 5 max/urgent.
PRIORITIES = {"min": 1, "low": 2, "default": 3, "high": 4, "max": 5, "urgent": 5}


def new_topic():
    """A hard-to-guess topic name: 'plexcouch-' + 24 URL-safe random characters."""
    return TOPIC_PREFIX + secrets.token_urlsafe(18)


def subscribe_url(server, topic):
    return "%s/%s" % ((server or DEFAULT_SERVER).rstrip("/"), topic)


def build_payload(topic, title, message, tags=None, priority=None, click=None):
    """The JSON body for ntfy's JSON publish API (POST to the server root). JSON keeps
    Romanian diacritics intact, unlike HTTP headers."""
    body = {"topic": topic, "title": title, "message": message}
    if tags:
        body["tags"] = list(tags)
    if priority is not None:
        p = PRIORITIES.get(priority, priority) if isinstance(priority, str) else priority
        if isinstance(p, int) and 1 <= p <= 5:
            body["priority"] = p
    if click:
        body["click"] = click
    return body


def is_loopback_url(url):
    host = (urllib.parse.urlsplit(url or "").hostname or "").lower()
    if host == "localhost":
        return True
    try:
        return ipaddress.ip_address(host).is_loopback
    except ValueError:
        return False


class Notifier:
    """Sends ntfy notifications. Disabled while no topic is configured.

    send() never raises; failures are logged by type only (the URL contains the topic).
    In dry-run it only logs, except towards a server on this machine (127.0.0.1/localhost),
    which is how the app is tested with a fake ntfy server: nothing ever reaches the internet."""

    def __init__(self, config, dry_run):
        self.config = config
        self.dry_run = dry_run

    @property
    def _settings(self):
        return self.config.data.get("notifications") or {}

    @property
    def topic(self):
        t = self._settings.get("topic")
        return t if isinstance(t, str) and t.strip() else None

    @property
    def server(self):
        return str(self._settings.get("server") or DEFAULT_SERVER).rstrip("/")

    @property
    def enabled(self):
        return self.topic is not None

    def _post(self, url, data, timeout):
        req = urllib.request.Request(url, data=data, method="POST",
                                     headers={"Content-Type": "application/json; charset=utf-8"})
        with urllib.request.urlopen(req, timeout=timeout) as r:
            r.read()
            return r.status

    def send(self, title, message, tags=None, priority=None, click=None, timeout=TIMEOUT):
        """True if the notification was accepted (or only logged in dry-run)."""
        topic = self.topic
        if topic is None:
            return False
        if self.dry_run and not is_loopback_url(self.server):
            log.info("dry-run: notify %s", title)
            return True
        body = json.dumps(build_payload(topic, title, message, tags, priority, click),
                          ensure_ascii=False).encode("utf-8")
        try:
            status = self._post(self.server + "/", body, timeout)
        except urllib.error.HTTPError as e:
            log.warning("notify %r failed: HTTP %s", title, e.code)
            e.close()
            return False
        except Exception as e:  # never let a notification break the caller
            log.warning("notify %r failed: %s", title, type(e).__name__)
            return False
        log.info("notify sent: %s", title)
        return 200 <= (status or 200) < 300

    def send_async(self, *args, **kwargs):
        threading.Thread(target=self.send, args=args, kwargs=kwargs, name="notify", daemon=True).start()


class StateFile:
    """A small JSON file (state.local.json next to the config) for what was already notified,
    so a restart does not repeat messages. A missing or broken file reads as empty."""

    def __init__(self, path):
        self.path = os.path.abspath(path)
        self._lock = threading.Lock()

    def load(self):
        with self._lock:
            try:
                with open(self.path, "r", encoding="utf-8") as f:
                    data = json.load(f)
            except (OSError, ValueError):
                return {}
            return data if isinstance(data, dict) else {}

    def save(self, data):
        with self._lock:
            tmp = self.path + ".tmp"
            try:
                with open(tmp, "w", encoding="utf-8") as f:
                    json.dump(data, f, indent=2, ensure_ascii=False, sort_keys=True)
                    f.write("\n")
                os.replace(tmp, self.path)
            except OSError as e:
                log.warning("cannot save the state file: %s", type(e).__name__)
