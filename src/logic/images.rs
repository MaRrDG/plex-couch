//! Poster/backdrop image routes `/img/<kind>/<id>` (spec 3.6).
//!
//! Path parsing and URL building here, the route in `http::routes`. [`poster`] fetches
//! through `services` (Radarr `mediacover/<id>/poster-500.jpg` then `poster.jpg`,
//! `fanart-360.jpg` then `fanart.jpg`; the Plex photo transcoder 240x360 `thumb` / 720x405
//! `art`), or draws the demo SVG posters when `app.demo`.

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
pub async fn poster(app: &Arc<AppState>, kind: &str, ident: &str) -> Option<(Vec<u8>, String)> {
    if app.demo {
        return crate::logic::demo::poster(kind, ident);
    }
    let s = &app.services;
    match kind {
        "movie" => s.radarr.poster(ident).await,
        "plex" => s.plex.poster(ident).await,
        "movie-art" => s.radarr.fanart(ident).await,
        "plex-art" => s.plex.art(ident).await,
        _ => None,
    }
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

    use crate::logic::testutil::{self, serve, status_answer, Hit};
    use serde_json::json;

    fn img(ct: &str, body: &[u8]) -> testutil::Answer {
        (200, ct.into(), body.to_vec())
    }

    #[tokio::test]
    async fn real_fetch_from_radarr_and_plex() {
        let radarr = serve(Arc::new(|h: &Hit| match h.path.as_str() {
            "/api/v3/mediacover/2/poster-500.jpg" => status_answer(404),
            "/api/v3/mediacover/2/poster.jpg" => img("image/png; charset=x", b"PNG2"),
            "/api/v3/mediacover/2/fanart-360.jpg" => img("text/plain", b"FAN"),
            "/api/v3/mediacover/3/poster-500.jpg" => img("image/jpeg", b""),
            "/api/v3/mediacover/4/poster-500.jpg" => img("image/jpeg", &vec![0u8; crate::services::IMG_MAX_BYTES + 1]),
            "/api/v3/mediacover/5/poster-500.jpg" => img("IMAGE/WEBP", b"W"),
            _ => status_answer(404),
        }))
        .await;
        let plex = serve(Arc::new(|h: &Hit| {
            if h.path.starts_with("/photo/:/transcode?") {
                img("image/jpeg", b"PLEX")
            } else {
                status_answer(404)
            }
        }))
        .await;
        let cfg = testutil::config(json!({"services": {"radarr": {"url": radarr.url}, "plex": {"url": plex.url}}}));
        let app = testutil::app(cfg);
        assert_eq!(poster(&app, "movie", "2").await, Some((b"PNG2".to_vec(), "image/png".into())));
        assert_eq!(poster(&app, "movie-art", "2").await, Some((b"FAN".to_vec(), "image/jpeg".into())));
        assert_eq!(poster(&app, "movie", "3").await, None); // empty body, then poster.jpg 404
        assert_eq!(poster(&app, "movie", "4").await, None); // over 4 MiB, then 404
        assert_eq!(poster(&app, "movie", "5").await, Some((b"W".to_vec(), "image/webp".into())));
        let paths = radarr.paths();
        assert!(paths.contains(&"/api/v3/mediacover/3/poster.jpg".to_string()));
        assert!(radarr.hits().iter().all(|h| h.header("X-Api-Key") == Some("radarr-test-key") && h.header("Accept") == Some("image/*")));
        assert_eq!(poster(&app, "plex", "40").await, Some((b"PLEX".to_vec(), "image/jpeg".into())));
        assert_eq!(poster(&app, "plex-art", "40").await.map(|x| x.0), Some(b"PLEX".to_vec()));
        let p = plex.paths();
        assert_eq!(p[0], "/photo/:/transcode?width=240&height=360&minSize=1&upscale=1&url=%2Flibrary%2Fmetadata%2F40%2Fthumb");
        assert_eq!(p[1], "/photo/:/transcode?width=720&height=405&minSize=1&upscale=1&url=%2Flibrary%2Fmetadata%2F40%2Fart");
        assert_eq!(plex.hits()[0].header("X-Plex-Token"), Some("plex-test-token"));
        // ids that are not ASCII digits never reach an upstream
        let before = (radarr.hits().len(), plex.hits().len());
        assert_eq!(app.services.radarr.poster("1a").await, None);
        assert_eq!(app.services.plex.poster("").await, None);
        assert_eq!(poster(&app, "tv", "1").await, None);
        assert_eq!((radarr.hits().len(), plex.hits().len()), before);
    }

    #[tokio::test]
    async fn plex_without_a_token_sends_no_header() {
        let plex = serve(Arc::new(|_: &Hit| img("image/jpeg", b"P"))).await;
        let cfg = testutil::config(json!({"services": {"plex": {"url": plex.url, "token": "",
                                                                "token_registry_key": "Software\\PlexCouchTest\\Nope",
                                                                "token_registry_value": "Nope"}}}));
        let app = testutil::app(cfg);
        assert!(poster(&app, "plex", "1").await.is_some());
        assert_eq!(plex.hits()[0].header("X-Plex-Token"), None);
    }

    #[tokio::test]
    async fn demo_posters() {
        let app = testutil::app_with(testutil::config(json!({})), crate::app::AppOptions { demo: true, ..Default::default() });
        let (svg, ct) = poster(&app, "plex", "101").await.unwrap();
        assert_eq!(ct, "image/svg+xml");
        assert!(svg.starts_with(b"<svg"));
        assert!(poster(&app, "movie-art", "6").await.is_some());
        assert!(poster(&app, "movie", "9").await.is_none());
    }
}
