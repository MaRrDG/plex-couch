//! Poster/backdrop image routes `/img/<kind>/<id>` (spec 3.6).
//!
//! Done (Phase 1): path parsing and URL building, the route itself (`http::routes`).
//! TODO(Phase 2a): [`poster`]: fetch from Radarr/Plex through `services` (Radarr
//! `mediacover/<id>/poster-500.jpg` then `poster.jpg`, `fanart-360.jpg` then `fanart.jpg`; Plex
//! photo transcoder 240x360 `thumb` / 720x405 `art`), and the demo SVG posters
//! (demo.py:31-84) when `app.demo`.

use crate::app::AppState;
use std::sync::Arc;

/// movie: Radarr poster, plex: Plex poster, plex-art / movie-art: backdrops.
pub const IMG_KINDS: [&str; 4] = ["movie", "plex", "plex-art", "movie-art"];

fn ascii_id(s: &str) -> bool {
    (1..=10).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// `^/img/(movie|plex|plex-art|movie-art)/([0-9]{1,10})$`, full match.
pub fn parse_img_path(path: Option<&str>) -> Option<(&'static str, String)> {
    let rest = path?.strip_prefix("/img/")?;
    let (kind, ident) = rest.split_once('/')?;
    let kind = IMG_KINDS.iter().find(|k| **k == kind)?;
    ascii_id(ident).then(|| (*kind, ident.to_string()))
}

/// `/img/<kind>/<id>` for a known kind and an id of 1-10 ASCII digits, else None.
pub fn poster_url(kind: &str, ident: &str) -> Option<String> {
    (IMG_KINDS.contains(&kind) && ascii_id(ident)).then(|| format!("/img/{kind}/{ident}"))
}

/// `(bytes, content type)` for `/img/<kind>/<ident>` (ident: 1-10 ASCII digits), or None.
///
/// TODO(Phase 2a): implement (real upstream fetch; demo SVGs when `app.demo`).
pub async fn poster(app: &Arc<AppState>, kind: &str, ident: &str) -> Option<(Vec<u8>, String)> {
    let _ = (app, kind, ident);
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // py: test_logic.TestImgRoutes.test_parse_img_path
    #[test]
    fn parse_img_paths() {
        assert_eq!(parse_img_path(Some("/img/movie/12")), Some(("movie", "12".into())));
        assert_eq!(parse_img_path(Some("/img/plex/1")), Some(("plex", "1".into())));
        assert_eq!(parse_img_path(Some("/img/plex-art/1")), Some(("plex-art", "1".into())));
        assert_eq!(parse_img_path(Some("/img/movie-art/22")), Some(("movie-art", "22".into())));
        for p in ["/img/plex-art/x", "/img/movie-art/", "/img/art/1", "/img/plex-art/1/2"] {
            assert_eq!(parse_img_path(Some(p)), None, "{p}");
        }
        for p in ["/img/movie/abc", "/img/movie/", "/img/movie/1a", "/img/movie/-1", "/img/movie/1/",
                  "/img/movie/../x", "/img/movie/%31", "/img/movie/1 ", "/img/other/1",
                  "/img/movie/\u{ff11}\u{ff12}", "/img/movie/\u{0661}", "/img/movie/12345678901",
                  "/img/plex/1?x", "/img/movie/1.jpg", "/img/movie/1\n", ""] {
            assert_eq!(parse_img_path(Some(p)), None, "{p:?}");
        }
        assert_eq!(parse_img_path(None), None);
    }

    // py: test_logic.TestImgRoutes.test_poster_url
    #[test]
    fn poster_urls() {
        assert_eq!(poster_url("movie", "2").as_deref(), Some("/img/movie/2"));
        assert_eq!(poster_url("plex", "101").as_deref(), Some("/img/plex/101"));
        for ident in ["", "x1", "\u{0661}\u{0662}", "1/2"] {
            assert_eq!(poster_url("movie", ident), None, "{ident}");
        }
        assert_eq!(poster_url("tv", "1"), None);
    }
}
