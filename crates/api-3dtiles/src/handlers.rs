//! HTTP handlers for the 3D Tiles API.
//!
//! Serves OGC 3D Tiles from any collection implementing `VolumeEngine`:
//! - `GET /collections/{id}/tileset.json` — the tileset (bounding region from
//!   `VolumeInfo`, content pointing at the `.pnts` below).
//! - `GET /collections/{id}/content.pnts` — the point-cloud tile, sampled on a
//!   blocking thread and encoded by `ds-3dtiles`.

use std::collections::HashMap;
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
    /// Drop points below this physical value (e.g. a dBZ floor); carried into
    /// the tileset's `content.uri` so the `.pnts` fetch applies it.
    pub min_value: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct ContentParams {
    pub quantity: Option<String>,
    pub datetime: Option<String>,
    /// Drop points below this physical value (e.g. a dBZ floor).
    pub min_value: Option<f64>,
}

/// Percent-encode a query-string value (RFC 3986 unreserved set passes
/// through). The `VolumeEngine` trait places no charset constraint on quantity
/// names, so a name containing `&`/`+`/`#`/`%`/… must not break the tileset's
/// `content.uri` — the same class of bug as the `+hh:mm` datetime offset.
fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn parse_datetime(s: &str) -> Result<DateTime<Utc>, Tiles3dError> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| Tiles3dError::BadRequest(format!("invalid datetime: {s:?}")))
}

/// Strong content-derived ETag — quoted hex with no `W/` prefix (the bytes are
/// exact, so byte-equal responses are equivalent per RFC 7232 §2.1). FNV-1a
/// 64-bit — stable across Rust versions and instances (unlike `DefaultHasher`),
/// so a toolchain upgrade or a mixed-version fleet doesn't silently invalidate
/// ETags.
fn etag_of(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("\"{h:016x}\"")
}

/// The bundled CesiumJS viewer page (collection + quantity picker), baked into
/// the binary and served at `GET /3dtiles/viewer`.
const VIEWER_HTML: &str = include_str!("../viewer/index.html");

/// `GET /viewer` — interactive CesiumJS viewer. It calls this same API
/// (same-origin by default), so it works on any deployment.
pub async fn get_viewer() -> axum::response::Html<&'static str> {
    axum::response::Html(VIEWER_HTML)
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
    let region = info.region.ok_or_else(|| {
        Tiles3dError::NotFound(format!("collection '{id}' has no spatial coverage yet"))
    })?;

    // The `.pnts` content lives next to this tileset; carry the resolved
    // quantity (and pinned time, if any) so the content fetch is deterministic.
    // Re-format the parsed time as UTC `…Z` — a raw `+hh:mm` offset would be
    // decoded as a space by the client's URL parser and 400 on the fetch.
    let mut query = format!("quantity={}", pct_encode(&quantity));
    if let Some(dt_str) = &params.datetime {
        let dt = parse_datetime(dt_str)?;
        query.push_str(&format!("&datetime={}", dt.format("%Y-%m-%dT%H:%M:%SZ")));
    }
    if let Some(min) = params.min_value {
        // `serde_urlencoded` parses "NaN"/"inf" as valid f64; a non-finite
        // threshold filters every point → a silently-empty tile. Reject → 400.
        if !min.is_finite() {
            return Err(Tiles3dError::BadRequest("min_value must be finite".into()));
        }
        // f64 Display is URL-safe for a finite value (digits, `.`, `-`).
        query.push_str(&format!("&min_value={min}"));
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
    // A non-finite threshold (parsed from "NaN"/"inf") would drop every point.
    if min_value.is_some_and(|m| !m.is_finite()) {
        return Err(Tiles3dError::BadRequest("min_value must be finite".into()));
    }

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
    // Sample, encode, and hash all on the blocking thread (the ETag hash over a
    // multi-MB tile is real CPU — keep it off the async worker too).
    let (bytes, etag) =
        tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, String), Tiles3dError> {
            let cloud = engine.read_point_cloud(quantity.as_deref(), time, min_value, None)?;
            let bytes = ds_3dtiles::encode_pnts(&cloud, colormap.as_ref())
                .map_err(|e| Tiles3dError::Internal(format!("pnts encode failed: {e}")))?;
            let etag = etag_of(&bytes);
            Ok((bytes, etag))
        })
        .await
        .map_err(|e| Tiles3dError::Internal(format!("sample task failed: {e}")))??;

    // A 304 here saves the *network transfer* but not the recompute: the
    // sample + encode + hash already ran above (the `If-None-Match` value can't
    // be trusted to match without recomputing the content). Making 304s
    // CPU-cheap needs an ETag cache keyed by (collection, quantity, time,
    // min_value) + a data-version (latest data changes on poll) — a follow-up.
    // RFC 7232 §3.2: `If-None-Match` may be `*` or a comma-separated list.
    let not_modified = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|v| v == "*" || v.split(',').any(|t| t.trim() == etag));
    if not_modified {
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
            { "href": format!("{base}/3dtiles/viewer"), "rel": "alternate", "type": "text/html", "title": "Interactive 3D Tiles viewer" },
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

#[cfg(test)]
mod tests {
    use super::pct_encode;

    #[test]
    fn pct_encode_passes_unreserved_and_escapes_specials() {
        assert_eq!(pct_encode("DBZH"), "DBZH");
        assert_eq!(pct_encode("a-b_c.d~e"), "a-b_c.d~e");
        // Chars that would break a query value are escaped.
        assert_eq!(pct_encode("a&b+c#d%e"), "a%26b%2Bc%23d%25e");
        assert_eq!(pct_encode("x y"), "x%20y");
    }
}
