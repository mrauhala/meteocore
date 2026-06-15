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
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
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
            grid_size: None,
            layer_subtitle: None,
            reference_times: Vec::new(),
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
    // A second, named style so legend tests can assert the selected STYLE flows
    // through (distinct colormap + range → distinct legend, plus the style name
    // on the legend's second title line).
    layer_styles.insert(
        "radar_fmi".to_string(),
        StyleInfo {
            name: "radar_fmi".to_string(),
            title: "FMI Radar".to_string(),
            colormap: Arc::new(LutColorMap::from_builtin(
                BuiltinColormap::RadarDbz,
                -32.0,
                95.0,
            )),
            min: -32.0,
            max: 95.0,
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
        tile_cache: Arc::new(ds_render::TilePixelCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
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
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
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
            grid_size: None,
            layer_subtitle: None,
            reference_times: Vec::new(),
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
        tile_cache: Arc::new(ds_render::TilePixelCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
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
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
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
            grid_size: None,
            layer_subtitle: None,
            reference_times: Vec::new(),
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
        tile_cache: Arc::new(ds_render::TilePixelCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
    }));
    api_wms::router(state)
}

const GETMAP_URI: &str = "/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS=radar\
                          &CRS=CRS:84&BBOX=10,55,30,70&WIDTH=64&HEIGHT=64\
                          &FORMAT=image/png";

/// `ELEVATION` against a layer with no vertical dimension
/// (`raster_info().vertical` is `None`) is a 400 ServiceException.
#[tokio::test]
async fn elevation_against_non_vertical_layer_returns_400() {
    let app = build_populated_router();
    let req = Request::builder()
        .uri(format!("{GETMAP_URI}&ELEVATION=0.5"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

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

// --- Meta-tiling (#202) ----------------------------------------------------

/// Data-producing engine that counts every `get_raster_tile` call. Lets the
/// meta-tiling tests prove that overlapping viewports reuse cached tiles
/// (fewer fresh engine renders) and that the non-3857 / kill-switch paths
/// bypass meta-tiling entirely.
struct CountingMockMapEngine {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl MapEngine for CountingMockMapEngine {
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
        // All in-range, opaque pixels so tiles are cached (have_data) and
        // colorize to a non-transparent image.
        Ok(RasterTile {
            width,
            height,
            values: vec![Some(0.5); (width * height) as usize],
        })
    }

    fn raster_info(&self) -> RasterInfo {
        RasterInfo {
            native_crs: "EPSG:3857".into(),
            spatial_extent: Some([-20.0, 30.0, 40.0, 80.0]),
            times: vec![chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc)],
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

/// Build a WMS router over a single `data` collection backed by a counting
/// engine, returning handles to the shared meta-tile cache and the engine's
/// call counter so tests can assert reuse. `metatile_mb = 0` exercises the
/// kill switch (meta-tiling bypassed).
fn build_counting_router(
    metatile_mb: u64,
) -> (
    axum::Router,
    Arc<ds_render::TilePixelCache>,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let engine: Arc<dyn MapEngine> = Arc::new(CountingMockMapEngine {
        calls: calls.clone(),
    });
    let mut engines = HashMap::new();
    let mut collections = HashMap::new();
    let mut styles_map = HashMap::new();

    engines.insert("data".to_string(), engine);
    collections.insert(
        "data".to_string(),
        CollectionConfig {
            id: "data".to_string(),
            title: "Data".to_string(),
            description: "Data-producing fixture for meta-tiling (#202)".to_string(),
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
    styles_map.insert("data".to_string(), layer_styles);

    let tile_cache = Arc::new(ds_render::TilePixelCache::new(metatile_mb));
    let state = Arc::new(ArcSwap::from_pointee(WmsState {
        engines,
        collections,
        styles: styles_map,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        tile_cache: tile_cache.clone(),
        base_url: String::new(),
        trust_proxy_headers: false,
    }));
    (api_wms::router(state), tile_cache, calls)
}

async fn get_map(app: &axum::Router, crs: &str, bbox: &str, w: u32, h: u32) -> StatusCode {
    let uri = format!(
        "/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS=data&STYLES=\
         &FORMAT=image/png&CRS={crs}&BBOX={bbox}&WIDTH={w}&HEIGHT={h}"
    );
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    resp.status()
}

/// Two overlapping EPSG:3857 fullscreen requests at the same resolution must
/// reuse cached meta-tiles: the second request hits the tile cache and renders
/// strictly fewer fresh tiles than the first. This is the core #202 win.
#[tokio::test]
async fn meta_tiling_reuses_tiles_across_overlapping_viewports() {
    let (app, tile_cache, calls) = build_counting_router(64);

    // Request A.
    let s1 = get_map(
        &app,
        "EPSG:3857",
        "2000000,8000000,3000000,9000000",
        512,
        512,
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    let calls_a = calls.load(std::sync::atomic::Ordering::Relaxed);
    let (hits_a, misses_a) = tile_cache.stats();
    assert!(calls_a > 0, "first request renders tiles");
    assert_eq!(
        misses_a as usize, calls_a,
        "every fresh tile is a cache miss"
    );
    assert_eq!(hits_a, 0, "no hits on a cold cache");

    // Request B: panned east by 200 km, same size/resolution → overlaps A.
    let s2 = get_map(
        &app,
        "EPSG:3857",
        "2200000,8000000,3200000,9000000",
        512,
        512,
    )
    .await;
    assert_eq!(s2, StatusCode::OK);
    let calls_b_delta = calls.load(std::sync::atomic::Ordering::Relaxed) - calls_a;
    let (hits_b, _) = tile_cache.stats();
    assert!(hits_b > 0, "overlapping viewport must hit cached tiles");
    assert!(
        calls_b_delta < calls_a,
        "panned request must render fewer fresh tiles ({calls_b_delta}) than the cold first one ({calls_a})"
    );
}

/// Non-Web-Mercator requests (here CRS:84) bypass meta-tiling and render
/// directly: the meta-tile cache is never touched.
#[tokio::test]
async fn non_web_mercator_bypasses_meta_tiling() {
    let (app, tile_cache, calls) = build_counting_router(64);
    let status = get_map(&app, "CRS:84", "10,55,30,70", 256, 256).await;
    assert_eq!(status, StatusCode::OK);
    let (hits, misses) = tile_cache.stats();
    assert_eq!(
        (hits, misses),
        (0, 0),
        "CRS:84 must not touch the meta-tile cache"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "direct render makes exactly one engine call"
    );
}

/// Kill switch: `metatile_cache_mb = 0` disables meta-tiling even for EPSG:3857,
/// reverting to a single direct render. Reversible via config reload.
#[tokio::test]
async fn zero_metatile_cache_disables_meta_tiling() {
    let (app, tile_cache, calls) = build_counting_router(0);
    let status = get_map(
        &app,
        "EPSG:3857",
        "2000000,8000000,3000000,9000000",
        512,
        512,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (hits, misses) = tile_cache.stats();
    assert_eq!((hits, misses), (0, 0), "disabled cache is never consulted");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "kill switch must fall back to a single direct render"
    );
}

/// Multi-parameter mock standing in for a PVOL radar-site collection: two
/// bare-quantity parameters with human labels, and a `layer_subtitle` carrying
/// the site place name. Drives the flat-client disambiguation in
/// `get_capabilities_xml` (parent layer per collection, one child per param).
struct SiteMockMapEngine;

impl MapEngine for SiteMockMapEngine {
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
            values: vec![None; pixel_count],
        })
    }

    fn raster_info(&self) -> RasterInfo {
        RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: Some([20.0, 58.0, 28.0, 62.0]),
            times: vec![chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc)],
            parameter: "DBZH".into(),
            unit: String::new(),
            parameters: vec![
                ("DBZH".into(), "DBZH — Reflectivity (horizontal)".into()),
                (
                    "VRADH".into(),
                    "VRADH — Radial velocity (horizontal)".into(),
                ),
            ],
            vertical: None,
            grid_size: None,
            layer_subtitle: Some("Vihti".into()),
            reference_times: Vec::new(),
        }
    }
}

fn site_collection_config(id: &str) -> CollectionConfig {
    CollectionConfig {
        id: id.to_string(),
        title: format!("Finnish radar volumes — Vihti ({id})"),
        description: "PVOL radar site".into(),
        data_path: None,
        apis: vec!["wms".to_string()],
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
        preview: None,
    }
}

/// A multi-parameter (PVOL site) collection emits, per WMS spec, a
/// non-requestable parent layer plus one requestable child layer per
/// parameter. The child `<Name>` stays `{id}/{quantity}` (the requestable
/// token), while the child `<Title>` is prefixed with the site place name from
/// `layer_subtitle` so a WMS client that ignores the parent tree can still tell
/// the sites apart. Without the prefix every site's child is titled identically.
#[test]
fn child_layer_titles_are_site_prefixed_for_flat_clients() {
    let mut engines: HashMap<String, Arc<dyn MapEngine>> = HashMap::new();
    let mut collections = HashMap::new();
    engines.insert("radar-fivih".to_string(), Arc::new(SiteMockMapEngine));
    collections.insert(
        "radar-fivih".to_string(),
        site_collection_config("radar-fivih"),
    );

    let styles: HashMap<String, HashMap<String, StyleInfo>> = HashMap::new();
    let xml = api_wms::capabilities::get_capabilities_xml(&engines, &collections, &styles, "");
    let xml = String::from_utf8(xml).expect("capabilities XML is UTF-8");

    // Requestable child <Name> is unchanged (id/quantity).
    assert!(
        xml.contains("<Name>radar-fivih/DBZH</Name>"),
        "child layer Name must stay the requestable id/quantity token; got:\n{xml}"
    );
    // Child <Title> is site-prefixed + human-readable.
    assert!(
        xml.contains("<Title>Vihti — DBZH — Reflectivity (horizontal)</Title>"),
        "child layer Title must be prefixed with the site name; got:\n{xml}"
    );
    assert!(
        xml.contains("<Title>Vihti — VRADH — Radial velocity (horizontal)</Title>"),
        "second child layer Title must also be site-prefixed; got:\n{xml}"
    );
    // The bare quantity must NOT appear as a standalone title (the bug being fixed).
    assert!(
        !xml.contains("<Title>DBZH</Title>"),
        "child Title must not be the bare quantity (ambiguous across sites); got:\n{xml}"
    );
}

/// Per-collection keywords surface as a `<KeywordList>` and the license as an
/// `<Attribution>` in the capabilities XML (on the parent layer of a
/// multi-param collection). Element order follows the WMS 1.3.0 schema.
#[test]
fn capabilities_emit_keywords_and_attribution() {
    let mut engines: HashMap<String, Arc<dyn MapEngine>> = HashMap::new();
    let mut collections = HashMap::new();
    engines.insert("radar-fivih".to_string(), Arc::new(SiteMockMapEngine));
    let mut config = site_collection_config("radar-fivih");
    config.keywords = vec!["radar".into(), "precipitation".into()];
    config.license = Some(ds_core::config::LicenseConfig {
        title: "CC-BY 4.0".into(),
        url: Some("https://example/lic".into()),
    });
    collections.insert("radar-fivih".to_string(), config);

    let styles: HashMap<String, HashMap<String, StyleInfo>> = HashMap::new();
    let xml = api_wms::capabilities::get_capabilities_xml(&engines, &collections, &styles, "");
    let xml = String::from_utf8(xml).expect("capabilities XML is UTF-8");

    assert!(
        xml.contains(
            "<KeywordList><Keyword>radar</Keyword><Keyword>precipitation</Keyword></KeywordList>"
        ),
        "KeywordList must list the configured keywords; got:\n{xml}"
    );
    assert!(
        xml.contains("<Attribution><Title>CC-BY 4.0</Title>")
            && xml.contains("xlink:href=\"https://example/lic\""),
        "Attribution must carry the license title + URL; got:\n{xml}"
    );
    // WMS 1.3.0 order: Abstract → KeywordList → (CRS/bbox), and
    // Dimension → Attribution. Anchor on the *layer's own* Abstract content
    // (not a bare `<Abstract>`, which also matches the service-level Abstract
    // earlier in the document), and on EX_GeographicBoundingBox as the lower
    // boundary (a root <CRS> appears earlier).
    let layer_abstract = xml.find("<Abstract>PVOL radar site</Abstract>").unwrap();
    let kw = xml.find("<KeywordList>").unwrap();
    assert!(
        layer_abstract < kw && kw < xml.find("<EX_GeographicBoundingBox>").unwrap(),
        "KeywordList must sit between the layer Abstract and the bounding box; got:\n{xml}"
    );
    assert!(
        xml.find("<Dimension").unwrap() < xml.find("<Attribution>").unwrap(),
        "Attribution must follow the Dimension elements; got:\n{xml}"
    );

    // The per-element xmlns:xlink declarations were dropped in favour of one on
    // the root; assert the root carries it so every xlink:href stays a defined
    // namespace reference (a strict parser would otherwise reject the document).
    let root_start = xml.find("<WMS_Capabilities").unwrap();
    let root_end = root_start + xml[root_start..].find('>').unwrap();
    assert!(
        xml[root_start..root_end].contains("xmlns:xlink=\"http://www.w3.org/1999/xlink\""),
        "root <WMS_Capabilities> must declare xmlns:xlink; got:\n{xml}"
    );
}

/// The single-parameter `write_layer` branch (no parent layer) must also emit
/// `<KeywordList>` after `<Abstract>` and `<Attribution>` after `<Dimension>`,
/// in WMS 1.3.0 schema order — the multi-param test above only covers the
/// parent-layer path.
#[test]
fn capabilities_single_param_layer_emits_keywords_and_attribution() {
    let mut engines: HashMap<String, Arc<dyn MapEngine>> = HashMap::new();
    let mut collections = HashMap::new();
    // EmptyMockMapEngine reports zero parameters → the single-layer path.
    engines.insert("solo".to_string(), Arc::new(EmptyMockMapEngine));
    collections.insert(
        "solo".to_string(),
        CollectionConfig {
            id: "solo".to_string(),
            title: "Solo".to_string(),
            description: "Single-param fixture".to_string(),
            data_path: None,
            apis: vec!["wms".to_string()],
            engine_type: "geotiff".to_string(),
            keywords: vec!["radar".into()],
            license: Some(ds_core::config::LicenseConfig {
                title: "CC-BY 4.0".into(),
                url: Some("https://example/lic".into()),
            }),
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            zarr: None,
            odim: None,
            cap: None,
            postgis: None,
            preview: None,
        },
    );

    let styles: HashMap<String, HashMap<String, StyleInfo>> = HashMap::new();
    let xml = api_wms::capabilities::get_capabilities_xml(&engines, &collections, &styles, "");
    let xml = String::from_utf8(xml).expect("capabilities XML is UTF-8");

    assert!(
        xml.contains("<KeywordList><Keyword>radar</Keyword></KeywordList>"),
        "single-param layer must emit its KeywordList; got:\n{xml}"
    );
    assert!(
        xml.contains("<Attribution><Title>CC-BY 4.0</Title>"),
        "single-param layer must emit its Attribution; got:\n{xml}"
    );
    let layer_abstract = xml
        .find("<Abstract>Single-param fixture</Abstract>")
        .unwrap();
    let kw = xml.find("<KeywordList>").unwrap();
    assert!(
        layer_abstract < kw && kw < xml.find("<EX_GeographicBoundingBox>").unwrap(),
        "KeywordList must sit between the layer Abstract and the bounding box; got:\n{xml}"
    );
    assert!(
        xml.find("<Dimension").unwrap() < xml.find("<Attribution>").unwrap(),
        "Attribution must follow the Dimension elements; got:\n{xml}"
    );
}

// ---------------------------------------------------------------------------
// Forecast `reference_time` dimension (#337 Phase 2)
// ---------------------------------------------------------------------------

/// Records the `reference_time` argument of each `get_raster_tile` call, so a
/// test can assert which model run the WMS handler asked the engine to render.
type RunRecorder = Arc<std::sync::Mutex<Vec<Option<chrono::DateTime<chrono::Utc>>>>>;

/// Two forecast model runs, ascending (latest last). Matches the canonical
/// EDR/GRIB convention.
fn forecast_runs() -> [chrono::DateTime<chrono::Utc>; 2] {
    [
        chrono::DateTime::parse_from_rfc3339("2026-06-07T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
        chrono::DateTime::parse_from_rfc3339("2026-06-07T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    ]
}

/// Mock forecast engine that retains two model runs and records the
/// `reference_time` it was asked to render, so tests can assert that the WMS
/// `DIM_REFERENCE_TIME` selector reaches the engine (and that omitting it
/// defaults to `None` ⇒ latest run).
struct ForecastMockMapEngine {
    calls: RunRecorder,
}

impl MapEngine for ForecastMockMapEngine {
    fn get_raster_tile(
        &self,
        _bbox: [f64; 4],
        width: u32,
        height: u32,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        _output_crs: &OutputCrs,
        _parameter: Option<&str>,
        _z: Option<f64>,
        reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        self.calls.lock().unwrap().push(reference_time);
        let pixel_count = (width * height) as usize;
        // Non-uniform so the response avoids the all-nodata fast path.
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
            times: vec![chrono::DateTime::parse_from_rfc3339("2026-06-07T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc)],
            parameter: "2t".into(),
            unit: "K".into(),
            parameters: vec![],
            vertical: None,
            grid_size: None,
            layer_subtitle: None,
            reference_times: forecast_runs().to_vec(),
        }
    }
}

/// Build a WMS router whose `ecmwf-fc` collection is a forecast engine with two
/// runs. Returns the router and the call-recorder so tests can assert which run
/// the engine was asked to render.
fn build_forecast_router() -> (axum::Router, RunRecorder) {
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let engine: Arc<dyn MapEngine> = Arc::new(ForecastMockMapEngine {
        calls: calls.clone(),
    });
    let mut engines = HashMap::new();
    let mut collections = HashMap::new();
    let mut styles_map = HashMap::new();

    engines.insert("ecmwf-fc".to_string(), engine);
    collections.insert(
        "ecmwf-fc".to_string(),
        CollectionConfig {
            id: "ecmwf-fc".to_string(),
            title: "ECMWF Forecast".to_string(),
            description: "Forecast fixture for #337 reference_time".into(),
            data_path: None,
            apis: vec!["wms".to_string()],
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
    styles_map.insert("ecmwf-fc".to_string(), layer_styles);

    let state = Arc::new(ArcSwap::from_pointee(WmsState {
        engines,
        collections,
        styles: styles_map,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        tile_cache: Arc::new(ds_render::TilePixelCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
    }));
    (api_wms::router(state), calls)
}

const FC_GETMAP_URI: &str = "/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS=ecmwf-fc\
                             &STYLES=&CRS=CRS:84&BBOX=10,55,30,70&WIDTH=64&HEIGHT=64\
                             &FORMAT=image/png";

/// A forecast collection advertises a custom `reference_time` dimension (the
/// model run) alongside the standard `time` dimension, defaulting to the latest
/// run, with both runs in the value list — and it sits among the `<Dimension>`s
/// (before any `<Attribution>`/`<Style>`).
#[test]
fn capabilities_emit_reference_time_dimension_for_forecast() {
    let mut engines: HashMap<String, Arc<dyn MapEngine>> = HashMap::new();
    let mut collections = HashMap::new();
    engines.insert(
        "ecmwf-fc".to_string(),
        Arc::new(ForecastMockMapEngine {
            calls: Arc::new(std::sync::Mutex::new(Vec::new())),
        }),
    );
    collections.insert(
        "ecmwf-fc".to_string(),
        CollectionConfig {
            id: "ecmwf-fc".to_string(),
            title: "ECMWF Forecast".to_string(),
            description: "Forecast fixture".into(),
            data_path: None,
            apis: vec!["wms".to_string()],
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
            preview: None,
        },
    );

    let styles: HashMap<String, HashMap<String, StyleInfo>> = HashMap::new();
    let xml = api_wms::capabilities::get_capabilities_xml(&engines, &collections, &styles, "");
    let xml = String::from_utf8(xml).expect("capabilities XML is UTF-8");

    // Both dimensions present: the valid-time axis and the run axis.
    assert!(
        xml.contains("<Dimension name=\"time\""),
        "forecast layer must keep the standard time dimension; got:\n{xml}"
    );
    assert!(
        xml.contains("<Dimension name=\"reference_time\" units=\"ISO8601\""),
        "forecast layer must advertise a reference_time dimension; got:\n{xml}"
    );
    // Default is the latest run.
    assert!(
        xml.contains("default=\"2026-06-07T12:00:00+00:00\""),
        "reference_time default must be the latest run; got:\n{xml}"
    );
    // Both runs listed as values.
    assert!(
        xml.contains("2026-06-07T00:00:00+00:00,2026-06-07T12:00:00+00:00</Dimension>"),
        "reference_time must list both runs ascending; got:\n{xml}"
    );
    // No nearestValue on the run dimension (exact match required).
    let rt_idx = xml.find("name=\"reference_time\"").unwrap();
    let rt_end = rt_idx + xml[rt_idx..].find('>').unwrap();
    assert!(
        !xml[rt_idx..rt_end].contains("nearestValue"),
        "reference_time dimension must not advertise nearestValue; got:\n{xml}"
    );
}

/// A non-forecast layer (no `reference_times`) emits no `reference_time`
/// dimension — the standard `time` dimension is the only one.
#[test]
fn capabilities_omit_reference_time_dimension_for_non_forecast() {
    let mut engines: HashMap<String, Arc<dyn MapEngine>> = HashMap::new();
    let mut collections = HashMap::new();
    engines.insert("empty".to_string(), Arc::new(EmptyMockMapEngine));
    collections.insert(
        "empty".to_string(),
        CollectionConfig {
            id: "empty".to_string(),
            title: "Empty".to_string(),
            description: "Non-forecast".to_string(),
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
            preview: None,
        },
    );

    let styles: HashMap<String, HashMap<String, StyleInfo>> = HashMap::new();
    let xml = api_wms::capabilities::get_capabilities_xml(&engines, &collections, &styles, "");
    let xml = String::from_utf8(xml).expect("capabilities XML is UTF-8");
    assert!(
        !xml.contains("reference_time"),
        "non-forecast layer must not advertise a reference_time dimension; got:\n{xml}"
    );
}

/// `GetMap` with no `DIM_REFERENCE_TIME` defaults to the latest run — the
/// engine is called with `reference_time = None`.
#[tokio::test]
async fn getmap_default_reference_time_is_none() {
    let (app, calls) = build_forecast_router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri(FC_GETMAP_URI)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let recorded = calls.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec![None],
        "omitting DIM_REFERENCE_TIME must query the latest run (None)"
    );
}

/// `GetMap` with a valid `DIM_REFERENCE_TIME` selects that run — the engine is
/// called with the pinned reference time.
#[tokio::test]
async fn getmap_selects_pinned_reference_time() {
    let (app, calls) = build_forecast_router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "{FC_GETMAP_URI}&DIM_REFERENCE_TIME=2026-06-07T00:00:00Z"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let recorded = calls.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec![Some(forecast_runs()[0])],
        "DIM_REFERENCE_TIME must select the pinned run"
    );
}

/// The compact EDR instance-id form (`20260607T0000Z`) is also accepted as a
/// `DIM_REFERENCE_TIME` value, resolving to the same run.
#[tokio::test]
async fn getmap_accepts_compact_instance_id_reference_time() {
    let (app, calls) = build_forecast_router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("{FC_GETMAP_URI}&DIM_REFERENCE_TIME=20260607T0000Z"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let recorded = calls.lock().unwrap().clone();
    assert_eq!(recorded, vec![Some(forecast_runs()[0])]);
}

/// The Web Mercator (EPSG:3857) meta-tile render path propagates the pinned run
/// in two independent places — `TileKeyPrefix.reference_time` and the
/// `get_raster_tile` closure inside `render_metatiled` — neither of which the
/// CRS:84 direct-path tests above exercise. Pin a non-latest run (so it isn't
/// normalised to `None`) and assert every tile render saw it.
#[tokio::test]
async fn getmap_metatile_path_selects_pinned_reference_time() {
    let (app, calls) = build_forecast_router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri(
                    "/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS=ecmwf-fc\
                     &STYLES=&CRS=EPSG:3857&BBOX=1113194,6982997,3339584,9100048\
                     &WIDTH=256&HEIGHT=256&FORMAT=image/png\
                     &DIM_REFERENCE_TIME=2026-06-07T00:00:00Z",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let recorded = calls.lock().unwrap().clone();
    assert!(
        !recorded.is_empty(),
        "meta-tile path must call the engine at least once"
    );
    assert!(
        recorded.iter().all(|rt| *rt == Some(forecast_runs()[0])),
        "every meta-tile get_raster_tile must carry the pinned run; got {recorded:?}"
    );
}

/// Explicitly pinning the *current latest* run is normalised to `None` so it
/// shares cache entries (and the engine's latest-run path) with requests that
/// omit the dimension — both render identical pixels. The engine therefore sees
/// `None`, not `Some(latest)`.
#[tokio::test]
async fn getmap_explicit_latest_run_normalizes_to_none() {
    let (app, calls) = build_forecast_router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "{FC_GETMAP_URI}&DIM_REFERENCE_TIME=2026-06-07T12:00:00Z"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let recorded = calls.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec![None],
        "pinning the current latest run must collapse to None (cache unified with no-dimension)"
    );
}

/// A `DIM_REFERENCE_TIME` that doesn't match an advertised run is a 400
/// `InvalidDimensionValue` ServiceException — not a rendered (red) tile.
#[tokio::test]
async fn getmap_unknown_reference_time_returns_400() {
    let (app, calls) = build_forecast_router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "{FC_GETMAP_URI}&DIM_REFERENCE_TIME=2000-01-01T00:00:00Z"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let xml = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        xml.contains("InvalidDimensionValue"),
        "unknown run must yield InvalidDimensionValue; got:\n{xml}"
    );
    // The engine must not have been asked to render an invalid run.
    assert!(
        calls.lock().unwrap().is_empty(),
        "engine must not be called for an invalid reference_time"
    );
}

/// An unparseable `DIM_REFERENCE_TIME` is also a 400 `InvalidDimensionValue`.
#[tokio::test]
async fn getmap_unparseable_reference_time_returns_400() {
    let (app, _calls) = build_forecast_router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("{FC_GETMAP_URI}&DIM_REFERENCE_TIME=not-a-time"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// `DIM_REFERENCE_TIME` against a non-forecast layer (no advertised runs) is a
/// 400 `InvalidDimensionValue` — the dimension doesn't exist for that layer.
#[tokio::test]
async fn getmap_reference_time_against_non_forecast_layer_returns_400() {
    let app = build_populated_router(); // "radar" has empty reference_times
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "{GETMAP_URI}&DIM_REFERENCE_TIME=2026-06-07T00:00:00Z"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let xml = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        xml.contains("InvalidDimensionValue"),
        "reference_time on a non-forecast layer must be InvalidDimensionValue; got:\n{xml}"
    );
}

// ---------------------------------------------------------------------------
// TIME-less GetMap must track the engine's latest timestamp
// ---------------------------------------------------------------------------

/// Mock engine whose advertised `times` can be advanced mid-test, recording
/// the `time` each render was asked for. Regression fixture for the
/// stale-latest bug: the rendered/meta-tile caches have no TTL, so a TIME-less
/// request keyed as `time: None` would serve the first rendered frame forever.
struct AdvancingMockMapEngine {
    times: Arc<std::sync::Mutex<Vec<chrono::DateTime<chrono::Utc>>>>,
    calls: Arc<std::sync::Mutex<Vec<Option<chrono::DateTime<chrono::Utc>>>>>,
}

impl MapEngine for AdvancingMockMapEngine {
    fn get_raster_tile(
        &self,
        _bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<chrono::DateTime<chrono::Utc>>,
        _output_crs: &OutputCrs,
        _parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        self.calls.lock().unwrap().push(time);
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
        RasterInfo {
            native_crs: "EPSG:4326".into(),
            spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
            times: self.times.lock().unwrap().clone(),
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

type AdvancingFixture = (
    axum::Router,
    Arc<std::sync::Mutex<Vec<chrono::DateTime<chrono::Utc>>>>,
    Arc<std::sync::Mutex<Vec<Option<chrono::DateTime<chrono::Utc>>>>>,
);

fn build_advancing_router() -> AdvancingFixture {
    let t1 = chrono::DateTime::parse_from_rfc3339("2026-06-10T10:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let times = Arc::new(std::sync::Mutex::new(vec![t1]));
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let engine: Arc<dyn MapEngine> = Arc::new(AdvancingMockMapEngine {
        times: times.clone(),
        calls: calls.clone(),
    });
    let mut engines = HashMap::new();
    let mut collections = HashMap::new();
    let mut styles_map = HashMap::new();

    engines.insert("radar-live".to_string(), engine);
    collections.insert(
        "radar-live".to_string(),
        CollectionConfig {
            id: "radar-live".to_string(),
            title: "Live Radar".to_string(),
            description: "Fixture for the TIME-less stale-latest regression".into(),
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
    styles_map.insert("radar-live".to_string(), layer_styles);

    let state = Arc::new(ArcSwap::from_pointee(WmsState {
        engines,
        collections,
        styles: styles_map,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        tile_cache: Arc::new(ds_render::TilePixelCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
    }));
    (api_wms::router(state), times, calls)
}

const LIVE_GETMAP_URI: &str = "/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS=radar-live\
                               &STYLES=&CRS=CRS:84&BBOX=10,55,30,70&WIDTH=64&HEIGHT=64\
                               &FORMAT=image/png";

/// A TIME-less GetMap is keyed on the engine's *current latest* timestamp, not
/// `time: None` — so when a newer volume arrives, the next TIME-less request
/// re-renders instead of serving the previous frame from the TTL-less cache.
#[tokio::test]
async fn timeless_getmap_tracks_new_latest_data() {
    let (app, times, calls) = build_advancing_router();

    // First TIME-less request renders the current latest (t1).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(LIVE_GETMAP_URI)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["x-cache"], "MISS");
    let t1 = times.lock().unwrap()[0];
    assert_eq!(calls.lock().unwrap().clone(), vec![Some(t1)]);

    // Same request again: cache HIT, no new engine call.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(LIVE_GETMAP_URI)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["x-cache"], "HIT");
    assert_eq!(calls.lock().unwrap().len(), 1);

    // A newer timestep arrives (poll cycle): the next TIME-less request must
    // re-render at the new latest, not serve the stale cached frame.
    let t2 = chrono::DateTime::parse_from_rfc3339("2026-06-10T10:05:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    times.lock().unwrap().push(t2);

    let resp = app
        .oneshot(
            Request::builder()
                .uri(LIVE_GETMAP_URI)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()["x-cache"],
        "MISS",
        "a TIME-less request after new data must re-render, not serve the stale frame"
    );
    assert_eq!(calls.lock().unwrap().last().copied(), Some(Some(t2)));
}

/// Parse a PNG's IHDR `(width, height)`. The signature is 8 bytes, then the
/// IHDR chunk: 4-byte length, the `IHDR` tag, then width/height as big-endian
/// u32s. Lets the legend tests assert dimensions without a PNG decoder dep.
fn png_dims(bytes: &[u8]) -> (u32, u32) {
    assert!(
        bytes.starts_with(&[0x89, b'P', b'N', b'G']),
        "not a PNG: {:?}",
        &bytes[..4.min(bytes.len())]
    );
    let w = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let h = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    (w, h)
}

/// `GetLegendGraphic` (#371): with no WIDTH/HEIGHT the handler returns the new
/// labelled default-size legend (180×300), as an immutable-cacheable PNG. The
/// title/unit resolution from `raster_info()` runs end-to-end (a panic there
/// would fail this test).
#[tokio::test]
async fn legend_graphic_defaults_to_labelled_size() {
    let app = build_empty_router();
    let req = Request::builder()
        .uri(
            "/?SERVICE=WMS&REQUEST=GetLegendGraphic&VERSION=1.3.0\
             &LAYER=empty&FORMAT=image/png",
        )
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let headers = resp.headers().clone();
    assert_eq!(headers.get("content-type").unwrap(), "image/png");
    assert_eq!(
        headers.get("cache-control").unwrap(),
        "public, max-age=86400, immutable"
    );
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        png_dims(&body),
        (180, 300),
        "legend should default to the labelled 180×300 size"
    );
}

/// `GetLegendGraphic` reflects the selected `STYLES`: the named style's distinct
/// colormap/range (and its name on the legend) make its legend bytes differ from
/// the default style's. Confirms the legend isn't pinned to the default colormap.
#[tokio::test]
async fn legend_graphic_reflects_selected_style() {
    let app = build_empty_router();
    let fetch = |styles: &str| {
        let uri = format!(
            "/?SERVICE=WMS&REQUEST=GetLegendGraphic&VERSION=1.3.0\
             &LAYER=empty&FORMAT=image/png&STYLES={styles}"
        );
        let app = app.clone();
        async move {
            let resp = app
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            resp.into_body().collect().await.unwrap().to_bytes()
        }
    };
    let default = fetch("default").await;
    let named = fetch("radar_fmi").await;
    assert!(default.starts_with(&[0x89, b'P', b'N', b'G']));
    assert!(named.starts_with(&[0x89, b'P', b'N', b'G']));
    assert_ne!(
        default, named,
        "the selected style must change the rendered legend"
    );
}

/// `GetLegendGraphic` honours the singular `STYLE` alias, not just plural
/// `STYLES` (#165). A client sending only `STYLE=radar_fmi` must get that
/// style's legend, identical to `STYLES=radar_fmi` — not the default.
#[tokio::test]
async fn legend_graphic_accepts_singular_style_alias() {
    let app = build_empty_router();
    let fetch = |query: &str| {
        let uri = format!(
            "/?SERVICE=WMS&REQUEST=GetLegendGraphic&VERSION=1.3.0\
             &LAYER=empty&FORMAT=image/png&{query}"
        );
        let app = app.clone();
        async move {
            let resp = app
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            resp.into_body().collect().await.unwrap().to_bytes()
        }
    };
    let singular = fetch("STYLE=radar_fmi").await;
    let plural = fetch("STYLES=radar_fmi").await;
    let default = fetch("STYLES=default").await;
    assert_eq!(
        singular, plural,
        "STYLE=… must resolve the same style as STYLES=…"
    );
    assert_ne!(
        singular, default,
        "STYLE=radar_fmi must not fall through to the default legend"
    );
}

/// A client may still request an explicit (smaller) legend size; the handler
/// honours it and the renderer degrades to a bare swatch when too narrow for
/// labels. Asserts the requested dimensions round-trip.
#[tokio::test]
async fn legend_graphic_honours_explicit_size() {
    let app = build_empty_router();
    let req = Request::builder()
        .uri(
            "/?SERVICE=WMS&REQUEST=GetLegendGraphic&VERSION=1.3.0\
             &LAYER=empty&FORMAT=image/png&WIDTH=20&HEIGHT=120",
        )
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(png_dims(&body), (20, 120));
}
