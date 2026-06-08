//! HTTP handlers for the 3D Tiles API.
//!
//! Serves OGC 3D Tiles from any collection implementing `VolumeEngine`:
//! - `GET /collections/{id}/tileset.json` — the tileset (bounding region from
//!   `VolumeInfo`, content pointing at the `.pnts` below).
//! - `GET /collections/{id}/content.pnts` — the point-cloud tile, sampled on a
//!   blocking thread and encoded by `ds-3dtiles`.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use chrono::{DateTime, Utc};
use ds_core::config::CollectionConfig;
use ds_core::volume::VolumeEngine;
use ds_render::ColorMap;
use serde::Deserialize;
use serde_json::json;

use crate::error::Tiles3dError;

/// Shared state for the 3D Tiles API. Wrapped in `ArcSwap` for lock-free reads
/// and atomic swap on config reload.
#[derive(Clone)]
pub struct TilesState3d {
    /// Collections that can produce volumetric point clouds, keyed by id.
    pub volume_engines: HashMap<String, Arc<dyn VolumeEngine>>,
    /// Per-collection config (title/description/etc.), keyed by id.
    pub collections: HashMap<String, CollectionConfig>,
    /// Colour ramp applied to point values when encoding `.pnts` RGB. v1 uses
    /// one shared ramp (reflectivity); per-collection/per-quantity colormaps
    /// from config are a follow-up.
    pub colormap: Arc<dyn ColorMap>,
    /// Bounds concurrent sampling/encoding, shared with the raster render path.
    pub render_semaphore: Arc<tokio::sync::Semaphore>,
    /// Public base URL for absolute links.
    pub base_url: String,
}

pub type AppState = Arc<ArcSwap<TilesState3d>>;

/// Look up a collection's volume engine + config, 404 if absent.
fn lookup<'a>(
    state: &'a TilesState3d,
    id: &str,
) -> Result<(&'a Arc<dyn VolumeEngine>, &'a CollectionConfig), Tiles3dError> {
    let engine = state
        .volume_engines
        .get(id)
        .ok_or_else(|| Tiles3dError::NotFound(format!("Collection '{id}' not found")))?;
    let config = state
        .collections
        .get(id)
        .ok_or_else(|| Tiles3dError::Internal(format!("config missing for '{id}'")))?;
    Ok((engine, config))
}

#[derive(Debug, Deserialize)]
pub struct TilesetParams {
    /// Quantity to render (`None` → the collection default).
    pub quantity: Option<String>,
    /// Valid time (RFC 3339). `None` → latest.
    pub datetime: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ContentParams {
    pub quantity: Option<String>,
    pub datetime: Option<String>,
    /// Drop points below this physical value (e.g. a dBZ floor).
    pub min_value: Option<f64>,
}

fn parse_datetime(s: &str) -> Result<DateTime<Utc>, Tiles3dError> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| Tiles3dError::BadRequest(format!("invalid datetime: {s:?}")))
}

/// Weak content-derived ETag (quoted hex of a hash of the bytes).
fn etag_of(bytes: &[u8]) -> String {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    format!("\"{:016x}\"", h.finish())
}

// ---------------------------------------------------------------------------
// Tileset
// ---------------------------------------------------------------------------

/// `GET /collections/{id}/tileset.json`
pub async fn get_tileset(
    Path(id): Path<String>,
    Query(params): Query<TilesetParams>,
    State(state): State<AppState>,
) -> Result<Response, Tiles3dError> {
    let state = state.load_full();
    let (engine, _config) = lookup(&state, &id)?;
    let info = engine.volume_info();

    // Resolve + validate the quantity against what the collection advertises.
    let quantity = match &params.quantity {
        Some(q) => {
            if !info.quantities.iter().any(|(qid, _)| qid == q) {
                return Err(Tiles3dError::BadRequest(format!(
                    "unknown quantity '{q}' for collection '{id}'"
                )));
            }
            q.clone()
        }
        None => info.default_quantity.clone(),
    };
    if quantity.is_empty() {
        return Err(Tiles3dError::NotFound(format!(
            "collection '{id}' has no quantities"
        )));
    }
    // Validate datetime if present (so the tileset never embeds a bad value).
    if let Some(dt) = &params.datetime {
        parse_datetime(dt)?;
    }

    let region = info.region.ok_or_else(|| {
        Tiles3dError::NotFound(format!("collection '{id}' has no spatial coverage yet"))
    })?;

    // The `.pnts` content lives next to this tileset; carry the resolved
    // quantity (and pinned time, if any) so the content fetch is deterministic.
    let mut query = format!("quantity={quantity}");
    if let Some(dt) = &params.datetime {
        query.push_str(&format!("&datetime={dt}"));
    }
    let content_uri = format!("content.pnts?{query}");

    let tileset = ds_3dtiles::tileset_json_for_region(region, &content_uri)
        .map_err(|e| Tiles3dError::Internal(format!("tileset build failed: {e}")))?;

    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "public, max-age=60"),
        ],
        tileset,
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// Content (.pnts)
// ---------------------------------------------------------------------------

/// `GET /collections/{id}/content.pnts`
pub async fn get_content(
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<ContentParams>,
    State(state): State<AppState>,
) -> Result<Response, Tiles3dError> {
    let state = state.load_full();
    let (engine, _config) = lookup(&state, &id)?;

    let time = params.datetime.as_deref().map(parse_datetime).transpose()?;
    let quantity = params.quantity.clone();
    let min_value = params.min_value;

    // Sample + encode off the request worker: `read_point_cloud` does blocking
    // HDF5 I/O and a long CPU loop (CLAUDE.md concurrency rules), so bound it
    // with the shared render semaphore and run it on a blocking thread.
    let _permit = state
        .render_semaphore
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| Tiles3dError::Internal("render semaphore closed".into()))?;

    let engine = engine.clone();
    let colormap = state.colormap.clone();
    let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, Tiles3dError> {
        let cloud = engine.read_point_cloud(quantity.as_deref(), time, min_value, None)?;
        ds_3dtiles::encode_pnts(&cloud, colormap.as_ref())
            .map_err(|e| Tiles3dError::Internal(format!("pnts encode failed: {e}")))
    })
    .await
    .map_err(|e| Tiles3dError::Internal(format!("sample task failed: {e}")))??;

    // Content-derived ETag → cheap 304s; the bytes are deterministic for a
    // given (collection, quantity, time, min_value).
    let etag = etag_of(&bytes);
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|h| h.to_str().ok());
    if if_none_match == Some(etag.as_str()) {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag.as_str()),
                (header::CACHE_CONTROL, "public, max-age=60"),
            ],
        )
            .into_response());
    }

    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::ETAG, etag.as_str()),
            (header::CACHE_CONTROL, "public, max-age=60"),
        ],
        bytes,
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// Landing / collections
// ---------------------------------------------------------------------------

/// `GET /` — API landing document.
pub async fn landing_page(State(state): State<AppState>) -> Json<serde_json::Value> {
    let state = state.load_full();
    let base = &state.base_url;
    Json(json!({
        "title": "MeteoCore — 3D Tiles",
        "description": "Volumetric weather data as OGC 3D Tiles (radar polar volumes)",
        "links": [
            { "href": format!("{base}/3dtiles/"), "rel": "self", "type": "application/json", "title": "This document" },
            { "href": format!("{base}/3dtiles/collections"), "rel": "data", "type": "application/json", "title": "Collections" },
        ]
    }))
}

/// `GET /collections` — list 3D-Tiles-capable collections.
pub async fn collections(State(state): State<AppState>) -> Json<serde_json::Value> {
    let state = state.load_full();
    let base = &state.base_url;
    let mut ids: Vec<&String> = state.volume_engines.keys().collect();
    ids.sort();
    let items: Vec<_> = ids
        .iter()
        .map(|id| collection_doc(&state, id, base))
        .collect();
    Json(json!({ "collections": items }))
}

/// `GET /collections/{id}` — one collection document.
pub async fn collection(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, Tiles3dError> {
    let state = state.load_full();
    // 404 if not a volume collection.
    lookup(&state, &id)?;
    Ok(Json(collection_doc(&state, &id, &state.base_url)))
}

fn collection_doc(state: &TilesState3d, id: &str, base: &str) -> serde_json::Value {
    let cfg = state.collections.get(id);
    let title = cfg
        .map(|c| c.title.clone())
        .unwrap_or_else(|| id.to_string());
    let description = cfg.map(|c| c.description.clone()).unwrap_or_default();
    let info = state.volume_engines.get(id).map(|e| e.volume_info());
    let quantities: Vec<_> = info
        .as_ref()
        .map(|i| {
            i.quantities
                .iter()
                .map(|(qid, label)| json!({ "id": qid, "label": label }))
                .collect()
        })
        .unwrap_or_default();
    json!({
        "id": id,
        "title": title,
        "description": description,
        "quantities": quantities,
        "links": [
            { "href": format!("{base}/3dtiles/collections/{id}"), "rel": "self", "type": "application/json", "title": "This document" },
            { "href": format!("{base}/3dtiles/collections/{id}/tileset.json"), "rel": "3dtiles", "type": "application/json", "title": "3D Tiles tileset" },
        ]
    })
}
