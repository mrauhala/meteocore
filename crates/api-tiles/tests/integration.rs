use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use api_tiles::TilesState;
use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use ds_mvt::VectorTileCache;
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
            layer_subtitle: None,
            reference_times: Vec::new(),
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
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        let pixel_count = (width * height) as usize;
        let values: Vec<Option<f64>> = (0..pixel_count)
            .map(|i| Some(i as f64 / pixel_count as f64))
            .collect();
        Ok(RasterTile {
            width,
            height,
            values: values.into(),
        })
    }

    fn raster_info(&self) -> RasterInfo {
        Self::make_info()
    }
}

/// A `MapEngine` whose `get_raster_tile` always returns `InvalidParameter`
/// — mirrors a multi-parameter PVOL collection tiled without a
/// `<site>:<quantity>` selection. Used to verify the handler returns 400,
/// not 500.
struct InvalidParamEngine;

impl MapEngine for InvalidParamEngine {
    fn get_raster_tile(
        &self,
        _bbox: [f64; 4],
        _width: u32,
        _height: u32,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        _output_crs: &OutputCrs,
        _parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        Err(DataServerError::InvalidParameter(
            "collection requires a `<site>:<quantity>` parameter (e.g. `fivih:DBZH`)".into(),
        ))
    }

    fn raster_info(&self) -> RasterInfo {
        MockMapEngine::make_info()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_router() -> axum::Router {
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
            apis: vec!["tiles".to_string()],
            engine_type: "geotiff".to_string(),
            keywords: Vec::new(),
            license: None,
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            zarr: None,
            odim: None,
            cap: None,
            postgis: None,
            nowcast: None,
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

    let state = Arc::new(ArcSwap::from_pointee(TilesState {
        map_engines: engines,
        collections,
        styles: styles_map,
        feature_engines: HashMap::new(),
        feature_collections: HashMap::new(),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        vector_tile_cache: Arc::new(VectorTileCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
    }));
    api_tiles::router(state)
}

/// A Tiles router backed by a caller-supplied engine, for exercising
/// engine error paths.
fn build_router_with_engine(engine: Arc<dyn MapEngine>) -> axum::Router {
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
            apis: vec!["tiles".to_string()],
            engine_type: "odim-volume".to_string(),
            keywords: Vec::new(),
            license: None,
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            zarr: None,
            odim: None,
            cap: None,
            postgis: None,
            nowcast: None,
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
    styles_map.insert("radar".to_string(), layer_styles);
    let state = Arc::new(ArcSwap::from_pointee(TilesState {
        map_engines: engines,
        collections,
        styles: styles_map,
        feature_engines: HashMap::new(),
        feature_collections: HashMap::new(),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        vector_tile_cache: Arc::new(VectorTileCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
    }));
    api_tiles::router(state)
}

async fn get(uri: &str) -> (StatusCode, Value) {
    let app = build_router();
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
        assert!(links.iter().any(|l| l["rel"] == "service-desc"));
        assert!(links.iter().any(|l| l["rel"] == "service-doc"));
        assert!(links.iter().any(|l| l["rel"] == "conformance"));
        assert!(links.iter().any(|l| l["rel"] == "data"));
        assert!(links.iter().any(|l| l["rel"] == "tiling-schemes"));
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
    async fn declares_tileset() {
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        assert!(classes
            .iter()
            .any(|c| c.as_str().unwrap().contains("conf/tileset")));
    }

    #[tokio::test]
    async fn declares_tilesets_list() {
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        assert!(classes
            .iter()
            .any(|c| c.as_str().unwrap().contains("conf/tilesets-list")));
    }

    #[tokio::test]
    async fn declares_mvt() {
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        assert!(
            classes
                .iter()
                .any(|c| c.as_str().unwrap().ends_with("conf/mvt")),
            "conformance must include the OGC API Tiles MVT class once ?f=mvt is wired"
        );
    }

    #[tokio::test]
    async fn declares_tilematrixset() {
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        assert!(classes
            .iter()
            .any(|c| c.as_str().unwrap().contains("conf/tilematrixset")));
    }

    #[tokio::test]
    async fn declares_ogcapi_common_part1_and_part2() {
        // OGC API - Common Part 1 (Core) + Part 2 (Collections, JSON) — the
        // landing page / conformance / collections resources satisfy them, so
        // they are advertised for discovery (#291).
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        let has = |needle: &str| classes.iter().any(|c| c.as_str().unwrap().contains(needle));
        assert!(has("ogcapi-common-1/1.0/conf/core"), "Common Part 1 Core");
        assert!(
            has("ogcapi-common-2/1.0/conf/collections"),
            "Common Part 2 Collections"
        );
        assert!(has("ogcapi-common-2/1.0/conf/json"), "Common Part 2 JSON");
    }

    #[tokio::test]
    async fn declares_common_html_class() {
        // HTML representation of the metadata endpoints is now served via
        // `?f=html` / Accept, so the Common Part 2 HTML class is declared (#296).
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        assert!(classes
            .iter()
            .any(|c| c.as_str().unwrap().contains("common-2/1.0/conf/html")));
    }
}

// ---------------------------------------------------------------------------
// TileMatrixSets tests
// ---------------------------------------------------------------------------

mod tile_matrix_sets {
    use super::*;

    #[tokio::test]
    async fn list_returns_200() {
        let (status, _) = get("/tileMatrixSets").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn list_has_tile_matrix_sets_array() {
        let (_, json) = get("/tileMatrixSets").await;
        assert!(json["tileMatrixSets"].is_array());
    }

    #[tokio::test]
    async fn list_contains_web_mercator_quad() {
        let (_, json) = get("/tileMatrixSets").await;
        let sets = json["tileMatrixSets"].as_array().unwrap();
        assert!(sets.iter().any(|s| s["id"] == "WebMercatorQuad"));
    }

    #[tokio::test]
    async fn list_contains_world_crs84_quad() {
        let (_, json) = get("/tileMatrixSets").await;
        let sets = json["tileMatrixSets"].as_array().unwrap();
        assert!(sets.iter().any(|s| s["id"] == "WorldCRS84Quad"));
    }

    #[tokio::test]
    async fn detail_returns_200() {
        let (status, _) = get("/tileMatrixSets/WebMercatorQuad").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn detail_has_tile_matrices() {
        let (_, json) = get("/tileMatrixSets/WebMercatorQuad").await;
        assert!(json["tileMatrices"].is_array());
        assert!(!json["tileMatrices"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unknown_tms_returns_404() {
        let (status, _) = get("/tileMatrixSets/UnknownTMS").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
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
    async fn collection_has_tile_matrix_set_links() {
        let (_, json) = get("/collections").await;
        let c = &json["collections"][0];
        assert!(c["tileMatrixSetLinks"].is_array());
        assert!(!c["tileMatrixSetLinks"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn collection_omits_nonstandard_apis_field() {
        let (_, json) = get("/collections/radar").await;
        assert!(
            json.get("apis").is_none(),
            "apis must not be present in the standard collection JSON"
        );
    }

    #[tokio::test]
    async fn spatial_extent_has_grid_resolution() {
        let (_, json) = get("/collections/radar").await;
        let grid = json["extent"]["spatial"]["grid"]
            .as_array()
            .expect("spatial.grid must be present for raster collections");
        assert_eq!(grid.len(), 2);
        assert_eq!(grid[0]["cellsCount"], 2000);
        assert_eq!(grid[1]["cellsCount"], 1500);
    }

    #[tokio::test]
    async fn raster_collection_advertises_storage_crs() {
        let (_, json) = get("/collections/radar").await;
        // WGS84 data is lon-first -> CRS84 URI, not the lat-first EPSG:4326 one.
        assert_eq!(
            json["storageCrs"],
            "http://www.opengis.net/def/crs/OGC/1.3/CRS84"
        );
    }

    #[tokio::test]
    async fn temporal_extent_has_regular_grid_resolution() {
        let (_, json) = get("/collections/radar").await;
        let grid = &json["extent"]["temporal"]["grid"];
        assert_eq!(grid["cellsCount"], 2);
        assert_eq!(grid["resolution"], "PT1H");
    }

    #[tokio::test]
    async fn unknown_collection_returns_404() {
        let (status, _) = get("/collections/nonexistent").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

// ---------------------------------------------------------------------------
// Tilesets tests
// ---------------------------------------------------------------------------

mod tilesets {
    use super::*;

    #[tokio::test]
    async fn collection_tilesets_returns_200() {
        let (status, _) = get("/collections/radar/tiles").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn collection_tilesets_has_tilesets_array() {
        let (_, json) = get("/collections/radar/tiles").await;
        assert!(json["tilesets"].is_array());
        assert!(!json["tilesets"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unknown_collection_returns_404() {
        let (status, _) = get("/collections/nonexistent/tiles").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

// ---------------------------------------------------------------------------
// Get tile tests
// ---------------------------------------------------------------------------

mod get_tile {
    use super::*;

    #[tokio::test]
    async fn valid_tile_returns_200_png() {
        let (status, headers, body) =
            get_raw("/collections/radar/tiles/WebMercatorQuad/0/0/0").await;
        assert_eq!(status, StatusCode::OK);
        let ct = headers.get("content-type").unwrap().to_str().unwrap();
        assert_eq!(ct, "image/png");
        assert!(!body.is_empty());
        // Check PNG magic bytes
        assert!(body.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[tokio::test]
    async fn has_cache_headers() {
        let (_, headers, _) = get_raw("/collections/radar/tiles/WebMercatorQuad/0/0/0").await;
        assert!(headers.contains_key("etag"));
        assert!(headers.contains_key("cache-control"));
    }

    /// `elevation` against a collection with no vertical dimension
    /// (`MockMapEngine.raster_info().vertical` is `None`) is a 400.
    #[tokio::test]
    async fn elevation_against_non_vertical_collection_returns_400() {
        let (status, _, _) =
            get_raw("/collections/radar/tiles/WebMercatorQuad/0/0/0?elevation=0.5").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// An engine `InvalidParameter` from `get_raster_tile` (e.g. a
    /// multi-parameter PVOL collection tiled without a `<site>:<quantity>`
    /// selection) is a 400 **with the engine's message in the body**, not
    /// a 500 — parity with the maps regression test.
    #[tokio::test]
    async fn render_invalid_parameter_is_400_with_message() {
        let app = build_router_with_engine(Arc::new(InvalidParamEngine));
        let req = Request::builder()
            .uri("/collections/radar/tiles/WebMercatorQuad/0/0/0")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["code"], "BadRequest");
        assert!(
            json["description"]
                .as_str()
                .unwrap_or_default()
                .contains("parameter"),
            "the helpful engine message must reach the client, got {json}"
        );
    }

    #[tokio::test]
    async fn explicit_datetime_gets_immutable_cache() {
        let (_, headers, _) =
            get_raw("/collections/radar/tiles/WebMercatorQuad/0/0/0?datetime=2024-01-01T00:00:00Z")
                .await;
        let cc = headers.get("cache-control").unwrap().to_str().unwrap();
        assert!(cc.contains("immutable"));
    }

    #[tokio::test]
    async fn no_datetime_gets_short_cache() {
        let (_, headers, _) = get_raw("/collections/radar/tiles/WebMercatorQuad/0/0/0").await;
        let cc = headers.get("cache-control").unwrap().to_str().unwrap();
        assert!(cc.contains("must-revalidate"));
    }

    #[tokio::test]
    async fn unknown_collection_returns_404() {
        let (status, _) = get("/collections/nonexistent/tiles/WebMercatorQuad/0/0/0").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_tms_returns_400() {
        let (status, _) = get("/collections/radar/tiles/UnknownTMS/0/0/0").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn out_of_range_tile_coords_returns_404() {
        // WebMercatorQuad z=0 has 1x1 tile, so row=1 is out of range
        let (status, _) = get("/collections/radar/tiles/WebMercatorQuad/0/1/0").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn out_of_range_col_returns_404() {
        // WebMercatorQuad z=0 has 1x1 tile, so col=1 is out of range
        let (status, _) = get("/collections/radar/tiles/WebMercatorQuad/0/0/1").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_style_returns_404() {
        let (status, _) =
            get("/collections/radar/styles/nonexistent/tiles/WebMercatorQuad/0/0/0").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn world_crs84_quad_tile_returns_200() {
        let (status, _, body) = get_raw("/collections/radar/tiles/WorldCRS84Quad/0/0/0").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[tokio::test]
    async fn parameter_name_query_is_accepted_for_single_param_engine() {
        // `MockMapEngine.raster_info().parameters` is empty (single-band
        // engine convention). The handler must accept any `parameter-name=`
        // and forward it to the engine, which is free to ignore it.
        let (status, _, body) =
            get_raw("/collections/radar/tiles/WebMercatorQuad/0/0/0?parameter-name=anything").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    /// Regression for #145: the response `ETag` must be FNV-1a over the
    /// rendered bytes — not over the cache key — so a server-side fix that
    /// produces different pixels under the same `(z, x, y, style, time)`
    /// produces a fresh ETag and browsers holding the stale entry refetch
    /// instead of receiving an infinite 304. Direct check: build the same
    /// `CachedRendered` the handler does and verify the header matches.
    #[tokio::test]
    async fn etag_is_content_derived_over_response_body() {
        let (status, headers, body) =
            get_raw("/collections/radar/tiles/WebMercatorQuad/0/0/0").await;
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
    /// `x-cache: HIT`) and the post-render MISS branch. A fresh router
    /// would still 304 — just via the MISS path — so the `x-cache`
    /// assertion is what makes "we exercised the HIT branch" testable.
    #[tokio::test]
    async fn if_none_match_after_cache_warm_returns_304_via_cache_hit() {
        let app = build_router();
        let resp_a = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/collections/radar/tiles/WebMercatorQuad/0/0/0")
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

        let req = Request::builder()
            .uri("/collections/radar/tiles/WebMercatorQuad/0/0/0")
            .header("If-None-Match", &etag)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            resp.headers().get("etag").unwrap().to_str().unwrap(),
            etag,
            "304 response must echo the same content-derived ETag"
        );
        assert_eq!(
            resp.headers().get("x-cache").map(|v| v.to_str().unwrap()),
            Some("HIT"),
            "304 must come from the cache-HIT branch, not post-render MISS"
        );
    }

    /// Pin the post-render MISS → 304 branch. Use a fresh router (no
    /// cache-warm) so the first `If-None-Match`-bearing request must
    /// go through the full render path; assert the 304 carries
    /// `x-cache: MISS` rather than the cache-HIT branch's `HIT`. Tests
    /// the path the other test deliberately bypasses.
    #[tokio::test]
    async fn if_none_match_against_fresh_router_returns_304_via_miss_branch() {
        // Step 1: render once on a separate fresh router to learn the ETag.
        let etag = {
            let warm = build_router();
            let resp = warm
                .oneshot(
                    Request::builder()
                        .uri("/collections/radar/tiles/WebMercatorQuad/0/0/0")
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

        // Step 2: brand-new router with an empty cache. The handler must
        // render, compute the same content-derived ETag, see the match,
        // and 304 via the post-render branch.
        let app = build_router();
        let req = Request::builder()
            .uri("/collections/radar/tiles/WebMercatorQuad/0/0/0")
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
                    .uri("/collections/wx/tiles/WebMercatorQuad/0/0/0?parameter-name=2t")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp_10u = app
            .oneshot(
                Request::builder()
                    .uri("/collections/wx/tiles/WebMercatorQuad/0/0/0?parameter-name=10u")
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
        // When the engine advertises a non-empty `parameters` list, the
        // handler must validate against it and reject unknown names with a
        // helpful error rather than rendering the default with a confusing
        // colormap.
        let app = build_multi_param_router();
        let req = Request::builder()
            .uri("/collections/wx/tiles/WebMercatorQuad/0/0/0?parameter-name=nope")
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
            .uri("/collections/wx/tiles/WebMercatorQuad/0/0/0?parameter-name=2t")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// Regression for #162: when the engine produces an all-nodata tile,
    /// the handler short-circuits to the pre-generated `EMPTY_TILE_PNG`
    /// global without going through the format-aware encoder. Before
    /// the fix the response carried the *requested* Content-Type (e.g.
    /// image/jpeg) over PNG bytes, breaking decoders that trust the
    /// header. Both header and body must agree.
    #[tokio::test]
    async fn empty_tile_forces_png_content_type_even_when_jpeg_requested() {
        let app = build_empty_router();
        let req = Request::builder()
            .uri("/collections/empty/tiles/WebMercatorQuad/0/0/0?f=image/jpeg")
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
        let app = build_empty_router();
        let req = Request::builder()
            .uri("/collections/empty/tiles/WebMercatorQuad/0/0/0?f=image/webp")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let headers = resp.headers().clone();
        assert_eq!(headers.get("content-type").unwrap(), "image/png");
        assert_eq!(headers.get("x-cache").unwrap(), "EMPTY");
    }

    /// Revalidating a cached empty-tile response must round-trip the
    /// `EMPTY` label, not be silently re-tagged as `MISS`. Empty tiles
    /// share the global `EMPTY_TILE_CACHED` (never inserted into
    /// `rendered_cache`), so an `If-None-Match` request always falls
    /// through to the post-render branch — which is exactly the branch
    /// the round-7 fix targets. A viewer panning over out-of-coverage
    /// areas would otherwise see `304 x-cache: MISS` for every empty
    /// tile, hiding them from dashboards filtered on `EMPTY`.
    #[tokio::test]
    async fn if_none_match_on_empty_tile_returns_304_with_x_cache_empty() {
        // Step 1: render the empty tile to capture its (deterministic) ETag.
        let app = build_empty_router();
        let etag = {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/collections/empty/tiles/WebMercatorQuad/0/0/0")
                        .body(Body::empty())
                        .unwrap(),
                )
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

        // Step 2: revalidate. The handler must reach the post-render
        // branch (empty tiles bypass `rendered_cache`), match the ETag,
        // and 304 with `x-cache: EMPTY` — forwarding the same label
        // the 200 response would carry, not the legacy hard-coded MISS.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/collections/empty/tiles/WebMercatorQuad/0/0/0")
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

/// Mock engine that returns an all-`None` (all-nodata) `RasterTile` —
/// drives the empty-tile fast path that bypasses the format-aware
/// encoder. The regression test for #162 lives in `mod get_tile` above.
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
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        let pixel_count = (width * height) as usize;
        Ok(RasterTile {
            width,
            height,
            values: vec![None; pixel_count].into(),
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
            apis: vec!["tiles".to_string()],
            engine_type: "geotiff".to_string(),
            keywords: Vec::new(),
            license: None,
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            zarr: None,
            odim: None,
            cap: None,
            postgis: None,
            nowcast: None,
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

    let state = Arc::new(ArcSwap::from_pointee(TilesState {
        map_engines: engines,
        collections,
        styles: styles_map,
        feature_engines: HashMap::new(),
        feature_collections: HashMap::new(),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        vector_tile_cache: Arc::new(VectorTileCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
    }));
    api_tiles::router(state)
}

struct MultiParamMockEngine;

impl MapEngine for MultiParamMockEngine {
    fn get_raster_tile(
        &self,
        _bbox: [f64; 4],
        width: u32,
        height: u32,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        _output_crs: &OutputCrs,
        parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        // Vary the pixel values by parameter so the `parameter` field on
        // the cache key actually changes the rendered bytes — the
        // cross-parameter ETag test depends on this. Fold the name into
        // a value in [0, 1] (within the style's min/max range) so the
        // colormap produces a different uniform fill per parameter.
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
        Ok(RasterTile {
            width,
            height,
            values: values.into(),
        })
    }

    fn raster_info(&self) -> RasterInfo {
        RasterInfo {
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
            layer_subtitle: None,
            reference_times: Vec::new(),
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
            apis: vec!["tiles".to_string()],
            engine_type: "grib".to_string(),
            keywords: Vec::new(),
            license: None,
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            zarr: None,
            odim: None,
            cap: None,
            postgis: None,
            nowcast: None,
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

    let state = Arc::new(ArcSwap::from_pointee(TilesState {
        map_engines: engines,
        collections,
        styles: styles_map,
        feature_engines: HashMap::new(),
        feature_collections: HashMap::new(),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        vector_tile_cache: Arc::new(VectorTileCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
    }));
    api_tiles::router(state)
}

// ---------------------------------------------------------------------------
// API definition tests
// ---------------------------------------------------------------------------

mod api_definition {
    use super::*;

    #[tokio::test]
    async fn returns_200() {
        let (status, _) = get("/api").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn has_openapi_field() {
        let (_, json) = get("/api").await;
        assert!(json["openapi"].is_string());
    }

    #[tokio::test]
    async fn has_paths() {
        let (_, json) = get("/api").await;
        assert!(json["paths"].is_object());
    }

    #[tokio::test]
    async fn has_tile_path_for_collection() {
        let (_, json) = get("/api").await;
        let paths = json["paths"].as_object().unwrap();
        let has_tile_path = paths
            .keys()
            .any(|k| k.contains("radar") && k.contains("tiles") && k.contains("tileMatrix"));
        assert!(
            has_tile_path,
            "Expected a tile path for the radar collection in the OpenAPI paths"
        );
    }
}

// ---------------------------------------------------------------------------
// Styled tile tests
// ---------------------------------------------------------------------------

mod styled_tile {
    use super::*;

    #[tokio::test]
    async fn valid_style_returns_200() {
        let (status, _, body) =
            get_raw("/collections/radar/styles/default/tiles/WebMercatorQuad/0/0/0").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[tokio::test]
    async fn grayscale_style_returns_200() {
        let (status, _, body) =
            get_raw("/collections/radar/styles/grayscale/tiles/WebMercatorQuad/0/0/0").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[tokio::test]
    async fn unknown_style_returns_404() {
        let (status, _) =
            get("/collections/radar/styles/nonexistent/tiles/WebMercatorQuad/0/0/0").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_collection_returns_404() {
        let (status, _) =
            get("/collections/nonexistent/styles/default/tiles/WebMercatorQuad/0/0/0").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

// ---------------------------------------------------------------------------
// Vector tile (MVT) tests
// ---------------------------------------------------------------------------

mod mvt {
    use super::*;
    use ds_core::feature::{Feature, FeaturePage, FeatureQuery, Geometry, PropertyValue};
    use ds_core::feature_engine::FeatureEngine;
    use std::sync::Arc as StdArc;

    struct PointFeatureEngine {
        features: Vec<Feature>,
        extent: Option<[f64; 4]>,
    }

    impl PointFeatureEngine {
        fn three_points() -> Self {
            let mk = |id: &str, x: f64, y: f64, name: &str| Feature {
                id: id.into(),
                geometry: StdArc::new(Geometry::Point { x, y }),
                properties: StdArc::new(
                    [("name".to_string(), PropertyValue::String(name.into()))]
                        .into_iter()
                        .collect(),
                ),
            };
            Self {
                features: vec![
                    mk("1", 0.0, 0.0, "origin"),
                    mk("2", 10.0, 20.0, "northeast"),
                    mk("3", -50.0, 30.0, "atlantic"),
                ],
                extent: Some([-50.0, 0.0, 10.0, 30.0]),
            }
        }
    }

    impl FeatureEngine for PointFeatureEngine {
        fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
            let bbox = query.bbox.as_ref();
            let matched: Vec<&Feature> = self
                .features
                .iter()
                .filter(|f| match (&*f.geometry, bbox) {
                    (Geometry::Point { x, y }, Some(b)) => b.contains(*x, *y),
                    _ => true,
                })
                .collect();
            let n = matched.len();
            // Honour `limit` literally to match `GeoJsonEngine`'s real
            // semantics (a previous version of the mock special-cased zero
            // as "no limit", which hid the empty-tile bug fixed alongside
            // this test).
            let take = query.limit.min(n);
            let features: Vec<Feature> = matched.into_iter().take(take).cloned().collect();
            Ok(FeaturePage {
                features,
                number_matched: n,
                number_returned: take,
                next_offset: None,
            })
        }

        fn get_feature(&self, id: &str) -> Result<Feature, DataServerError> {
            self.features
                .iter()
                .find(|f| f.id == id)
                .cloned()
                .ok_or_else(|| DataServerError::FeatureNotFound(id.to_string()))
        }

        fn feature_count(&self) -> usize {
            self.features.len()
        }

        fn spatial_extent(&self) -> Option<[f64; 4]> {
            self.extent
        }
    }

    fn build_mvt_router() -> axum::Router {
        let engine: Arc<dyn FeatureEngine> = Arc::new(PointFeatureEngine::three_points());
        let mut feature_engines = HashMap::new();
        let mut feature_collections = HashMap::new();
        feature_engines.insert("places".to_string(), engine);
        feature_collections.insert(
            "places".to_string(),
            CollectionConfig {
                id: "places".to_string(),
                title: "Places".to_string(),
                description: "Test points".to_string(),
                data_path: None,
                apis: vec!["tiles".to_string()],
                engine_type: "geojson".to_string(),
                keywords: Vec::new(),
                license: None,
                geotiff: None,
                querydata: None,
                wms: None,
                grib: None,
                zarr: None,
                odim: None,
                cap: None,
                postgis: None,
                nowcast: None,
                preview: None,
            },
        );

        let state = Arc::new(ArcSwap::from_pointee(TilesState {
            map_engines: HashMap::new(),
            collections: HashMap::new(),
            styles: HashMap::new(),
            feature_engines,
            feature_collections,
            render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
            rendered_cache: Arc::new(RenderedCache::new(16)),
            vector_tile_cache: Arc::new(VectorTileCache::new(16)),
            base_url: String::new(),
            trust_proxy_headers: false,
        }));
        api_tiles::router(state)
    }

    /// Mock engine that always returns at least one more feature than the
    /// density cap permits, regardless of the requested bbox or limit.
    /// Used to exercise the `tile-too-dense` 400 branch.
    struct OverflowingFeatureEngine {
        /// How many features to emit. The handler asks for
        /// `MAX_FEATURES_PER_TILE + 1`; the density guard fires when
        /// `features.len() > MAX_FEATURES_PER_TILE`, so any value ≥ that
        /// threshold trips the branch.
        emit: usize,
    }

    impl FeatureEngine for OverflowingFeatureEngine {
        fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
            // Honour `limit` so the test still trips the guard cleanly: the
            // handler asks for `MAX + 1`, and we cap at that — returning a
            // gigantic vector here would just waste memory.
            let take = query.limit.min(self.emit);
            let f = Feature {
                id: "pt".into(),
                geometry: StdArc::new(Geometry::Point { x: 0.0, y: 0.0 }),
                properties: StdArc::new(Default::default()),
            };
            let features: Vec<Feature> = std::iter::repeat_with(|| f.clone()).take(take).collect();
            let len = features.len();
            Ok(FeaturePage {
                features,
                number_matched: self.emit,
                number_returned: len,
                next_offset: None,
            })
        }

        fn get_feature(&self, _id: &str) -> Result<Feature, DataServerError> {
            Err(DataServerError::FeatureNotFound("unused".into()))
        }

        fn feature_count(&self) -> usize {
            self.emit
        }

        fn spatial_extent(&self) -> Option<[f64; 4]> {
            Some([-180.0, -85.0, 180.0, 85.0])
        }
    }

    fn build_dense_router(emit: usize) -> axum::Router {
        let engine: Arc<dyn FeatureEngine> = Arc::new(OverflowingFeatureEngine { emit });
        let mut feature_engines = HashMap::new();
        let mut feature_collections = HashMap::new();
        feature_engines.insert("dense".to_string(), engine);
        feature_collections.insert(
            "dense".to_string(),
            CollectionConfig {
                id: "dense".to_string(),
                title: "Dense".to_string(),
                description: "Always-overflow mock".to_string(),
                data_path: None,
                apis: vec!["tiles".to_string()],
                engine_type: "geojson".to_string(),
                keywords: Vec::new(),
                license: None,
                geotiff: None,
                querydata: None,
                wms: None,
                grib: None,
                zarr: None,
                odim: None,
                cap: None,
                postgis: None,
                nowcast: None,
                preview: None,
            },
        );

        let state = Arc::new(ArcSwap::from_pointee(TilesState {
            map_engines: HashMap::new(),
            collections: HashMap::new(),
            styles: HashMap::new(),
            feature_engines,
            feature_collections,
            render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
            rendered_cache: Arc::new(RenderedCache::new(16)),
            vector_tile_cache: Arc::new(VectorTileCache::new(16)),
            base_url: String::new(),
            trust_proxy_headers: false,
        }));
        api_tiles::router(state)
    }

    async fn fetch(uri: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let app = build_mvt_router();
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

    #[tokio::test]
    async fn pbf_route_returns_200_with_mvt_content_type() {
        let (status, headers, body) =
            fetch("/collections/places/tiles/WebMercatorQuad/0/0/0?f=mvt").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers.get("content-type").unwrap(),
            "application/vnd.mapbox-vector-tile"
        );
        assert!(!body.is_empty(), "MVT body must be non-empty");
        // Layer name and tag key both appear in plaintext in the protobuf.
        assert!(
            body.windows(b"places".len()).any(|w| w == b"places"),
            "layer name 'places' missing from encoded MVT"
        );
        assert!(
            body.windows(b"name".len()).any(|w| w == b"name"),
            "property key 'name' missing from encoded MVT"
        );
    }

    #[tokio::test]
    async fn pbf_route_emits_etag_and_cache_control() {
        let (_, headers, _) = fetch("/collections/places/tiles/WebMercatorQuad/0/0/0?f=mvt").await;
        let etag = headers
            .get("etag")
            .expect("ETag must be set")
            .to_str()
            .unwrap();
        assert!(etag.starts_with('"') && etag.ends_with('"'));
        assert_eq!(headers.get("cache-control").unwrap(), "public, max-age=300");
        assert_eq!(
            headers.get("x-content-type-options").unwrap(),
            "nosniff",
            "MVT responses must set nosniff to match the raster tile path"
        );
    }

    #[tokio::test]
    async fn pbf_route_if_none_match_returns_304() {
        // First fetch to learn the ETag.
        let (_, headers, _) = fetch("/collections/places/tiles/WebMercatorQuad/0/0/0?f=mvt").await;
        let etag = headers.get("etag").unwrap().clone();

        let app = build_mvt_router();
        let req = Request::builder()
            .uri("/collections/places/tiles/WebMercatorQuad/0/0/0?f=mvt")
            .header("if-none-match", etag)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn pbf_unknown_collection_returns_404() {
        let (status, _, _) = fetch("/collections/missing/tiles/WebMercatorQuad/0/0/0?f=mvt").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn pbf_unsupported_tms_returns_400() {
        let (status, _, _) = fetch("/collections/places/tiles/Bogus/0/0/0?f=mvt").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn pbf_out_of_range_tile_returns_404() {
        // z=0 has only one tile (0/0/0); (0/99/99) is outside the matrix.
        let (status, _, _) = fetch("/collections/places/tiles/WebMercatorQuad/0/99/99?f=mvt").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn pbf_crs84_route_also_works() {
        let (status, headers, body) =
            fetch("/collections/places/tiles/WorldCRS84Quad/0/0/0?f=mvt").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers.get("content-type").unwrap(),
            "application/vnd.mapbox-vector-tile"
        );
        assert!(!body.is_empty());
    }

    #[tokio::test]
    async fn raster_default_path_still_404s_without_map_engine() {
        // Same URL without `?f=mvt` requests a raster tile; with no MapEngine
        // registered for this collection, that must 404.
        let (status, _, _) = fetch("/collections/places/tiles/WebMercatorQuad/0/0/0").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn mvt_via_canonical_mime_token_also_works() {
        let (status, headers, body) = fetch(
            "/collections/places/tiles/WebMercatorQuad/0/0/0?f=application/vnd.mapbox-vector-tile",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers.get("content-type").unwrap(),
            "application/vnd.mapbox-vector-tile"
        );
        assert!(!body.is_empty());
    }

    #[tokio::test]
    async fn vector_only_collection_listed_in_collections() {
        let app = build_mvt_router();
        let req = Request::builder()
            .uri("/collections")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let ids: Vec<&str> = json["collections"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["id"].as_str())
            .collect();
        assert!(
            ids.contains(&"places"),
            "vector-only collection 'places' must appear in /collections, got {ids:?}"
        );
        let places = json["collections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"].as_str() == Some("places"))
            .unwrap();
        assert_eq!(
            places["dataType"].as_str(),
            Some("vector"),
            "vector-only collection must be advertised as dataType=vector"
        );
    }

    #[tokio::test]
    async fn pbf_tile_too_dense_returns_422() {
        // One past the cap → density guard fires.
        let app = build_dense_router(api_tiles::params::MAX_FEATURES_PER_TILE + 1);
        let req = Request::builder()
            .uri("/collections/dense/tiles/WebMercatorQuad/0/0/0?f=mvt")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // 422 Unprocessable Content: the request is well-formed; only the
        // data exceeds the per-tile feature budget.
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let body_str = std::str::from_utf8(&body).unwrap_or("");
        assert!(
            body_str.contains("tile-too-dense"),
            "422 body should mention the density guard, got: {body_str}"
        );
    }

    #[tokio::test]
    async fn pbf_tile_at_density_cap_returns_200() {
        // Exactly at the cap → success path. Confirms the guard's boundary
        // condition isn't off-by-one (guard fires only on `>`, not `>=`).
        let app = build_dense_router(api_tiles::params::MAX_FEATURES_PER_TILE);
        let req = Request::builder()
            .uri("/collections/dense/tiles/WebMercatorQuad/0/0/0?f=mvt")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn vector_only_tilesets_advertises_mvt_item_link() {
        let app = build_mvt_router();
        let req = Request::builder()
            .uri("/collections/places/tiles")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let any_mvt_link = json["tilesets"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|t| t["links"].as_array().unwrap())
            .any(|l| l["type"].as_str() == Some("application/vnd.mapbox-vector-tile"));
        assert!(
            any_mvt_link,
            "vector-only collection tilesets must include at least one MVT item link"
        );
    }
}

// ---------------------------------------------------------------------------
// Temporal-grid jitter (mirrors the api-maps regression test, since
// temporal_grid is duplicated across the two crates)
// ---------------------------------------------------------------------------

mod temporal_grid_jitter {
    use super::*;

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
            _reference_time: Option<chrono::DateTime<chrono::Utc>>,
        ) -> Result<RasterTile, DataServerError> {
            Ok(RasterTile {
                width,
                height,
                values: vec![None; (width * height) as usize].into(),
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
                layer_subtitle: None,
                reference_times: Vec::new(),
            }
        }
    }

    #[tokio::test]
    async fn temporal_grid_treats_jittered_series_as_regular() {
        let engine: Arc<dyn MapEngine> = Arc::new(JitteredTimesMockEngine);
        let mut engines = HashMap::new();
        let mut collections = HashMap::new();
        engines.insert("jit".to_string(), engine);
        collections.insert(
            "jit".to_string(),
            CollectionConfig {
                id: "jit".to_string(),
                title: "Jittered".to_string(),
                description: "Jittered".to_string(),
                data_path: None,
                apis: vec!["tiles".to_string()],
                engine_type: "geotiff".to_string(),
                keywords: Vec::new(),
                license: None,
                geotiff: None,
                querydata: None,
                wms: None,
                grib: None,
                zarr: None,
                odim: None,
                cap: None,
                postgis: None,
                nowcast: None,
                preview: None,
            },
        );
        let state = Arc::new(ArcSwap::from_pointee(TilesState {
            map_engines: engines,
            collections,
            styles: HashMap::new(),
            feature_engines: HashMap::new(),
            feature_collections: HashMap::new(),
            render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
            rendered_cache: Arc::new(RenderedCache::new(16)),
            vector_tile_cache: Arc::new(VectorTileCache::new(16)),
            base_url: String::new(),
            trust_proxy_headers: false,
        }));
        let app = api_tiles::router(state);
        let req = Request::builder()
            .uri("/collections/jit")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&body).unwrap();
        let grid = &json["extent"]["temporal"]["grid"];
        assert_eq!(grid["cellsCount"], 4);
        assert_eq!(grid["resolution"], "PT1H");
        assert!(grid.get("coordinates").is_none());
    }
}

// ---------------------------------------------------------------------------
// OGC API - Common - Part 4: Searchable Collections — handler wiring smoke
// ---------------------------------------------------------------------------

mod part4_searchable {
    use super::*;

    #[tokio::test]
    async fn conformance_declares_searchable_collections() {
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        assert!(classes.iter().any(|c| c
            .as_str()
            .unwrap()
            .contains("common-4/1.0/conf/searchable-collections")));
    }

    #[tokio::test]
    async fn collections_has_match_counts() {
        let (status, json) = get("/collections").await;
        assert_eq!(status, StatusCode::OK);
        assert!(json["numberMatched"].is_number());
        assert!(json["numberReturned"].is_number());
    }

    #[tokio::test]
    async fn invalid_limit_is_400() {
        let (status, _) = get("/collections?limit=-1").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}

// ---------------------------------------------------------------------------
// Content negotiation — ?f=json|html (OGC API Common Part 2 conf/html, #296)
// ---------------------------------------------------------------------------
mod content_negotiation {
    use super::*;

    #[tokio::test]
    async fn f_html_serves_html() {
        let (status, headers, body) = get_raw("/collections?f=html").await;
        assert_eq!(status, StatusCode::OK);
        let ct = headers.get("content-type").unwrap().to_str().unwrap();
        assert!(ct.starts_with("text/html"), "content-type was {ct}");
        assert!(String::from_utf8_lossy(&body).contains("<!DOCTYPE html>"));
    }

    #[tokio::test]
    async fn collection_detail_serves_html() {
        let (status, headers, _) = get_raw("/collections/radar?f=html").await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/html"));
    }

    #[tokio::test]
    async fn unknown_f_is_400() {
        let (status, _, _) = get_raw("/collections?f=xml").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn accept_header_selects_html() {
        let app = build_router();
        let req = Request::builder()
            .uri("/collections")
            .header("accept", "text/html")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ct.starts_with("text/html"), "content-type was {ct}");
    }

    /// Negotiated responses carry `Vary: Accept` so shared caches don't serve
    /// the wrong representation.
    #[tokio::test]
    async fn negotiated_responses_set_vary_accept() {
        for uri in [
            "/collections",
            "/collections?f=html",
            "/collections/radar",
            "/conformance",
            "/",
        ] {
            let (_, headers, _) = get_raw(uri).await;
            let vary = headers
                .get("vary")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            assert!(
                vary.to_ascii_lowercase().contains("accept"),
                "{uri}: missing Vary: Accept (got {vary:?})"
            );
        }
    }
}

/// Keywords + license surface in the Tiles collection JSON. api-maps/api-wms
/// had assertions already; this closes the gap for Tiles (review on PR #324).
mod metadata_extras {
    use super::*;

    fn state_with(
        keywords: Vec<String>,
        license: Option<ds_core::config::LicenseConfig>,
    ) -> axum::Router {
        let mut engines = HashMap::new();
        let mut collections = HashMap::new();
        engines.insert(
            "radar".to_string(),
            Arc::new(MockMapEngine::new()) as Arc<dyn MapEngine>,
        );
        collections.insert(
            "radar".to_string(),
            CollectionConfig {
                id: "radar".to_string(),
                title: "Test Radar".to_string(),
                description: "Test radar data".to_string(),
                data_path: None,
                apis: vec!["tiles".to_string()],
                engine_type: "geotiff".to_string(),
                keywords,
                license,
                geotiff: None,
                querydata: None,
                wms: None,
                grib: None,
                zarr: None,
                odim: None,
                cap: None,
                postgis: None,
                nowcast: None,
                preview: None,
            },
        );
        let state = Arc::new(ArcSwap::from_pointee(TilesState {
            map_engines: engines,
            collections,
            styles: HashMap::new(),
            feature_engines: HashMap::new(),
            feature_collections: HashMap::new(),
            render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
            rendered_cache: Arc::new(RenderedCache::new(16)),
            vector_tile_cache: Arc::new(VectorTileCache::new(16)),
            base_url: String::new(),
            trust_proxy_headers: false,
        }));
        api_tiles::router(state)
    }

    async fn collection_json(
        keywords: Vec<String>,
        license: Option<ds_core::config::LicenseConfig>,
    ) -> Value {
        let app = state_with(keywords, license);
        let req = Request::builder()
            .uri("/collections/radar")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&body).unwrap()
    }

    async fn q_count(q: &str) -> u64 {
        let app = state_with(vec!["thunderstorm".into()], None);
        let req = Request::builder().uri(q).body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&body).unwrap();
        json["numberMatched"].as_u64().unwrap()
    }

    #[tokio::test]
    async fn keyword_is_matched_by_q_search() {
        // End-to-end guard for config.keywords -> tuple -> CollectionMatch.
        assert_eq!(q_count("/collections?q=thunderstorm").await, 1);
        assert_eq!(q_count("/collections?q=zzznotaword").await, 0);
    }

    #[tokio::test]
    async fn keywords_and_license_surface_in_json() {
        let lic = ds_core::config::LicenseConfig {
            title: "CC-BY-4.0".into(),
            url: None,
        };
        let json = collection_json(vec!["radar".into(), "weather".into()], Some(lic)).await;
        assert_eq!(json["keywords"], serde_json::json!(["radar", "weather"]));
        let link = json["links"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["rel"] == "license")
            .expect("a rel=license link");
        assert_eq!(link["href"], "https://spdx.org/licenses/CC-BY-4.0.html");
        assert_eq!(link["title"], "CC-BY-4.0");
    }

    #[tokio::test]
    async fn no_keywords_or_license_when_unset() {
        let json = collection_json(Vec::new(), None).await;
        assert!(json.get("keywords").is_none());
        assert!(json["links"]
            .as_array()
            .unwrap()
            .iter()
            .all(|l| l["rel"] != "license"));
    }
}

// ---------------------------------------------------------------------------
// Run-less tile must track the engine's latest model run (#521)
// ---------------------------------------------------------------------------

/// Mock forecast engine whose run list can be advanced mid-test — the Tiles
/// twin of the WMS/Maps `RunSwapMockMapEngine`. Tiles never pins a run (the
/// `reference_time` query parameter is a #337 Phase 4 follow-up), so every
/// request exercises the `resolve_reference_time(time, None)` path.
struct RunSwapMockMapEngine {
    runs: Arc<std::sync::RwLock<Vec<chrono::DateTime<chrono::Utc>>>>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl MapEngine for RunSwapMockMapEngine {
    fn get_raster_tile(
        &self,
        _bbox: [f64; 4],
        width: u32,
        height: u32,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        _output_crs: &OutputCrs,
        _parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let idx = self.runs.read().unwrap().len().min(2);
        let v = 0.2 + 0.3 * idx as f64;
        Ok(RasterTile {
            width,
            height,
            values: vec![Some(v); (width * height) as usize].into(),
        })
    }

    fn raster_info(&self) -> RasterInfo {
        RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
            times: vec!["2026-07-11T20:00:00Z".parse().unwrap()],
            parameter: "reflectivity".into(),
            unit: "dBZ".into(),
            parameters: vec![],
            vertical: None,
            grid_size: None,
            layer_subtitle: None,
            reference_times: self.runs.read().unwrap().clone(),
        }
    }

    fn resolve_reference_time(
        &self,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        // Run-retaining engine contract (#521): None ⇒ the concrete run the
        // render would use (latest here).
        reference_time.or_else(|| self.runs.read().unwrap().last().copied())
    }
}

/// The #521 stale-run replay on the Tiles path: the no-TTL rendered cache must
/// key on the concrete latest run so a new run (or nowcast generation)
/// re-renders instead of serving the first run's pixels forever.
#[tokio::test]
async fn tile_re_renders_when_a_new_run_lands() {
    let runs = Arc::new(std::sync::RwLock::new(vec!["2026-07-11T12:00:00Z"
        .parse()
        .unwrap()]));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let app = build_router_with_engine(Arc::new(RunSwapMockMapEngine {
        runs: runs.clone(),
        calls: calls.clone(),
    }));
    let uri = "/collections/radar/tiles/WebMercatorQuad/2/2/1";

    // 1. Cold render under run 1.
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);

    // 2. Repeat: pure cache hit under the concrete run-1 key.
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);

    // 3. A new run supersedes the latest → the same request must re-render.
    //    Pre-#521 (key reference_time: None) this served the stale hit.
    runs.write()
        .unwrap()
        .push("2026-07-11T18:00:00Z".parse().unwrap());
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "new latest run must miss the run-1 cache entry and re-render"
    );
}
