import os
import sys
import unittest
from datetime import datetime, timezone

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from plexcouch import logic  # noqa: E402
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
        self.assertEqual(logic.hr_remaining_text(0.1, 13 * H), "mai are nevoie de 35h seed sau ratio 1")
        self.assertEqual(logic.hr_remaining_text(0.1, 13 * H + 1), "mai are nevoie de 35h seed sau ratio 1")
        self.assertEqual(logic.hr_remaining_text(0, 48 * H - 10), "mai are nevoie de 1h seed sau ratio 1")

    def test_summarize_and_warnings(self):
        ts = [
            {"name": "A", "state": "stalledUP", "progress": 1, "ratio": 0.0, "seeding_time": 903},
            {"name": "B", "state": "uploading", "progress": 1, "ratio": 1.2, "seeding_time": 60},
            {"name": "C", "state": "downloading", "progress": 0.4, "ratio": 0.0, "seeding_time": 0},
        ]
        s = [logic.summarize_torrent(t) for t in ts]
        self.assertEqual([x["hr"] for x in s], ["pending", "safe", "downloading"])
        self.assertTrue(s[1]["safe"])
        self.assertEqual(s[0]["remaining"], "mai are nevoie de 48h seed sau ratio 1")
        w = logic.hr_warnings(s)
        self.assertEqual(w, [{"name": "A", "remaining": "mai are nevoie de 48h seed sau ratio 1"}])
        self.assertNotIn("tracker", s[0])


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
        t = logic.sign_session(self.key, now=1000)
        self.assertTrue(logic.verify_session(self.key, t, now=1000 + 179 * 86400))
        self.assertFalse(logic.verify_session(self.key, t, now=1000 + 180 * 86400 + 1))

    def test_tamper(self):
        t = logic.sign_session(self.key, now=1000)
        exp, sig = t.split(".")
        self.assertFalse(logic.verify_session(self.key, "%d.%s" % (int(exp) + 999999, sig), now=1001))
        self.assertFalse(logic.verify_session(self.key, exp + "." + ("0" * len(sig)), now=1001))
        self.assertFalse(logic.verify_session(b"other", t, now=1001))
        for bad in ("", None, "abc", "1.2.3", ".x", "x.", "12a.ff", "1.é"):
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

    def test_global_cap(self):
        g = logic.LoginGuard()
        locked = 0
        for i in range(20):  # 20 different IPs, 1 failure each
            locked = g.record_failure("10.0.1.%d" % i, now=1000 + i)
        self.assertEqual(locked, 600)
        self.assertGreater(g.remaining("10.9.9.9", now=1100), 0)  # everyone locked
        self.assertEqual(g.remaining("10.9.9.9", now=1019 + 601), 0)

    def test_global_window_expires(self):
        g = logic.LoginGuard()
        for i in range(19):
            g.record_failure("10.0.1.%d" % i, now=1000 + i)
        # an hour later the old failures no longer count
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
        lib = logic.summarize_library(movies, logic.bazarr_ro_map([
            {"radarrId": 2, "subtitles": [{"code2": "ro", "path": "x"}], "missing_subtitles": []}]))
        self.assertEqual(lib, [{"title": "The Godfather", "year": 1972, "size_gb": 19.0, "ro_subs": True}])
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
        data["MediaContainer"]["Metadata"][0]["TranscodeSession"] = {"videoDecision": "transcode"}
        self.assertTrue(logic.parse_plex_sessions(data)[0]["transcode"])

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


if __name__ == "__main__":
    unittest.main()
