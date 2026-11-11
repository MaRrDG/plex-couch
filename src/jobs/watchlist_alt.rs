//! Watchlist step 5 (Radarr only): find a movie that the indexer lists under another title.
//!
//! Radarr searches the indexers by IMDb id; some releases (anime films on FileList, for
//! example) are not linked to the id and carry a romaji title, so Radarr finds nothing. For
//! such a movie this step searches Prowlarr by text and pushes the matching releases to
//! Radarr (`POST /api/v3/release/push`), which matches the movie by its alternate titles,
//! checks the quality profile and grabs an approved release at once.
//!
//! - Candidates: Radarr movies that are `monitored`, have no file, are `isAvailable`, were
//!   `added` at least 60 min ago, are not in the Radarr queue and have no `grabbed` event in
//!   their history. Each movie is tried at most once per 6 h (in memory, so a restart
//!   retries), at most 2 per tick, never-tried first.
//! - Titles: `title`, `alternateTitles[].title`, `originalTitle`, Latin script only.
//!   Matching form: diacritics stripped, lower case, letters and digits only, romaji long
//!   vowels collapsed (`ou` -> `o`, `uu` -> `u`).
//! - Queries (Prowlarr `GET /api/v1/search?query=..&type=search`, header `X-Api-Key`): the
//!   longest word sequence shared by the most Latin titles (at least 2 words, in at least 2
//!   titles, e.g. "kimetsu no yaiba"), then the main title.
//! - A release qualifies when it is a torrent with seeders, the year in its name equals the
//!   movie year, and its name before the year equals a movie title or one contains the
//!   other with the shorter at least 80 % of the longer (both at least 6 characters).
//! - Order: 2160p, 1080p, 720p, other, then seeders. Pushed one by one (at most 5) until
//!   Radarr answers `approved`. A Prowlarr error stops the step for this tick (the movie is
//!   retried next tick). Dry-run: GETs only, the would-be pushes are logged.

use crate::app::AppState;
use crate::jobs::common::{id_of, parse_iso_utc, Quiet};
use crate::log_info;
use crate::services::arr_jobs::Arr;
use crate::services::prowlarr::Prowlarr;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

/// A movie must have been in Radarr this long (its own search had its chance).
pub const MIN_AGE: f64 = 3600.0;
/// One attempt per movie per this long.
pub const RETRY_EVERY: f64 = 6.0 * 3600.0;
/// Movies attempted (searched in Prowlarr) per tick.
pub const MAX_MOVIES: usize = 2;
pub const MAX_QUERIES: usize = 3;
pub const MAX_PUSHES: usize = 5;
/// Radarr history `eventType` for `grabbed`.
const GRABBED: i64 = 1;
const MIN_OVERLAP: f64 = 0.8;
const MIN_CONTAIN_LEN: usize = 6;

/// The ASCII letters for a lower-case accented Latin letter; None for anything else.
fn fold(c: char) -> Option<&'static str> {
    Some(match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' | 'ǎ' => "a",
        'æ' => "ae",
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => "c",
        'ď' | 'đ' | 'ð' => "d",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => "e",
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => "g",
        'ĥ' | 'ħ' => "h",
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' | 'ǐ' => "i",
        'ĵ' => "j",
        'ķ' => "k",
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => "l",
        'ñ' | 'ń' | 'ņ' | 'ň' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' | 'ǒ' => "o",
        'œ' => "oe",
        'ŕ' | 'ŗ' | 'ř' => "r",
        'ś' | 'ŝ' | 'ş' | 'š' | 'ș' => "s",
        'ß' => "ss",
        'ţ' | 'ť' | 'ŧ' | 'ț' => "t",
        'þ' => "th",
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' | 'ǔ' => "u",
        'ŵ' => "w",
        'ý' | 'ÿ' | 'ŷ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        _ => return None,
    })
}

/// The words of a Latin-script title: lower case, diacritics stripped, split on anything
/// that is not a letter or digit. None when it has a non-Latin letter (CJK, Cyrillic...)
/// or no word at all.
pub fn latin_words(t: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    for c in t.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            cur.push(c);
        } else if ('\u{300}'..='\u{36f}').contains(&c) {
            // A combining accent (decomposed text): drop it.
        } else if let Some(f) = fold(c) {
            cur.push_str(f);
        } else if c.is_alphanumeric() {
            return None;
        } else if !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    (!words.is_empty()).then_some(words)
}

/// Joined without separators, romaji long vowels collapsed (`ou` -> `o`, `uu` -> `u`).
pub fn squash(words: &[String]) -> String {
    let mut s = words.concat();
    loop {
        let next = s.replace("ou", "o").replace("uu", "u");
        if next == s {
            return s;
        }
        s = next;
    }
}

/// The matching form of a title; None when it is not Latin script.
pub fn norm(t: &str) -> Option<String> {
    latin_words(t).map(|w| squash(&w))
}

/// A release name -> (matching form of the part before the year, year). The year is the
/// last stand-alone 4-digit token in 1900..=2099 that has something before it.
pub fn split_release(name: &str) -> Option<(String, i64)> {
    let tokens: Vec<&str> = name.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).collect();
    let (i, year) = tokens.iter().enumerate().skip(1).rev().find_map(|(i, t)| {
        let y = (t.len() == 4 && t.bytes().all(|b| b.is_ascii_digit())).then(|| t.parse::<i64>().ok()).flatten()?;
        (1900..=2099).contains(&y).then_some((i, y))
    })?;
    Some((norm(&tokens[..i].join(" "))?, year))
}

/// The release's title part matches one of the movie's (matching-form) titles: equal, or
/// one contains the other and the shorter is at least 80 % of the longer.
pub fn title_matches(rel: &str, titles: &[String]) -> bool {
    titles.iter().any(|t| {
        if rel == t {
            return true;
        }
        let (short, long) = if rel.len() <= t.len() { (rel, t.as_str()) } else { (t.as_str(), rel) };
        short.len() >= MIN_CONTAIN_LEN && long.contains(short) && short.len() as f64 >= MIN_OVERLAP * long.len() as f64
    })
}

/// A release from Prowlarr that may be the movie: torrent, seeded, same year, title match.
pub fn release_matches(r: &Value, titles: &[String], year: i64) -> bool {
    let torrent = r.get("protocol").and_then(Value::as_str).is_some_and(|p| p.eq_ignore_ascii_case("torrent"));
    let seeded = r.get("seeders").and_then(Value::as_i64).unwrap_or(0) > 0;
    let link = ["downloadUrl", "magnetUrl"].iter().any(|k| r.get(*k).and_then(Value::as_str).is_some_and(|u| !u.is_empty()));
    let Some(name) = r.get("title").and_then(Value::as_str) else { return false };
    torrent
        && seeded
        && link
        && split_release(name).is_some_and(|(rel, y)| y == year && title_matches(&rel, titles))
}

/// 3 for 2160p/4K/UHD, 2 for 1080p, 1 for 720p, 0 otherwise.
pub fn resolution_rank(name: &str) -> u8 {
    let toks: Vec<String> = name.split(|c: char| !c.is_alphanumeric()).map(str::to_lowercase).collect();
    let has = |k: &[&str]| toks.iter().any(|t| k.contains(&t.as_str()));
    if has(&["2160p", "4k", "uhd"]) {
        3
    } else if has(&["1080p"]) {
        2
    } else if has(&["720p"]) {
        1
    } else {
        0
    }
}

/// Best first: resolution, then seeders.
pub fn order_releases(rels: &mut [Value]) {
    let key = |r: &Value| {
        let name = r.get("title").and_then(Value::as_str).unwrap_or("");
        (resolution_rank(name), r.get("seeders").and_then(Value::as_i64).unwrap_or(0))
    };
    rels.sort_by_key(|r| std::cmp::Reverse(key(r)));
}

/// The movie's Latin titles in Radarr order: `title`, `alternateTitles[].title`,
/// `originalTitle` (raw text; deduplicated by matching form).
pub fn movie_titles(m: &Value) -> Vec<String> {
    let mut raw: Vec<&str> = Vec::new();
    raw.extend(m.get("title").and_then(Value::as_str));
    for a in m.get("alternateTitles").and_then(Value::as_array).into_iter().flatten() {
        raw.extend(a.get("title").and_then(Value::as_str));
    }
    raw.extend(m.get("originalTitle").and_then(Value::as_str));
    let mut seen = HashSet::new();
    raw.into_iter().filter(|t| norm(t).is_some_and(|n| seen.insert(n))).map(String::from).collect()
}

/// The longest word sequence (at least 2 words) found in the most titles (at least 2),
/// compared in matching form, returned as plain lower-case words of its first occurrence.
pub fn core_phrase(titles: &[String]) -> Option<String> {
    let mut found: HashMap<String, (HashSet<usize>, usize, String)> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for (ti, t) in titles.iter().enumerate() {
        let Some(words) = latin_words(t) else { continue };
        for n in 2..=words.len() {
            for w in words.windows(n) {
                let key = w.iter().map(|x| squash(std::slice::from_ref(x))).collect::<Vec<_>>().join(" ");
                let e = found.entry(key.clone()).or_insert_with(|| {
                    order.push(key.clone());
                    (HashSet::new(), n, w.join(" "))
                });
                e.0.insert(ti);
            }
        }
    }
    let mut best: Option<(usize, usize, &String)> = None;
    for k in &order {
        let (in_titles, n, text) = &found[k];
        let c = in_titles.len();
        if c >= 2 && best.is_none_or(|(bc, bn, _)| (c, *n) > (bc, bn)) {
            best = Some((c, *n, text));
        }
    }
    best.map(|(_, _, t)| t.clone())
}

/// The Prowlarr queries for a movie: the core phrase, then the main title (at most 3).
pub fn queries(m: &Value) -> Vec<String> {
    let titles = movie_titles(m);
    let mut out: Vec<String> = Vec::new();
    let mut add = |q: String| {
        if !q.is_empty() && !out.contains(&q) && out.len() < MAX_QUERIES {
            out.push(q);
        }
    };
    if let Some(c) = core_phrase(&titles) {
        add(c);
    }
    if let Some(w) = m.get("title").and_then(Value::as_str).and_then(latin_words) {
        add(w.join(" "));
    }
    out
}

/// Monitored, no file, available, added at least [`MIN_AGE`] ago, has a year, not queued.
pub fn eligible(m: &Value, queued: &HashSet<i64>, now: f64) -> bool {
    let flag = |k: &str| m.get(k).and_then(Value::as_bool).unwrap_or(false);
    let Some(id) = id_of(m.get("id")) else { return false };
    let old = m.get("added").and_then(Value::as_str).and_then(parse_iso_utc).is_some_and(|t| now - t as f64 >= MIN_AGE);
    flag("monitored")
        && !flag("hasFile")
        && flag("isAvailable")
        && old
        && m.get("year").and_then(Value::as_i64).unwrap_or(0) > 0
        && !queued.contains(&id)
}

/// Not tried in the last [`RETRY_EVERY`].
pub fn due(tried: &HashMap<i64, f64>, id: i64, now: f64) -> bool {
    tried.get(&id).is_none_or(|t| now - t >= RETRY_EVERY)
}

/// The eligible, due movies, never-tried first, then the longest ago.
pub fn pick<'a>(movies: &'a [Value], queued: &HashSet<i64>, tried: &HashMap<i64, f64>, now: f64) -> Vec<&'a Value> {
    let mut v: Vec<&Value> = movies
        .iter()
        .filter(|m| eligible(m, queued, now))
        .filter(|m| id_of(m.get("id")).is_some_and(|id| due(tried, id, now)))
        .collect();
    let last = |m: &Value| id_of(m.get("id")).and_then(|id| tried.get(&id).copied()).unwrap_or(f64::MIN);
    v.sort_by(|a, b| last(a).total_cmp(&last(b)));
    v
}

/// The body for `POST /api/v3/release/push` (the indexer as Radarr names Prowlarr's).
pub fn push_body(r: &Value) -> Value {
    let g = |k: &str| r.get(k).cloned().unwrap_or(Value::Null);
    let indexer = r.get("indexer").and_then(Value::as_str).filter(|i| !i.is_empty());
    let mut b = json!({
        "title": g("title"), "protocol": "torrent", "publishDate": g("publishDate"),
        "indexer": indexer.map(|i| format!("{i} (Prowlarr)")).unwrap_or_else(|| "Prowlarr".into()),
        "size": g("size"), "seeders": g("seeders"),
    });
    for k in ["downloadUrl", "magnetUrl"] {
        if let Some(u) = r.get(k).and_then(Value::as_str).filter(|u| !u.is_empty()) {
            b[k] = json!(u);
        }
    }
    b
}

fn grabbed(history: &[Value]) -> bool {
    history.iter().any(|h| match h.get("eventType") {
        Some(Value::String(s)) => s.eq_ignore_ascii_case("grabbed"),
        Some(v) => v.as_i64() == Some(GRABBED),
        None => false,
    })
}

/// One run of the step (see the module docs). `tried`: movie id -> last attempt.
pub async fn run(app: &AppState, radarr: &Arr, tried: &mut HashMap<i64, f64>, now: f64, quiet: &mut Quiet) {
    let movies = match radarr.items().await {
        Ok(m) => {
            quiet.clear("alt-movies");
            m
        }
        Err(e) => {
            quiet.warn("alt-movies", format!("watchlist: Radarr movies failed, alternative-title search skipped: {e}"));
            return;
        }
    };
    let none = HashSet::new();
    if pick(&movies, &none, tried, now).is_empty() {
        return;
    }
    let queued: HashSet<i64> = match radarr.queue_all().await {
        Ok(q) => {
            quiet.clear("alt-queue");
            q.iter().filter_map(|r| id_of(r.get("movieId"))).collect()
        }
        Err(e) => {
            quiet.warn("alt-queue", format!("watchlist: Radarr queue failed, alternative-title search skipped: {e}"));
            return;
        }
    };
    let prowlarr = Prowlarr::new(app.services.prowlarr.c.clone());
    let mut attempts = 0;
    for m in pick(&movies, &queued, tried, now) {
        if attempts >= MAX_MOVIES {
            break;
        }
        let Some(id) = id_of(m.get("id")) else { continue };
        let name = m.get("title").and_then(Value::as_str).unwrap_or("").to_string();
        match radarr.movie_history(id, GRABBED).await {
            Ok(h) if grabbed(&h) => {
                tried.insert(id, now);
                continue;
            }
            Ok(_) => {}
            Err(e) => {
                quiet.warn(&format!("alt-history-{id}"), format!("watchlist: Radarr history of '{name}' failed: {e}"));
                continue;
            }
        }
        attempts += 1;
        let Some(found) = search(&prowlarr, m, quiet).await else { return };
        tried.insert(id, now);
        let key = format!("alt-{id}");
        if found.is_empty() {
            quiet.info(&key, format!("watchlist: '{name}' not found under an alternative title either"));
            continue;
        }
        if app.dry_run {
            let names: Vec<&str> = found.iter().filter_map(|r| r.get("title").and_then(Value::as_str)).collect();
            log_info!("dry-run: would push to Radarr for '{name}', in order until one is approved: {}", names.join(" | "));
            continue;
        }
        push(radarr, &name, &key, &found, quiet).await;
    }
}

/// The qualifying releases, best first (at most [`MAX_PUSHES`]); None on a Prowlarr error.
async fn search(prowlarr: &Prowlarr, m: &Value, quiet: &mut Quiet) -> Option<Vec<Value>> {
    let titles: Vec<String> = movie_titles(m).iter().filter_map(|t| norm(t)).collect();
    let year = m.get("year").and_then(Value::as_i64).unwrap_or(0);
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    for q in queries(m) {
        let rels = match prowlarr.search(&q).await {
            Ok(r) => {
                quiet.clear("alt-prowlarr");
                r
            }
            Err(e) => {
                quiet.warn("alt-prowlarr", format!("watchlist: Prowlarr search failed, alternative-title search skipped: {e}"));
                return None;
            }
        };
        for r in rels {
            let link = ["downloadUrl", "magnetUrl", "guid", "title"]
                .iter()
                .find_map(|k| r.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()))
                .unwrap_or("")
                .to_string();
            if release_matches(&r, &titles, year) && seen.insert(link) {
                found.push(r);
            }
        }
    }
    order_releases(&mut found);
    found.truncate(MAX_PUSHES);
    Some(found)
}

/// Push in order until Radarr approves one (and so grabs it).
async fn push(radarr: &Arr, name: &str, key: &str, found: &[Value], quiet: &mut Quiet) {
    let mut why = Vec::new();
    for r in found {
        let title = r.get("title").and_then(Value::as_str).unwrap_or("");
        match radarr.release_push(&push_body(r)).await {
            Ok(d) if d.iter().any(|x| x.get("approved").and_then(Value::as_bool) == Some(true)) => {
                log_info!("watchlist: '{name}' found under an alternative title: {title}");
                quiet.clear(key);
                return;
            }
            Ok(d) => {
                let rej: Vec<String> = d
                    .iter()
                    .flat_map(|x| x.get("rejections").and_then(Value::as_array).cloned().unwrap_or_default())
                    .filter_map(|x| x.as_str().map(String::from).or_else(|| x.get("reason").and_then(Value::as_str).map(String::from)))
                    .collect();
                why.push(if rej.is_empty() { "not approved".to_string() } else { rej.join("; ") });
            }
            Err(e) => why.push(format!("push failed: {e}")),
        }
    }
    quiet.info(
        key,
        format!("watchlist: '{name}': Radarr approved none of {} alternative-title release(s) ({})", found.len(), why.join(" / ")),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::testutil::{app, fake, ok, status, Fake, Req, Resp};
    use std::sync::{Arc, Mutex};

    const T0: f64 = 1_793_000_000.0;
    const MUGEN_1080: &str = "Gekijô-ban.Kimetsu.no.Yaiba.Mugen.Jô-hen.2025.1080p.CR.WEB-DL.DDP2.0.H.264-Kitsune";
    const MUGEN_720: &str = "Gekijô-ban.Kimetsu.no.Yaiba.Mugen.Jô-hen.2025.720p.CR.WEB-DL.AAC2.0.H.264-Kitsune";
    const RESSHA: &str = "Kimetsu.no.Yaiba.Mugen.Ressha-Hen.2020.1080p.BluRay.x264-WiKi";

    /// The real Radarr record (abridged), added long before T0.
    fn infinity_castle(id: i64) -> Value {
        json!({
            "id": id, "title": "Demon Slayer: Kimetsu no Yaiba Infinity Castle", "year": 2025,
            "originalTitle": "劇場版「鬼滅の刃」無限城編", "imdbId": "tt32820897",
            "monitored": true, "hasFile": false, "isAvailable": true, "added": "2026-10-01T00:00:00Z",
            "alternateTitles": [
                {"title": "Gekijōban Kimetsu no Yaiba Mugenjō-hen"},
                {"title": "Gekijouban Kimetsu no Yaiba Mugenjou-hen"},
                {"title": "Kimetsu no Yaiba: Mugenjou-hen"},
                {"title": "Kimetsu no Yaiba Movie: Mugen Jou-hen"},
                {"title": "鬼灭之刃 无限城篇"},
                {"title": "Истребитель демонов: Бесконечная крепость"},
                {"title": "Guardianes de la noche: Castillo infinito"}
            ]
        })
    }

    fn titles() -> Vec<String> {
        movie_titles(&infinity_castle(1)).iter().filter_map(|t| norm(t)).collect()
    }

    fn rel(title: &str, seeders: i64) -> Value {
        json!({"title": title, "seeders": seeders, "protocol": "torrent", "size": 9_100_000_000i64,
               "indexer": "FileList.io", "publishDate": "2025-10-20T00:00:00Z",
               "downloadUrl": format!("http://prowlarr/dl/{}", title.len() as i64 * 1000 + seeders)})
    }

    #[test]
    fn normalization() {
        assert_eq!(norm("Gekijōban Kimetsu no Yaiba Mugenjō-hen").unwrap(), "gekijobankimetsunoyaibamugenjohen");
        assert_eq!(norm("Gekijouban Kimetsu no Yaiba Mugenjou-hen").unwrap(), "gekijobankimetsunoyaibamugenjohen");
        assert_eq!(norm("Gekijô-ban Kimetsu no Yaiba Mugen Jô-hen").unwrap(), "gekijobankimetsunoyaibamugenjohen");
        assert_eq!(norm("Pokémon: Ça, Ünïcode ß").unwrap(), "pokemoncaunicodess");
        // Decomposed accents (o + combining macron).
        assert_eq!(norm("Mugenjo\u{304}").unwrap(), "mugenjo");
        assert_eq!(norm("Kyuukyoku"), Some("kyukyoku".into()));
        assert_eq!(norm("劇場版「鬼滅の刃」無限城編"), None);
        assert_eq!(norm("Истребитель демонов"), None);
        assert_eq!(norm(" - "), None);
        let t = movie_titles(&infinity_castle(1));
        assert_eq!(t.len(), 5, "{t:?}"); // main, 3 distinct romaji (Gekijou = Gekijō), Spanish
        assert!(t.iter().all(|x| !x.contains('鬼') && !x.contains('И')));
    }

    #[test]
    fn release_matching_with_the_real_titles() {
        let t = titles();
        assert_eq!(split_release(MUGEN_1080), Some(("gekijobankimetsunoyaibamugenjohen".into(), 2025)));
        assert!(release_matches(&rel(MUGEN_1080, 53), &t, 2025));
        assert!(release_matches(&rel(MUGEN_720, 20), &t, 2025));
        // The 2020 film: other year and other title.
        assert!(!release_matches(&rel(RESSHA, 99), &t, 2025));
        let (ressha, y) = split_release(RESSHA).unwrap();
        assert_eq!(y, 2020);
        assert!(!title_matches(&ressha, &t), "Mugen Ressha-hen is another film even with the right year");
        // Wrong year for the right title.
        assert!(!release_matches(&rel(&MUGEN_1080.replace("2025", "2024"), 53), &t, 2025));
        // Not seeded / not a torrent / no year.
        assert!(!release_matches(&rel(MUGEN_1080, 0), &t, 2025));
        let mut usenet = rel(MUGEN_1080, 5);
        usenet["protocol"] = json!("usenet");
        assert!(!release_matches(&usenet, &t, 2025));
        assert!(!release_matches(&rel("Gekijo-ban.Kimetsu.no.Yaiba.Mugen.Jo-hen.1080p", 5), &t, 2025));
        // The English name and a short prefix of it.
        assert!(release_matches(&rel("Demon.Slayer.Kimetsu.no.Yaiba.Infinity.Castle.2025.2160p.WEB-DL", 5), &t, 2025));
        assert!(!release_matches(&rel("Demon.Slayer.2025.1080p", 5), &t, 2025));
        assert!(!release_matches(&rel("Kimetsu.no.Yaiba.2025.1080p", 5), &t, 2025));
        // A title inside a longer name only with >= 80 % overlap.
        assert!(title_matches("demonslayerkimetsunoyaibainfinitycastlepart1", &t));
        assert!(!title_matches("kimetsunoyaibamugenjohenspecialeditionextrascollection", &t));
        // Short titles match only exactly.
        assert!(title_matches("up", &["up".into()]));
        assert!(!title_matches("upx", &["up".into()]));
        assert_eq!(split_release("Blade.Runner.2049.2017.1080p"), Some(("bladerunner2049".into(), 2017)));
        assert_eq!(split_release("2012.2009.720p"), Some(("2012".into(), 2009)));
    }

    #[test]
    fn core_phrase_and_queries() {
        let m = infinity_castle(1);
        assert_eq!(core_phrase(&movie_titles(&m)).as_deref(), Some("kimetsu no yaiba"));
        assert_eq!(queries(&m), vec!["kimetsu no yaiba", "demon slayer kimetsu no yaiba infinity castle"]);
        // A single Latin title: just that title.
        let solo = json!({"title": "Dune: Part Two", "alternateTitles": [{"title": "デューン"}]});
        assert_eq!(queries(&solo), vec!["dune part two"]);
        assert!(queries(&json!({"title": "デューン"})).is_empty());
    }

    #[test]
    fn order_is_resolution_then_seeders() {
        let mut v = vec![rel("A.2025.720p", 90), rel("B.2025.1080p", 5), rel("C.2025.2160p", 1), rel("D.2025", 500), rel("E.2025.1080p", 50)];
        order_releases(&mut v);
        let names: Vec<&str> = v.iter().map(|r| r["title"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["C.2025.2160p", "E.2025.1080p", "B.2025.1080p", "A.2025.720p", "D.2025"]);
    }

    #[test]
    fn push_body_names_the_prowlarr_indexer() {
        let b = push_body(&rel(MUGEN_1080, 53));
        assert_eq!(b["indexer"], "FileList.io (Prowlarr)");
        assert_eq!(b["protocol"], "torrent");
        assert_eq!(b["title"], MUGEN_1080);
        assert_eq!(b["seeders"], 53);
        assert!(b["downloadUrl"].as_str().unwrap().starts_with("http://prowlarr/dl/"));
        assert!(b.get("magnetUrl").is_none());
    }

    #[test]
    fn candidate_selection() {
        let mut ms = vec![infinity_castle(1)];
        let mut add = |id: i64, k: &str, v: Value| {
            let mut m = infinity_castle(id);
            m[k] = v;
            ms.push(m);
        };
        add(2, "hasFile", json!(true));
        add(3, "isAvailable", json!(false));
        add(4, "monitored", json!(false));
        add(5, "added", json!("2026-10-26T06:40:00Z")); // T0 is 2026-10-26T07:33:20Z: 53 min
        add(6, "id", json!(6)); // queued
        add(7, "year", json!(0));
        add(8, "added", json!("2026-10-26T06:33:00Z")); // 60 min 20 s: old enough
        let queued = HashSet::from([6]);
        let ids = |v: Vec<&Value>| v.iter().map(|m| m["id"].as_i64().unwrap()).collect::<Vec<_>>();
        assert_eq!(ids(pick(&ms, &queued, &HashMap::new(), T0)), vec![1, 8]);
        // Throttle: tried 1 an hour ago -> only 8; tried 8 long ago -> 8 is due again, 1 is not.
        let tried = HashMap::from([(1, T0 - 3600.0), (8, T0 - RETRY_EVERY)]);
        assert_eq!(ids(pick(&ms, &queued, &tried, T0)), vec![8]);
        // Never tried before the longest ago.
        let tried = HashMap::from([(1, T0 - 2.0 * RETRY_EVERY)]);
        assert_eq!(ids(pick(&ms, &queued, &tried, T0)), vec![8, 1]);
    }

    struct World {
        movies: Value,
        queue: Value,
        /// movie id -> grabbed history.
        grabbed: HashSet<i64>,
        search: Mutex<Resp>,
        /// Push answers by release title: approved or not.
        approve: Vec<&'static str>,
    }

    fn world(movies: Value) -> World {
        let search = json!([rel(RESSHA, 99), rel(MUGEN_720, 20), rel(MUGEN_1080, 53),
                            rel("Kimetsu.no.Yaiba.Mugen.Ressha-Hen.2020.720p", 10)]);
        World { movies, queue: json!({"totalRecords": 0, "records": []}), grabbed: HashSet::new(), search: Mutex::new(ok(search)), approve: vec![] }
    }

    async fn serve(w: Arc<World>) -> Fake {
        fake(move |r: &Req| {
            let p = r.path.as_str();
            if p == "/api/v3/movie" {
                return ok(w.movies.clone());
            }
            if p.starts_with("/api/v3/queue") {
                return ok(w.queue.clone());
            }
            if let Some(q) = p.strip_prefix("/api/v3/history/movie?movieId=") {
                let id: i64 = q.split('&').next().unwrap().parse().unwrap();
                let h = if w.grabbed.contains(&id) { json!([{"eventType": "grabbed", "movieId": id}]) } else { json!([]) };
                return ok(h);
            }
            if p.starts_with("/api/v1/search") {
                return w.search.lock().unwrap().clone();
            }
            if p == "/api/v3/release/push" {
                let t = r.json()["title"].as_str().unwrap_or("").to_string();
                let a = w.approve.iter().any(|x| *x == t);
                return ok(json!([{"title": t, "approved": a, "rejections": if a { json!([]) } else { json!(["Not wanted in profile"]) }}]));
            }
            status(404)
        })
        .await
    }

    struct Run {
        f: Fake,
        a: Arc<AppState>,
        tried: HashMap<i64, f64>,
        quiet: Quiet,
    }

    async fn start(w: World, dry: bool) -> Run {
        let f = serve(Arc::new(w)).await;
        let (a, _d) = app(&f.base, dry, json!({}));
        Run { f, a, tried: HashMap::new(), quiet: Quiet::default() }
    }

    impl Run {
        async fn tick(&mut self, now: f64) {
            let radarr = Arr::radarr(self.a.services.radarr.c.clone());
            run(&self.a, &radarr, &mut self.tried, now, &mut self.quiet).await;
        }
        fn pushes(&self) -> Vec<String> {
            self.f.find("POST", "/api/v3/release/push").iter().map(|r| r.json()["title"].as_str().unwrap().to_string()).collect()
        }
        fn searches(&self) -> Vec<Req> {
            self.f.find("GET", "/api/v1/search")
        }
    }

    #[tokio::test]
    async fn pushes_best_first_and_stops_at_the_first_approved() {
        let mut w = world(json!([infinity_castle(1)]));
        w.approve = vec![MUGEN_720];
        let mut r = start(w, false).await;
        r.tick(T0).await;
        // 1080p (rejected) then 720p (approved); the 2020 film never.
        assert_eq!(r.pushes(), vec![MUGEN_1080.to_string(), MUGEN_720.to_string()]);
        let p = &r.f.find("POST", "/api/v3/release/push")[0];
        assert_eq!(p.header("x-api-key"), Some("radarr-key"));
        let b = p.json();
        assert_eq!((b["indexer"].as_str(), b["protocol"].as_str(), b["size"].as_i64()), (Some("FileList.io (Prowlarr)"), Some("torrent"), Some(9_100_000_000)));
        let s = r.searches();
        assert_eq!(
            s.iter().map(|q| q.path.clone()).collect::<Vec<_>>(),
            vec![
                "/api/v1/search?query=kimetsu%20no%20yaiba&type=search".to_string(),
                "/api/v1/search?query=demon%20slayer%20kimetsu%20no%20yaiba%20infinity%20castle&type=search".to_string()
            ]
        );
        assert!(s.iter().all(|q| q.header("x-api-key") == Some("prowlarr-key")));
        assert_eq!(r.tried.get(&1), Some(&T0));
    }

    #[tokio::test]
    async fn at_most_five_pushes_when_nothing_is_approved() {
        let many: Vec<Value> = (1..=8).map(|n| rel(&format!("Kimetsu.no.Yaiba.Mugenjou-hen.2025.1080p-G{n}"), n)).collect();
        let w = world(json!([infinity_castle(1)]));
        *w.search.lock().unwrap() = ok(Value::Array(many));
        let mut r = start(w, false).await;
        r.tick(T0).await;
        let p = r.pushes();
        assert_eq!(p.len(), MAX_PUSHES);
        assert_eq!(p[0], "Kimetsu.no.Yaiba.Mugenjou-hen.2025.1080p-G8", "most seeders first");
    }

    #[tokio::test]
    async fn skipped_candidates_are_never_searched() {
        let mut ms = Vec::new();
        for (id, k, v) in [
            (1, "hasFile", json!(true)),
            (2, "isAvailable", json!(false)),
            (3, "added", json!("2026-10-26T07:10:00Z")),
            (4, "monitored", json!(true)), // queued
            (5, "monitored", json!(true)), // grabbed before
        ] {
            let mut m = infinity_castle(id);
            m[k] = v;
            ms.push(m);
        }
        let mut w = world(Value::Array(ms));
        w.queue = json!({"totalRecords": 1, "records": [{"movieId": 4}]});
        w.grabbed = HashSet::from([5]);
        let mut r = start(w, false).await;
        r.tick(T0).await;
        assert!(r.searches().is_empty());
        assert!(r.f.mutations().is_empty());
        // The grabbed one was looked up once and is not looked up again for 6 h.
        assert_eq!(r.f.find("GET", "/api/v3/history/movie").len(), 1);
        r.tick(T0 + 180.0).await;
        assert_eq!(r.f.find("GET", "/api/v3/history/movie").len(), 1);
    }

    #[tokio::test]
    async fn one_attempt_per_six_hours() {
        let w = world(json!([infinity_castle(1)])); // nothing approved
        let mut r = start(w, false).await;
        r.tick(T0).await;
        let n = r.searches().len();
        assert_eq!(n, 2);
        r.tick(T0 + 180.0).await;
        r.tick(T0 + RETRY_EVERY - 1.0).await;
        assert_eq!(r.searches().len(), n);
        r.tick(T0 + RETRY_EVERY).await;
        assert_eq!(r.searches().len(), 2 * n);
    }

    #[tokio::test]
    async fn dry_run_makes_no_post() {
        let mut w = world(json!([infinity_castle(1)]));
        w.approve = vec![MUGEN_1080];
        let mut r = start(w, true).await;
        r.tick(T0).await;
        assert_eq!(r.searches().len(), 2);
        assert!(r.f.mutations().is_empty(), "{:?}", r.f.mutations());
    }

    #[tokio::test]
    async fn prowlarr_error_pushes_nothing_and_retries_next_tick() {
        for bad in [status(500), ok(json!({"not": "an array"})), (200, "nope".into(), vec![])] {
            let w = world(json!([infinity_castle(1), infinity_castle(2)]));
            *w.search.lock().unwrap() = bad;
            let mut r = start(w, false).await;
            r.tick(T0).await;
            assert!(r.f.mutations().is_empty());
            assert_eq!(r.searches().len(), 1, "the step stops at the first error");
            assert!(r.tried.is_empty(), "not counted as an attempt");
        }
    }

    #[tokio::test]
    async fn radarr_errors_skip_the_step() {
        let mut w = world(json!([infinity_castle(1)]));
        w.queue = json!({"records": []}); // no totalRecords: a paging error
        let mut r = start(w, false).await;
        r.tick(T0).await;
        assert!(r.searches().is_empty());
        let mut r = start(world(json!({"bad": 1})), false).await;
        r.tick(T0).await;
        assert!(r.searches().is_empty());
        assert!(r.f.find("GET", "/api/v3/queue").is_empty());
    }

    #[tokio::test]
    async fn at_most_two_movies_per_tick() {
        let w = world(json!([infinity_castle(1), infinity_castle(2), infinity_castle(3)]));
        let mut r = start(w, false).await;
        r.tick(T0).await;
        assert_eq!(r.tried.len(), MAX_MOVIES);
        r.tick(T0 + 180.0).await;
        assert_eq!(r.tried.len(), 3);
    }
}
