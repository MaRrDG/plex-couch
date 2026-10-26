//! `GET /api/status` (spec 3.5) and the demo data (spec 8).
//!
//! The 7 upstream calls run concurrently with one shared 6 s deadline (plan fix 14: each
//! request also has its own timeout, and whatever is still running at the deadline is
//! cancelled, so nothing piles up). Seeding items carry the seed keeper's `watched` and
//! `waiting_watch` (plan "New behaviour" 3) through [`seed_snapshot`].

use crate::app::AppState;
use crate::logic::images::poster_url;
use crate::logic::movies::{bazarr_ro_map, queue_movie_ids, summarize_library, summarize_queue_record, summarize_waiting};
use crate::logic::power::{is_tv_player, power_status};
use crate::logic::torrents::{hr_warnings, sort_torrents, summarize_torrent, torrent_title, torrent_title_map, SeedInfo};
use crate::log_debug;
use crate::services::{UpResult, UpstreamError};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

/// The shared deadline of the upstream calls.
pub const STATUS_DEADLINE: Duration = Duration::from_secs(6);

/// What the seed keeper knows per torrent hash (lower case): ledger seconds, watched and
/// its own "H&R done, not watched yet" (`jobs::seed_keeper::snapshot`). A torrent the
/// ledger does not know falls back to qBittorrent's `seeding_time` and `watched: null`.
pub fn seed_snapshot(app: &AppState) -> HashMap<String, SeedInfo> {
    crate::jobs::seed_keeper::snapshot(app)
        .into_iter()
        .map(|(hash, s)| {
            let info = SeedInfo { seconds: Some(s.seconds), watched: s.watched, waiting_watch: Some(s.waiting_watch) };
            (hash.to_lowercase(), info)
        })
        .collect()
}

async fn until<T>(deadline: tokio::time::Instant, name: &str, f: impl Future<Output = UpResult<T>>) -> Option<T> {
    let r = match tokio::time::timeout_at(deadline, f).await {
        Ok(r) => r,
        Err(_) => Err(UpstreamError::Timeout),
    };
    match r {
        Ok(v) => Some(v),
        Err(e) => {
            log_debug!("status: {name} failed: {}", e.type_name());
            None
        }
    }
}

/// The seeding list: FileList torrents with Radarr titles and the seed keeper's view.
pub fn build_torrents(
    torrents: &[Value],
    history: Option<&[Value]>,
    queue: Option<&[Value]>,
    movies: Option<&[Value]>,
    seeds: &HashMap<String, SeedInfo>,
) -> Vec<Value> {
    let titles = torrent_title_map(history, queue, movies);
    let mut out: Vec<Value> = torrents
        .iter()
        .map(|t| {
            let hash = t.get("hash").and_then(Value::as_str).unwrap_or("").to_lowercase();
            summarize_torrent(t, Some(&torrent_title(t, Some(&titles))), seeds.get(&hash))
        })
        .collect();
    sort_torrents(&mut out);
    out
}

/// The full status JSON.
pub async fn status(app: &Arc<AppState>) -> Value {
    if app.demo {
        let mut st = demo_status(app);
        st["notifications"] = notifications_status(app);
        return st;
    }
    let s = &app.services;
    let deadline = tokio::time::Instant::now() + STATUS_DEADLINE;
    // PC info and own IPs are blocking calls (disks, DNS): off the async workers.
    let a2 = app.clone();
    let local = tokio::task::spawn_blocking(move || (crate::system::info::pc_info(&a2.config), a2.own_ips()));
    let (plex, queue, movies, bazarr, prowlarr, qbittorrent, history) = tokio::join!(
        until(deadline, "plex", s.plex.sessions()),
        until(deadline, "radarr_queue", s.radarr.queue()),
        until(deadline, "radarr_movies", s.radarr.movies()),
        until(deadline, "bazarr", s.bazarr.movies()),
        until(deadline, "prowlarr", s.prowlarr.ping()),
        until(deadline, "qbittorrent", s.qbittorrent.filelist_torrents()),
        until(deadline, "radarr_history", s.radarr.history()),
    );
    let (pc, own_ips) = local.await.unwrap_or_else(|_| (json!({"hostname": "", "uptime": null, "disks": [], "platform": "win32"}), HashSet::new()));

    let services = json!({
        "plex": plex.is_some(),
        "radarr": movies.is_some() || queue.is_some(),
        "prowlarr": prowlarr.is_some(),
        "bazarr": bazarr.is_some(),
        "qbittorrent": qbittorrent.is_some(),
    });
    let torrents = build_torrents(
        qbittorrent.as_deref().unwrap_or(&[]),
        history.as_deref(),
        queue.as_deref(),
        movies.as_deref(),
        &seed_snapshot(app),
    );
    let ro_map = bazarr.as_deref().map(bazarr_ro_map);
    let queue_ids = queue_movie_ids(queue.as_deref().unwrap_or(&[]));
    let now = crate::auth::session::now();

    let mut out = Map::new();
    out.insert("app_name".into(), json!(app.config.app_name()));
    out.insert("server_time".into(), json!(now));
    out.insert("dry_run".into(), json!(app.dry_run));
    out.insert("pc".into(), pc);
    out.insert("services".into(), services);
    let playing: Vec<Value> =
        plex.as_deref().unwrap_or(&[]).iter().map(|x| public_playing(x, movies.as_deref(), &own_ips)).collect();
    out.insert("now_playing".into(), Value::Array(playing));
    out.insert(
        "downloads".into(),
        queue.as_ref().map(|q| Value::Array(q.iter().map(summarize_queue_record).collect())).unwrap_or(Value::Null),
    );
    out.insert(
        "waiting".into(),
        movies.as_ref().map(|m| Value::Array(summarize_waiting(m, &queue_ids, now))).unwrap_or(Value::Null),
    );
    out.insert(
        "library".into(),
        movies.as_ref().map(|m| Value::Array(summarize_library(m, ro_map.as_ref()))).unwrap_or(Value::Null),
    );
    let warnings = hr_warnings(&torrents);
    out.insert("torrents".into(), if qbittorrent.is_some() { Value::Array(torrents) } else { Value::Null });
    out.insert("hr_warnings".into(), Value::Array(warnings));
    out.insert("power".into(), power_status(app));
    out.insert("settings".into(), app.config.settings());
    out.insert("notifications".into(), notifications_status(app));
    Value::Object(out)
}

/// Demo status (fixed data).
pub fn demo_status(app: &Arc<AppState>) -> Value {
    let played = 2843 + app.started.elapsed().as_secs() as i64;
    crate::logic::demo::demo_status(
        &app.config.app_name(),
        power_status(app),
        app.config.settings(),
        played,
        crate::auth::session::now(),
    )
}

/// The demo movie as an internal Plex session (for arming "shut down after the movie").
pub fn demo_sessions(app: &AppState) -> Vec<Value> {
    let played = 2843 + app.started.elapsed().as_secs() as i64;
    let progress = crate::json::py_round(played as f64 / 9960.0 * 100.0, 1);
    vec![json!({"key": "demo", "title": "Dune: Part Two", "year": 2024, "state": "playing",
                "progress": progress, "address": "10.0.0.20"})]
}

/// Only ever returned to logged-in clients; the topic is never logged.
pub fn notifications_status(app: &AppState) -> Value {
    let n = &app.notifier;
    let enabled = n.enabled();
    json!({"enabled": enabled, "topic": n.topic(), "server": if enabled { Value::from(n.server()) } else { Value::Null }})
}

/// Drop the internal fields (`address`, `key`).
pub fn public_session(s: &Value) -> Value {
    match s {
        Value::Object(m) => {
            let out: Map<String, Value> =
                m.iter().filter(|(k, _)| *k != "address" && *k != "key").map(|(k, v)| (k.clone(), v.clone())).collect();
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// No Plex art for a playing movie: use Radarr's fanart of the same movie (title casefold +
/// year).
pub fn with_backdrop_fallback(mut session: Value, movies: Option<&[Value]>) -> Value {
    let has = crate::json::truthy(session.get("backdrop"));
    let Some(movies) = movies.filter(|m| !m.is_empty()) else { return session };
    if has {
        return session;
    }
    let title = session.get("title").and_then(Value::as_str).unwrap_or("").to_lowercase();
    for m in movies {
        let mt = m.get("title").and_then(Value::as_str).unwrap_or("").to_lowercase();
        if mt == title && m.get("year") == session.get("year") {
            let id = match m.get("id") {
                Some(Value::Number(n)) => n.to_string(),
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            };
            session["backdrop"] = poster_url("movie-art", &id).map(Value::from).unwrap_or(Value::Null);
            break;
        }
    }
    session
}

/// A `now_playing` entry: public fields, backdrop fallback, `on_tv`.
pub fn public_playing(session: &Value, movies: Option<&[Value]>, own_ips: &HashSet<String>) -> Value {
    let mut out = with_backdrop_fallback(public_session(session), movies);
    out["on_tv"] = Value::Bool(is_tv_player(session.get("address").and_then(Value::as_str), own_ips));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // py: test_security.TestTvSessions.test_on_tv_flag_in_status
    #[test]
    fn on_tv_flag_in_status() {
        let own: HashSet<String> = ["192.168.0.10".to_string()].into_iter().collect();
        let remote = public_playing(&json!({"title": "X", "address": "85.120.10.1"}), Some(&[]), &own);
        let tv = public_playing(&json!({"title": "Y", "address": "192.168.0.40"}), Some(&[]), &own);
        assert_eq!((remote["on_tv"].clone(), tv["on_tv"].clone()), (json!(false), json!(true)));
        assert!(tv.get("address").is_none());
    }

    use crate::logic::testutil::{self, json_answer, serve, status_answer, Hit};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const H1: &str = "aaaa000000000000000000000000000000000001";
    const H2: &str = "bbbb000000000000000000000000000000000002";
    const H3: &str = "cccc000000000000000000000000000000000003";
    const H4: &str = "dddd000000000000000000000000000000000004";

    fn radarr(h: &Hit) -> testutil::Answer {
        let p = h.path.as_str();
        if h.header("X-Api-Key") != Some("radarr-test-key") {
            return status_answer(401);
        }
        if p.starts_with("/api/v3/queue?includeMovie=true&pageSize=200") {
            json_answer(&json!({"records": [
                {"movieId": 3, "size": 100, "sizeleft": 38, "status": "downloading", "trackedDownloadState": "downloading",
                 "timeleft": "00:08:00", "downloadId": H3.to_uppercase(), "movie": {"title": "Anatomy of a Fall", "year": 2023}},
                {"movieId": 4, "size": 10, "sizeleft": 5, "status": "downloading", "timeleft": "x.01:00:00",
                 "movie": {"title": "Bad Time", "year": 2020}},
            ]}))
        } else if p == "/api/v3/movie" {
            json_answer(&json!([
                {"id": 1, "title": "The Odyssey", "year": 2026, "monitored": true, "hasFile": false, "isAvailable": false,
                 "digitalRelease": "2099-11-17T00:00:00Z"},
                {"id": 2, "title": "The Godfather", "year": 1972, "monitored": true, "hasFile": true, "movieFile": {"size": 20419862888u64}},
                {"id": 3, "title": "Anatomy of a Fall", "year": 2023, "monitored": true, "hasFile": false, "isAvailable": true},
                {"id": 5, "title": "Dune: Part Two", "year": 2024, "monitored": true, "hasFile": true, "sizeOnDisk": 1073741824},
            ]))
        } else if p.starts_with("/api/v3/history?eventType=1&pageSize=200") {
            json_answer(&json!({"records": [{"downloadId": H1.to_uppercase(), "movieId": 2}]}))
        } else {
            status_answer(404)
        }
    }

    struct World {
        plex: testutil::FakePlex,
        radarr: testutil::Fake,
        bazarr: testutil::Fake,
        prowlarr: testutil::Fake,
        qbit: testutil::Fake,
        tracker_calls: Arc<AtomicUsize>,
    }

    async fn world() -> World {
        let plex = testutil::fake_plex().await;
        plex.set_sessions(&[
            testutil::session("7", "Dune: Part Two", 25.0, "192.168.0.40", "playing"),
            testutil::session("8", "Remote", 5.0, "85.120.10.1", "paused"),
        ]);
        let radarr = serve(Arc::new(radarr)).await;
        let bazarr = serve(Arc::new(|h: &Hit| {
            if h.header("X-API-KEY") != Some("bazarr-test-key") {
                return status_answer(401);
            }
            json_answer(&json!({"data": [{"radarrId": 2, "subtitles": [{"code2": "ro"}], "missing_subtitles": []},
                                         {"radarrId": 5, "subtitles": [], "missing_subtitles": [{"code2": "ro"}]}]}))
        }))
        .await;
        let prowlarr = serve(Arc::new(|_| (200, "text/plain".into(), b"pong".to_vec()))).await;
        let logged_in = Arc::new(AtomicBool::new(false));
        let tracker_calls = Arc::new(AtomicUsize::new(0));
        let (li, tc) = (logged_in.clone(), tracker_calls.clone());
        let qbit = serve(Arc::new(move |h: &Hit| {
            if h.path == "/api/v2/auth/login" {
                li.store(true, Ordering::SeqCst);
                return (200, "text/plain\r\nSet-Cookie: SID=qb-sid-1; HttpOnly; path=/".into(), b"Ok.".to_vec());
            }
            if !li.load(Ordering::SeqCst) || h.header("Cookie") != Some("SID=qb-sid-1") {
                return status_answer(403);
            }
            if h.path == "/api/v2/torrents/info" {
                return json_answer(&json!([
                    // FileList by its current tracker
                    {"hash": H1, "name": "The.Godfather.1972.2160p.WEB.H265-PETRiFiED.mkv", "state": "stalledUP", "progress": 1,
                     "ratio": 0.1, "seeding_time": 903, "tracker": "https://reactor.thefl.org/x/announce"},
                    // current tracker is another one, FileList is in its list (plan fix 13)
                    {"hash": H2, "name": "Past.Lives.2023.2160p.WEB-DL-GROUP", "state": "uploading", "progress": 1, "ratio": 1.2,
                     "seeding_time": 60, "tracker": "udp://tracker.opentrackr.org:1337/announce"},
                    // no current tracker, FileList in the list
                    {"hash": H3, "name": "Anatomy.of.a.Fall.2023.2160p.WEB.H265-GROUP", "state": "downloading", "progress": 0.62,
                     "ratio": 0.0, "seeding_time": 0, "tracker": "", "dlspeed": 1000},
                    // not FileList at all
                    {"hash": H4, "name": "Other.2020.1080p", "state": "uploading", "progress": 1, "ratio": 3, "tracker": ""},
                ]));
            }
            if let Some(q) = h.path.strip_prefix("/api/v2/torrents/trackers?hash=") {
                tc.fetch_add(1, Ordering::SeqCst);
                let fl = q == H2 || q == H3;
                let url = if fl { "http://tracker.filelist.io/announce" } else { "udp://open.example/announce" };
                return json_answer(&json!([{"url": "** [DHT] **", "status": 0}, {"url": url, "status": 2}]));
            }
            status_answer(404)
        }))
        .await;
        World { plex, radarr, bazarr, prowlarr, qbit, tracker_calls }
    }

    fn world_app(w: &World) -> Arc<AppState> {
        let cfg = testutil::config(json!({
            "services": {
                "plex": {"url": w.plex.fake.url}, "radarr": {"url": w.radarr.url}, "bazarr": {"url": w.bazarr.url},
                "prowlarr": {"url": w.prowlarr.url}, "qbittorrent": {"url": w.qbit.url},
            },
            "disks": [std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into())],
        }));
        testutil::app(cfg)
    }

    const KEYS: [&str; 14] = ["app_name", "server_time", "dry_run", "pc", "services", "now_playing", "downloads",
                              "waiting", "library", "torrents", "hr_warnings", "power", "settings", "notifications"];

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_from_every_service() {
        let w = world().await;
        let app = world_app(&w);
        let st = status(&app).await;
        let keys: Vec<&String> = st.as_object().unwrap().keys().collect();
        assert_eq!(keys, KEYS);
        assert_eq!(st["services"], json!({"plex": true, "radarr": true, "prowlarr": true, "bazarr": true, "qbittorrent": true}));
        assert_eq!((st["app_name"].clone(), st["dry_run"].clone()), (json!("Plex Couch"), json!(true)));
        assert!(st["server_time"].is_f64());
        assert_eq!(st["pc"]["platform"], json!("win32"));
        #[cfg(windows)]
        assert_eq!(st["pc"]["disks"].as_array().unwrap().len(), 1);

        let np = st["now_playing"].as_array().unwrap();
        assert_eq!(np.len(), 2);
        assert_eq!((np[0]["title"].clone(), np[0]["on_tv"].clone(), np[1]["on_tv"].clone()), (json!("Dune: Part Two"), json!(true), json!(false)));
        assert!(np[0].get("address").is_none() && np[0].get("key").is_none());
        assert_eq!(np[0]["backdrop"], json!(null)); // no Plex art, and the session has no year to match Radarr's fanart

        let d = st["downloads"].as_array().unwrap();
        assert_eq!((d[0]["percent"].clone(), d[0]["time_left"].clone(), d[0]["poster"].clone()), (json!(62.0), json!("8 min"), json!("/img/movie/3")));
        assert_eq!(d[1]["time_left"], json!("")); // plan fix 17: a bad day part does not abort the status

        let wt: Vec<(String, String)> = st["waiting"].as_array().unwrap().iter()
            .map(|x| (x["title"].as_str().unwrap().into(), x["status"].as_str().unwrap().into())).collect();
        assert_eq!(wt, [("The Odyssey".to_string(), "Așteaptă lansarea digitală (17 nov 2099)".to_string()),
                        ("Anatomy of a Fall".to_string(), "Se descarcă acum".to_string())]);

        let lib = st["library"].as_array().unwrap();
        let l: Vec<(Value, Value, Value)> = lib.iter().map(|x| (x["title"].clone(), x["size_gb"].clone(), x["ro_subs"].clone())).collect();
        assert_eq!(l, [(json!("Dune: Part Two"), json!(1.0), json!(false)), (json!("The Godfather"), json!(19.0), json!(true))]);

        let t = st["torrents"].as_array().unwrap();
        let tt: Vec<(&str, &str)> = t.iter().map(|x| (x["title"].as_str().unwrap(), x["hr"].as_str().unwrap())).collect();
        assert_eq!(tt, [("The Godfather", "pending"), ("Anatomy of a Fall", "downloading"), ("Past Lives (2023)", "safe")]);
        assert!(t.iter().all(|x| x.get("hash").is_none() && x.get("tracker").is_none()));
        assert!(t.iter().all(|x| x["watched"].is_null() && x["waiting_watch"] == json!(false)));
        assert_eq!(st["hr_warnings"], json!([{"title": "The Godfather", "name": "The.Godfather.1972.2160p.WEB.H265-PETRiFiED.mkv",
                                              "remaining": "mai are nevoie de 48 h sau ratio 1"}]));
        assert_eq!((st["power"].clone(), st["notifications"]["enabled"].clone()), (Value::Null, json!(false)));

        // qBittorrent: 403 -> login (form, Referer/Origin) -> retry with the cookie
        let qb = w.qbit.hits();
        assert_eq!(qb[0].path, "/api/v2/torrents/info");
        let login = qb.iter().find(|h| h.path == "/api/v2/auth/login").unwrap();
        assert_eq!((login.method.as_str(), String::from_utf8_lossy(&login.body).to_string()), ("POST", "username=admin&password=x".to_string()));
        assert_eq!((login.header("Referer"), login.header("Origin")), (Some(w.qbit.url.as_str()), Some(w.qbit.url.as_str())));
        // the tracker lists: H2, H3, H4 (not H1, whose current tracker is FileList); cached per hash
        assert_eq!(w.tracker_calls.load(Ordering::SeqCst), 3);
        let again = status(&app).await;
        assert_eq!(w.tracker_calls.load(Ordering::SeqCst), 3);
        assert_eq!(again["torrents"].as_array().unwrap().len(), 3);
        // Radarr movies (30 s), history (60 s) and Bazarr (60 s) are cached; the queue is not
        let rp = w.radarr.paths();
        assert_eq!(rp.iter().filter(|p| p.as_str() == "/api/v3/movie").count(), 1);
        assert_eq!(rp.iter().filter(|p| p.starts_with("/api/v3/queue")).count(), 2);
        assert_eq!(w.bazarr.hits().len(), 1);
        assert_eq!(w.plex.fake.hits().len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_with_every_service_down() {
        let app = testutil::app(testutil::config(json!({})));
        let st = status(&app).await;
        let keys: Vec<&String> = st.as_object().unwrap().keys().collect();
        assert_eq!(keys, KEYS);
        assert_eq!(st["services"], json!({"plex": false, "radarr": false, "prowlarr": false, "bazarr": false, "qbittorrent": false}));
        assert_eq!(st["now_playing"], json!([])); // [] when Plex is down, not null
        for k in ["downloads", "waiting", "library", "torrents", "power"] {
            assert_eq!(st[k], Value::Null, "{k}");
        }
        assert_eq!(st["hr_warnings"], json!([]));
        assert_eq!(st["notifications"], json!({"enabled": false, "topic": null, "server": null}));
    }

    // Plan fix 14: a hung upstream costs its own timeout, nothing more, and the rest is served.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hung_upstream_does_not_hold_the_status() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((s, _)) = l.accept().await {
                held.push(s); // never answers
            }
        });
        let w = world().await;
        let cfg = testutil::config(json!({"services": {"prowlarr": {"url": url}, "radarr": {"url": w.radarr.url}}}));
        let app = testutil::app(cfg);
        let t0 = std::time::Instant::now();
        let st = status(&app).await;
        let took = t0.elapsed();
        assert!(took < Duration::from_millis(4000), "{took:?}");
        assert_eq!((st["services"]["prowlarr"].clone(), st["services"]["radarr"].clone()), (json!(false), json!(true)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_uses_the_seed_ledger() {
        let w = world().await;
        let app = world_app(&w);
        let ledger = json!({"version": 1, "torrents": {
            H1: {"seconds": 50.0 * 3600.0, "watched": false, "waiting_watch": true},
            H2: {"seconds": 70.0 * 3600.0, "watched": true, "waiting_watch": false},
        }});
        std::fs::write(crate::jobs::seed_keeper::ledger_path(&app), crate::json::dumps(&ledger)).unwrap();
        let st = status(&app).await;
        let t = st["torrents"].as_array().unwrap();
        let by: HashMap<&str, &Value> = t.iter().map(|x| (x["title"].as_str().unwrap(), x)).collect();
        let g = by["The Godfather"];
        assert_eq!((g["hr"].clone(), g["seed_hours"].clone(), g["watched"].clone(), g["waiting_watch"].clone()),
                   (json!("safe"), json!(50.0), json!(false), json!(true)));
        let p = by["Past Lives (2023)"];
        assert_eq!((p["watched"].clone(), p["waiting_watch"].clone()), (json!(true), json!(false)));
        let a = by["Anatomy of a Fall"]; // not in the ledger: qBittorrent's numbers
        assert_eq!((a["watched"].clone(), a["waiting_watch"].clone(), a["seed_hours"].clone()), (Value::Null, json!(false), json!(0.0)));
        assert_eq!(st["hr_warnings"], json!([])); // the ledger's 50 h satisfy the rule
    }

    #[test]
    fn torrents_use_the_seed_keeper() {
        let raw = [json!({"hash": H1, "name": "X.2020.1080p", "state": "stalledUP", "progress": 1, "ratio": 0.1, "seeding_time": 60})];
        let mut seeds = HashMap::new();
        seeds.insert(H1.to_string(), SeedInfo { seconds: Some(50.0 * 3600.0), watched: Some(false), waiting_watch: Some(true) });
        let t = build_torrents(&raw, None, None, None, &seeds);
        assert_eq!((t[0]["hr"].clone(), t[0]["watched"].clone(), t[0]["waiting_watch"].clone(), t[0]["seed_hours"].clone()),
                   (json!("safe"), json!(false), json!(true), json!(50.0)));
        // upper-case hashes from qBittorrent still match the lower-case ledger keys
        let up = [json!({"hash": H1.to_uppercase(), "progress": 1, "ratio": 0.1})];
        assert_eq!(build_torrents(&up, None, None, None, &seeds)[0]["waiting_watch"], json!(true));
        assert_eq!(build_torrents(&raw, None, None, None, &HashMap::new())[0]["hr"], json!("pending"));
    }

    #[tokio::test]
    async fn demo_status_shape() {
        let app = testutil::app_with(testutil::config(json!({})), crate::app::AppOptions { demo: true, ..Default::default() });
        let st = status(&app).await;
        let keys: Vec<&String> = st.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["app_name", "server_time", "dry_run", "demo", "pc", "services", "now_playing", "downloads", "waiting",
                          "library", "torrents", "hr_warnings", "power", "settings", "notifications"]);
        assert_eq!((st["demo"].clone(), st["dry_run"].clone(), st["pc"]["hostname"].clone()), (json!(true), json!(true), json!("DEMO-PC")));
        assert_eq!(st["library"][2], json!({"title": "The Godfather", "year": 1972, "radarr_id": 1, "poster": "/img/movie/1",
                                            "size_gb": 19.0, "ro_subs": true}));
        assert!(String::from_utf8(crate::json::dumps(&st["library"])).unwrap().contains("\"size_gb\": 19.0"));
        assert!(st["now_playing"][0]["position"].as_i64().unwrap() >= 2843);
        assert_eq!(st["torrents"].as_array().unwrap().len(), 3);
    }
}
