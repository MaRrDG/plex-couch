"""Pure logic: no network, no Windows calls. Everything here is unit-tested."""

import hashlib
import hmac
import ipaddress
import math
import ntpath
import re
import secrets
import threading
import time
from datetime import datetime, timezone

# ---------------------------------------------------------------------------
# FileList hit & run rule
# ---------------------------------------------------------------------------

DEFAULT_FILELIST_REGEX = r"thefl\.org|filelist\.(io|ro)|flro\.org"
FILELIST_RE = re.compile(DEFAULT_FILELIST_REGEX, re.IGNORECASE)
HR_RATIO = 1.0
HR_SEED_HOURS = 48.0


def compile_tracker_regex(pattern=None):
    return re.compile(pattern or DEFAULT_FILELIST_REGEX, re.IGNORECASE)


def is_filelist_tracker(url, regex=None):
    """True if a tracker URL belongs to FileList. Never log/return the URL itself."""
    return bool(url) and bool((regex or FILELIST_RE).search(url))


def is_hr_safe(ratio, seeding_time_s):
    """A FileList torrent is safe once ratio >= 1.0 OR seeded >= 48h."""
    return (ratio or 0) >= HR_RATIO or (seeding_time_s or 0) >= HR_SEED_HOURS * 3600


def rule_progress(ratio, seeding_time_s):
    """Progress towards the H&R rule, 0..1 (whichever condition is closer)."""
    seed_hours = max(0.0, (seeding_time_s or 0) / 3600.0)
    p = max(max(0.0, ratio or 0) / HR_RATIO, seed_hours / HR_SEED_HOURS)
    return min(1.0, p)


def is_torrent_complete(progress):
    return (progress or 0) >= 1.0


def hr_status(progress, ratio, seeding_time_s):
    """'downloading' | 'safe' | 'pending' (completed but not safe yet)."""
    if not is_torrent_complete(progress):
        return "downloading"
    if is_hr_safe(ratio, seeding_time_s):
        return "safe"
    return "pending"


def hr_remaining_text(ratio, seeding_time_s):
    """Romanian description of what is left, e.g. 'mai are nevoie de 35 h sau ratio 1'."""
    if is_hr_safe(ratio, seeding_time_s):
        return "regula îndeplinită"
    hours_left = HR_SEED_HOURS - (seeding_time_s or 0) / 3600.0
    hours_left = max(1, int(math.ceil(hours_left)))
    return "mai are nevoie de %d h sau ratio 1" % hours_left


QBIT_STATE_RO = {
    "downloading": "Se descarcă",
    "forcedDL": "Se descarcă",
    "metaDL": "Se descarcă",
    "forcedMetaDL": "Se descarcă",
    "stalledDL": "Blocat (descărcare)",
    "queuedDL": "În coadă",
    "pausedDL": "Pauză",
    "stoppedDL": "Oprit",
    "checkingDL": "Se verifică",
    "uploading": "Seed activ",
    "forcedUP": "Seed activ",
    "stalledUP": "Seed (fără cereri)",
    "queuedUP": "Seed în coadă",
    "pausedUP": "Seed oprit",
    "stoppedUP": "Seed oprit",
    "checkingUP": "Se verifică",
    "checkingResumeData": "Se verifică",
    "moving": "Se mută",
    "missingFiles": "Fișiere lipsă",
    "error": "Eroare",
}


def qbit_state_ro(state):
    return QBIT_STATE_RO.get(state or "", state or "necunoscut")


def summarize_torrent(t, title=None):
    """Turn a qBittorrent /torrents/info entry into the API shape (no tracker URL, no hash).

    title: the movie title for the UI (see torrent_title); defaults to one parsed from the name."""
    progress = float(t.get("progress") or 0)
    ratio = float(t.get("ratio") or 0)
    seeding = int(t.get("seeding_time") or 0)
    status = hr_status(progress, ratio, seeding)
    return {
        "title": title or release_title(t.get("name")),
        "name": t.get("name") or "",
        "state": t.get("state") or "",
        "state_ro": qbit_state_ro(t.get("state")),
        "progress": round(progress * 100, 1),
        "ratio": round(ratio, 2),
        "seed_hours": round(seeding / 3600.0, 1),
        "hr": status,
        "safe": status == "safe",
        "rule_progress": round(rule_progress(ratio, seeding), 3),
        "remaining": hr_remaining_text(ratio, seeding) if status == "pending" else "",
        "dlspeed": int(t.get("dlspeed") or 0),
        "upspeed": int(t.get("upspeed") or 0),
        "eta": int(t.get("eta") or 0),
        "size": int(t.get("size") or 0),
    }


def hr_warnings(torrent_summaries):
    return [
        {"title": t.get("title") or release_title(t["name"]), "name": t["name"], "remaining": t["remaining"]}
        for t in torrent_summaries
        if t["hr"] == "pending"
    ]


# ---------------------------------------------------------------------------
# Movie titles for torrents (the UI never shows release file names)
# ---------------------------------------------------------------------------

_VIDEO_EXT_RE = re.compile(r"\.(mkv|mp4|avi|m4v|mov|wmv|ts|m2ts|iso)$", re.IGNORECASE)
_YEAR_RE = re.compile(r"^[(\[]?((?:19|20)\d\d)[)\]]?$")
# Tokens that start the technical part of a release name (compared lower case, without dots).
_RELEASE_TAGS = {
    "2160p", "1080p", "1080i", "720p", "576p", "480p", "4k", "uhd", "fhd",
    "web", "webdl", "web-dl", "webrip", "web-rip", "hdtv", "bluray", "blu-ray", "bdrip", "brrip",
    "bdremux", "remux", "dvdrip", "hdrip", "hdr", "hdr10", "hdr10+", "hdr10plus", "dovi",
    "x264", "x265", "h264", "h265", "hevc", "avc", "av1", "10bit", "8bit",
    "repack", "proper", "rerip", "extended", "remastered", "unrated", "uncut", "imax",
    "multi", "subbed", "dubbed", "aac", "ac3", "dts", "dts-hd", "truehd",
    "atmos", "ddp", "eac3", "flac", "amzn", "dsnp", "hmax", "atvp",
}
_TAG_PREFIX_RE = re.compile(r"^(ddp\d|dd\d|aac\d|ac3\d|dts-|x26[45]|h26[45]|\d{3,4}[pi]$)", re.IGNORECASE)


def _is_release_tag(tok):
    t = tok.lower().strip("[]()")
    return t in _RELEASE_TAGS or bool(_TAG_PREFIX_RE.match(t))


def release_title(name):
    """A clean 'Title (Year)' from a release or file name, e.g.
    'The.Godfather.1972.2160p.WEB.H265-PETRiFiED.mkv' -> 'The Godfather (1972)'.
    Without a year it returns the title part only; never the raw name for a parseable one."""
    s = (name or "").strip()
    if not s:
        return ""
    s = _VIDEO_EXT_RE.sub("", s)
    # Leading site/group tags: "[YTS.MX] ", "[ www.site.org ] - "
    s = re.sub(r"^(\s*\[[^\]]*\]\s*[-_.]?\s*)+", "", s)
    if " " not in s.strip() or s.count(".") >= 3 or "_" in s:
        s = re.sub(r"[._]+", " ", s)
    s = re.sub(r"[\[\]]", " ", s)
    tokens = s.split()
    if not tokens:
        return (name or "").strip()

    # The release year: a year-like token after at least one title word, preferably the one
    # followed by a release tag or nothing ('Blade Runner 2049 2017 2160p' -> 2017,
    # '2001 A Space Odyssey 1968' -> 1968).
    years = [i for i, tok in enumerate(tokens) if i > 0 and _YEAR_RE.match(tok)]
    strong = [i for i in years if i + 1 == len(tokens) or _is_release_tag(tokens[i + 1])]
    year_idx = (strong or years or [None])[-1]
    end = year_idx
    if end is None:
        end = next((i for i, tok in enumerate(tokens) if i > 0 and _is_release_tag(tok)), len(tokens))
    title = " ".join(tokens[:end]).strip(" -–_.(")
    if not title:
        return (name or "").strip()
    if year_idx is not None:
        return "%s (%s)" % (title, _YEAR_RE.match(tokens[year_idx]).group(1))
    return title


def torrent_title_map(history_records, queue_records, movies):
    """Map torrent hash (upper case) -> movie title using Radarr.

    Radarr's 'grabbed' history and its queue carry the download client id, which for
    qBittorrent is the torrent hash in upper case, plus the movieId. History wins over the
    queue; ids without a known movie are skipped (the caller falls back to release_title)."""
    by_id = {m.get("id"): m for m in movies or [] if m.get("id") is not None}
    out = {}
    for records in (history_records or [], queue_records or []):
        for rec in records:
            dl = str(rec.get("downloadId") or "").strip().upper()
            movie = by_id.get(rec.get("movieId")) or rec.get("movie") or {}
            title = movie.get("title")
            if dl and title and dl not in out:
                out[dl] = title
    return out


def torrent_title(t, title_map):
    """The movie title for a raw qBittorrent torrent: Radarr first, then the parsed name."""
    h = str(t.get("hash") or "").upper()
    return (title_map or {}).get(h) or release_title(t.get("name"))


# ---------------------------------------------------------------------------
# Poster image routes
# ---------------------------------------------------------------------------

# movie: Radarr poster, plex: Plex poster, plex-art / movie-art: backdrops (Plex art, Radarr fanart).
IMG_KINDS = ("movie", "plex", "plex-art", "movie-art")


def parse_img_path(path):
    """'/img/movie/12' -> ('movie', '12'); None for anything else (ids are 1-10 ASCII digits)."""
    m = re.fullmatch(r"/img/(movie|plex|plex-art|movie-art)/([0-9]{1,10})", path or "")
    return (m.group(1), m.group(2)) if m else None


def poster_url(kind, ident):
    ident = str(ident if ident is not None else "")
    if kind in IMG_KINDS and ident.isascii() and ident.isdigit() and len(ident) <= 10:
        return "/img/%s/%s" % (kind, ident)
    return None


# ---------------------------------------------------------------------------
# Network / IP checks
# ---------------------------------------------------------------------------

def is_private_client(ip):
    """True for private (LAN) or loopback addresses; False for anything else or garbage."""
    try:
        addr = ipaddress.ip_address((ip or "").split("%", 1)[0])
    except ValueError:
        return False
    mapped = getattr(addr, "ipv4_mapped", None)
    if mapped is not None:
        addr = mapped
    return bool(addr.is_private or addr.is_loopback)


LOCAL_ADDRESSES = {"127.0.0.1", "::1", "localhost", "::ffff:127.0.0.1"}


def is_local_player(address, own_ips):
    """True if a Plex player address is this PC (loopback or one of our own IPv4s)."""
    if not address:
        return False
    a = address.strip().lower()
    if a in LOCAL_ADDRESSES or a in (own_ips or set()):
        return True
    try:
        return ipaddress.ip_address(a).is_loopback
    except ValueError:
        return False


# ---------------------------------------------------------------------------
# PIN hashing and session tokens
# ---------------------------------------------------------------------------

PBKDF2_ITERATIONS = 200_000


def hash_pin(pin, salt=None, iterations=PBKDF2_ITERATIONS):
    salt = salt if salt is not None else secrets.token_bytes(16)
    dk = hashlib.pbkdf2_hmac("sha256", pin.encode("utf-8"), salt, iterations)
    return "pbkdf2_sha256$%d$%s$%s" % (iterations, salt.hex(), dk.hex())


def verify_pin(pin, stored):
    """Constant-time PIN verification against a stored 'pbkdf2_sha256$iter$salt$hash' string."""
    try:
        algo, iters, salt_hex, hash_hex = (stored or "").split("$")
        if algo != "pbkdf2_sha256":
            return False
        iters = int(iters)
        salt = bytes.fromhex(salt_hex)
        expected = bytes.fromhex(hash_hex)
    except (ValueError, AttributeError):
        return False
    dk = hashlib.pbkdf2_hmac("sha256", (pin or "").encode("utf-8"), salt, iters)
    return hmac.compare_digest(dk, expected)


def valid_pin_format(pin):
    return isinstance(pin, str) and pin.isdigit() and pin.isascii() and 4 <= len(pin) <= 6


SESSION_DAYS = 180


def _sig(key, payload):
    return hmac.new(key, payload.encode("ascii"), hashlib.sha256).hexdigest()


def sign_session(key, now=None, days=SESSION_DAYS):
    """Session token 'expiry.signature' (expiry = epoch seconds)."""
    now = time.time() if now is None else now
    expiry = int(now + days * 86400)
    return "%d.%s" % (expiry, _sig(key, str(expiry)))


def verify_session(key, token, now=None):
    now = time.time() if now is None else now
    if not token or not isinstance(token, str) or token.count(".") != 1:
        return False
    exp_s, sig = token.split(".")
    if not exp_s.isdigit() or len(exp_s) > 12:
        return False
    if not hmac.compare_digest(sig.encode("ascii", "replace"), _sig(key, exp_s).encode("ascii")):
        return False
    return int(exp_s) > now


def session_key(secret, pin_hash):
    """Signing key bound to the PIN hash: changing the PIN invalidates all sessions."""
    return hmac.new(secret.encode("utf-8"), (pin_hash or "").encode("utf-8"), hashlib.sha256).digest()


# ---------------------------------------------------------------------------
# Brute-force guard
# ---------------------------------------------------------------------------

class LoginGuard:
    """5 wrong attempts per IP -> 10 min lockout for that IP.
    20 failures per hour globally -> everyone locked for 10 min."""

    def __init__(self, per_ip_max=5, global_max=20, lock_seconds=600, window_seconds=3600):
        self.per_ip_max = per_ip_max
        self.global_max = global_max
        self.lock_seconds = lock_seconds
        self.window_seconds = window_seconds
        self._ip_fail = {}
        self._ip_locked_until = {}
        self._global_fails = []
        self._global_locked_until = 0.0
        self._lock = threading.Lock()

    def remaining(self, ip, now=None):
        """Seconds of lockout left for this IP (0 = may try)."""
        now = time.time() if now is None else now
        with self._lock:
            return self._remaining_locked(ip, now)

    def _remaining_locked(self, ip, now):
        left = max(self._global_locked_until, self._ip_locked_until.get(ip, 0.0)) - now
        return int(math.ceil(left)) if left > 0 else 0

    def attempts_left(self, ip):
        with self._lock:
            return self.per_ip_max - self._ip_fail.get(ip, 0)

    def record_failure(self, ip, now=None):
        """Register a failed attempt; returns lockout seconds now in force (0 if none)."""
        now = time.time() if now is None else now
        with self._lock:
            count = self._ip_fail.get(ip, 0) + 1
            if count >= self.per_ip_max:
                self._ip_locked_until[ip] = now + self.lock_seconds
                count = 0
            self._ip_fail[ip] = count
            cutoff = now - self.window_seconds
            self._global_fails = [t for t in self._global_fails if t > cutoff]
            self._global_fails.append(now)
            if len(self._global_fails) >= self.global_max:
                self._global_locked_until = now + self.lock_seconds
                self._global_fails = []
            return self._remaining_locked(ip, now)

    def record_success(self, ip):
        with self._lock:
            self._ip_fail.pop(ip, None)
            self._ip_locked_until.pop(ip, None)


# ---------------------------------------------------------------------------
# Radarr helpers (Romanian text)
# ---------------------------------------------------------------------------

RO_MONTHS = ["ian", "feb", "mar", "apr", "mai", "iun", "iul", "aug", "sep", "oct", "nov", "dec"]


def parse_iso(s):
    if not s:
        return None
    try:
        dt = datetime.fromisoformat(s.replace("Z", "+00:00"))
    except ValueError:
        return None
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt


def format_ro_date(dt):
    return "%d %s %d" % (dt.day, RO_MONTHS[dt.month - 1], dt.year)


def waiting_status(movie, now=None, downloading=False):
    """Human Romanian status for a monitored movie without a file."""
    now = now or datetime.now(timezone.utc)
    if downloading:
        return "Se descarcă acum"
    if not movie.get("isAvailable"):
        digital = parse_iso(movie.get("digitalRelease"))
        physical = parse_iso(movie.get("physicalRelease"))
        if digital and digital > now:
            return "Așteaptă lansarea digitală (%s)" % format_ro_date(digital)
        if physical and physical > now:
            return "Așteaptă lansarea pe disc (%s)" % format_ro_date(physical)
        if not digital and not physical and movie.get("status") in ("announced", "tba"):
            return "Anunțat, fără dată de lansare"
    return "Se caută în 4K pe FileList"


def waiting_sort_key(movie):
    dt = parse_iso(movie.get("digitalRelease")) or parse_iso(movie.get("physicalRelease"))
    return (dt.timestamp() if dt else float("inf"), (movie.get("title") or "").lower())


def parse_timeleft(s):
    """Radarr timeleft 'd.hh:mm:ss' or 'hh:mm:ss' -> seconds (None if missing)."""
    if not s:
        return None
    days = 0
    if "." in s.split(":")[0]:
        d, s = s.split(".", 1)
        days = int(d)
    parts = s.split(":")
    try:
        h, m, sec = int(parts[0]), int(parts[1]), int(float(parts[2]))
    except (ValueError, IndexError):
        return None
    return days * 86400 + h * 3600 + m * 60 + sec


def format_duration_ro(seconds):
    if seconds is None:
        return ""
    seconds = int(seconds)
    if seconds < 60:
        return "sub 1 min"
    d, rem = divmod(seconds, 86400)
    h, rem = divmod(rem, 3600)
    m = rem // 60
    if d:
        return "%dz %dh" % (d, h)
    if h:
        return "%dh %dm" % (h, m)
    return "%d min" % m


QUEUE_STATUS_RO = {
    "downloading": "Se descarcă",
    "paused": "Pauză",
    "queued": "În coadă",
    "completed": "Terminat",
    "delay": "Amânat",
    "downloadClientUnavailable": "Client indisponibil",
    "warning": "Avertisment",
    "failed": "Eșuat",
    "importPending": "Așteaptă importul",
    "importing": "Se importă",
    "imported": "Importat",
    "failedPending": "Eșuat",
}


def summarize_queue_record(rec):
    size = float(rec.get("size") or 0)
    left = float(rec.get("sizeleft") or 0)
    pct = round((size - left) / size * 100, 1) if size > 0 else 0.0
    state = rec.get("trackedDownloadState") or rec.get("status") or ""
    if state in ("downloading",) and rec.get("status") in ("paused", "queued", "warning", "failed"):
        state = rec.get("status")
    movie = rec.get("movie") or {}
    secs = parse_timeleft(rec.get("timeleft"))
    return {
        "title": movie.get("title") or rec.get("title") or "",
        "year": movie.get("year"),
        "percent": max(0.0, min(100.0, pct)),
        "time_left": format_duration_ro(secs),
        "status": QUEUE_STATUS_RO.get(state, state),
        "movie_id": rec.get("movieId"),
        "radarr_id": rec.get("movieId"),
        "poster": poster_url("movie", rec.get("movieId")),
    }


def bazarr_ro_map(bazarr_movies):
    """radarrId -> True/False: has Romanian subtitle and it is not listed as missing."""
    out = {}
    for m in bazarr_movies or []:
        rid = m.get("radarrId")
        if rid is None:
            continue
        has = any((s or {}).get("code2") == "ro" for s in m.get("subtitles") or [])
        missing = any((s or {}).get("code2") == "ro" for s in m.get("missing_subtitles") or [])
        out[rid] = has and not missing
    return out


def summarize_library(movies, ro_map):
    """Movies with files. ro_map None (Bazarr down) -> ro_subs None."""
    out = []
    for m in movies or []:
        if not m.get("hasFile"):
            continue
        size = ((m.get("movieFile") or {}).get("size")) or m.get("sizeOnDisk") or 0
        ro = None if ro_map is None else ro_map.get(m.get("id"))
        out.append({
            "title": m.get("title") or "",
            "year": m.get("year"),
            "size_gb": round(size / 1024 ** 3, 1),
            "ro_subs": ro,
            "radarr_id": m.get("id"),
            "poster": poster_url("movie", m.get("id")),
        })
    out.sort(key=lambda x: x["title"].lower())
    return out


def summarize_waiting(movies, queue_movie_ids=(), now=None):
    queue_movie_ids = set(queue_movie_ids or ())
    items = [m for m in movies or [] if m.get("monitored") and not m.get("hasFile")]
    items.sort(key=waiting_sort_key)
    return [
        {
            "title": m.get("title") or "",
            "year": m.get("year"),
            "status": waiting_status(m, now=now, downloading=m.get("id") in queue_movie_ids),
            "downloading": m.get("id") in queue_movie_ids,
            "radarr_id": m.get("id"),
            "poster": poster_url("movie", m.get("id")),
        }
        for m in items
    ]


# ---------------------------------------------------------------------------
# Plex sessions
# ---------------------------------------------------------------------------

STATE_RO = {"playing": "rulează", "paused": "în pauză", "buffering": "se încarcă"}


def playback_decision(meta):
    """'direct_play' | 'direct_stream' | 'transcode'.

    A TranscodeSession is also created for sidecar subtitles only, so look at the
    video/audio decisions: 'transcode' -> transcode, 'copy' -> direct stream."""
    ts = meta.get("TranscodeSession")
    if not ts:
        return "direct_play"
    decisions = (ts.get("videoDecision"), ts.get("audioDecision"))
    if "transcode" in decisions:
        return "transcode"
    if "copy" in decisions:
        return "direct_stream"
    return "direct_play"


DECISION_RO = {"direct_play": "redare directă", "direct_stream": "flux direct", "transcode": "transcodare"}


def parse_plex_sessions(data):
    """Plex /status/sessions JSON -> list of session dicts (internal; includes address)."""
    container = (data or {}).get("MediaContainer") or {}
    out = []
    for meta in container.get("Metadata") or []:
        player = meta.get("Player") or {}
        title = meta.get("title") or ""
        if meta.get("type") == "episode" and meta.get("grandparentTitle"):
            se = ""
            if meta.get("parentIndex") is not None and meta.get("index") is not None:
                se = " S%02dE%02d" % (int(meta["parentIndex"]), int(meta["index"]))
            title = "%s%s – %s" % (meta["grandparentTitle"], se, title)
        duration = meta.get("duration") or 0
        offset = meta.get("viewOffset") or 0
        decision = playback_decision(meta)
        state = player.get("state") or ""
        # Episodes show the show's poster (2:3), movies their own.
        rating_key = str(meta.get("grandparentRatingKey") if meta.get("type") == "episode"
                         and meta.get("grandparentRatingKey") else meta.get("ratingKey") or "")
        has_art = bool(meta.get("grandparentArt") if meta.get("type") == "episode" else meta.get("art"))
        out.append({
            "key": str(meta.get("sessionKey") or (meta.get("Session") or {}).get("id") or ""),
            "title": title,
            "year": meta.get("year"),
            "player": player.get("title") or player.get("device") or player.get("product") or "",
            "product": player.get("product") or "",
            "state": state,
            "state_ro": STATE_RO.get(state, state),
            "progress": round(offset / duration * 100, 1) if duration else 0.0,
            "position": int(offset / 1000),
            "duration": int(duration / 1000),
            "decision": decision,
            "decision_ro": DECISION_RO[decision],
            "transcode": decision == "transcode",
            "address": player.get("address") or "",
            "rating_key": rating_key if poster_url("plex", rating_key) else None,
            "poster": poster_url("plex", rating_key),
            "backdrop": poster_url("plex-art", rating_key) if has_art else None,
        })
    return out


def public_session(s):
    """Strip internal fields before returning to the client."""
    return {k: v for k, v in s.items() if k not in ("address", "key")}


def with_backdrop_fallback(session, movies):
    """When Plex has no art for a playing movie, use Radarr's fanart of the same movie
    (matched by title and year). Returns the session dict (updated in place)."""
    if session.get("backdrop") or not movies:
        return session
    title = (session.get("title") or "").casefold()
    for m in movies:
        if (m.get("title") or "").casefold() == title and m.get("year") == session.get("year"):
            session["backdrop"] = poster_url("movie-art", m.get("id"))
            break
    return session


def auto_monitor_decision(handled, sessions, own_ips, enabled):
    """Decide whether to turn the monitors off.

    handled: set of sessionKeys already decided on (triggered or ignored).
    A session is decided the first time it is seen in state 'playing'. It triggers
    only if enabled and the player is not this PC. Ended sessions are forgotten.
    Returns (new_handled, trigger)."""
    handled = set(handled or ())
    current = set()
    trigger = False
    for s in sessions or []:
        key = s.get("key")
        if not key:
            continue
        current.add(key)
        if key in handled:
            continue
        if s.get("state") == "playing":
            handled.add(key)
            if enabled and not is_local_player(s.get("address"), own_ips):
                trigger = True
    return handled & current, trigger


def tv_sessions(sessions, own_ips):
    """Plex sessions on another device (the TV), not on this PC."""
    return [s for s in sessions or [] if s.get("key") and not is_local_player(s.get("address"), own_ips)]


# ---------------------------------------------------------------------------
# Shut down after the movie
# ---------------------------------------------------------------------------

AFTER_MOVIE_DONE_PCT = 90.0   # a session that ends at or past this point was watched to the end
AFTER_MOVIE_IDLE_S = 3 * 3600  # disarm after this long with nothing playing on the TV


def _pick_session(sessions, title=None):
    """The session to follow: the same title if it is back, else one that plays, else any."""
    if title:
        same = [s for s in sessions if s.get("title") == title]
        if same:
            sessions = same
    playing = [s for s in sessions if s.get("state") == "playing"]
    return (playing or sessions or [None])[0]


def after_movie_arm(sessions, own_ips, now):
    """Arm 'shut down after the movie' on what plays on the TV. None if nothing plays there."""
    s = _pick_session(tv_sessions(sessions, own_ips))
    if s is None:
        return None
    return {"key": s["key"], "title": s.get("title") or "", "since": now,
            "progress": float(s.get("progress") or 0), "seen": now}


def after_movie_step(state, sessions, own_ips, now,
                     done_pct=AFTER_MOVIE_DONE_PCT, idle_s=AFTER_MOVIE_IDLE_S):
    """One poll of the Plex sessions while armed. Returns (new_state, action).

    action is None, 'fire' (the followed session ended at >= done_pct: shut down now; the
    state is cleared, so it fires once) or 'expire' (nothing played on the TV for idle_s).
    A session that ends earlier (the user stopped it) does not fire: the state stays armed,
    follows the next session on the TV (the same title first) and expires after idle_s.
    sessions must be a successful poll; on a failed poll do not call this."""
    if not state:
        return None, None
    state = dict(state)
    tv = tv_sessions(sessions, own_ips)
    if state.get("key"):
        cur = next((s for s in tv if s["key"] == state["key"]), None)
        if cur is not None:
            state["progress"] = float(cur.get("progress") or 0)
            state["title"] = cur.get("title") or state.get("title") or ""
            state["seen"] = now
            return state, None
        if state.get("progress", 0) >= done_pct:
            return None, "fire"
        state.update(key=None, seen=now)  # stopped early: stay armed, wait for the next session
    nxt = _pick_session(tv, state.get("title"))
    if nxt is not None:
        state.update(key=nxt["key"], title=nxt.get("title") or state.get("title") or "",
                     progress=float(nxt.get("progress") or 0), seen=now)
        return state, None
    if now - state.get("seen", state.get("since", now)) >= idle_s:
        return None, "expire"
    return state, None


def public_after_movie(state):
    """The armed state as /api/status shows it, or None."""
    if not state:
        return None
    return {"armed": True, "title": state.get("title") or "", "since": int(state.get("since") or 0)}


# ---------------------------------------------------------------------------
# Notifications: movie ready, disk low
# ---------------------------------------------------------------------------

def movie_label(movie):
    title = movie.get("title") or ""
    return "%s (%s)" % (title, movie["year"]) if movie.get("year") else title


def movie_ready_text(movie, ro):
    """ro: True (Romanian subtitle found), False (still missing), None (Bazarr unknown)."""
    label = movie_label(movie)
    if ro is True:
        return "%s e gata de vizionat, cu subtitrare RO." % label
    if ro is False:
        return "%s e gata de vizionat. Subtitrarea RO încă lipsește." % label
    return "%s e gata de vizionat." % label


MOVIE_READY_FLOOD = 10  # more new movies than this in one poll are recorded, not notified


def movie_ready_step(state, movies, ro_map, now, wait_s):
    """Decide which 'movie ready' notifications to send. Pure; the caller persists the state.

    state: {"primed": bool, "done": [radarr ids], "pending": {"id": first seen with a file}}.
    movies: Radarr /movie list, or None when Radarr is down (nothing changes then).
    ro_map: bazarr_ro_map(...) or None when Bazarr is down.
    The first call only records the movies that already have a file (they never notify).
    A movie that newly has a file notifies once: right away when Bazarr shows a Romanian
    subtitle, else once wait_s has passed since it was first seen with a file. Notified ids
    are kept for good, so a file that disappears and comes back (an upgrade) never repeats.
    Returns (new_state, [message, ...])."""
    state = state or {}
    done = {str(x) for x in state.get("done") or []}
    pending = {str(k): float(v) for k, v in (state.get("pending") or {}).items()}
    if movies is None:
        return {"primed": bool(state.get("primed")), "done": sorted(done, key=_id_order), "pending": pending}, []
    by_id = {str(m.get("id")): m for m in movies if m.get("id") is not None}
    with_file = {k for k, m in by_id.items() if m.get("hasFile")}
    if not state.get("primed"):
        return {"primed": True, "done": sorted(with_file, key=_id_order), "pending": {}}, []

    new = with_file - done - set(pending)
    if len(new) > MOVIE_READY_FLOOD:
        done |= new  # a mass import (or Radarr came back empty-handed earlier): no flood
    messages = []
    for mid in sorted(with_file - done, key=_id_order):
        first = pending.setdefault(mid, now)
        ro = None if ro_map is None else bool(ro_map.get(by_id[mid].get("id")))
        if ro:
            messages.append(movie_ready_text(by_id[mid], True))
        elif now - first >= wait_s:
            messages.append(movie_ready_text(by_id[mid], ro))
        else:
            continue
        done.add(mid)
        pending.pop(mid, None)
    pending = {k: v for k, v in pending.items() if k in with_file and k not in done}
    return {"primed": True, "done": sorted(done, key=_id_order), "pending": pending}, messages


def _id_order(x):
    return (0, int(x)) if str(x).isdigit() else (1, str(x))


def drive_of(path):
    r"""'D:\Movies' -> 'D:'; '\\server\share\Movies' -> '\\server\share'; None without a drive."""
    d = ntpath.splitdrive(str(path or ""))[0]
    return d.upper() if len(d) == 2 else (d or None)


DISK_REARM_GB = 10


def disk_low_step(low, free_gb, min_gb, rearm_gb=DISK_REARM_GB):
    """Hysteresis for the low-disk warning. low: already warned for this drive.
    Returns (new_low, notify): warn once when free < min_gb, re-arm above min_gb + rearm_gb."""
    if free_gb is None:
        return bool(low), False
    if not low and free_gb < min_gb:
        return True, True
    if low and free_gb > min_gb + rearm_gb:
        return False, False
    return bool(low), False


def disk_low_text(drive, free_gb):
    return "Mai ai %d GB liberi pe %s. Un film 4K are 20–30 GB." % (int(free_gb), drive)


# ---------------------------------------------------------------------------
# Power
# ---------------------------------------------------------------------------

def pending_power(state, now=None, grace=10):
    """Return the pending action dict, or None if none/expired."""
    now = time.time() if now is None else now
    if not state:
        return None
    if state.get("at", 0) + grace < now:
        return None
    return {"kind": state["kind"], "at": int(state["at"])}


# ---------------------------------------------------------------------------
# HTTPS helpers
# ---------------------------------------------------------------------------

_HOSTNAME_RE = re.compile(r"^[A-Za-z0-9](?:[A-Za-z0-9-]{0,62})(?:\.[A-Za-z0-9-]{1,63})*\.?$")


def split_host_header(host_header):
    """Return the host part of a Host header ('192.168.1.50:8765' -> '192.168.1.50',
    '[fe80::1]:8765' -> '[fe80::1]'), or None if it is missing or not a sane host name/IP."""
    h = (host_header or "").strip()
    if not h or len(h) > 255:
        return None
    if h.startswith("["):
        end = h.find("]")
        if end < 0:
            return None
        inner, rest = h[1:end], h[end + 1:]
        if rest and not re.fullmatch(r":\d{1,5}", rest):
            return None
        try:
            ipaddress.IPv6Address(inner.split("%", 1)[0])
        except ValueError:
            return None
        return "[%s]" % inner
    host, sep, port = h.rpartition(":") if h.count(":") == 1 else (h, "", "")
    if sep and not re.fullmatch(r"\d{1,5}", port):
        return None
    if not host:
        return None
    if _HOSTNAME_RE.match(host):
        return host.lower()
    return None


def https_url(host_header, https_port, path="/", fallback_host="127.0.0.1"):
    """Build the HTTPS URL for a plain-HTTP request: same host as the browser used, HTTPS port."""
    host = split_host_header(host_header)
    if host is None:
        fb = fallback_host or "127.0.0.1"
        host = "[%s]" % fb if ":" in fb and not fb.startswith("[") else fb
    if not path or not path.startswith("/") or any(ord(c) < 33 or ord(c) == 127 for c in path):
        path = "/"
    if path.startswith("//"):
        path = "/" + path.lstrip("/")
    port = int(https_port)
    return "https://%s%s%s" % (host, "" if port == 443 else ":%d" % port, path)


def session_cookie(name, value, max_age, secure):
    """Set-Cookie value for the session cookie; Secure only when served over HTTPS."""
    return "%s=%s; Max-Age=%d; HttpOnly; SameSite=Strict; Path=/%s" % (
        name, value, int(max_age), "; Secure" if secure else "")


def cert_der(data):
    """Return DER bytes for a certificate given as PEM text or DER bytes."""
    import ssl
    if data.lstrip().startswith(b"-----BEGIN"):
        return ssl.PEM_cert_to_DER_cert(data.decode("ascii"))
    return data
