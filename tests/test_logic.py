import os
import sys
import time
import unittest
from datetime import datetime, timezone

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from plexcouch import logic  # noqa: E402
from plexcouch.system import power_args  # noqa: E402
from plexcouch.services import parse_bazarr_apikey  # noqa: E402

H = 3600


class TestHitAndRun(unittest.TestCase):
    def test_safe_by_ratio(self):
        self.assertTrue(logic.is_hr_safe(1.0, 0))
        self.assertTrue(logic.is_hr_safe(2.5, 10))

    def test_safe_by_seed_time(self):
        self.assertTrue(logic.is_hr_safe(0.0, 48 * H))
        self.assertFalse(logic.is_hr_safe(0.99, 48 * H - 1))

    def test_status(self):
        self.assertEqual(logic.hr_status(0.5, 0, 0), "downloading")
        self.assertEqual(logic.hr_status(1.0, 0.2, 13 * H), "pending")
        self.assertEqual(logic.hr_status(1.0, 1.0, 0), "safe")

    def test_rule_progress(self):
        self.assertAlmostEqual(logic.rule_progress(0.5, 0), 0.5)
        self.assertAlmostEqual(logic.rule_progress(0.1, 24 * H), 0.5)
        self.assertAlmostEqual(logic.rule_progress(0.75, 12 * H), 0.75)
        self.assertEqual(logic.rule_progress(3.0, 0), 1.0)
        self.assertEqual(logic.rule_progress(0, 100 * H), 1.0)
        self.assertEqual(logic.rule_progress(None, None), 0.0)

    def test_remaining_text(self):
        self.assertEqual(logic.hr_remaining_text(0.1, 13 * H), "mai are nevoie de 35 h sau ratio 1")
        self.assertEqual(logic.hr_remaining_text(0.1, 13 * H + 1), "mai are nevoie de 35 h sau ratio 1")
        self.assertEqual(logic.hr_remaining_text(0, 48 * H - 10), "mai are nevoie de 1 h sau ratio 1")

    def test_summarize_and_warnings(self):
        ts = [
            {"name": "A", "state": "stalledUP", "progress": 1, "ratio": 0.0, "seeding_time": 903},
            {"name": "B", "state": "uploading", "progress": 1, "ratio": 1.2, "seeding_time": 60},
            {"name": "C", "state": "downloading", "progress": 0.4, "ratio": 0.0, "seeding_time": 0},
        ]
        s = [logic.summarize_torrent(t) for t in ts]
        self.assertEqual([x["hr"] for x in s], ["pending", "safe", "downloading"])
        self.assertTrue(s[1]["safe"])
        self.assertEqual(s[0]["remaining"], "mai are nevoie de 48 h sau ratio 1")
        w = logic.hr_warnings(s)
        self.assertEqual(w, [{"title": "A", "name": "A", "remaining": "mai are nevoie de 48 h sau ratio 1"}])
        self.assertNotIn("tracker", s[0])
        self.assertNotIn("hash", s[0])

    def test_title_given_or_parsed(self):
        raw = {"name": "The.Godfather.1972.2160p.WEB.H265-PETRiFiED.mkv", "progress": 1, "ratio": 0.1}
        self.assertEqual(logic.summarize_torrent(raw)["title"], "The Godfather (1972)")
        t = logic.summarize_torrent(raw, "The Godfather")
        self.assertEqual(t["title"], "The Godfather")
        self.assertEqual(logic.hr_warnings([t])[0]["title"], "The Godfather")


class TestReleaseTitle(unittest.TestCase):
    CASES = [
        ("The.Godfather.1972.2160p.WEB.H265-PETRiFiED.mkv", "The Godfather (1972)"),
        ("The.Godfather.1972.2160p.WEB-DL.AAC5.1.H.265-PETRiFiED", "The Godfather (1972)"),
        ("Anatomy.of.a.Fall.2023.REPACK.2160p.WEB-DL.DDP5.1.HDR.H.265-GROUP", "Anatomy of a Fall (2023)"),
        ("Dune Part Two 2024 2160p UHD BluRay x265-GROUP", "Dune Part Two (2024)"),
        ("[YTS.MX] Past Lives (2023) [2160p] [WEB-DL]", "Past Lives (2023)"),
        ("Perfect Days (2023) [1080p] [BluRay] [5.1] [YTS.MX]", "Perfect Days (2023)"),
        ("Blade.Runner.2049.2017.2160p.UHD.BluRay.REMUX.HDR.HEVC.Atmos-GROUP", "Blade Runner 2049 (2017)"),
        ("2001.A.Space.Odyssey.1968.2160p.UHD.BluRay.x265.mkv", "2001 A Space Odyssey (1968)"),
        ("A.Complete.Unknown.2024.1080p.WEBRip.x264.mp4", "A Complete Unknown (2024)"),
        ("Mad_Max_Fury_Road_2015_1080p_BluRay_x264", "Mad Max Fury Road (2015)"),
        ("Some.Movie.2160p.WEB-DL.DDP5.1-GROUP", "Some Movie"),
        ("The Odyssey", "The Odyssey"),
    ]

    def test_shapes(self):
        for name, want in self.CASES:
            self.assertEqual(logic.release_title(name), want, name)

    def test_empty(self):
        self.assertEqual(logic.release_title(""), "")
        self.assertEqual(logic.release_title(None), "")


class TestTorrentTitles(unittest.TestCase):
    MOVIES = [{"id": 2, "title": "The Godfather"}, {"id": 3, "title": "Anatomy of a Fall"}]
    H1 = "85b755cd2d0a8a7a0e15348b9fdcbff2f1580a55"
    H2 = "c3" * 20

    def test_history_then_queue_case_insensitive(self):
        history = [{"downloadId": self.H1.upper(), "movieId": 2, "eventType": "grabbed"}]
        queue = [{"downloadId": self.H2.upper(), "movieId": 3}]
        m = logic.torrent_title_map(history, queue, self.MOVIES)
        self.assertEqual(m, {self.H1.upper(): "The Godfather", self.H2.upper(): "Anatomy of a Fall"})
        t = {"hash": self.H1, "name": "The.Godfather.1972.2160p.WEB.H265-PETRiFiED.mkv"}
        self.assertEqual(logic.torrent_title(t, m), "The Godfather")
        mixed = logic.torrent_title_map([{"downloadId": self.H1.lower(), "movieId": 2}], [], self.MOVIES)
        self.assertEqual(logic.torrent_title(t, mixed), "The Godfather")

    def test_history_wins_and_unknown_movie_skipped(self):
        history = [{"downloadId": self.H1, "movieId": 3}, {"downloadId": "FFFF", "movieId": 99}]
        queue = [{"downloadId": self.H1, "movieId": 2}]
        m = logic.torrent_title_map(history, queue, self.MOVIES)
        self.assertEqual(m, {self.H1.upper(): "Anatomy of a Fall"})

    def test_queue_movie_object_and_fallback(self):
        queue = [{"downloadId": self.H2, "movieId": 7, "movie": {"title": "Past Lives"}}]
        m = logic.torrent_title_map(None, queue, None)
        self.assertEqual(logic.torrent_title({"hash": self.H2}, m), "Past Lives")
        t = {"hash": "ab" * 20, "name": "Past.Lives.2023.2160p.WEB-DL-GROUP"}
        self.assertEqual(logic.torrent_title(t, m), "Past Lives (2023)")
        self.assertEqual(logic.torrent_title(t, None), "Past Lives (2023)")


class TestImgRoutes(unittest.TestCase):
    def test_parse_img_path(self):
        self.assertEqual(logic.parse_img_path("/img/movie/12"), ("movie", "12"))
        self.assertEqual(logic.parse_img_path("/img/plex/1"), ("plex", "1"))
        self.assertEqual(logic.parse_img_path("/img/plex-art/1"), ("plex-art", "1"))
        self.assertEqual(logic.parse_img_path("/img/movie-art/22"), ("movie-art", "22"))
        for path in ("/img/plex-art/x", "/img/movie-art/", "/img/art/1", "/img/plex-art/1/2"):
            self.assertIsNone(logic.parse_img_path(path), path)
        bad = ["/img/movie/abc", "/img/movie/", "/img/movie/1a", "/img/movie/-1", "/img/movie/1/",
               "/img/movie/../x", "/img/movie/%31", "/img/movie/1 ", "/img/other/1",
               "/img/movie/１２", "/img/movie/١", "/img/movie/12345678901",
               "/img/plex/1?x", "/img/movie/1.jpg", "/img/movie/1\n", "", None]
        for path in bad:
            self.assertIsNone(logic.parse_img_path(path), path)

    def test_poster_url(self):
        self.assertEqual(logic.poster_url("movie", 2), "/img/movie/2")
        self.assertEqual(logic.poster_url("plex", "101"), "/img/plex/101")
        for ident in (None, "", "x1", "١٢", "1/2"):
            self.assertIsNone(logic.poster_url("movie", ident), ident)
        self.assertIsNone(logic.poster_url("tv", 1))


class TestTrackerMatch(unittest.TestCase):
    def test_filelist_hosts(self):
        for url in ("http://reactor.thefl.org/abc/announce", "https://filelist.io/announce",
                    "http://tracker.filelist.ro/x", "http://flro.org/a", "HTTP://REACTOR.THEFL.ORG/x"):
            self.assertTrue(logic.is_filelist_tracker(url), url)

    def test_other_hosts(self):
        for url in ("", None, "udp://tracker.opentrackr.org:1337/announce", "http://example.org/thefl",
                    "https://filelist.com/announce"):
            self.assertFalse(logic.is_filelist_tracker(url), url)

    def test_custom_regex(self):
        rx = logic.compile_tracker_regex(r"example\.net")
        self.assertTrue(logic.is_filelist_tracker("http://t.example.net/a", rx))
        self.assertFalse(logic.is_filelist_tracker("http://reactor.thefl.org/a", rx))


class TestPrivateIp(unittest.TestCase):
    def test_allowed(self):
        for ip in ("127.0.0.1", "::1", "10.0.0.5", "172.16.3.4", "::ffff:10.1.2.3", "fd00::1", "169.254.1.1"):
            self.assertTrue(logic.is_private_client(ip), ip)

    def test_rejected(self):
        for ip in ("8.8.8.8", "1.1.1.1", "2001:4860:4860::8888", "::ffff:8.8.8.8", "", "garbage", None):
            self.assertFalse(logic.is_private_client(ip), ip)


class TestSessionToken(unittest.TestCase):
    key = b"k" * 32

    def test_roundtrip(self):
        t = logic.sign_session(self.key, now=1000)
        self.assertTrue(logic.verify_session(self.key, t, now=1001))

    def test_expiry(self):
        self.assertEqual(logic.SESSION_DAYS, 30)
        t = logic.sign_session(self.key, now=1000)
        self.assertTrue(logic.verify_session(self.key, t, now=1000 + 29 * 86400))
        self.assertEqual(logic.session_expiry(self.key, t, now=1001), 1000 + 30 * 86400)
        self.assertFalse(logic.verify_session(self.key, t, now=1000 + 30 * 86400))
        self.assertFalse(logic.verify_session(self.key, t, now=1000 + 180 * 86400))

    def test_tamper(self):
        t = logic.sign_session(self.key, now=1000)
        ep, exp, sig = t.split(".")
        self.assertEqual(ep, "1")
        self.assertFalse(logic.verify_session(self.key, "%s.%d.%s" % (ep, int(exp) + 999999, sig), now=1001))
        self.assertFalse(logic.verify_session(self.key, "2.%s.%s" % (exp, sig), now=1001, epoch=2))
        self.assertFalse(logic.verify_session(self.key, "%s.%s.%s" % (ep, exp, "0" * len(sig)), now=1001))
        self.assertFalse(logic.verify_session(b"other", t, now=1001))
        for bad in ("", None, "abc", "1.2.3", ".x", "x.", "12a.ff", "1.é", "1.2", exp + "." + sig,
                    "1.1.2.3", "a.%s.%s" % (exp, sig), "1.%s.é" % exp):
            self.assertFalse(logic.verify_session(self.key, bad, now=1001), bad)

    def test_key_bound_to_pin(self):
        k1 = logic.session_key("secret", "hash1")
        k2 = logic.session_key("secret", "hash2")
        t = logic.sign_session(k1, now=1000)
        self.assertTrue(logic.verify_session(k1, t, now=1001))
        self.assertFalse(logic.verify_session(k2, t, now=1001))


class TestPin(unittest.TestCase):
    def test_hash_verify(self):
        h = logic.hash_pin("1234", iterations=1000)
        self.assertTrue(h.startswith("pbkdf2_sha256$1000$"))
        self.assertTrue(logic.verify_pin("1234", h))
        self.assertFalse(logic.verify_pin("1235", h))
        self.assertFalse(logic.verify_pin("", h))

    def test_default_iterations_and_salt(self):
        h1 = logic.hash_pin("1234")
        h2 = logic.hash_pin("1234")
        self.assertIn("$200000$", h1)
        self.assertNotEqual(h1, h2)  # random salt
        self.assertEqual(len(h1.split("$")[2]), 32)  # 16-byte salt hex

    def test_garbage_hash(self):
        for bad in ("", None, "x", "md5$1$aa$bb", "pbkdf2_sha256$x$aa$bb", "pbkdf2_sha256$10$zz$bb"):
            self.assertFalse(logic.verify_pin("1234", bad), bad)

    def test_format(self):
        self.assertTrue(logic.valid_pin_format("1234"))
        self.assertTrue(logic.valid_pin_format("741852"))
        for bad in ("123", "12a4", "", "1234567", "١٢٣٤"):
            self.assertFalse(logic.valid_pin_format(bad), bad)


class TestLoginGuard(unittest.TestCase):
    def test_per_ip_lockout(self):
        g = logic.LoginGuard()
        for i in range(4):
            self.assertEqual(g.record_failure("10.0.0.2", now=100 + i), 0)
        self.assertEqual(g.attempts_left("10.0.0.2"), 1)
        self.assertEqual(g.record_failure("10.0.0.2", now=105), 600)
        self.assertEqual(g.remaining("10.0.0.2", now=105), 600)
        self.assertEqual(g.remaining("10.0.0.2", now=405), 300)
        self.assertEqual(g.remaining("10.0.0.3", now=405), 0)  # other IP unaffected
        self.assertEqual(g.remaining("10.0.0.2", now=706), 0)

    def test_success_resets(self):
        g = logic.LoginGuard()
        for i in range(4):
            g.record_failure("10.0.0.2", now=100 + i)
        g.record_success("10.0.0.2")
        self.assertEqual(g.record_failure("10.0.0.2", now=110), 0)
        self.assertEqual(g.attempts_left("10.0.0.2"), 4)

    def test_global_slow_down_for_unknown_ips(self):
        g = logic.LoginGuard()
        for i in range(19):  # 19 different IPs, 1 failure each: no slow-down yet
            self.assertEqual(g.record_failure("10.0.1.%d" % i, now=1000 + i), 0)
        self.assertEqual(g.remaining("10.9.9.9", now=1019), 0)
        g.record_failure("10.0.1.99", now=1019)  # the 20th failure within the hour
        # unknown IPs may now try once per global_delay (60 s) in total, not 10 min each
        self.assertEqual(g.remaining("10.9.9.9", now=1020), 59)
        self.assertEqual(g.remaining("10.9.9.9", now=1019 + 60), 0)
        ticket, wait = g.reserve("10.9.9.9", now=1080)
        self.assertEqual(wait, 0)
        g.fail(ticket, now=1080)
        self.assertEqual(g.remaining("10.9.9.8", now=1081), 59)

    def test_global_window_expires(self):
        g = logic.LoginGuard()
        for i in range(20):
            g.record_failure("10.0.1.%d" % i, now=1000 + i)
        self.assertGreater(g.remaining("10.0.2.1", now=1030), 0)
        # an hour later the old failures no longer count
        self.assertEqual(g.remaining("10.0.2.1", now=1000 + 3700), 0)
        self.assertEqual(g.record_failure("10.0.2.1", now=1000 + 3700), 0)


class TestWaitingStatus(unittest.TestCase):
    now = datetime(2026, 10, 10, tzinfo=timezone.utc)

    def test_waiting_digital(self):
        m = {"isAvailable": False, "digitalRelease": "2026-11-17T00:00:00Z",
             "physicalRelease": "2026-11-17T00:00:00Z", "status": "inCinemas"}
        self.assertEqual(logic.waiting_status(m, now=self.now), "Așteaptă lansarea digitală (17 nov 2026)")

    def test_waiting_physical_only(self):
        m = {"isAvailable": False, "digitalRelease": "2026-01-01T00:00:00Z",
             "physicalRelease": "2027-03-02T00:00:00Z"}
        self.assertEqual(logic.waiting_status(m, now=self.now), "Așteaptă lansarea pe disc (2 mar 2027)")

    def test_searching(self):
        m = {"isAvailable": True, "digitalRelease": "2020-01-01T00:00:00Z"}
        self.assertEqual(logic.waiting_status(m, now=self.now), "Se caută în 4K pe FileList")
        m2 = {"isAvailable": False, "digitalRelease": "2026-01-01T00:00:00Z"}
        self.assertEqual(logic.waiting_status(m2, now=self.now), "Se caută în 4K pe FileList")

    def test_announced_and_downloading(self):
        self.assertEqual(logic.waiting_status({"isAvailable": False, "status": "announced"}, now=self.now),
                         "Anunțat, fără dată de lansare")
        self.assertEqual(logic.waiting_status({"isAvailable": True}, now=self.now, downloading=True),
                         "Se descarcă acum")

    def test_summaries(self):
        movies = [
            {"id": 1, "title": "The Odyssey", "year": 2026, "monitored": True, "hasFile": False,
             "isAvailable": False, "digitalRelease": "2026-11-17T00:00:00Z"},
            {"id": 2, "title": "The Godfather", "year": 1972, "monitored": True, "hasFile": True,
             "movieFile": {"size": 20419862888}},
            {"id": 3, "title": "Unmonitored", "monitored": False, "hasFile": False},
        ]
        w = logic.summarize_waiting(movies, now=self.now)
        self.assertEqual([x["title"] for x in w], ["The Odyssey"])
        self.assertEqual((w[0]["radarr_id"], w[0]["poster"], w[0]["downloading"]), (1, "/img/movie/1", False))
        self.assertTrue(logic.summarize_waiting(movies, {1}, now=self.now)[0]["downloading"])
        lib = logic.summarize_library(movies, logic.bazarr_ro_map([
            {"radarrId": 2, "subtitles": [{"code2": "ro", "path": "x"}], "missing_subtitles": []}]))
        self.assertEqual(lib, [{"title": "The Godfather", "year": 1972, "size_gb": 19.0, "ro_subs": True,
                                "radarr_id": 2, "poster": "/img/movie/2"}])
        self.assertIsNone(logic.summarize_library(movies, None)[0]["ro_subs"])
        missing = logic.bazarr_ro_map([{"radarrId": 2, "subtitles": [{"code2": "ro"}],
                                        "missing_subtitles": [{"code2": "ro"}]}])
        self.assertFalse(missing[2])

    def test_timeleft(self):
        self.assertEqual(logic.parse_timeleft("01:02:03"), 3723)
        self.assertEqual(logic.parse_timeleft("1.00:00:10"), 86410)
        self.assertIsNone(logic.parse_timeleft(None))
        self.assertEqual(logic.format_duration_ro(3723), "1h 2m")
        self.assertEqual(logic.format_duration_ro(86410 + 7200), "1z 2h")

    def test_queue_record_poster(self):
        q = logic.summarize_queue_record({"movieId": 3, "size": 100, "sizeleft": 38, "status": "downloading",
                                          "trackedDownloadState": "downloading", "timeleft": "00:08:00",
                                          "movie": {"title": "Anatomy of a Fall", "year": 2023}})
        self.assertEqual((q["percent"], q["time_left"], q["radarr_id"], q["poster"]),
                         (62.0, "8 min", 3, "/img/movie/3"))


class TestAutoMonitor(unittest.TestCase):
    own = {"10.0.0.131"}

    def s(self, key, state, addr):
        return {"key": key, "state": state, "address": addr}

    def test_new_remote_playing_triggers_once(self):
        handled, trig = logic.auto_monitor_decision(set(), [self.s("1", "playing", "10.0.0.129")], self.own, True)
        self.assertTrue(trig)
        handled, trig = logic.auto_monitor_decision(handled, [self.s("1", "playing", "10.0.0.129")], self.own, True)
        self.assertFalse(trig)  # same session never re-triggers
        handled, trig = logic.auto_monitor_decision(handled, [self.s("1", "paused", "10.0.0.129")], self.own, True)
        handled, trig = logic.auto_monitor_decision(handled, [self.s("1", "playing", "10.0.0.129")], self.own, True)
        self.assertFalse(trig)

    def test_buffering_then_playing(self):
        handled, trig = logic.auto_monitor_decision(set(), [self.s("2", "buffering", "10.0.0.129")], self.own, True)
        self.assertFalse(trig)
        handled, trig = logic.auto_monitor_decision(handled, [self.s("2", "playing", "10.0.0.129")], self.own, True)
        self.assertTrue(trig)

    def test_local_player_skipped(self):
        for addr in ("127.0.0.1", "::1", "10.0.0.131"):
            handled, trig = logic.auto_monitor_decision(set(), [self.s("3", "playing", addr)], self.own, True)
            self.assertFalse(trig, addr)
            self.assertEqual(handled, {"3"})

    def test_disabled(self):
        handled, trig = logic.auto_monitor_decision(set(), [self.s("4", "playing", "10.0.0.129")], self.own, False)
        self.assertFalse(trig)
        # enabling later does not fire for an already-seen session
        handled, trig = logic.auto_monitor_decision(handled, [self.s("4", "playing", "10.0.0.129")], self.own, True)
        self.assertFalse(trig)

    def test_ended_sessions_forgotten(self):
        handled, _ = logic.auto_monitor_decision(set(), [self.s("5", "playing", "10.0.0.129")], self.own, True)
        handled, trig = logic.auto_monitor_decision(handled, [], self.own, True)
        self.assertEqual(handled, set())
        handled, trig = logic.auto_monitor_decision(handled, [self.s("6", "playing", "10.0.0.129")], self.own, True)
        self.assertTrue(trig)


class TestPlexParse(unittest.TestCase):
    def test_parse(self):
        data = {"MediaContainer": {"Metadata": [{
            "title": "The Godfather", "year": 1972, "type": "movie", "sessionKey": "2",
            "duration": 10000, "viewOffset": 2500,
            "Player": {"title": "TV 2024", "state": "paused", "address": "10.0.0.129"},
            "TranscodeSession": {"subtitleDecision": "copy"},
        }]}}
        s = logic.parse_plex_sessions(data)[0]
        self.assertEqual(s["progress"], 25.0)
        self.assertEqual(s["decision"], "direct_play")  # subtitle-only transcode session
        self.assertEqual(s["state_ro"], "în pauză")
        self.assertEqual(s["decision_ro"], "redare directă")
        pub = logic.public_session(s)
        self.assertNotIn("address", pub)
        self.assertEqual((pub["rating_key"], pub["poster"], pub["backdrop"]), (None, None, None))
        data["MediaContainer"]["Metadata"][0]["ratingKey"] = "1"
        s = logic.parse_plex_sessions(data)[0]
        self.assertEqual((s["rating_key"], s["poster"], s["backdrop"]), ("1", "/img/plex/1", None))
        data["MediaContainer"]["Metadata"][0]["art"] = "/library/metadata/1/art/123"
        self.assertEqual(logic.parse_plex_sessions(data)[0]["backdrop"], "/img/plex-art/1")
        data["MediaContainer"]["Metadata"][0]["TranscodeSession"] = {"videoDecision": "transcode"}
        self.assertTrue(logic.parse_plex_sessions(data)[0]["transcode"])

    def test_backdrop_fallback_to_radarr(self):
        movies = [{"id": 2, "title": "The Godfather", "year": 1972}, {"id": 3, "title": "Other", "year": 1972}]
        s = {"title": "The Godfather", "year": 1972, "backdrop": None}
        self.assertEqual(logic.with_backdrop_fallback(dict(s), movies)["backdrop"], "/img/movie-art/2")
        self.assertIsNone(logic.with_backdrop_fallback(dict(s, year=1990), movies)["backdrop"])
        self.assertIsNone(logic.with_backdrop_fallback(dict(s), None)["backdrop"])
        kept = dict(s, backdrop="/img/plex-art/1")
        self.assertEqual(logic.with_backdrop_fallback(kept, movies)["backdrop"], "/img/plex-art/1")

    def test_episode_uses_show_poster(self):
        data = {"MediaContainer": {"Metadata": [{
            "title": "Pilot", "type": "episode", "grandparentTitle": "Show", "parentIndex": 1, "index": 2,
            "ratingKey": "50", "grandparentRatingKey": "40", "Player": {"state": "playing"}}]}}
        s = logic.parse_plex_sessions(data)[0]
        self.assertEqual(s["title"], "Show S01E02 – Pilot")
        self.assertEqual(s["poster"], "/img/plex/40")
        self.assertIsNone(s["backdrop"])
        data["MediaContainer"]["Metadata"][0]["grandparentArt"] = "/library/metadata/40/art/9"
        self.assertEqual(logic.parse_plex_sessions(data)[0]["backdrop"], "/img/plex-art/40")

    def test_empty(self):
        self.assertEqual(logic.parse_plex_sessions({"MediaContainer": {"size": 0}}), [])


class TestBazarrKey(unittest.TestCase):
    def test_parse(self):
        lines = ["general:\n", "  apikey: wrong\n", "auth:\n", "  password: x\n", "  apikey: 'abc123'\n",
                 "  type: form\n", "other:\n", "  apikey: nope\n"]
        self.assertEqual(parse_bazarr_apikey(lines), "abc123")
        self.assertEqual(parse_bazarr_apikey(["auth:\n", "  type: x\n"]), "")


class TestPower(unittest.TestCase):
    def test_pending(self):
        self.assertIsNone(logic.pending_power(None))
        self.assertEqual(logic.pending_power({"kind": "shutdown", "at": 1060.4}, now=1000),
                         {"kind": "shutdown", "at": 1060})
        self.assertIsNone(logic.pending_power({"kind": "shutdown", "at": 1060}, now=1100))


class TestPowerArgs(unittest.TestCase):
    def test_shutdown_is_hybrid_and_immediate(self):
        self.assertEqual(power_args("shutdown", "shutdown.exe"),
                         ["shutdown.exe", "/s", "/hybrid", "/t", "0"])

    def test_restart_has_no_hybrid(self):
        self.assertEqual(power_args("restart", "shutdown.exe"), ["shutdown.exe", "/r", "/t", "0"])

    def test_never_forced(self):
        # /t above 0 implies /f; we keep /t 0 so unsaved work can stop it, like Start > Shut down
        for kind in ("shutdown", "restart"):
            self.assertNotIn("/f", power_args(kind, "shutdown.exe"))

    def test_unknown_kind_raises(self):
        with self.assertRaises(KeyError):
            power_args("hibernate", "shutdown.exe")


class TestPowerTimer(unittest.TestCase):
    def setUp(self):
        from plexcouch import system
        self.system = system
        self.old_delay = system.POWER_DELAY
        system.POWER_DELAY = 0.2
        self.fired = []
        self.old_run = system.subprocess.run
        system.subprocess.run = lambda *a, **k: self.fired.append(a[0])  # never runs shutdown.exe

    def tearDown(self):
        self.system.POWER_DELAY = self.old_delay
        self.system.subprocess.run = self.old_run

    def test_cancel_stops_it(self):
        acts = self.system.Actions(dry_run=True, app_name="App")
        self.assertIsNotNone(acts.power("shutdown"))
        self.assertIsNone(acts.power("restart"))  # one at a time
        acts.cancel_power()
        time.sleep(0.4)
        self.assertIsNone(acts.pending())
        self.assertEqual(self.fired, [])

    def test_dry_run_never_executes(self):
        acts = self.system.Actions(dry_run=True, app_name="App")
        acts.power("shutdown")
        time.sleep(0.4)
        self.assertEqual(self.fired, [])


if __name__ == "__main__":
    unittest.main()
