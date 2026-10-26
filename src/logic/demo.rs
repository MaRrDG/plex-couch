//! Fake status data and drawn posters for `--demo` (spec 8). No network,
//! no secrets. Posters are SVGs served from the same `/img/...` URLs the real app proxies.

use crate::logic::images::poster_url;
use crate::logic::torrents::{hr_warnings, sort_torrents, summarize_torrent, torrent_title, torrent_title_map};
use serde_json::{json, Value};

/// radarr id -> (title, year, top colour, bottom colour, ink colour, serif)
pub const MOVIES: [(u32, &str, i64, &str, &str, &str, bool); 6] = [
    (1, "The Godfather", 1972, "#0B0A09", "#3A2A1A", "#E8DCC4", true),
    (2, "Dune: Part Two", 2024, "#C9772E", "#3B1D10", "#FFF1DE", false),
    (3, "Anatomy of a Fall", 2023, "#D9D4CA", "#7A7468", "#1C1B19", true),
    (4, "Past Lives", 2023, "#2E4A66", "#B9C6D2", "#0F1B27", true),
    (5, "The Odyssey", 2026, "#0D2233", "#5E7F86", "#EAF1F2", true),
    (6, "Perfect Days", 2023, "#3E5A3A", "#C9D3B0", "#14200F", true),
];

/// Plex rating key -> radarr id (the movie playing on the TV).
pub const PLEX_KEYS: [(&str, u32); 1] = [("101", 2)];

fn movie(rid: u32) -> Option<&'static (u32, &'static str, i64, &'static str, &'static str, &'static str, bool)> {
    MOVIES.iter().find(|m| m.0 == rid)
}

/// `xml.sax.saxutils.escape`.
fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn lines(title: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for w in title.to_uppercase().split_whitespace() {
        if !cur.is_empty() && cur.chars().count() + 1 + w.chars().count() > width {
            out.push(std::mem::take(&mut cur));
            cur = w.to_string();
        } else {
            cur = format!("{cur} {w}").trim().to_string();
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out.truncate(4);
    out
}

/// A drawn 2:3 poster with the title.
pub fn poster_svg(title: &str, top: &str, bottom: &str, ink: &str, serif: bool) -> Vec<u8> {
    let lines = lines(title, 10);
    let longest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(1);
    let fs = ((88.0 / longest as f64 - 1.0) / 0.72).clamp(8.0, 17.0);
    let y0 = 110.0 - (lines.len() as f64 - 1.0) * fs * 0.55;
    let family = if serif { "Georgia, serif" } else { "Arial Black, Arial, sans-serif" };
    let text: String = lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            format!(
                "<text x=\"50\" y=\"{:.1}\" text-anchor=\"middle\" font-family=\"{family}\" font-size=\"{fs:.1}\" font-weight=\"700\" \
                 letter-spacing=\"1\" fill=\"{ink}\">{}</text>",
                y0 + i as f64 * fs * 1.1,
                escape(line)
            )
        })
        .collect();
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 150\" width=\"240\" height=\"360\">\
         <defs><linearGradient id=\"g\" x1=\"0\" y1=\"0\" x2=\"0\" y2=\"1\"><stop offset=\"0\" stop-color=\"{top}\"/>\
         <stop offset=\"1\" stop-color=\"{bottom}\"/></linearGradient></defs>\
         <rect width=\"100\" height=\"150\" fill=\"url(#g)\"/>\
         <circle cx=\"50\" cy=\"56\" r=\"22\" fill=\"{ink}\" opacity=\".14\"/>{text}</svg>"
    )
    .into_bytes()
}

/// A drawn 16:9 backdrop: a soft gradient with two light shapes (no text).
pub fn backdrop_svg(top: &str, bottom: &str, ink: &str) -> Vec<u8> {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 160 90\" width=\"720\" height=\"405\">\
         <defs><linearGradient id=\"g\" x1=\"0\" y1=\"0\" x2=\"1\" y2=\"1\"><stop offset=\"0\" stop-color=\"{top}\"/>\
         <stop offset=\"1\" stop-color=\"{bottom}\"/></linearGradient></defs>\
         <rect width=\"160\" height=\"90\" fill=\"url(#g)\"/>\
         <circle cx=\"118\" cy=\"30\" r=\"26\" fill=\"{ink}\" opacity=\".22\"/>\
         <ellipse cx=\"40\" cy=\"92\" rx=\"70\" ry=\"24\" fill=\"{bottom}\" opacity=\".18\"/></svg>"
    )
    .into_bytes()
}

/// `(bytes, content type)` for a demo `/img/<kind>/<ident>`, or None.
pub fn poster(kind: &str, ident: &str) -> Option<(Vec<u8>, String)> {
    let rid = if kind.starts_with("plex") {
        PLEX_KEYS.iter().find(|(k, _)| *k == ident).map(|(_, r)| *r)
    } else {
        ident.parse::<u32>().ok()
    }?;
    let &(_, title, _, top, bottom, ink, serif) = movie(rid)?;
    let svg = if kind.ends_with("-art") { backdrop_svg(top, bottom, ink) } else { poster_svg(title, top, bottom, ink, serif) };
    Some((svg, "image/svg+xml".into()))
}

fn demo_movie(rid: u32, extra: Value) -> Value {
    let m = movie(rid).expect("demo movie");
    let mut d = json!({"title": m.1, "year": m.2, "radarr_id": rid, "poster": poster_url("movie", &rid.to_string())});
    if let (Value::Object(o), Value::Object(e)) = (&mut d, extra) {
        o.extend(e);
    }
    d
}

const H_DUNE: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const H_PAST: &str = "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";
const H_ANAT: &str = "c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3";

/// The three demo torrents through the real title-map, summarize and sort functions.
pub fn demo_torrents() -> Vec<Value> {
    let raw = [
        json!({"hash": H_DUNE, "name": "Dune.Part.Two.2024.2160p.UHD.BluRay.x265-GROUP",
               "state": "uploading", "progress": 1, "ratio": 0.42, "seeding_time": 12 * 3600}),
        json!({"hash": H_PAST, "name": "Past.Lives.2023.2160p.WEB-DL.DDP5.1.HDR.H.265-GROUP",
               "state": "stalledUP", "progress": 1, "ratio": 1.37, "seeding_time": 30 * 3600}),
        json!({"hash": H_ANAT, "name": "Anatomy.of.a.Fall.2023.2160p.WEB.H265-GROUP",
               "state": "downloading", "progress": 0.62, "ratio": 0.0, "seeding_time": 0, "dlspeed": 18_400_000}),
    ];
    let history = [json!({"downloadId": H_DUNE.to_uppercase(), "movieId": 2}),
                   json!({"downloadId": H_PAST.to_uppercase(), "movieId": 4})];
    let queue = [json!({"downloadId": H_ANAT.to_uppercase(), "movieId": 3})];
    let movies: Vec<Value> = MOVIES.iter().map(|m| json!({"id": m.0, "title": m.1})).collect();
    let titles = torrent_title_map(Some(&history), Some(&queue), Some(&movies));
    let mut list: Vec<Value> =
        raw.iter().map(|t| summarize_torrent(t, Some(&torrent_title(t, Some(&titles))), None)).collect();
    sort_torrents(&mut list);
    list
}

/// `demo_status(app_name, power, settings)`; `played` = seconds into the
/// demo movie.
pub fn demo_status(app_name: &str, power: Value, settings: Value, played: i64, now: f64) -> Value {
    let torrents = demo_torrents();
    let warnings = hr_warnings(&torrents);
    json!({
        "app_name": app_name,
        "server_time": now,
        "dry_run": true,
        "demo": true,
        "pc": {"hostname": "DEMO-PC", "uptime": 3 * 86400 + 4 * 3600, "platform": "win32", "disks": [
            {"drive": "C:", "free_gb": 181.4, "total_gb": 930.5, "used_pct": 80.5},
            {"drive": "D:", "free_gb": 807.2, "total_gb": 931.5, "used_pct": 13.3},
        ]},
        "services": {"plex": true, "radarr": true, "prowlarr": true, "bazarr": true, "qbittorrent": true},
        "now_playing": [{
            "title": "Dune: Part Two", "year": 2024, "player": "Samsung TV, sufragerie",
            "product": "Plex for Samsung", "state": "playing", "state_ro": "rulează",
            "progress": crate::json::py_round(played as f64 / 9960.0 * 100.0, 1), "position": played, "duration": 9960,
            "decision": "direct_play", "decision_ro": "redare directă", "transcode": false,
            "rating_key": "101", "poster": poster_url("plex", "101"),
            "backdrop": poster_url("plex-art", "101"), "on_tv": true,
        }],
        "downloads": [demo_movie(3, json!({"percent": 62.0, "time_left": "8 min", "status": "Se descarcă", "movie_id": 3}))],
        "waiting": [
            demo_movie(3, json!({"status": "Se descarcă acum", "downloading": true})),
            demo_movie(5, json!({"status": "Așteaptă lansarea digitală (17 nov 2026)", "downloading": false})),
            demo_movie(6, json!({"status": "Se caută în 4K pe FileList", "downloading": false})),
        ],
        "library": [
            demo_movie(2, json!({"size_gb": 61.8, "ro_subs": true})),
            demo_movie(4, json!({"size_gb": 19.1, "ro_subs": false})),
            demo_movie(1, json!({"size_gb": 19.0, "ro_subs": true})),
        ],
        "torrents": torrents,
        "hr_warnings": warnings,
        "power": power,
        "settings": settings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_reference() {
        // The reference SVG for movie 2, byte for byte.
        let (svg, ct) = poster("movie", "2").unwrap();
        assert_eq!(ct, "image/svg+xml");
        let s = String::from_utf8(svg).unwrap();
        let want = "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 150\" width=\"240\" height=\"360\"><defs><linearGradient id=\"g\" x1=\"0\" y1=\"0\" x2=\"0\" y2=\"1\"><stop offset=\"0\" stop-color=\"#C9772E\"/><stop offset=\"1\" stop-color=\"#3B1D10\"/></linearGradient></defs><rect width=\"100\" height=\"150\" fill=\"url(#g)\"/><circle cx=\"50\" cy=\"56\" r=\"22\" fill=\"#FFF1DE\" opacity=\".14\"/><text x=\"50\" y=\"104.0\" text-anchor=\"middle\" font-family=\"Arial Black, Arial, sans-serif\" font-size=\"10.8\" font-weight=\"700\" letter-spacing=\"1\" fill=\"#FFF1DE\">DUNE: PART</text><text x=\"50\" y=\"116.0\" text-anchor=\"middle\" font-family=\"Arial Black, Arial, sans-serif\" font-size=\"10.8\" font-weight=\"700\" letter-spacing=\"1\" fill=\"#FFF1DE\">TWO</text></svg>";
        assert_eq!(s, want);
        assert!(s.contains(">TWO</text></svg>"));
        assert_eq!(poster("plex", "101").unwrap().0, poster("movie", "2").unwrap().0);
        assert!(String::from_utf8(poster("plex-art", "101").unwrap().0).unwrap().contains("viewBox=\"0 0 160 90\""));
        assert!(poster("plex", "2").is_none());
        assert!(poster("movie", "7").is_none());
        assert_eq!(lines("Anatomy of a Fall", 10), ["ANATOMY OF", "A FALL"]);
    }

    #[test]
    fn demo_torrents_shape() {
        let t = demo_torrents();
        let titles: Vec<&str> = t.iter().map(|x| x["title"].as_str().unwrap()).collect();
        assert_eq!(titles, ["Dune: Part Two", "Anatomy of a Fall", "Past Lives"]);
        assert_eq!(t[0]["remaining"], json!("mai are nevoie de 36 h sau ratio 1"));
        assert_eq!(hr_warnings(&t).len(), 1);
    }
}
