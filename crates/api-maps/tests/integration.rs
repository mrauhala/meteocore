use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use api_maps::MapsState;
use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use ds_render::{BuiltinColormap, LutColorMap, RenderedCache, StyleInfo};

// ---------------------------------------------------------------------------
// Mock engine
// ---------------------------------------------------------------------------

struct MockMapEngine;

impl MockMapEngine {
    fn new() -> Self {
        Self
    }

    fn make_info() -> RasterInfo {
        RasterInfo {
            // CRS:84 (lon-first) is what real WGS84 engines emit.
            native_crs: "CRS:84".to_string(),
            spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
            times: vec![
                chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&chrono::Utc),
                chrono::DateTime::parse_from_rfc3339("2024-01-01T01:00:00Z")
                    .unwrap()
                    .with_timezone(&chrono::Utc),
            ],
            parameter: "reflectivity".to_string(),
            unit: "dBZ".to_string(),
            parameters: vec![],
            vertical: None,
            // bbox [10,55,30,70] over 2000x1500 cells => 0.01° per cell.
            grid_size: Some([2000, 1500]),
        }
    }
}

impl MapEngine for MockMapEngine {
    fn get_raster_tile(
        &self,
        _bbox: [f64; 4],
        width: u32,
        height: u32,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        _output_crs: &OutputCrs,
        _parameter: Option<&str>,
        _z: Option<f64>,
    ) -> Result<RasterTile, DataServerError> {
        let pixel_count = (width * height) as usize;
        let values: Vec<Option<f64>> = (0..pixel_count)
            .map(|i| Some(i as f64 / pixel_count as f64))
            .collect();
        Ok(RasterTile {
            width,
            height,
            values,
        })
    }

    fn raster_info(&self) -> RasterInfo {
        Self::make_info()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_router() -> axum::Router {
    build_router_with_apis(vec!["maps".to_string()])
}

fn build_router_with_apis(apis: Vec<String>) -> axum::Router {
    let engine: Arc<dyn MapEngine> = Arc::new(MockMapEngine::new());
    let mut engines = HashMap::new();
    let mut collections = HashMap::new();
    let mut styles_map = HashMap::new();

    engines.insert("radar".to_string(), engine);
    collections.insert(
        "radar".to_string(),
        CollectionConfig {
            id: "radar".to_string(),
            title: "Test Radar".to_string(),
            description: "Test radar data".to_string(),
            data_path: None,
            apis,
            engine_type: "geotiff".to_string(),
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            odim: None,
            postgis: None,
            preview: None,
        },
    );

    let cmap = Arc::new(LutColorMap::from_builtin(
        BuiltinColormap::Viridis,
        0.0,
        1.0,
    ));
    let mut layer_styles = HashMap::new();
    layer_styles.insert(
        "default".to_string(),
        StyleInfo {
            name: "default".to_string(),
            title: "Default".to_string(),
            colormap: cmap.clone(),
            min: 0.0,
            max: 1.0,
            parameter: None,
        },
    );
    layer_styles.insert(
        "grayscale".to_string(),
        StyleInfo {
            name: "grayscale".to_string(),
            title: "Grayscale".to_string(),
            colormap: Arc::new(LutColorMap::from_builtin(
                BuiltinColormap::Grayscale,
                0.0,
                1.0,
            )),
            min: 0.0,
            max: 1.0,
            parameter: None,
        },
    );
    styles_map.insert("radar".to_string(), layer_styles);

    let state = Arc::new(ArcSwap::from_pointee(MapsState {
        engines,
        collections,
        styles: styles_map,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        base_url: String::new(),
    }));
    api_maps::router(state)
}

async fn get(uri: &str) -> (StatusCode, Value) {
    get_on(build_router(), uri).await
}

async fn get_with_apis(uri: &str, apis: Vec<String>) -> (StatusCode, Value) {
    get_on(build_router_with_apis(apis), uri).await
}

async fn get_on(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap();
    (status, json)
}

async fn get_raw(uri: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let app = build_router();
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

/// Fetch the collection JSON for an ad-hoc single-engine router. Used by the
/// extent edge-case tests that need a bespoke `RasterInfo`.
async fn fetch_collection_json(engine: Arc<dyn MapEngine>, id: &str, apis: Vec<String>) -> Value {
    let mut engines = HashMap::new();
    let mut collections = HashMap::new();
    engines.insert(id.to_string(), engine);
    collections.insert(
        id.to_string(),
        CollectionConfig {
            id: id.to_string(),
            title: "Test".to_string(),
            description: "Test".to_string(),
            data_path: None,
            apis,
            engine_type: "geotiff".to_string(),
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            odim: None,
            postgis: None,
            preview: None,
        },
    );
    let state = Arc::new(ArcSwap::from_pointee(MapsState {
        engines,
        collections,
        styles: HashMap::new(),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        base_url: String::new(),
    }));
    let (_, json) = get_on(api_maps::router(state), &format!("/collections/{id}")).await;
    json
}

// ---------------------------------------------------------------------------
// Landing page tests
// ---------------------------------------------------------------------------

mod landing_page {
    use super::*;

    #[tokio::test]
    async fn returns_200() {
        let (status, _) = get("/").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn has_title() {
        let (_, json) = get("/").await;
        assert!(json["title"].is_string());
    }

    #[tokio::test]
    async fn has_required_links() {
        let (_, json) = get("/").await;
        let links = json["links"].as_array().unwrap();
        assert!(links.iter().any(|l| l["rel"] == "self"));
        assert!(links.iter().any(|l| l["rel"] == "conformance"));
        assert!(links.iter().any(|l| l["rel"] == "data"));
    }
}

// ---------------------------------------------------------------------------
// Conformance tests
// ---------------------------------------------------------------------------

mod conformance {
    use super::*;

    #[tokio::test]
    async fn returns_200() {
        let (status, _) = get("/conformance").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn has_conforms_to() {
        let (_, json) = get("/conformance").await;
        assert!(json["conformsTo"].is_array());
    }

    #[tokio::test]
    async fn declares_core() {
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        assert!(classes
            .iter()
            .any(|c| c.as_str().unwrap().contains("conf/core")));
    }

    #[tokio::test]
    async fn declares_collection_map() {
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        assert!(classes
            .iter()
            .any(|c| c.as_str().unwrap().contains("conf/collection-map")));
    }

    #[tokio::test]
    async fn declares_styled_map() {
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        assert!(classes
            .iter()
            .any(|c| c.as_str().unwrap().contains("conf/styled-map")));
    }

    #[tokio::test]
    async fn declares_png() {
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        assert!(classes
            .iter()
            .any(|c| c.as_str().unwrap().contains("conf/png")));
    }

    #[tokio::test]
    async fn omits_map_tilesets_conformance_class() {
        // We link to map tilesets (tilesets-map rel) but do NOT implement the
        // Map Tilesets class's /map/tiles endpoints, so the class must not be
        // declared — that would be a false conformance claim. (Tiles are
        // served by the standalone OGC API Tiles service.)
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        assert!(!classes
            .iter()
            .any(|c| c.as_str().unwrap().contains("conf/tilesets")));
    }
}

// ---------------------------------------------------------------------------
// Collections tests
// ---------------------------------------------------------------------------

mod collections {
    use super::*;

    #[tokio::test]
    async fn returns_200() {
        let (status, _) = get("/collections").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn has_collections_array() {
        let (_, json) = get("/collections").await;
        assert!(json["collections"].is_array());
        assert!(!json["collections"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn each_collection_has_id_and_title() {
        let (_, json) = get("/collections").await;
        for c in json["collections"].as_array().unwrap() {
            assert!(c["id"].is_string());
            assert!(c["title"].is_string());
        }
    }

    #[tokio::test]
    async fn collection_has_crs_and_styles() {
        let (_, json) = get("/collections").await;
        let c = &json["collections"][0];
        assert!(c["crs"].is_array());
        assert!(c["styles"].is_array());
    }

    #[tokio::test]
    async fn collection_has_extent() {
        let (_, json) = get("/collections").await;
        let c = &json["collections"][0];
        assert!(c["extent"]["spatial"]["bbox"].is_array());
    }

    #[tokio::test]
    async fn collection_has_temporal_extent() {
        let (_, json) = get("/collections").await;
        let c = &json["collections"][0];
        assert!(c["extent"]["temporal"]["interval"].is_array());
    }

    #[tokio::test]
    async fn collection_has_links() {
        let (_, json) = get("/collections").await;
        let c = &json["collections"][0];
        let links = c["links"].as_array().unwrap();
        assert!(links.iter().any(|l| l["rel"] == "self"));
        assert!(links.iter().any(|l| l["rel"] == "map"));
        assert!(links.iter().any(|l| l["rel"] == "styles"));
    }

    #[tokio::test]
    async fn collection_detail_returns_200() {
        let (status, _) = get("/collections/radar").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn collection_detail_has_id() {
        let (_, json) = get("/collections/radar").await;
        assert_eq!(json["id"], "radar");
    }

    #[tokio::test]
    async fn collection_omits_nonstandard_apis_field() {
        // `apis` is a vendor extension with no OGC schema; it must not leak
        // into the standard collection JSON.
        let (_, json) = get("/collections/radar").await;
        assert!(
            json.get("apis").is_none(),
            "apis must not be present in the standard collection JSON"
        );
    }

    #[tokio::test]
    async fn collection_advertises_storage_crs() {
        let (_, json) = get("/collections/radar").await;
        // WGS84 data is lon-first -> CRS84 URI, not the lat-first EPSG:4326 one.
        assert_eq!(
            json["storageCrs"],
            "http://www.opengis.net/def/crs/OGC/1.3/CRS84"
        );
    }

    #[tokio::test]
    async fn spatial_extent_has_grid_resolution() {
        let (_, json) = get("/collections/radar").await;
        let grid = json["extent"]["spatial"]["grid"]
            .as_array()
            .expect("spatial.grid must be present");
        assert_eq!(grid.len(), 2, "one grid axis per spatial dimension");
        // bbox [10,55,30,70] over 2000x1500 cells => 0.01° per cell.
        assert_eq!(grid[0]["cellsCount"], 2000);
        assert_eq!(grid[1]["cellsCount"], 1500);
        assert!((grid[0]["resolution"].as_f64().unwrap() - 0.01).abs() < 1e-9);
        assert!((grid[1]["resolution"].as_f64().unwrap() - 0.01).abs() < 1e-9);
    }

    #[tokio::test]
    async fn temporal_extent_has_regular_grid_resolution() {
        let (_, json) = get("/collections/radar").await;
        let grid = &json["extent"]["temporal"]["grid"];
        // Two timestamps one hour apart => regular PT1H step.
        assert_eq!(grid["cellsCount"], 2);
        assert_eq!(grid["resolution"], "PT1H");
    }

    #[tokio::test]
    async fn collection_omits_tilesets_map_link_without_tiles_api() {
        let (_, json) = get("/collections/radar").await;
        let links = json["links"].as_array().unwrap();
        assert!(!links
            .iter()
            .any(|l| l["rel"] == "http://www.opengis.net/def/rel/ogc/1.0/tilesets-map"));
    }

    #[tokio::test]
    async fn collection_advertises_tilesets_map_link_with_tiles_api() {
        let (_, json) =
            get_with_apis("/collections/radar", vec!["maps".into(), "tiles".into()]).await;
        let links = json["links"].as_array().unwrap();
        let tileset_link = links
            .iter()
            .find(|l| l["rel"] == "http://www.opengis.net/def/rel/ogc/1.0/tilesets-map")
            .expect("tilesets-map link must be present when tiles API is enabled");
        assert!(tileset_link["href"]
            .as_str()
            .unwrap()
            .ends_with("/tiles/collections/radar/tiles"));
    }

    #[tokio::test]
    async fn unknown_collection_returns_404() {
        let (status, _) = get("/collections/nonexistent").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

// ---------------------------------------------------------------------------
// Styles tests
// ---------------------------------------------------------------------------

mod styles_endpoint {
    use super::*;

    #[tokio::test]
    async fn returns_200() {
        let (status, _) = get("/collections/radar/styles").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn has_styles_array() {
        let (_, json) = get("/collections/radar/styles").await;
        let styles = json["styles"].as_array().unwrap();
        assert!(!styles.is_empty());
    }

    #[tokio::test]
    async fn default_style_present() {
        let (_, json) = get("/collections/radar/styles").await;
        let styles = json["styles"].as_array().unwrap();
        assert!(styles.iter().any(|s| s["id"] == "default"));
    }

    #[tokio::test]
    async fn grayscale_style_present() {
        let (_, json) = get("/collections/radar/styles").await;
        let styles = json["styles"].as_array().unwrap();
        assert!(styles.iter().any(|s| s["id"] == "grayscale"));
    }

    #[tokio::test]
    async fn unknown_collection_returns_404() {
        let (status, _) = get("/collections/nonexistent/styles").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

// ---------------------------------------------------------------------------
// GetMap tests
// ---------------------------------------------------------------------------

mod get_map {
    use super::*;

    #[tokio::test]
    async fn returns_png() {
        let (status, headers, body) = get_raw("/collections/radar/map?bbox=10,55,30,70").await;
        assert_eq!(status, StatusCode::OK);
        let ct = headers.get("content-type").unwrap().to_str().unwrap();
        assert_eq!(ct, "image/png");
        // Check PNG magic bytes
        assert!(body.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[tokio::test]
    async fn has_cache_headers() {
        let (_, headers, _) = get_raw("/collections/radar/map?bbox=10,55,30,70").await;
        assert!(headers.contains_key("cache-control"));
        assert!(headers.contains_key("etag"));
    }

    /// `elevation` against a collection with no vertical dimension
    /// (`MockMapEngine.raster_info().vertical` is `None`) is a 400.
    #[tokio::test]
    async fn elevation_against_non_vertical_collection_returns_400() {
        let (status, _, _) = get_raw("/collections/radar/map?bbox=10,55,30,70&elevation=0.5").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn explicit_time_immutable_cache() {
        let (_, headers, _) =
            get_raw("/collections/radar/map?bbox=10,55,30,70&datetime=2024-01-01T00:00:00Z").await;
        let cc = headers.get("cache-control").unwrap().to_str().unwrap();
        assert!(cc.contains("immutable"));
    }

    #[tokio::test]
    async fn no_time_short_cache() {
        let (_, headers, _) = get_raw("/collections/radar/map?bbox=10,55,30,70").await;
        let cc = headers.get("cache-control").unwrap().to_str().unwrap();
        assert!(cc.contains("must-revalidate"));
    }

    #[tokio::test]
    async fn custom_dimensions() {
        let (status, _, body) =
            get_raw("/collections/radar/map?bbox=10,55,30,70&width=128&height=128").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[tokio::test]
    async fn jpeg_format() {
        let (status, headers, body) =
            get_raw("/collections/radar/map?bbox=10,55,30,70&f=image/jpeg").await;
        assert_eq!(status, StatusCode::OK);
        let ct = headers.get("content-type").unwrap().to_str().unwrap();
        assert_eq!(ct, "image/jpeg");
        assert!(body[0] == 0xFF && body[1] == 0xD8);
    }

    #[tokio::test]
    async fn missing_bbox_returns_400() {
        let (status, json) = get("/collections/radar/map").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(json["code"].is_string());
    }

    #[tokio::test]
    async fn invalid_bbox_returns_400() {
        let (status, _) = get("/collections/radar/map?bbox=invalid").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unknown_collection_returns_404() {
        let (status, _) = get("/collections/nonexistent/map?bbox=10,55,30,70").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unsupported_crs_returns_400() {
        let (status, _) = get("/collections/radar/map?bbox=10,55,30,70&crs=EPSG:9999").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unsupported_format_returns_400() {
        let (status, _) = get("/collections/radar/map?bbox=10,55,30,70&f=image/gif").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn dimension_too_large_returns_400() {
        let (status, _) = get("/collections/radar/map?bbox=10,55,30,70&width=9000").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn parameter_name_query_is_accepted_for_single_param_engine() {
        // `MockMapEngine.raster_info().parameters` is empty (single-band).
        // The handler accepts any `parameter-name=` and forwards it; the
        // engine ignores the value at render time per the trait contract.
        let (status, _, body) =
            get_raw("/collections/radar/map?bbox=10,55,30,70&parameter-name=anything").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    /// Regression for #145: the response `ETag` must be FNV-1a over the
    /// rendered bytes — not over the cache key — so a server-side fix that
    /// produces different pixels under the same key surfaces a fresh ETag
    /// and clients holding the stale entry refetch instead of receiving an
    /// infinite 304. Direct check: build the same `CachedRendered` the
    /// handler does and verify the header matches.
    #[tokio::test]
    async fn etag_is_content_derived_over_response_body() {
        let (status, headers, body) = get_raw("/collections/radar/map?bbox=10,55,30,70").await;
        assert_eq!(status, StatusCode::OK);
        let actual_etag = headers.get("etag").unwrap().to_str().unwrap();
        let expected_etag = ds_render::CachedRendered::new(bytes::Bytes::from(body))
            .etag()
            .to_string();
        assert_eq!(
            actual_etag, expected_etag,
            "ETag header must be FNV-1a over the response body (content-derived), \
             not derived from the CacheKey — see #145"
        );
    }

    /// Pin the cache-HIT→304 branch specifically. The handler returns 304
    /// from two places: the cache-HIT branch (this test, asserted via
    /// `x-cache: HIT`) and the post-render MISS branch (which also
    /// compares `If-None-Match` against the freshly-computed ETag and
    /// returns 304 when matched). A fresh router would still 304 — just
    /// via the MISS path — so the `x-cache` assertion is what makes
    /// "we exercised the HIT branch" testable. Sharing one router across
    /// both calls is how we warm the cache for that branch.
    #[tokio::test]
    async fn if_none_match_after_cache_warm_returns_304_via_cache_hit() {
        let app = build_router();
        // First request populates the rendered cache.
        let resp_a = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/collections/radar/map?bbox=10,55,30,70")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let etag = resp_a
            .headers()
            .get("etag")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        // Cache is warm. Same key + matching If-None-Match → cache HIT →
        // ETag compare against `cached.etag()` → 304 with `x-cache: HIT`.
        let req = Request::builder()
            .uri("/collections/radar/map?bbox=10,55,30,70")
            .header("If-None-Match", &etag)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(resp.headers().get("etag").unwrap().to_str().unwrap(), etag);
        assert_eq!(
            resp.headers().get("x-cache").map(|v| v.to_str().unwrap()),
            Some("HIT"),
            "304 must come from the cache-HIT branch, not post-render MISS"
        );
    }

    /// Pin the post-render MISS → 304 branch. Use a fresh router (no
    /// cache-warm) so the first `If-None-Match`-bearing request must
    /// go through the full render path; assert the 304 carries
    /// `x-cache: MISS` rather than the cache-HIT branch's `HIT`.
    #[tokio::test]
    async fn if_none_match_against_fresh_router_returns_304_via_miss_branch() {
        // Step 1: render once on a separate fresh router to learn the ETag.
        let etag = {
            let warm = build_router();
            let resp = warm
                .oneshot(
                    Request::builder()
                        .uri("/collections/radar/map?bbox=10,55,30,70")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            resp.headers()
                .get("etag")
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        };

        // Step 2: brand-new router with an empty cache. Handler must
        // render, compute the same content-derived ETag, match the
        // header, and 304 via the post-render branch.
        let app = build_router();
        let req = Request::builder()
            .uri("/collections/radar/map?bbox=10,55,30,70")
            .header("If-None-Match", &etag)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(resp.headers().get("etag").unwrap().to_str().unwrap(), etag);
        assert_eq!(
            resp.headers().get("x-cache").map(|v| v.to_str().unwrap()),
            Some("MISS"),
            "304 must come from the post-render MISS branch, not the cache-HIT branch"
        );
    }

    /// Cross-parameter staleness protection: different `parameter-name`
    /// values must produce different rendered bytes (because the
    /// `MultiParamMockEngine` varies its output by parameter), which under
    /// content-derived ETags (#145) means different ETags. Combined with
    /// `parameter` being part of the cache key, a client switching
    /// parameters can't get a 304 against the previous parameter's entry.
    #[tokio::test]
    async fn parameter_name_changes_content_etag_on_multi_param_engine() {
        let app = build_multi_param_router();
        let resp_2t = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/collections/wx/map?bbox=10,55,30,70&parameter-name=2t")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp_10u = app
            .oneshot(
                Request::builder()
                    .uri("/collections/wx/map?bbox=10,55,30,70&parameter-name=10u")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let etag_2t = resp_2t.headers().get("etag").unwrap().to_str().unwrap();
        let etag_10u = resp_10u.headers().get("etag").unwrap().to_str().unwrap();
        assert_ne!(
            etag_2t, etag_10u,
            "parameter-name varies the rendered pixels, so the content-derived \
             ETag must differ — otherwise a client switching from 2t to 10u \
             would get a stale 304"
        );
    }

    #[tokio::test]
    async fn unknown_parameter_name_returns_400_for_multi_param_engine() {
        let app = build_multi_param_router();
        let req = Request::builder()
            .uri("/collections/wx/map?bbox=10,55,30,70&parameter-name=nope")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let text = std::str::from_utf8(&body).unwrap();
        // Sorted alphabetically — "10u" < "2t" by lexicographic comparison.
        assert!(
            text.contains("Available: 10u, 2t"),
            "error body should list available parameters in sorted order; got: {text}"
        );
    }

    #[tokio::test]
    async fn known_parameter_name_is_accepted_by_multi_param_engine() {
        let app = build_multi_param_router();
        let req = Request::builder()
            .uri("/collections/wx/map?bbox=10,55,30,70&parameter-name=2t")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// Regression for #162: when the engine produces an all-nodata tile,
    /// the fast path emits PNG bytes without going through the
    /// format-aware encoder. Before this fix the response carried the
    /// *requested* Content-Type (e.g. image/jpeg) over PNG bytes,
    /// breaking decoders that trust the header. Both header and body
    /// must agree.
    #[tokio::test]
    async fn empty_tile_forces_png_content_type_even_when_jpeg_requested() {
        let app = build_empty_router();
        let req = Request::builder()
            .uri("/collections/empty/map?bbox=10,55,30,70&f=image/jpeg")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let headers = resp.headers().clone();
        assert_eq!(
            headers.get("content-type").unwrap(),
            "image/png",
            "empty-tile response must self-declare PNG, not the requested image/jpeg"
        );
        assert_eq!(headers.get("x-cache").unwrap(), "EMPTY");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(
            body.starts_with(&[0x89, b'P', b'N', b'G']),
            "body must be a real PNG, got first bytes {:?}",
            &body[..4.min(body.len())]
        );
    }

    #[tokio::test]
    async fn empty_tile_forces_png_content_type_even_when_webp_requested() {
        // Same regression as above, second format for symmetry — WebP
        // takes a different `ImageFormat::Webp` branch in the cache
        // key, so this exercises a distinct code path.
        let app = build_empty_router();
        let req = Request::builder()
            .uri("/collections/empty/map?bbox=10,55,30,70&f=image/webp")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let headers = resp.headers().clone();
        assert_eq!(headers.get("content-type").unwrap(), "image/png");
        assert_eq!(headers.get("x-cache").unwrap(), "EMPTY");
    }

    /// Revalidating a cached empty-tile response must round-trip the
    /// `EMPTY` label. Empty tiles bypass `rendered_cache`, so an
    /// `If-None-Match` request always falls through to the post-render
    /// branch — exactly the branch the round-7 fix targets.
    #[tokio::test]
    async fn if_none_match_on_empty_tile_returns_304_with_x_cache_empty() {
        let app = build_empty_router();
        let uri = "/collections/empty/map?bbox=10,55,30,70&f=image/png";

        let etag = {
            let resp = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.headers().get("x-cache").unwrap(), "EMPTY");
            resp.headers()
                .get("etag")
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        };

        let resp = app
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("If-None-Match", &etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(resp.headers().get("etag").unwrap().to_str().unwrap(), etag);
        assert_eq!(
            resp.headers().get("x-cache").map(|v| v.to_str().unwrap()),
            Some("EMPTY"),
            "post-render 304 must forward the `x_cache` label from the \
             match arm, not hard-code `MISS` — otherwise revalidating an \
             empty tile silently changes its dashboard category"
        );
    }
}

/// Mock engine that returns an all-`None` (all-nodata) `RasterTile`.
/// Used to exercise the empty-tile fast path that bypasses the
/// format-aware encoder. The regression test for #162 lives in
/// `mod get_map` above.
struct EmptyMockMapEngine;

impl MapEngine for EmptyMockMapEngine {
    fn get_raster_tile(
        &self,
        _bbox: [f64; 4],
        width: u32,
        height: u32,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        _output_crs: &OutputCrs,
        _parameter: Option<&str>,
        _z: Option<f64>,
    ) -> Result<RasterTile, DataServerError> {
        let pixel_count = (width * height) as usize;
        Ok(RasterTile {
            width,
            height,
            values: vec![None; pixel_count],
        })
    }

    fn raster_info(&self) -> RasterInfo {
        MockMapEngine::make_info()
    }
}

fn build_empty_router() -> axum::Router {
    let engine: Arc<dyn MapEngine> = Arc::new(EmptyMockMapEngine);
    let mut engines = HashMap::new();
    let mut collections = HashMap::new();
    let mut styles_map = HashMap::new();

    engines.insert("empty".to_string(), engine);
    collections.insert(
        "empty".to_string(),
        CollectionConfig {
            id: "empty".to_string(),
            title: "Empty".to_string(),
            description: "All-nodata fixture for #162".to_string(),
            data_path: None,
            apis: vec!["maps".to_string()],
            engine_type: "geotiff".to_string(),
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            odim: None,
            postgis: None,
            preview: None,
        },
    );

    let cmap = Arc::new(LutColorMap::from_builtin(
        BuiltinColormap::Viridis,
        0.0,
        1.0,
    ));
    let mut layer_styles = HashMap::new();
    layer_styles.insert(
        "default".to_string(),
        StyleInfo {
            name: "default".to_string(),
            title: "Default".to_string(),
            colormap: cmap,
            min: 0.0,
            max: 1.0,
            parameter: None,
        },
    );
    styles_map.insert("empty".to_string(), layer_styles);

    let state = Arc::new(ArcSwap::from_pointee(MapsState {
        engines,
        collections,
        styles: styles_map,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        base_url: String::new(),
    }));
    api_maps::router(state)
}

struct MultiParamMockEngine;

impl MapEngine for MultiParamMockEngine {
    fn get_raster_tile(
        &self,
        _bbox: [f64; 4],
        width: u32,
        height: u32,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        _output_crs: &ds_core::map_engine::OutputCrs,
        parameter: Option<&str>,
        _z: Option<f64>,
    ) -> Result<ds_core::map_engine::RasterTile, ds_core::error::DataServerError> {
        // Vary the pixel values by parameter so the `parameter` field on the
        // cache key actually changes the rendered bytes — the cross-parameter
        // ETag test depends on this. Fold the parameter name into a value
        // in [0, 1] (within the style's min/max range) so the colormap
        // produces a different uniform fill per parameter.
        let fill: f64 = parameter
            .map(|p| {
                let mut h: u64 = 0xcbf29ce484222325;
                for &b in p.as_bytes() {
                    h ^= b as u64;
                    h = h.wrapping_mul(0x100000001b3);
                }
                ((h & 0xff) as f64) / 255.0
            })
            .unwrap_or(0.0);
        let pixel_count = (width * height) as usize;
        let values: Vec<Option<f64>> = vec![Some(fill); pixel_count];
        Ok(ds_core::map_engine::RasterTile {
            width,
            height,
            values,
        })
    }

    fn raster_info(&self) -> ds_core::map_engine::RasterInfo {
        ds_core::map_engine::RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: Some([0.0, 0.0, 10.0, 10.0]),
            times: vec![],
            parameter: "2t".into(),
            unit: "K".into(),
            parameters: vec![
                ("2t".into(), "Temperature".into()),
                ("10u".into(), "U Wind".into()),
            ],
            vertical: None,
            grid_size: None,
        }
    }
}

fn build_multi_param_router() -> axum::Router {
    let engine: Arc<dyn MapEngine> = Arc::new(MultiParamMockEngine);
    let mut engines = HashMap::new();
    let mut collections = HashMap::new();
    let mut styles_map = HashMap::new();

    engines.insert("wx".to_string(), engine);
    collections.insert(
        "wx".to_string(),
        CollectionConfig {
            id: "wx".to_string(),
            title: "Forecast".to_string(),
            description: "Test multi-param".to_string(),
            data_path: None,
            apis: vec!["maps".to_string()],
            engine_type: "grib".to_string(),
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            odim: None,
            postgis: None,
            preview: None,
        },
    );

    let cmap = Arc::new(LutColorMap::from_builtin(
        BuiltinColormap::Viridis,
        0.0,
        1.0,
    ));
    let mut layer_styles = HashMap::new();
    layer_styles.insert(
        "default".to_string(),
        StyleInfo {
            name: "default".to_string(),
            title: "Default".to_string(),
            colormap: cmap,
            min: 0.0,
            max: 1.0,
            parameter: None,
        },
    );
    styles_map.insert("wx".to_string(), layer_styles);

    let state = Arc::new(ArcSwap::from_pointee(MapsState {
        engines,
        collections,
        styles: styles_map,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        base_url: String::new(),
    }));
    api_maps::router(state)
}

// ---------------------------------------------------------------------------
// Styled map tests
// ---------------------------------------------------------------------------

mod styled_map {
    use super::*;

    #[tokio::test]
    async fn default_style_returns_png() {
        let (status, _, body) =
            get_raw("/collections/radar/styles/default/map?bbox=10,55,30,70").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[tokio::test]
    async fn grayscale_style_returns_png() {
        let (status, _, body) =
            get_raw("/collections/radar/styles/grayscale/map?bbox=10,55,30,70").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[tokio::test]
    async fn unknown_style_returns_404() {
        let (status, _) = get("/collections/radar/styles/nonexistent/map?bbox=10,55,30,70").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_collection_returns_404() {
        let (status, _) = get("/collections/nonexistent/styles/default/map?bbox=10,55,30,70").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

// ---------------------------------------------------------------------------
// TileMatrixSets tests
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Error response tests
// ---------------------------------------------------------------------------

mod errors {
    use super::*;

    #[tokio::test]
    async fn error_responses_have_code_and_description() {
        let (_, json) = get("/collections/nonexistent").await;
        assert!(json["code"].is_string());
        assert!(json["description"].is_string());
    }

    #[tokio::test]
    async fn bad_request_has_error_body() {
        let (status, json) = get("/collections/radar/map?bbox=invalid").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(json["code"].is_string());
    }
}

// ---------------------------------------------------------------------------
// Vertical extent tests (radar elevation angle, OGC API Common Part 2 form)
// ---------------------------------------------------------------------------

mod vertical_extent {
    use super::*;
    use ds_core::vertical::{VerticalDimension, VerticalKind};

    struct VerticalMockEngine;

    impl MapEngine for VerticalMockEngine {
        fn get_raster_tile(
            &self,
            _bbox: [f64; 4],
            width: u32,
            height: u32,
            _time: Option<chrono::DateTime<chrono::Utc>>,
            _output_crs: &OutputCrs,
            _parameter: Option<&str>,
            _z: Option<f64>,
        ) -> Result<RasterTile, DataServerError> {
            Ok(RasterTile {
                width,
                height,
                values: vec![None; (width * height) as usize],
            })
        }

        fn raster_info(&self) -> RasterInfo {
            RasterInfo {
                native_crs: "CRS:84".into(),
                spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
                times: vec![],
                parameter: "DBZH".into(),
                unit: "dBZ".into(),
                parameters: vec![],
                vertical: Some(VerticalDimension::new(
                    VerticalKind::ElevationAngle,
                    vec![0.5, 1.5, 5.0],
                )),
                grid_size: None,
            }
        }
    }

    async fn fetch_collection() -> Value {
        fetch_collection_json(
            Arc::new(VerticalMockEngine),
            "pvol",
            vec!["maps".to_string()],
        )
        .await
    }

    #[tokio::test]
    async fn vertical_extent_has_common_part2_form() {
        let json = fetch_collection().await;
        let v = &json["extent"]["vertical"];
        // Back-compat fields retained.
        assert_eq!(v["values"], serde_json::json!([0.5, 1.5, 5.0]));
        assert_eq!(v["interval"], serde_json::json!([[0.5, 5.0]]));
        // OGC API Common Part 2 additive form.
        assert_eq!(v["unit"], "deg");
        assert_eq!(v["grid"]["coordinates"], serde_json::json!([0.5, 1.5, 5.0]));
        // vrs is intentionally omitted for radar elevation angle.
        assert!(v.get("vrs").is_none());
    }

    /// A `VerticalDimension` with no levels must not emit `"interval": null`
    /// (invalid per OGC API Common Part 2) — the whole vertical extent is
    /// omitted instead.
    struct EmptyVerticalMockEngine;

    impl MapEngine for EmptyVerticalMockEngine {
        fn get_raster_tile(
            &self,
            _bbox: [f64; 4],
            width: u32,
            height: u32,
            _time: Option<chrono::DateTime<chrono::Utc>>,
            _output_crs: &OutputCrs,
            _parameter: Option<&str>,
            _z: Option<f64>,
        ) -> Result<RasterTile, DataServerError> {
            Ok(RasterTile {
                width,
                height,
                values: vec![None; (width * height) as usize],
            })
        }

        fn raster_info(&self) -> RasterInfo {
            RasterInfo {
                native_crs: "CRS:84".into(),
                spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
                times: vec![],
                parameter: "DBZH".into(),
                unit: "dBZ".into(),
                parameters: vec![],
                vertical: Some(VerticalDimension::new(VerticalKind::ElevationAngle, vec![])),
                grid_size: None,
            }
        }
    }

    #[tokio::test]
    async fn empty_vertical_dimension_omits_extent() {
        let json = fetch_collection_json(
            Arc::new(EmptyVerticalMockEngine),
            "empty-z",
            vec!["maps".to_string()],
        )
        .await;
        assert!(
            json["extent"].get("vertical").is_none(),
            "vertical extent must be omitted when there are no levels, got {:?}",
            json["extent"].get("vertical")
        );
    }
}

// ---------------------------------------------------------------------------
// Extent edge cases (storageCrs omission, temporal-grid jitter)
// ---------------------------------------------------------------------------

mod extent_edge_cases {
    use super::*;

    /// Engine whose native CRS is a projection with no canonical OGC URI
    /// (engines label these "TM"/"LAEA"/"projected"/…).
    struct ProjectedMockEngine;

    impl MapEngine for ProjectedMockEngine {
        fn get_raster_tile(
            &self,
            _bbox: [f64; 4],
            width: u32,
            height: u32,
            _time: Option<chrono::DateTime<chrono::Utc>>,
            _output_crs: &OutputCrs,
            _parameter: Option<&str>,
            _z: Option<f64>,
        ) -> Result<RasterTile, DataServerError> {
            Ok(RasterTile {
                width,
                height,
                values: vec![None; (width * height) as usize],
            })
        }

        fn raster_info(&self) -> RasterInfo {
            RasterInfo {
                native_crs: "LAEA".into(),
                spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
                times: vec![],
                parameter: "reflectivity".into(),
                unit: "dBZ".into(),
                parameters: vec![],
                vertical: None,
                grid_size: Some([2000, 1500]),
            }
        }
    }

    /// Engine whose timestamps are genuinely hourly but carry ±1 s jitter on
    /// the first interval — the regression case for the gap-spread check.
    struct JitteredTimesMockEngine;

    impl MapEngine for JitteredTimesMockEngine {
        fn get_raster_tile(
            &self,
            _bbox: [f64; 4],
            width: u32,
            height: u32,
            _time: Option<chrono::DateTime<chrono::Utc>>,
            _output_crs: &OutputCrs,
            _parameter: Option<&str>,
            _z: Option<f64>,
        ) -> Result<RasterTile, DataServerError> {
            Ok(RasterTile {
                width,
                height,
                values: vec![None; (width * height) as usize],
            })
        }

        fn raster_info(&self) -> RasterInfo {
            // Gaps: 3599 s, 3601 s, 3600 s -> spread 2, mean 3600 -> PT1H.
            let t = |s: &str| {
                chrono::DateTime::parse_from_rfc3339(s)
                    .unwrap()
                    .with_timezone(&chrono::Utc)
            };
            RasterInfo {
                native_crs: "CRS:84".into(),
                spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
                times: vec![
                    t("2024-01-01T00:00:00Z"),
                    t("2024-01-01T00:59:59Z"),
                    t("2024-01-01T02:00:00Z"),
                    t("2024-01-01T03:00:00Z"),
                ],
                parameter: "reflectivity".into(),
                unit: "dBZ".into(),
                parameters: vec![],
                vertical: None,
                grid_size: None,
            }
        }
    }

    #[tokio::test]
    async fn storage_crs_and_spatial_grid_omitted_for_projected_native_crs() {
        let json =
            fetch_collection_json(Arc::new(ProjectedMockEngine), "proj", vec!["maps".into()]).await;
        // Mislabelling a projected grid as CRS84 is worse than omitting it.
        assert!(
            json.get("storageCrs").is_none(),
            "storageCrs must be absent for a native CRS with no OGC URI, got {:?}",
            json.get("storageCrs")
        );
        // The bbox is still advertised...
        assert!(json["extent"]["spatial"]["bbox"].is_array());
        // ...but not a CRS84-degree grid: a projected grid isn't degree-regular.
        assert!(
            json["extent"]["spatial"].get("grid").is_none(),
            "projected grids must not advertise a CRS84-degree spatial.grid"
        );
    }

    #[tokio::test]
    async fn temporal_grid_treats_jittered_series_as_regular() {
        let json = fetch_collection_json(
            Arc::new(JitteredTimesMockEngine),
            "jit",
            vec!["maps".into()],
        )
        .await;
        let grid = &json["extent"]["temporal"]["grid"];
        assert_eq!(grid["cellsCount"], 4);
        // Despite the ±1 s jitter on the first interval, the series is regular.
        assert_eq!(grid["resolution"], "PT1H");
        assert!(
            grid.get("coordinates").is_none(),
            "regular series must not fall back to a coordinates list"
        );
    }

    /// Engine whose bbox crosses the anti-meridian (east < west).
    struct AntiMeridianMockEngine;

    impl MapEngine for AntiMeridianMockEngine {
        fn get_raster_tile(
            &self,
            _bbox: [f64; 4],
            width: u32,
            height: u32,
            _time: Option<chrono::DateTime<chrono::Utc>>,
            _output_crs: &OutputCrs,
            _parameter: Option<&str>,
            _z: Option<f64>,
        ) -> Result<RasterTile, DataServerError> {
            Ok(RasterTile {
                width,
                height,
                values: vec![None; (width * height) as usize],
            })
        }

        fn raster_info(&self) -> RasterInfo {
            RasterInfo {
                native_crs: "CRS:84".into(),
                // 20°-wide box straddling 180°: east (-170) < west (170).
                spatial_extent: Some([170.0, 60.0, -170.0, 70.0]),
                times: vec![],
                parameter: "reflectivity".into(),
                unit: "dBZ".into(),
                parameters: vec![],
                vertical: None,
                grid_size: Some([2000, 1000]),
            }
        }
    }

    #[tokio::test]
    async fn spatial_grid_resolution_positive_across_antimeridian() {
        let json =
            fetch_collection_json(Arc::new(AntiMeridianMockEngine), "am", vec!["maps".into()])
                .await;
        let grid = json["extent"]["spatial"]["grid"].as_array().unwrap();
        // 20° / 2000 cells = 0.01, positive — not the -340°/2000 a naive
        // (east - west) would give.
        let lon_res = grid[0]["resolution"].as_f64().unwrap();
        assert!(lon_res > 0.0, "resolution must be positive, got {lon_res}");
        assert!((lon_res - 0.01).abs() < 1e-9);
    }
}
