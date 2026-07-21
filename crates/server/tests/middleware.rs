//! Server-level integration tests for cross-cutting middleware concerns.
//!
//! These tests assemble the full router with mock engines and verify
//! gzip compression, conditional requests (304), and load shedding (503).

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use tower_http::compression::CompressionLayer;

use api_wms::WmsState;
use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use ds_render::{BuiltinColormap, LutColorMap, RenderedCache, StyleInfo};

// ---------------------------------------------------------------------------
// Mock engine
// ---------------------------------------------------------------------------

struct MockMapEngine;

impl MapEngine for MockMapEngine {
    #[allow(clippy::too_many_arguments)]
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
        let n = (width * height) as usize;
        Ok(RasterTile {
            width,
            height,
            values: (0..n)
                .map(|i| Some(i as f64 / n as f64))
                .collect::<Vec<_>>()
                .into(),
        })
    }

    fn raster_info(&self) -> RasterInfo {
        RasterInfo {
            native_crs: "EPSG:4326".to_string(),
            spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
            times: vec![chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc)],
            parameter: "reflectivity".to_string(),
            unit: "dBZ".to_string(),
            parameters: vec![],
            vertical: None,
            grid_size: None,
            layer_subtitle: None,
            reference_times: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_collection(id: &str) -> CollectionConfig {
    CollectionConfig {
        id: id.to_string(),
        title: "Test".to_string(),
        description: "Test data".to_string(),
        data_path: None,
        apis: vec!["wms".to_string()],
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
    }
}

fn make_styles() -> HashMap<String, StyleInfo> {
    let mut m = HashMap::new();
    m.insert(
        "default".to_string(),
        StyleInfo {
            name: "default".to_string(),
            title: "Default".to_string(),
            colormap: Arc::new(LutColorMap::from_builtin(
                BuiltinColormap::Viridis,
                0.0,
                1.0,
            )),
            min: 0.0,
            max: 1.0,
            parameter: None,
        },
    );
    m
}

fn build_wms_router() -> axum::Router {
    let engine: Arc<dyn MapEngine> = Arc::new(MockMapEngine);
    let mut engines = HashMap::new();
    let mut collections = HashMap::new();
    let mut styles = HashMap::new();

    engines.insert("radar".to_string(), engine);
    collections.insert("radar".to_string(), make_collection("radar"));
    styles.insert("radar".to_string(), make_styles());

    let state = Arc::new(ArcSwap::from_pointee(WmsState {
        engines,
        collections,
        styles,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        tile_cache: Arc::new(ds_render::TilePixelCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
    }));
    api_wms::router(state)
}

/// Build the WMS router wrapped in CompressionLayer (like main.rs does).
fn build_compressed_router() -> axum::Router {
    build_wms_router().layer(CompressionLayer::new())
}

const WMS_GETMAP: &str = "/?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap\
    &LAYERS=radar&CRS=CRS:84&STYLES=default\
    &BBOX=10,55,30,70&WIDTH=64&HEIGHT=64&FORMAT=image/png";

const WMS_GETCAP: &str = "/?SERVICE=WMS&REQUEST=GetCapabilities";

async fn wms_request(
    router: axum::Router,
    uri: &str,
    headers: Vec<(&str, &str)>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().uri(uri);
    for (k, v) in headers {
        builder = builder.header(k, v);
    }
    let req = builder.body(Body::empty()).unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let resp_headers = resp.headers().clone();
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, resp_headers, body)
}

// ---------------------------------------------------------------------------
// Gzip compression tests
// ---------------------------------------------------------------------------

mod compression {
    use super::*;

    #[tokio::test]
    async fn getcapabilities_compressed_when_accepted() {
        let app = build_compressed_router();
        let (status, headers, _body) =
            wms_request(app, WMS_GETCAP, vec![("accept-encoding", "gzip")]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers.get("content-encoding").map(|v| v.to_str().unwrap()),
            Some("gzip")
        );
    }

    #[tokio::test]
    async fn getcapabilities_uncompressed_without_header() {
        let app = build_compressed_router();
        let (status, headers, _body) = wms_request(app, WMS_GETCAP, vec![]).await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers.get("content-encoding").is_none());
    }

    #[tokio::test]
    async fn png_not_double_compressed() {
        let app = build_compressed_router();
        let (status, _headers, body) =
            wms_request(app, WMS_GETMAP, vec![("accept-encoding", "gzip")]).await;
        assert_eq!(status, StatusCode::OK);
        // PNG magic bytes should be present (not wrapped in gzip)
        // CompressionLayer skips image/* content types
        let is_png = body.starts_with(&[0x89, b'P', b'N', b'G']);
        let is_gzip = body.starts_with(&[0x1f, 0x8b]);
        assert!(
            is_png || !is_gzip,
            "PNG response should not be gzip-compressed"
        );
    }
}

// ---------------------------------------------------------------------------
// Conditional requests (304 Not Modified)
// ---------------------------------------------------------------------------

mod conditional_requests {
    use super::*;

    #[tokio::test]
    async fn returns_304_for_matching_etag() {
        let app = build_wms_router();

        // First request — get the ETag
        let (status, headers, _) = wms_request(app, WMS_GETMAP, vec![]).await;
        assert_eq!(status, StatusCode::OK);
        let etag = headers
            .get(header::ETAG)
            .expect("response should have ETag")
            .to_str()
            .unwrap()
            .to_string();

        // Second request with If-None-Match
        let app = build_wms_router();
        let (status, _, body) = wms_request(app, WMS_GETMAP, vec![("if-none-match", &etag)]).await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
        assert!(body.is_empty(), "304 body should be empty");
    }

    #[tokio::test]
    async fn returns_200_for_non_matching_etag() {
        let app = build_wms_router();
        let (status, _, body) =
            wms_request(app, WMS_GETMAP, vec![("if-none-match", "\"wrongetag\"")]).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.is_empty());
    }

    #[tokio::test]
    async fn returns_304_for_wildcard() {
        let app = build_wms_router();
        let (status, _, _) = wms_request(app, WMS_GETMAP, vec![("if-none-match", "*")]).await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn returns_304_for_etag_in_list() {
        let app = build_wms_router();

        // Get the real ETag first
        let (_, headers, _) = wms_request(app, WMS_GETMAP, vec![]).await;
        let etag = headers.get(header::ETAG).unwrap().to_str().unwrap();

        // Send it in a comma-separated list
        let multi = format!("\"aaa\", {etag}, \"zzz\"");
        let app = build_wms_router();
        let (status, _, _) = wms_request(app, WMS_GETMAP, vec![("if-none-match", &multi)]).await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn returns_304_for_weak_etag() {
        let app = build_wms_router();

        let (_, headers, _) = wms_request(app, WMS_GETMAP, vec![]).await;
        let etag = headers.get(header::ETAG).unwrap().to_str().unwrap();

        // Wrap in W/ weak prefix
        let weak = format!("W/{etag}");
        let app = build_wms_router();
        let (status, _, _) = wms_request(app, WMS_GETMAP, vec![("if-none-match", &weak)]).await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn not_modified_includes_etag_and_cache_control() {
        let app = build_wms_router();
        let (status, _, _) = wms_request(app, WMS_GETMAP, vec![("if-none-match", "*")]).await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);

        // Rebuild for the assertion (oneshot consumes)
        let app = build_wms_router();
        let (_, headers, _) = wms_request(app, WMS_GETMAP, vec![("if-none-match", "*")]).await;
        assert!(headers.get(header::ETAG).is_some(), "304 must include ETag");
        assert!(
            headers.get(header::CACHE_CONTROL).is_some(),
            "304 must include Cache-Control"
        );
    }
}

// ---------------------------------------------------------------------------
// Load shedding (503 Service Unavailable)
// ---------------------------------------------------------------------------

mod load_shedding {
    use super::*;

    #[tokio::test]
    async fn returns_503_when_semaphore_exhausted() {
        // Build router with 0 permits — every render request will timeout
        let engine: Arc<dyn MapEngine> = Arc::new(MockMapEngine);
        let mut engines = HashMap::new();
        let mut collections = HashMap::new();
        let mut styles = HashMap::new();

        engines.insert("radar".to_string(), engine);
        collections.insert("radar".to_string(), make_collection("radar"));
        styles.insert("radar".to_string(), make_styles());

        let state = Arc::new(ArcSwap::from_pointee(WmsState {
            engines,
            collections,
            styles,
            render_semaphore: Arc::new(tokio::sync::Semaphore::new(0)), // 0 permits!
            rendered_cache: Arc::new(RenderedCache::new(16)),
            tile_cache: Arc::new(ds_render::TilePixelCache::new(16)),
            base_url: String::new(),
            trust_proxy_headers: false,
        }));
        let app = api_wms::router(state);

        // This should timeout and return 503
        // Use a short timeout by temporarily checking the behavior
        let (status, _, body) = wms_request(app, WMS_GETMAP, vec![]).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        // WMS returns XML error
        let body_str = String::from_utf8_lossy(&body);
        assert!(
            body_str.contains("ServiceException"),
            "503 should return WMS ServiceExceptionReport XML"
        );
    }
}
