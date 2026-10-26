//! Radarr and Bazarr summaries in Romanian (spec 5.2, 5.3): downloads,
//! waiting, library, release dates, the RO-subtitle map.
//!
//! Plan fix 17: a bad `timeleft` day part makes that item's time unknown (`""`, like a bad
//! h/m/s part) instead of aborting `/api/status`.

use crate::json::{py_int, py_round};
use crate::logic::images::poster_url;
use crate::logic::val;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

pub const RO_MONTHS: [&str; 12] = ["ian", "feb", "mar", "apr", "mai", "iun", "iul", "aug", "sep", "oct", "nov", "dec"];

/// A parsed ISO date-time: its own calendar date (for the text) and the UTC timestamp.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IsoDate {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub ts: f64,
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

fn digits(s: &str, n: usize) -> Option<u32> {
    (s.len() == n && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten()
}

/// `datetime.fromisoformat(s.replace("Z", "+00:00"))`, naive read as UTC; None if bad.
/// Accepts `YYYY-MM-DD`, then optionally `T` or a space, `HH[:MM[:SS[.f+]]]`, and `Z`,
/// `±HH[:MM]` or `±HHMM`.
pub fn parse_iso(s: Option<&Value>) -> Option<IsoDate> {
    let s = s?.as_str()?;
    if s.is_empty() {
        return None;
    }
    let s = s.replace('Z', "+00:00");
    if s.len() < 10 || !s.is_char_boundary(10) {
        return None;
    }
    let (date, rest) = s.split_at(10);
    let b = date.as_bytes();
    if b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let year = digits(&date[0..4], 4)? as i64;
    let month = digits(&date[5..7], 2)?;
    let day = digits(&date[8..10], 2)?;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) || year == 0 {
        return None;
    }
    let mut secs: f64 = 0.0;
    let mut offset: i64 = 0;
    if !rest.is_empty() {
        let mut chars = rest.chars();
        let sep = chars.next()?;
        if sep != 'T' && sep != ' ' {
            return None;
        }
        let time = chars.as_str();
        let tz_at = time.find(['+', '-']);
        let (clock, tz) = match tz_at {
            Some(i) => (&time[..i], Some(&time[i..])),
            None => (time, None),
        };
        let (main, frac) = match clock.split_once(['.', ',']) {
            Some((m, f)) => (m, Some(f)),
            None => (clock, None),
        };
        let parts: Vec<&str> = main.split(':').collect();
        if parts.is_empty() || parts.len() > 3 {
            return None;
        }
        let h = digits(parts[0], 2)?;
        let mi = if parts.len() > 1 { digits(parts[1], 2)? } else { 0 };
        let se = if parts.len() > 2 { digits(parts[2], 2)? } else { 0 };
        if h > 23 || mi > 59 || se > 59 {
            return None;
        }
        secs = (h * 3600 + mi * 60 + se) as f64;
        if let Some(f) = frac {
            if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) || parts.len() != 3 {
                return None;
            }
            secs += format!("0.{f}").parse::<f64>().ok()?;
        }
        if let Some(tz) = tz {
            let sign = if tz.starts_with('-') { -1 } else { 1 };
            let body = tz[1..].replace(':', "");
            let (oh, om) = match body.len() {
                2 => (digits(&body, 2)?, 0),
                4 => (digits(&body[..2], 2)?, digits(&body[2..], 2)?),
                _ => return None,
            };
            if oh > 23 || om > 59 {
                return None;
            }
            offset = sign * (oh as i64 * 3600 + om as i64 * 60);
        }
    }
    let ts = (days_from_civil(year, month, day) * 86400) as f64 + secs - offset as f64;
    Some(IsoDate { year, month, day, ts })
}

/// `17 nov 2026`.
pub fn format_ro_date(d: &IsoDate) -> String {
    format!("{} {} {}", d.day, RO_MONTHS[(d.month - 1) as usize], d.year)
}

/// Romanian status of a monitored movie without a file.
pub fn waiting_status(movie: &Value, now: f64, downloading: bool) -> String {
    if downloading {
        return "Se descarcă acum".into();
    }
    if !crate::json::truthy(movie.get("isAvailable")) {
        let digital = parse_iso(movie.get("digitalRelease"));
        let physical = parse_iso(movie.get("physicalRelease"));
        if let Some(d) = digital.filter(|d| d.ts > now) {
            return format!("Așteaptă lansarea digitală ({})", format_ro_date(&d));
        }
        if let Some(p) = physical.filter(|p| p.ts > now) {
            return format!("Așteaptă lansarea pe disc ({})", format_ro_date(&p));
        }
        let status = movie.get("status").and_then(Value::as_str);
        if digital.is_none() && physical.is_none() && matches!(status, Some("announced") | Some("tba")) {
            return "Anunțat, fără dată de lansare".into();
        }
    }
    "Se caută în 4K pe FileList".into()
}

fn waiting_sort_ts(movie: &Value) -> f64 {
    parse_iso(movie.get("digitalRelease"))
        .or_else(|| parse_iso(movie.get("physicalRelease")))
        .map(|d| d.ts)
        .unwrap_or(f64::INFINITY)
}

/// Radarr `timeleft` `d.hh:mm:ss` or `hh:mm:ss` -> seconds; None if missing or bad.
pub fn parse_timeleft(s: Option<&Value>) -> Option<i64> {
    let s = s?.as_str()?;
    if s.is_empty() {
        return None;
    }
    let mut days = 0;
    let mut rest = s;
    if s.split(':').next().unwrap_or("").contains('.') {
        let (d, r) = s.split_once('.')?;
        days = py_int(&Value::String(d.to_string()))?; // plan fix 17: a bad day part is None
        rest = r;
    }
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.len() < 3 {
        return None;
    }
    let h = py_int(&Value::String(parts[0].to_string()))?;
    let m = py_int(&Value::String(parts[1].to_string()))?;
    let sec = parts[2].trim().parse::<f64>().ok().filter(|f| f.is_finite())?.trunc() as i64;
    Some(days * 86400 + h * 3600 + m * 60 + sec)
}

pub fn format_duration_ro(seconds: Option<i64>) -> String {
    let Some(s) = seconds else { return String::new() };
    if s < 60 {
        return "sub 1 min".into();
    }
    let (d, rem) = (s.div_euclid(86400), s.rem_euclid(86400));
    let (h, rem) = (rem / 3600, rem % 3600);
    let m = rem / 60;
    if d != 0 {
        format!("{d}z {h}h")
    } else if h != 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m} min")
    }
}

pub fn queue_status_ro(state: &str) -> String {
    match state {
        "downloading" => "Se descarcă",
        "paused" => "Pauză",
        "queued" => "În coadă",
        "completed" => "Terminat",
        "delay" => "Amânat",
        "downloadClientUnavailable" => "Client indisponibil",
        "warning" => "Avertisment",
        "failed" | "failedPending" => "Eșuat",
        "importPending" => "Așteaptă importul",
        "importing" => "Se importă",
        "imported" => "Importat",
        other => other,
    }
    .to_string()
}

fn poster_of(kind: &str, id: Option<&Value>) -> Value {
    poster_url(kind, &val::ident(id)).map(Value::from).unwrap_or(Value::Null)
}

/// One Radarr queue record for `downloads`.
pub fn summarize_queue_record(rec: &Value) -> Value {
    let size = val::f(rec.get("size"));
    let left = val::f(rec.get("sizeleft"));
    let pct = if size > 0.0 { py_round((size - left) / size * 100.0, 1) } else { 0.0 };
    let status = val::s(rec, "status");
    let mut state = match val::get_truthy(rec, "trackedDownloadState") {
        Some(v) => crate::json::py_str(v),
        None => status.clone(),
    };
    if state == "downloading" && matches!(status.as_str(), "paused" | "queued" | "warning" | "failed") {
        state = status;
    }
    let movie = val::obj(rec, "movie");
    let title = match val::get_truthy(movie, "title").or_else(|| val::get_truthy(rec, "title")) {
        Some(t) => crate::json::py_str(t),
        None => String::new(),
    };
    json!({
        "title": title,
        "year": val::raw(movie, "year"),
        "percent": pct.clamp(0.0, 100.0),
        "time_left": format_duration_ro(parse_timeleft(rec.get("timeleft"))),
        "status": queue_status_ro(&state),
        "movie_id": val::raw(rec, "movieId"),
        "radarr_id": val::raw(rec, "movieId"),
        "poster": poster_of("movie", rec.get("movieId")),
    })
}

/// radarrId -> has a Romanian subtitle that is not listed as missing.
pub type RoMap = HashMap<String, bool>;

pub fn bazarr_ro_map(movies: &[Value]) -> RoMap {
    let mut out = HashMap::new();
    let ro = |list: &[Value]| list.iter().any(|s| s.get("code2").and_then(Value::as_str) == Some("ro"));
    for m in movies {
        let Some(rid) = m.get("radarrId").filter(|v| !v.is_null()) else { continue };
        let has = ro(val::arr(m, "subtitles"));
        let missing = ro(val::arr(m, "missing_subtitles"));
        out.insert(val::key(rid), has && !missing);
    }
    out
}

/// `ro_map.get(id)` (None when Bazarr does not know the movie).
pub fn ro_of(ro_map: &RoMap, id: Option<&Value>) -> Option<bool> {
    ro_map.get(&val::key(id.unwrap_or(&Value::Null))).copied()
}

/// Movies with a file, sorted by title. `ro_map` None (Bazarr down) -> `ro_subs` null.
pub fn summarize_library(movies: &[Value], ro_map: Option<&RoMap>) -> Vec<Value> {
    let mut out: Vec<Value> = movies
        .iter()
        .filter(|m| crate::json::truthy(m.get("hasFile")))
        .map(|m| {
            let size = val::get_truthy(val::obj(m, "movieFile"), "size")
                .or_else(|| val::get_truthy(m, "sizeOnDisk"))
                .map(|v| val::f(Some(v)))
                .unwrap_or(0.0);
            let ro = ro_map.and_then(|r| ro_of(r, m.get("id")));
            json!({
                "title": val::s(m, "title"),
                "year": val::raw(m, "year"),
                "size_gb": py_round(size / 1024f64.powi(3), 1),
                "ro_subs": ro,
                "radarr_id": val::raw(m, "id"),
                "poster": poster_of("movie", m.get("id")),
            })
        })
        .collect();
    out.sort_by_key(|x| val::s(x, "title").to_lowercase());
    out
}

/// Queue movie ids as dict keys.
pub fn queue_movie_ids(queue: &[Value]) -> HashSet<String> {
    queue.iter().map(|r| val::key(r.get("movieId").unwrap_or(&Value::Null))).collect()
}

/// Monitored movies without a file, by release date then title.
pub fn summarize_waiting(movies: &[Value], queue_ids: &HashSet<String>, now: f64) -> Vec<Value> {
    let mut items: Vec<&Value> = movies
        .iter()
        .filter(|m| crate::json::truthy(m.get("monitored")) && !crate::json::truthy(m.get("hasFile")))
        .collect();
    items.sort_by(|a, b| {
        let (ta, tb) = (waiting_sort_ts(a), waiting_sort_ts(b));
        ta.partial_cmp(&tb)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| val::s(a, "title").to_lowercase().cmp(&val::s(b, "title").to_lowercase()))
    });
    items
        .into_iter()
        .map(|m| {
            let downloading = queue_ids.contains(&val::key(m.get("id").unwrap_or(&Value::Null)));
            json!({
                "title": val::s(m, "title"),
                "year": val::raw(m, "year"),
                "status": waiting_status(m, now, downloading),
                "downloading": downloading,
                "radarr_id": val::raw(m, "id"),
                "poster": poster_of("movie", m.get("id")),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-10T00:00:00Z
    fn now() -> f64 {
        (days_from_civil(2026, 10, 10) * 86400) as f64
    }

    // py: test_logic.TestWaitingStatus.test_waiting_digital
    #[test]
    fn waiting_digital() {
        let m = json!({"isAvailable": false, "digitalRelease": "2026-11-17T00:00:00Z",
                       "physicalRelease": "2026-11-17T00:00:00Z", "status": "inCinemas"});
        assert_eq!(waiting_status(&m, now(), false), "Așteaptă lansarea digitală (17 nov 2026)");
    }

    // py: test_logic.TestWaitingStatus.test_waiting_physical_only
    #[test]
    fn waiting_physical_only() {
        let m = json!({"isAvailable": false, "digitalRelease": "2026-01-01T00:00:00Z",
                       "physicalRelease": "2027-03-02T00:00:00Z"});
        assert_eq!(waiting_status(&m, now(), false), "Așteaptă lansarea pe disc (2 mar 2027)");
    }

    // py: test_logic.TestWaitingStatus.test_searching
    #[test]
    fn searching() {
        let m = json!({"isAvailable": true, "digitalRelease": "2020-01-01T00:00:00Z"});
        assert_eq!(waiting_status(&m, now(), false), "Se caută în 4K pe FileList");
        let m2 = json!({"isAvailable": false, "digitalRelease": "2026-01-01T00:00:00Z"});
        assert_eq!(waiting_status(&m2, now(), false), "Se caută în 4K pe FileList");
    }

    // py: test_logic.TestWaitingStatus.test_announced_and_downloading
    #[test]
    fn announced_and_downloading() {
        assert_eq!(waiting_status(&json!({"isAvailable": false, "status": "announced"}), now(), false),
                   "Anunțat, fără dată de lansare");
        assert_eq!(waiting_status(&json!({"isAvailable": true}), now(), true), "Se descarcă acum");
    }

    // py: test_logic.TestWaitingStatus.test_summaries
    #[test]
    fn summaries() {
        let movies = vec![
            json!({"id": 1, "title": "The Odyssey", "year": 2026, "monitored": true, "hasFile": false,
                   "isAvailable": false, "digitalRelease": "2026-11-17T00:00:00Z"}),
            json!({"id": 2, "title": "The Godfather", "year": 1972, "monitored": true, "hasFile": true,
                   "movieFile": {"size": 20419862888u64}}),
            json!({"id": 3, "title": "Unmonitored", "monitored": false, "hasFile": false}),
        ];
        let w = summarize_waiting(&movies, &HashSet::new(), now());
        let titles: Vec<&str> = w.iter().map(|x| x["title"].as_str().unwrap()).collect();
        assert_eq!(titles, ["The Odyssey"]);
        assert_eq!((w[0]["radarr_id"].clone(), w[0]["poster"].clone(), w[0]["downloading"].clone()),
                   (json!(1), json!("/img/movie/1"), json!(false)));
        let q = queue_movie_ids(&[json!({"movieId": 1})]);
        assert_eq!(summarize_waiting(&movies, &q, now())[0]["downloading"], json!(true));
        let ro = bazarr_ro_map(&[json!({"radarrId": 2, "subtitles": [{"code2": "ro", "path": "x"}], "missing_subtitles": []})]);
        let lib = summarize_library(&movies, Some(&ro));
        assert_eq!(lib, vec![json!({"title": "The Godfather", "year": 1972, "size_gb": 19.0, "ro_subs": true,
                                     "radarr_id": 2, "poster": "/img/movie/2"})]);
        assert!(String::from_utf8(crate::json::dumps(&lib[0])).unwrap().contains("\"size_gb\": 19.0"));
        assert_eq!(summarize_library(&movies, None)[0]["ro_subs"], Value::Null);
        let missing = bazarr_ro_map(&[json!({"radarrId": 2, "subtitles": [{"code2": "ro"}], "missing_subtitles": [{"code2": "ro"}]})]);
        assert_eq!(ro_of(&missing, Some(&json!(2))), Some(false));
        // a movie Bazarr does not know: null
        assert_eq!(summarize_library(&movies, Some(&RoMap::new()))[0]["ro_subs"], Value::Null);
    }

    // py: test_logic.TestWaitingStatus.test_timeleft
    #[test]
    fn timeleft() {
        assert_eq!(parse_timeleft(Some(&json!("01:02:03"))), Some(3723));
        assert_eq!(parse_timeleft(Some(&json!("1.00:00:10"))), Some(86410));
        assert_eq!(parse_timeleft(None), None);
        assert_eq!(format_duration_ro(Some(3723)), "1h 2m");
        assert_eq!(format_duration_ro(Some(86410 + 7200)), "1z 2h");
        assert_eq!(format_duration_ro(Some(59)), "sub 1 min");
        assert_eq!(format_duration_ro(Some(600)), "10 min");
        assert_eq!(format_duration_ro(None), "");
        assert_eq!(parse_timeleft(Some(&json!("00:00:07.5"))), Some(7));
        assert_eq!(parse_timeleft(Some(&json!("aa:00:07"))), None);
        assert_eq!(parse_timeleft(Some(&json!("01:02"))), None);
        // plan fix 17: a bad day part no longer raises
        assert_eq!(parse_timeleft(Some(&json!("x.01:00:00"))), None);
        let q = summarize_queue_record(&json!({"movieId": 1, "size": 10, "sizeleft": 5, "timeleft": "x.01:00:00"}));
        assert_eq!(q["time_left"], json!(""));
    }

    // py: test_logic.TestWaitingStatus.test_queue_record_poster
    #[test]
    fn queue_record_poster() {
        let q = summarize_queue_record(&json!({"movieId": 3, "size": 100, "sizeleft": 38, "status": "downloading",
                                               "trackedDownloadState": "downloading", "timeleft": "00:08:00",
                                               "movie": {"title": "Anatomy of a Fall", "year": 2023}}));
        assert_eq!((q["percent"].clone(), q["time_left"].clone(), q["radarr_id"].clone(), q["poster"].clone()),
                   (json!(62.0), json!("8 min"), json!(3), json!("/img/movie/3")));
        assert_eq!((q["status"].clone(), q["title"].clone(), q["year"].clone()),
                   (json!("Se descarcă"), json!("Anatomy of a Fall"), json!(2023)));
        let p = summarize_queue_record(&json!({"movieId": 3, "size": 0, "status": "paused", "trackedDownloadState": "downloading"}));
        assert_eq!((p["status"].clone(), p["percent"].clone(), p["time_left"].clone()), (json!("Pauză"), json!(0.0), json!("")));
    }

    #[test]
    fn iso_dates() {
        let d = parse_iso(Some(&json!("2026-11-17T00:00:00Z"))).unwrap();
        assert_eq!((d.year, d.month, d.day, d.ts), (2026, 11, 17, 1794873600.0));
        assert_eq!(parse_iso(Some(&json!("2026-11-17"))).unwrap().ts, 1794873600.0);
        assert_eq!(parse_iso(Some(&json!("2026-11-17T02:00:00+02:00"))).unwrap().ts, 1794873600.0);
        assert_eq!(parse_iso(Some(&json!("2026-11-17T00:00:00.500Z"))).unwrap().ts, 1794873600.5);
        for bad in ["", "garbage", "2026-13-01", "2026-02-30T00:00:00Z", "2026-11-17X00:00"] {
            assert_eq!(parse_iso(Some(&json!(bad))), None, "{bad}");
        }
        assert_eq!(parse_iso(Some(&json!(5))), None);
        assert_eq!(parse_iso(None), None);
    }
}
