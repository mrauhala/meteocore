use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;

use ds_core::config::CollectionConfig;
use ds_core::map_engine::MapEngine;
use ds_render::{CacheKey, RenderedCache, StyleInfo};

use crate::error::WmsError;
use crate::params::{WmsQuery, WmsRequestType};

#[derive(Clone)]
pub struct WmsState {
    pub engines: HashMap<String, Arc<dyn MapEngine>>,
    pub collections: HashMap<String, CollectionConfig>,
    /// Map of layer → style name → StyleInfo. Every layer has at least "default".
    pub styles: HashMap<String, HashMap<String, StyleInfo>>,
    pub render_semaphore: Arc<tokio::sync::Semaphore>,
    pub rendered_cache: Arc<RenderedCache>,
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

            // Validate `LAYERS=collection/parameter` against the engine's
            // advertised list (mirroring Maps + Tiles). Without this, an
            // unknown parameter would silently render whatever the engine
            // defaults to and cache that result under the invalid name —
            // ServiceException is the correct OGC response here.
            if let Some(pname) = layer_parameter.as_deref() {
                let info = engine.raster_info();
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

            let colormap = style_info.colormap.clone();
            let content_type = params.format.content_type();
            let has_explicit_time = params.time.is_some();

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
                bbox: ds_render::quantize_bbox(&params.bbox),
                width: params.width,
                height: params.height,
                time: params.time,
                // WMS picks the parameter via `LAYERS=collection/param`
                // (parsed into layer_parameter) and then `style_info.parameter`.
                // Both are already folded into `style_parameter` below; mirror
                // that here so the rendered-cache distinguishes parameters.
                parameter: layer_parameter
                    .clone()
                    .or_else(|| style_info.parameter.clone()),
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
            let _permit =
                tokio::time::timeout(ds_render::RENDER_TIMEOUT, state.render_semaphore.acquire())
                    .await
                    .map_err(|_| {
                        WmsError::ServiceUnavailable("Server busy, try again later".to_string())
                    })?
                    .map_err(|_| WmsError::Internal("Render semaphore closed".to_string()))?;

            // Render on a blocking thread
            let engine = engine.clone();
            let bbox = params.bbox;
            let width = params.width;
            let height = params.height;
            let time = params.time;
            let output_crs = params.output_crs;
            let format = params.format;
            let rendered_cache = state.rendered_cache.clone();

            // Layer parameter (from "collection/param") takes priority over style parameter
            let style_parameter =
                layer_parameter.or_else(|| style_info.parameter.as_deref().map(String::from));

            let render_result = tokio::task::spawn_blocking(move || {
                let tile = engine.get_raster_tile(
                    bbox,
                    width,
                    height,
                    time,
                    &output_crs,
                    style_parameter.as_deref(),
                )?;
                // If every pixel is nodata, skip colorization + encoding entirely.
                if tile.is_empty() {
                    return Ok(None);
                }
                ds_render::render_tile(&tile, colormap.as_ref(), format).map(Some)
            })
            .await
            .map_err(|e| WmsError::Internal(format!("Render task failed: {e}")))?;

            // The EMPTY and ERROR fast paths skip the format-aware encoder and
            // emit PNG bytes directly. Track the *actual* Content-Type per
            // branch so the header never lies about the payload (#162). Wrap
            // every branch in `CachedRendered` so the response ETag is
            // FNV-1a over the actual bytes — different pixels, different
            // ETag — regardless of which exit we take (#145).
            let (cached, x_cache, response_content_type, insert_into_cache) = match render_result {
                Ok(Some(bytes)) => {
                    let cached = ds_render::CachedRendered::new(bytes::Bytes::from(bytes));
                    (cached, "MISS", content_type, true)
                }
                Ok(None) => {
                    // Empty tile: transparent PNG, never cached.
                    let rgba = vec![0u8; (params.width * params.height * 4) as usize];
                    let png =
                        ds_render::encode_png(&rgba, params.width, params.height).map_err(|e| {
                            WmsError::Internal(format!("Failed to encode empty tile: {e}"))
                        })?;
                    let cached = ds_render::CachedRendered::new(bytes::Bytes::from(png));
                    (cached, "EMPTY", "image/png", false)
                }
                Err(e) => {
                    tracing::warn!("WMS render error for layer '{}': {e}", params.layer);
                    let png = render_error_tile(params.width, params.height)?;
                    let cached = ds_render::CachedRendered::new(bytes::Bytes::from(png));
                    (cached, "ERROR", "image/png", false)
                }
            };

            if insert_into_cache {
                rendered_cache.insert(cache_key, cached.clone());
            }

            // Now that we have the content-derived ETag, do the
            // `If-None-Match` comparison. Same flow as `render_vector_tile`
            // in api-tiles: cache lookup → revalidate against cached ETag,
            // miss → encode → revalidate against fresh ETag.
            if let Some(ref inm) = if_none_match {
                if ds_render::etag_matches(inm, cached.etag()) {
                    // `x-cache` is *intentionally* absent here. The HIT-path
                    // 304 above emits `x-cache: HIT`; the absence on this
                    // post-render path is what lets clients and the
                    // regression test (`if_none_match_after_cache_warm_...`)
                    // distinguish the two branches. Adding `x-cache: MISS`
                    // here would silently break that invariant.
                    return Ok(axum::response::Response::builder()
                        .status(StatusCode::NOT_MODIFIED)
                        .header(header::ETAG, cached.etag())
                        .header(header::CACHE_CONTROL, cache_control)
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
