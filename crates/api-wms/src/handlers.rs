use std::collections::HashMap;
use std::hash::{Hash, Hasher};
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

#[derive(Clone)]
pub struct WmsState {
    pub engines: HashMap<String, Arc<dyn MapEngine>>,
    pub collections: HashMap<String, CollectionConfig>,
    pub colormaps: HashMap<String, Arc<dyn ColorMap>>,
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

#[derive(Clone, Debug)]
struct CacheKey {
    layer: String,
    crs: String,
    bbox: [i64; 4], // bbox quantized to microdegrees (6 decimal places)
    width: u32,
    height: u32,
    time: Option<DateTime<Utc>>,
}

impl PartialEq for CacheKey {
    fn eq(&self, other: &Self) -> bool {
        self.layer == other.layer
            && self.crs == other.crs
            && self.bbox == other.bbox
            && self.width == other.width
            && self.height == other.height
            && self.time == other.time
    }
}

impl Eq for CacheKey {}

impl Hash for CacheKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.layer.hash(state);
        self.crs.hash(state);
        self.bbox.hash(state);
        self.width.hash(state);
        self.height.hash(state);
        self.time.hash(state);
    }
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

            // Look up colormap
            let colormap = state
                .colormaps
                .get(&params.layer)
                .ok_or_else(|| {
                    WmsError::Internal(format!(
                        "No colormap configured for layer '{}'",
                        params.layer
                    ))
                })?
                .clone();

            // Check rendered cache
            let cache_key = CacheKey {
                layer: params.layer.clone(),
                crs: params.crs.clone(),
                bbox: quantize_bbox(&params.bbox),
                width: params.width,
                height: params.height,
                time: params.time,
            };

            if let Some(cached) = state.rendered_cache.get(&cache_key) {
                return Ok((
                    [
                        (header::CONTENT_TYPE, "image/png"),
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
            let rendered_cache = state.rendered_cache.clone();

            let render_result = tokio::task::spawn_blocking(move || {
                let tile = engine.get_raster_tile(bbox, width, height, time, &output_crs)?;
                ds_render::render_tile_png(&tile, colormap.as_ref())
            })
            .await
            .map_err(|e| WmsError::Internal(format!("Render task failed: {e}")))?;

            let (png_bytes, cacheable) = match render_result {
                Ok(bytes) => (bytes, true),
                Err(e) => {
                    tracing::warn!("WMS render error for layer '{}': {e}", params.layer);
                    // Return a semi-transparent red error tile so the client
                    // can see which areas failed instead of silent white gaps.
                    (render_error_tile(params.width, params.height)?, false)
                }
            };

            let png_arc = Arc::new(png_bytes);
            if cacheable {
                rendered_cache.insert(cache_key, png_arc.clone());
            }

            Ok((
                [
                    (header::CONTENT_TYPE, "image/png"),
                    (
                        header::HeaderName::from_static("x-content-type-options"),
                        "nosniff",
                    ),
                    (header::HeaderName::from_static("x-cache"), "MISS"),
                ],
                png_arc.as_ref().clone(),
            )
                .into_response())
        }
    }
}
