//! The workbench's stylesheets and scripts as static assets (#1220).
//!
//! Every HTML page links them instead of inlining them, so a browser fetches
//! each once and keeps it. Each is served at a versioned URL, the FNV-1a hash
//! of its bytes in the path (`{base}/workbench/{hash}/map.js`), and that URL
//! is `immutable`: a new build that changes a file changes its URL. The route
//! is server-wide, below the external base URL like `/preview/vendor/`, so a
//! page on any API mount (`/edr`, `/features`, `/maps`, `/tiles`) or on the
//! shared root links the same URL and shares one cached copy. The server
//! mounts [`router`] at its root.
use std::sync::LazyLock;

use axum::body::Body;
use axum::extract::Path;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use ds_core::http_cache::{etag_of, if_none_match_matches};

/// The route the server mounts: a hash, then the file name.
pub const ROUTE: &str = "/workbench/{version}/{file}";

/// A versioned URL is never reused for other bytes: a year, `immutable`.
pub const CACHE_IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// A URL whose hash is not the current one, from a page rendered by another
/// build (an open tab across a deploy, or a fleet mid-rollout): the current
/// bytes, so the page still works, but never kept under that URL.
pub const CACHE_STALE: &str = "no-cache";

struct Asset {
    name: &'static str,
    media_type: &'static str,
    body: &'static str,
}

const ASSETS: [Asset; 4] = [
    Asset {
        name: "style.css",
        media_type: "text/css; charset=utf-8",
        body: include_str!("style.css"),
    },
    Asset {
        name: "app.js",
        media_type: "text/javascript; charset=utf-8",
        body: include_str!("app.js"),
    },
    Asset {
        name: "map.css",
        media_type: "text/css; charset=utf-8",
        body: include_str!("map.css"),
    },
    Asset {
        name: "map.js",
        media_type: "text/javascript; charset=utf-8",
        body: include_str!("map.js"),
    },
];

/// Each asset's strong ETag, `"<16 hex>"`, whose hex is also its URL's
/// version. FNV-1a is stable across toolchains and instances
/// (`ds_core::http_cache::etag_of`), so every server of one build links the
/// same URLs. Hashed once, on first use.
static ETAGS: LazyLock<[String; 4]> = LazyLock::new(|| {
    ASSETS
        .each_ref()
        .map(|asset| etag_of(asset.body.as_bytes()))
});

fn find(name: &str) -> Option<(&'static Asset, &'static str)> {
    let index = ASSETS.iter().position(|asset| asset.name == name)?;
    Some((&ASSETS[index], ETAGS[index].as_str()))
}

/// The versioned URL of the asset `name` below the external base URL.
///
/// # Panics
/// For a name that is not one of the workbench's assets: a programming error.
pub fn href(base: &str, name: &str) -> String {
    let (asset, etag) = find(name).unwrap_or_else(|| panic!("no workbench asset {name}"));
    format!("{base}/workbench/{}/{}", etag.trim_matches('"'), asset.name)
}

/// The router serving the assets at [`ROUTE`], for the server's root.
pub fn router() -> Router {
    Router::new().route(ROUTE, get(asset))
}

/// `GET /workbench/{version}/{file}`. The current version is answered
/// `immutable` and any other with `no-cache`, both with the current bytes;
/// `If-None-Match` with the ETag is a 304. An unknown file is a plain 404
/// that does not echo the path.
pub async fn asset(Path((version, file)): Path<(String, String)>, headers: HeaderMap) -> Response {
    let nosniff = (
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    let Some((asset, etag)) = find(&file) else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .header(nosniff.0, nosniff.1)
            .body(Body::from("Not Found"))
            .expect("static 404 response headers are valid");
    };
    let cache = if etag.trim_matches('"') == version {
        CACHE_IMMUTABLE
    } else {
        CACHE_STALE
    };
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| if_none_match_matches(value, etag));
    let builder = Response::builder()
        .header(header::CACHE_CONTROL, cache)
        .header(header::ETAG, etag)
        .header(nosniff.0, nosniff.1);
    if fresh {
        return builder
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .expect("static 304 response headers are valid");
    }
    builder
        .header(header::CONTENT_TYPE, asset.media_type)
        .body(Body::from(asset.body))
        .expect("static asset response headers are valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    async fn get(uri: &str, if_none_match: Option<&str>) -> Response {
        let mut request = axum::http::Request::builder().uri(uri);
        if let Some(tag) = if_none_match {
            request = request.header(header::IF_NONE_MATCH, tag);
        }
        router()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn text(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// The URL carries the content hash; it is answered `immutable` with
    /// the file's bytes, its media type and a strong ETag that a
    /// revalidation matches with a 304.
    #[tokio::test]
    async fn a_versioned_url_is_immutable_and_revalidates_by_etag() {
        for (name, media_type, body) in [
            (
                "map.js",
                "text/javascript; charset=utf-8",
                include_str!("map.js"),
            ),
            (
                "map.css",
                "text/css; charset=utf-8",
                include_str!("map.css"),
            ),
            (
                "style.css",
                "text/css; charset=utf-8",
                include_str!("style.css"),
            ),
            (
                "app.js",
                "text/javascript; charset=utf-8",
                include_str!("app.js"),
            ),
        ] {
            let href = href("https://example.test/prefix", name);
            let path = href.strip_prefix("https://example.test/prefix").unwrap();
            let version = etag_of(body.as_bytes());
            assert_eq!(
                path,
                format!("/workbench/{}/{name}", version.trim_matches('"'))
            );
            let response = get(path, None).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_TYPE], media_type);
            assert_eq!(response.headers()[header::CACHE_CONTROL], CACHE_IMMUTABLE);
            assert_eq!(response.headers()[header::ETAG], version.as_str());
            assert_eq!(
                response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                "nosniff"
            );
            assert_eq!(text(response).await, body);
            let again = get(path, Some(&format!("\"other\", {version}"))).await;
            assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
            assert_eq!(again.headers()[header::ETAG], version.as_str());
            assert_eq!(again.headers()[header::CACHE_CONTROL], CACHE_IMMUTABLE);
            assert!(text(again).await.is_empty());
        }
    }

    /// Another build's URL still gets the current file, but `no-cache`; an
    /// unknown file is a 404 that does not echo the request.
    #[tokio::test]
    async fn another_version_is_served_uncached_and_unknown_files_are_404() {
        let response = get("/workbench/0123456789abcdef/map.js", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], CACHE_STALE);
        assert_eq!(text(response).await, include_str!("map.js"));
        let missing = get("/workbench/0123456789abcdef/%3Cscript%3E.js", None).await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(missing.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(text(missing).await, "Not Found");
    }

    /// One edit changes one URL: the hash is the file's own.
    #[test]
    fn each_asset_has_its_own_version() {
        let versions: std::collections::BTreeSet<String> =
            ["style.css", "app.js", "map.css", "map.js"]
                .into_iter()
                .map(|name| href("", name))
                .collect();
        assert_eq!(versions.len(), 4);
    }
}
