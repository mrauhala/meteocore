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
            native_crs: "EPSG:4326".to_string(),
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
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
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
    async fn collection_exposes_apis_array() {
        let (_, json) = get("/collections/radar").await;
        let apis = json["apis"].as_array().expect("apis must be present");
        assert!(apis.iter().any(|a| a == "tiles"));
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

    #[tokio::test]
    async fn parameter_name_changes_etag() {
        // Different parameter-name values must produce different cache
        // entries (and thus different ETags) — otherwise a client switching
        // from "2t" to "10u" would get a 304 against the stale parameter.
        let (_, headers_a, _) =
            get_raw("/collections/radar/tiles/WebMercatorQuad/0/0/0?parameter-name=2t").await;
        let (_, headers_b, _) =
            get_raw("/collections/radar/tiles/WebMercatorQuad/0/0/0?parameter-name=10u").await;
        let etag_a = headers_a.get("etag").unwrap();
        let etag_b = headers_b.get("etag").unwrap();
        assert_ne!(
            etag_a, etag_b,
            "ETags must differ across parameter-name values"
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
        assert!(
            text.contains("Available: 2t, 10u") || text.contains("Available: 10u, 2t"),
            "error body should list available parameters; got: {text}"
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
        _parameter: Option<&str>,
    ) -> Result<RasterTile, DataServerError> {
        let pixel_count = (width * height) as usize;
        let values: Vec<Option<f64>> = (0..pixel_count).map(|i| Some(i as f64)).collect();
        Ok(RasterTile {
            width,
            height,
            values,
        })
    }

    fn raster_info(&self) -> RasterInfo {
        RasterInfo {
            native_crs: "EPSG:4326".into(),
            spatial_extent: Some([0.0, 0.0, 10.0, 10.0]),
            times: vec![],
            parameter: "2t".into(),
            unit: "K".into(),
            parameters: vec![
                ("2t".into(), "Temperature".into()),
                ("10u".into(), "U Wind".into()),
            ],
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
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
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
                geotiff: None,
                querydata: None,
                wms: None,
                grib: None,
                postgis: None,
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
                geotiff: None,
                querydata: None,
                wms: None,
                grib: None,
                postgis: None,
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
