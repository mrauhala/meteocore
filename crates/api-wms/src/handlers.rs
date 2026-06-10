use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;

use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs};
use ds_render::{CacheKey, RenderedCache, StyleInfo};

use crate::error::WmsError;
use crate::params::{WmsQuery, WmsRequestType};

/// Log a phase breakdown for any GetMap render at or above this wall-clock time,
/// so production tells us where a slow render's time actually goes (queue wait
/// vs tile render vs assemble vs encode). Diagnostic for the cold-render tail.
const SLOW_RENDER_LOG_MS: u64 = 400;

/// Which path a GetMap render took, for the slow-render diagnostic log. Keeps the
/// four cases distinct (a meta render, an all-nodata meta render, a meta render
/// that *fell back* to direct, and a genuine non-meta render) rather than
/// conflating the last three under one "direct" label.
enum RenderPath {
    /// Web Mercator meta-tiling, with per-phase stats.
    Meta(ds_render::MetaTileStats),
    /// Meta-tiling path, every covered pixel nodata — carries the tile-loop
    /// timing (assemble/encode skipped).
    MetaEmpty(ds_render::MetaTileStats),
    /// Meta-tiling declined (degenerate / >MAX_TILES / extreme zoom) → direct.
    Fallback,
    /// Genuine non-meta path (non-3857 CRS, or meta cache disabled).
    Direct,
}

#[derive(Clone)]
pub struct WmsState {
    pub engines: HashMap<String, Arc<dyn MapEngine>>,
    pub collections: HashMap<String, CollectionConfig>,
    /// Map of layer → style name → StyleInfo. Every layer has at least "default".
    pub styles: HashMap<String, HashMap<String, StyleInfo>>,
    pub render_semaphore: Arc<tokio::sync::Semaphore>,
    pub rendered_cache: Arc<RenderedCache>,
    /// Decoded-RGBA meta-tile cache for the Web Mercator GetMap path (#202).
    pub tile_cache: Arc<ds_render::TilePixelCache>,
    pub base_url: String,
}

pub type AppState = Arc<ArcSwap<WmsState>>;

/// Render a semi-transparent red error tile to make failed areas visible.
fn render_error_tile(width: u32, height: u32) -> Result<Vec<u8>, WmsError> {
    let pixel_count = (width * height) as usize;
    let mut rgba = Vec::with_capacity(pixel_count * 4);
    for _ in 0..pixel_count {
        rgba.extend_from_slice(&[255, 0, 0, 100]);
    }
    ds_render::encode_png(&rgba, width, height)
        .map_err(|e| WmsError::Internal(format!("Failed to encode error tile: {e}")))
}

/// Cache-Control header value for a WMS response.
///
/// - Requests with explicit TIME: immutable data, cache for 24 hours
/// - Requests without TIME (latest): short cache (60s) since "latest" changes
fn cache_control_value(has_explicit_time: bool) -> &'static str {
    if has_explicit_time {
        "public, max-age=86400, immutable"
    } else {
        "public, max-age=60, must-revalidate"
    }
}

/// Main WMS handler — dispatches on REQUEST parameter.
pub async fn wms_handler(
    headers: HeaderMap,
    Query(query): Query<WmsQuery>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, WmsError> {
    let state = state.load_full();

    match query.request_type()? {
        WmsRequestType::GetCapabilities => {
            let xml = crate::capabilities::get_capabilities_xml(
                &state.engines,
                &state.collections,
                &state.styles,
                &state.base_url,
            );
            Ok((
                [
                    (header::CONTENT_TYPE, "text/xml"),
                    (
                        header::HeaderName::from_static("x-content-type-options"),
                        "nosniff",
                    ),
                ],
                xml,
            )
                .into_response())
        }
        WmsRequestType::GetMap => {
            let params = query.validate_get_map()?;

            // Parse layer name: "collection-id" or "collection-id/parameter"
            let (collection_id, layer_parameter) =
                if let Some((cid, param)) = params.layer.split_once('/') {
                    (cid.to_string(), Some(param.to_string()))
                } else {
                    (params.layer.clone(), None)
                };

            // Look up engine by collection ID
            let engine = state
                .engines
                .get(&collection_id)
                .ok_or_else(|| WmsError::layer_not_found(&params.layer))?;

            // Look up style: try full layer name first (e.g., "ecmwf-kenya/2t" for
            // per-parameter defaults), then fall back to collection ID
            let layer_styles = state
                .styles
                .get(&params.layer)
                .or_else(|| state.styles.get(&collection_id))
                .ok_or_else(|| WmsError::layer_not_found(&params.layer))?;

            let style_info = layer_styles.get(&params.style).ok_or_else(|| {
                WmsError::StyleNotDefined(format!(
                    "Style '{}' not defined for layer '{}'. Available: {}",
                    params.style,
                    params.layer,
                    layer_styles.keys().cloned().collect::<Vec<_>>().join(", ")
                ))
            })?;

            // One metadata snapshot for all parameter/dimension validation
            // below. `raster_info()` clones its vecs, so take it once rather
            // than once per check (parameter, ELEVATION, reference_time).
            let info = engine.raster_info();

            // Validate `LAYERS=collection/parameter` against the engine's
            // advertised list (mirroring Maps + Tiles). Without this, an
            // unknown parameter would silently render whatever the engine
            // defaults to and cache that result under the invalid name —
            // ServiceException is the correct OGC response here.
            if let Some(pname) = layer_parameter.as_deref() {
                if !info.parameters.is_empty()
                    && !info.parameters.iter().any(|(name, _)| name == pname)
                {
                    let mut supported: Vec<&str> =
                        info.parameters.iter().map(|(n, _)| n.as_str()).collect();
                    supported.sort_unstable();
                    return Err(WmsError::LayerNotDefined(format!(
                        "Parameter '{pname}' is not available for layer \
                         '{collection_id}'. Available: {}",
                        supported.join(", ")
                    )));
                }
            }

            // Reject an `ELEVATION` against a layer with no vertical axis.
            if params.elevation.is_some() && info.vertical.is_none() {
                return Err(WmsError::invalid_parameter(&format!(
                    "Layer '{collection_id}' has no ELEVATION dimension"
                )));
            }

            // Validate `DIM_REFERENCE_TIME` against the layer's advertised model
            // runs. The engine requires an exact run match (`select_run` →
            // `ReferenceTimeNotFound`, which the GetMap render path would turn
            // into a red 200 tile); surfacing `InvalidDimensionValue` here is the
            // correct WMS response — mirroring the parameter/ELEVATION checks.
            if let Some(rt) = params.reference_time {
                if info.reference_times.is_empty() {
                    return Err(WmsError::InvalidDimensionValue(format!(
                        "Layer '{collection_id}' has no reference_time dimension"
                    )));
                }
                if !info.reference_times.contains(&rt) {
                    return Err(WmsError::InvalidDimensionValue(format!(
                        "reference_time '{}' is not an available model run for layer \
                         '{collection_id}'",
                        rt.to_rfc3339()
                    )));
                }
            }

            let colormap = style_info.colormap.clone();
            let content_type = params.format.content_type();
            let has_explicit_time = params.time.is_some();

            // Normalise an explicit pin of the *current* latest run to `None`, so
            // it shares cache entries (and the engine's latest-run path) with
            // requests that omit the dimension — they render identical pixels.
            // The common client flow is echoing the GetCapabilities `default=`
            // (= the latest run), so without this those requests fragment the
            // cache from the no-dimension ones. A pin of an *older* run stays
            // explicit. (`info.reference_times` is ascending; latest is `.last()`.)
            let reference_time = params
                .reference_time
                .filter(|&rt| info.reference_times.last().copied() != Some(rt));

            // Resolve a TIME-less request to the engine's *current* latest
            // timestamp before any cache key is built. The rendered + meta-tile
            // caches have no TTL, so keying "latest" as `None` would freeze the
            // first rendered frame (and its ETag) forever while the engine's
            // catalog moves on — a TIME-less layer must track new data. Mirrors
            // Maps/Tiles, which resolve latest the same way before keying.
            // `info.times` is ascending; when it's empty (e.g. STAC cold start)
            // the request falls through as `None` = the engine's own latest.
            let time = params.time.or_else(|| info.times.last().copied());

            // Build cache key
            let cache_key = CacheKey {
                layer: params.layer.clone(),
                style: params.style.clone(),
                format: match params.format {
                    ds_render::ImageFormat::Png => 0,
                    ds_render::ImageFormat::Jpeg => 1,
                    ds_render::ImageFormat::Webp => 2,
                },
                crs: params.crs.clone(),
                // For a projected output CRS the rendered pixels are laid out
                // over the projected-metres bbox (carried in `output_crs`), not
                // the WGS84 envelope in `params.bbox` — two requests with
                // different projected bboxes can share an envelope, so key on the
                // metres to avoid serving one's tile for the other (#267 review).
                bbox: match &params.output_crs {
                    OutputCrs::Projected { bbox, .. } => ds_render::quantize_bbox(bbox),
                    _ => ds_render::quantize_bbox(&params.bbox),
                },
                width: params.width,
                height: params.height,
                time,
                // WMS picks the parameter via `LAYERS=collection/param`
                // (parsed into layer_parameter) and then `style_info.parameter`.
                // Both are already folded into `style_parameter` below; mirror
                // that here so the rendered-cache distinguishes parameters.
                parameter: layer_parameter
                    .clone()
                    .or_else(|| style_info.parameter.clone()),
                z: params.elevation.map(ds_render::quantize_z),
                // The forecast run pinned via the `reference_time` dimension
                // (None ⇒ latest), so runs don't collide in the rendered cache.
                reference_time,
            };

            let cache_control = cache_control_value(has_explicit_time);
            // Read If-None-Match into an owned String so it survives the move
            // into spawn_blocking and the cache-hit/miss branches below.
            let if_none_match = headers
                .get(header::IF_NONE_MATCH)
                .and_then(|h| h.to_str().ok())
                .map(str::to_string);

            // Cache lookup runs BEFORE the If-None-Match check. The ETag is
            // content-derived (see `CachedRendered::new`), so a key-derived
            // 304 short-circuit would be wrong: it would return 304 for any
            // request matching the cache key, even after a server-side fix
            // produces different pixels. Mirror the MVT path in
            // `render_vector_tile` (the bug #145 fixed for raster tiles).
            if let Some(cached) = state.rendered_cache.get(&cache_key) {
                if let Some(ref inm) = if_none_match {
                    if ds_render::etag_matches(inm, cached.etag()) {
                        // 304 from the cache-HIT branch. The `x-cache: HIT`
                        // header lets the regression test (and curious
                        // clients) distinguish this from a post-render
                        // MISS→304, which the handler also serves.
                        return Ok(axum::response::Response::builder()
                            .status(StatusCode::NOT_MODIFIED)
                            .header(header::ETAG, cached.etag())
                            .header(header::CACHE_CONTROL, cache_control)
                            .header(header::HeaderName::from_static("x-cache"), "HIT")
                            .body(axum::body::Body::empty())
                            .unwrap()
                            .into_response());
                    }
                }
                return Ok(axum::response::Response::builder()
                    .header(header::CONTENT_TYPE, content_type)
                    .header(header::ETAG, cached.etag())
                    .header(header::CACHE_CONTROL, cache_control)
                    .header(
                        header::HeaderName::from_static("x-content-type-options"),
                        "nosniff",
                    )
                    .header(header::HeaderName::from_static("x-cache"), "HIT")
                    .body(axum::body::Body::from(cached.into_bytes()))
                    .unwrap()
                    .into_response());
            }

            // Acquire render semaphore (with timeout to shed load under pressure)
            let t_sem = std::time::Instant::now();
            let _permit =
                tokio::time::timeout(ds_render::RENDER_TIMEOUT, state.render_semaphore.acquire())
                    .await
                    .map_err(|_| {
                        WmsError::ServiceUnavailable("Server busy, try again later".to_string())
                    })?
                    .map_err(|_| WmsError::Internal("Render semaphore closed".to_string()))?;
            let sem_wait_ms = t_sem.elapsed().as_millis() as u64;

            // Render on a blocking thread
            let engine = engine.clone();
            let bbox = params.bbox;
            let width = params.width;
            let height = params.height;
            // `time` (latest-resolved above) is `Copy`; it flows into both the
            // direct and meta-tile render closures below.
            let output_crs = params.output_crs.clone();
            let format = params.format;
            let elevation = params.elevation;
            // `reference_time` (normalised above) is `Copy`; it flows into both
            // the direct and meta-tile render closures below.
            let z_q = elevation.map(ds_render::quantize_z);
            let layer = params.layer.clone();
            // Key meta-tiles on the *resolved* style name, not the raw STYLES
            // param: `STYLES=` (empty → default) and `STYLES=default` resolve to
            // the same StyleInfo, so they must share cached tiles.
            let style = style_info.name.clone();
            let rendered_cache = state.rendered_cache.clone();
            let tile_cache = state.tile_cache.clone();

            // Layer parameter (from "collection/param") takes priority over style parameter
            let style_parameter =
                layer_parameter.or_else(|| style_info.parameter.as_deref().map(String::from));

            // Spans spawn_blocking *dispatch* + execution, so `render_ms` includes
            // any wait for a free blocking-pool thread (itself a useful signal: if
            // render_ms greatly exceeds the internal phase sum
            // tile_render_ms+assemble_ms+encode_ms, the gap is scheduling latency).
            let t_render = std::time::Instant::now();
            let render_outcome = tokio::task::spawn_blocking(
                move || -> Result<(Option<Vec<u8>>, RenderPath), DataServerError> {
                    // Direct single-shot render: one get_raster_tile → colorize → encode.
                    let direct = || -> Result<Option<Vec<u8>>, DataServerError> {
                        let tile = engine.get_raster_tile(
                            bbox,
                            width,
                            height,
                            time,
                            &output_crs,
                            style_parameter.as_deref(),
                            elevation,
                            reference_time,
                        )?;
                        // If every pixel is nodata, skip colorization + encoding entirely.
                        if tile.is_empty() {
                            return Ok(None);
                        }
                        ds_render::render_tile(&tile, colormap.as_ref(), format).map(Some)
                    };

                    // Web Mercator: decompose into cached 256×256 meta-tiles and
                    // resample to the exact viewport (#202). The expensive
                    // per-tile work is cached and reused across overlapping
                    // fullscreen views; other CRSs render directly. A zero-byte
                    // tile cache (`metatile_cache_mb = 0`) is the kill switch:
                    // it bypasses meta-tiling so an operator can revert to the
                    // direct path via config reload, no redeploy.
                    if output_crs == OutputCrs::WebMercator && tile_cache.capacity() > 0 {
                        let prefix = ds_render::TileKeyPrefix {
                            layer,
                            parameter: style_parameter.clone(),
                            style,
                            time,
                            z: z_q,
                            reference_time,
                        };
                        // `bbox` is in WGS84 degrees here — the params layer
                        // converts EPSG:3857 metres to degrees before this point;
                        // render_metatiled re-projects back to metres internally.
                        let outcome = ds_render::render_metatiled(
                            bbox,
                            width,
                            height,
                            &prefix,
                            colormap.as_ref(),
                            format,
                            tile_cache.as_ref(),
                            |tbbox, tw, th| {
                                engine.get_raster_tile(
                                    tbbox,
                                    tw,
                                    th,
                                    time,
                                    &OutputCrs::WebMercator,
                                    style_parameter.as_deref(),
                                    elevation,
                                    reference_time,
                                )
                            },
                        )?;
                        match outcome {
                            ds_render::MetaTile::Image { bytes, stats } => {
                                Ok((Some(bytes), RenderPath::Meta(stats)))
                            }
                            ds_render::MetaTile::Empty { stats } => {
                                Ok((None, RenderPath::MetaEmpty(stats)))
                            }
                            ds_render::MetaTile::Fallback => {
                                direct().map(|o| (o, RenderPath::Fallback))
                            }
                        }
                    } else {
                        direct().map(|o| (o, RenderPath::Direct))
                    }
                },
            )
            .await
            .map_err(|e| WmsError::Internal(format!("Render task failed: {e}")))?;
            let render_ms = t_render.elapsed().as_millis() as u64;

            // Split the render outcome: bytes flow into the existing response
            // match below; the path + timing are logged for slow renders so prod
            // pinpoints the cost (queue wait vs tile render vs assemble vs encode).
            // `render_path` is irrelevant on error (the log is gated on success).
            // `render_path` is `None` on error (no fabricated placeholder); the
            // slow-log is gated on success anyway, so it's never read on error.
            let (render_result, render_path): (
                Result<Option<Vec<u8>>, DataServerError>,
                Option<RenderPath>,
            ) = match render_outcome {
                Ok((bytes, path)) => (Ok(bytes), Some(path)),
                Err(e) => (Err(e), None),
            };
            // Only log *successful* slow renders (the 200-status tail we're
            // diagnosing); errors are surfaced by the WmsError render warn arm
            // below. The arms stay distinct so a meta render that fell back to
            // direct isn't conflated with a genuine non-meta render.
            if render_ms >= SLOW_RENDER_LOG_MS && render_result.is_ok() {
                match render_path {
                    Some(RenderPath::Meta(s)) => tracing::info!(
                        layer = %params.layer,
                        sem_wait_ms,
                        render_ms,
                        tiles = s.tiles,
                        misses = s.misses,
                        tile_loop_ms = s.tile_loop_ms,
                        assemble_ms = s.assemble_ms,
                        encode_ms = s.encode_ms,
                        width = params.width,
                        height = params.height,
                        "slow WMS meta-tile render"
                    ),
                    Some(RenderPath::MetaEmpty(s)) => tracing::info!(
                        layer = %params.layer,
                        sem_wait_ms,
                        render_ms,
                        tiles = s.tiles,
                        misses = s.misses,
                        tile_loop_ms = s.tile_loop_ms,
                        width = params.width,
                        height = params.height,
                        "slow WMS meta-tile render (all nodata)"
                    ),
                    Some(RenderPath::Fallback) => tracing::info!(
                        layer = %params.layer,
                        sem_wait_ms,
                        render_ms,
                        width = params.width,
                        height = params.height,
                        "slow WMS render (meta-tiling fell back to direct)"
                    ),
                    // `Direct` covers both a non-Web-Mercator CRS and a Web
                    // Mercator request with meta-tiling disabled (metatile_cache_mb
                    // = 0), so the label stays generic rather than claiming a CRS.
                    Some(RenderPath::Direct) => tracing::info!(
                        layer = %params.layer,
                        sem_wait_ms,
                        render_ms,
                        width = params.width,
                        height = params.height,
                        "slow WMS direct render"
                    ),
                    None => {}
                }
            }

            // The EMPTY and ERROR fast paths skip the format-aware encoder and
            // emit PNG bytes directly. Track the *actual* Content-Type per
            // branch so the header never lies about the payload (#162). Wrap
            // every branch in `CachedRendered` so the response ETag is
            // FNV-1a over the actual bytes — different pixels, different
            // ETag — regardless of which exit we take (#145).
            // Each arm produces a `CachedRendered` ready to serve. Only the
            // populated `Ok(Some(_))` path inserts into the rendered cache;
            // the EMPTY and ERROR fast-paths intentionally don't (their
            // bytes are deterministic for fixed dimensions and the
            // engine error case shouldn't poison the cache).
            let (cached, x_cache, response_content_type) = match render_result {
                Ok(Some(bytes)) => {
                    let cached = ds_render::CachedRendered::new(bytes::Bytes::from(bytes));
                    rendered_cache.insert(cache_key, cached.clone());
                    (cached, "MISS", content_type)
                }
                Ok(None) => {
                    // Empty tile: transparent PNG, never cached.
                    let rgba = vec![0u8; (params.width * params.height * 4) as usize];
                    let png =
                        ds_render::encode_png(&rgba, params.width, params.height).map_err(|e| {
                            WmsError::Internal(format!("Failed to encode empty tile: {e}"))
                        })?;
                    let cached = ds_render::CachedRendered::new(bytes::Bytes::from(png));
                    (cached, "EMPTY", "image/png")
                }
                Err(e) => {
                    tracing::warn!("WMS render error for layer '{}': {e}", params.layer);
                    let png = render_error_tile(params.width, params.height)?;
                    let cached = ds_render::CachedRendered::new(bytes::Bytes::from(png));
                    (cached, "ERROR", "image/png")
                }
            };

            // Now that we have the content-derived ETag, do the
            // `If-None-Match` comparison. Same flow as `render_vector_tile`
            // in api-tiles: cache lookup → revalidate against cached ETag,
            // miss → encode → revalidate against fresh ETag.
            if let Some(ref inm) = if_none_match {
                if ds_render::etag_matches(inm, cached.etag()) {
                    // 304 from the post-render branch. Forward the same
                    // `x_cache` label the 200 response would carry — `"MISS"`,
                    // `"EMPTY"`, or `"ERROR"` — so revalidations look the
                    // same on dashboards as initial fetches. A client
                    // revalidating a cached transparent-tile response sees
                    // `304 x-cache: EMPTY`, not a misleading `MISS`.
                    return Ok(axum::response::Response::builder()
                        .status(StatusCode::NOT_MODIFIED)
                        .header(header::ETAG, cached.etag())
                        .header(header::CACHE_CONTROL, cache_control)
                        .header(header::HeaderName::from_static("x-cache"), x_cache)
                        .body(axum::body::Body::empty())
                        .unwrap()
                        .into_response());
                }
            }

            Ok(axum::response::Response::builder()
                .header(header::CONTENT_TYPE, response_content_type)
                .header(header::ETAG, cached.etag())
                .header(header::CACHE_CONTROL, cache_control)
                .header(
                    header::HeaderName::from_static("x-content-type-options"),
                    "nosniff",
                )
                .header(header::HeaderName::from_static("x-cache"), x_cache)
                .body(axum::body::Body::from(cached.into_bytes()))
                .unwrap()
                .into_response())
        }
        WmsRequestType::GetLegendGraphic => {
            let layer_name = query
                .layers
                .as_deref()
                .or(query.layer.as_deref())
                .ok_or(WmsError::missing_parameter("LAYER"))?;

            let style_name = query.styles.as_deref().unwrap_or("default");
            let style_name = if style_name.is_empty() {
                "default"
            } else {
                style_name
            };

            // Support "collection/parameter" layer names for legend
            let legend_collection_id = layer_name.split('/').next().unwrap_or(layer_name);
            let layer_styles = state
                .styles
                .get(legend_collection_id)
                .ok_or_else(|| WmsError::layer_not_found(layer_name))?;

            let style_info = layer_styles.get(style_name).ok_or_else(|| {
                WmsError::StyleNotDefined(format!(
                    "Style '{style_name}' not defined for layer '{layer_name}'"
                ))
            })?;

            let width: u32 = query
                .width
                .as_deref()
                .and_then(|s| s.parse().ok())
                .unwrap_or(40);
            let height: u32 = query
                .height
                .as_deref()
                .and_then(|s| s.parse().ok())
                .unwrap_or(200);
            let width = width.min(256);
            let height = height.min(1024);

            let format = crate::params::parse_image_format(query.format.as_deref())?;

            let colormap = style_info.colormap.clone();
            let min = style_info.min;
            let max = style_info.max;

            let legend_bytes = tokio::task::spawn_blocking(move || {
                ds_render::render_legend(colormap.as_ref(), min, max, width, height, format)
            })
            .await
            .map_err(|e| WmsError::Internal(format!("Legend render failed: {e}")))?
            .map_err(|e| WmsError::Internal(format!("Legend render error: {e}")))?;

            Ok((
                [
                    (header::CONTENT_TYPE, format.content_type()),
                    (
                        header::HeaderName::from_static("x-content-type-options"),
                        "nosniff",
                    ),
                    // Legends are static — cache for 24h
                    (header::CACHE_CONTROL, "public, max-age=86400, immutable"),
                ],
                legend_bytes,
            )
                .into_response())
        }
    }
}
