use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use quick_cache::sync::Cache;

use ds_core::config::CollectionConfig;
use ds_core::map_engine::MapEngine;
use ds_render::ColorMap;

use crate::error::WmsError;
use crate::params::{WmsQuery, WmsRequestType};

/// How long rendered tiles stay valid in the cache.
const CACHE_TTL_SECS: u64 = 300; // 5 minutes

/// A named style with its colormap and value range.
#[derive(Clone)]
pub struct StyleInfo {
    pub name: String,
    pub title: String,
    pub colormap: Arc<dyn ColorMap>,
    pub min: f64,
    pub max: f64,
}

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

/// Cache for rendered map images (Tier 2).
/// Keys are quantized to improve hit rates for tiled clients.
/// Entries expire after `CACHE_TTL_SECS` to allow recovery from transient S3 failures.
pub struct RenderedCache {
    cache: Cache<CacheKey, CacheEntry>,
}

#[derive(Clone)]
struct CacheEntry {
    data: Arc<Vec<u8>>,
    created: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CacheKey {
    layer: String,
    style: String,
    format: u8, // 0=png, 1=jpeg
    crs: String,
    bbox: [i64; 4], // bbox quantized to microdegrees (6 decimal places)
    width: u32,
    height: u32,
    time: Option<DateTime<Utc>>,
}

impl RenderedCache {
    pub fn new(capacity_mb: u64) -> Self {
        // Estimate ~60KB per tile, capacity in items
        let estimated_tile_size = 60 * 1024;
        let capacity = if capacity_mb == 0 {
            0
        } else {
            ((capacity_mb * 1024 * 1024) / estimated_tile_size).max(1) as usize
        };
        Self {
            cache: Cache::new(capacity),
        }
    }

    fn get(&self, key: &CacheKey) -> Option<Arc<Vec<u8>>> {
        let entry = self.cache.get(key)?;
        if entry.created.elapsed().as_secs() > CACHE_TTL_SECS {
            return None; // expired
        }
        Some(entry.data)
    }

    fn insert(&self, key: CacheKey, value: Arc<Vec<u8>>) {
        self.cache.insert(
            key,
            CacheEntry {
                data: value,
                created: Instant::now(),
            },
        );
    }
}

/// Render a semi-transparent red error tile to make failed areas visible.
fn render_error_tile(width: u32, height: u32) -> Result<Vec<u8>, WmsError> {
    let pixel_count = (width * height) as usize;
    let mut rgba = Vec::with_capacity(pixel_count * 4);
    // Semi-transparent red: RGBA(255, 0, 0, 100)
    for _ in 0..pixel_count {
        rgba.extend_from_slice(&[255, 0, 0, 100]);
    }
    ds_render::encode_png(&rgba, width, height)
        .map_err(|e| WmsError::Internal(format!("Failed to encode error tile: {e}")))
}

fn quantize_bbox(bbox: &[f64; 4]) -> [i64; 4] {
    [
        (bbox[0] * 1_000_000.0).round() as i64,
        (bbox[1] * 1_000_000.0).round() as i64,
        (bbox[2] * 1_000_000.0).round() as i64,
        (bbox[3] * 1_000_000.0).round() as i64,
    ]
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

            // Check rendered cache
            let cache_key = CacheKey {
                layer: params.layer.clone(),
                style: params.style.clone(),
                format: match params.format {
                    ds_render::ImageFormat::Png => 0,
                    ds_render::ImageFormat::Jpeg => 1,
                },
                crs: params.crs.clone(),
                bbox: quantize_bbox(&params.bbox),
                width: params.width,
                height: params.height,
                time: params.time,
            };

            if let Some(cached) = state.rendered_cache.get(&cache_key) {
                return Ok((
                    [
                        (header::CONTENT_TYPE, content_type),
                        (
                            header::HeaderName::from_static("x-content-type-options"),
                            "nosniff",
                        ),
                        (header::HeaderName::from_static("x-cache"), "HIT"),
                    ],
                    cached.as_ref().clone(),
                )
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

            Ok((
                [
                    (header::CONTENT_TYPE, content_type),
                    (
                        header::HeaderName::from_static("x-content-type-options"),
                        "nosniff",
                    ),
                    (header::HeaderName::from_static("x-cache"), "MISS"),
                ],
                image_arc.as_ref().clone(),
            )
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
                    (header::HeaderName::from_static("x-cache"), "MISS"),
                ],
                legend_bytes,
            )
                .into_response())
        }
    }
}
