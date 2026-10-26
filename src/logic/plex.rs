//! Plex `/status/sessions` parsing (logic.py:718-785; spec 5.1).

use crate::json::{py_round, truthy};
use crate::logic::images::poster_url;
use crate::logic::val;
use serde_json::{json, Value};

pub fn state_ro(state: &str) -> String {
    match state {
        "playing" => "rulează",
        "paused" => "în pauză",
        "buffering" => "se încarcă",
        other => other,
    }
    .to_string()
}

/// `direct_play` | `direct_stream` | `transcode`. A TranscodeSession also exists for
/// sidecar subtitles only, so only the video/audio decisions count.
pub fn playback_decision(meta: &Value) -> &'static str {
    let Some(ts) = val::get_truthy(meta, "TranscodeSession") else { return "direct_play" };
    let d = [ts.get("videoDecision"), ts.get("audioDecision")];
    if d.iter().any(|x| x.and_then(Value::as_str) == Some("transcode")) {
        "transcode"
    } else if d.iter().any(|x| x.and_then(Value::as_str) == Some("copy")) {
        "direct_stream"
    } else {
        "direct_play"
    }
}

pub fn decision_ro(decision: &str) -> &'static str {
    match decision {
        "transcode" => "transcodare",
        "direct_stream" => "flux direct",
        _ => "redare directă",
    }
}

fn first_truthy(d: &Value, keys: &[&str]) -> String {
    keys.iter().find_map(|k| val::get_truthy(d, k)).map(crate::json::py_str).unwrap_or_default()
}

/// Plex `/status/sessions` JSON -> sessions (internal: `key` and `address` included).
pub fn parse_plex_sessions(data: Option<&Value>) -> Vec<Value> {
    let empty = json!({});
    let container = data.map(|d| val::obj(d, "MediaContainer")).unwrap_or(&empty);
    let mut out = Vec::new();
    for meta in val::arr(container, "Metadata") {
        let player = val::obj(meta, "Player");
        let episode = meta.get("type").and_then(Value::as_str) == Some("episode");
        let mut title = val::s(meta, "title");
        if episode {
            if let Some(show) = val::get_truthy(meta, "grandparentTitle") {
                let mut se = String::new();
                let (p, i) = (meta.get("parentIndex"), meta.get("index"));
                if let (Some(p), Some(i)) = (p.filter(|v| !v.is_null()), i.filter(|v| !v.is_null())) {
                    if let (Some(p), Some(i)) = (crate::json::py_int(p), crate::json::py_int(i)) {
                        se = format!(" S{p:02}E{i:02}");
                    }
                }
                title = format!("{}{se} – {title}", crate::json::py_str(show));
            }
        }
        let duration = val::f(meta.get("duration"));
        let offset = val::f(meta.get("viewOffset"));
        let decision = playback_decision(meta);
        let state = val::s(player, "state");
        let rating_key = if episode && truthy(meta.get("grandparentRatingKey")) {
            val::s(meta, "grandparentRatingKey")
        } else {
            val::s(meta, "ratingKey")
        };
        let has_art = if episode { truthy(meta.get("grandparentArt")) } else { truthy(meta.get("art")) };
        let key = match val::get_truthy(meta, "sessionKey").or_else(|| val::get_truthy(val::obj(meta, "Session"), "id")) {
            Some(v) => crate::json::py_str(v),
            None => String::new(),
        };
        let poster = poster_url("plex", &rating_key);
        out.push(json!({
            "key": key,
            "title": title,
            "year": val::raw(meta, "year"),
            "player": first_truthy(player, &["title", "device", "product"]),
            "product": val::s(player, "product"),
            "state": state,
            "state_ro": state_ro(&state),
            "progress": if duration != 0.0 { py_round(offset / duration * 100.0, 1) } else { 0.0 },
            "position": (offset / 1000.0).trunc() as i64,
            "duration": (duration / 1000.0).trunc() as i64,
            "decision": decision,
            "decision_ro": decision_ro(decision),
            "transcode": decision == "transcode",
            "address": val::s(player, "address"),
            "rating_key": if poster.is_some() { Value::from(rating_key.clone()) } else { Value::Null },
            "poster": poster.clone(),
            "backdrop": if has_art { poster_url("plex-art", &rating_key) } else { None },
        }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logic::status::{public_session, with_backdrop_fallback};

    // py: test_logic.TestPlexParse.test_parse
    #[test]
    fn parse() {
        let mut data = json!({"MediaContainer": {"Metadata": [{
            "title": "The Godfather", "year": 1972, "type": "movie", "sessionKey": "2",
            "duration": 10000, "viewOffset": 2500,
            "Player": {"title": "TV 2024", "state": "paused", "address": "10.0.0.129"},
            "TranscodeSession": {"subtitleDecision": "copy"},
        }]}});
        let s = parse_plex_sessions(Some(&data))[0].clone();
        assert_eq!(s["progress"], json!(25.0));
        assert_eq!(s["decision"], json!("direct_play"));
        assert_eq!(s["state_ro"], json!("în pauză"));
        assert_eq!(s["decision_ro"], json!("redare directă"));
        assert_eq!((s["key"].clone(), s["position"].clone(), s["duration"].clone()), (json!("2"), json!(2), json!(10)));
        let public = public_session(&s);
        assert!(public.get("address").is_none());
        assert!(public.get("key").is_none());
        assert_eq!((public["rating_key"].clone(), public["poster"].clone(), public["backdrop"].clone()),
                   (Value::Null, Value::Null, Value::Null));
        data["MediaContainer"]["Metadata"][0]["ratingKey"] = json!("1");
        let s = parse_plex_sessions(Some(&data))[0].clone();
        assert_eq!((s["rating_key"].clone(), s["poster"].clone(), s["backdrop"].clone()),
                   (json!("1"), json!("/img/plex/1"), Value::Null));
        data["MediaContainer"]["Metadata"][0]["art"] = json!("/library/metadata/1/art/123");
        assert_eq!(parse_plex_sessions(Some(&data))[0]["backdrop"], json!("/img/plex-art/1"));
        data["MediaContainer"]["Metadata"][0]["TranscodeSession"] = json!({"videoDecision": "transcode"});
        assert_eq!(parse_plex_sessions(Some(&data))[0]["transcode"], json!(true));
        data["MediaContainer"]["Metadata"][0]["TranscodeSession"] = json!({"videoDecision": "copy", "audioDecision": "copy"});
        let s = parse_plex_sessions(Some(&data))[0].clone();
        assert_eq!((s["decision"].clone(), s["decision_ro"].clone()), (json!("direct_stream"), json!("flux direct")));
    }

    // py: test_logic.TestPlexParse.test_backdrop_fallback_to_radarr
    #[test]
    fn backdrop_fallback_to_radarr() {
        let movies = [json!({"id": 2, "title": "The Godfather", "year": 1972}), json!({"id": 3, "title": "Other", "year": 1972})];
        let s = json!({"title": "The Godfather", "year": 1972, "backdrop": null});
        assert_eq!(with_backdrop_fallback(s.clone(), Some(&movies))["backdrop"], json!("/img/movie-art/2"));
        let mut other_year = s.clone();
        other_year["year"] = json!(1990);
        assert_eq!(with_backdrop_fallback(other_year, Some(&movies))["backdrop"], Value::Null);
        assert_eq!(with_backdrop_fallback(s.clone(), None)["backdrop"], Value::Null);
        let mut kept = s;
        kept["backdrop"] = json!("/img/plex-art/1");
        assert_eq!(with_backdrop_fallback(kept, Some(&movies))["backdrop"], json!("/img/plex-art/1"));
    }

    // py: test_logic.TestPlexParse.test_episode_uses_show_poster
    #[test]
    fn episode_uses_show_poster() {
        let mut data = json!({"MediaContainer": {"Metadata": [{
            "title": "Pilot", "type": "episode", "grandparentTitle": "Show", "parentIndex": 1, "index": 2,
            "ratingKey": "50", "grandparentRatingKey": "40", "Player": {"state": "playing"}}]}});
        let s = parse_plex_sessions(Some(&data))[0].clone();
        assert_eq!(s["title"], json!("Show S01E02 – Pilot"));
        assert_eq!(s["poster"], json!("/img/plex/40"));
        assert_eq!(s["backdrop"], Value::Null);
        data["MediaContainer"]["Metadata"][0]["grandparentArt"] = json!("/library/metadata/40/art/9");
        assert_eq!(parse_plex_sessions(Some(&data))[0]["backdrop"], json!("/img/plex-art/40"));
        data["MediaContainer"]["Metadata"][0]["index"] = Value::Null;
        assert_eq!(parse_plex_sessions(Some(&data))[0]["title"], json!("Show – Pilot"));
    }

    // py: test_logic.TestPlexParse.test_empty
    #[test]
    fn empty() {
        assert!(parse_plex_sessions(Some(&json!({"MediaContainer": {"size": 0}}))).is_empty());
        assert!(parse_plex_sessions(None).is_empty());
    }
}
