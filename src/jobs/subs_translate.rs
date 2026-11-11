//! The translation of [`crate::jobs::subs`]: an episode or movie gets a Romanian subtitle
//! translated by Bazarr's own translator (Gemini) from an English one.
//!
//! Two ways in:
//! - **Manual** (the default): the user asks for it in Seriale / Filme. The queue, its API
//!   and its processing live in [`crate::jobs::subs_manual`]; manual requests go before
//!   anything else and need no prior provider search.
//! - **Automatic** (`jobs.subs.auto_translate`, default off; the old `jobs.subs.translate`
//!   key is read as its alias): an item still missing `ro` after at least one paced provider
//!   search.
//!
//! For both:
//! - Only when Bazarr's `translator.translator_type` is `gemini` and `translator.gemini_keys`
//!   is not empty (`GET /api/system/settings`; nothing else of it is read, never logged).
//!   Google Translate gets rate-limited and gave nothing: it is not used.
//! - At most one translation in flight (persisted, given up after [`JOB_TIMEOUT`]) and none
//!   while Bazarr runs any translation job (`GET /api/system/jobs`, its queue cannot cancel
//!   a running job). A daily cap (`jobs.subs.translate_per_day`, default 40, per local day,
//!   manual and automatic together) keeps the Gemini free tier (500/day) safe.
//! - Automatic candidates: wanted items that already have a `subs_search` attempt, episodes
//!   in the search order (next unwatched first), movies unwatched and newest Radarr id first
//!   (the one just downloaded is likely watched soon); when both are due, episode and movie
//!   take turns ([`pick_kind`]).
//! - The English source, best first ([`external_en`], [`pick_track`]): `<video>.en.srt` or
//!   `.eng.srt` next to the video, as is; else the embedded English text track (ass, ssa,
//!   subrip, mov_text; not Signs/Songs/Forced), extracted by ffmpeg to stdout and cleaned
//!   ([`ass_to_cues`], [`srt_to_cues`]) into `<video>.en.srt` (temp + rename, in the video's
//!   folder). None: the item waits 24 h.
//! - Then `PATCH` scan-disk (Bazarr must index the English file, else the translate call is
//!   a 400 "Invalid source language code") and `PATCH /api/subtitles?action=translate`.
//!   Bazarr creates an empty `<video>.ro.srt` at once and fills it when the job is done.
//! - Later ticks ([`judge`]): done and non-empty -> its `# Subtitles translated with ... #`
//!   header cue is removed ([`strip_header`]), Bazarr rescans, Plex refreshes the folder, ntfy
//!   (`jobs.subs.translate_notify`, default on). Failed or timed out -> the empty `.ro.srt` is
//!   deleted (else Bazarr would count it as present); an automatic item waits 1 h, 6 h, then
//!   24 h, a manual one is reported (page + ntfy) and left to the user.
//! - Never overwrites a non-empty `.ro.srt`, never touches the video, writes only in its
//!   folder. Dry-run: only logged, nothing written, no PATCH.
//! - Plex sees the new `.ro.srt` but keeps playing the default (English) track, so the video
//!   goes on a persisted list ([`RoSelect`], also fed by convert and strip-dv when they
//!   rewrite a video that has a `.ro.srt`, see [`ro_select_after_rewrite`]). Every paced tick
//!   ([`select_ro`]) looks each one up in Plex (one library listing per section): no subtitle
//!   chosen yet and a `ron` stream there -> `PUT /library/parts/<id>?subtitleStreamID=..`;
//!   RO already chosen or another subtitle chosen (the user's choice, never overridden) ->
//!   dropped; not in Plex yet or no `ron` stream yet -> tried again next tick, given up after
//!   [`RO_SELECT_MAX_AGE`].
//! - The same list takes the RO subtitles Bazarr got from a provider (its own search on
//!   import or the paced one): every paced tick, before [`select_ro`], [`ro_from_history`]
//!   reads Bazarr's newest history records (`GET /api/episodes/history` and
//!   `/api/movies/history?start=0&length=100`). A non-forced `ro` record that is a download,
//!   manual pick, upgrade or upload ([`HISTORY_ACTIONS`]; not a translation, queued above)
//!   and not seen before ([`RoHistory`], persisted as `ro_history`; the first run looks back a
//!   day only) puts the video next to its `subtitles_path` ([`video_for_sub`]) on the list,
//!   in the TV or movie section. Bazarr down: logged once, the cursor stays. No Plex token:
//!   not read. Dry-run: listed and logged, the selection only logged.

use crate::app::AppState;
use crate::jobs::common::{ffmpeg_bin, job_cfg, quiet_command, run_capture, unix_now, Quiet};
use crate::jobs::subs::Cand;
use crate::services::arr_jobs::urlencode;
use crate::services::plex_jobs::{path_key, PartRef, PlexJobs, SubStream};
use crate::{log_info, log_warning};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// `state.local.json` key: `{"day", "count", "last", "items": {key: {"tries", "next", "why"}},
/// "inflight": {...} | null, "ro_select": [{"path", "section", "movie", "since"}],
/// "ro_history": {"ep": [mark] | null, "movie": [mark] | null}}`.
pub const TR_KEY: &str = "subs_translate";
/// A video still not found in Plex with a RO stream after this is dropped from the list.
pub const RO_SELECT_MAX_AGE: f64 = 24.0 * 3600.0;
/// A video is looked up in Plex only this long after it went on the list (Plex rescans the
/// folder on its own time; a rewritten file must not be judged on its old streams).
pub const RO_SELECT_SETTLE: f64 = 60.0;
/// Video extensions next to which Bazarr writes `<stem>.ro.srt` ([`video_of`]).
const VIDEO_EXTS: [&str; 9] = ["mkv", "mp4", "m4v", "avi", "ts", "m2ts", "mov", "wmv", "webm"];
/// Waits after the 1st, 2nd and every later failed translation of the same item.
pub const TR_BACKOFF: [f64; 3] = [3600.0, 6.0 * 3600.0, 24.0 * 3600.0];
/// No English source (or no video file): look again after a day.
pub const NO_SOURCE_WAIT: f64 = 24.0 * 3600.0;
/// Translated (or a `.ro.srt` was already there): not again for a day, dropped once Bazarr
/// no longer lists the item as wanted.
pub const DONE_WAIT: f64 = 24.0 * 3600.0;
/// A translation still not done after this is given up.
pub const JOB_TIMEOUT: f64 = 15.0 * 60.0;
pub const DEFAULT_PER_DAY: u32 = 40;
/// Styles that are no dialogue: OP/ED (karaoke, both as `OP1 - ENG A` and `ED_ROM`), signs,
/// the preview, titles, songs, credits, logos. `OP`/`ED` only as a word of their own, so
/// `Default - Top` stays.
pub const EXCLUDE_STYLES: &str =
    r"(?i)(?:^|[^a-z])(?:op|ed|opening|ending)(?:[^a-z]|$)|sign|preview|title|song|karaoke|insert|credit|logo";
/// The fallback when [`EXCLUDE_STYLES`] leaves too little (see [`ass_to_cues`]): keep only
/// styles that look like dialogue.
pub const DIALOGUE_STYLES: &str = r"(?i)default|main|dialog|italic|flashback|thought|narrat";
/// The exclusion filter is trusted if it keeps at least this many cues and this share of all.
pub const MIN_CUES: usize = 50;
pub const MIN_SHARE_PERCENT: usize = 30;
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
const SCAN_TIMEOUT: Duration = Duration::from_secs(120);
const TRANSLATE_TIMEOUT: Duration = Duration::from_secs(60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(120);
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(300);

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("regex"))
}

fn exclude_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, EXCLUDE_STYLES)
}

fn dialogue_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, DIALOGUE_STYLES)
}

/// `{...}` override blocks (ASS, and `{\an8}` in SRT).
fn brace_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"\{[^}]*\}")
}

fn font_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"(?i)</?font[^>]*>")
}

/// A vector drawing (`{\p1}`); `\pos` does not match.
fn drawing_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"\\p[1-9]")
}

// -- subtitle text -----------------------------------------------------------------------

/// One subtitle line, times in milliseconds.
#[derive(Debug, Clone, PartialEq)]
pub struct Cue {
    pub start: u64,
    pub end: u64,
    pub text: String,
}

/// `00:00:06,310`.
pub fn srt_time(ms: u64) -> String {
    format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// `0:00:06.31` (ASS, centiseconds) or `00:00:06,310` (SRT) -> milliseconds.
pub fn parse_time(s: &str) -> Option<u64> {
    let mut parts = s.trim().split(':');
    let (h, m, rest) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let (sec, frac) = rest.split_once(['.', ',']).unwrap_or((rest, ""));
    let n = |x: &str| x.trim().parse::<u64>().ok();
    let frac = frac.trim();
    let ms = if frac.is_empty() {
        0
    } else {
        if !frac.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        // Scale the first three digits to milliseconds: "31" -> 310, "5" -> 500.
        let d: String = frac.chars().chain("000".chars()).take(3).collect();
        d.parse::<u64>().ok()?
    };
    Some(n(h)? * 3_600_000 + n(m)? * 60_000 + n(sec)? * 1000 + ms)
}

/// Text lines trimmed, empty ones dropped.
fn tidy(s: &str) -> String {
    s.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n")
}

/// ASS dialogue text -> plain text: override blocks gone, `\N`/`\n` new lines, `\h` a space.
pub fn clean_ass_text(t: &str) -> String {
    let s = brace_re().replace_all(t, "");
    tidy(&s.replace("\\N", "\n").replace("\\n", "\n").replace("\\h", " "))
}

/// SRT text without `<font ...>`/`</font>` and `{\an8}`-style tags (`<i>` and `<b>` stay).
pub fn clean_srt_text(t: &str) -> String {
    let s = brace_re().replace_all(t, "");
    tidy(&font_re().replace_all(&s, ""))
}

fn strip_prefix_ci<'a>(l: &'a str, p: &str) -> Option<&'a str> {
    l.get(..p.len()).filter(|h| h.eq_ignore_ascii_case(p)).map(|_| &l[p.len()..])
}

/// One `Dialogue:` event.
#[derive(Debug, Clone)]
struct AssEvent {
    style: String,
    start: u64,
    end: u64,
    raw: String,
    text: String,
}

/// The `Dialogue:` events of the `[Events]` section, by its `Format:` line (the text is the
/// last field and may hold commas). `Comment:` lines and unparsable times are skipped.
fn ass_events(text: &str) -> Vec<AssEvent> {
    let mut format: Vec<String> =
        ["layer", "start", "end", "style", "name", "marginl", "marginr", "marginv", "effect", "text"].map(String::from).to_vec();
    let mut in_events = false;
    let mut out = Vec::new();
    for line in text.lines() {
        let l = line.trim_start_matches('\u{feff}').trim();
        if l.starts_with('[') {
            in_events = l.eq_ignore_ascii_case("[events]");
            continue;
        }
        if !in_events {
            continue;
        }
        if let Some(f) = strip_prefix_ci(l, "format:") {
            format = f.split(',').map(|x| x.trim().to_ascii_lowercase()).collect();
            continue;
        }
        let Some(rest) = strip_prefix_ci(l, "dialogue:") else { continue };
        let parts: Vec<&str> = rest.splitn(format.len(), ',').collect();
        if parts.len() < format.len() {
            continue;
        }
        let field = |n: &str| format.iter().position(|f| f == n).map(|i| parts[i]);
        let (Some(start), Some(end), Some(raw)) =
            (field("start").and_then(parse_time), field("end").and_then(parse_time), field("text"))
        else {
            continue;
        };
        let style = field("style").unwrap_or("").trim().trim_start_matches('*').to_string();
        out.push(AssEvent { style, start, end, raw: raw.to_string(), text: clean_ass_text(raw) });
    }
    out
}

/// The dialogue of an ASS/SSA script, sorted by start:
/// 1. events with text (no vector drawings), whose style does not match [`EXCLUDE_STYLES`];
/// 2. if that keeps fewer than [`MIN_CUES`] cues or under [`MIN_SHARE_PERCENT`] % of all
///    events (the real dialogue style was probably excluded, or the styles are odd), only the
///    styles matching [`DIALOGUE_STYLES`] instead (unless none match);
/// 3. in any style but the dominant one (the most events), events positioned with
///    `\pos`/`\move` are signs and go;
/// 4. exact duplicates (same times and text, e.g. a shadow layer) are dropped.
pub fn ass_to_cues(text: &str) -> Vec<Cue> {
    let all: Vec<AssEvent> = ass_events(text).into_iter().filter(|e| !e.text.is_empty() && !drawing_re().is_match(&e.raw)).collect();
    let mut pick: Vec<&AssEvent> = all.iter().filter(|e| !exclude_re().is_match(&e.style)).collect();
    if pick.len() < MIN_CUES || pick.len() * 100 < all.len() * MIN_SHARE_PERCENT {
        let fallback: Vec<&AssEvent> = all.iter().filter(|e| dialogue_re().is_match(&e.style)).collect();
        if !fallback.is_empty() {
            pick = fallback;
        }
    }
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for e in &pick {
        *counts.entry(e.style.as_str()).or_default() += 1;
    }
    let dominant = counts.iter().max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0))).map(|(s, _)| s.to_string()).unwrap_or_default();
    let positioned = |e: &AssEvent| e.raw.contains("\\pos(") || e.raw.contains("\\move(");
    let mut seen = HashSet::new();
    let mut cues: Vec<Cue> = pick
        .into_iter()
        .filter(|e| e.style == dominant || !positioned(e))
        .filter(|e| seen.insert((e.start, e.end, e.text.clone())))
        .map(|e| Cue { start: e.start, end: e.end, text: e.text.clone() })
        .collect();
    cues.sort_by_key(|c| c.start);
    cues
}

/// SRT blocks: (timing line, text lines). Index lines are dropped; a block without `-->` is
/// text that belongs to the cue before it.
fn srt_blocks(text: &str) -> Vec<(String, Vec<String>)> {
    let text = text.trim_start_matches('\u{feff}').lines().map(str::trim_end).collect::<Vec<_>>().join("\n");
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for block in text.split("\n\n").map(|b| b.trim_matches('\n')) {
        let lines: Vec<&str> = block.lines().collect();
        match lines.iter().position(|l| l.contains("-->")) {
            Some(i) => out.push((lines[i].trim().to_string(), lines[i + 1..].iter().map(|l| l.to_string()).collect())),
            None => {
                if let Some(last) = out.last_mut() {
                    last.1.extend(lines.iter().filter(|l| !l.trim().is_empty()).map(|l| l.to_string()));
                }
            }
        }
    }
    out
}

/// An SRT (ffmpeg's conversion of subrip/mov_text) -> cues, tags cleaned, sorted by start.
pub fn srt_to_cues(text: &str) -> Vec<Cue> {
    let mut cues: Vec<Cue> = srt_blocks(text)
        .into_iter()
        .filter_map(|(timing, lines)| {
            let (a, b) = timing.split_once("-->")?;
            // `00:00:01,000 --> 00:00:02,000 X1:..` (position) keeps only the time.
            let end = b.split_whitespace().next()?;
            let text = clean_srt_text(&lines.join("\n"));
            (!text.is_empty()).then_some(Cue { start: parse_time(a)?, end: parse_time(end)?, text })
        })
        .collect();
    cues.sort_by_key(|c| c.start);
    cues
}

/// Numbered SRT, `\n` line ends, no BOM.
pub fn cues_to_srt(cues: &[Cue]) -> String {
    let mut s = String::new();
    for (i, c) in cues.iter().enumerate() {
        s.push_str(&format!("{}\n{} --> {}\n{}\n\n", i + 1, srt_time(c.start), srt_time(c.end), c.text));
    }
    s
}

/// The English SRT to write for an extracted track (`ass`: ASS/SSA, else SRT) and its cue
/// count; None if no dialogue is left.
pub fn english_srt(text: &str, ass: bool) -> Option<(String, usize)> {
    let cues = if ass { ass_to_cues(text) } else { srt_to_cues(text) };
    (!cues.is_empty()).then(|| (cues_to_srt(&cues), cues.len()))
}

/// Bazarr's `# Subtitles translated with <model> #` cue.
pub fn is_header(text: &str) -> bool {
    let t = text.trim();
    t.len() > 1 && t.starts_with('#') && t.ends_with('#') && t.to_lowercase().contains("translated with")
}

/// The SRT without Bazarr's header cue, renumbered, with its BOM and line ends kept; None if
/// there is no such cue (leave the file alone).
pub fn strip_header(text: &str) -> Option<String> {
    let blocks = srt_blocks(text);
    let kept: Vec<&(String, Vec<String>)> = blocks.iter().filter(|(_, l)| !is_header(&l.join("\n"))).collect();
    if kept.len() == blocks.len() {
        return None;
    }
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut s = String::new();
    if text.starts_with('\u{feff}') {
        s.push('\u{feff}');
    }
    for (i, (timing, lines)) in kept.iter().enumerate() {
        s.push_str(&format!("{}{nl}{timing}{nl}", i + 1));
        for l in lines.iter() {
            s.push_str(l.trim_end());
            s.push_str(nl);
        }
        s.push_str(nl);
    }
    Some(s)
}

// -- the English source ------------------------------------------------------------------

/// `<dir>\<video stem>.<suffix>`.
pub fn sibling(video: &Path, suffix: &str) -> PathBuf {
    let stem = video.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    video.with_file_name(format!("{stem}.{suffix}"))
}

/// The video whose [`sibling`] `ro.srt` is `ro` (a video file in the same folder), if any.
pub fn video_of(ro: &Path) -> Option<PathBuf> {
    let want = path_key(&ro.to_string_lossy());
    fs::read_dir(ro.parent()?)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()).is_some_and(|x| VIDEO_EXTS.contains(&x.to_ascii_lowercase().as_str())))
        .find(|p| p.is_file() && path_key(&sibling(p, &format!("{}.srt", crate::jobs::subs::LANGUAGE)).to_string_lossy()) == want)
}

fn non_empty(p: &Path) -> bool {
    fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() > 0)
}

/// An English SRT next to the video (`.en.srt`, then `.eng.srt`), not empty.
pub fn external_en(video: &Path) -> Option<PathBuf> {
    ["en.srt", "eng.srt"].iter().map(|s| sibling(video, s)).find(|p| non_empty(p))
}

/// An embedded English text track: `(stream index, codec)`. Not ass/ssa/subrip/mov_text,
/// forced, or titled Signs/Songs/Forced: skipped. Several: the most events (mkvmerge's
/// `NUMBER_OF_FRAMES` tag), then the most bytes, then the first.
pub fn pick_track(probe: &Value) -> Option<(i64, String)> {
    let tag = |s: &Value, name: &str| -> Option<String> {
        s.get("tags")?.as_object()?.iter().find(|(k, _)| k.to_ascii_uppercase().starts_with(name)).and_then(|(_, v)| v.as_str().map(String::from))
    };
    let count = |s: &Value, name: &str| tag(s, name).and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0);
    probe
        .get("streams")?
        .as_array()?
        .iter()
        .filter(|s| s.get("codec_type").and_then(Value::as_str) == Some("subtitle"))
        .filter_map(|s| {
            let codec = s.get("codec_name").and_then(Value::as_str)?.to_ascii_lowercase();
            if !matches!(codec.as_str(), "ass" | "ssa" | "subrip" | "srt" | "mov_text") {
                return None;
            }
            let lang = tag(s, "LANGUAGE").unwrap_or_default().to_ascii_lowercase();
            if lang != "eng" && lang != "en" {
                return None;
            }
            let title = tag(s, "TITLE").unwrap_or_default().to_lowercase();
            if ["sign", "song", "forced"].iter().any(|w| title.contains(w)) {
                return None;
            }
            if s.get("disposition").and_then(|d| d.get("forced")).and_then(Value::as_i64) == Some(1) {
                return None;
            }
            let index = s.get("index").and_then(Value::as_i64)?;
            Some((count(s, "NUMBER_OF_FRAMES"), count(s, "NUMBER_OF_BYTES"), index, codec))
        })
        .max_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)).then(b.2.cmp(&a.2)))
        .map(|(_, _, i, c)| (i, c))
}

/// ffprobe/ffmpeg, behind a trait so tests can fake them.
pub trait SubTools: Send + Sync {
    /// `ffprobe -v error -show_streams -of json <file>`.
    fn probe(&self, file: &Path) -> Result<Value, String>;
    /// Stream `index` as text: ASS when `ass`, else SRT (ffmpeg to stdout, nothing written).
    fn extract(&self, file: &Path, index: i64, ass: bool) -> Result<String, String>;
}

/// The real tools (below-normal priority, no window, tied to the app's Job Object).
pub struct FfTools {
    pub bin: PathBuf,
}

impl SubTools for FfTools {
    fn probe(&self, file: &Path) -> Result<Value, String> {
        let mut c = quiet_command(&self.bin.join("ffprobe.exe"));
        c.args(["-v", "error", "-show_streams", "-of", "json"]).arg(file);
        let (_code, out, _err) = run_capture(c, PROBE_TIMEOUT, &|| false)?;
        serde_json::from_slice(&out).map_err(|_| "ffprobe: raspuns JSON invalid".to_string())
    }

    fn extract(&self, file: &Path, index: i64, ass: bool) -> Result<String, String> {
        let mut c = quiet_command(&self.bin.join("ffmpeg.exe"));
        c.args(["-v", "error", "-nostdin", "-i"]).arg(file).args(["-map", &format!("0:{index}")]);
        c.args(if ass { ["-c:s", "ass", "-f", "ass"] } else { ["-c:s", "srt", "-f", "srt"] }).arg("pipe:1");
        let (code, out, _err) = run_capture(c, EXTRACT_TIMEOUT, &|| false)?;
        if code != 0 {
            return Err(format!("ffmpeg exit {code}"));
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

/// The installed ffmpeg (the convert job's settings), if any.
pub fn real_tools(app: &AppState) -> Option<Arc<dyn SubTools>> {
    let cfg = crate::jobs::convert::Settings::from_app(app);
    ffmpeg_bin(&cfg.ffmpeg_bin, &cfg.ffmpeg_packages).map(|bin| Arc::new(FfTools { bin }) as Arc<dyn SubTools>)
}

/// Write `bytes` to `path` via `<path>.tmp` in the same folder and a rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let r = fs::write(&tmp, bytes).and_then(|_| fs::rename(&tmp, path));
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

// -- Bazarr ------------------------------------------------------------------------------

/// Bazarr's translator is Gemini with at least one key (`/api/system/settings`; the body or
/// its `data`). Nothing else of the settings is looked at.
pub fn translator_ready(settings: &Value) -> bool {
    let tr = settings.get("translator").or_else(|| settings.get("data").and_then(|d| d.get("translator")));
    let Some(tr) = tr else { return false };
    let gemini = tr.get("translator_type").and_then(Value::as_str).is_some_and(|t| t.trim().eq_ignore_ascii_case("gemini"));
    let keys = match tr.get("gemini_keys") {
        Some(Value::Array(a)) => a.iter().any(|k| k.as_str().is_some_and(|s| !s.trim().is_empty())),
        Some(Value::String(s)) => !s.trim().is_empty(),
        _ => false,
    };
    gemini && keys
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JobState {
    Running,
    Completed,
    Failed,
}

/// A Bazarr job's `status`: completed, failed, anything else (pending, running) is running.
pub fn job_state(j: &Value) -> JobState {
    match j.get("status").and_then(Value::as_str).unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "completed" | "complete" | "done" | "finished" | "success" => JobState::Completed,
        "failed" | "error" | "cancelled" | "canceled" => JobState::Failed,
        _ => JobState::Running,
    }
}

/// The state of the job translating `en` (its `progress_message` is that path); several (an
/// older attempt): running before completed before failed.
pub fn find_job(jobs: &[Value], en: &str) -> Option<JobState> {
    let want = path_key(en);
    let name = Path::new(en).file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let rank = |s: JobState| match s {
        JobState::Running => 0,
        JobState::Completed => 1,
        JobState::Failed => 2,
    };
    jobs.iter()
        .filter(|j| {
            let m = j.get("progress_message").and_then(Value::as_str).unwrap_or("");
            path_key(m) == want || (!name.is_empty() && m.to_lowercase().contains(&name))
        })
        .map(job_state)
        .min_by_key(|s| rank(*s))
}

/// Bazarr is running (or has queued) some translation job, whoever started it.
pub fn translation_running(jobs: &[Value]) -> bool {
    jobs.iter().any(|j| {
        j.get("job_name").and_then(Value::as_str).is_some_and(|n| n.to_lowercase().contains("translat")) && job_state(j) == JobState::Running
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Wait,
    Done,
    Failed(&'static str),
}

/// What became of our translation: `jobs` is Bazarr's job list (None: it did not answer),
/// `ro_len` the `.ro.srt` size now (0 if missing).
/// - its job completed: done if the file has text, else failed;
/// - its job failed: failed;
/// - running: wait, failed after [`JOB_TIMEOUT`];
/// - not listed (Bazarr pruned it) or no list: done if the file has text (only with a list,
///   or at the timeout), else wait until the timeout.
pub fn judge(jobs: Option<&[Value]>, en: &str, ro_len: u64, started: f64, now: f64) -> Verdict {
    let late = now - started > JOB_TIMEOUT;
    match jobs.and_then(|j| find_job(j, en)) {
        Some(JobState::Completed) if ro_len > 0 => Verdict::Done,
        Some(JobState::Completed) => Verdict::Failed("Bazarr a terminat, dar fișierul e gol"),
        Some(JobState::Failed) => Verdict::Failed("job eșuat în Bazarr"),
        Some(JobState::Running) if late => Verdict::Failed("timeout"),
        Some(JobState::Running) => Verdict::Wait,
        None if ro_len > 0 && (jobs.is_some() || late) => Verdict::Done,
        None if late => Verdict::Failed("timeout"),
        None => Verdict::Wait,
    }
}

// -- state -------------------------------------------------------------------------------

/// One item's translation memory.
#[derive(Debug, Clone, PartialEq)]
pub struct TrItem {
    /// Failed translations so far.
    pub tries: u32,
    /// Unix time before which it is not tried again.
    pub next: f64,
    /// `failed`, `no_source`, `no_file`, `has_ro` or `done` (for whoever reads the file).
    pub why: String,
}

/// The translation we started and wait for.
#[derive(Debug, Clone, PartialEq)]
pub struct InFlight {
    pub key: String,
    pub series_id: i64,
    pub episode_id: i64,
    pub movie_id: i64,
    pub name: String,
    pub en: String,
    pub ro: String,
    pub started: f64,
    pub tries: u32,
    /// Asked for by the user ([`crate::jobs::subs_manual`]): a failure is reported, no backoff.
    pub manual: bool,
}

impl InFlight {
    fn to_json(&self) -> Value {
        json!({"key": self.key, "series_id": self.series_id, "episode_id": self.episode_id, "movie_id": self.movie_id,
               "name": self.name, "en": self.en, "ro": self.ro, "started": self.started, "tries": self.tries, "manual": self.manual})
    }

    fn from_json(v: &Value) -> Option<InFlight> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
        let i = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
        Some(InFlight {
            key: s("key")?,
            series_id: i("series_id"),
            episode_id: i("episode_id"),
            movie_id: i("movie_id"),
            name: s("name").unwrap_or_default(),
            en: s("en")?,
            ro: s("ro")?,
            started: v.get("started")?.as_f64()?,
            tries: i("tries") as u32,
            manual: v.get("manual").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    fn cand(&self) -> Cand {
        Cand { key: self.key.clone(), series_id: self.series_id, episode_id: self.episode_id, movie_id: self.movie_id, title: self.name.clone(), season: 0, episode: 0 }
    }
}

/// Everything the translation keeps between ticks (and in `state.local.json`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrState {
    pub items: HashMap<String, TrItem>,
    /// The local day `count` is for (`YYYY-MM-DD`).
    pub day: String,
    pub count: u32,
    /// The kind of the last one started (`ep` / `movie`), to take turns.
    pub last: String,
    pub inflight: Option<InFlight>,
    /// Videos whose RO subtitle Plex should be told to play ([`select_ro`]), one per
    /// [`path_key`].
    pub ro_select: Vec<RoSelect>,
    /// How far Bazarr's subtitle history has been read ([`ro_from_history`]).
    pub ro_history: RoHistory,
}

/// The cursor into Bazarr's subtitle history, per kind: the marks ([`history_mark`]) of the
/// newest records read last time. `None`: never read (the first run looks back a day only).
/// Bazarr's history API gives neither a record id nor a numeric time (see
/// [`ro_from_history`]), so the cursor is the set of records already seen.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RoHistory {
    pub ep: Option<Vec<String>>,
    pub movie: Option<Vec<String>>,
}

impl RoHistory {
    fn to_json(&self) -> Value {
        let side = |m: &Option<Vec<String>>| m.as_ref().map_or(Value::Null, |v| json!(v));
        json!({"ep": side(&self.ep), "movie": side(&self.movie)})
    }

    /// Missing or broken (a state file from before the cursor): never read.
    fn from_json(v: Option<&Value>) -> RoHistory {
        let side = |k: &str| -> Option<Vec<String>> {
            v.and_then(|v| v.get(k)).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
        };
        RoHistory { ep: side("ep"), movie: side("movie") }
    }
}

/// A video whose `.ro.srt` Plex should select: its Plex section, movie or episode (which
/// listing finds it), when it went on the list.
#[derive(Debug, Clone, PartialEq)]
pub struct RoSelect {
    pub path: String,
    pub section: u32,
    pub movie: bool,
    pub since: f64,
}

impl RoSelect {
    fn to_json(&self) -> Value {
        json!({"path": self.path, "section": self.section, "movie": self.movie, "since": self.since})
    }

    fn from_json(v: &Value) -> Option<RoSelect> {
        Some(RoSelect {
            path: v.get("path")?.as_str().filter(|p| !p.is_empty())?.to_string(),
            section: v.get("section")?.as_u64()? as u32,
            movie: v.get("movie").and_then(Value::as_bool).unwrap_or(false),
            since: v.get("since")?.as_f64()?,
        })
    }
}

impl TrState {
    pub fn from_json(v: Option<&Value>) -> TrState {
        let Some(v) = v else { return TrState::default() };
        let items = v
            .get("items")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, a)| {
                        let why = a.get("why").and_then(Value::as_str).unwrap_or("").to_string();
                        Some((k.clone(), TrItem { tries: a.get("tries")?.as_u64()? as u32, next: a.get("next")?.as_f64()?, why }))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut tr = TrState {
            items,
            day: v.get("day").and_then(Value::as_str).unwrap_or("").to_string(),
            count: v.get("count").and_then(Value::as_u64).unwrap_or(0) as u32,
            last: v.get("last").and_then(Value::as_str).unwrap_or("").to_string(),
            inflight: v.get("inflight").and_then(InFlight::from_json),
            ro_select: Vec::new(),
            ro_history: RoHistory::from_json(v.get("ro_history")),
        };
        // Missing (a state file from before the list): empty.
        for e in v.get("ro_select").and_then(Value::as_array).into_iter().flatten().filter_map(RoSelect::from_json) {
            tr.want_ro_select(e);
        }
        tr
    }

    pub fn to_json(&self) -> Value {
        let items: Map<String, Value> =
            self.items.iter().map(|(k, x)| (k.clone(), json!({"tries": x.tries, "next": x.next, "why": x.why}))).collect();
        json!({"day": self.day, "count": self.count, "last": self.last, "items": items,
               "inflight": self.inflight.as_ref().map_or(Value::Null, InFlight::to_json),
               "ro_select": self.ro_select.iter().map(RoSelect::to_json).collect::<Vec<_>>(),
               "ro_history": self.ro_history.to_json()})
    }

    /// Put `e` on the RO-select list; a video already there (same [`path_key`]) takes the new
    /// section and kind and the later `since`.
    pub fn want_ro_select(&mut self, e: RoSelect) {
        let key = path_key(&e.path);
        match self.ro_select.iter_mut().find(|x| path_key(&x.path) == key) {
            Some(x) => {
                x.section = e.section;
                x.movie = e.movie;
                x.since = x.since.max(e.since);
            }
            None => self.ro_select.push(e),
        }
    }

    /// Not waiting for a backoff and not the one in flight.
    pub fn eligible(&self, key: &str, now: f64) -> bool {
        self.items.get(key).is_none_or(|x| x.next <= now) && self.inflight.as_ref().is_none_or(|f| f.key != key)
    }

    pub fn used_today(&self, today: &str) -> u32 {
        if self.day == today {
            self.count
        } else {
            0
        }
    }

    pub fn note_started(&mut self, today: &str, movie: bool) {
        if self.day != today {
            self.day = today.to_string();
            self.count = 0;
        }
        self.count += 1;
        self.last = if movie { "movie" } else { "ep" }.to_string();
    }

    /// Forget the items of `prefix` that `still` says are no longer wanted (never the one in
    /// flight); true if any went.
    pub fn prune(&mut self, prefix: &str, still: impl Fn(&str) -> bool) -> bool {
        let before = self.items.len();
        self.items.retain(|k, _| !k.starts_with(prefix) || still(k));
        self.items.len() != before
    }

    pub fn mark(&mut self, key: &str, tries: u32, wait: f64, now: f64, why: &str) {
        self.items.insert(key.to_string(), TrItem { tries, next: now + wait, why: why.to_string() });
    }

    fn failed(&mut self, key: &str, now: f64) {
        let tries = self.items.get(key).map_or(0, |x| x.tries) + 1;
        self.mark(key, tries, tr_backoff(tries), now, "failed");
    }

    pub fn prev_tries(&self, key: &str) -> u32 {
        self.items.get(key).map_or(0, |x| x.tries)
    }
}

/// The wait after the `tries`-th failed translation: 1 h, 6 h, then 24 h.
pub fn tr_backoff(tries: u32) -> f64 {
    TR_BACKOFF[(tries.max(1) as usize - 1).min(TR_BACKOFF.len() - 1)]
}

/// `jobs.subs.auto_translate` (default off; `jobs.subs.translate` is read as its alias when
/// it is missing), `.translate_per_day` (default 40, manual and automatic together),
/// `.translate_notify` (default on), `.movie_section` (default the DV strip's Radarr
/// library, else 1). Manual requests work whatever `auto_translate` says.
#[derive(Debug, Clone)]
pub struct TrSettings {
    /// Pick candidates on its own (after a provider search).
    pub auto: bool,
    pub per_day: u32,
    pub notify: bool,
    pub movie_section: u32,
}

/// `auto_translate`, else the old `translate`, else off (see [`TrSettings`]).
pub fn auto_setting(auto_translate: Option<&Value>, translate: Option<&Value>) -> bool {
    auto_translate.and_then(Value::as_bool).or_else(|| translate.and_then(Value::as_bool)).unwrap_or(false)
}

impl TrSettings {
    pub fn from_app(app: &AppState) -> TrSettings {
        let movie_section = job_cfg(app, "subs", "movie_section").and_then(|v| v.as_u64()).map(|n| n as u32).unwrap_or_else(|| {
            crate::jobs::strip_dv::DvConfig::from_app(app).libraries.iter().find(|l| l.arr == "radarr").map_or(1, |l| l.section)
        });
        TrSettings {
            auto: auto_setting(job_cfg(app, "subs", "auto_translate").as_ref(), job_cfg(app, "subs", "translate").as_ref()),
            per_day: job_cfg(app, "subs", "translate_per_day").and_then(|v| v.as_u64()).map_or(DEFAULT_PER_DAY, |n| n as u32),
            notify: job_cfg(app, "subs", "translate_notify").and_then(|v| v.as_bool()).unwrap_or(true),
            movie_section,
        }
    }
}

/// The local day `YYYY-MM-DD` (the daily cap counts per local day).
pub fn local_today() -> String {
    crate::log::asctime().get(..10).unwrap_or("").to_string()
}

// -- candidates --------------------------------------------------------------------------

/// The items worth translating, in `ranked` order: searched at least once (`searched`),
/// not in a translation backoff or in flight, not `skip` (searched this very tick).
pub fn candidates(ranked: &[Cand], searched: &dyn Fn(&str) -> bool, tr: &TrState, now: f64, skip: Option<&str>) -> Vec<Cand> {
    ranked.iter().filter(|c| searched(&c.key) && tr.eligible(&c.key, now) && Some(c.key.as_str()) != skip).cloned().collect()
}

/// Movies to translate: unwatched (Plex `viewCount` of the file, `views` by [`path_key`]),
/// with a known file, newest Radarr id first. `paths`: Radarr id -> path.
pub fn order_movie_cands(cands: &[Cand], paths: &HashMap<i64, String>, views: &HashMap<String, i64>) -> Vec<Cand> {
    let mut out: Vec<Cand> = cands
        .iter()
        .filter(|c| paths.get(&c.movie_id).is_some_and(|p| !p.is_empty() && views.get(&path_key(p)).copied().unwrap_or(0) == 0))
        .cloned()
        .collect();
    out.sort_by_key(|c| std::cmp::Reverse(c.movie_id));
    out
}

/// Episode or movie: when both are due, the kind not started last.
pub fn pick_kind(ep: Option<Cand>, movie: Option<Cand>, last: &str) -> Option<Cand> {
    match (ep, movie) {
        (Some(e), Some(m)) => Some(if last == "ep" { m } else { e }),
        (e, m) => e.or(m),
    }
}

/// Why no translation may start now, if so (in flight, the daily cap).
pub fn manual_gate(tr: &TrState, cfg: &TrSettings, today: &str) -> Result<(), &'static str> {
    if tr.inflight.is_some() {
        Err("in flight")
    } else if tr.used_today(today) >= cfg.per_day {
        Err("daily cap")
    } else {
        Ok(())
    }
}

/// [`manual_gate`] for the automatic pick, which also needs `auto_translate`.
pub fn local_gate(tr: &TrState, cfg: &TrSettings, today: &str) -> Result<(), &'static str> {
    if !cfg.auto {
        Err("off")
    } else {
        manual_gate(tr, cfg, today)
    }
}

/// The Bazarr side of the gate: its translator is Gemini with a key, and it is not running
/// a translation. Each failure is logged once; the translator answer is remembered for the
/// page ([`crate::jobs::subs_manual::Manual::note_translator`]).
pub async fn bazarr_gate(app: &AppState, quiet: &mut Quiet) -> bool {
    let bz = &app.services.bazarr;
    match bz.get("/api/system/settings", LIST_TIMEOUT).await {
        Ok(v) if translator_ready(&v) => {
            app.subs.note_translator(true);
            quiet.clear("tr-settings");
        }
        Ok(_) => {
            app.subs.note_translator(false);
            quiet.warn("tr-settings", "subs: Bazarr's translator is not Gemini with an API key, no translations".into());
            return false;
        }
        Err(e) => {
            quiet.warn("tr-settings", format!("subs: Bazarr settings failed ({e}), no translations"));
            return false;
        }
    }
    match bz.data("/api/system/jobs", LIST_TIMEOUT).await {
        Ok(j) if translation_running(&j) => {
            quiet.warn("tr-jobs", "subs: Bazarr is translating already, waiting".into());
            false
        }
        Ok(_) => {
            quiet.clear("tr-jobs");
            true
        }
        Err(e) => {
            quiet.warn("tr-jobs", format!("subs: Bazarr jobs failed ({e}), no translations"));
            false
        }
    }
}

fn ascii(s: &str) -> String {
    s.chars().map(|ch| if ch.is_ascii() { ch } else { '?' }).collect()
}

impl Cand {
    /// Bazarr indexes the item's files again.
    pub fn scan_path(&self) -> String {
        if self.movie_id != 0 {
            format!("/api/movies?radarrid={}&action=scan-disk", self.movie_id)
        } else {
            format!("/api/series?seriesid={}&action=scan-disk", self.series_id)
        }
    }

    /// Bazarr translates `en` into `ro` for this item.
    pub fn translate_path(&self, en: &str) -> String {
        let (kind, id) = if self.movie_id != 0 { ("movie", self.movie_id) } else { ("episode", self.episode_id) };
        format!("/api/subtitles?action=translate&language={}&path={}&type={kind}&id={id}", crate::jobs::subs::LANGUAGE, urlencode(en))
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f).await.map_err(|_| "panic".to_string()).and_then(|r| r)
}

/// The time of a tick: unix seconds and the local day (`YYYY-MM-DD`, for the daily cap).
#[derive(Debug, Clone, Copy)]
pub struct When<'a> {
    pub now: f64,
    pub today: &'a str,
}

/// What [`prepare`] came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Prep {
    /// Bazarr translates it now.
    Started(InFlight),
    /// Dry-run: only logged.
    DryRun,
    /// The video file is gone.
    NoFile,
    /// A non-empty `.ro.srt` is there already.
    HasRo,
    /// No English source; the reason for the page.
    NoSource(&'static str),
    /// Something failed on the way; the reason for the page.
    Failed(&'static str),
}

/// Find the English source of `c` (its video at `video`), write it if it is embedded, and
/// ask Bazarr to translate it (the steps in the module docs). The state is not touched: the
/// automatic ([`start`]) and the manual caller each book the outcome their own way.
pub async fn prepare(app: &AppState, quiet: &mut Quiet, tools: Option<Arc<dyn SubTools>>, c: &Cand, video: &str, now: f64, tries: u32) -> Prep {
    let name = ascii(&c.name());
    let video = PathBuf::from(video);
    if !video.is_file() {
        log_info!("subs: the file of {name} is missing, not translated");
        return Prep::NoFile;
    }
    let ro = sibling(&video, &format!("{}.srt", crate::jobs::subs::LANGUAGE));
    if non_empty(&ro) {
        log_info!("subs: {name} already has a RO subtitle file, not translated");
        return Prep::HasRo;
    }
    // The English source: an external file, else the embedded track.
    let (en, text, from) = match external_en(&video) {
        Some(p) => {
            let from = p.file_name().map(|n| ascii(&n.to_string_lossy())).unwrap_or_default();
            (p, None, from)
        }
        None => {
            let Some(tools) = tools else {
                quiet.warn("tr-ffmpeg", "subs: ffmpeg is not installed, only external English subtitles are translated".into());
                return Prep::NoSource("nu are .en.srt, iar ffmpeg nu e instalat");
            };
            let (t, v) = (tools.clone(), video.clone());
            let probe = match blocking(move || t.probe(&v)).await {
                Ok(p) => p,
                Err(e) => {
                    log_warning!("subs: ffprobe of {name} failed: {e}");
                    return Prep::Failed("ffprobe a eșuat");
                }
            };
            let Some((index, codec)) = pick_track(&probe) else {
                log_info!("subs: {name} has no English text subtitle, not translated");
                return Prep::NoSource("nu are subtitrare în engleză");
            };
            let ass = codec == "ass" || codec == "ssa";
            let from = format!("the embedded {codec} track {index}");
            if app.dry_run {
                (sibling(&video, "en.srt"), None, from)
            } else {
                let (t, v) = (tools.clone(), video.clone());
                let raw = match blocking(move || t.extract(&v, index, ass)).await {
                    Ok(r) => r,
                    Err(e) => {
                        log_warning!("subs: extracting the English subtitle of {name} failed: {e}");
                        return Prep::Failed("extragerea subtitrării în engleză a eșuat");
                    }
                };
                let Some(srt) = english_srt(&raw, ass) else {
                    log_info!("subs: the English track of {name} has no dialogue, not translated");
                    return Prep::NoSource("subtitrarea în engleză nu are dialog");
                };
                (sibling(&video, "en.srt"), Some(srt), from)
            }
        }
    };
    if app.dry_run {
        log_info!("subs (dry-run): would translate {name} to RO with Bazarr (Gemini) from {from}");
        return Prep::DryRun;
    }
    if let Some((srt, n)) = text {
        if let Err(e) = write_atomic(&en, srt.as_bytes()) {
            log_warning!("subs: cannot write the English subtitle of {name}: {}", crate::state::io_error_name(&e));
            return Prep::Failed("nu pot scrie subtitrarea în engleză");
        }
        log_info!("subs: wrote an English subtitle for {name} ({n} lines) from {from}");
    }
    let bz = &app.services.bazarr;
    if let Err(e) = bz.patch(&c.scan_path(), SCAN_TIMEOUT).await {
        log_warning!("subs: Bazarr scan-disk for {name} failed: {e}");
        return Prep::Failed("Bazarr nu a putut scana folderul");
    }
    let en_s = en.to_string_lossy().into_owned();
    if let Err(e) = bz.patch(&c.translate_path(&en_s), TRANSLATE_TIMEOUT).await {
        log_warning!("subs: Bazarr translate for {name} failed: {e}");
        return Prep::Failed("Bazarr nu a pornit traducerea");
    }
    log_info!("subs: translating {name} to RO with Bazarr (Gemini) from {from}");
    Prep::Started(InFlight {
        key: c.key.clone(),
        series_id: c.series_id,
        episode_id: c.episode_id,
        movie_id: c.movie_id,
        name: c.name(),
        en: en_s,
        ro: ro.to_string_lossy().into_owned(),
        started: now,
        tries,
        manual: false,
    })
}

/// Start the automatic translation of `c` (its video at `video`) and book the outcome: no
/// file or no English source waits 24 h, a RO file there a day, a failure its backoff.
/// Returns true: the state changed.
pub async fn start(app: &AppState, tr: &mut TrState, quiet: &mut Quiet, tools: Option<Arc<dyn SubTools>>, c: &Cand, video: &str, when: When<'_>) -> bool {
    let When { now, today } = when;
    let prev = tr.prev_tries(&c.key);
    match prepare(app, quiet, tools, c, video, now, prev).await {
        Prep::NoFile => tr.mark(&c.key, prev, NO_SOURCE_WAIT, now, "no_file"),
        Prep::HasRo => tr.mark(&c.key, prev, DONE_WAIT, now, "has_ro"),
        Prep::NoSource(_) => tr.mark(&c.key, prev, NO_SOURCE_WAIT, now, "no_source"),
        Prep::Failed(_) => tr.failed(&c.key, now),
        Prep::DryRun => {
            tr.mark(&c.key, prev + 1, tr_backoff(prev + 1), now, "dry_run");
            tr.note_started(today, c.movie_id != 0);
        }
        Prep::Started(f) => {
            tr.inflight = Some(f);
            tr.note_started(today, c.movie_id != 0);
        }
    }
    true
}

/// Follow the translation in flight. True if it ended (the state changed).
pub async fn poll(app: &AppState, tr: &mut TrState, cfg: &TrSettings, tv_section: u32, now: f64) -> bool {
    let Some(f) = tr.inflight.clone() else { return false };
    let bz = &app.services.bazarr;
    let jobs = bz.data("/api/system/jobs", LIST_TIMEOUT).await.ok();
    let ro = PathBuf::from(&f.ro);
    let ro_len = fs::metadata(&ro).map(|m| m.len()).unwrap_or(0);
    let verdict = judge(jobs.as_deref(), &f.en, ro_len, f.started, now);
    let c = f.cand();
    let name = ascii(&f.name);
    match verdict {
        Verdict::Wait => return false,
        Verdict::Done => {
            // Bazarr's header cue goes (only a file that is valid UTF-8 is rewritten).
            if let Some(clean) = fs::read(&ro).ok().and_then(|b| String::from_utf8(b).ok()).and_then(|t| strip_header(&t)) {
                if let Err(e) = write_atomic(&ro, clean.as_bytes()) {
                    log_warning!("subs: cannot remove the translator line from {name}: {}", crate::state::io_error_name(&e));
                }
            }
            if let Err(e) = bz.patch(&c.scan_path(), SCAN_TIMEOUT).await {
                log_warning!("subs: Bazarr scan-disk after translating {name} failed: {e}");
            }
            if !app.services.keys.get("plex").is_empty() {
                let section = if f.movie_id != 0 { cfg.movie_section } else { tv_section };
                let dir = ro.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
                if let Err(e) = PlexJobs::new(app.services.plex.c.clone()).refresh_path(section, &dir).await {
                    log_warning!("subs: Plex refresh failed: {}", e.type_name());
                }
                // Plex picks the new stream up on its own time; [`select_ro`] makes it the one played.
                if let Some(video) = video_of(&ro) {
                    tr.want_ro_select(RoSelect { path: video.to_string_lossy().into_owned(), section, movie: f.movie_id != 0, since: now });
                }
            }
            log_info!("subs: RO subtitle translated for {name}");
            if cfg.notify {
                let msg = format!("Subtitrare RO tradusă: {}", f.name);
                app.notifier.send("Subtitrări RO", &msg, Some(vec!["speech_balloon".into()]), None, crate::services::ntfy::TIMEOUT).await;
            }
            tr.mark(&f.key, 0, DONE_WAIT, now, "done");
            if f.manual {
                app.subs.record(crate::jobs::subs_manual::Outcome::new(&f.key, &f.name, true, "tradusă", now));
            }
        }
        Verdict::Failed(why) => {
            log_warning!("subs: the RO translation of {name} failed ({why})");
            // The empty file Bazarr made would count as a Romanian subtitle.
            if ro.is_file() && ro_len == 0 && fs::remove_file(&ro).is_ok() {
                if let Err(e) = bz.patch(&c.scan_path(), SCAN_TIMEOUT).await {
                    log_warning!("subs: Bazarr scan-disk for {name} failed: {e}");
                }
            }
            if f.manual {
                // Reported; the user may ask again (no backoff).
                crate::jobs::subs_manual::report_failure(app, cfg, &f.key, &f.name, why, now).await;
            } else {
                let tries = f.tries + 1;
                tr.mark(&f.key, tries, tr_backoff(tries), now, "failed");
            }
        }
    }
    tr.inflight = None;
    // The series screens and movie sheets show the new state at once.
    app.convert.invalidate_list();
    app.services.bazarr.clear_cache();
    true
}

// -- the RO subtitle chosen in Plex --------------------------------------------------------

/// What to do with a part, from its subtitle streams.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RoPick {
    /// No subtitle chosen, a RO stream there: choose this stream id.
    Select(i64),
    /// The RO stream is the chosen one already.
    Done,
    /// Another subtitle is chosen (the user's choice): left as is.
    Skipped,
    /// No RO stream yet (Plex has not seen the file): later.
    NotYet,
}

/// What became of one video on the list this tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RoOutcome {
    Selected,
    Done,
    Skipped,
    /// Kept for the next tick: not in Plex yet, no RO stream yet, or Plex failed.
    NotYet,
}

fn is_ro(s: &SubStream) -> bool {
    matches!(s.language.to_ascii_lowercase().as_str(), "ron" | "rum" | "ro")
}

/// [`RoPick`] for a part's subtitle streams; of several RO streams the external one (our
/// `.ro.srt`) wins.
pub fn ro_pick(subs: &[SubStream]) -> RoPick {
    if let Some(chosen) = subs.iter().find(|s| s.selected) {
        return if is_ro(chosen) { RoPick::Done } else { RoPick::Skipped };
    }
    match subs.iter().find(|s| is_ro(s) && s.external).or_else(|| subs.iter().find(|s| is_ro(s))) {
        Some(s) => RoPick::Select(s.id),
        None => RoPick::NotYet,
    }
}

fn video_name(path: &str) -> String {
    ascii(&Path::new(path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default())
}

/// Convert or strip-dv rewrote `file` (Plex section `section`): if it has a non-empty
/// `.ro.srt`, the subs job selects it in Plex again (the rewrite may reset the choice).
/// Nothing without a Plex token.
pub fn ro_select_after_rewrite(app: &AppState, file: &Path, section: u32, movie: bool) {
    if app.services.keys.get("plex").is_empty() || !non_empty(&sibling(file, &format!("{}.srt", crate::jobs::subs::LANGUAGE))) {
        return;
    }
    app.subs.note_ro_select(RoSelect { path: file.to_string_lossy().into_owned(), section, movie, since: unix_now() });
}

/// One video: find its part in `parts`, read its subtitle streams, choose the RO one.
async fn select_one(app: &AppState, plex: &PlexJobs, quiet: &mut Quiet, parts: &HashMap<String, PartRef>, e: &RoSelect) -> RoOutcome {
    let Some(p) = parts.get(&path_key(&e.path)) else { return RoOutcome::NotYet };
    let name = video_name(&e.path);
    let subs = match plex.part_subtitles(&p.rating_key, p.part_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return RoOutcome::NotYet,
        Err(err) => {
            quiet.warn("ro-select", format!("subs: Plex failed ({}), the RO subtitle is selected later", err.type_name()));
            return RoOutcome::NotYet;
        }
    };
    match ro_pick(&subs) {
        RoPick::Select(_) if app.dry_run => {
            log_info!("subs (dry-run): would select the RO subtitle in Plex for {name}");
            RoOutcome::Selected
        }
        RoPick::Select(stream) => match plex.select_subtitle(p.part_id, stream).await {
            Ok(()) => {
                quiet.clear("ro-select");
                log_info!("subs: selected the RO subtitle in Plex for {name}");
                RoOutcome::Selected
            }
            Err(err) => {
                quiet.warn("ro-select", format!("subs: Plex failed ({}), the RO subtitle is selected later", err.type_name()));
                RoOutcome::NotYet
            }
        },
        RoPick::Done => RoOutcome::Done,
        RoPick::Skipped => {
            log_info!("subs: {name} already has a subtitle chosen in Plex, left as is");
            RoOutcome::Skipped
        }
        RoPick::NotYet => RoOutcome::NotYet,
    }
}

/// The RO-select list: take what convert / strip-dv noted, drop what is older than
/// [`RO_SELECT_MAX_AGE`], then look up every entry older than [`RO_SELECT_SETTLE`] (one
/// library listing per section and kind) and select its RO subtitle ([`RoPick`]). Only
/// [`RoOutcome::NotYet`] stays. No Plex token: nothing looked up. True: the list changed.
pub async fn select_ro(app: &AppState, tr: &mut TrState, quiet: &mut Quiet, now: f64) -> bool {
    let mut changed = false;
    for e in app.subs.take_ro_select() {
        tr.want_ro_select(e);
        changed = true;
    }
    let before = tr.ro_select.len();
    tr.ro_select.retain(|e| {
        let fresh = now - e.since <= RO_SELECT_MAX_AGE;
        if !fresh {
            log_info!("subs: {} has no RO subtitle in Plex after a day, not selected", video_name(&e.path));
        }
        fresh
    });
    changed |= tr.ro_select.len() != before;
    if tr.ro_select.is_empty() || app.services.keys.get("plex").is_empty() {
        return changed;
    }
    let plex = PlexJobs::new(app.services.plex.c.clone());
    let mut listings: HashMap<(u32, bool), Option<HashMap<String, PartRef>>> = HashMap::new();
    let mut keep = Vec::new();
    for e in std::mem::take(&mut tr.ro_select) {
        if now - e.since < RO_SELECT_SETTLE {
            keep.push(e);
            continue;
        }
        let lk = (e.section, e.movie);
        if let std::collections::hash_map::Entry::Vacant(slot) = listings.entry(lk) {
            let qk = format!("ro-select-{}-{}", e.section, e.movie);
            let listing = match plex.part_refs(e.section, !e.movie).await {
                Ok(m) => {
                    quiet.clear(&qk);
                    Some(m)
                }
                Err(err) => {
                    quiet.warn(&qk, format!("subs: Plex section {} failed ({}), the RO subtitle is selected later", e.section, err.type_name()));
                    None
                }
            };
            slot.insert(listing);
        }
        let outcome = match &listings[&lk] {
            Some(parts) => select_one(app, &plex, quiet, parts, &e).await,
            None => RoOutcome::NotYet,
        };
        if outcome == RoOutcome::NotYet {
            keep.push(e);
        } else {
            changed = true;
        }
    }
    tr.ro_select = keep;
    changed
}

// -- RO subtitles Bazarr downloaded from a provider --------------------------------------

/// Bazarr's newest history records read per kind and tick.
pub const HISTORY_LENGTH: usize = 100;
/// A history record older than this (when its age is known) is not queued; on the first run
/// (no cursor yet) only records known to be this recent are.
pub const HISTORY_WINDOW: f64 = 24.0 * 3600.0;
/// Bazarr history actions that put a subtitle on disk: 1 downloaded, 2 manual (picked in
/// Bazarr's search), 3 upgraded, 4 uploaded by the user. Not 0 deleted, 5 synced, 6
/// translated (queued by [`poll`] already), 7 extracted.
pub const HISTORY_ACTIONS: [i64; 4] = [1, 2, 3, 4];

/// `ro` / `ron` / `rum` (any case).
fn is_ro_code(c: &str) -> bool {
    matches!(c.to_ascii_lowercase().as_str(), "ro" | "ron" | "rum")
}

fn int_of(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A history record that put a non-forced Romanian subtitle on disk ([`HISTORY_ACTIONS`]),
/// with a `subtitles_path`. `language` is Bazarr's `{"code2", "code3", "forced", "hi"}`
/// (older Bazarr: the raw `ro`, `ro:hi`, `ro:forced` string).
pub fn ro_download(rec: &Value) -> bool {
    let ro = match rec.get("language") {
        Some(Value::Object(l)) => {
            // `code2`, else `code3`.
            let code = ["code2", "code3"].iter().find_map(|k| l.get(*k).and_then(Value::as_str).filter(|c| !c.is_empty()));
            code.is_some_and(is_ro_code) && !l.get("forced").and_then(Value::as_bool).unwrap_or(false)
        }
        Some(Value::String(s)) => s.split(':').next().is_some_and(is_ro_code) && !s.ends_with(":forced"),
        _ => false,
    };
    ro && int_of(rec.get("action")).is_some_and(|a| HISTORY_ACTIONS.contains(&a))
        && rec.get("subtitles_path").and_then(Value::as_str).is_some_and(|p| !p.trim().is_empty())
}

/// `pretty.date` text (what Bazarr gives as `timestamp`): `now`, `12 seconds ago`, `a minute
/// ago`, `5 minutes ago`, `an hour ago`, `3 hours ago`, `yesterday`, `4 days ago`, `last
/// week`, ... (also the short `5m ago`) -> seconds, roughly. Anything else: `None`.
pub fn relative_age(s: &str) -> Option<f64> {
    const MIN: f64 = 60.0;
    const HOUR: f64 = 3600.0;
    const DAY: f64 = 86400.0;
    let t = s.trim().to_ascii_lowercase();
    let t = t.strip_suffix("ago").unwrap_or(&t).trim();
    let fixed = match t {
        "now" | "just now" => Some(0.0),
        "a second" => Some(1.0),
        "a minute" => Some(MIN),
        "an hour" => Some(HOUR),
        "yesterday" | "yest" | "a day" => Some(DAY),
        "day before" => Some(2.0 * DAY),
        "last week" | "a week" => Some(7.0 * DAY),
        "last month" | "a month" => Some(30.0 * DAY),
        "last year" | "a year" => Some(365.0 * DAY),
        _ => None,
    };
    if fixed.is_some() {
        return fixed;
    }
    let digits = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let n: f64 = t[..digits].parse().ok()?;
    let unit = match t[digits..].trim() {
        "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
        "m" | "min" | "mins" | "minute" | "minutes" => MIN,
        "h" | "hr" | "hrs" | "hour" | "hours" => HOUR,
        "d" | "day" | "days" => DAY,
        "w" | "wk" | "week" | "weeks" => 7.0 * DAY,
        "mo" | "month" | "months" => 30.0 * DAY,
        "y" | "yr" | "year" | "years" => 365.0 * DAY,
        _ => return None,
    };
    Some(n * unit)
}

/// How old a history record is, if it says: a numeric `timestamp` (unix seconds, or
/// milliseconds; older Bazarr) or Bazarr's relative `timestamp` text ([`relative_age`]).
/// `parsed_timestamp` (`strftime('%x %X')`, locale-dependent, local time) is not read.
pub fn history_age(rec: &Value, now: f64) -> Option<f64> {
    match rec.get("timestamp")? {
        Value::Number(n) => {
            let t = n.as_f64()?;
            Some(now - if t > 1e12 { t / 1000.0 } else { t })
        }
        Value::String(s) => match s.trim().parse::<f64>() {
            Ok(t) => Some(now - if t > 1e12 { t / 1000.0 } else { t }),
            Err(_) => relative_age(s),
        },
        _ => None,
    }
}

/// FNV-1a, 64 bit (stable across builds, unlike the std hasher).
fn fnv64(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// A history record's identity, kept as the cursor ([`RoHistory`]): a hash of the fields that
/// never change for a record (action, language, file, ids, provider, `parsed_timestamp`;
/// not the relative `timestamp`, which does, unless it is a number).
pub fn history_mark(rec: &Value) -> String {
    let field = |k: &str| match rec.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Object(_)) => rec[k].to_string(),
        _ => String::new(),
    };
    let ts = match rec.get("timestamp") {
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    let lang = match rec.get("language") {
        Some(Value::Object(l)) => ["code2", "code3", "forced", "hi"].iter().map(|k| l.get(*k).map_or(String::new(), Value::to_string)).collect::<Vec<_>>().join(","),
        _ => field("language"),
    };
    let parts = [field("action"), lang, field("subtitles_path"), field("sonarrEpisodeId"), field("radarrId"), field("provider"), field("parsed_timestamp"), ts];
    format!("{:016x}", fnv64(&parts.join("\u{1f}")))
}

/// The RO downloads among `recs` (newest first, as Bazarr gives them) not seen before
/// (`seen`: the cursor, `None` on the first run), at most [`HISTORY_WINDOW`] old if the age
/// is known; on the first run the age must be known.
pub fn history_new<'a>(recs: &'a [Value], seen: Option<&[String]>, now: f64) -> Vec<&'a Value> {
    recs.iter()
        .filter(|r| ro_download(r))
        .filter(|r| seen.is_none_or(|s| !s.contains(&history_mark(r))))
        .filter(|r| match history_age(r, now) {
            Some(age) => age <= HISTORY_WINDOW,
            None => seen.is_some(),
        })
        .collect()
}

/// The video next to subtitle file `sub`: a video file in the same folder whose name (no
/// extension) followed by `.ro.`, `.ron.` or `.rum.` starts the subtitle's name (`X.ro.srt`,
/// `X.ro.hi.srt`, `X.ron.srt`); the longest such name wins.
pub fn video_for_sub(sub: &Path) -> Option<PathBuf> {
    let name = sub.file_name()?.to_string_lossy().to_lowercase();
    fs::read_dir(sub.parent()?)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()).is_some_and(|x| VIDEO_EXTS.contains(&x.to_ascii_lowercase().as_str())))
        .filter(|p| p.is_file())
        .filter_map(|p| {
            let stem = p.file_stem()?.to_string_lossy().to_lowercase();
            let rest = name.strip_prefix(&stem)?.strip_prefix('.')?;
            let code = rest.split('.').next()?;
            (is_ro_code(code) && rest.len() > code.len()).then_some((stem.len(), p))
        })
        .max_by_key(|(n, _)| *n)
        .map(|(_, p)| p)
}

/// The video a history record is for: its `video_path` / `path` if Bazarr gives one (a video
/// file name; current Bazarr drops both), else the one next to `subtitles_path`
/// ([`video_for_sub`]).
pub fn history_video(rec: &Value) -> Option<String> {
    let given = ["video_path", "path"].iter().filter_map(|k| rec.get(*k).and_then(Value::as_str)).find(|p| {
        Path::new(p).extension().and_then(|x| x.to_str()).is_some_and(|x| VIDEO_EXTS.contains(&x.to_ascii_lowercase().as_str()))
    });
    if let Some(p) = given {
        return Some(p.to_string());
    }
    let sub = rec.get("subtitles_path").and_then(Value::as_str)?;
    video_for_sub(Path::new(sub)).map(|p| p.to_string_lossy().into_owned())
}

/// Bazarr's subtitle history (`GET /api/episodes/history` and `/api/movies/history`, newest
/// [`HISTORY_LENGTH`] each): every RO subtitle a provider (or the user) put on disk since the
/// last read ([`history_new`]) puts its video on the RO-select list (`tv_section` for
/// episodes, `movie_section` for movies), so [`select_ro`] makes Plex play it. A failed
/// read is logged once and leaves that kind's cursor alone. No Plex token: nothing read.
/// True: the state changed.
///
/// Bazarr's records (assumed from its API, tolerant): `data[]` with `action`, `language`
/// ([`ro_download`]), `subtitles_path` (mapped to this machine's paths), `timestamp` (relative
/// text, [`history_age`]), `parsed_timestamp`, `provider`, `sonarrEpisodeId` / `radarrId`;
/// no record id, no video path.
pub async fn ro_from_history(app: &AppState, tr: &mut TrState, quiet: &mut Quiet, tv_section: u32, movie_section: u32, now: f64) -> bool {
    if app.services.keys.get("plex").is_empty() {
        return false;
    }
    let bz = &app.services.bazarr;
    let mut changed = false;
    for movie in [false, true] {
        let (kind, section) = if movie { ("movies", movie_section) } else { ("episodes", tv_section) };
        let qk = format!("bazarr-history-{kind}");
        let recs = match bz.data(&format!("/api/{kind}/history?start=0&length={HISTORY_LENGTH}"), LIST_TIMEOUT).await {
            Ok(r) => {
                quiet.clear(&qk);
                r
            }
            Err(e) => {
                quiet.warn(&qk, format!("subs: Bazarr {kind} history failed ({e}), downloaded RO subtitles are selected later"));
                continue;
            }
        };
        let seen = if movie { &tr.ro_history.movie } else { &tr.ro_history.ep };
        let fresh: Vec<(String, String)> = history_new(&recs, seen.as_deref(), now)
            .into_iter()
            .filter_map(|r| Some((history_video(r)?, r.get("provider").and_then(Value::as_str).unwrap_or("").to_string())))
            .collect();
        for (video, provider) in fresh {
            let name = video_name(&video);
            let from = if provider.is_empty() { String::new() } else { format!(" ({})", ascii(&provider)) };
            let dry = if app.dry_run { " (dry-run)" } else { "" };
            log_info!("subs{dry}: Bazarr downloaded a RO subtitle for {name}{from}, it is selected in Plex next");
            tr.want_ro_select(RoSelect { path: video, section, movie, since: now });
            changed = true;
        }
        let marks: Vec<String> = recs.iter().map(history_mark).collect();
        let slot = if movie { &mut tr.ro_history.movie } else { &mut tr.ro_history.ep };
        if slot.as_ref() != Some(&marks) {
            *slot = Some(marks);
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dlg(start: &str, end: &str, style: &str, text: &str) -> String {
        format!("Dialogue: 0,{start},{end},{style},,0,0,0,,{text}\n")
    }

    fn script(events: &str) -> String {
        format!(
            "[Script Info]\nTitle: Test\n\n[V4+ Styles]\nFormat: Name, Fontname\nStyle: Default,Arial\n\n[Events]\n\
             Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n{events}"
        )
    }

    fn many(n: usize, style: &str, from_s: u64) -> String {
        (0..n).map(|i| dlg(&format!("0:{:02}:{:02}.00", (from_s + i as u64) / 60, (from_s + i as u64) % 60), "0:59:00.00", style, &format!("{style} {i}"))).collect()
    }

    #[test]
    fn times() {
        assert_eq!(parse_time("0:00:06.31"), Some(6310));
        assert_eq!(srt_time(parse_time("0:00:06.31").unwrap()), "00:00:06,310");
        assert_eq!(parse_time("1:02:03.5"), Some(3_723_500));
        assert_eq!(parse_time("00:00:01,000"), Some(1000));
        assert_eq!(parse_time("00:00:01.123"), Some(1123));
        assert_eq!(parse_time("nope"), None);
        assert_eq!(parse_time("0:00:0x.10"), None);
        assert_eq!(srt_time(3_723_456), "01:02:03,456");
    }

    #[test]
    fn ass_tags_and_order() {
        assert_eq!(clean_ass_text(r"{\i1}Second{\i0}\Nline"), "Second\nline");
        assert_eq!(clean_ass_text(r"First\hline \n  "), "First line");
        assert_eq!(clean_ass_text(r"{\k20}{\fad(1,2)}"), "");
        // 60 dialogue lines plus karaoke, signs and the preview; out of order on purpose.
        let mut ev = String::new();
        ev.push_str(&dlg("0:00:10.00", "0:00:12.00", "Default", r"{\i1}Second{\i0}\Nline"));
        ev.push_str(&dlg("0:00:06.31", "0:00:08.50", "Default", r"First\hline"));
        ev.push_str("Comment: 0,0:00:07.00,0:00:08.00,Default,,0,0,0,,a comment\n");
        ev.push_str(&many(58, "Default", 100));
        ev.push_str(&many(20, "OP1 - ENG A", 20));
        ev.push_str(&many(5, "ED1 - ROM", 40));
        ev.push_str(&dlg("0:00:30.00", "0:00:31.00", "To Be Continued Sign", "To be continued"));
        ev.push_str(&dlg("0:00:30.00", "0:00:31.00", "Episode Preview Sign 2", "Next time"));
        // A positioned line in the dominant style stays; a positioned one in another style goes.
        ev.push_str(&dlg("0:00:50.00", "0:00:51.00", "Default", r"{\pos(10,20)}Up here"));
        ev.push_str(&dlg("0:00:52.00", "0:00:53.00", "Italics", r"{\move(1,2,3,4)}Shop name"));
        ev.push_str(&dlg("0:00:54.00", "0:00:55.00", "Italics", r"{\i1}A thought{\i0}"));
        // Drawings, empty texts and a shadow-layer duplicate go.
        ev.push_str(&dlg("0:00:56.00", "0:00:57.00", "Default", r"{\p1}m 0 0 l 100 0{\p0}"));
        ev.push_str(&dlg("0:00:58.00", "0:00:59.00", "Default", r"{\an8}"));
        ev.push_str(&dlg("0:00:06.31", "0:00:08.50", "Default", r"{\blur3}First\hline"));
        // `Default - Top` is no OP.
        ev.push_str(&dlg("0:00:59.00", "0:00:59.50", "Default - Top", "Top line"));
        let cues = ass_to_cues(&script(&ev));
        assert_eq!(cues.len(), 2 + 58 + 1 + 1 + 1, "{cues:#?}");
        assert!(cues.windows(2).all(|w| w[0].start <= w[1].start), "sorted by start");
        assert!(cues.iter().all(|c| !c.text.contains("OP1") && !c.text.contains("ED1") && !c.text.contains("Next")));
        assert!(cues.iter().any(|c| c.text == "Up here") && !cues.iter().any(|c| c.text == "Shop name"));
        assert!(cues.iter().any(|c| c.text == "A thought") && cues.iter().any(|c| c.text == "Top line"));
        let srt = cues_to_srt(&cues);
        assert!(srt.starts_with("1\n00:00:06,310 --> 00:00:08,500\nFirst line\n\n2\n00:00:10,000 --> 00:00:12,000\nSecond\nline\n\n"), "{srt}");
        assert!(!srt.starts_with('\u{feff}'));
    }

    #[test]
    fn style_fallback() {
        // The dialogue sits in a style the exclusion matches ("Main Title"): too little is
        // left, so only dialogue-looking styles are kept (Main..., Narrator), songs are not.
        let ev = format!("{}{}{}", many(60, "Main Title", 0), many(30, "Insert Song", 100), many(5, "Narrator", 200));
        let cues = ass_to_cues(&script(&ev));
        assert_eq!(cues.len(), 65);
        assert!(cues.iter().all(|c| !c.text.starts_with("Insert")));
        // A short script: everything is in Default anyway.
        let ev = format!("{}{}", many(10, "Default", 0), many(30, "OP", 100));
        assert_eq!(ass_to_cues(&script(&ev)).len(), 10);
        // Nothing dialogue-looking: the exclusion result stays.
        let ev = format!("{}{}", many(10, "Alice", 0), many(30, "OP", 100));
        assert_eq!(ass_to_cues(&script(&ev)).len(), 10);
        // Nothing at all.
        assert!(english_srt(&script(&many(5, "Sign", 0)), true).is_none());
    }

    #[test]
    fn srt_cleaning() {
        let raw = "\u{feff}1\r\n00:00:02,000 --> 00:00:03,000\r\n<font face=\"Arial\" size=\"20\">{\\an8}Hello</font>\r\n\r\n\
                   2\r\n00:00:01,000 --> 00:00:01,500\r\n<i>First</i>\r\n\r\n3\r\n00:00:04,000 --> 00:00:05,000\r\n{\\an8}\r\n";
        let (srt, n) = english_srt(raw, false).unwrap();
        assert_eq!(n, 2);
        assert_eq!(srt, "1\n00:00:01,000 --> 00:00:01,500\n<i>First</i>\n\n2\n00:00:02,000 --> 00:00:03,000\nHello\n\n");
    }

    #[test]
    fn header_cue_removed() {
        let t = "1\r\n00:00:01,000 --> 00:00:05,000\r\n# Subtitles translated with gemini-3.5-flash-lite #\r\n\r\n\
                 2\r\n00:00:06,310 --> 00:00:08,500\r\nPrima linie\r\na doua\r\n\r\n3\r\n00:00:10,000 --> 00:00:12,000\r\nAlta\r\n\r\n";
        assert_eq!(
            strip_header(t).unwrap(),
            "1\r\n00:00:06,310 --> 00:00:08,500\r\nPrima linie\r\na doua\r\n\r\n2\r\n00:00:10,000 --> 00:00:12,000\r\nAlta\r\n\r\n"
        );
        let lf = "\u{feff}1\n00:00:01,000 --> 00:00:05,000\n# Subtitles translated with gemini #\n\n2\n00:00:06,000 --> 00:00:07,000\nDa\n";
        assert_eq!(strip_header(lf).unwrap(), "\u{feff}1\n00:00:06,000 --> 00:00:07,000\nDa\n\n");
        assert_eq!(strip_header("1\n00:00:06,000 --> 00:00:07,000\n# Nu e antet\n\n"), None);
        assert!(!is_header("#") && is_header(" # Subtitles Translated with x # "));
    }

    fn sub(index: i64, codec: &str, lang: &str, title: &str, frames: &str, forced: i64) -> Value {
        json!({"index": index, "codec_type": "subtitle", "codec_name": codec, "disposition": {"forced": forced},
               "tags": {"language": lang, "title": title, "NUMBER_OF_FRAMES-eng": frames}})
    }

    #[test]
    fn source_preference() {
        let probe = json!({"streams": [
            {"index": 0, "codec_type": "video", "codec_name": "h264"},
            sub(2, "ass", "eng", "Signs & Songs", "900", 0),
            sub(3, "ass", "eng", "", "800", 1),
            sub(4, "ass", "eng", "Full", "251", 0),
            sub(5, "subrip", "eng", "Full SRT", "100", 0),
            sub(6, "ass", "jpn", "", "999", 0),
            sub(7, "hdmv_pgs_subtitle", "eng", "", "999", 0),
            sub(8, "ass", "eng", "Forced", "999", 0)]});
        assert_eq!(pick_track(&probe), Some((4, "ass".to_string())));
        let probe = json!({"streams": [sub(3, "mov_text", "en", "", "", 0), sub(4, "ass", "spa", "", "", 0)]});
        assert_eq!(pick_track(&probe), Some((3, "mov_text".to_string())));
        assert_eq!(pick_track(&json!({"streams": [sub(2, "ass", "eng", "Signs", "5", 0)]})), None);
        // External files: not empty, `.en.srt` before `.eng.srt`.
        let d = crate::jobs::testutil::temp_dir("subs-src");
        let v = d.join("Show - S01E02.mkv");
        fs::write(&v, b"x").unwrap();
        assert_eq!(external_en(&v), None);
        fs::write(d.join("Show - S01E02.en.srt"), b"").unwrap();
        assert_eq!(external_en(&v), None, "empty");
        fs::write(d.join("Show - S01E02.eng.srt"), b"1").unwrap();
        assert_eq!(external_en(&v), Some(d.join("Show - S01E02.eng.srt")));
        fs::write(d.join("Show - S01E02.en.srt"), b"1").unwrap();
        assert_eq!(external_en(&v), Some(d.join("Show - S01E02.en.srt")));
        assert_eq!(sibling(&v, "ro.srt"), d.join("Show - S01E02.ro.srt"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn auto_translate_and_its_alias() {
        let (t, f) = (json!(true), json!(false));
        assert!(!auto_setting(None, None), "off by default");
        assert!(!auto_setting(None, Some(&f)), "the live config: translate: false");
        assert!(auto_setting(None, Some(&t)), "the old key still turns it on");
        assert!(!auto_setting(Some(&f), Some(&t)), "auto_translate wins");
        assert!(auto_setting(Some(&t), Some(&f)));
        assert!(!auto_setting(Some(&json!("yes")), None), "not a bool: off");
    }

    #[test]
    fn translator_settings() {
        assert!(translator_ready(&json!({"translator": {"translator_type": "gemini", "gemini_keys": ["k"]}})));
        assert!(translator_ready(&json!({"data": {"translator": {"translator_type": "Gemini", "gemini_keys": "k"}}})));
        assert!(!translator_ready(&json!({"translator": {"translator_type": "gemini", "gemini_keys": []}})));
        assert!(!translator_ready(&json!({"translator": {"translator_type": "gemini", "gemini_keys": [""]}})));
        assert!(!translator_ready(&json!({"translator": {"translator_type": "google_translate", "gemini_keys": ["k"]}})));
        assert!(!translator_ready(&json!({})));
    }

    #[test]
    fn job_status() {
        let en = "D:\\Seriale\\Show\\Show - S01E02.en.srt";
        let job = |status: &str, msg: &str| json!({"job_name": "Translating from EN to RO using Gemini", "status": status, "progress_message": msg});
        let t0 = 1000.0;
        let run = [job("running", en)];
        assert_eq!(judge(Some(&run), en, 0, t0, t0 + 60.0), Verdict::Wait);
        assert_eq!(judge(Some(&run), en, 0, t0, t0 + JOB_TIMEOUT + 1.0), Verdict::Failed("timeout"));
        assert!(translation_running(&run) && !translation_running(&[job("completed", en)]));
        assert!(!translation_running(&[json!({"job_name": "Search", "status": "running"})]));
        let done = [job("completed", "d:/seriale/show/show - s01e02.en.srt")];
        assert_eq!(judge(Some(&done), en, 500, t0, t0 + 60.0), Verdict::Done);
        assert!(matches!(judge(Some(&done), en, 0, t0, t0 + 60.0), Verdict::Failed(_)), "completed but empty");
        assert!(matches!(judge(Some(&[job("failed", en)]), en, 0, t0, t0 + 60.0), Verdict::Failed(_)));
        // An older failed attempt and ours running: running wins.
        assert_eq!(judge(Some(&[job("failed", en), job("pending", en)]), en, 0, t0, t0 + 60.0), Verdict::Wait);
        // Not listed: the file decides; no list: wait (the file may be half written), until the timeout.
        let other = [job("running", "D:\\x\\other.en.srt")];
        assert_eq!(judge(Some(&other), en, 500, t0, t0 + 60.0), Verdict::Done);
        assert_eq!(judge(Some(&other), en, 0, t0, t0 + 60.0), Verdict::Wait);
        assert_eq!(judge(None, en, 500, t0, t0 + 60.0), Verdict::Wait);
        assert_eq!(judge(None, en, 500, t0, t0 + JOB_TIMEOUT + 1.0), Verdict::Done);
        assert_eq!(judge(None, en, 0, t0, t0 + JOB_TIMEOUT + 1.0), Verdict::Failed("timeout"));
        // The progress message holding the file name in a longer text matches too.
        assert_eq!(find_job(&[job("completed", "Translating Show - S01E02.en.srt")], en), Some(JobState::Completed));
    }

    fn cand(key: &str, movie: i64) -> Cand {
        Cand { key: key.into(), series_id: 1, episode_id: 0, movie_id: movie, title: key.into(), season: 1, episode: 1 }
    }

    #[test]
    fn candidate_selection() {
        let ranked = vec![cand("ep:3", 0), cand("ep:1", 0), cand("ep:2", 0), cand("ep:4", 0)];
        let searched = |k: &str| k != "ep:4";
        let mut tr = TrState::default();
        tr.items.insert("ep:1".into(), TrItem { tries: 1, next: 500.0, why: "failed".into() });
        let keys = |v: Vec<Cand>| v.into_iter().map(|c| c.key).collect::<Vec<_>>();
        // Order kept; never searched (ep:4), in backoff (ep:1) and the one searched now (ep:2) skipped.
        assert_eq!(keys(candidates(&ranked, &searched, &tr, 100.0, Some("ep:2"))), ["ep:3"]);
        assert_eq!(keys(candidates(&ranked, &searched, &tr, 600.0, None)), ["ep:3", "ep:1", "ep:2"]);
        // In flight: not a candidate, and nothing may start.
        tr.inflight = Some(InFlight { key: "ep:3".into(), series_id: 1, episode_id: 3, movie_id: 0, name: "x".into(), en: "a".into(), ro: "b".into(), started: 0.0, tries: 0, manual: false });
        assert_eq!(keys(candidates(&ranked, &searched, &tr, 600.0, None)), ["ep:1", "ep:2"]);
        let cfg = TrSettings { auto: true, per_day: 2, notify: true, movie_section: 1 };
        assert_eq!(local_gate(&tr, &cfg, "2026-10-28"), Err("in flight"));
        tr.inflight = None;
        assert_eq!(local_gate(&tr, &cfg, "2026-10-28"), Ok(()));
        // The daily cap, per local day.
        tr.note_started("2026-10-28", false);
        tr.note_started("2026-10-28", true);
        assert_eq!(local_gate(&tr, &cfg, "2026-10-28"), Err("daily cap"));
        assert_eq!(local_gate(&tr, &cfg, "2026-10-29"), Ok(()));
        let off = TrSettings { auto: false, ..cfg.clone() };
        assert_eq!(local_gate(&tr, &off, "2026-10-29"), Err("off"));
        // Manual requests do not need auto_translate, but the cap counts for them too.
        assert_eq!(manual_gate(&tr, &off, "2026-10-29"), Ok(()));
        assert_eq!(manual_gate(&tr, &off, "2026-10-28"), Err("daily cap"));
        // Episode and movie take turns.
        let (e, m) = (Some(cand("ep:1", 0)), Some(cand("movie:7", 7)));
        assert_eq!(pick_kind(e.clone(), m.clone(), "ep").unwrap().key, "movie:7");
        assert_eq!(pick_kind(e.clone(), m.clone(), "movie").unwrap().key, "ep:1");
        assert_eq!(pick_kind(e.clone(), m.clone(), "").unwrap().key, "ep:1");
        assert_eq!(pick_kind(None, m, "movie").unwrap().key, "movie:7");
        assert_eq!(pick_kind(e, None, "ep").unwrap().key, "ep:1");
        // Movies: unwatched, with a file, newest first.
        let ms = vec![cand("movie:3", 3), cand("movie:9", 9), cand("movie:5", 5), cand("movie:4", 4)];
        let paths: HashMap<i64, String> = [(3, "D:\\M\\3.mkv"), (9, "D:\\M\\9.mkv"), (5, "D:\\M\\5.mkv")].into_iter().map(|(i, p)| (i, p.to_string())).collect();
        let views: HashMap<String, i64> = [(path_key("d:/m/9.mkv"), 1)].into_iter().collect();
        assert_eq!(keys(order_movie_cands(&ms, &paths, &views)), ["movie:5", "movie:3"]);
        // State round trip.
        tr.inflight = Some(InFlight { key: "movie:5".into(), series_id: 0, episode_id: 0, movie_id: 5, name: "Film".into(), en: "a".into(), ro: "b".into(), started: 9.5, tries: 2, manual: true });
        assert_eq!(TrState::from_json(Some(&tr.to_json())), tr);
        // An in-flight record from before `manual` existed is automatic.
        let mut old = tr.to_json();
        old["inflight"].as_object_mut().unwrap().remove("manual");
        assert!(!TrState::from_json(Some(&old)).inflight.unwrap().manual);
        assert_eq!(tr_backoff(1), 3600.0);
        assert_eq!(tr_backoff(2), 21600.0);
        assert_eq!(tr_backoff(7), 86400.0);
    }

    // -- the RO subtitle chosen in Plex ----------------------------------------------------

    use crate::jobs::testutil::{app, fake, ok, status, Fake, Req};
    use std::sync::Mutex;

    fn entry(path: &str, section: u32, movie: bool, since: f64) -> RoSelect {
        RoSelect { path: path.into(), section, movie, since }
    }

    fn stream(id: i64, lang: &str, selected: bool, external: bool) -> Value {
        let mut s = json!({"id": id, "streamType": 3, "languageCode": lang});
        if selected {
            s["selected"] = json!(true);
        }
        if external {
            s["key"] = json!(format!("/library/streams/{id}"));
        }
        s
    }

    fn ss(id: i64, lang: &str, selected: bool, external: bool) -> SubStream {
        SubStream { id, language: lang.into(), selected, external }
    }

    /// One Plex item: section, file as Plex lists it, ratingKey, part id, subtitle streams.
    type Item = (u32, &'static str, &'static str, i64, Vec<Value>);

    /// A Plex that lists `items` per section, answers their metadata (500 if `meta_fails`)
    /// and accepts every PUT.
    async fn plex(items: Vec<Item>, meta_fails: bool) -> Fake {
        let items = Arc::new(Mutex::new(items));
        fake(move |r: &Req| {
            let p = r.path.as_str();
            let items = items.lock().unwrap();
            if r.method == "PUT" && p.starts_with("/library/parts/") {
                return status(200);
            }
            if let Some(rest) = p.strip_prefix("/library/sections/") {
                let n: u32 = rest.split('/').next().and_then(|s| s.parse().ok()).unwrap_or(0);
                let md: Vec<Value> = items
                    .iter()
                    .filter(|i| i.0 == n)
                    .map(|i| json!({"ratingKey": i.2, "Media": [{"Part": [{"id": i.3, "file": i.1}]}]}))
                    .collect();
                return ok(json!({"MediaContainer": {"Metadata": md}}));
            }
            if let Some(rk) = p.strip_prefix("/library/metadata/") {
                if meta_fails {
                    return status(500);
                }
                let Some(i) = items.iter().find(|i| i.2 == rk) else { return status(404) };
                let mut streams = vec![json!({"id": 1000, "streamType": 1}), json!({"id": 1001, "streamType": 2, "selected": true})];
                streams.extend(i.4.iter().cloned());
                return ok(json!({"MediaContainer": {"Metadata": [{"ratingKey": i.2, "Media": [{"Part": [{"id": i.3, "file": i.1, "Stream": streams}]}]}]}}));
            }
            status(404)
        })
        .await
    }

    fn puts(f: &Fake) -> Vec<String> {
        f.find("PUT", "/library/parts/").into_iter().map(|r| r.path).collect()
    }

    fn paths(tr: &TrState) -> Vec<&str> {
        tr.ro_select.iter().map(|e| e.path.as_str()).collect()
    }

    #[test]
    fn ro_pick_choices() {
        // Nothing chosen: the RO stream, the external one first.
        assert_eq!(ro_pick(&[ss(1, "eng", false, false), ss(2, "ron", false, false), ss(3, "ron", false, true)]), RoPick::Select(3));
        assert_eq!(ro_pick(&[ss(1, "eng", false, false), ss(2, "RON", false, false)]), RoPick::Select(2));
        assert_eq!(ro_pick(&[ss(1, "eng", false, false), ss(2, "ron", true, true)]), RoPick::Done);
        assert_eq!(ro_pick(&[ss(1, "eng", true, false), ss(2, "ron", false, true)]), RoPick::Skipped);
        assert_eq!(ro_pick(&[ss(1, "eng", false, false)]), RoPick::NotYet);
        assert_eq!(ro_pick(&[]), RoPick::NotYet);
    }

    #[tokio::test]
    async fn ro_select_selects_skips_and_keeps() {
        let now = 1_000_000.0;
        let f = plex(
            vec![
                // Nothing chosen, RO there (Plex lists the path in another case and with `/`).
                (2, "d:/tv/show/a.mkv", "10", 100, vec![stream(1, "eng", false, false), stream(2, "ron", false, true)]),
                // RO chosen already.
                (2, "D:\\TV\\Show\\B.mkv", "11", 101, vec![stream(3, "eng", false, false), stream(4, "ron", true, true)]),
                // The user chose English.
                (2, "D:\\TV\\Show\\C.mkv", "12", 102, vec![stream(5, "eng", true, false), stream(6, "ron", false, true)]),
                // No RO stream yet.
                (2, "D:\\TV\\Show\\D.mkv", "13", 103, vec![stream(7, "eng", false, false)]),
                // Older than a day.
                (2, "D:\\TV\\Show\\F.mkv", "15", 105, vec![stream(9, "ron", false, true)]),
                // Just added: Plex gets time to rescan.
                (2, "D:\\TV\\Show\\G.mkv", "16", 106, vec![stream(10, "ron", false, true)]),
                // A movie, in its own section (listed without type=4).
                (1, "D:\\Movies\\M.mkv", "20", 200, vec![stream(11, "ron", false, false), stream(12, "eng", false, false)]),
            ],
            false,
        )
        .await;
        let (a, _d) = app(&f.base, false, json!({}));
        let mut tr = TrState::default();
        for e in [
            entry("D:\\TV\\Show\\A.mkv", 2, false, now - 600.0),
            entry("D:\\TV\\Show\\B.mkv", 2, false, now - 600.0),
            entry("D:\\TV\\Show\\C.mkv", 2, false, now - 600.0),
            entry("D:\\TV\\Show\\D.mkv", 2, false, now - 600.0),
            // Not in Plex yet.
            entry("D:\\TV\\Show\\E.mkv", 2, false, now - 600.0),
            entry("D:\\TV\\Show\\F.mkv", 2, false, now - RO_SELECT_MAX_AGE - 1.0),
            entry("D:\\TV\\Show\\G.mkv", 2, false, now - 10.0),
            entry("D:\\Movies\\M.mkv", 1, true, now - 600.0),
        ] {
            tr.want_ro_select(e);
        }
        let mut quiet = Quiet::default();
        assert!(select_ro(&a, &mut tr, &mut quiet, now).await);
        assert_eq!(puts(&f), ["/library/parts/100?subtitleStreamID=2&allParts=1", "/library/parts/200?subtitleStreamID=11&allParts=1"]);
        assert!(f.find("PUT", "/").iter().all(|r| r.header("X-Plex-Token") == Some("plex-tok")));
        assert_eq!(paths(&tr), ["D:\\TV\\Show\\D.mkv", "D:\\TV\\Show\\E.mkv", "D:\\TV\\Show\\G.mkv"]);
        // One listing per section, episodes for the TV one.
        let lists: Vec<String> = f.find("GET", "/library/sections/").into_iter().map(|r| r.path).collect();
        assert_eq!(lists, ["/library/sections/2/all?type=4", "/library/sections/1/all"]);
        // Neither the stale nor the fresh one was looked up.
        assert!(f.find("GET", "/library/metadata/15").is_empty() && f.find("GET", "/library/metadata/16").is_empty());

        // Next tick: G is due and selected; D and E wait on.
        assert!(select_ro(&a, &mut tr, &mut quiet, now + 180.0).await);
        assert_eq!(puts(&f).last().map(String::as_str), Some("/library/parts/106?subtitleStreamID=10&allParts=1"));
        assert_eq!(paths(&tr), ["D:\\TV\\Show\\D.mkv", "D:\\TV\\Show\\E.mkv"]);
        // Nothing changes while they are not in Plex.
        assert!(!select_ro(&a, &mut tr, &mut quiet, now + 360.0).await);
        assert_eq!(tr.ro_select.len(), 2);
    }

    #[tokio::test]
    async fn ro_select_dry_run_and_plex_errors() {
        let now = 1_000_000.0;
        let a_item = || vec![(2, "D:\\TV\\A.mkv", "10", 100, vec![stream(1, "eng", false, false), stream(2, "ron", false, true)])];
        let mut quiet = Quiet::default();
        // Dry-run: only logged, dropped.
        let f = plex(a_item(), false).await;
        let (a, _d) = app(&f.base, true, json!({}));
        let mut tr = TrState { ro_select: vec![entry("D:\\TV\\A.mkv", 2, false, now - 600.0)], ..Default::default() };
        assert!(select_ro(&a, &mut tr, &mut quiet, now).await);
        assert!(f.mutations().is_empty());
        assert!(tr.ro_select.is_empty());
        // The metadata call fails: kept, nothing sent.
        let f = plex(a_item(), true).await;
        let (a, _d) = app(&f.base, false, json!({}));
        let mut tr = TrState { ro_select: vec![entry("D:\\TV\\A.mkv", 2, false, now - 600.0)], ..Default::default() };
        assert!(!select_ro(&a, &mut tr, &mut quiet, now).await);
        assert!(f.mutations().is_empty());
        assert_eq!(tr.ro_select.len(), 1);
        // Plex is down (the listing fails): kept.
        let f = fake(|_r: &Req| status(503)).await;
        let (a, _d) = app(&f.base, false, json!({}));
        assert!(!select_ro(&a, &mut tr, &mut quiet, now).await);
        assert_eq!(tr.ro_select.len(), 1);
        // The PUT fails: kept.
        let f = fake(|r: &Req| {
            if r.method == "PUT" {
                return status(500);
            }
            if r.path.starts_with("/library/sections/") {
                return ok(json!({"MediaContainer": {"Metadata": [{"ratingKey": "10", "Media": [{"Part": [{"id": 100, "file": "D:\\TV\\A.mkv"}]}]}]}}));
            }
            ok(json!({"MediaContainer": {"Metadata": [{"Media": [{"Part": [{"id": 100, "Stream": [stream(2, "ron", false, true)]}]}]}]}}))
        })
        .await;
        let (a, _d) = app(&f.base, false, json!({}));
        assert!(!select_ro(&a, &mut tr, &mut quiet, now).await);
        assert_eq!(puts(&f), ["/library/parts/100?subtitleStreamID=2&allParts=1"]);
        assert_eq!(tr.ro_select.len(), 1);
    }

    #[tokio::test]
    async fn rewritten_videos_with_a_ro_srt_are_noted() {
        let f = plex(vec![], false).await;
        let (a, d) = app(&f.base, false, json!({}));
        let with = d.join("Film.mkv");
        let without = d.join("Other.mkv");
        fs::write(&with, b"video").unwrap();
        fs::write(&without, b"video").unwrap();
        fs::write(sibling(&with, "ro.srt"), b"1\n").unwrap();
        fs::write(sibling(&without, "ro.srt"), b"").unwrap(); // Bazarr's empty placeholder
        ro_select_after_rewrite(&a, &with, 1, true);
        ro_select_after_rewrite(&a, &without, 1, true);
        // The subs job takes them over on its next tick (too fresh to look up yet).
        let mut tr = TrState::default();
        assert!(select_ro(&a, &mut tr, &mut Quiet::default(), unix_now()).await);
        assert_eq!(paths(&tr), [with.to_string_lossy()]);
        assert!(tr.ro_select[0].movie && tr.ro_select[0].section == 1);
        assert!(a.subs.take_ro_select().is_empty());
        assert!(f.requests().is_empty());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn video_of_inverts_sibling() {
        let d = crate::jobs::testutil::temp_dir("ro-video");
        let v = d.join("Show - S01E02.mkv");
        fs::write(&v, b"video").unwrap();
        fs::write(d.join("Show - S01E02.en.srt"), b"x").unwrap();
        fs::write(d.join("Show - S01E03.mkv"), b"video").unwrap();
        assert_eq!(video_of(&sibling(&v, "ro.srt")), Some(v.clone()));
        assert_eq!(video_of(&d.join("Show - S01E09.ro.srt")), None);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn ro_select_state() {
        let mut tr = TrState::default();
        tr.want_ro_select(entry("D:\\TV\\A.mkv", 2, false, 100.0));
        tr.want_ro_select(entry("d:/tv/a.mkv", 2, false, 50.0)); // the same file: the later `since` stays
        tr.want_ro_select(entry("D:\\Movies\\M.mkv", 1, true, 70.0));
        assert_eq!(tr.ro_select, [entry("D:\\TV\\A.mkv", 2, false, 100.0), entry("D:\\Movies\\M.mkv", 1, true, 70.0)]);
        // Round trip.
        let v = tr.to_json();
        assert_eq!(v["ro_select"][1], json!({"path": "D:\\Movies\\M.mkv", "section": 1, "movie": true, "since": 70.0}));
        assert_eq!(TrState::from_json(Some(&v)), tr);
        // A state file from before the list: empty. Broken entries are skipped, duplicates merged.
        let mut old = v.clone();
        old.as_object_mut().unwrap().remove("ro_select");
        assert!(TrState::from_json(Some(&old)).ro_select.is_empty());
        old["ro_select"] = json!([{"path": "D:\\TV\\A.mkv", "section": 2, "since": 1.0}, {"path": "d:/TV/a.mkv", "section": 2, "since": 5.0}, {"section": 2, "since": 1.0}, {"path": "x", "since": 1.0}]);
        assert_eq!(TrState::from_json(Some(&old)).ro_select, [entry("D:\\TV\\A.mkv", 2, false, 5.0)]);
    }

    // -- RO subtitles Bazarr downloaded --------------------------------------------------

    /// A Bazarr history record (current API shape).
    fn hist(action: i64, code2: &str, subs: &str, when: &str, ids: Value) -> Value {
        let mut r = json!({"action": action, "language": {"name": "Romanian", "code2": code2, "code3": "ron", "forced": false, "hi": false},
                           "subtitles_path": subs, "timestamp": when, "parsed_timestamp": format!("11/11/26 10:00:00 {subs}"),
                           "provider": "titrari", "score": "90.0%", "upgradable": false});
        for (k, v) in ids.as_object().unwrap() {
            r[k] = v.clone();
        }
        r
    }

    fn ep_ids() -> Value {
        json!({"seriesTitle": "Show", "sonarrSeriesId": 4, "sonarrEpisodeId": 11})
    }

    fn movie_ids() -> Value {
        json!({"title": "Film", "radarrId": 7})
    }

    #[test]
    fn history_records() {
        // A RO subtitle from a provider: episode or movie, downloaded, manual, upgraded, uploaded.
        assert!(ro_download(&hist(1, "ro", "D:\\TV\\A.ro.srt", "3 hours ago", ep_ids())));
        assert!(ro_download(&hist(1, "ro", "D:\\Movies\\M.ro.srt", "now", movie_ids())));
        for a in [2, 3, 4] {
            assert!(ro_download(&hist(a, "ro", "D:\\TV\\A.ro.srt", "now", ep_ids())), "action {a}");
        }
        // Another language, a forced one, no file.
        assert!(!ro_download(&hist(1, "en", "D:\\TV\\A.en.srt", "now", ep_ids())));
        let mut forced = hist(1, "ro", "D:\\TV\\A.ro.forced.srt", "now", ep_ids());
        forced["language"]["forced"] = json!(true);
        assert!(!ro_download(&forced));
        assert!(!ro_download(&hist(1, "ro", "", "now", ep_ids())));
        // Deleted, synced, translated (queued when the translation is judged), extracted.
        for a in [0, 5, 6, 7] {
            assert!(!ro_download(&hist(a, "ro", "D:\\TV\\A.ro.srt", "now", ep_ids())), "action {a}");
        }
        // Older shapes: the raw language string, a string action, code3 only.
        assert!(ro_download(&json!({"action": "1", "language": "ro:hi", "subtitles_path": "x.ro.hi.srt"})));
        assert!(!ro_download(&json!({"action": 1, "language": "ro:forced", "subtitles_path": "x.ro.forced.srt"})));
        assert!(ro_download(&json!({"action": 1, "language": {"code3": "rum"}, "subtitles_path": "x.rum.srt"})));
        assert!(!ro_download(&json!({"language": "ro", "subtitles_path": "x.ro.srt"})));
    }

    #[test]
    fn history_ages() {
        let now = 1_800_000_000.0;
        let age = |t: Value| history_age(&json!({"timestamp": t}), now);
        assert_eq!(age(json!("now")), Some(0.0));
        assert_eq!(age(json!("12 seconds ago")), Some(12.0));
        assert_eq!(age(json!("a minute ago")), Some(60.0));
        assert_eq!(age(json!("5 minutes ago")), Some(300.0));
        assert_eq!(age(json!("an hour ago")), Some(3600.0));
        assert_eq!(age(json!("23 hours ago")), Some(23.0 * 3600.0));
        assert_eq!(age(json!("5m ago")), Some(300.0));
        assert_eq!(age(json!("yesterday")), Some(86400.0));
        assert_eq!(age(json!("3 days ago")), Some(3.0 * 86400.0));
        assert_eq!(age(json!("last week")), Some(7.0 * 86400.0));
        // Unix seconds or milliseconds (older Bazarr).
        assert_eq!(age(json!(now - 100.0)), Some(100.0));
        assert_eq!(age(json!((now - 100.0) * 1000.0)), Some(100.0));
        assert_eq!(age(json!("1799999900")), Some(100.0));
        // Not understood: unknown.
        assert_eq!(age(json!("today")), None);
        assert_eq!(age(json!("11/11/26 10:00:00")), None);
        assert_eq!(history_age(&json!({"parsed_timestamp": "11/11/26 10:00:00"}), now), None);
    }

    #[test]
    fn history_cursor_and_first_run() {
        let now = 1_000_000.0;
        let mut unknown = hist(1, "ro", "D:\\TV\\C.ro.srt", "", ep_ids());
        unknown.as_object_mut().unwrap().remove("timestamp");
        let recs = vec![
            hist(1, "ro", "D:\\TV\\A.ro.srt", "3 hours ago", ep_ids()),
            hist(1, "ro", "D:\\TV\\B.ro.srt", "2 days ago", ep_ids()),
            unknown,
            hist(1, "en", "D:\\TV\\D.en.srt", "now", ep_ids()),
            hist(6, "ro", "D:\\TV\\E.ro.srt", "now", ep_ids()),
            hist(3, "ro", "D:\\TV\\F.ro.srt", "a minute ago", ep_ids()),
        ];
        // First run: only RO downloads known to be from the last day.
        assert_eq!(history_new(&recs, None, now), [&recs[0], &recs[5]]);
        // With a cursor: what it holds is old news; an unknown age is new, an old one is not.
        let seen: Vec<String> = [&recs[0], &recs[3]].iter().map(|r| history_mark(r)).collect();
        assert_eq!(history_new(&recs, Some(&seen), now), [&recs[2], &recs[5]]);
        // A record keeps its mark while its relative time and flags move on; another download
        // of the same file (another time) is a new record.
        let mut later = recs[0].clone();
        later["timestamp"] = json!("4 hours ago");
        later["upgradable"] = json!(true);
        later["score"] = json!("91.0%");
        assert_eq!(history_mark(&later), history_mark(&recs[0]));
        later["parsed_timestamp"] = json!("11/11/26 11:00:00");
        assert_ne!(history_mark(&later), history_mark(&recs[0]));
        assert_eq!(history_mark(&recs[0]).len(), 16);
    }

    #[test]
    fn history_videos() {
        let d = crate::jobs::testutil::temp_dir("ro-hist-video");
        let v = d.join("Demon Slayer (2025) WEBDL-1080p.mkv");
        fs::write(&v, b"video").unwrap();
        fs::write(d.join("Demon Slayer (2025) WEBDL-1080p.en.srt"), b"x").unwrap();
        fs::write(d.join("Demon Slayer (2025).mkv"), b"video").unwrap();
        let sub = |n: &str| d.join(n).to_string_lossy().into_owned();
        let video = |s: String| history_video(&json!({"subtitles_path": s}));
        let want = Some(v.to_string_lossy().into_owned());
        assert_eq!(video(sub("Demon Slayer (2025) WEBDL-1080p.ro.srt")), want);
        assert_eq!(video(sub("demon slayer (2025) webdl-1080p.RO.hi.srt")), want);
        assert_eq!(video(sub("Demon Slayer (2025) WEBDL-1080p.ron.srt")), want);
        assert_eq!(video(sub("Demon Slayer (2025).ro.srt")), Some(d.join("Demon Slayer (2025).mkv").to_string_lossy().into_owned()));
        // No such video, or not a RO name.
        assert_eq!(video(sub("Other.ro.srt")), None);
        assert_eq!(video(sub("Demon Slayer (2025) WEBDL-1080p.en.srt")), None);
        assert_eq!(video_for_sub(&d.join("Demon Slayer (2025) WEBDL-1080p.ro")), None);
        // The record's own video path wins (a video file name only).
        assert_eq!(history_video(&json!({"video_path": "D:\\TV\\X.mkv", "subtitles_path": sub("Other.ro.srt")})).as_deref(), Some("D:\\TV\\X.mkv"));
        assert_eq!(history_video(&json!({"path": "D:\\Movies\\Film", "subtitles_path": sub("Other.ro.srt")})), None);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn ro_history_state() {
        let tr = TrState { ro_history: RoHistory { ep: Some(vec!["00000000000000aa".into()]), movie: Some(vec![]) }, ..Default::default() };
        let v = tr.to_json();
        assert_eq!(v["ro_history"], json!({"ep": ["00000000000000aa"], "movie": []}));
        assert_eq!(TrState::from_json(Some(&v)), tr);
        // A state file from before the cursor: never read (the first run's day window).
        let mut old = v.clone();
        old.as_object_mut().unwrap().remove("ro_history");
        assert_eq!(TrState::from_json(Some(&old)).ro_history, RoHistory::default());
        old["ro_history"] = json!({"ep": null, "movie": "x"});
        assert_eq!(TrState::from_json(Some(&old)).ro_history, RoHistory::default());
    }

    struct Hist {
        eps: Mutex<Vec<Value>>,
        movies: Mutex<Vec<Value>>,
        up: Mutex<bool>,
    }

    async fn bazarr_history(h: Arc<Hist>) -> Fake {
        fake(move |r: &Req| {
            if !*h.up.lock().unwrap() {
                return status(500);
            }
            if r.path.starts_with("/api/episodes/history?") {
                return ok(json!({"data": h.eps.lock().unwrap().clone(), "total": 1000}));
            }
            if r.path.starts_with("/api/movies/history?") {
                return ok(json!({"data": h.movies.lock().unwrap().clone(), "total": 1000}));
            }
            status(404)
        })
        .await
    }

    #[tokio::test]
    async fn history_feeds_the_ro_select_list() {
        let d = crate::jobs::testutil::temp_dir("ro-hist");
        let ep = d.join("Show - S01E02.mkv");
        let film = d.join("Film (2025) WEBDL-1080p.mkv");
        let old = d.join("Old.mkv");
        for v in [&ep, &film, &old] {
            fs::write(v, b"video").unwrap();
        }
        let s = |p: &Path, suffix: &str| sibling(p, suffix).to_string_lossy().into_owned();
        let h = Arc::new(Hist {
            eps: Mutex::new(vec![
                hist(1, "ro", &s(&ep, "ro.srt"), "2 hours ago", ep_ids()),
                hist(6, "ro", &s(&old, "ro.srt"), "3 hours ago", ep_ids()),
                hist(1, "ro", &s(&old, "ro.srt"), "2 days ago", ep_ids()),
            ]),
            movies: Mutex::new(vec![hist(1, "ro", &s(&film, "ro.srt"), "5 hours ago", movie_ids()), hist(1, "en", &s(&film, "en.srt"), "5 hours ago", movie_ids())]),
            up: Mutex::new(true),
        });
        let f = bazarr_history(h.clone()).await;
        let (a, _cfg) = app(&f.base, false, json!({}));
        let now = 1_000_000.0;
        let mut tr = TrState::default();
        let mut quiet = Quiet::default();
        // First run: the downloads of the last day, episodes in the TV section, movies in theirs.
        assert!(ro_from_history(&a, &mut tr, &mut quiet, 2, 1, now).await);
        assert_eq!(tr.ro_select, [entry(&ep.to_string_lossy(), 2, false, now), entry(&film.to_string_lossy(), 1, true, now)]);
        assert_eq!(tr.ro_history.ep.as_ref().map(Vec::len), Some(3));
        assert_eq!(tr.ro_history.movie.as_ref().map(Vec::len), Some(2));
        let reqs: Vec<String> = f.requests().into_iter().map(|r| r.path).collect();
        assert_eq!(reqs, ["/api/episodes/history?start=0&length=100", "/api/movies/history?start=0&length=100"]);
        assert!(f.requests().iter().all(|r| r.header("x-api-key") == Some("b")));
        // Nothing new: nothing queued, nothing changed.
        tr.ro_select.clear();
        assert!(!ro_from_history(&a, &mut tr, &mut quiet, 2, 1, now + 180.0).await);
        assert!(tr.ro_select.is_empty());
        // Bazarr down: nothing queued, the cursor stays.
        let cursor = tr.ro_history.clone();
        h.eps.lock().unwrap().insert(0, hist(3, "ro", &s(&old, "ro.srt"), "now", ep_ids()));
        *h.up.lock().unwrap() = false;
        assert!(!ro_from_history(&a, &mut tr, &mut quiet, 2, 1, now + 360.0).await);
        assert!(tr.ro_select.is_empty());
        assert_eq!(tr.ro_history, cursor);
        // Back up: the upgrade made meanwhile is queued.
        *h.up.lock().unwrap() = true;
        assert!(ro_from_history(&a, &mut tr, &mut quiet, 2, 1, now + 540.0).await);
        assert_eq!(paths(&tr), [old.to_string_lossy()]);
        assert_eq!(tr.ro_history.ep.as_ref().map(Vec::len), Some(4));
        // No Plex token: Bazarr is not even asked.
        let no_token = json!({"services": {"plex": {"token": "", "token_registry_key": "Software\\PlexCouchTest\\Nope", "token_registry_value": "Nope"}}});
        let (a2, _cfg2) = app(&f.base, false, no_token);
        let n = f.requests().len();
        assert!(!ro_from_history(&a2, &mut TrState::default(), &mut quiet, 2, 1, now).await);
        assert_eq!(f.requests().len(), n);
        let _ = fs::remove_dir_all(&d);
    }
}
