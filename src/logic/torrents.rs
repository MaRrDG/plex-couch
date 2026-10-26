//! FileList hit & run rule, torrent summaries and movie titles for torrents
//! (spec 5.5 and 5.2 "Torrent hash -> title", "release_title").
//!
//! New (plan "New behaviour" 3): the seed keeper's ledger seconds replace qBittorrent's
//! `seeding_time` for the H&R rule when it knows the torrent, and every summary carries
//! `watched` (bool|null) and `waiting_watch` (H&R done, known not watched).

use crate::logic::val;
use crate::json::py_round;
use regex::{Regex, RegexBuilder};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::OnceLock;

pub use crate::config::DEFAULT_FILELIST_REGEX;
pub const HR_RATIO: f64 = 1.0;
pub const HR_SEED_HOURS: f64 = 48.0;

/// `re.compile(pattern or DEFAULT, re.IGNORECASE)`. A pattern the regex engine rejects
/// falls back to the default.
pub fn compile_tracker_regex(pattern: Option<&str>) -> Regex {
    let p = pattern.filter(|p| !p.is_empty()).unwrap_or(DEFAULT_FILELIST_REGEX);
    RegexBuilder::new(p).case_insensitive(true).build().unwrap_or_else(|_| {
        crate::log_warning!("filelist_tracker_regex is not a valid regex, using the default");
        default_tracker_regex().clone()
    })
}

pub fn default_tracker_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| RegexBuilder::new(DEFAULT_FILELIST_REGEX).case_insensitive(true).build().unwrap())
}

/// True if a tracker URL belongs to FileList. Never log/return the URL itself.
pub fn is_filelist_tracker(url: Option<&str>, regex: &Regex) -> bool {
    url.is_some_and(|u| !u.is_empty() && regex.is_match(u))
}

/// Safe once ratio >= 1.0 OR seeded >= 48 h.
pub fn is_hr_safe(ratio: f64, seeding_s: f64) -> bool {
    ratio >= HR_RATIO || seeding_s >= HR_SEED_HOURS * 3600.0
}

/// Progress towards the H&R rule, 0..1 (whichever condition is closer).
pub fn rule_progress(ratio: f64, seeding_s: f64) -> f64 {
    let hours = (seeding_s / 3600.0).max(0.0);
    (ratio.max(0.0) / HR_RATIO).max(hours / HR_SEED_HOURS).min(1.0)
}

pub fn is_torrent_complete(progress: f64) -> bool {
    progress >= 1.0
}

/// `downloading` | `safe` | `pending` (completed but not safe yet).
pub fn hr_status(progress: f64, ratio: f64, seeding_s: f64) -> &'static str {
    if !is_torrent_complete(progress) {
        "downloading"
    } else if is_hr_safe(ratio, seeding_s) {
        "safe"
    } else {
        "pending"
    }
}

/// e.g. `mai are nevoie de 35 h sau ratio 1`.
pub fn hr_remaining_text(ratio: f64, seeding_s: f64) -> String {
    if is_hr_safe(ratio, seeding_s) {
        return "regula îndeplinită".into();
    }
    let left = (HR_SEED_HOURS - seeding_s / 3600.0).ceil().max(1.0) as i64;
    format!("mai are nevoie de {left} h sau ratio 1")
}

pub fn qbit_state_ro(state: &str) -> String {
    let ro = match state {
        "downloading" | "forcedDL" | "metaDL" | "forcedMetaDL" => "Se descarcă",
        "stalledDL" => "Blocat (descărcare)",
        "queuedDL" => "În coadă",
        "pausedDL" => "Pauză",
        "stoppedDL" => "Oprit",
        "checkingDL" | "checkingUP" | "checkingResumeData" => "Se verifică",
        "uploading" | "forcedUP" => "Seed activ",
        "stalledUP" => "Seed (fără cereri)",
        "queuedUP" => "Seed în coadă",
        "pausedUP" | "stoppedUP" => "Seed oprit",
        "moving" => "Se mută",
        "missingFiles" => "Fișiere lipsă",
        "error" => "Eroare",
        "" => "necunoscut",
        other => other,
    };
    ro.to_string()
}

/// What the seed keeper knows about one torrent.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SeedInfo {
    /// Seconds seeded according to the ledger (None: not in the ledger).
    pub seconds: Option<f64>,
    /// Watched in Plex (None: unknown / not mapped).
    pub watched: Option<bool>,
    /// The keeper's own "H&R done, not watched yet" (None: decide from `watched`).
    pub waiting_watch: Option<bool>,
}

/// A qBittorrent `/torrents/info` entry in the API shape (no tracker URL, no hash).
/// `title`: the movie title (see [`torrent_title`]), else parsed from the name.
pub fn summarize_torrent(t: &Value, title: Option<&str>, seed: Option<&SeedInfo>) -> Value {
    let progress = val::f(t.get("progress"));
    let ratio = val::f(t.get("ratio"));
    let qb_seeding = val::i(t.get("seeding_time"));
    let seeding = seed.and_then(|s| s.seconds).map(|s| s as i64).unwrap_or(qb_seeding) as f64;
    let status = hr_status(progress, ratio, seeding);
    let name = val::s(t, "name");
    let state = val::s(t, "state");
    let title = match title.filter(|x| !x.is_empty()) {
        Some(x) => x.to_string(),
        None => release_title(t.get("name").and_then(Value::as_str)),
    };
    let watched = seed.and_then(|s| s.watched);
    let waiting = status == "safe" && seed.and_then(|s| s.waiting_watch).unwrap_or(watched == Some(false));
    json!({
        "title": title,
        "name": name,
        "state": state,
        "state_ro": qbit_state_ro(&state),
        "progress": py_round(progress * 100.0, 1),
        "ratio": py_round(ratio, 2),
        "seed_hours": py_round(seeding / 3600.0, 1),
        "hr": status,
        "safe": status == "safe",
        "rule_progress": py_round(rule_progress(ratio, seeding), 3),
        "remaining": if status == "pending" { hr_remaining_text(ratio, seeding) } else { String::new() },
        "dlspeed": val::i(t.get("dlspeed")),
        "upspeed": val::i(t.get("upspeed")),
        "eta": val::i(t.get("eta")),
        "size": val::i(t.get("size")),
        "watched": watched,
        "waiting_watch": waiting,
    })
}

/// Pending torrents as `{title, name, remaining}`.
pub fn hr_warnings(summaries: &[Value]) -> Vec<Value> {
    summaries
        .iter()
        .filter(|t| t.get("hr").and_then(Value::as_str) == Some("pending"))
        .map(|t| {
            let name = val::s(t, "name");
            let title = match val::get_truthy(t, "title") {
                Some(x) => crate::json::py_str(x),
                None => release_title(Some(&name)),
            };
            json!({"title": title, "name": name, "remaining": val::raw(t, "remaining")})
        })
        .collect()
}

/// Sort key of the seeding list: pending, downloading, safe; then the title lowercased.
pub fn sort_torrents(list: &mut [Value]) {
    let rank = |t: &Value| match t.get("hr").and_then(Value::as_str) {
        Some("pending") => 0,
        Some("downloading") => 1,
        _ => 2,
    };
    list.sort_by(|a, b| {
        let ka = (rank(a), val::s(a, "title").to_lowercase());
        let kb = (rank(b), val::s(b, "title").to_lowercase());
        ka.cmp(&kb)
    });
}

// ---------------------------------------------------------------------------
// Movie titles for torrents (the UI never shows release file names)
// ---------------------------------------------------------------------------

fn re(p: &str) -> Regex {
    Regex::new(p).expect("static regex")
}

fn video_ext_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"(?i)\.(mkv|mp4|avi|m4v|mov|wmv|ts|m2ts|iso)$"))
}

fn lead_tags_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"^(\s*\[[^\]]*\]\s*[-_.]?\s*)+"))
}

fn sep_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"[._]+"))
}

fn year_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"^[(\[]?((?:19|20)\d\d)[)\]]?$"))
}

fn tag_prefix_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| re(r"(?i)^(ddp\d|dd\d|aac\d|ac3\d|dts-|x26[45]|h26[45]|\d{3,4}[pi]$)"))
}

const RELEASE_TAGS: &[&str] = &[
    "2160p", "1080p", "1080i", "720p", "576p", "480p", "4k", "uhd", "fhd", "web", "webdl", "web-dl", "webrip",
    "web-rip", "hdtv", "bluray", "blu-ray", "bdrip", "brrip", "bdremux", "remux", "dvdrip", "hdrip", "hdr",
    "hdr10", "hdr10+", "hdr10plus", "dovi", "x264", "x265", "h264", "h265", "hevc", "avc", "av1", "10bit",
    "8bit", "repack", "proper", "rerip", "extended", "remastered", "unrated", "uncut", "imax", "multi", "subbed",
    "dubbed", "aac", "ac3", "dts", "dts-hd", "truehd", "atmos", "ddp", "eac3", "flac", "amzn", "dsnp", "hmax",
    "atvp",
];

fn is_release_tag(tok: &str) -> bool {
    let lower = tok.to_lowercase();
    let t = lower.trim_matches(|c| matches!(c, '[' | ']' | '(' | ')'));
    RELEASE_TAGS.contains(&t) || tag_prefix_re().is_match(t)
}

/// A clean `Title (Year)` from a release or file name
/// (`The.Godfather.1972.2160p.WEB.H265-PETRiFiED.mkv` -> `The Godfather (1972)`).
pub fn release_title(name: Option<&str>) -> String {
    let original = name.unwrap_or("").trim();
    if original.is_empty() {
        return String::new();
    }
    let s = video_ext_re().replace(original, "").into_owned();
    let mut s = lead_tags_re().replace(&s, "").into_owned();
    if !s.trim().contains(' ') || s.matches('.').count() >= 3 || s.contains('_') {
        s = sep_re().replace_all(&s, " ").into_owned();
    }
    let s = s.replace(['[', ']'], " ");
    let tokens: Vec<&str> = s.split_whitespace().collect();
    if tokens.is_empty() {
        return original.to_string();
    }
    let years: Vec<usize> = (1..tokens.len()).filter(|&i| year_re().is_match(tokens[i])).collect();
    let strong: Vec<usize> =
        years.iter().copied().filter(|&i| i + 1 == tokens.len() || is_release_tag(tokens[i + 1])).collect();
    let year_idx = strong.last().or(years.last()).copied();
    let end = year_idx.unwrap_or_else(|| (1..tokens.len()).find(|&i| is_release_tag(tokens[i])).unwrap_or(tokens.len()));
    let joined = tokens[..end].join(" ");
    let title = joined.trim_matches(|c| matches!(c, ' ' | '-' | '–' | '_' | '.' | '('));
    if title.is_empty() {
        return original.to_string();
    }
    match year_idx {
        Some(i) => {
            let y = year_re().captures(tokens[i]).and_then(|c| c.get(1)).map(|m| m.as_str()).unwrap_or("");
            format!("{title} ({y})")
        }
        None => title.to_string(),
    }
}

/// Torrent hash (upper case) -> movie title from Radarr's grabbed history, then its queue.
/// History wins; ids without a known movie title are skipped.
pub fn torrent_title_map(history: Option<&[Value]>, queue: Option<&[Value]>, movies: Option<&[Value]>) -> HashMap<String, String> {
    let mut by_id: HashMap<String, &Value> = HashMap::new();
    for m in movies.unwrap_or(&[]) {
        if let Some(id) = m.get("id").filter(|v| !v.is_null()) {
            by_id.insert(val::key(id), m);
        }
    }
    let mut out = HashMap::new();
    for records in [history.unwrap_or(&[]), queue.unwrap_or(&[])] {
        for rec in records {
            let dl = val::s(rec, "downloadId").trim().to_uppercase();
            let mid = rec.get("movieId").map(val::key).unwrap_or_else(|| "null".into());
            let movie = by_id
                .get(&mid)
                .copied()
                .filter(|m| crate::json::truthy(Some(m)))
                .or_else(|| val::get_truthy(rec, "movie"));
            let title = movie.map(|m| val::s(m, "title")).unwrap_or_default();
            if !dl.is_empty() && !title.is_empty() && !out.contains_key(&dl) {
                out.insert(dl, title);
            }
        }
    }
    out
}

/// The movie title for a raw qBittorrent torrent: Radarr first, then the parsed name.
pub fn torrent_title(t: &Value, map: Option<&HashMap<String, String>>) -> String {
    let h = val::s(t, "hash").to_uppercase();
    match map.and_then(|m| m.get(&h)).filter(|x| !x.is_empty()) {
        Some(x) => x.clone(),
        None => release_title(t.get("name").and_then(Value::as_str)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: f64 = 3600.0;

    fn summ(t: Value) -> Value {
        summarize_torrent(&t, None, None)
    }

    // py: test_logic.TestHitAndRun.test_safe_by_ratio
    #[test]
    fn safe_by_ratio() {
        assert!(is_hr_safe(1.0, 0.0));
        assert!(is_hr_safe(2.5, 10.0));
    }

    // py: test_logic.TestHitAndRun.test_safe_by_seed_time
    #[test]
    fn safe_by_seed_time() {
        assert!(is_hr_safe(0.0, 48.0 * H));
        assert!(!is_hr_safe(0.99, 48.0 * H - 1.0));
    }

    // py: test_logic.TestHitAndRun.test_status
    #[test]
    fn status() {
        assert_eq!(hr_status(0.5, 0.0, 0.0), "downloading");
        assert_eq!(hr_status(1.0, 0.2, 13.0 * H), "pending");
        assert_eq!(hr_status(1.0, 1.0, 0.0), "safe");
    }

    // py: test_logic.TestHitAndRun.test_rule_progress
    #[test]
    fn rule_progress_cases() {
        assert!((rule_progress(0.5, 0.0) - 0.5).abs() < 1e-9);
        assert!((rule_progress(0.1, 24.0 * H) - 0.5).abs() < 1e-9);
        assert!((rule_progress(0.75, 12.0 * H) - 0.75).abs() < 1e-9);
        assert_eq!(rule_progress(3.0, 0.0), 1.0);
        assert_eq!(rule_progress(0.0, 100.0 * H), 1.0);
        // (None, None): missing values read as 0
        assert_eq!(rule_progress(val::f(None), val::f(None)), 0.0);
        assert_eq!(rule_progress(-1.0, -5.0), 0.0);
    }

    // py: test_logic.TestHitAndRun.test_remaining_text
    #[test]
    fn remaining_text() {
        assert_eq!(hr_remaining_text(0.1, 13.0 * H), "mai are nevoie de 35 h sau ratio 1");
        assert_eq!(hr_remaining_text(0.1, 13.0 * H + 1.0), "mai are nevoie de 35 h sau ratio 1");
        assert_eq!(hr_remaining_text(0.0, 48.0 * H - 10.0), "mai are nevoie de 1 h sau ratio 1");
        assert_eq!(hr_remaining_text(1.0, 0.0), "regula îndeplinită");
    }

    // py: test_logic.TestHitAndRun.test_summarize_and_warnings
    #[test]
    fn summarize_and_warnings() {
        let ts = [
            json!({"name": "A", "state": "stalledUP", "progress": 1, "ratio": 0.0, "seeding_time": 903}),
            json!({"name": "B", "state": "uploading", "progress": 1, "ratio": 1.2, "seeding_time": 60}),
            json!({"name": "C", "state": "downloading", "progress": 0.4, "ratio": 0.0, "seeding_time": 0}),
        ];
        let s: Vec<Value> = ts.into_iter().map(summ).collect();
        let hrs: Vec<&str> = s.iter().map(|x| x["hr"].as_str().unwrap()).collect();
        assert_eq!(hrs, ["pending", "safe", "downloading"]);
        assert_eq!(s[1]["safe"], json!(true));
        assert_eq!(s[0]["remaining"], json!("mai are nevoie de 48 h sau ratio 1"));
        assert_eq!(hr_warnings(&s), vec![json!({"title": "A", "name": "A", "remaining": "mai are nevoie de 48 h sau ratio 1"})]);
        assert!(s[0].get("tracker").is_none());
        assert!(s[0].get("hash").is_none());
        // the field types: floats stay floats, ints stay ints
        let enc = String::from_utf8(crate::json::dumps(&s[2])).unwrap();
        assert!(enc.contains("\"progress\": 40.0") && enc.contains("\"ratio\": 0.0") && enc.contains("\"seed_hours\": 0.0"), "{enc}");
        assert!(enc.contains("\"dlspeed\": 0") && enc.contains("\"rule_progress\": 0.0"), "{enc}");
        assert_eq!(s[0]["state_ro"], json!("Seed (fără cereri)"));
    }

    // py: test_logic.TestHitAndRun.test_title_given_or_parsed
    #[test]
    fn title_given_or_parsed() {
        let raw = json!({"name": "The.Godfather.1972.2160p.WEB.H265-PETRiFiED.mkv", "progress": 1, "ratio": 0.1});
        assert_eq!(summarize_torrent(&raw, None, None)["title"], json!("The Godfather (1972)"));
        let t = summarize_torrent(&raw, Some("The Godfather"), None);
        assert_eq!(t["title"], json!("The Godfather"));
        assert_eq!(hr_warnings(&[t])[0]["title"], json!("The Godfather"));
    }

    const CASES: [(&str, &str); 12] = [
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
    ];

    // py: test_logic.TestReleaseTitle.test_shapes
    #[test]
    fn release_title_shapes() {
        for (name, want) in CASES {
            assert_eq!(release_title(Some(name)), want, "{name}");
        }
    }

    // py: test_logic.TestReleaseTitle.test_empty
    #[test]
    fn release_title_empty() {
        assert_eq!(release_title(Some("")), "");
        assert_eq!(release_title(None), "");
    }

    const H1: &str = "85b755cd2d0a8a7a0e15348b9fdcbff2f1580a55";

    fn h2() -> String {
        "c3".repeat(20)
    }

    fn movies() -> Vec<Value> {
        vec![json!({"id": 2, "title": "The Godfather"}), json!({"id": 3, "title": "Anatomy of a Fall"})]
    }

    // py: test_logic.TestTorrentTitles.test_history_then_queue_case_insensitive
    #[test]
    fn history_then_queue_case_insensitive() {
        let history = [json!({"downloadId": H1.to_uppercase(), "movieId": 2, "eventType": "grabbed"})];
        let queue = [json!({"downloadId": h2().to_uppercase(), "movieId": 3})];
        let m = torrent_title_map(Some(&history), Some(&queue), Some(&movies()));
        let want: HashMap<String, String> = [
            (H1.to_uppercase(), "The Godfather".to_string()),
            (h2().to_uppercase(), "Anatomy of a Fall".to_string()),
        ]
        .into_iter()
        .collect();
        assert_eq!(m, want);
        let t = json!({"hash": H1, "name": "The.Godfather.1972.2160p.WEB.H265-PETRiFiED.mkv"});
        assert_eq!(torrent_title(&t, Some(&m)), "The Godfather");
        let mixed = torrent_title_map(Some(&[json!({"downloadId": H1.to_lowercase(), "movieId": 2})]), Some(&[]), Some(&movies()));
        assert_eq!(torrent_title(&t, Some(&mixed)), "The Godfather");
    }

    // py: test_logic.TestTorrentTitles.test_history_wins_and_unknown_movie_skipped
    #[test]
    fn history_wins_and_unknown_movie_skipped() {
        let history = [json!({"downloadId": H1, "movieId": 3}), json!({"downloadId": "FFFF", "movieId": 99})];
        let queue = [json!({"downloadId": H1, "movieId": 2})];
        let m = torrent_title_map(Some(&history), Some(&queue), Some(&movies()));
        let want: HashMap<String, String> = [(H1.to_uppercase(), "Anatomy of a Fall".to_string())].into_iter().collect();
        assert_eq!(m, want);
    }

    // py: test_logic.TestTorrentTitles.test_queue_movie_object_and_fallback
    #[test]
    fn queue_movie_object_and_fallback() {
        let queue = [json!({"downloadId": h2(), "movieId": 7, "movie": {"title": "Past Lives"}})];
        let m = torrent_title_map(None, Some(&queue), None);
        assert_eq!(torrent_title(&json!({"hash": h2()}), Some(&m)), "Past Lives");
        let t = json!({"hash": "ab".repeat(20), "name": "Past.Lives.2023.2160p.WEB-DL-GROUP"});
        assert_eq!(torrent_title(&t, Some(&m)), "Past Lives (2023)");
        assert_eq!(torrent_title(&t, None), "Past Lives (2023)");
    }

    // py: test_logic.TestTrackerMatch.test_filelist_hosts
    #[test]
    fn filelist_hosts() {
        let rx = compile_tracker_regex(None);
        for url in ["http://reactor.thefl.org/abc/announce", "https://filelist.io/announce", "http://tracker.filelist.ro/x",
                    "http://flro.org/a", "HTTP://REACTOR.THEFL.ORG/x"] {
            assert!(is_filelist_tracker(Some(url), &rx), "{url}");
        }
    }

    // py: test_logic.TestTrackerMatch.test_other_hosts
    #[test]
    fn other_hosts() {
        let rx = compile_tracker_regex(None);
        for url in [Some(""), None, Some("udp://tracker.opentrackr.org:1337/announce"), Some("http://example.org/thefl"),
                    Some("https://filelist.com/announce")] {
            assert!(!is_filelist_tracker(url, &rx), "{url:?}");
        }
    }

    // py: test_logic.TestTrackerMatch.test_custom_regex
    #[test]
    fn custom_regex() {
        let rx = compile_tracker_regex(Some(r"example\.net"));
        assert!(is_filelist_tracker(Some("http://t.example.net/a"), &rx));
        assert!(!is_filelist_tracker(Some("http://reactor.thefl.org/a"), &rx));
        // an invalid pattern falls back to the default
        let bad = compile_tracker_regex(Some("(unclosed"));
        assert!(is_filelist_tracker(Some("http://reactor.thefl.org/a"), &bad));
    }

    // New: the seed keeper's ledger drives the H&R rule; watched / waiting_watch.
    #[test]
    fn seeding_fields_from_the_ledger() {
        let t = json!({"name": "X", "state": "stalledUP", "progress": 1, "ratio": 0.2, "seeding_time": 3600});
        let plain = summarize_torrent(&t, None, None);
        assert_eq!((plain["hr"].clone(), plain["watched"].clone(), plain["waiting_watch"].clone()),
                   (json!("pending"), Value::Null, json!(false)));
        assert_eq!(plain["seed_hours"], json!(1.0));
        // ledger: 49 h seeded although qBittorrent says 1 h -> safe; not watched -> waiting
        let led = SeedInfo { seconds: Some(49.0 * H), watched: Some(false), waiting_watch: None };
        let s = summarize_torrent(&t, None, Some(&led));
        assert_eq!((s["hr"].clone(), s["safe"].clone(), s["waiting_watch"].clone(), s["watched"].clone()),
                   (json!("safe"), json!(true), json!(true), json!(false)));
        assert_eq!((s["seed_hours"].clone(), s["rule_progress"].clone(), s["remaining"].clone()), (json!(49.0), json!(1.0), json!("")));
        // watched -> not waiting
        let s = summarize_torrent(&t, None, Some(&SeedInfo { seconds: Some(49.0 * H), watched: Some(true), waiting_watch: None }));
        assert_eq!((s["watched"].clone(), s["waiting_watch"].clone()), (json!(true), json!(false)));
        // ledger below qBittorrent's figure is still the truth (qB loses it on restart)
        let s = summarize_torrent(&t, None, Some(&SeedInfo { seconds: Some(1800.0), watched: None, waiting_watch: None }));
        assert_eq!((s["seed_hours"].clone(), s["remaining"].clone()), (json!(0.5), json!("mai are nevoie de 48 h sau ratio 1")));
        // no ledger entry: qBittorrent's seeding_time; unknown watched never waits
        let s = summarize_torrent(&t, None, Some(&SeedInfo { seconds: None, watched: None, waiting_watch: None }));
        assert_eq!((s["seed_hours"].clone(), s["waiting_watch"].clone()), (json!(1.0), json!(false)));
        // a downloading torrent never waits, even when not watched
        let d = json!({"name": "Y", "progress": 0.5, "ratio": 5});
        let s = summarize_torrent(&d, None, Some(&SeedInfo { seconds: Some(99.0 * H), watched: Some(false), waiting_watch: Some(true) }));
        assert_eq!((s["hr"].clone(), s["waiting_watch"].clone()), (json!("downloading"), json!(false)));
        // the keeper's flag wins (its H&R margin is 49 h): 48.5 h is safe but not yet waiting
        let k = SeedInfo { seconds: Some(48.5 * H), watched: Some(false), waiting_watch: Some(false) };
        let s2 = summarize_torrent(&t, None, Some(&k));
        assert_eq!((s2["hr"].clone(), s2["waiting_watch"].clone()), (json!("safe"), json!(false)));
        let k = SeedInfo { seconds: Some(50.0 * H), watched: Some(false), waiting_watch: Some(true) };
        assert_eq!(summarize_torrent(&t, None, Some(&k))["waiting_watch"], json!(true));
        let keys: Vec<&String> = s.as_object().unwrap().keys().collect();
        assert_eq!(keys.last().map(|k| k.as_str()), Some("waiting_watch"));
    }

    #[test]
    fn sort_order() {
        let mut l = vec![
            json!({"hr": "safe", "title": "a"}),
            json!({"hr": "downloading", "title": "B"}),
            json!({"hr": "pending", "title": "z"}),
            json!({"hr": "pending", "title": "C"}),
        ];
        sort_torrents(&mut l);
        let t: Vec<&str> = l.iter().map(|x| x["title"].as_str().unwrap()).collect();
        assert_eq!(t, ["C", "z", "B", "a"]);
    }
}
