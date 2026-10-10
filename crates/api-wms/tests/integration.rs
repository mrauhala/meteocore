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
            values: vec![None; pixel_count].into(),
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
            nowcast: None,
            bufr: None,
            satellite: None,
            preview: None,
            derive_wind: None,
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
            palette: ds_render::builtin_palette_arc("viridis").unwrap(),
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
            palette: ds_render::builtin_palette_arc("radar_dbz").unwrap(),
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
            nowcast: None,
            bufr: None,
            satellite: None,
            preview: None,
            derive_wind: None,
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
            palette: ds_render::builtin_palette_arc("viridis").unwrap(),
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

/// A failed render is transient: its error tile under an explicit TIME must
/// be revalidated, not pinned `immutable` in browsers for a day.
#[tokio::test]
async fn error_tile_is_never_immutable() {
    let app = build_failing_router();
    let req = Request::builder()
        .uri(
            "/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS=broken\
             &CRS=CRS:84&BBOX=10,55,30,70&WIDTH=64&HEIGHT=64\
             &FORMAT=image/png&TIME=2024-01-01T00:00:00Z",
        )
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["x-cache"], "ERROR");
    let cc = resp.headers()["cache-control"].to_str().unwrap();
    assert!(
        cc.contains("must-revalidate") && !cc.contains("immutable"),
        "{cc}"
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
            values: values.into(),
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
    build_populated_router_with_engine(Arc::new(PopulatedMockMapEngine))
}

fn build_populated_router_with_engine(engine: Arc<dyn MapEngine>) -> axum::Router {
    api_wms::router(build_populated_state(engine))
}

fn build_populated_state(engine: Arc<dyn MapEngine>) -> Arc<ArcSwap<WmsState>> {
    build_populated_state_with_wms(engine, None)
}

/// [`build_populated_state`] with the collection's `[wms]` config, for the
/// per-collection settings the handler reads (e.g. `webp_quality`).
fn build_populated_state_with_wms(
    engine: Arc<dyn MapEngine>,
    wms: Option<ds_core::config::WmsConfig>,
) -> Arc<ArcSwap<WmsState>> {
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
            wms,
            grib: None,
            zarr: None,
            odim: None,
            cap: None,
            postgis: None,
            nowcast: None,
            bufr: None,
            satellite: None,
            preview: None,
            derive_wind: None,
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
            palette: ds_render::builtin_palette_arc("viridis").unwrap(),
            colormap: cmap,
            min: 0.0,
            max: 1.0,
            parameter: None,
        },
    );
    styles_map.insert("radar".to_string(), layer_styles);

    Arc::new(ArcSwap::from_pointee(WmsState {
        engines,
        collections,
        styles: styles_map,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        tile_cache: Arc::new(ds_render::TilePixelCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
    }))
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
    /// `MapEngine::content_version` — bumped by a test to simulate content
    /// revised in place under the same TIME (a push-fed alert set).
    content_version: Arc<std::sync::atomic::AtomicU64>,
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
            values: vec![Some(0.5); (width * height) as usize].into(),
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

    fn content_version(&self) -> u64 {
        self.content_version
            .load(std::sync::atomic::Ordering::Relaxed)
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
    let (router, tile_cache, calls, _) = build_counting_router_versioned(metatile_mb);
    (router, tile_cache, calls)
}

/// [`build_counting_router`] also handing out the engine's content-version
/// knob.
fn build_counting_router_versioned(
    metatile_mb: u64,
) -> (
    axum::Router,
    Arc<ds_render::TilePixelCache>,
    Arc<std::sync::atomic::AtomicUsize>,
    Arc<std::sync::atomic::AtomicU64>,
) {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let content_version = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let engine: Arc<dyn MapEngine> = Arc::new(CountingMockMapEngine {
        calls: calls.clone(),
        content_version: content_version.clone(),
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
            nowcast: None,
            bufr: None,
            satellite: None,
            preview: None,
            derive_wind: None,
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
            palette: ds_render::builtin_palette_arc("viridis").unwrap(),
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
    (api_wms::router(state), tile_cache, calls, content_version)
}

/// Content revised in place under the same TIME (a push-fed alert set: a
/// warning published later is active at instants already rendered) must
/// not be served from the no-TTL caches — on BOTH render paths. The
/// engine's `content_version` is part of the rendered key and the
/// meta-tile key; an unchanged version keeps hitting.
#[tokio::test]
async fn content_version_change_invalidates_rendered_and_metatile_caches() {
    use std::sync::atomic::Ordering;
    for (crs, bbox, metatile_mb) in [
        ("CRS:84", "10,55,30,70", 64), // direct path (rendered cache)
        ("EPSG:3857", "1113194,7361866,3339584,11068715", 64), // meta-tiled
        ("EPSG:3067", "100000,6500000,612000,7012000", 64),
        ("EPSG:3035", "4000000,3000000,4512000,3512000", 64),
    ] {
        let (app, _tiles, calls, version) = build_counting_router_versioned(metatile_mb);
        let uri = format!(
            "/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS=data&STYLES=\
             &FORMAT=image/png&CRS={crs}&BBOX={bbox}&WIDTH=64&HEIGHT=64\
             &TIME=2024-01-01T00:00:00Z"
        );
        // Returns (x-cache, cache-control).
        let get = |app: axum::Router, uri: String| async move {
            let resp = app
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let h = |name: &str| {
                resp.headers()
                    .get(name)
                    .map(|v| v.to_str().unwrap().to_string())
                    .unwrap_or_default()
            };
            (h("x-cache"), h("cache-control"))
        };
        // content_version 0 = immutable timesteps: explicit TIME may be
        // cached downstream without revalidation.
        let (x, cc) = get(app.clone(), uri.clone()).await;
        assert_eq!(x, "MISS");
        assert!(cc.contains("immutable"), "{crs}: {cc}");
        let after_first = calls.load(Ordering::Relaxed);
        assert!(after_first > 0);
        // Same content: served from cache, engine not called.
        assert_eq!(get(app.clone(), uri.clone()).await.0, "HIT");
        assert_eq!(
            calls.load(Ordering::Relaxed),
            after_first,
            "{crs}: cache must hit"
        );
        // Revised content under the same TIME: re-rendered, and downstream
        // caches are told to revalidate rather than keep the old tile 24 h.
        version.fetch_add(1, Ordering::Relaxed);
        let (x, cc) = get(app.clone(), uri.clone()).await;
        assert_eq!(x, "MISS");
        assert!(
            cc.contains("must-revalidate") && !cc.contains("immutable"),
            "{crs}: {cc}"
        );
        assert!(
            calls.load(Ordering::Relaxed) > after_first,
            "{crs}: a new content_version must reach the engine"
        );
        // …and the revised render is cached under the new version.
        let after_revised = calls.load(Ordering::Relaxed);
        assert_eq!(get(app, uri).await.0, "HIT");
        assert_eq!(calls.load(Ordering::Relaxed), after_revised);
    }
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

// --- #507: requested-time vs resolved-timestep cache poisoning --------------

/// Engine mimicking the geotiff latest-not-after time selection over a
/// swappable timestep catalog, rendering a DIFFERENT constant value per
/// timestep. `resolve_time` mirrors `get_raster_tile`'s selection — the #507
/// contract: caches key on what actually renders, not on what was asked for.
struct SnappingMockMapEngine {
    /// Available timesteps, ascending. Pushing a new entry simulates a file
    /// finishing ingestion between requests.
    catalog: Arc<std::sync::RwLock<Vec<chrono::DateTime<chrono::Utc>>>>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    /// Satellite-style: an empty catalog renders an all-nodata tile instead
    /// of failing, so `resolve_time` must then be `None`, not the request.
    empty_renders_nothing: bool,
}

impl SnappingMockMapEngine {
    fn select(
        &self,
        time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        let cat = self.catalog.read().unwrap();
        match time {
            Some(t) => cat
                .iter()
                .rev()
                .find(|&&ts| ts <= t)
                .copied()
                .or_else(|| cat.first().copied()),
            None => cat.last().copied(),
        }
    }
}

impl MapEngine for SnappingMockMapEngine {
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
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Some(resolved) = self.select(time) else {
            if self.empty_renders_nothing {
                return Ok(RasterTile {
                    width,
                    height,
                    values: vec![None; (width * height) as usize].into(),
                });
            }
            return Err(DataServerError::Engine("no data".into()));
        };
        let cat = self.catalog.read().unwrap();
        let idx = cat.iter().position(|&ts| ts == resolved).unwrap_or(0);
        // Distinct pixel value per timestep so stale tiles are detectable.
        let v = 0.25 + 0.5 * (idx.min(1) as f64);
        Ok(RasterTile {
            width,
            height,
            values: vec![Some(v); (width * height) as usize].into(),
        })
    }

    fn raster_info(&self) -> RasterInfo {
        RasterInfo {
            native_crs: "EPSG:3857".into(),
            spatial_extent: Some([-20.0, 30.0, 40.0, 80.0]),
            times: self.catalog.read().unwrap().clone(),
            parameter: "reflectivity".into(),
            unit: "dBZ".into(),
            parameters: vec![],
            vertical: None,
            grid_size: None,
            layer_subtitle: None,
            reference_times: Vec::new(),
        }
    }

    fn resolve_time(
        &self,
        time: Option<chrono::DateTime<chrono::Utc>>,
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        if self.empty_renders_nothing {
            self.select(time)
        } else {
            self.select(time).or(time)
        }
    }
}

type SnappingRouter = (
    axum::Router,
    Arc<std::sync::RwLock<Vec<chrono::DateTime<chrono::Utc>>>>,
    Arc<std::sync::atomic::AtomicUsize>,
);

fn build_snapping_router(initial_times: &[&str]) -> SnappingRouter {
    build_snapping_router_with(initial_times, false)
}

fn build_snapping_router_with(
    initial_times: &[&str],
    empty_renders_nothing: bool,
) -> SnappingRouter {
    let catalog = Arc::new(std::sync::RwLock::new(
        initial_times
            .iter()
            .map(|s| s.parse().unwrap())
            .collect::<Vec<chrono::DateTime<chrono::Utc>>>(),
    ));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let engine: Arc<dyn MapEngine> = Arc::new(SnappingMockMapEngine {
        catalog: catalog.clone(),
        calls: calls.clone(),
        empty_renders_nothing,
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
            description: "Time-snapping fixture for the #507 poison regression".to_string(),
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
            bufr: None,
            satellite: None,
            preview: None,
            derive_wind: None,
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
            palette: ds_render::builtin_palette_arc("viridis").unwrap(),
            colormap: cmap,
            min: 0.0,
            max: 1.0,
            parameter: None,
        },
    );
    styles_map.insert("data".to_string(), layer_styles);

    let tile_cache = Arc::new(ds_render::TilePixelCache::new(64));
    let state = Arc::new(ArcSwap::from_pointee(WmsState {
        engines,
        collections,
        styles: styles_map,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        tile_cache,
        base_url: String::new(),
        trust_proxy_headers: false,
    }));
    (api_wms::router(state), catalog, calls)
}

async fn get_map_time(app: &axum::Router, bbox: &str, time: &str) -> StatusCode {
    let uri = format!(
        "/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS=data&STYLES=\
         &FORMAT=image/png&CRS=EPSG:3857&BBOX={bbox}&WIDTH=512&HEIGHT=512&TIME={time}"
    );
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    resp.status()
}

/// The #507 incident replay: a `TIME=T` request served while T's file is not
/// yet ingested must cache its (T−1) pixels under **T−1's** key — so that
/// (a) an explicit T−1 request shares those entries, and (b) once T lands,
/// the same `TIME=T` request re-renders fresh pixels instead of serving the
/// pre-ingest tiles (the partial one-step-behind animation regions seen in
/// production under storm load).
#[tokio::test]
async fn requested_time_ahead_of_catalog_does_not_poison_the_frame() {
    const T5: &str = "2026-07-11T20:15:00Z"; // ingested
    const T: &str = "2026-07-11T20:20:00Z"; // requested before ingestion
    const BBOX: &str = "2000000,8000000,3000000,9000000";
    let (app, catalog, calls) = build_snapping_router(&[T5]);

    // 1. Too-early request for frame T: engine snaps to T−5 and renders it.
    assert_eq!(get_map_time(&app, BBOX, T).await, StatusCode::OK);
    let calls_cold = calls.load(std::sync::atomic::Ordering::Relaxed);
    assert!(calls_cold > 0, "cold request renders tiles");

    // 2. An explicit T−5 request for the same viewport must be a pure cache
    //    hit — the too-early T output was keyed under its RESOLVED timestep,
    //    so the two frames legitimately share every entry.
    assert_eq!(get_map_time(&app, BBOX, T5).await, StatusCode::OK);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        calls_cold,
        "explicit T−5 must reuse the tiles the too-early T request cached under T−5's key"
    );

    // 3. T's file finishes ingestion.
    catalog.write().unwrap().push(T.parse().unwrap());

    // 4. The same TIME=T request must now re-render every tile fresh — the
    //    pre-fix behaviour served the stale T−5 tiles as cache hits here.
    assert_eq!(get_map_time(&app, BBOX, T).await, StatusCode::OK);
    let calls_after = calls.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        calls_after - calls_cold,
        calls_cold,
        "post-ingest TIME=T must miss every cached T−5 tile and render fresh (got {} fresh renders, expected {})",
        calls_after - calls_cold,
        calls_cold
    );
}

/// A satellite-style engine before its first scan (a start or reload still
/// backfilling) renders nothing and resolves every TIME to `None`. A
/// `TIME=T` request then must neither pin its empty image in browsers for a
/// day nor leave empty meta-tiles under T's key: once T lands, the same
/// request renders T. On nexus the tutka loop asked for the next scan during
/// a reload's backfill and the frame stayed blank after it arrived.
#[tokio::test]
async fn time_requested_before_the_first_timestep_is_not_pinned() {
    const T: &str = "2026-09-30T04:30:00Z";
    const BBOX: &str = "2000000,8000000,3000000,9000000"; // meta-tiled
    let (app, catalog, calls) = build_snapping_router_with(&[], true);
    let uri = format!(
        "/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS=data&STYLES=\
         &FORMAT=image/png&CRS=EPSG:3857&BBOX={BBOX}&WIDTH=512&HEIGHT=512&TIME={T}"
    );
    // Returns (x-cache, cache-control).
    let get = || async {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let h = |name: &str| resp.headers()[name].to_str().unwrap().to_string();
        (h("x-cache"), h("cache-control"))
    };

    let (x, cc) = get().await;
    assert_eq!(x, "EMPTY");
    assert!(
        cc.contains("must-revalidate") && !cc.contains("immutable"),
        "nothing rendered for T yet: {cc}"
    );
    let calls_empty = calls.load(std::sync::atomic::Ordering::Relaxed);
    assert!(calls_empty > 0);

    catalog.write().unwrap().push(T.parse().unwrap());

    let (x, cc) = get().await;
    assert_eq!(x, "MISS", "T landed: its pixels, not the pre-scan empties");
    assert!(cc.contains("immutable"), "{cc}");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed) - calls_empty,
        calls_empty,
        "every meta-tile renders fresh once T exists"
    );
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

/// Projected panning reuses tiles while the zero-cache switch keeps the
/// original one-call direct path available for both supported projections.
#[tokio::test]
async fn projected_pans_reuse_tiles_and_honour_the_kill_switch() {
    use std::sync::atomic::Ordering;
    for (crs, x, y) in [
        ("EPSG:3067", 100000, 6500000),
        ("EPSG:3035", 4000000, 3000000),
    ] {
        for budget in [0, 64] {
            let (app, cache, calls) = build_counting_router(budget);
            let mut first_calls = 0;
            for pan in [0, 32000] {
                let bbox = format!("{},{},{},{}", x + pan, y, x + pan + 256000, y + 256000);
                assert_eq!(get_map(&app, crs, &bbox, 512, 512).await, StatusCode::OK);
                if pan == 0 {
                    first_calls = calls.load(Ordering::Relaxed);
                }
            }
            if budget == 0 {
                assert_eq!(calls.load(Ordering::Relaxed), 2);
                assert_eq!(cache.stats(), (0, 0));
            } else {
                assert!(cache.stats().0 > 0, "{crs} must reuse tiles");
                assert!(calls.load(Ordering::Relaxed) - first_calls < first_calls);
            }
        }
    }
}

/// Geographic requests (here CRS:84) bypass meta-tiling and render
/// directly: the meta-tile cache is never touched.
#[tokio::test]
async fn geographic_output_bypasses_meta_tiling() {
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

// --- Render latency by collection and outcome (#466) -------------------------

/// GetMap on `layers`, returning the render timing the server's metrics
/// middleware records as `render_duration_seconds`.
async fn render_timing(
    app: &axum::Router,
    layers: &str,
    crs: &str,
    bbox: &str,
    size: u32,
) -> Option<(String, ds_executor::RenderOutcome)> {
    let uri = format!(
        "/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS={layers}&STYLES=\
         &FORMAT=image/png&CRS={crs}&BBOX={bbox}&WIDTH={size}&HEIGHT={size}"
    );
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    resp.extensions()
        .get::<ds_executor::RenderTiming>()
        .map(|t| (t.collection.clone(), t.outcome))
}

/// One `/wms` histogram hid which collection owned the tail and buried cold
/// renders under cache hits. Each GetMap now reports its collection's
/// registry id and the path that served it: a cold engine read, a
/// rendered-cache hit, or a view assembled from cached meta-tiles only.
#[tokio::test]
async fn getmap_reports_render_outcome_per_collection() {
    use ds_executor::RenderOutcome::{Assembled, Cold, Hit};
    use std::sync::atomic::Ordering;
    let (app, _tiles, calls) = build_counting_router(64);
    let data = |outcome| Some(("data".to_string(), outcome));
    let full = "2000000,8000000,3000000,9000000";

    assert_eq!(
        render_timing(&app, "data", "EPSG:3857", full, 512).await,
        data(Cold)
    );
    assert_eq!(
        render_timing(&app, "data", "EPSG:3857", full, 512).await,
        data(Hit)
    );
    // A sub-view at the same resolution: a rendered-cache miss whose
    // covering meta-tiles are all cached, so the engine is not called.
    let before = calls.load(Ordering::Relaxed);
    let quarter = "2000000,8000000,2500000,8500000";
    assert_eq!(
        render_timing(&app, "data", "EPSG:3857", quarter, 256).await,
        data(Assembled)
    );
    assert_eq!(calls.load(Ordering::Relaxed), before);
    // Direct geographic render; a `collection/parameter` layer still labels
    // the collection, never the raw layer name.
    assert_eq!(
        render_timing(&app, "data/any", "CRS:84", "10,55,30,70", 256).await,
        data(Cold)
    );
}

/// A failed render served as an error tile is not a render outcome: it must
/// not dilute the cold-render latency.
#[tokio::test]
async fn error_tile_reports_no_render_timing() {
    let app = build_failing_router();
    let timing = render_timing(&app, "broken", "CRS:84", "10,55,30,70", 64).await;
    assert_eq!(timing, None);
}

/// The phases a served GetMap reports for `render_phase_seconds` (#147).
async fn render_phases(app: &axum::Router, crs: &str, bbox: &str, size: u32) -> Vec<&'static str> {
    let uri = format!(
        "/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS=data&STYLES=\
         &FORMAT=image/png&CRS={crs}&BBOX={bbox}&WIDTH={size}&HEIGHT={size}"
    );
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let timing = resp
        .extensions()
        .get::<ds_executor::RenderTiming>()
        .unwrap();
    timing
        .phases
        .iter()
        .map(|(phase, _)| phase.as_str())
        .collect()
}

/// Each path reports the phases it ran: a cold meta-tiled view all four, a
/// hit none, a view from cached meta-tiles no engine read, and a direct
/// render no assembly.
#[tokio::test]
async fn getmap_reports_render_phases_per_path() {
    let (app, _tiles, _calls) = build_counting_router(64);
    let full = "2000000,8000000,3000000,9000000";
    assert_eq!(
        render_phases(&app, "EPSG:3857", full, 512).await,
        ["queue", "engine", "assemble", "encode"]
    );
    assert!(render_phases(&app, "EPSG:3857", full, 512).await.is_empty());
    let quarter = "2000000,8000000,2500000,8500000";
    assert_eq!(
        render_phases(&app, "EPSG:3857", quarter, 256).await,
        ["queue", "assemble", "encode"]
    );
    assert_eq!(
        render_phases(&app, "CRS:84", "10,55,30,70", 256).await,
        ["queue", "engine", "encode"]
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
            values: vec![None; pixel_count].into(),
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
                ds_core::map_engine::ParameterInfo {
                    name: "DBZH".into(),
                    title: "DBZH — Reflectivity (horizontal)".into(),
                    unit: "dBZ".into(),
                },
                ds_core::map_engine::ParameterInfo {
                    name: "VRADH".into(),
                    title: "VRADH — Radial velocity (horizontal)".into(),
                    unit: "m/s".into(),
                },
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
        nowcast: None,
        bufr: None,
        satellite: None,
        preview: None,
        derive_wind: None,
    }
}

/// Mock engine advertising a fixed spatial extent, for the capabilities bbox.
struct ExtentMockMapEngine([f64; 4]);

impl MapEngine for ExtentMockMapEngine {
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
        RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: Some(self.0),
            times: Vec::new(),
            parameter: "t2m".into(),
            unit: "K".into(),
            parameters: vec![],
            vertical: None,
            grid_size: None,
            layer_subtitle: None,
            reference_times: Vec::new(),
        }
    }
}

/// The WMS 1.3.0 schema bounds `EX_GeographicBoundingBox` to ±180°/±90°. A
/// global 0.25° grid's cell edges reach past both and become the whole globe;
/// an extent that is still the empty-accumulator sentinel emits no box at all
/// rather than ±1.8e308. An antimeridian crossing keeps west > east in the
/// ISO box only.
#[test]
fn capabilities_bbox_stays_in_the_crs84_domain() {
    let capabilities = |extent: [f64; 4]| {
        let mut engines: HashMap<String, Arc<dyn MapEngine>> = HashMap::new();
        engines.insert("grid".to_string(), Arc::new(ExtentMockMapEngine(extent)));
        let collections = HashMap::from([("grid".to_string(), site_collection_config("grid"))]);
        let styles: HashMap<String, HashMap<String, StyleInfo>> = HashMap::new();
        let xml = api_wms::capabilities::get_capabilities_xml(&engines, &collections, &styles, "");
        String::from_utf8(xml).expect("capabilities XML is UTF-8")
    };

    let xml = capabilities([-180.125, -90.125, 179.875, 90.125]);
    for element in [
        "<westBoundLongitude>-180.000000</westBoundLongitude>",
        "<eastBoundLongitude>180.000000</eastBoundLongitude>",
        "<southBoundLatitude>-90.000000</southBoundLatitude>",
        "<northBoundLatitude>90.000000</northBoundLatitude>",
    ] {
        assert!(xml.contains(element), "missing {element}; got:\n{xml}");
    }
    assert!(
        xml.contains(
            r#"<BoundingBox CRS="CRS:84" minx="-180.000000" miny="-90.000000" maxx="180.000000" maxy="90.000000"/>"#
        ),
        "CRS:84 BoundingBox must match; got:\n{xml}"
    );

    // Across the antimeridian (GOES-West): the ISO box keeps west > east,
    // the min/max envelope spans every longitude.
    let xml = capabilities([173.5, 11.0, -174.5, 16.0]);
    for element in [
        "<westBoundLongitude>173.500000</westBoundLongitude>",
        "<eastBoundLongitude>-174.500000</eastBoundLongitude>",
    ] {
        assert!(xml.contains(element), "missing {element}; got:\n{xml}");
    }
    assert!(
        xml.contains(
            r#"<BoundingBox CRS="CRS:84" minx="-180.000000" miny="11.000000" maxx="180.000000" maxy="16.000000"/>"#
        ),
        "a crossing extent's CRS:84 BoundingBox spans every longitude; got:\n{xml}"
    );

    let xml = capabilities([f64::MAX, f64::MAX, f64::MIN, f64::MIN]);
    assert!(
        !xml.contains("EX_GeographicBoundingBox") && !xml.contains("<BoundingBox"),
        "a sentinel extent must emit no bounding box; got:\n{xml}"
    );
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
        media_type: None,
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
                media_type: None,
            }),
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            zarr: None,
            odim: None,
            cap: None,
            postgis: None,
            nowcast: None,
            bufr: None,
            satellite: None,
            preview: None,
            derive_wind: None,
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
// `<Service>` request limits (#1012)
// ---------------------------------------------------------------------------

/// The children of the capabilities' `<Service>` element in document order:
/// each element's name and its text, or for `<OnlineResource>` its
/// `xlink:href`.
fn service_children(xml: &str) -> Vec<(String, String)> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_str(xml);
    // Depth below `<Service>`: `Some(0)` = directly inside it.
    let mut depth: Option<usize> = None;
    let mut children: Vec<(String, String)> = Vec::new();
    loop {
        match reader.read_event().expect("capabilities XML parses") {
            Event::Start(e) => {
                let name = e.name().as_ref().to_owned();
                match depth {
                    None if name == "Service" => depth = Some(0),
                    None => {}
                    Some(d) => {
                        if d == 0 {
                            children.push((name, String::new()));
                        }
                        depth = Some(d + 1);
                    }
                }
            }
            Event::Empty(e) if depth == Some(0) => {
                let name = e.name().as_ref().to_owned();
                let href = e
                    .try_get_attribute("xlink:href")
                    .expect("attributes parse")
                    .map(|a| a.value.into_owned())
                    .unwrap_or_default();
                children.push((name, href));
            }
            Event::Text(t) if depth == Some(1) => {
                children.last_mut().unwrap().1.push_str(&t);
            }
            Event::End(_) => match depth {
                Some(0) => break,
                Some(d) => depth = Some(d - 1),
                None => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    children
}

/// The value of the `<Service>` child `name`, parsed as an integer.
fn service_limit(children: &[(String, String)], name: &str) -> usize {
    let (_, value) = children
        .iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("<Service> has no <{name}>; got {children:?}"));
    value
        .parse()
        .unwrap_or_else(|_| panic!("<{name}> {value:?} is not an integer"))
}

/// One WMS request through `app`: its status and body.
async fn wms_get(app: &axum::Router, query: &str) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/?SERVICE=WMS&VERSION=1.3.0&{query}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// `<Service>` advertises the limits GetMap enforces — `LayerLimit`,
/// `MaxWidth`, `MaxHeight` — and its mandatory `OnlineResource`, in the
/// element order of the WMS 1.3.0 schema's `Service` sequence (Name, Title,
/// Abstract, KeywordList, OnlineResource, ContactInformation, Fees,
/// AccessConstraints, LayerLimit, MaxWidth, MaxHeight).
#[test]
fn capabilities_service_advertises_getmap_limits_in_schema_order() {
    let mut engines: HashMap<String, Arc<dyn MapEngine>> = HashMap::new();
    engines.insert("radar".to_string(), Arc::new(PopulatedMockMapEngine));
    let mut collections = HashMap::new();
    collections.insert("radar".to_string(), site_collection_config("radar"));
    let styles: HashMap<String, HashMap<String, StyleInfo>> = HashMap::new();
    let xml = api_wms::capabilities::get_capabilities_xml(
        &engines,
        &collections,
        &styles,
        "https://wms.example",
    );
    let xml = String::from_utf8(xml).expect("capabilities XML is UTF-8");

    let children = service_children(&xml);
    let names: Vec<&str> = children.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "Name",
            "Title",
            "Abstract",
            "OnlineResource",
            "LayerLimit",
            "MaxWidth",
            "MaxHeight"
        ],
        "<Service> children out of WMS 1.3.0 schema order; got:\n{xml}"
    );
    assert_eq!(children[0].1, "WMS");
    assert_eq!(children[3].1, "https://wms.example/wms");
    assert_eq!(service_limit(&children, "LayerLimit"), 1);
    assert_eq!(
        service_limit(&children, "LayerLimit"),
        api_wms::params::LAYER_LIMIT
    );
    for side in ["MaxWidth", "MaxHeight"] {
        assert_eq!(
            service_limit(&children, side),
            api_wms::params::MAX_MAP_DIMENSION as usize,
            "<{side}>"
        );
    }
}

/// A client sizing GetMap from the advertised limits is served at them and
/// refused one past them: the capabilities and the GetMap validation read
/// the same constants.
#[tokio::test]
async fn getmap_is_served_at_the_advertised_limits_and_refused_past_them() {
    let app = build_populated_router();
    let (status, caps) = wms_get(&app, "REQUEST=GetCapabilities").await;
    assert_eq!(status, StatusCode::OK);
    let children = service_children(&caps);
    let max_width = service_limit(&children, "MaxWidth");
    let max_height = service_limit(&children, "MaxHeight");
    let layer_limit = service_limit(&children, "LayerLimit");

    let get_map = |layers: usize, width: usize, height: usize| {
        let layers = vec!["radar"; layers].join(",");
        format!(
            "REQUEST=GetMap&LAYERS={layers}&STYLES=&CRS=CRS:84&BBOX=10,55,30,70\
             &WIDTH={width}&HEIGHT={height}&FORMAT=image/png"
        )
    };
    for (what, query, expected) in [
        ("WIDTH = MaxWidth", get_map(1, max_width, 1), StatusCode::OK),
        (
            "WIDTH = MaxWidth + 1",
            get_map(1, max_width + 1, 1),
            StatusCode::BAD_REQUEST,
        ),
        (
            "HEIGHT = MaxHeight",
            get_map(1, 1, max_height),
            StatusCode::OK,
        ),
        (
            "HEIGHT = MaxHeight + 1",
            get_map(1, 1, max_height + 1),
            StatusCode::BAD_REQUEST,
        ),
        (
            "LayerLimit layers",
            get_map(layer_limit, 64, 64),
            StatusCode::OK,
        ),
        (
            "LayerLimit + 1 layers",
            get_map(layer_limit + 1, 64, 64),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let (status, body) = wms_get(&app, &query).await;
        assert_eq!(status, expected, "{what}: {query}\n{body}");
    }
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
            values: values.into(),
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

    fn resolve_reference_time(
        &self,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        // Run-retaining engine contract (#521): None ⇒ the concrete run the
        // render would use (latest here), Some ⇒ echo the pin.
        reference_time.or_else(|| forecast_runs().last().copied())
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
    (build_forecast_router_with_engine(engine), calls)
}

/// Like [`build_forecast_router`] but with a caller-supplied engine, for
/// exercising engine-specific run-resolution behaviours (e.g. the cross-run
/// fallback shape).
fn build_forecast_router_with_engine(engine: Arc<dyn MapEngine>) -> axum::Router {
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
            nowcast: None,
            bufr: None,
            satellite: None,
            preview: None,
            derive_wind: None,
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
            palette: ds_render::builtin_palette_arc("viridis").unwrap(),
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
    api_wms::router(state)
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
            nowcast: None,
            bufr: None,
            satellite: None,
            preview: None,
            derive_wind: None,
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
            nowcast: None,
            bufr: None,
            satellite: None,
            preview: None,
            derive_wind: None,
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
async fn getmap_default_reference_time_resolves_to_latest_run() {
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
        vec![Some(forecast_runs()[1])],
        "omitting DIM_REFERENCE_TIME must pin the concrete latest run (#521): \
         keying the no-TTL caches on None would freeze the first-rendered run"
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

/// The compact EDR instance-id form served before #947 (`20260607T0000Z`) is
/// also accepted as a `DIM_REFERENCE_TIME` value, resolving to the same run.
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
/// CRS:84 direct-path tests above exercise. Pin a non-latest run (so the value
/// is distinguishable from the default latest-run pin, #521) and assert every
/// tile render saw it.
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

/// Explicitly pinning the *current latest* run and omitting the dimension must
/// produce the same engine call (and therefore the same cache key): both pin
/// the concrete latest run (#521). The old behaviour normalised explicit-latest
/// to `None`, which unified the cache in the other direction — and froze the
/// first-rendered run's pixels in the no-TTL caches when a newer run landed.
#[tokio::test]
async fn getmap_explicit_latest_run_shares_key_with_default() {
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
        vec![Some(forecast_runs()[1])],
        "pinning the current latest run must produce the same concrete-run call \
         as omitting the dimension"
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
// Run-less GetMap must track the engine's latest model run (#521)
// ---------------------------------------------------------------------------

/// Mock forecast engine whose run list can be advanced mid-test. Each render
/// paints a value derived from the run it was asked for (falling back to the
/// current latest), so a stale cached tile is detectable by whether the engine
/// re-rendered at all.
struct RunSwapMockMapEngine {
    /// Ascending reference times; pushing simulates a new run (or nowcast
    /// generation) superseding the latest.
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
            native_crs: "EPSG:3857".into(),
            spatial_extent: Some([-20.0, 30.0, 40.0, 80.0]),
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
        // render would use (latest here), Some ⇒ echo the pin.
        reference_time.or_else(|| self.runs.read().unwrap().last().copied())
    }
}

type RunSwapRouter = (
    axum::Router,
    Arc<std::sync::RwLock<Vec<chrono::DateTime<chrono::Utc>>>>,
    Arc<std::sync::atomic::AtomicUsize>,
);

fn build_run_swap_router(initial_runs: &[&str]) -> RunSwapRouter {
    let runs = Arc::new(std::sync::RwLock::new(
        initial_runs
            .iter()
            .map(|s| s.parse().unwrap())
            .collect::<Vec<chrono::DateTime<chrono::Utc>>>(),
    ));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let engine: Arc<dyn MapEngine> = Arc::new(RunSwapMockMapEngine {
        runs: runs.clone(),
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
            description: "Run-swap fixture for the #521 stale-run regression".to_string(),
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
            nowcast: None,
            bufr: None,
            satellite: None,
            preview: None,
            derive_wind: None,
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
            palette: ds_render::builtin_palette_arc("viridis").unwrap(),
            colormap: cmap,
            min: 0.0,
            max: 1.0,
            parameter: None,
        },
    );
    styles_map.insert("data".to_string(), layer_styles);

    let tile_cache = Arc::new(ds_render::TilePixelCache::new(64));
    let state = Arc::new(ArcSwap::from_pointee(WmsState {
        engines,
        collections,
        styles: styles_map,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        tile_cache,
        base_url: String::new(),
        trust_proxy_headers: false,
    }));
    (api_wms::router(state), runs, calls)
}

/// The #521 stale-run replay: a request that omits `DIM_REFERENCE_TIME` must
/// key the no-TTL rendered/meta-tile caches on the CONCRETE latest run, so
/// that when a newer run supersedes it (a new nowcast generation every ~5 min,
/// a new NWP run every few hours) the same request re-renders fresh pixels.
/// Keyed as `None`, step 3 would serve the run-1 tiles as cache hits forever.
#[tokio::test]
async fn run_less_getmap_re_renders_when_a_new_run_lands() {
    const RUN1: &str = "2026-07-11T12:00:00Z";
    const RUN2: &str = "2026-07-11T18:00:00Z";
    const BBOX: &str = "2000000,8000000,3000000,9000000";
    let (app, runs, calls) = build_run_swap_router(&[RUN1]);

    // 1. Cold request (no DIM_REFERENCE_TIME): renders run 1's pixels.
    assert_eq!(
        get_map_time(&app, BBOX, "2026-07-11T20:00:00Z").await,
        StatusCode::OK
    );
    let calls_cold = calls.load(std::sync::atomic::Ordering::Relaxed);
    assert!(calls_cold > 0, "cold request renders tiles");

    // 2. Same request again: pure cache hit under the concrete run-1 key.
    assert_eq!(
        get_map_time(&app, BBOX, "2026-07-11T20:00:00Z").await,
        StatusCode::OK
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        calls_cold,
        "repeat request under the same latest run must be a pure cache hit"
    );

    // 3. A new run supersedes the latest, re-covering the same valid time
    //    with different pixels.
    runs.write().unwrap().push(RUN2.parse().unwrap());

    // 4. The same run-less request must re-render every tile fresh — the
    //    pre-#521 behaviour (key `reference_time: None`) served run 1's
    //    cached tiles here.
    assert_eq!(
        get_map_time(&app, BBOX, "2026-07-11T20:00:00Z").await,
        StatusCode::OK
    );
    let calls_after = calls.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        calls_after - calls_cold,
        calls_cold,
        "after a new run lands, the run-less request must miss every stale \
         run-1 tile and render fresh (got {} fresh renders, expected {})",
        calls_after - calls_cold,
        calls_cold
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
            values: values.into(),
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
            nowcast: None,
            bufr: None,
            satellite: None,
            preview: None,
            derive_wind: None,
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
            palette: ds_render::builtin_palette_arc("viridis").unwrap(),
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
        "public, max-age=86400" /* no immutable: palettes hot-reload */
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

/// `GetLegendGraphic&FORMAT=application/json` returns the machine-readable
/// legend instead of a picture: the style's range, interpolation mode, and one
/// entry per palette stop, so a client can draw its own legend.
#[tokio::test]
async fn legend_graphic_json_describes_the_style_palette() {
    let app = build_empty_router();
    let req = Request::builder()
        .uri(
            "/?SERVICE=WMS&REQUEST=GetLegendGraphic&VERSION=1.3.0\
             &LAYER=empty&FORMAT=application/json",
        )
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/json"
    );
    // A day is fine, but not `immutable` — palettes are hot-reloadable.
    assert_eq!(
        resp.headers().get("cache-control").unwrap(),
        "public, max-age=86400"
    );
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["style"], "default");
    assert_eq!(json["title"], "Default");
    // Resolved from the engine's raster_info(), as the legend image title is.
    assert_eq!(json["parameter"], "reflectivity");
    assert_eq!(json["unit"], "dBZ");
    assert_eq!(json["min"], 0.0);
    assert_eq!(json["max"], 1.0);
    assert_eq!(json["interpolation"], "linear");

    let stops = json["stops"].as_array().unwrap();
    let palette = ds_render::builtin_palette("viridis").unwrap();
    assert_eq!(
        stops.len(),
        palette.stops.len(),
        "every palette stop must be described"
    );
    assert_eq!(stops[0]["value"], palette.stops[0].value);
    assert_eq!(stops[0]["color"], "#440154");
}

/// The JSON legend follows `STYLES=` just like the rendered one: the named
/// style's palette, range, and title — not the default's.
#[tokio::test]
async fn legend_graphic_json_reflects_selected_style() {
    let app = build_empty_router();
    let req = Request::builder()
        .uri(
            "/?SERVICE=WMS&REQUEST=GetLegendGraphic&VERSION=1.3.0\
             &LAYER=empty&FORMAT=application/json&STYLES=radar_fmi",
        )
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["style"], "radar_fmi");
    assert_eq!(json["title"], "FMI Radar");
    assert_eq!(json["min"], -32.0);
    assert_eq!(json["max"], 95.0);
    assert_eq!(
        json["stops"].as_array().unwrap().len(),
        ds_render::builtin_palette("radar_dbz").unwrap().stops.len()
    );
    // Fully transparent stops keep their alpha channel in the hex string.
    assert_eq!(json["stops"][0]["color"], "#00000000");
}

/// `FORMAT=APPLICATION/JSON` is matched case-insensitively, and the image
/// formats are untouched: a PNG request still returns PNG bytes, and an
/// unsupported FORMAT is still a ServiceException.
#[tokio::test]
async fn legend_graphic_json_format_is_case_insensitive_and_images_unchanged() {
    let app = build_empty_router();
    let fetch = |format: &str| {
        let uri = format!(
            "/?SERVICE=WMS&REQUEST=GetLegendGraphic&VERSION=1.3.0\
             &LAYER=empty&FORMAT={format}"
        );
        let app = app.clone();
        async move {
            let resp = app
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = resp.status();
            let content_type = resp
                .headers()
                .get("content-type")
                .map(|v| v.to_str().unwrap().to_string());
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            (status, content_type, body)
        }
    };

    let (status, content_type, _) = fetch("APPLICATION/JSON").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type.as_deref(), Some("application/json"));

    let (status, content_type, body) = fetch("image/png").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type.as_deref(), Some("image/png"));
    assert_eq!(png_dims(&body), (180, 300));

    let (status, _, _) = fetch("text/html").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an unsupported FORMAT must still be rejected"
    );
}

// ---------------------------------------------------------------------------
// Explicit pin of the current latest run keeps fallback-tolerant resolution
// ---------------------------------------------------------------------------

/// Mock forecast engine simulating GRIB's cross-run fallback: an UNPINNED
/// request resolves to the OLDER run (as if the requested valid time predates
/// the newest run's reference time), while an explicit pin echoes exactly.
/// Records the run each render was asked for.
struct FallbackForecastMockMapEngine {
    calls: RunRecorder,
}

impl MapEngine for FallbackForecastMockMapEngine {
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
        RasterInfo {
            native_crs: "EPSG:4326".into(),
            spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
            times: vec!["2026-06-07T00:00:00Z".parse().unwrap()],
            parameter: "2t".into(),
            unit: "K".into(),
            parameters: vec![],
            vertical: None,
            grid_size: None,
            layer_subtitle: None,
            reference_times: forecast_runs().to_vec(),
        }
    }

    fn resolve_reference_time(
        &self,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        // GRIB-shaped: `None` falls back across runs (older run covers the
        // valid time); an explicit pin is exact.
        reference_time.or_else(|| forecast_runs().first().copied())
    }
}

/// Regression (#526 review round 5): a client echoing the GetCapabilities
/// `default=` (the current latest run) must keep the fallback-tolerant run
/// resolution an omitted dimension gets — the explicit-latest pin is
/// normalised to `None` BEFORE `resolve_reference_time`, so the engine may
/// still fall back to an older covering run instead of refusing. A pin of an
/// older run stays exact.
#[tokio::test]
async fn getmap_explicit_latest_pin_keeps_fallback_resolution() {
    let calls: RunRecorder = Arc::new(std::sync::Mutex::new(Vec::new()));
    let engine: Arc<dyn MapEngine> = Arc::new(FallbackForecastMockMapEngine {
        calls: calls.clone(),
    });
    let app = build_forecast_router_with_engine(engine);

    // Echoing the latest run (forecast_runs()[1]) must resolve like an
    // omitted dimension: the mock's fallback picks the OLDER run.
    let resp = app
        .clone()
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
    assert_eq!(
        calls.lock().unwrap().clone(),
        vec![Some(forecast_runs()[0])],
        "explicit-latest pin must keep the fallback resolution (older covering run)"
    );

    // An explicit pin of the OLDER run resolves to the same concrete run the
    // fallback picked — so it must be a pure cache hit on the entry the
    // first request created (both key `Some(older)`), not a re-render.
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
    assert_eq!(
        calls.lock().unwrap().len(),
        1,
        "older-run pin must share the cache entry keyed on the fallback-resolved run"
    );
}

// ---------------------------------------------------------------------------
// Per-parameter styles: GetLegendGraphic + GetCapabilities
// ---------------------------------------------------------------------------

/// A `StyleInfo` over a named built-in palette, resolved through the same
/// `StyleContext` the server uses so `colormap` and `palette` agree.
fn param_palette_style(name: &str, palette: &str, parameter: Option<&str>) -> StyleInfo {
    let resolved = ds_render::StyleContext::with_builtins()
        .build_colormap(&ds_render::StyleSpec {
            colormap: Some(palette),
            ..Default::default()
        })
        .expect("built-in palette resolves");
    StyleInfo {
        name: name.to_string(),
        title: format!("{palette} over {name}"),
        colormap: resolved.colormap,
        palette: resolved.palette,
        min: resolved.min,
        max: resolved.max,
        parameter: parameter.map(str::to_string),
    }
}

/// Style registry for a multi-parameter collection as the server builds it:
/// the collection-level map plus a per-parameter map for VRADH only. DBZH has
/// no layer of its own, so it must fall back to the collection map.
fn param_layer_styles() -> HashMap<String, HashMap<String, StyleInfo>> {
    let mut styles = HashMap::new();
    styles.insert(
        "radar-fivih".to_string(),
        HashMap::from([
            (
                "default".to_string(),
                param_palette_style("default", "viridis", None),
            ),
            (
                "gray".to_string(),
                param_palette_style("gray", "grayscale", Some("VRADH")),
            ),
        ]),
    );
    styles.insert(
        "radar-fivih/VRADH".to_string(),
        HashMap::from([
            (
                "default".to_string(),
                param_palette_style("default", "radial_velocity", Some("VRADH")),
            ),
            (
                "vradh_only".to_string(),
                param_palette_style("vradh_only", "temperature_classic", Some("VRADH")),
            ),
        ]),
    );
    styles
}

fn build_param_layer_router() -> axum::Router {
    let mut engines: HashMap<String, Arc<dyn MapEngine>> = HashMap::new();
    engines.insert("radar-fivih".to_string(), Arc::new(SiteMockMapEngine));
    let mut collections = HashMap::new();
    collections.insert(
        "radar-fivih".to_string(),
        site_collection_config("radar-fivih"),
    );

    let state = Arc::new(ArcSwap::from_pointee(WmsState {
        engines,
        collections,
        styles: param_layer_styles(),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        tile_cache: Arc::new(ds_render::TilePixelCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
    }));
    api_wms::router(state)
}

async fn legend_json_for(layer: &str) -> serde_json::Value {
    let app = build_param_layer_router();
    let req = Request::builder()
        .uri(format!(
            "/?SERVICE=WMS&REQUEST=GetLegendGraphic&VERSION=1.3.0\
             &LAYER={layer}&FORMAT=application/json"
        ))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "legend request for {layer}");
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

/// The gap this fixes: `LAYER=coll/param` resolves the parameter layer's own
/// style, so the legend describes the palette GetMap renders for that same
/// LAYER. Previously the "/param" segment was stripped and every parameter's
/// legend showed the collection default.
#[tokio::test]
async fn legend_graphic_uses_the_full_layer_key() {
    let json = legend_json_for("radar-fivih/VRADH").await;
    let radial = ds_render::builtin_palette("radial_velocity").unwrap();
    assert_eq!(json["min"], -48.0);
    assert_eq!(json["max"], 48.0);
    assert_eq!(json["stops"].as_array().unwrap().len(), radial.stops.len());
    // The parameter still resolves — from the style, which the per-parameter
    // layer tags.
    assert_eq!(json["parameter"], "VRADH");
    assert_eq!(json["unit"], "m/s");

    // Distinct from the collection-level default it used to serve.
    let collection = legend_json_for("radar-fivih").await;
    assert_eq!(collection["min"], 0.0);
    assert_eq!(collection["max"], 1.0);
    assert_ne!(
        json["stops"], collection["stops"],
        "the parameter layer's legend must differ from the collection default's"
    );
}

/// A parameter with no style layer of its own still falls back to the
/// collection map — the pre-existing behaviour for every single-parameter
/// collection.
#[tokio::test]
async fn legend_graphic_falls_back_to_the_collection_style() {
    let json = legend_json_for("radar-fivih/DBZH").await;
    assert_eq!(json["min"], 0.0);
    assert_eq!(json["max"], 1.0);
    assert_eq!(
        json["stops"].as_array().unwrap().len(),
        ds_render::builtin_palette("viridis").unwrap().stops.len()
    );
    // The layer-name segment supplies the parameter when the style doesn't.
    assert_eq!(json["parameter"], "DBZH");
    assert_eq!(json["unit"], "dBZ");
}

#[tokio::test]
async fn legend_layer_parameter_overrides_style_parameter_like_get_map() {
    let app = build_param_layer_router();
    let req = Request::builder()
        .uri("/?SERVICE=WMS&REQUEST=GetLegendGraphic&VERSION=1.3.0&LAYER=radar-fivih/DBZH&STYLE=gray&FORMAT=application/json")
        .body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["parameter"], "DBZH");
    assert_eq!(json["unit"], "dBZ");
}

/// A style defined only on a parameter layer is reachable through that layer.
#[tokio::test]
async fn legend_graphic_serves_a_parameter_scoped_style() {
    let app = build_param_layer_router();
    let req = Request::builder()
        .uri(
            "/?SERVICE=WMS&REQUEST=GetLegendGraphic&VERSION=1.3.0\
             &LAYER=radar-fivih/VRADH&STYLES=vradh_only&FORMAT=application/json",
        )
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["style"], "vradh_only");
    assert_eq!(json["min"], -40.0);
    assert_eq!(json["max"], 50.0);
}

/// GetCapabilities XML for the multi-parameter site collection styled by
/// [`param_layer_styles`].
fn param_layer_capabilities_xml() -> String {
    let mut engines: HashMap<String, Arc<dyn MapEngine>> = HashMap::new();
    engines.insert("radar-fivih".to_string(), Arc::new(SiteMockMapEngine));
    let mut collections = HashMap::new();
    collections.insert(
        "radar-fivih".to_string(),
        site_collection_config("radar-fivih"),
    );
    let xml = api_wms::capabilities::get_capabilities_xml(
        &engines,
        &collections,
        &param_layer_styles(),
        "",
    );
    String::from_utf8(xml).expect("capabilities XML is UTF-8")
}

/// Slice one child `<Layer>` element out of the capabilities XML by its
/// `<Name>`, so a test can assert what that layer alone advertises.
fn child_layer_section<'a>(xml: &'a str, layer_name: &str) -> &'a str {
    let name_tag = format!("<Name>{layer_name}</Name>");
    let start = xml
        .find(&name_tag)
        .unwrap_or_else(|| panic!("no child layer named {layer_name} in:\n{xml}"));
    let end = xml[start..]
        .find("</Layer>")
        .map(|i| start + i)
        .unwrap_or(xml.len());
    &xml[start..end]
}

/// Each child layer advertises ITS OWN styles. Reusing the collection map for
/// every child advertised a parameter-scoped style on layers that would reject
/// it at GetMap time, and hid the styles the layer really has.
#[test]
fn capabilities_advertise_per_child_layer_styles() {
    let xml = param_layer_capabilities_xml();
    let vradh = child_layer_section(&xml, "radar-fivih/VRADH");
    let dbzh = child_layer_section(&xml, "radar-fivih/DBZH");

    // The parameter-scoped style appears under its layer only.
    assert!(
        vradh.contains("<Name>vradh_only</Name>"),
        "VRADH layer must advertise its own style; got:\n{vradh}"
    );
    assert!(
        !dbzh.contains("<Name>vradh_only</Name>"),
        "a VRADH-scoped style must not be advertised on the DBZH layer (GetMap \
         would reject it); got:\n{dbzh}"
    );
    // DBZH has no layer of its own → the collection's styles, as before.
    assert!(
        dbzh.contains("<Name>gray</Name>"),
        "DBZH must fall back to the collection styles; got:\n{dbzh}"
    );
    assert!(
        !vradh.contains("<Name>gray</Name>"),
        "the collection-only style must not leak onto a layer that defines its \
         own style set; got:\n{vradh}"
    );
}

/// A child layer's LegendURL points at the full layer name, so following it
/// returns the legend for the palette that layer actually renders with.
#[test]
fn capabilities_legend_url_carries_the_full_layer_name() {
    let xml = param_layer_capabilities_xml();
    let vradh = child_layer_section(&xml, "radar-fivih/VRADH");

    assert!(
        vradh.contains("LAYER=radar-fivih/VRADH&amp;STYLE=default"),
        "LegendURL must keep the /param segment (the key GetLegendGraphic \
         resolves); got:\n{vradh}"
    );
}

#[tokio::test]
async fn oversized_render_is_rejected_before_engine_dispatch() {
    let (app, _, calls) = build_counting_router(16);
    let request = Request::builder().uri("/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS=data&STYLES=&CRS=CRS:84&BBOX=20,60,30,70&WIDTH=8000&HEIGHT=8000&FORMAT=image/png").body(Body::empty()).unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["retry-after"], "1");
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
}

struct AlertDefaultMock {
    requested: Arc<std::sync::Mutex<Option<chrono::DateTime<chrono::Utc>>>>,
}
impl MapEngine for AlertDefaultMock {
    fn raster_info(&self) -> RasterInfo {
        let mut info = PopulatedMockMapEngine.raster_info();
        let now = self.default_time().unwrap();
        info.times = vec![now, now + chrono::Duration::days(1)];
        info
    }
    fn default_time(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        Some("2024-01-01T00:00:00Z".parse().unwrap())
    }
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<chrono::DateTime<chrono::Utc>>,
        crs: &OutputCrs,
        param: Option<&str>,
        z: Option<f64>,
        rt: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        *self.requested.lock().unwrap() = time;
        PopulatedMockMapEngine.get_raster_tile(bbox, width, height, time, crs, param, z, rt)
    }
}

#[tokio::test]
async fn engine_default_is_used_for_capabilities_and_timeless_map() {
    let requested = Arc::new(std::sync::Mutex::new(None));
    let engine = Arc::new(AlertDefaultMock {
        requested: requested.clone(),
    });
    let expected = engine.default_time();
    let app = build_populated_router_with_engine(engine);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/?SERVICE=WMS&REQUEST=GetCapabilities")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let xml = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(xml.contains("default=\"2024-01-01T00:00:00+00:00\""));
    assert!(xml.contains("2024-01-02T00:00:00+00:00"));
    let response = app
        .oneshot(
            Request::builder()
                .uri(GETMAP_URI)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(*requested.lock().unwrap(), expected);
}

struct ExhaustedEngine;
impl MapEngine for ExhaustedEngine {
    fn get_raster_tile(
        &self,
        _: [f64; 4],
        _: u32,
        _: u32,
        _: Option<chrono::DateTime<chrono::Utc>>,
        _: &OutputCrs,
        _: Option<&str>,
        _: Option<f64>,
        _: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        Err(DataServerError::ResourceExhausted)
    }
    fn raster_info(&self) -> RasterInfo {
        PopulatedMockMapEngine.raster_info()
    }
}

#[tokio::test]
async fn decode_exhaustion_is_503_not_a_successful_error_image() {
    let app = build_populated_router_with_engine(Arc::new(ExhaustedEngine));
    let response = app.oneshot(Request::builder()
        .uri("/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS=radar&CRS=CRS:84&BBOX=20,60,25,65&WIDTH=32&HEIGHT=32&FORMAT=image/png")
        .body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["retry-after"], "1");
    assert!(response.headers().get("etag").is_none());
}

struct DeadlineMockEngine {
    stalled: std::sync::atomic::AtomicBool,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}
impl MapEngine for DeadlineMockEngine {
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<chrono::DateTime<chrono::Utc>>,
        crs: &OutputCrs,
        parameter: Option<&str>,
        z: Option<f64>,
        reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        if self.stalled.load(std::sync::atomic::Ordering::SeqCst) {
            self.release
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(3))
                .unwrap();
            ds_core::deadline::check()?;
        }
        PopulatedMockMapEngine.get_raster_tile(
            bbox,
            width,
            height,
            time,
            crs,
            parameter,
            z,
            reference_time,
        )
    }
    fn raster_info(&self) -> RasterInfo {
        PopulatedMockMapEngine.raster_info()
    }
}

#[test]
fn render_deadline_queue_shedding_and_cache_bypass() {
    const CHILD: &str = "MC_TEST_RENDER_DEADLINE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "render_deadline_queue_shedding_and_cache_bypass",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("MC_RENDER_TIMEOUT_MS", "100")
            .env("MC_RENDER_QUEUE_CAPACITY", "0")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (release, wait) = std::sync::mpsc::channel();
        let engine = Arc::new(DeadlineMockEngine {
            stalled: std::sync::atomic::AtomicBool::new(false),
            release: std::sync::Mutex::new(wait),
        });
        let state = build_populated_state(engine.clone());
        let slots = state.load().render_semaphore.clone();
        let app = api_wms::router(state);
        let request = |uri: &str| Request::builder().uri(uri).body(Body::empty()).unwrap();
        assert_eq!(
            app.clone()
                .oneshot(request(GETMAP_URI))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let held = slots.clone().acquire_many_owned(4).await.unwrap();
        let hit = app.clone().oneshot(request(GETMAP_URI)).await.unwrap();
        assert_eq!(hit.status(), StatusCode::OK);
        assert_eq!(hit.headers()["x-cache"], "HIT");
        let miss_uri = GETMAP_URI.replace("WIDTH=64", "WIDTH=65");
        let shed = app.clone().oneshot(request(&miss_uri)).await.unwrap();
        assert_eq!(shed.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(shed.headers()["retry-after"], "1");
        assert!(shed.headers().get("etag").is_none());
        drop(held);
        engine
            .stalled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let response = app.oneshot(request(&miss_uri)).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["retry-after"], "1");
        assert!(response.headers().get("etag").is_none());
        assert_eq!(
            slots.available_permits(),
            3,
            "running worker must keep its permit after timeout"
        );
        release.send(()).unwrap();
        let _all_released =
            tokio::time::timeout(std::time::Duration::from_secs(2), slots.acquire_many(4))
                .await
                .unwrap()
                .unwrap();
    });
}

// ---------------------------------------------------------------------------
// Per-parameter time axes (#819)
// ---------------------------------------------------------------------------

mod per_parameter_times {
    use super::*;
    use chrono::{DateTime, Utc};

    const T0: &str = "2026-09-25T19:00:00Z";
    const T1: &str = "2026-09-25T19:10:00Z";
    const T2: &str = "2026-09-25T19:20:00Z";

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// Two parameters on different time axes, as in a satellite collection:
    /// `a` has three scans; `b`, a product that lands later, only the first
    /// two. The collection's times are the union. Records every render's
    /// `(parameter, time)`.
    /// One render's `(parameter, time)`.
    type Render = (Option<String>, Option<DateTime<Utc>>);

    #[derive(Default)]
    struct Engine {
        renders: std::sync::Mutex<Vec<Render>>,
    }

    fn times(parameter: Option<&str>) -> Vec<DateTime<Utc>> {
        match parameter {
            Some("b") => vec![t(T0), t(T1)],
            _ => vec![t(T0), t(T1), t(T2)],
        }
    }

    impl MapEngine for Engine {
        fn get_raster_tile(
            &self,
            _bbox: [f64; 4],
            width: u32,
            height: u32,
            time: Option<DateTime<Utc>>,
            _output_crs: &OutputCrs,
            parameter: Option<&str>,
            _z: Option<f64>,
            _reference_time: Option<DateTime<Utc>>,
        ) -> Result<RasterTile, DataServerError> {
            self.renders
                .lock()
                .unwrap()
                .push((parameter.map(str::to_string), time));
            Ok(RasterTile {
                width,
                height,
                values: vec![Some(0.5); (width * height) as usize].into(),
            })
        }

        fn raster_info(&self) -> RasterInfo {
            let parameter = |name: &str| ds_core::map_engine::ParameterInfo {
                name: name.to_string(),
                title: format!("Parameter {name}"),
                unit: "K".into(),
            };
            RasterInfo {
                native_crs: "CRS:84".into(),
                spatial_extent: Some([-20.0, 30.0, 40.0, 80.0]),
                times: times(None),
                parameter: "a".into(),
                unit: "K".into(),
                parameters: vec![parameter("a"), parameter("b")],
                vertical: None,
                grid_size: None,
                layer_subtitle: None,
                reference_times: Vec::new(),
            }
        }

        fn parameter_times(&self, parameter: &str) -> Option<Arc<[DateTime<Utc>]>> {
            Some(times(Some(parameter)).into())
        }

        fn resolve_parameter_time(
            &self,
            parameter: Option<&str>,
            time: Option<DateTime<Utc>>,
            _reference_time: Option<DateTime<Utc>>,
        ) -> Option<DateTime<Utc>> {
            let times = times(parameter);
            match time {
                Some(time) => times.into_iter().rev().find(|&ts| ts <= time),
                None => times.last().copied(),
            }
        }
    }

    fn router(engine: Arc<Engine>) -> axum::Router {
        let engine: Arc<dyn MapEngine> = engine;
        let config = CollectionConfig {
            id: "sat".to_string(),
            title: "Satellite".to_string(),
            description: "Two parameters on different time axes".to_string(),
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
            nowcast: None,
            bufr: None,
            satellite: None,
            preview: None,
            derive_wind: None,
        };
        let style = StyleInfo {
            name: "default".to_string(),
            title: "Default".to_string(),
            palette: ds_render::builtin_palette_arc("viridis").unwrap(),
            colormap: Arc::new(LutColorMap::from_builtin(
                BuiltinColormap::Viridis,
                0.0,
                1.0,
            )),
            min: 0.0,
            max: 1.0,
            parameter: None,
        };
        let state = Arc::new(ArcSwap::from_pointee(WmsState {
            engines: HashMap::from([("sat".to_string(), engine)]),
            collections: HashMap::from([("sat".to_string(), config)]),
            styles: HashMap::from([(
                "sat".to_string(),
                HashMap::from([("default".to_string(), style)]),
            )]),
            render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
            rendered_cache: Arc::new(RenderedCache::new(16)),
            tile_cache: Arc::new(ds_render::TilePixelCache::new(64)),
            base_url: String::new(),
            trust_proxy_headers: false,
        }));
        api_wms::router(state)
    }

    async fn get(app: &axum::Router, query: &str) -> (StatusCode, String) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/?SERVICE=WMS&VERSION=1.3.0&{query}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    /// The `<Layer>` element named `name`, as text.
    fn layer<'a>(caps: &'a str, name: &str) -> &'a str {
        let start = caps.find(&format!("<Name>{name}</Name>")).unwrap();
        let end = caps[start..].find("</Layer>").unwrap();
        &caps[start..start + end]
    }

    /// The parent layer advertises the union; each child layer re-declares
    /// the `time` dimension with its own values and default (WMS 1.3.0
    /// "replace" inheritance).
    #[tokio::test]
    async fn child_layers_advertise_their_own_times() {
        let app = router(Arc::default());
        let (status, caps) = get(&app, "REQUEST=GetCapabilities").await;
        assert_eq!(status, StatusCode::OK);
        let parent_dim = caps.find("<Dimension name=\"time\"").unwrap();
        assert!(caps[parent_dim..].starts_with(&format!(
            "<Dimension name=\"time\" units=\"ISO8601\" default=\"{}\"",
            t(T2).to_rfc3339()
        )));
        let b = layer(&caps, "sat/b");
        assert!(
            b.contains(&format!("default=\"{}\"", t(T1).to_rfc3339())),
            "{b}"
        );
        assert!(b.contains(&format!("{},{}<", t(T0).to_rfc3339(), t(T1).to_rfc3339())));
        assert!(!b.contains(&t(T2).to_rfc3339()));
        let a = layer(&caps, "sat/a");
        assert!(a.contains(&format!("default=\"{}\"", t(T2).to_rfc3339())));
    }

    /// An omitted TIME renders the requested parameter's latest time, and a
    /// time it lacks snaps on its own axis — before any cache key is built.
    #[tokio::test]
    async fn getmap_resolves_time_per_parameter() {
        let engine = Arc::new(Engine::default());
        let app = router(engine.clone());
        let map = |layer: &str, bbox: &str, time: Option<&str>| {
            format!(
                "REQUEST=GetMap&LAYERS={layer}&STYLES=&FORMAT=image/png&CRS=CRS:84\
                 &BBOX={bbox}&WIDTH=64&HEIGHT=64{}",
                time.map(|t| format!("&TIME={t}")).unwrap_or_default()
            )
        };
        let renders = |engine: &Engine| engine.renders.lock().unwrap().clone();

        assert_eq!(
            get(&app, &map("sat/b", "0,40,10,50", None)).await.0,
            StatusCode::OK
        );
        assert_eq!(
            get(&app, &map("sat/a", "0,40,10,50", None)).await.0,
            StatusCode::OK
        );
        assert_eq!(
            renders(&engine),
            [
                (Some("b".into()), Some(t(T1))),
                (Some("a".into()), Some(t(T2)))
            ]
        );

        // T2 exists for `a` only: `b` resolves to its T1 scan, which the
        // omitted-TIME request above already cached under T1 — a hit.
        assert_eq!(
            get(&app, &map("sat/b", "0,40,10,50", Some(T2))).await.0,
            StatusCode::OK
        );
        assert_eq!(renders(&engine).len(), 2);
        // Elsewhere, the same request renders `b` at T1.
        assert_eq!(
            get(&app, &map("sat/b", "10,40,20,50", Some(T2))).await.0,
            StatusCode::OK
        );
        assert_eq!(renders(&engine)[2], (Some("b".into()), Some(t(T1))));
    }
}

// --- TRANSPARENT / BGCOLOR (#163) ------------------------------------------

/// Data west of 20°E, nodata east of it, counting engine calls. Geographic,
/// not tile-local, so the direct and the meta-tiled path agree: a lon 10–30
/// view is half data, half nodata either way.
struct HalfDataMockMapEngine {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl MapEngine for HalfDataMockMapEngine {
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
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
        let [west, _, east, _] = bbox;
        let values: Vec<Option<f64>> = (0..width * height)
            .map(|i| {
                let lon = west + ((i % width) as f64 + 0.5) / width as f64 * (east - west);
                (lon < 20.0).then_some(0.5)
            })
            .collect();
        Ok(RasterTile {
            width,
            height,
            values: values.into(),
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

fn build_half_data_router() -> (axum::Router, Arc<std::sync::atomic::AtomicUsize>) {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let engine = Arc::new(HalfDataMockMapEngine {
        calls: calls.clone(),
    });
    (build_populated_router_with_engine(engine), calls)
}

/// The same lon 10–30 / lat 55–70 view on the direct (CRS:84) and the
/// meta-tiled (EPSG:3857) render path.
const HALF_VIEWS: [(&str, &str); 2] = [
    ("CRS:84", "10,55,30,70"),
    ("EPSG:3857", "1113194,7361866,3339584,11068715"),
];

const BG: [u8; 4] = [0x33, 0x66, 0x99, 255];

/// GET a 64×64 GetMap of `layer` with `extra` query parameters appended.
async fn get_map_with(
    app: &axum::Router,
    layer: &str,
    crs: &str,
    bbox: &str,
    extra: &str,
) -> (StatusCode, String, bytes::Bytes) {
    let uri = format!(
        "/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS={layer}&STYLES=\
         &CRS={crs}&BBOX={bbox}&WIDTH=64&HEIGHT=64{extra}"
    );
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let x_cache = resp
        .headers()
        .get("x-cache")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, x_cache, body)
}

/// Decode a PNG (indexed or truecolour, with or without alpha) to RGBA pixels.
fn png_pixels(bytes: &[u8]) -> Vec<[u8; 4]> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0u8; reader.output_buffer_size().expect("png buffer size")];
    let frame = reader.next_frame(&mut buf).unwrap();
    let data = &buf[..frame.buffer_size()];
    match frame.color_type {
        png::ColorType::Rgba => data.as_chunks::<4>().0.to_vec(),
        png::ColorType::Rgb => data
            .as_chunks::<3>()
            .0
            .iter()
            .map(|&[r, g, b]| [r, g, b, 255])
            .collect(),
        other => panic!("unexpected decoded PNG colour type: {other:?}"),
    }
}

/// #1010: a meta-tiled EPSG:3857 viewport is cached on its first render,
/// though its bbox and size match no tile grid. Such viewports are the views
/// a live deployment saw repeated (a fixed display cycling its frames, a
/// client's default view), so admitting only tile-aligned views, or only a
/// view's second render, would forfeit the hits this cache earns.
#[tokio::test]
async fn meta_tiled_viewport_is_cached_on_its_first_render() {
    use std::sync::atomic::Ordering;
    let (app, calls) = build_half_data_router();
    let view = "/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS=radar&STYLES=\
                &CRS=EPSG:3857&BBOX=1113194,7361866,3339584,11068715\
                &WIDTH=96&HEIGHT=160&FORMAT=image/png";
    let get = |uri: String| {
        let app = app.clone();
        async move {
            let resp = app
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let x_cache = resp.headers()["x-cache"].to_str().unwrap().to_string();
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            (x_cache, body)
        }
    };

    let (x, first) = get(view.to_string()).await;
    assert_eq!(x, "MISS");
    let rendered = calls.load(Ordering::Relaxed);
    assert!(rendered > 0);

    // Meta-tiled: an opaque copy of the view is another rendered-cache key,
    // yet it is assembled from the meta-tiles cached above.
    let (x, _) = get(format!("{view}&TRANSPARENT=FALSE")).await;
    assert_eq!(x, "MISS");
    assert_eq!(calls.load(Ordering::Relaxed), rendered, "meta-tiled view");

    let (x, second) = get(view.to_string()).await;
    assert_eq!(x, "HIT", "cached on its first render");
    assert_eq!(second, first);
}

/// TRANSPARENT=FALSE paints nodata with BGCOLOR and returns an opaque image;
/// the default keeps nodata transparent — on both render paths.
#[tokio::test]
async fn transparent_false_composites_nodata_over_bgcolor() {
    for (crs, bbox) in HALF_VIEWS {
        let (app, _) = build_half_data_router();

        let (status, _, body) = get_map_with(&app, "radar", crs, bbox, "&FORMAT=image/png").await;
        assert_eq!(status, StatusCode::OK);
        let default = png_pixels(&body);
        assert!(
            default.iter().any(|p| p[3] == 0),
            "{crs}: default output keeps nodata transparent"
        );
        assert!(default.iter().any(|p| p[3] == 255), "{crs}: data is drawn");

        let (status, _, body) = get_map_with(
            &app,
            "radar",
            crs,
            bbox,
            "&FORMAT=image/png&TRANSPARENT=FALSE&BGCOLOR=0x336699",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let opaque = png_pixels(&body);
        assert!(
            opaque.iter().all(|p| p[3] == 255),
            "{crs}: TRANSPARENT=FALSE output must be opaque"
        );
        assert!(opaque.contains(&BG), "{crs}: nodata takes BGCOLOR");
        assert!(
            opaque.iter().any(|&p| p != BG),
            "{crs}: data pixels keep their colour"
        );
        // Pixel for pixel: data unchanged, nodata → BGCOLOR.
        for (d, o) in default.iter().zip(&opaque) {
            let expected = if d[3] == 0 { BG } else { *d };
            assert_eq!(*o, expected, "{crs}");
        }

        // Without BGCOLOR the background is the spec default, white.
        let (_, _, body) = get_map_with(
            &app,
            "radar",
            crs,
            bbox,
            "&FORMAT=image/png&transparent=false",
        )
        .await;
        let white = png_pixels(&body);
        assert!(white.iter().all(|p| p[3] == 255));
        assert!(white.contains(&[255, 255, 255, 255]), "{crs}");
    }
}

/// The rendered-image cache must never serve a transparent image to an opaque
/// request (or vice versa), while the meta-tile cache — plain RGBA — is shared
/// by both.
#[tokio::test]
async fn opaque_request_is_not_served_a_cached_transparent_image() {
    use std::sync::atomic::Ordering;
    for (crs, bbox) in HALF_VIEWS {
        let (app, calls) = build_half_data_router();
        let png = "&FORMAT=image/png";
        let opaque = "&FORMAT=image/png&TRANSPARENT=FALSE&BGCOLOR=0x336699";

        let (_, x, transparent_body) = get_map_with(&app, "radar", crs, bbox, png).await;
        assert_eq!(x, "MISS");
        let after_transparent = calls.load(Ordering::Relaxed);

        let (_, x, opaque_body) = get_map_with(&app, "radar", crs, bbox, opaque).await;
        assert_eq!(
            x, "MISS",
            "{crs}: opaque must not hit the transparent entry"
        );
        assert!(png_pixels(&opaque_body).iter().all(|p| p[3] == 255));
        if crs == "EPSG:3857" {
            assert_eq!(
                calls.load(Ordering::Relaxed),
                after_transparent,
                "meta-tiled: the opaque view composites the cached RGBA tiles"
            );
        }

        let (_, x, body) = get_map_with(&app, "radar", crs, bbox, opaque).await;
        assert_eq!(x, "HIT", "{crs}");
        assert_eq!(body, opaque_body);
        let (_, x, body) = get_map_with(&app, "radar", crs, bbox, png).await;
        assert_eq!(x, "HIT", "{crs}");
        assert_eq!(body, transparent_body);
        // A different background is a different image.
        let (_, x, body) = get_map_with(
            &app,
            "radar",
            crs,
            bbox,
            "&FORMAT=image/png&TRANSPARENT=FALSE&BGCOLOR=0x000000",
        )
        .await;
        assert_eq!(x, "MISS", "{crs}");
        assert_ne!(body, opaque_body);
    }
}

/// An all-nodata view with TRANSPARENT=FALSE is a solid BGCOLOR image, on the
/// direct and the meta-tiled empty path; the default stays transparent.
#[tokio::test]
async fn all_nodata_opaque_view_is_solid_bgcolor() {
    let app = build_empty_router();
    for (crs, bbox) in HALF_VIEWS {
        let (status, x, body) = get_map_with(
            &app,
            "empty",
            crs,
            bbox,
            "&FORMAT=image/png&TRANSPARENT=FALSE&BGCOLOR=0x336699",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(x, "EMPTY", "{crs}");
        let pixels = png_pixels(&body);
        assert_eq!(pixels.len(), 64 * 64);
        assert!(pixels.iter().all(|&p| p == BG), "{crs}: solid BGCOLOR");

        let (_, x, body) = get_map_with(&app, "empty", crs, bbox, "&FORMAT=image/png").await;
        assert_eq!(x, "EMPTY", "{crs}");
        assert!(png_pixels(&body).iter().all(|&p| p == [0, 0, 0, 0]));
    }
}

/// The semi-transparent red error tile is composited over BGCOLOR too.
#[tokio::test]
async fn error_tile_honours_transparent_false() {
    let app = build_failing_router();
    let (_, x, body) = get_map_with(
        &app,
        "broken",
        "CRS:84",
        "10,55,30,70",
        "&FORMAT=image/png&TRANSPARENT=FALSE",
    )
    .await;
    assert_eq!(x, "ERROR");
    // [255, 0, 0, 100] over white.
    assert!(png_pixels(&body).iter().all(|&p| p == [255, 155, 155, 255]));

    let (_, x, body) =
        get_map_with(&app, "broken", "CRS:84", "10,55,30,70", "&FORMAT=image/png").await;
    assert_eq!(x, "ERROR");
    assert!(png_pixels(&body).iter().all(|&p| p == [255, 0, 0, 100]));
}

/// JPEG has no alpha, so it is always composited over BGCOLOR (default
/// white): TRANSPARENT is moot, and BGCOLOR changes the image.
#[tokio::test]
async fn jpeg_is_composited_over_bgcolor() {
    let (app, _) = build_half_data_router();
    let (crs, bbox) = HALF_VIEWS[0];
    let (status, x, default) = get_map_with(&app, "radar", crs, bbox, "&FORMAT=image/jpeg").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(x, "MISS");
    assert_eq!(&default[..2], &[0xFF, 0xD8], "a JPEG");

    // Same white background, so the same cached image.
    let (_, x, body) = get_map_with(
        &app,
        "radar",
        crs,
        bbox,
        "&FORMAT=image/jpeg&TRANSPARENT=FALSE&BGCOLOR=0xFFFFFF",
    )
    .await;
    assert_eq!(x, "HIT");
    assert_eq!(body, default);

    let (_, x, body) = get_map_with(
        &app,
        "radar",
        crs,
        bbox,
        "&FORMAT=image/jpeg&BGCOLOR=0x336699",
    )
    .await;
    assert_eq!(x, "MISS");
    assert_eq!(&body[..2], &[0xFF, 0xD8]);
    assert_ne!(body, default, "BGCOLOR must replace the white background");
}

/// A malformed BGCOLOR or TRANSPARENT is an InvalidParameterValue
/// ServiceException, not a silently ignored parameter.
#[tokio::test]
async fn malformed_bgcolor_or_transparent_is_invalid_parameter_value() {
    let (app, calls) = build_half_data_router();
    for extra in [
        "&BGCOLOR=%23336699",
        "&BGCOLOR=336699",
        "&BGCOLOR=0x3366",
        "&BGCOLOR=0x33669G",
        "&BGCOLOR=0X336699",
        "&TRANSPARENT=FALSE&BGCOLOR=white",
        "&TRANSPARENT=maybe",
    ] {
        let (status, _, body) = get_map_with(
            &app,
            "radar",
            "CRS:84",
            "10,55,30,70",
            &format!("&FORMAT=image/png{extra}"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{extra}");
        let xml = String::from_utf8(body.to_vec()).unwrap();
        assert!(xml.contains("<ServiceExceptionReport"), "{extra}: {xml}");
        assert!(
            xml.contains("code=\"InvalidParameterValue\""),
            "{extra}: {xml}"
        );
    }
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "rejected before rendering"
    );
}

// --- RGB composite layers (#819) ------------------------------------------

mod composites {
    use super::*;
    use chrono::{DateTime, Utc};
    use ds_core::map_engine::{select_common_time, CompositeChannel, CompositeDef};
    use ds_render::{CompositeSpec, ImageFormat};
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    const T0: &str = "2026-09-25T19:00:00Z";
    const T1: &str = "2026-09-25T19:10:00Z";
    const T2: &str = "2026-09-25T19:20:00Z";
    const RGB: &str = "rgb";

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// Red `a - b` over -10..40, green `b` over 0..100, blue `a` inverted
    /// over 100..0.
    fn rgb() -> CompositeDef {
        let channel = |parameter: &str, minus: Option<&str>, min, max| CompositeChannel {
            parameter: parameter.into(),
            minus: minus.map(String::from),
            min,
            max,
            gamma: 1.0,
        };
        CompositeDef {
            name: RGB.into(),
            title: "A and B".into(),
            channels: [
                channel("a", Some("b"), -10.0, 40.0),
                channel("b", None, 0.0, 100.0),
                channel("a", None, 100.0, 0.0),
            ],
        }
    }

    /// One band's tile: `a` is 50 + the scan's ten-minute step everywhere;
    /// `b` is 20 + the step east of 5°E and nodata west of it. So each
    /// scan draws its own pixels, and a composite reading `b` is
    /// transparent west of 5°E.
    fn band(parameter: &str, time: DateTime<Utc>, bbox: [f64; 4], w: u32, h: u32) -> RasterTile {
        let step = ((time - t(T0)).num_minutes() / 10) as f64;
        let values: Vec<Option<f64>> = (0..h)
            .flat_map(|_| {
                (0..w).map(move |i| {
                    let lon = bbox[0] + (i as f64 + 0.5) * (bbox[2] - bbox[0]) / w as f64;
                    match parameter {
                        "a" => Some(50.0 + step),
                        "b" if lon >= 5.0 => Some(20.0 + step),
                        _ => None,
                    }
                })
            })
            .collect();
        RasterTile {
            width: w,
            height: h,
            values: values.into(),
        }
    }

    /// One `get_raster_tiles` call: its bands and time.
    type Call = (Vec<String>, Option<DateTime<Utc>>);

    /// Two bands on their own time axes, like a satellite collection, and
    /// one composite over both. The axes can change under a running router.
    struct Engine {
        axes: Mutex<BTreeMap<&'static str, Vec<DateTime<Utc>>>>,
        calls: Mutex<Vec<Call>>,
    }

    impl Engine {
        fn new(a: &[&str], b: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                axes: Mutex::new(BTreeMap::from([
                    ("a", a.iter().map(|s| t(s)).collect()),
                    ("b", b.iter().map(|s| t(s)).collect()),
                ])),
                calls: Mutex::default(),
            })
        }

        fn add_scan(&self, band: &'static str, time: &str) {
            let mut axes = self.axes.lock().unwrap();
            let axis = axes.get_mut(band).unwrap();
            axis.push(t(time));
            axis.sort();
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        fn axis(&self, band: &str) -> Vec<DateTime<Utc>> {
            self.axes.lock().unwrap()[band].clone()
        }

        /// The scans both bands have.
        fn shared(&self) -> Vec<DateTime<Utc>> {
            let b = self.axis("b");
            self.axis("a")
                .into_iter()
                .filter(|t| b.contains(t))
                .collect()
        }
    }

    impl MapEngine for Engine {
        fn get_raster_tile(
            &self,
            bbox: [f64; 4],
            width: u32,
            height: u32,
            time: Option<DateTime<Utc>>,
            _output_crs: &OutputCrs,
            parameter: Option<&str>,
            _z: Option<f64>,
            _reference_time: Option<DateTime<Utc>>,
        ) -> Result<RasterTile, DataServerError> {
            let parameter = parameter.unwrap_or("a");
            if parameter == RGB {
                return Err(DataServerError::InvalidParameter("not a band".into()));
            }
            let time = time.unwrap_or_else(|| *self.axis(parameter).last().unwrap());
            Ok(band(parameter, time, bbox, width, height))
        }

        fn get_raster_tiles(
            &self,
            bbox: [f64; 4],
            width: u32,
            height: u32,
            time: Option<DateTime<Utc>>,
            _output_crs: &OutputCrs,
            parameters: &[&str],
            _z: Option<f64>,
            _reference_time: Option<DateTime<Utc>>,
        ) -> Result<Vec<RasterTile>, DataServerError> {
            self.calls
                .lock()
                .unwrap()
                .push((parameters.iter().map(|p| p.to_string()).collect(), time));
            let time = time.expect("a composite renders the time it was keyed on");
            parameters
                .iter()
                .map(|p| {
                    if self.axis(p).contains(&time) {
                        Ok(band(p, time, bbox, width, height))
                    } else {
                        Err(DataServerError::InvalidParameter(format!(
                            "{p} has no scan"
                        )))
                    }
                })
                .collect()
        }

        fn raster_info(&self) -> RasterInfo {
            let parameter = |name: &str| ds_core::map_engine::ParameterInfo {
                name: name.to_string(),
                title: format!("Band {name}"),
                unit: "K".into(),
            };
            let mut times: Vec<DateTime<Utc>> = self
                .axes
                .lock()
                .unwrap()
                .values()
                .flatten()
                .copied()
                .collect();
            times.sort();
            times.dedup();
            RasterInfo {
                native_crs: "CRS:84".into(),
                spatial_extent: Some([-20.0, 30.0, 40.0, 80.0]),
                times,
                parameter: "a".into(),
                unit: "K".into(),
                parameters: vec![parameter("a"), parameter("b")],
                vertical: None,
                grid_size: None,
                layer_subtitle: None,
                reference_times: Vec::new(),
            }
        }

        fn parameter_times(&self, parameter: &str) -> Option<Arc<[DateTime<Utc>]>> {
            if parameter == RGB {
                return Some(self.shared().into());
            }
            Some(self.axis(parameter).into())
        }

        fn resolve_parameter_time(
            &self,
            parameter: Option<&str>,
            time: Option<DateTime<Utc>>,
            _reference_time: Option<DateTime<Utc>>,
        ) -> Option<DateTime<Utc>> {
            let parameter = parameter.unwrap_or("a");
            if parameter == RGB {
                let (a, b) = (self.axis("a"), self.axis("b"));
                return select_common_time(&[&a, &b], time);
            }
            select_common_time(&[&self.axis(parameter)], time)
        }

        fn composites(&self) -> Arc<[CompositeDef]> {
            Arc::from([rgb()])
        }
    }

    fn router(engine: Arc<Engine>) -> (axum::Router, Arc<ds_render::TilePixelCache>) {
        let engine: Arc<dyn MapEngine> = engine;
        let config = CollectionConfig {
            id: "sat".to_string(),
            title: "Satellite".to_string(),
            description: "Two bands and an RGB composite".to_string(),
            data_path: None,
            apis: vec!["wms".to_string()],
            engine_type: "satellite".to_string(),
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
            bufr: None,
            satellite: None,
            preview: None,
            derive_wind: None,
        };
        let style = StyleInfo {
            name: "default".to_string(),
            title: "Default".to_string(),
            palette: ds_render::builtin_palette_arc("viridis").unwrap(),
            colormap: Arc::new(LutColorMap::from_builtin(
                BuiltinColormap::Viridis,
                0.0,
                100.0,
            )),
            min: 0.0,
            max: 100.0,
            parameter: None,
        };
        let mut viridis = style.clone();
        viridis.name = "viridis".into();
        let tile_cache = Arc::new(ds_render::TilePixelCache::new(64));
        let state = Arc::new(ArcSwap::from_pointee(WmsState {
            engines: HashMap::from([("sat".to_string(), engine)]),
            collections: HashMap::from([("sat".to_string(), config)]),
            styles: HashMap::from([(
                "sat".to_string(),
                HashMap::from([
                    ("default".to_string(), style),
                    ("viridis".to_string(), viridis),
                ]),
            )]),
            render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
            rendered_cache: Arc::new(RenderedCache::new(16)),
            tile_cache: tile_cache.clone(),
            base_url: String::new(),
            trust_proxy_headers: false,
        }));
        (api_wms::router(state), tile_cache)
    }

    struct Reply {
        status: StatusCode,
        x_cache: String,
        content_type: String,
        body: bytes::Bytes,
    }

    async fn get(app: &axum::Router, query: &str) -> Reply {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/?SERVICE=WMS&VERSION=1.3.0&{query}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let header = |name: &str| {
            resp.headers()
                .get(name)
                .map(|v| v.to_str().unwrap().to_string())
                .unwrap_or_default()
        };
        let (status, x_cache, content_type) =
            (resp.status(), header("x-cache"), header("content-type"));
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            x_cache,
            content_type,
            body,
        }
    }

    fn get_map(layer: &str, crs: &str, bbox: &str, extra: &str) -> String {
        format!(
            "REQUEST=GetMap&LAYERS={layer}&STYLES=&FORMAT=image/png&CRS={crs}\
             &BBOX={bbox}&WIDTH=64&HEIGHT=64{extra}"
        )
    }

    /// The `<Layer>` element named `name`, as text.
    fn layer<'a>(caps: &'a str, name: &str) -> &'a str {
        let start = caps.find(&format!("<Name>{name}</Name>")).unwrap();
        let end = caps[start..].find("</Layer>").unwrap();
        &caps[start..start + end]
    }

    /// The composite is a child layer next to the bands: its own time
    /// dimension (the scans both bands have), one `default` style and a
    /// legend URL.
    #[tokio::test]
    async fn capabilities_list_the_composite_layer() {
        let (app, _) = router(Engine::new(&[T0, T1, T2], &[T0, T1]));
        let reply = get(&app, "REQUEST=GetCapabilities").await;
        assert_eq!(reply.status, StatusCode::OK);
        let caps = String::from_utf8(reply.body.to_vec()).unwrap();
        assert!(caps.contains("<Name>sat/a</Name>"));
        assert!(caps.contains("<Name>sat/b</Name>"));

        let composite = layer(&caps, "sat/rgb");
        assert!(composite.contains("<Title>A and B</Title>"), "{composite}");
        assert!(
            composite.contains("<Abstract>RGB composite: red a - b, green b, blue a</Abstract>"),
            "{composite}"
        );
        assert!(
            composite.contains(&format!(
                "<Dimension name=\"time\" units=\"ISO8601\" default=\"{}\" nearestValue=\"1\">{},{}</Dimension>",
                t(T1).to_rfc3339(),
                t(T0).to_rfc3339(),
                t(T1).to_rfc3339()
            )),
            "{composite}"
        );
        assert_eq!(composite.matches("<Style>").count(), 1, "{composite}");
        assert!(composite.contains("<Name>default</Name>"), "{composite}");
        assert!(
            composite.contains("REQUEST=GetLegendGraphic&amp;LAYER=sat/rgb&amp;STYLE=default"),
            "{composite}"
        );
    }

    /// A GetMap is `render_composite_tiles` over the band tiles at the
    /// resolved time: the scans both bands have, T1, not `a`'s newer T2.
    /// TRANSPARENT=FALSE fills the nodata half with BGCOLOR, as for a
    /// parameter layer.
    #[tokio::test]
    async fn getmap_composes_the_bands_at_the_shared_time() {
        let engine = Engine::new(&[T0, T1, T2], &[T0, T1]);
        let (app, _) = router(engine.clone());
        let reply = get(&app, &get_map("sat/rgb", "CRS:84", "0,40,10,50", "")).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.x_cache, "MISS");
        assert_eq!(reply.content_type, "image/png");
        assert_eq!(
            engine.calls(),
            [(vec!["a".to_string(), "b".to_string()], Some(t(T1)))]
        );

        let spec = CompositeSpec::from(&rgb());
        let tiles = engine
            .get_raster_tiles(
                [0.0, 40.0, 10.0, 50.0],
                64,
                64,
                Some(t(T1)),
                &OutputCrs::Wgs84,
                &["a", "b"],
                None,
                None,
            )
            .unwrap();
        let expected = ds_render::render_composite_tiles(&tiles, &spec, ImageFormat::Png, None)
            .unwrap()
            .unwrap();
        assert_eq!(reply.body, expected);

        // East of 5°E: red 51 - 21 = 30 over -10..40 → 204, green 21 → 54,
        // blue 51 inverted over 100..0 → 125. West of it `b` is nodata.
        let pixels = png_pixels(&reply.body);
        assert_eq!(pixels[63], [204, 54, 125, 255]);
        assert_eq!(pixels[0], [0, 0, 0, 0]);

        let opaque = get(
            &app,
            &get_map(
                "sat/rgb",
                "CRS:84",
                "0,40,10,50",
                "&TRANSPARENT=FALSE&BGCOLOR=0x00FF00",
            ),
        )
        .await;
        assert_eq!(opaque.status, StatusCode::OK);
        let pixels = png_pixels(&opaque.body);
        assert_eq!(pixels[0], [0, 255, 0, 255]);
        assert_eq!(pixels[63], [204, 54, 125, 255]);
    }

    /// #507: the caches key on the time every band is read from. A newer
    /// scan of ONE band leaves the composite's frame, its key and its
    /// pixels, alone; only a scan both bands have moves it.
    #[tokio::test]
    async fn a_newer_scan_of_one_band_does_not_move_the_frame() {
        let engine = Engine::new(&[T0, T1], &[T0, T1]);
        let (app, _) = router(engine.clone());
        let map = |extra: &str| get_map("sat/rgb", "CRS:84", "0,40,10,50", extra);

        let first = get(&app, &map("")).await;
        assert_eq!(first.x_cache, "MISS");
        assert_eq!(engine.calls().len(), 1);

        engine.add_scan("a", T2);
        let latest = get(&app, &map("")).await;
        assert_eq!(latest.x_cache, "HIT");
        assert_eq!(latest.body, first.body);
        // T2 exists for `a` only: it resolves to T1, the same key.
        let pinned = get(&app, &map(&format!("&TIME={T2}"))).await;
        assert_eq!(pinned.x_cache, "HIT");
        assert_eq!(engine.calls().len(), 1);

        // Once `b` has T2 too, the frame moves and renders anew.
        engine.add_scan("b", T2);
        let moved = get(&app, &map("")).await;
        assert_eq!(moved.x_cache, "MISS");
        assert_ne!(moved.body, first.body);
        assert_eq!(engine.calls()[1].1, Some(t(T2)));
    }

    /// Projected output goes through meta-tiling: every tile reads the
    /// bands at the resolved time, the composed tiles are cached, and a
    /// newer scan of one band reuses them (#507).
    #[tokio::test]
    async fn projected_composites_are_meta_tiled_at_the_resolved_time() {
        let engine = Engine::new(&[T0, T1], &[T0, T1]);
        let (app, tile_cache) = router(engine.clone());
        let bbox = "0,4900000,1100000,6000000";
        let first = get(&app, &get_map("sat/rgb", "EPSG:3857", bbox, "")).await;
        assert_eq!(first.status, StatusCode::OK);
        assert_eq!(first.content_type, "image/png");
        let calls = engine.calls();
        assert!(!calls.is_empty());
        assert!(calls
            .iter()
            .all(|(bands, time)| bands == &["a", "b"] && *time == Some(t(T1))));
        let (_, misses) = tile_cache.stats();
        assert_eq!(misses as usize, calls.len());
        let pixels = png_pixels(&first.body);
        assert_eq!(pixels[63], [204, 54, 125, 255]);
        assert_eq!(pixels[0], [0, 0, 0, 0]);

        // Another format misses the rendered cache but assembles from the
        // same composed tiles, still at T1 after `a` gains T2.
        engine.add_scan("a", T2);
        let webp = get(
            &app,
            &get_map("sat/rgb", "EPSG:3857", bbox, "").replace("image/png", "image/webp"),
        )
        .await;
        assert_eq!(webp.status, StatusCode::OK);
        assert_eq!(webp.content_type, "image/webp");
        assert_eq!(engine.calls().len(), calls.len());

        // EPSG:3067 meta-tiles too.
        let projected = get(
            &app,
            &get_map("sat/rgb", "EPSG:3067", "100000,6500000,356000,6756000", ""),
        )
        .await;
        assert_eq!(projected.status, StatusCode::OK);
        assert!(engine.calls().len() > calls.len());
        assert!(engine.calls().iter().all(|(_, time)| *time == Some(t(T1))));
    }

    /// Bands that share no scan resolve no time: an empty image, and the
    /// engine is not asked for a timestep no key names.
    #[tokio::test]
    async fn no_shared_scan_is_an_empty_image() {
        let engine = Engine::new(&[T0], &[T1]);
        let (app, _) = router(engine.clone());
        for (crs, bbox) in [
            ("CRS:84", "0,40,10,50"),
            ("EPSG:3857", "0,4900000,1100000,6000000"),
        ] {
            let reply = get(&app, &get_map("sat/rgb", crs, bbox, &format!("&TIME={T1}"))).await;
            assert_eq!(reply.status, StatusCode::OK);
            assert_eq!(reply.x_cache, "EMPTY");
        }
        assert!(engine.calls().is_empty());
    }

    /// A composite has one style: another is StyleNotDefined, on GetMap
    /// and GetLegendGraphic alike. Unknown layer names list the composite.
    #[tokio::test]
    async fn composites_reject_other_styles_and_list_among_layers() {
        let (app, _) = router(Engine::new(&[T0, T1], &[T0, T1]));
        let styled = get(
            &app,
            &get_map("sat/rgb", "CRS:84", "0,40,10,50", "").replace("STYLES=", "STYLES=viridis"),
        )
        .await;
        assert_eq!(styled.status, StatusCode::BAD_REQUEST);
        let body = String::from_utf8(styled.body.to_vec()).unwrap();
        assert!(body.contains("StyleNotDefined"), "{body}");
        assert!(body.contains("Available: default"), "{body}");

        let legend = get(
            &app,
            "REQUEST=GetLegendGraphic&LAYER=sat/rgb&STYLE=viridis&FORMAT=image/png",
        )
        .await;
        assert_eq!(legend.status, StatusCode::BAD_REQUEST);

        let unknown = get(&app, &get_map("sat/nope", "CRS:84", "0,40,10,50", "")).await;
        assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
        let body = String::from_utf8(unknown.body.to_vec()).unwrap();
        assert!(body.contains("Available: a, b, rgb"), "{body}");

        // The bands' own layers still render with their styles.
        let band = get(
            &app,
            &get_map("sat/a", "CRS:84", "0,40,10,50", "").replace("STYLES=", "STYLES=viridis"),
        )
        .await;
        assert_eq!(band.status, StatusCode::OK);
    }

    /// The composite's legend is its channel list, JSON or image, with the
    /// bands' units and no colour bar.
    #[tokio::test]
    async fn legend_graphic_lists_the_channels() {
        let (app, _) = router(Engine::new(&[T0, T1], &[T0, T1]));
        let json = get(
            &app,
            "REQUEST=GetLegendGraphic&LAYER=sat/rgb&FORMAT=application/json",
        )
        .await;
        assert_eq!(json.status, StatusCode::OK);
        let legend: serde_json::Value = serde_json::from_slice(&json.body).unwrap();
        assert_eq!(
            legend,
            serde_json::json!({
                "style": "default",
                "parameter": "rgb",
                "title": "A and B",
                "channels": [
                    {"channel": "red", "label": "a - b", "parameters": ["a", "b"],
                     "min": -10.0, "max": 40.0, "gamma": 1.0, "unit": "K"},
                    {"channel": "green", "label": "b", "parameters": ["b"],
                     "min": 0.0, "max": 100.0, "gamma": 1.0, "unit": "K"},
                    {"channel": "blue", "label": "a", "parameters": ["a"],
                     "min": 100.0, "max": 0.0, "gamma": 1.0, "unit": "K"},
                ]
            })
        );

        let image = get(
            &app,
            "REQUEST=GetLegendGraphic&LAYER=sat/rgb&STYLE=default&FORMAT=image/png",
        )
        .await;
        assert_eq!(image.status, StatusCode::OK);
        assert_eq!(image.content_type, "image/png");
        let expected = ds_render::render_composite_legend(
            &CompositeSpec::from(&rgb()),
            &[Some("K"), Some("K")],
            ds_render::LEGEND_DEFAULT_WIDTH,
            ds_render::LEGEND_DEFAULT_HEIGHT,
            ImageFormat::Png,
        )
        .unwrap();
        assert_eq!(image.body, expected);
    }
}

// ---------------------------------------------------------------------------
// QUALITY vendor parameter and `[wms] webp_quality`
// ---------------------------------------------------------------------------

/// The first RIFF chunk of a WebP body: `VP8L` is the lossless bitstream,
/// `VP8 ` / `VP8X` lossy (the latter with an alpha chunk).
fn webp_chunk(body: &[u8]) -> &[u8] {
    assert_eq!(&body[..4], b"RIFF", "not a WebP body");
    assert_eq!(&body[8..12], b"WEBP", "not a WebP body");
    &body[12..16]
}

struct QualityResponse {
    status: StatusCode,
    content_type: String,
    x_cache: String,
    etag: String,
    body: Vec<u8>,
}

async fn get_quality(app: &axum::Router, crs: &str, bbox: &str, extra: &str) -> QualityResponse {
    let uri = format!(
        "/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS=radar&STYLES=\
         &CRS={crs}&BBOX={bbox}&WIDTH=64&HEIGHT=64&TIME=2024-01-01T00:00:00Z{extra}"
    );
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default()
    };
    let (status, content_type, x_cache, etag) = (
        resp.status(),
        header("content-type"),
        header("x-cache"),
        header("etag"),
    );
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    QualityResponse {
        status,
        content_type,
        x_cache,
        etag,
        body,
    }
}

/// Both render paths: the direct one (CRS:84) and the meta-tiled one
/// (EPSG:3857), whose final encode of the assembled view takes the quality.
const QUALITY_VIEWS: [(&str, &str); 2] = [
    ("CRS:84", "10,55,30,70"),
    ("EPSG:3857", "1113194,7361866,3339584,11068715"),
];

/// `QUALITY=1..99` on `image/webp` is a lossy encode; the key is
/// case-insensitive like every WMS parameter.
#[tokio::test]
async fn quality_selects_lossy_webp_with_any_key_case() {
    for (crs, bbox) in QUALITY_VIEWS {
        let app = build_populated_router();
        let upper = get_quality(&app, crs, bbox, "&FORMAT=image/webp&QUALITY=80").await;
        assert_eq!(upper.status, StatusCode::OK, "{crs}");
        assert_eq!(upper.content_type, "image/webp");
        assert_eq!(upper.x_cache, "MISS");
        assert_ne!(webp_chunk(&upper.body), b"VP8L", "{crs}: must be lossy");
        // The other spellings name the same request: a cache hit on the
        // same bytes.
        for key in ["quality", "Quality"] {
            let other = get_quality(&app, crs, bbox, &format!("&FORMAT=image/webp&{key}=80")).await;
            assert_eq!(other.status, StatusCode::OK, "{crs} {key}");
            assert_eq!(other.x_cache, "HIT", "{crs} {key}");
            assert_eq!(other.body, upper.body, "{crs} {key}");
        }
    }
}

/// A lossy and a lossless WebP of one view never share a rendered-cache
/// entry, on either render path; `QUALITY=100` is the lossless default.
#[tokio::test]
async fn lossy_and_lossless_webp_never_alias_in_the_cache() {
    for (crs, bbox) in QUALITY_VIEWS {
        let app = build_populated_router();
        let lossless = get_quality(&app, crs, bbox, "&FORMAT=image/webp").await;
        assert_eq!(lossless.x_cache, "MISS", "{crs}");
        assert_eq!(
            webp_chunk(&lossless.body),
            b"VP8L",
            "{crs}: default is lossless"
        );

        let lossy = get_quality(&app, crs, bbox, "&FORMAT=image/webp&QUALITY=80").await;
        assert_eq!(
            lossy.x_cache, "MISS",
            "{crs}: a lossless entry must not serve lossy"
        );
        assert_ne!(webp_chunk(&lossy.body), b"VP8L");
        assert_ne!(lossy.etag, lossless.etag);

        // Explicit 100 is the lossless request: served from its entry.
        let explicit = get_quality(&app, crs, bbox, "&FORMAT=image/webp&QUALITY=100").await;
        assert_eq!(explicit.x_cache, "HIT", "{crs}");
        assert_eq!(explicit.body, lossless.body);

        // Another lossy quality is another entry; the first lossy one hits.
        let q50 = get_quality(&app, crs, bbox, "&FORMAT=image/webp&QUALITY=50").await;
        assert_eq!(q50.x_cache, "MISS", "{crs}");
        let again = get_quality(&app, crs, bbox, "&FORMAT=image/webp&QUALITY=80").await;
        assert_eq!(again.x_cache, "HIT", "{crs}");
        assert_eq!(again.body, lossy.body);
    }
}

/// `[wms] webp_quality` applies to a WebP GetMap without QUALITY; an explicit
/// QUALITY wins, including 100 for lossless. JPEG keeps its own default.
#[tokio::test]
async fn collection_webp_quality_is_the_default_and_quality_overrides_it() {
    let wms: ds_core::config::WmsConfig =
        serde_json::from_value(serde_json::json!({ "webp_quality": 70 })).unwrap();
    for (crs, bbox) in QUALITY_VIEWS {
        let app = api_wms::router(build_populated_state_with_wms(
            Arc::new(PopulatedMockMapEngine),
            Some(wms.clone()),
        ));
        let plain = build_populated_router();

        let default = get_quality(&app, crs, bbox, "&FORMAT=image/webp").await;
        assert_eq!(default.status, StatusCode::OK);
        assert_ne!(
            webp_chunk(&default.body),
            b"VP8L",
            "{crs}: collection default is lossy"
        );
        // It is exactly QUALITY=70: same key, a cache hit.
        let seventy = get_quality(&app, crs, bbox, "&FORMAT=image/webp&QUALITY=70").await;
        assert_eq!(seventy.x_cache, "HIT", "{crs}");
        assert_eq!(seventy.body, default.body);

        let lossless = get_quality(&app, crs, bbox, "&FORMAT=image/webp&QUALITY=100").await;
        assert_eq!(lossless.x_cache, "MISS", "{crs}");
        assert_eq!(
            webp_chunk(&lossless.body),
            b"VP8L",
            "{crs}: explicit 100 is lossless"
        );
        // …the same bytes a collection without a default serves.
        let reference = get_quality(&plain, crs, bbox, "&FORMAT=image/webp").await;
        assert_eq!(lossless.body, reference.body, "{crs}");

        let q90 = get_quality(&app, crs, bbox, "&FORMAT=image/webp&QUALITY=90").await;
        assert_eq!(q90.x_cache, "MISS", "{crs}");
        assert_ne!(q90.body, default.body);

        // JPEG ignores the WebP default.
        let jpeg = get_quality(&app, crs, bbox, "&FORMAT=image/jpeg").await;
        let jpeg_plain = get_quality(&plain, crs, bbox, "&FORMAT=image/jpeg").await;
        assert_eq!(jpeg.content_type, "image/jpeg");
        assert_eq!(jpeg.body, jpeg_plain.body, "{crs}");
    }
}

/// JPEG takes QUALITY as its quality factor (default 85).
#[tokio::test]
async fn quality_sets_the_jpeg_quality() {
    let app = build_populated_router();
    let (crs, bbox) = QUALITY_VIEWS[0];
    let default = get_quality(&app, crs, bbox, "&FORMAT=image/jpeg").await;
    let q85 = get_quality(&app, crs, bbox, "&FORMAT=image/jpeg&QUALITY=85").await;
    assert_eq!(q85.x_cache, "HIT", "85 is the JPEG default");
    assert_eq!(q85.body, default.body);
    let q20 = get_quality(&app, crs, bbox, "&FORMAT=image/jpeg&QUALITY=20").await;
    assert_eq!(q20.status, StatusCode::OK);
    assert_eq!(q20.x_cache, "MISS");
    assert!(q20.body.len() < default.body.len());
}

/// A bad QUALITY, or one on PNG, is an `InvalidParameterValue` 400 naming
/// the problem, never silently ignored.
#[tokio::test]
async fn bad_quality_is_invalid_parameter_value() {
    let app = build_populated_router();
    let (crs, bbox) = QUALITY_VIEWS[0];
    for (extra, message) in [
        (
            "&FORMAT=image/webp&QUALITY=0",
            "QUALITY &apos;0&apos; must be an integer from 1 to 100",
        ),
        (
            "&FORMAT=image/webp&QUALITY=101",
            "QUALITY &apos;101&apos; must be an integer from 1 to 100",
        ),
        (
            "&FORMAT=image/jpeg&QUALITY=high",
            "QUALITY &apos;high&apos; must be an integer from 1 to 100",
        ),
        (
            "&FORMAT=image/png&QUALITY=80",
            "QUALITY applies only to image/jpeg and image/webp, not image/png",
        ),
    ] {
        let resp = get_quality(&app, crs, bbox, extra).await;
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{extra}");
        let body = String::from_utf8(resp.body).unwrap();
        assert!(
            body.contains("code=\"InvalidParameterValue\"") && body.contains(message),
            "{extra}: {body}"
        );
    }
}

/// QUALITY is a GetMap parameter; GetLegendGraphic ignores it like the
/// other GetMap-only parameters, so a client sending it everywhere works.
#[tokio::test]
async fn legend_graphic_ignores_quality() {
    let app = build_populated_router();
    let resp = app
        .oneshot(
            Request::builder()
                .uri(
                    "/?SERVICE=WMS&REQUEST=GetLegendGraphic&VERSION=1.3.0\
                     &LAYER=radar&FORMAT=image/webp&QUALITY=80",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(webp_chunk(&body), b"VP8L", "legends stay lossless");
}
