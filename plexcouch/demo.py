"""Fake status data for --demo (UI screenshots only). No network, no secrets.

Posters are drawn SVG placeholders served from the same /img/... URLs the real app proxies."""

import time
from xml.sax.saxutils import escape

from . import logic

_START = time.time()

# radarr id -> (title, year, top colour, bottom colour, ink colour, serif)
MOVIES = {
    1: ("The Godfather", 1972, "#0B0A09", "#3A2A1A", "#E8DCC4", True),
    2: ("Dune: Part Two", 2024, "#C9772E", "#3B1D10", "#FFF1DE", False),
    3: ("Anatomy of a Fall", 2023, "#D9D4CA", "#7A7468", "#1C1B19", True),
    4: ("Past Lives", 2023, "#2E4A66", "#B9C6D2", "#0F1B27", True),
    5: ("The Odyssey", 2026, "#0D2233", "#5E7F86", "#EAF1F2", True),
    6: ("Perfect Days", 2023, "#3E5A3A", "#C9D3B0", "#14200F", True),
}
# Plex rating key -> radarr id (the movie playing on the TV)
PLEX_KEYS = {"101": 2}

_HASHES = {
    "dune": "a1" * 20,
    "pastlives": "b2" * 20,
    "anatomy": "c3" * 20,
}


def _lines(title, width=10):
    lines, cur = [], ""
    for w in title.upper().split():
        if cur and len(cur) + 1 + len(w) > width:
            lines.append(cur)
            cur = w
        else:
            cur = (cur + " " + w).strip()
    if cur:
        lines.append(cur)
    return lines[:4]


def poster_svg(title, top, bottom, ink, serif):
    """A drawn 2:3 poster with the title, like the design mockups."""
    lines = _lines(title)
    # Fit the longest line into ~88 of the 100 units (uppercase glyphs are about 0.72em wide).
    longest = max([len(line) for line in lines] or [1])
    fs = max(8.0, min(17.0, (88.0 / longest - 1) / 0.72))
    y0 = 110 - (len(lines) - 1) * fs * 0.55
    family = "Georgia, serif" if serif else "Arial Black, Arial, sans-serif"
    text = "".join(
        '<text x="50" y="%.1f" text-anchor="middle" font-family="%s" font-size="%.1f" font-weight="700" '
        'letter-spacing="1" fill="%s">%s</text>' % (y0 + i * fs * 1.1, family, fs, ink, escape(line))
        for i, line in enumerate(lines))
    return ('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 150" width="240" height="360">'
            '<defs><linearGradient id="g" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="%s"/>'
            '<stop offset="1" stop-color="%s"/></linearGradient></defs>'
            '<rect width="100" height="150" fill="url(#g)"/>'
            '<circle cx="50" cy="56" r="22" fill="%s" opacity=".14"/>%s</svg>'
            % (top, bottom, ink, text)).encode("utf-8")


def backdrop_svg(top, bottom, ink):
    """A drawn 16:9 backdrop: a soft gradient with two light shapes (no text)."""
    return ('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 160 90" width="720" height="405">'
            '<defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="%s"/>'
            '<stop offset="1" stop-color="%s"/></linearGradient></defs>'
            '<rect width="160" height="90" fill="url(#g)"/>'
            '<circle cx="118" cy="30" r="26" fill="%s" opacity=".22"/>'
            '<ellipse cx="40" cy="92" rx="70" ry="24" fill="%s" opacity=".18"/></svg>'
            % (top, bottom, ink, bottom)).encode("utf-8")


def poster(kind, ident):
    """(bytes, content_type) for a demo /img/<kind>/<ident>, or None."""
    rid = PLEX_KEYS.get(ident) if kind.startswith("plex") else (int(ident) if ident.isdigit() else None)
    m = MOVIES.get(rid)
    if not m:
        return None
    title, _year, top, bottom, ink, serif = m
    if kind.endswith("-art"):
        return backdrop_svg(top, bottom, ink), "image/svg+xml"
    return poster_svg(title, top, bottom, ink, serif), "image/svg+xml"


def _movie(rid, **extra):
    title, year = MOVIES[rid][:2]
    d = {"title": title, "year": year, "radarr_id": rid, "poster": logic.poster_url("movie", rid)}
    d.update(extra)
    return d


def demo_sessions():
    """The demo movie as an internal Plex session (for arming 'shut down after the movie')."""
    played = 2843 + int(time.time() - _START)
    return [{"key": "demo", "title": "Dune: Part Two", "year": 2024, "state": "playing",
             "progress": round(played / 9960 * 100, 1), "address": "10.0.0.20"}]


def demo_status(app_name, power, settings):
    t = time.time()
    played = 2843 + int(t - _START)
    raw = (
        {"hash": _HASHES["dune"], "name": "Dune.Part.Two.2024.2160p.UHD.BluRay.x265-GROUP",
         "state": "uploading", "progress": 1, "ratio": 0.42, "seeding_time": 12 * 3600},
        {"hash": _HASHES["pastlives"], "name": "Past.Lives.2023.2160p.WEB-DL.DDP5.1.HDR.H.265-GROUP",
         "state": "stalledUP", "progress": 1, "ratio": 1.37, "seeding_time": 30 * 3600},
        {"hash": _HASHES["anatomy"], "name": "Anatomy.of.a.Fall.2023.2160p.WEB.H265-GROUP",
         "state": "downloading", "progress": 0.62, "ratio": 0.0, "seeding_time": 0, "dlspeed": 18_400_000},
    )
    history = [{"downloadId": _HASHES["dune"].upper(), "movieId": 2},
               {"downloadId": _HASHES["pastlives"].upper(), "movieId": 4}]
    queue = [{"downloadId": _HASHES["anatomy"].upper(), "movieId": 3}]
    movies = [{"id": rid, "title": m[0]} for rid, m in MOVIES.items()]
    titles = logic.torrent_title_map(history, queue, movies)
    torrents = [logic.summarize_torrent(x, logic.torrent_title(x, titles)) for x in raw]
    torrents.sort(key=lambda x: ({"pending": 0, "downloading": 1, "safe": 2}[x["hr"]], x["title"].lower()))
    return {
        "app_name": app_name,
        "server_time": t,
        "dry_run": True,
        "demo": True,
        "pc": {"hostname": "DEMO-PC", "uptime": 3 * 86400 + 4 * 3600, "platform": "win32", "disks": [
            {"drive": "C:", "free_gb": 181.4, "total_gb": 930.5, "used_pct": 80.5},
            {"drive": "D:", "free_gb": 807.2, "total_gb": 931.5, "used_pct": 13.3},
        ]},
        "services": {"plex": True, "radarr": True, "prowlarr": True, "bazarr": True, "qbittorrent": True},
        "now_playing": [{
            "title": "Dune: Part Two", "year": 2024, "player": "Samsung TV, sufragerie",
            "product": "Plex for Samsung", "state": "playing", "state_ro": "rulează",
            "progress": round(played / 9960 * 100, 1), "position": played, "duration": 9960,
            "decision": "direct_play", "decision_ro": "redare directă", "transcode": False,
            "rating_key": "101", "poster": logic.poster_url("plex", "101"),
            "backdrop": logic.poster_url("plex-art", "101"), "on_tv": True,
        }],
        "downloads": [_movie(3, percent=62.0, time_left="8 min", status="Se descarcă", movie_id=3)],
        "waiting": [
            _movie(3, status="Se descarcă acum", downloading=True),
            _movie(5, status="Așteaptă lansarea digitală (17 nov 2026)", downloading=False),
            _movie(6, status="Se caută în 4K pe FileList", downloading=False),
        ],
        "library": [
            _movie(2, size_gb=61.8, ro_subs=True),
            _movie(4, size_gb=19.1, ro_subs=False),
            _movie(1, size_gb=19.0, ro_subs=True),
        ],
        "torrents": torrents,
        "hr_warnings": logic.hr_warnings(torrents),
        "power": power,
        "settings": settings,
    }
