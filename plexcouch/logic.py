"""Pure logic: no network, no Windows calls. Everything here is unit-tested."""

import hashlib
import hmac
import ipaddress
import math
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
    """Romanian description of what is left, e.g. 'mai are nevoie de 35h seed sau ratio 1'."""
    if is_hr_safe(ratio, seeding_time_s):
        return "regula îndeplinită"
    hours_left = HR_SEED_HOURS - (seeding_time_s or 0) / 3600.0
    hours_left = max(1, int(math.ceil(hours_left)))
    return "mai are nevoie de %dh seed sau ratio 1" % hours_left


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


def summarize_torrent(t):
    """Turn a qBittorrent /torrents/info entry into the API shape (no tracker URL)."""
    progress = float(t.get("progress") or 0)
    ratio = float(t.get("ratio") or 0)
    seeding = int(t.get("seeding_time") or 0)
    status = hr_status(progress, ratio, seeding)
    return {
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
        {"name": t["name"], "remaining": t["remaining"]}
        for t in torrent_summaries
        if t["hr"] == "pending"
    ]


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
        })
    return out


def public_session(s):
    """Strip internal fields before returning to the client."""
    return {k: v for k, v in s.items() if k not in ("address", "key")}


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
