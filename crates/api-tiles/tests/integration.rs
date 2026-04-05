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
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
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
