"""Fake status data for --demo (UI screenshots only). No network, no secrets."""

import time

from . import logic

_START = time.time()


def demo_status(app_name, power, settings):
    t = time.time()
    played = 2843 + int(t - _START)
    torrents = [logic.summarize_torrent(x) for x in (
        {"name": "Dune.Part.Two.2024.2160p.UHD.BluRay.x265-GROUP", "state": "uploading", "progress": 1,
         "ratio": 0.42, "seeding_time": 12 * 3600},
        {"name": "Past.Lives.2023.2160p.WEB-DL.DDP5.1.HDR.H.265-GROUP", "state": "stalledUP", "progress": 1,
         "ratio": 1.37, "seeding_time": 30 * 3600},
        {"name": "Anatomy.of.a.Fall.2023.2160p.WEB.H265-GROUP", "state": "downloading", "progress": 0.62,
         "ratio": 0.0, "seeding_time": 0, "dlspeed": 18_400_000},
    )]
    return {
        "app_name": app_name,
        "server_time": t,
        "dry_run": True,
        "demo": True,
        "pc": {"hostname": "DEMO-PC", "uptime": 3 * 86400 + 4 * 3600, "platform": "win32", "disks": [
            {"drive": "C:", "free_gb": 181.4, "total_gb": 930.5, "used_pct": 80.5},
            {"drive": "D:", "free_gb": 826.2, "total_gb": 931.5, "used_pct": 11.3},
        ]},
        "services": {"plex": True, "radarr": True, "prowlarr": True, "bazarr": True, "qbittorrent": True},
        "now_playing": [{
            "title": "Dune: Part Two", "year": 2024, "player": "Televizor sufragerie",
            "product": "Plex for Samsung", "state": "playing", "state_ro": "rulează",
            "progress": round(played / 9960 * 100, 1), "position": played, "duration": 9960,
            "decision": "direct_play", "decision_ro": "redare directă", "transcode": False,
        }],
        "downloads": [{"title": "Anatomy of a Fall", "year": 2023, "percent": 62.0,
                       "time_left": "8 min", "status": "Se descarcă", "movie_id": 3}],
        "waiting": [
            {"title": "The Odyssey", "year": 2026, "status": "Așteaptă lansarea digitală (17 nov 2026)"},
            {"title": "Perfect Days", "year": 2023, "status": "Se caută în 4K pe FileList"},
        ],
        "library": [
            {"title": "Dune: Part Two", "year": 2024, "size_gb": 61.8, "ro_subs": True},
            {"title": "Past Lives", "year": 2023, "size_gb": 19.1, "ro_subs": False},
            {"title": "The Godfather", "year": 1972, "size_gb": 19.0, "ro_subs": True},
        ],
        "torrents": torrents,
        "hr_warnings": logic.hr_warnings(torrents),
        "power": power,
        "settings": settings,
    }
