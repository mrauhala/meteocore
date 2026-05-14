//! Integration tests for the WMS endpoint.
//!
//! The crate ships with handler-level unit tests in `src/error.rs` and
//! `src/params.rs`. This file covers behaviours that only emerge once
//! the router, mock engine, and full state are wired together — the
//! seed test is the regression for #162 (empty-tile Content-Type
//! mismatch when a non-PNG format is requested).

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use api_wms::WmsState;
use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use ds_render::{BuiltinColormap, LutColorMap, RenderedCache, StyleInfo};

/// Mock engine that returns an all-`None` (all-nodata) `RasterTile`.
/// Drives the empty-tile fast path that bypasses the format-aware
/// encoder and emits PNG bytes directly — the code path that #162
/// fixed.
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
    ) -> Result<RasterTile, DataServerError> {
        let pixel_count = (width * height) as usize;
        Ok(RasterTile {
            width,
            height,
            values: vec![None; pixel_count],
        })
    }

    fn raster_info(&self) -> RasterInfo {
        RasterInfo {
            native_crs: "EPSG:4326".into(),
            spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
            times: vec![chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc)],
            parameter: "reflectivity".into(),
            unit: "dBZ".into(),
            parameters: vec![],
        }
    }
}

/// Build a WMS router whose only collection (`empty`) is backed by
/// `EmptyMockMapEngine`. Used by the empty-tile regression tests.
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
            apis: vec!["wms".to_string()],
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
            colormap: cmap,
            min: 0.0,
            max: 1.0,
            parameter: None,
        },
    );
    styles_map.insert("empty".to_string(), layer_styles);

    let state = Arc::new(ArcSwap::from_pointee(WmsState {
        engines,
        collections,
        styles: styles_map,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        base_url: String::new(),
    }));
    api_wms::router(state)
}

/// Regression for #162: when the engine produces an all-nodata tile,
/// the WMS GetMap handler short-circuits to a freshly-encoded
/// transparent PNG without going through the format-aware encoder.
/// Before the fix the response carried the *requested* Content-Type
/// (e.g. `image/jpeg`) over PNG bytes, breaking decoders that trust
/// the header. Both header and body must agree.
#[tokio::test]
async fn empty_tile_forces_png_content_type_even_when_jpeg_requested() {
    let app = build_empty_router();
    let req = Request::builder()
        .uri(
            "/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS=empty\
             &CRS=CRS:84&BBOX=10,55,30,70&WIDTH=64&HEIGHT=64\
             &FORMAT=image/jpeg",
        )
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
    // Second format for symmetry — WebP takes a distinct `ImageFormat`
    // branch through the cache key, so this exercises a different code
    // path than the JPEG case above.
    let app = build_empty_router();
    let req = Request::builder()
        .uri(
            "/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS=empty\
             &CRS=CRS:84&BBOX=10,55,30,70&WIDTH=64&HEIGHT=64\
             &FORMAT=image/webp",
        )
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let headers = resp.headers().clone();
    assert_eq!(headers.get("content-type").unwrap(), "image/png");
    assert_eq!(headers.get("x-cache").unwrap(), "EMPTY");
}

/// Mock engine whose `get_raster_tile` always fails. Drives the WMS
/// `Err(e)` branch that emits the red `render_error_tile` PNG with
/// `X-Cache: ERROR`. WMS is the only handler that does this — Maps
/// and Tiles propagate engine errors as JSON 500.
struct FailingMockMapEngine;

impl MapEngine for FailingMockMapEngine {
    fn get_raster_tile(
        &self,
        _bbox: [f64; 4],
        _width: u32,
        _height: u32,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        _output_crs: &OutputCrs,
        _parameter: Option<&str>,
    ) -> Result<RasterTile, DataServerError> {
        Err(DataServerError::Engine("intentional render failure".into()))
    }

    fn raster_info(&self) -> RasterInfo {
        RasterInfo {
            native_crs: "EPSG:4326".into(),
            spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
            times: vec![chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc)],
            parameter: "reflectivity".into(),
            unit: "dBZ".into(),
            parameters: vec![],
        }
    }
}

/// WMS variant of [`build_empty_router`] for the engine-error path.
fn build_failing_router() -> axum::Router {
    let engine: Arc<dyn MapEngine> = Arc::new(FailingMockMapEngine);
    let mut engines = HashMap::new();
    let mut collections = HashMap::new();
    let mut styles_map = HashMap::new();

    engines.insert("broken".to_string(), engine);
    collections.insert(
        "broken".to_string(),
        CollectionConfig {
            id: "broken".to_string(),
            title: "Broken".to_string(),
            description: "Engine that always errors — for ERROR-path coverage".into(),
            data_path: None,
            apis: vec!["wms".to_string()],
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
            colormap: cmap,
            min: 0.0,
            max: 1.0,
            parameter: None,
        },
    );
    styles_map.insert("broken".to_string(), layer_styles);

    let state = Arc::new(ArcSwap::from_pointee(WmsState {
        engines,
        collections,
        styles: styles_map,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        base_url: String::new(),
    }));
    api_wms::router(state)
}

/// Regression for #162 (WMS error-tile branch): when the engine
/// returns `Err`, the handler swallows it and serves a red
/// `render_error_tile` PNG with `X-Cache: ERROR`. The PR also fixed
/// this branch to emit `Content-Type: image/png` instead of the
/// requested-but-misleading `image/jpeg`/`image/webp`.
#[tokio::test]
async fn error_tile_forces_png_content_type_even_when_jpeg_requested() {
    let app = build_failing_router();
    let req = Request::builder()
        .uri(
            "/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS=broken\
             &CRS=CRS:84&BBOX=10,55,30,70&WIDTH=64&HEIGHT=64\
             &FORMAT=image/jpeg",
        )
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "WMS swallows engine errors into a 200 + error-tile"
    );
    let headers = resp.headers().clone();
    assert_eq!(
        headers.get("content-type").unwrap(),
        "image/png",
        "error-tile response must self-declare PNG, not the requested image/jpeg"
    );
    assert_eq!(headers.get("x-cache").unwrap(), "ERROR");
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(
        body.starts_with(&[0x89, b'P', b'N', b'G']),
        "body must be a real PNG, got first bytes {:?}",
        &body[..4.min(body.len())]
    );
}
