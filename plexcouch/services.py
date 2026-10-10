"""Read-only clients for Plex, Radarr, Bazarr, Prowlarr and qBittorrent.

API keys/tokens are read at runtime from the local service config files (or the
app config) and kept only in memory. They are never logged or returned."""

import http.cookiejar
import json
import logging
import re
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

from . import logic

log = logging.getLogger("plexcouch")

TIMEOUT = 2.5
KEY_TTL = 300
IMG_TIMEOUT = 6
IMG_MAX_BYTES = 4 * 1024 * 1024


class _TTLCache:
    def __init__(self):
        self._d = {}
        self._lock = threading.Lock()

    def get(self, key, ttl, loader):
        now = time.monotonic()
        with self._lock:
            hit = self._d.get(key)
            if hit and now - hit[0] < ttl:
                return hit[1]
        value = loader()
        with self._lock:
            self._d[key] = (time.monotonic(), value)
        return value

    def drop(self, key):
        with self._lock:
            self._d.pop(key, None)


def _get_json(url, headers=None, timeout=TIMEOUT, opener=None):
    req = urllib.request.Request(url, headers=headers or {})
    req.add_header("Accept", "application/json")
    o = opener.open if opener else urllib.request.urlopen
    with o(req, timeout=timeout) as r:
        body = r.read()
    return json.loads(body.decode("utf-8")) if body else None


def _get_image(url, headers, timeout=IMG_TIMEOUT):
    """GET an image. Returns (bytes, content_type) or None if the upstream has none.
    Errors are logged by type only (never the URL or headers)."""
    req = urllib.request.Request(url, headers=headers or {})
    req.add_header("Accept", "image/*")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            ctype = (r.headers.get("Content-Type") or "").split(";", 1)[0].strip().lower()
            body = r.read(IMG_MAX_BYTES + 1)
    except (urllib.error.URLError, OSError, ValueError) as e:
        log.debug("image fetch failed: %s", type(e).__name__)
        return None
    if not body or len(body) > IMG_MAX_BYTES:
        return None
    return body, (ctype if ctype.startswith("image/") else "image/jpeg")


def read_radarr_key(path):
    with open(path, "r", encoding="utf-8-sig") as f:
        m = re.search(r"<ApiKey>\s*([^<\s]+)\s*</ApiKey>", f.read())
    return m.group(1) if m else ""


def parse_bazarr_apikey(lines):
    """The 'apikey:' line directly under the top-level 'auth:' block (no yaml lib)."""
    in_auth = False
    for line in lines:
        line = line.rstrip("\r\n")
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if not line[0].isspace():
            in_auth = line.split("#", 1)[0].strip() == "auth:"
            continue
        if in_auth:
            s = line.strip()
            if s.startswith("apikey:"):
                return s.split(":", 1)[1].strip().strip("'\"")
    return ""


def read_bazarr_key(path):
    with open(path, "r", encoding="utf-8-sig") as f:
        return parse_bazarr_apikey(f)


def read_plex_token(reg_key, reg_value):
    import winreg
    with winreg.OpenKey(winreg.HKEY_CURRENT_USER, reg_key) as k:
        return str(winreg.QueryValueEx(k, reg_value)[0])


class Services:
    def __init__(self, config):
        self.config = config
        self.cache = _TTLCache()
        self._fl_hash = {}  # torrent hash -> bool (is FileList); never stores URLs
        self._qb_jar = http.cookiejar.CookieJar()
        self._qb = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(self._qb_jar))
        self._qb_lock = threading.Lock()
        self.tracker_re = logic.compile_tracker_regex(config.data.get("filelist_tracker_regex"))

    # -- keys -------------------------------------------------------------
    def _key(self, name):
        def load():
            svc = self.config.service(name)
            try:
                if name == "plex":
                    return svc.get("token") or read_plex_token(
                        svc["token_registry_key"], svc["token_registry_value"])
                if name == "radarr":
                    return svc.get("api_key") or read_radarr_key(svc["config_xml"])
                if name == "bazarr":
                    return svc.get("api_key") or read_bazarr_key(svc["config_yaml"])
            except Exception as e:  # missing file / registry value
                log.warning("cannot read %s key: %s", name, type(e).__name__)
            return ""
        return self.cache.get(("key", name), KEY_TTL, load)

    def _url(self, name, path):
        return self.config.service(name).get("url", "").rstrip("/") + path

    # -- Plex -------------------------------------------------------------
    def plex_sessions_raw(self):
        token = self._key("plex")
        url = self._url("plex", "/status/sessions")
        headers = {"X-Plex-Token": token} if token else {}
        return _get_json(url, headers)

    def plex_sessions(self):
        return logic.parse_plex_sessions(self.plex_sessions_raw())

    def _plex_photo(self, rating_key, image, width, height):
        rating_key = str(rating_key)
        if not (rating_key.isascii() and rating_key.isdigit()):
            return None
        token = self._key("plex")
        src = "/library/metadata/%s/%s" % (rating_key, image)
        query = urllib.parse.urlencode({"width": width, "height": height, "minSize": 1, "upscale": 1, "url": src})
        headers = {"X-Plex-Token": token} if token else {}
        return _get_image(self._url("plex", "/photo/:/transcode?" + query), headers)

    def plex_poster(self, rating_key):
        """Poster for a Plex item, resized by Plex's photo transcoder (about 240x360)."""
        return self._plex_photo(rating_key, "thumb", 240, 360)

    def plex_art(self, rating_key):
        """Backdrop (art) for a Plex item, about 720x405."""
        return self._plex_photo(rating_key, "art", 720, 405)

    # -- Radarr -----------------------------------------------------------
    def _radarr(self, path):
        return _get_json(self._url("radarr", "/api/v3" + path), {"X-Api-Key": self._key("radarr")})

    def radarr_queue(self):
        data = self._radarr("/queue?includeMovie=true&pageSize=200")
        return (data or {}).get("records") or []

    def radarr_movies(self):
        return self.cache.get("radarr_movies", 30, lambda: self._radarr("/movie") or [])

    def radarr_history(self):
        """Recent 'grabbed' events: downloadId (torrent hash, upper case) -> movieId."""
        def load():
            data = self._radarr("/history?eventType=1&pageSize=200")
            return (data or {}).get("records") or []
        return self.cache.get("radarr_history", 60, load)

    def radarr_root_folders(self):
        """Radarr's root folders ([{"path": "D:\\Movies", ...}])."""
        return self.cache.get("radarr_rootfolder", 300, lambda: self._radarr("/rootfolder") or [])

    def radarr_poster(self, movie_id):
        """Radarr's local poster for a movie: the 500px copy, else the original."""
        return self._radarr_cover(movie_id, ("poster-500.jpg", "poster.jpg"))

    def radarr_fanart(self, movie_id):
        """Radarr's local backdrop for a movie: the 360px copy, else the original."""
        return self._radarr_cover(movie_id, ("fanart-360.jpg", "fanart.jpg"))

    def _radarr_cover(self, movie_id, names):
        movie_id = str(movie_id)
        if not (movie_id.isascii() and movie_id.isdigit()):
            return None
        headers = {"X-Api-Key": self._key("radarr")}
        for name in names:
            img = _get_image(self._url("radarr", "/api/v3/mediacover/%s/%s" % (movie_id, name)), headers)
            if img:
                return img
        return None

    # -- Bazarr -----------------------------------------------------------
    def bazarr_movies(self):
        def load():
            data = _get_json(self._url("bazarr", "/api/movies"),
                             {"X-API-KEY": self._key("bazarr")}, timeout=4)
            return (data or {}).get("data") or []
        return self.cache.get("bazarr_movies", 60, load)

    # -- Prowlarr ---------------------------------------------------------
    def prowlarr_ping(self):
        req = urllib.request.Request(self._url("prowlarr", "/ping"))
        with urllib.request.urlopen(req, timeout=TIMEOUT) as r:
            r.read()
        return True

    # -- qBittorrent ------------------------------------------------------
    def _qb_get(self, path):
        url = self._url("qbittorrent", "/api/v2" + path)
        try:
            return _get_json(url, opener=self._qb)
        except urllib.error.HTTPError as e:
            if e.code != 403:
                raise
        self._qb_login()
        return _get_json(url, opener=self._qb)

    def _qb_login(self):
        svc = self.config.service("qbittorrent")
        body = urllib.parse.urlencode({
            "username": svc.get("username") or "admin",
            "password": svc.get("password") or "x",
        }).encode()
        base = svc.get("url", "").rstrip("/")
        req = urllib.request.Request(base + "/api/v2/auth/login", data=body,
                                     headers={"Referer": base, "Origin": base})
        with self._qb_lock, self._qb.open(req, timeout=TIMEOUT) as r:
            r.read()

    def _is_filelist(self, t):
        h = t.get("hash") or ""
        if t.get("tracker"):
            return logic.is_filelist_tracker(t["tracker"], self.tracker_re)
        if h in self._fl_hash:
            return self._fl_hash[h]
        try:
            trackers = self._qb_get("/torrents/trackers?hash=" + urllib.parse.quote(h)) or []
        except Exception:
            return False
        result = any(logic.is_filelist_tracker(x.get("url"), self.tracker_re) for x in trackers)
        self._fl_hash[h] = result
        return result

    def filelist_torrents(self):
        info = self._qb_get("/torrents/info") or []
        return [t for t in info if self._is_filelist(t)]
