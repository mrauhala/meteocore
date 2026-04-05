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
            apis: vec!["maps".to_string()],
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
        let (status, _) = get("/collections/radar/map?bbox=10,55,30,70&width=5000").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn pixel_count_too_large_returns_400() {
        let (status, _) =
            get("/collections/radar/map?bbox=10,55,30,70&width=4096&height=4096").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
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
