//! `GET /robots.txt`: crawlers may index the landing pages, the API
//! definitions, the conformance declarations, the collection catalogs and
//! each collection's own page, at the shared OGC API root and under `/edr`.
//! Everything below a collection is off limits: items, data queries, model
//! runs, maps and tiles are generated per request, and an archive store's run
//! list alone links thousands of them. So is every other path, including the
//! per-API services the landing page no longer advertises, WMS, 3D Tiles,
//! `/preview`, `/health` and `/metrics`. The HTML pages' own stylesheets
//! and scripts under `/workbench/` (#1220) are crawlable, so a crawler that
//! renders a page sees it as a browser does.
//!
//! The paths assume the server answers at the host root, as robots.txt
//! itself must. When adding a crawlable root or catalog, add its rules here
//! and its paths to the tests.

use axum::http::header;
use axum::response::{IntoResponse, Response};

/// RFC 9309 rules. Major crawlers apply the longest matching rule, so for
/// them the order does not matter. For first-match parsers, the specific
/// `Disallow`s come before the `Allow`s that also match those paths, and
/// the catch-all `Disallow: /` comes last.
pub const ROBOTS_TXT: &str = "\
# MeteoCore: crawl the landing pages, API definitions, conformance, the
# collection lists and each collection's own page, nothing below a
# collection and nothing else, and the pages' own styles and scripts.
User-agent: *
Disallow: /collections/*/
Disallow: /edr/collections/*/
Allow: /$
Allow: /?
Allow: /api
Allow: /conformance
Allow: /collections$
Allow: /collections?
Allow: /collections/
Allow: /edr$
Allow: /edr?
Allow: /edr/$
Allow: /edr/?
Allow: /edr/api
Allow: /edr/conformance
Allow: /edr/collections$
Allow: /edr/collections?
Allow: /edr/collections/
Allow: /workbench/
Disallow: /
";

/// `GET /robots.txt`.
pub async fn handler() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        ROBOTS_TXT,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use robotstxt::DefaultMatcher;

    /// Google's own matcher (a Rust port), so the verdicts are the ones
    /// Googlebot reaches, not this crate's reading of RFC 9309.
    fn allowed(path: &str) -> bool {
        let url = format!("https://example.com{path}");
        DefaultMatcher::default().one_agent_allowed_by_robots(ROBOTS_TXT, "Googlebot", &url)
    }

    #[test]
    fn landing_pages_documents_catalogs_and_collection_pages_are_crawlable() {
        for path in [
            "/",
            "/?f=json",
            "/api",
            "/api?f=html",
            "/api/docs",
            "/conformance",
            "/conformance?f=json",
            "/collections",
            "/collections?f=html",
            "/collections?q=radar&f=html",
            "/collections/fmi-radar-composite-dbz",
            "/collections/fmi-radar-composite-dbz?f=json",
            "/edr",
            "/edr/",
            "/edr?f=json",
            "/edr/api",
            "/edr/api/docs",
            "/edr/conformance",
            "/edr/conformance?f=html",
            "/edr/collections",
            "/edr/collections?f=json",
            "/edr/collections/dwd-icon-eu",
            "/edr/collections/dwd-icon-eu?f=html",
            // The pages' stylesheets and scripts, for a crawler that
            // renders them.
            "/workbench/0123456789abcdef/style.css",
            "/workbench/0123456789abcdef/map.js",
        ] {
            assert!(allowed(path), "{path} should be crawlable");
        }
    }

    #[test]
    fn nothing_below_a_collection_is_crawlable() {
        for path in [
            "/collections/cap-alerts/items",
            "/collections/cap-alerts/items/abc?f=html",
            "/collections/radar/map?f=png",
            "/collections/radar/styles/default/map/tiles/WebMercatorQuad/3/4/5",
            "/collections/radar/tiles",
            "/collections/radar/",
            "/edr/collections/dwd-icon-eu/instances",
            "/edr/collections/dwd-icon-eu/instances?limit=100&offset=100",
            "/edr/collections/dwd-icon-eu/instances/2026-04-14T12:00:00Z",
            // The truncated ids an external crawler requests by the hundred.
            "/edr/collections/dwd-icon-eu/instances/2026-04-14T12",
            "/edr/collections/dwd-icon-eu/position?coords=POINT(25%2060)",
            "/edr/collections/dwd-icon-eu/area",
            "/edr/collections/obs/locations/101004",
        ] {
            assert!(!allowed(path), "{path} should not be crawlable");
        }
    }

    #[test]
    fn other_services_are_not_crawlable() {
        for path in [
            "/tileMatrixSets",
            "/collectionsx",
            "/edrx",
            "/features/api",
            "/features/conformance",
            "/features/collections",
            "/features/collections/cap-alerts",
            "/maps/collections/radar",
            "/tiles/collections/radar",
            "/wms?SERVICE=WMS&REQUEST=GetCapabilities",
            "/3dtiles/",
            "/3dtiles/collections/fivih",
            "/preview",
            "/preview/manifest.json",
            "/workbenchx",
            "/health",
            "/metrics",
            "/mcp",
            "/admin/collections/reload",
        ] {
            assert!(!allowed(path), "{path} should not be crawlable");
        }
    }

    #[tokio::test]
    async fn served_as_cacheable_plain_text() {
        let response = handler().await;
        let headers = response.headers();
        assert_eq!(headers[header::CONTENT_TYPE], "text/plain; charset=utf-8");
        assert_eq!(headers[header::CACHE_CONTROL], "public, max-age=86400");
        let body = http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(body, ROBOTS_TXT.as_bytes());
    }
}
