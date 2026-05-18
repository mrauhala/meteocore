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
        RasterInfo {
            native_crs: "EPSG:4326".into(),
            spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
            times: vec![chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc)],
            parameter: "reflectivity".into(),
            unit: "dBZ".into(),
            parameters: vec![],
            vertical: None,
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
        _z: Option<f64>,
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
            vertical: None,
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

/// Mock engine that returns a populated `RasterTile` so the regular
/// `Ok(Some(bytes))` render+encode path runs (the EMPTY/ERROR paths
/// have separate coverage above). Used by the #145 ETag regression
/// tests.
struct PopulatedMockMapEngine;

impl MapEngine for PopulatedMockMapEngine {
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
        // Linear gradient — yields a non-uniform PNG so the test isn't
        // accidentally cheating by serving the EMPTY_TILE_PNG fast path.
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
        RasterInfo {
            native_crs: "EPSG:4326".into(),
            spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
            times: vec![chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc)],
            parameter: "reflectivity".into(),
            unit: "dBZ".into(),
            parameters: vec![],
            vertical: None,
        }
    }
}

fn build_populated_router() -> axum::Router {
    let engine: Arc<dyn MapEngine> = Arc::new(PopulatedMockMapEngine);
    let mut engines = HashMap::new();
    let mut collections = HashMap::new();
    let mut styles_map = HashMap::new();

    engines.insert("radar".to_string(), engine);
    collections.insert(
        "radar".to_string(),
        CollectionConfig {
            id: "radar".to_string(),
            title: "Radar".to_string(),
            description: "Populated mock for #145 ETag tests".into(),
            data_path: None,
            apis: vec!["wms".to_string()],
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
    styles_map.insert("radar".to_string(), layer_styles);

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

const GETMAP_URI: &str = "/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS=radar\
                          &CRS=CRS:84&BBOX=10,55,30,70&WIDTH=64&HEIGHT=64\
                          &FORMAT=image/png";

/// Regression for #145: the WMS GetMap ETag must be FNV-1a over the
/// rendered bytes — not over the cache key — so a server-side fix
/// that produces different pixels under the same key surfaces a
/// fresh ETag and clients holding the stale entry refetch instead
/// of receiving an infinite 304.
#[tokio::test]
async fn etag_is_content_derived_over_response_body() {
    let app = build_populated_router();
    let req = Request::builder()
        .uri(GETMAP_URI)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let headers = resp.headers().clone();
    let actual_etag = headers.get("etag").unwrap().to_str().unwrap().to_string();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let expected_etag = ds_render::CachedRendered::new(body).etag().to_string();
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
    let app = build_populated_router();
    let resp_a = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(GETMAP_URI)
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

    let resp_b = app
        .oneshot(
            Request::builder()
                .uri(GETMAP_URI)
                .header("If-None-Match", &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp_b.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(
        resp_b.headers().get("etag").unwrap().to_str().unwrap(),
        etag,
        "304 response must echo the same content-derived ETag"
    );
    assert_eq!(
        resp_b.headers().get("x-cache").map(|v| v.to_str().unwrap()),
        Some("HIT"),
        "304 must come from the cache-HIT branch, not post-render MISS"
    );
}

/// Pin the post-render MISS → 304 branch. Use a fresh router (no
/// cache-warm) so the first `If-None-Match`-bearing request must go
/// through the full render path; assert the 304 carries
/// `x-cache: MISS` rather than the cache-HIT branch's `HIT`.
#[tokio::test]
async fn if_none_match_against_fresh_router_returns_304_via_miss_branch() {
    // Step 1: render once on a separate fresh router to learn the ETag.
    let etag = {
        let warm = build_populated_router();
        let resp = warm
            .oneshot(
                Request::builder()
                    .uri(GETMAP_URI)
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

    // Step 2: brand-new router with an empty cache. Handler must render,
    // compute the same content-derived ETag, match the header, and 304
    // via the post-render branch.
    let app = build_populated_router();
    let req = Request::builder()
        .uri(GETMAP_URI)
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

/// Empty-tile revalidation must round-trip the `EMPTY` label, not be
/// silently re-tagged as `MISS`. Empty tiles bypass `rendered_cache`,
/// so an `If-None-Match` request always falls through to the
/// post-render branch — exactly the branch the round-7 fix targets.
/// Without the fix, every cached transparent tile that gets
/// revalidated would show up on dashboards as `MISS`.
#[tokio::test]
async fn if_none_match_on_empty_tile_returns_304_with_x_cache_empty() {
    let app = build_empty_router();
    let uri = "/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS=empty\
               &CRS=CRS:84&BBOX=10,55,30,70&WIDTH=64&HEIGHT=64\
               &FORMAT=image/png";

    // Step 1: render the empty tile to capture its (deterministic) ETag.
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

    // Step 2: revalidate. The post-render branch must forward
    // `x-cache: EMPTY`, not a hard-coded MISS.
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

/// WMS-only: revalidating a cached error tile must round-trip the
/// `ERROR` label. WMS is the one handler that swallows engine errors
/// into a 200 + red error-tile (Maps/Tiles propagate as 500), so this
/// branch only exists here. Error tiles bypass `rendered_cache`, so
/// `If-None-Match` always reaches the post-render branch.
#[tokio::test]
async fn if_none_match_on_error_tile_returns_304_with_x_cache_error() {
    let app = build_failing_router();
    let uri = "/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS=broken\
               &CRS=CRS:84&BBOX=10,55,30,70&WIDTH=64&HEIGHT=64\
               &FORMAT=image/png";

    let etag = {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.headers().get("x-cache").unwrap(), "ERROR");
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
    assert_eq!(
        resp.headers().get("etag").unwrap().to_str().unwrap(),
        etag,
        "304 response must echo the same content-derived ETag back to the client"
    );
    assert_eq!(
        resp.headers().get("x-cache").map(|v| v.to_str().unwrap()),
        Some("ERROR"),
        "post-render 304 must forward the `x_cache` label — error \
         tiles revalidating must remain visible as ERROR on dashboards"
    );
}
