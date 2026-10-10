"""Notifications (ntfy), the movie-ready and low-disk checks, and 'shut down after the movie'.

Nothing here reaches the internet: notifications go to a fake ntfy server on 127.0.0.1 or to
a stub, and the power action is a stub (never shutdown.exe)."""

import http.client
import io
import json
import logging
import os
import re
import sys
import tempfile
import threading
import unittest
from contextlib import redirect_stdout
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)

import app  # noqa: E402
from plexcouch import logic, notify  # noqa: E402
from plexcouch.config import Config  # noqa: E402

TOPIC = "plexcouch-unit-test-topic-xyz"
H = 3600


class FakeNtfy:
    """A local stand-in for ntfy.sh that records every POSTed JSON body."""

    def __init__(self, status=200):
        received = self.received = []

        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_POST(self):
                n = int(self.headers.get("Content-Length") or 0)
                raw = self.rfile.read(n)
                received.append({"path": self.path, "ctype": self.headers.get("Content-Type"),
                                 "raw": raw, "json": json.loads(raw.decode("utf-8"))})
                self.send_response(status)
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"{}")

        self.srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.url = "http://127.0.0.1:%d" % self.srv.server_address[1]
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()

    def close(self):
        self.srv.shutdown()
        self.srv.server_close()


class StubConfig:
    def __init__(self, **notifications):
        self.data = {"notifications": notifications}


class RecordingNotifier(notify.Notifier):
    def __init__(self, config, dry_run):
        super().__init__(config, dry_run)
        self.posts = []

    def _post(self, url, data, timeout):
        self.posts.append((url, data, timeout))
        return 200


class TestNotifier(unittest.TestCase):
    def test_json_payload_keeps_diacritics(self):
        n = RecordingNotifier(StubConfig(topic=TOPIC, server="https://ntfy.example/"), dry_run=False)
        self.assertTrue(n.send("Spațiu puțin", "Mai ai 55 GB liberi pe D:. Un film 4K are 20–30 GB.",
                               tags=["warning"], priority="high"))
        url, data, timeout = n.posts[0]
        self.assertEqual((url, timeout), ("https://ntfy.example/", 10))
        self.assertIn("Spațiu puțin".encode("utf-8"), data)  # raw UTF-8, not \\u escapes
        self.assertEqual(json.loads(data.decode("utf-8")), {
            "topic": TOPIC, "title": "Spațiu puțin",
            "message": "Mai ai 55 GB liberi pe D:. Un film 4K are 20–30 GB.",
            "tags": ["warning"], "priority": 4})

    def test_payload_optional_fields(self):
        self.assertEqual(notify.build_payload("t", "a", "b"), {"topic": "t", "title": "a", "message": "b"})
        p = notify.build_payload("t", "a", "b", tags=("x",), priority=9, click="https://x")
        self.assertEqual(p, {"topic": "t", "title": "a", "message": "b", "tags": ["x"], "click": "https://x"})

    def test_topic_never_logged(self):
        class Failing(notify.Notifier):
            def _post(self, url, data, timeout):
                raise OSError("connection to %s refused" % url)

        log = logging.getLogger("plexcouch")
        buf = io.StringIO()
        handler = logging.StreamHandler(buf)
        log.addHandler(handler)
        old = log.level
        log.setLevel(logging.DEBUG)
        try:
            cfg = StubConfig(topic=TOPIC, server="https://ntfy.example")
            self.assertFalse(Failing(cfg, dry_run=False).send("Film gata", "x"))
            self.assertTrue(RecordingNotifier(cfg, dry_run=False).send("Film gata", "x"))
            self.assertTrue(RecordingNotifier(cfg, dry_run=True).send("Film gata", "x"))
        finally:
            log.removeHandler(handler)
            log.setLevel(old)
        out = buf.getvalue()
        self.assertIn("OSError", out)
        self.assertIn("dry-run: notify Film gata", out)
        self.assertNotIn(TOPIC, out)
        self.assertNotIn("ntfy.example", out)

    def test_disabled_without_topic(self):
        for topic in (None, "", "   "):
            n = RecordingNotifier(StubConfig(topic=topic), dry_run=False)
            self.assertFalse(n.enabled)
            self.assertFalse(n.send("a", "b"))
            self.assertEqual(n.posts, [])
        self.assertEqual(notify.Notifier(StubConfig(topic=TOPIC), False).server, "https://ntfy.sh")

    def test_dry_run_never_sends_to_a_real_server(self):
        n = RecordingNotifier(StubConfig(topic=TOPIC), dry_run=True)  # default https://ntfy.sh
        self.assertTrue(n.send("Film gata", "x"))
        self.assertEqual(n.posts, [])
        n = RecordingNotifier(StubConfig(topic=TOPIC, server="http://192.168.0.5:80"), dry_run=True)
        n.send("a", "b")
        self.assertEqual(n.posts, [])

    def test_dry_run_sends_to_a_local_test_server(self):
        for server in ("http://127.0.0.1:9", "http://localhost:9", "http://[::1]:9"):
            n = RecordingNotifier(StubConfig(topic=TOPIC, server=server), dry_run=True)
            n.send("a", "b")
            self.assertEqual(len(n.posts), 1, server)

    def test_real_post_to_fake_server(self):
        fake = FakeNtfy()
        try:
            n = notify.Notifier(StubConfig(topic=TOPIC, server=fake.url), dry_run=False)
            self.assertTrue(n.send("Film gata", "Dune (2024) e gata de vizionat. Subtitrarea RO încă lipsește.",
                                   tags=["movie_camera"]))
        finally:
            fake.close()
        got = fake.received[0]
        self.assertEqual(got["path"], "/")
        self.assertTrue(got["ctype"].startswith("application/json"))
        self.assertEqual(got["json"]["message"], "Dune (2024) e gata de vizionat. Subtitrarea RO încă lipsește.")
        self.assertEqual(got["json"]["tags"], ["movie_camera"])

    def test_http_error_returns_false(self):
        fake = FakeNtfy(status=429)
        try:
            self.assertFalse(notify.Notifier(StubConfig(topic=TOPIC, server=fake.url), False).send("a", "b"))
        finally:
            fake.close()

    def test_new_topic(self):
        a, b = notify.new_topic(), notify.new_topic()
        self.assertRegex(a, r"^plexcouch-[A-Za-z0-9_-]{24}$")
        self.assertNotEqual(a, b)
        self.assertEqual(notify.subscribe_url("https://ntfy.sh/", "t"), "https://ntfy.sh/t")


class TestSetupNtfy(unittest.TestCase):
    def test_creates_keeps_and_replaces(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "cfg.json")
            out = io.StringIO()
            with redirect_stdout(out):
                self.assertEqual(app.setup_ntfy(Config(path)), 0)
            first = Config(path).data["notifications"]["topic"]
            self.assertRegex(first, r"^plexcouch-[A-Za-z0-9_-]{24}$")
            self.assertIn("https://ntfy.sh/" + first, out.getvalue())
            with redirect_stdout(io.StringIO()):
                app.setup_ntfy(Config(path))
            self.assertEqual(Config(path).data["notifications"]["topic"], first)
            with redirect_stdout(io.StringIO()):
                app.setup_ntfy(Config(path), replace=True)
            cfg = Config(path)
            self.assertNotEqual(cfg.data["notifications"]["topic"], first)
            self.assertEqual(cfg.data["notifications"]["disk_min_gb"], 60)  # defaults still merged


def movie(mid, title, year, has_file=True):
    return {"id": mid, "title": title, "year": year, "hasFile": has_file}


class TestMovieReady(unittest.TestCase):
    WAIT = 20 * 60

    def step(self, state, movies, ro_map, now):
        return logic.movie_ready_step(state, movies, ro_map, now, self.WAIT)

    def primed(self):
        state, msgs = self.step(None, [movie(1, "The Godfather", 1972), movie(2, "Dune", 2024, False)], {}, 0)
        self.assertEqual(msgs, [])
        self.assertEqual(state, {"primed": True, "done": ["1"], "pending": {}})
        return state

    def test_first_run_is_a_baseline(self):
        state = self.primed()
        state, msgs = self.step(state, [movie(1, "The Godfather", 1972)], {1: False}, 10 * H)
        self.assertEqual(msgs, [])  # an old movie never notifies, even without RO subtitles

    def test_radarr_down_changes_nothing(self):
        state, msgs = self.step(None, None, None, 0)
        self.assertEqual((state["primed"], msgs), (False, []))
        state = self.primed()
        self.assertEqual(self.step(state, None, None, 99 * H), (state, []))

    def test_ro_present_notifies_at_once(self):
        state = self.primed()
        state, msgs = self.step(state, [movie(1, "The Godfather", 1972), movie(2, "Dune", 2024)], {2: True}, 100)
        self.assertEqual(msgs, ["Dune (2024) e gata de vizionat, cu subtitrare RO."])
        self.assertEqual(state["done"], ["1", "2"])
        state, msgs = self.step(state, [movie(2, "Dune", 2024)], {2: True}, 200)
        self.assertEqual(msgs, [])

    def test_ro_missing_waits_then_notifies(self):
        state = self.primed()
        movies = [movie(1, "The Godfather", 1972), movie(2, "Dune", 2024)]
        state, msgs = self.step(state, movies, {2: False}, 1000)
        self.assertEqual((msgs, state["pending"]), ([], {"2": 1000.0}))
        state, msgs = self.step(state, movies, {}, 1000 + self.WAIT - 1)
        self.assertEqual(msgs, [])
        state, msgs = self.step(state, movies, {2: False}, 1000 + self.WAIT)
        self.assertEqual(msgs, ["Dune (2024) e gata de vizionat. Subtitrarea RO încă lipsește."])
        self.assertEqual(state["pending"], {})
        self.assertEqual(self.step(state, movies, {2: True}, 5 * H)[1], [])

    def test_ro_arrives_during_the_wait(self):
        state = self.primed()
        movies = [movie(2, "Dune", 2024)]
        state, _ = self.step(state, movies, {2: False}, 0)
        state, msgs = self.step(state, movies, {2: True}, 300)
        self.assertEqual(msgs, ["Dune (2024) e gata de vizionat, cu subtitrare RO."])

    def test_bazarr_down_waits_full_time_generic_text(self):
        state = self.primed()
        movies = [movie(2, "Dune", 2024)]
        state, msgs = self.step(state, movies, None, 0)
        self.assertEqual(msgs, [])
        state, msgs = self.step(state, movies, None, self.WAIT)
        self.assertEqual(msgs, ["Dune (2024) e gata de vizionat."])

    def test_no_year(self):
        self.assertEqual(logic.movie_ready_text({"title": "X"}, None), "X e gata de vizionat.")

    def test_flood_is_recorded_not_sent(self):
        state = self.primed()
        movies = [movie(i, "M%d" % i, 2000) for i in range(10, 30)]
        state, msgs = self.step(state, movies, {i: True for i in range(10, 30)}, 0)
        self.assertEqual(msgs, [])
        self.assertIn("29", state["done"])

    def test_no_duplicates_across_a_reload_of_the_state_file(self):
        with tempfile.TemporaryDirectory() as d:
            sf = notify.StateFile(os.path.join(d, "state.local.json"))
            self.assertEqual(sf.load(), {})
            state = self.primed()
            movies = [movie(1, "The Godfather", 1972), movie(2, "Dune", 2024), movie(3, "Past Lives", 2023)]
            state, msgs = self.step(state, movies, {2: True, 3: False}, 0)
            self.assertEqual(len(msgs), 1)
            sf.save({"movie_ready": state})
            reloaded = notify.StateFile(sf.path).load()["movie_ready"]
            state, msgs = self.step(reloaded, movies, {2: True, 3: False}, 60)
            self.assertEqual(msgs, [])  # Dune not repeated, Past Lives still waiting
            sf.save({"movie_ready": state})
            state, msgs = self.step(sf.load()["movie_ready"], movies, {2: True, 3: False}, self.WAIT)
            self.assertEqual(msgs, ["Past Lives (2023) e gata de vizionat. Subtitrarea RO încă lipsește."])

    def test_broken_state_file_reads_empty(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "state.local.json")
            with open(path, "w") as f:
                f.write("{not json")
            self.assertEqual(notify.StateFile(path).load(), {})


class TestDiskLow(unittest.TestCase):
    def test_hysteresis(self):
        low, seq = False, []
        for free in (70, 55, 50, 65, 70, 71, 59, None, 58):
            low, warn = logic.disk_low_step(low, free, 60)
            seq.append(warn)
        self.assertEqual(seq, [False, True, False, False, False, False, True, False, False])

    def test_text(self):
        self.assertEqual(logic.disk_low_text("D:", 55.9), "Mai ai 55 GB liberi pe D:. Un film 4K are 20–30 GB.")

    def test_drive_of(self):
        self.assertEqual(logic.drive_of("d:\\Movies"), "D:")
        self.assertEqual(logic.drive_of("\\\\nas\\media\\Movies"), "\\\\nas\\media")
        self.assertIsNone(logic.drive_of("/movies"))
        self.assertIsNone(logic.drive_of(None))


def session(key, title="The Godfather", progress=10.0, address="192.168.0.40", state="playing"):
    return {"key": key, "title": title, "progress": progress, "address": address, "state": state}


class TestAfterMovie(unittest.TestCase):
    OWN = {"192.168.0.10"}

    def arm(self, sessions, now=0):
        return logic.after_movie_arm(sessions, self.OWN, now)

    def test_nothing_playing_on_the_tv(self):
        self.assertIsNone(self.arm([]))
        self.assertIsNone(self.arm(None))
        self.assertIsNone(self.arm([session("1", address="127.0.0.1"), session("2", address="192.168.0.10")]))

    def test_arm_follows_the_tv_session(self):
        st = self.arm([session("9", "Local", address="127.0.0.1"), session("5", progress=12.5)], now=100)
        self.assertEqual((st["key"], st["title"], st["since"], st["progress"]), ("5", "The Godfather", 100, 12.5))
        self.assertEqual(logic.public_after_movie(st), {"armed": True, "title": "The Godfather", "since": 100})
        self.assertIsNone(logic.public_after_movie(None))

    def test_ended_after_90_percent_fires_once(self):
        st = self.arm([session("5")])
        st, act = logic.after_movie_step(st, [session("5", progress=95.2)], self.OWN, 60)
        self.assertEqual((act, st["progress"]), (None, 95.2))
        st, act = logic.after_movie_step(st, [], self.OWN, 70)
        self.assertEqual((st, act), (None, "fire"))
        self.assertEqual(logic.after_movie_step(st, [], self.OWN, 80), (None, None))

    def test_exactly_90_fires(self):
        st = self.arm([session("5", progress=90.0)])
        self.assertEqual(logic.after_movie_step(st, [], self.OWN, 10)[1], "fire")

    def test_stopped_early_does_not_fire_and_stays_armed(self):
        st = self.arm([session("5", progress=40)])
        st, act = logic.after_movie_step(st, [session("5", progress=89.9)], self.OWN, 10)
        st, act = logic.after_movie_step(st, [], self.OWN, 20)
        self.assertIsNone(act)
        self.assertIsNotNone(st)
        self.assertIsNone(st["key"])
        # The user starts it again (new session key): it is followed, and fires at the end.
        st, act = logic.after_movie_step(st, [session("8", progress=89.95)], self.OWN, 30)
        self.assertEqual((st["key"], act), ("8", None))
        st, act = logic.after_movie_step(st, [session("8", progress=97)], self.OWN, 40)
        self.assertEqual(logic.after_movie_step(st, [], self.OWN, 50), (None, "fire"))

    def test_same_title_preferred_when_it_comes_back(self):
        st = self.arm([session("5", progress=30)])
        st, _ = logic.after_movie_step(st, [], self.OWN, 10)
        st, _ = logic.after_movie_step(st, [session("6", "Other", state="playing"),
                                            session("7", "The Godfather", state="paused")], self.OWN, 20)
        self.assertEqual(st["key"], "7")

    def test_local_sessions_ignored(self):
        st = self.arm([session("5", progress=95)])
        st, act = logic.after_movie_step(st, [session("5", progress=95, address="127.0.0.1")], self.OWN, 10)
        self.assertEqual((st, act), (None, "fire"))

    def test_expires_after_3_hours_with_nothing_on_the_tv(self):
        st = self.arm([session("5", progress=30)], now=0)
        st, _ = logic.after_movie_step(st, [], self.OWN, 100)  # stopped early at t=100
        st, act = logic.after_movie_step(st, [], self.OWN, 100 + 3 * H - 1)
        self.assertEqual((act, st is not None), (None, True))
        self.assertEqual(logic.after_movie_step(st, [], self.OWN, 100 + 3 * H), (None, "expire"))

    def test_long_movie_does_not_expire_while_it_plays(self):
        st = self.arm([session("5", progress=1)], now=0)
        st, act = logic.after_movie_step(st, [session("5", progress=80)], self.OWN, 4 * H)
        self.assertIsNone(act)
        st, act = logic.after_movie_step(st, [session("5", progress=99)], self.OWN, 4 * H + 60)
        self.assertEqual(logic.after_movie_step(st, [], self.OWN, 4 * H + 70), (None, "fire"))


class FakeActions:
    def __init__(self):
        self.calls = []
        self.pending_value = None

    def power(self, kind):
        self.calls.append(kind)
        self.pending_value = {"kind": kind, "at": 60}
        return self.pending_value

    def pending(self):
        return self.pending_value

    def cancel_power(self):
        self.pending_value = None


class FakeServices:
    def __init__(self):
        self.sessions = []
        self.movies = []
        self.bazarr = []
        self.roots = [{"path": "D:\\Movies"}]

    def plex_sessions(self):
        if isinstance(self.sessions, Exception):
            raise self.sessions
        return self.sessions

    def radarr_movies(self):
        return self.movies

    def bazarr_movies(self):
        return self.bazarr

    def radarr_root_folders(self):
        return self.roots


class AppCase(unittest.TestCase):
    """A real App (dry-run) on a temp config, with stubbed services, power and own IPs."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.fake = FakeNtfy()
        cfg = Config(os.path.join(self.tmp.name, "cfg.json"))
        cfg.set_pin_hash(logic.hash_pin("1234", iterations=1000))
        cfg._raw["notifications"] = {"topic": TOPIC, "server": self.fake.url}
        cfg._save_locked()
        cfg.load()
        self.app = app.App(cfg, dry_run=True)
        self.app.actions = FakeActions()
        self.app.services = FakeServices()
        self.app.own_ips = lambda: {"192.168.0.10"}

    def tearDown(self):
        self.app.pool.shutdown(wait=False)
        self.fake.close()
        self.tmp.cleanup()


class TestAppNotify(AppCase):
    def test_state_file_next_to_config(self):
        self.assertEqual(self.app.state_file.path, os.path.join(self.tmp.name, "state.local.json"))

    def test_tick_baseline_then_ready_then_no_repeat_after_restart(self):
        svc = self.app.services
        svc.movies = [movie(1, "The Godfather", 1972)]
        old_free = app.free_gb
        app.free_gb = lambda drive: 500.0
        try:
            self.assertEqual(self.app.notify_tick(now=0), [])
            svc.movies.append(movie(2, "Dune: Part Two", 2024))
            svc.bazarr = [{"radarrId": 2, "subtitles": [{"code2": "ro"}], "missing_subtitles": []}]
            sent = self.app.notify_tick(now=60)
            self.assertEqual([s[:2] for s in sent],
                             [("Film gata", "Dune: Part Two (2024) e gata de vizionat, cu subtitrare RO.")])
            # A new App on the same config folder (a restart) does not repeat it.
            again = app.App(self.app.config, dry_run=True)
            again.services = svc
            again.pool.shutdown(wait=False)
            self.assertEqual(again.notify_tick(now=120), [])
        finally:
            app.free_gb = old_free
        self.assertEqual([r["json"]["title"] for r in self.fake.received], ["Film gata"])
        self.assertEqual(self.fake.received[0]["json"]["tags"], ["movie_camera"])

    def test_tick_disk_low_once_on_radarr_drive(self):
        drives = []
        old_free = app.free_gb
        app.free_gb = lambda drive: drives.append(drive) or 55.4
        try:
            sent = self.app.notify_tick(now=0)
            self.assertEqual(self.app.notify_tick(now=60), [])
        finally:
            app.free_gb = old_free
        self.assertEqual(drives, ["D:", "D:"])
        self.assertEqual(sent, [("Spațiu puțin", "Mai ai 55 GB liberi pe D:. Un film 4K are 20–30 GB.",
                                 ["warning"], "high")])
        got = self.fake.received[0]["json"]
        self.assertEqual((got["title"], got["priority"]), ("Spațiu puțin", 4))

    def test_after_movie_poll_fires_power_once_and_notifies(self):
        svc = self.app.services
        svc.sessions = [session("5", progress=50)]
        self.assertEqual(self.app.arm_after_movie()[0], 200)
        self.assertIsNone(self.app.after_movie_poll([session("5", progress=96)], now=10))
        self.assertEqual(self.app.after_movie_poll([], now=20), "fire")
        self.assertIsNone(self.app.after_movie_poll([], now=30))
        self.assertEqual(self.app.actions.calls, ["shutdown"])
        for t in threading.enumerate():
            if t.name == "notify":
                t.join(5)
        got = self.fake.received[-1]["json"]
        self.assertEqual((got["title"], got["message"]),
                         ("Oprire după film", "PC-ul se oprește în 60 de secunde. Anulează din Plex Couch."))
        self.assertIsNone(self.app.power_status()["after_movie"])

    def test_after_movie_stopped_early_no_power(self):
        self.app.services.sessions = [session("5", progress=50)]
        self.app.arm_after_movie()
        self.assertIsNone(self.app.after_movie_poll([], now=10))
        self.assertEqual(self.app.actions.calls, [])
        self.assertTrue(self.app.power_status()["after_movie"]["armed"])

    def test_arm_errors(self):
        svc = self.app.services
        self.assertEqual(self.app.arm_after_movie(), (409, {"error": "Nu rulează nimic pe TV acum."}))
        svc.sessions = OSError("down")
        self.assertEqual(self.app.arm_after_movie()[0], 503)
        svc.sessions = [session("5")]
        self.app.actions.pending_value = {"kind": "shutdown", "at": 1}
        self.assertEqual(self.app.arm_after_movie()[0], 409)

    def test_power_status_shape(self):
        self.assertIsNone(self.app.power_status())
        self.app.actions.pending_value = {"kind": "shutdown", "at": 5}
        self.assertEqual(self.app.power_status(), {"kind": "shutdown", "at": 5, "after_movie": None})


class TestRoutes(AppCase):
    """The new endpoints over a real local server: login, CSRF, 409, the test notification."""

    def setUp(self):
        super().setUp()
        self.old_app = app.Handler.app
        app.Handler.app = self.app
        self.srv = app.Server(("127.0.0.1", 0), app.Handler)
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()
        self.port = self.srv.server_address[1]
        self.tok = self.app.new_session()

    def tearDown(self):
        self.srv.shutdown()
        self.srv.server_close()
        app.Handler.app = self.old_app
        super().tearDown()

    def req(self, method, path, cookie=True, csrf=True):
        c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        headers = {}
        if cookie:
            headers["Cookie"] = "pc_session=%s" % self.tok
        if csrf:
            headers["X-Requested-With"] = "plex-couch"
        if method == "POST":
            headers["Content-Type"] = "application/json"
        c.request(method, path, body=b"{}" if method == "POST" else None, headers=headers)
        r = c.getresponse()
        body = json.loads(r.read().decode("utf-8"))
        c.close()
        return r.status, body

    def test_auth_and_csrf_required(self):
        for path in ("/api/notify/test", "/api/power/after-movie", "/api/power/after-movie/cancel"):
            self.assertEqual(self.req("POST", path, cookie=False)[0], 401, path)
            self.assertEqual(self.req("POST", path, csrf=False)[0], 403, path)
        self.assertEqual(self.fake.received, [])

    def test_status_has_topic_when_logged_in(self):
        self.app.services.sessions = []
        status, body = self.req("GET", "/api/status", cookie=False)
        self.assertEqual(status, 401)
        self.assertNotIn(TOPIC, json.dumps(body))
        self.app.status = lambda: {"notifications": self.app.notifications_status(),
                                   "power": self.app.power_status()}
        status, body = self.req("GET", "/api/status")
        self.assertEqual((status, body["notifications"]["topic"], body["power"]), (200, TOPIC, None))

    def test_notify_test_reaches_the_server(self):
        status, body = self.req("POST", "/api/notify/test")
        self.assertEqual((status, body), (200, {"ok": True}))
        got = self.fake.received[0]
        self.assertEqual(got["json"], {"topic": TOPIC, "title": "Plex Couch",
                                       "message": "Test Plex Couch: notificările merg."})
        self.assertIn("notificările".encode("utf-8"), got["raw"])

    def test_notify_test_without_topic(self):
        self.app.config.data["notifications"]["topic"] = None
        status, body = self.req("POST", "/api/notify/test")
        self.assertEqual(status, 409)
        self.assertIn("--setup-ntfy", body["error"])

    def test_after_movie_nothing_playing_409(self):
        self.app.services.sessions = [session("1", address="127.0.0.1")]
        status, body = self.req("POST", "/api/power/after-movie")
        self.assertEqual((status, body), (409, {"error": "Nu rulează nimic pe TV acum."}))
        self.assertIsNone(self.app.power_status())

    def test_after_movie_arm_and_cancel(self):
        self.app.services.sessions = [session("5", "The Godfather", progress=20)]
        status, body = self.req("POST", "/api/power/after-movie")
        self.assertEqual(status, 200)
        self.assertEqual(body["power"]["after_movie"]["title"], "The Godfather")
        self.assertIsNone(body["power"].get("kind"))
        status, body = self.req("POST", "/api/power/after-movie/cancel")
        self.assertEqual((status, body), (200, {"ok": True, "power": None}))
        self.assertEqual(self.app.actions.calls, [])


class TestStaticUi(unittest.TestCase):
    def read(self, name):
        with open(os.path.join(app.STATIC_DIR, name), encoding="utf-8") as f:
            return f.read()

    def test_ui_texts(self):
        html, js = self.read("index.html"), self.read("app.js")
        for text in ("Oprește după film", "Nu mai opri", "Notificări", "Abonează-te în ntfy", "Copiază numele",
                     "Trimite un test", "Pornește notificările cu app.py --setup-ntfy."):
            self.assertIn(text, html)
        for text in ("PC-ul se oprește după film.", "Oprirea după film a fost anulată.",
                     "Notificarea de test a fost trimisă.", "Dacă îl oprești mai devreme, PC-ul rămâne pornit.",
                     "/api/power/after-movie", "/api/power/after-movie/cancel", "/api/notify/test"):
            self.assertIn(text, js)
        self.assertIsNone(re.search(r"plexcouch-[A-Za-z0-9_-]{20,}", html + js))


if __name__ == "__main__":
    unittest.main()
