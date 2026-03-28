use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::extract::{Query, State};
use axum::http::header;
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

            // Look up engine
            let engine = state
                .engines
                .get(&params.layer)
                .ok_or_else(|| WmsError::layer_not_found(&params.layer))?;

            // Look up style
            let layer_styles = state
                .styles
                .get(&params.layer)
                .ok_or_else(|| WmsError::layer_not_found(&params.layer))?;

            let style_info = layer_styles.get(&params.style).ok_or_else(|| {
                WmsError::StyleNotDefined(format!(
                    "Style '{}' not defined for layer '{}'. Available: {}",
                    params.style,
                    params.layer,
                    layer_styles.keys().cloned().collect::<Vec<_>>().join(", ")
                ))
            })?;

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
            };

            let etag = cache_key.etag();
            let cache_control = cache_control_value(has_explicit_time);

            // Check rendered cache
            if let Some(cached) = state.rendered_cache.get(&cache_key) {
                return Ok(axum::response::Response::builder()
                    .header(header::CONTENT_TYPE, content_type)
                    .header(header::ETAG, &etag)
                    .header(header::CACHE_CONTROL, cache_control)
                    .header(
                        header::HeaderName::from_static("x-content-type-options"),
                        "nosniff",
                    )
                    .header(header::HeaderName::from_static("x-cache"), "HIT")
                    .body(axum::body::Body::from(cached.as_ref().clone()))
                    .unwrap()
                    .into_response());
            }

            // Acquire render semaphore
            let _permit = state.render_semaphore.try_acquire().map_err(|_| {
                WmsError::Internal("Server busy: too many concurrent render requests".to_string())
            })?;

            // Render on a blocking thread
            let engine = engine.clone();
            let bbox = params.bbox;
            let width = params.width;
            let height = params.height;
            let time = params.time;
            let output_crs = params.output_crs;
            let format = params.format;
            let rendered_cache = state.rendered_cache.clone();

            let render_result = tokio::task::spawn_blocking(move || {
                let tile = engine.get_raster_tile(bbox, width, height, time, &output_crs)?;
                ds_render::render_tile(&tile, colormap.as_ref(), format)
            })
            .await
            .map_err(|e| WmsError::Internal(format!("Render task failed: {e}")))?;

            let (image_bytes, cacheable) = match render_result {
                Ok(bytes) => (bytes, true),
                Err(e) => {
                    tracing::warn!("WMS render error for layer '{}': {e}", params.layer);
                    (render_error_tile(params.width, params.height)?, false)
                }
            };

            let image_arc = Arc::new(image_bytes);
            if cacheable {
                rendered_cache.insert(cache_key, image_arc.clone());
            }

            Ok(axum::response::Response::builder()
                .header(header::CONTENT_TYPE, content_type)
                .header(header::ETAG, &etag)
                .header(header::CACHE_CONTROL, cache_control)
                .header(
                    header::HeaderName::from_static("x-content-type-options"),
                    "nosniff",
                )
                .header(
                    header::HeaderName::from_static("x-cache"),
                    if cacheable { "MISS" } else { "ERROR" },
                )
                .body(axum::body::Body::from(image_arc.as_ref().clone()))
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

            let layer_styles = state
                .styles
                .get(layer_name)
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

            let format = match query.format.as_deref() {
                Some("image/jpeg") => ds_render::ImageFormat::Jpeg,
                _ => ds_render::ImageFormat::Png,
            };

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
