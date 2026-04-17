use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use arc_swap::ArcSwap;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

use ds_core::config::CollectionConfig;
use ds_core::map_engine::MapEngine;
use ds_render::{CacheKey, RenderedCache, StyleInfo};

use crate::error::TilesError;
use crate::params::{self, TileQueryParams};
use crate::tilematrixset::{self, SUPPORTED_TILE_MATRIX_SETS};

/// Pre-generated 256x256 fully transparent PNG for empty (all-nodata) tiles.
/// Avoids running the colorization + encoding pipeline when a tile has no data.
static EMPTY_TILE_PNG: LazyLock<bytes::Bytes> = LazyLock::new(|| {
    let size = params::TILE_SIZE;
    let rgba = vec![0u8; (size * size * 4) as usize];
    bytes::Bytes::from(
        ds_render::encode_png(&rgba, size, size).expect("encoding empty tile PNG must not fail"),
    )
});

/// Shared state for the OGC API Tiles service.
#[derive(Clone)]
pub struct TilesState {
    /// Collections that can produce map tiles (raster rendering).
    pub map_engines: HashMap<String, Arc<dyn MapEngine>>,
    pub collections: HashMap<String, CollectionConfig>,
    pub styles: HashMap<String, HashMap<String, StyleInfo>>,
    pub render_semaphore: Arc<tokio::sync::Semaphore>,
    pub rendered_cache: Arc<RenderedCache>,
    pub base_url: String,
}

pub type AppState = Arc<ArcSwap<TilesState>>;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn lookup_engine<'a>(
    state: &'a TilesState,
    id: &str,
) -> Result<(&'a Arc<dyn MapEngine>, &'a CollectionConfig), TilesError> {
    let engine = state
        .map_engines
        .get(id)
        .ok_or_else(|| TilesError::NotFound(format!("Collection '{id}' not found")))?;
    let config = state
        .collections
        .get(id)
        .ok_or_else(|| TilesError::Internal("Collection config missing".into()))?;
    Ok((engine, config))
}

fn cache_control_value(has_explicit_time: bool) -> &'static str {
    if has_explicit_time {
        // Tiles at fixed z/x/y + timestamp are truly immutable
        "public, max-age=86400, immutable"
    } else {
        "public, max-age=60, must-revalidate"
    }
}

fn crs_to_uri(crs: &str) -> &'static str {
    match crs {
        "CRS:84" => "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
        "EPSG:3857" => "http://www.opengis.net/def/crs/EPSG/0/3857",
        _ => "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
    }
}

fn build_collection_metadata(
    config: &CollectionConfig,
    info: &ds_core::map_engine::RasterInfo,
    styles: Option<&HashMap<String, StyleInfo>>,
    base_url: &str,
) -> serde_json::Value {
    let mut tms_links = Vec::new();
    for tms_id in SUPPORTED_TILE_MATRIX_SETS {
        tms_links.push(json!({
            "tileMatrixSet": tms_id,
            "tileMatrixSetURI": format!("http://www.opengis.net/def/tilematrixset/OGC/1.0/{tms_id}"),
        }));
    }

    let mut style_list = Vec::new();
    if let Some(styles) = styles {
        let mut names: Vec<&String> = styles.keys().collect();
        names.sort_by(|a, b| {
            if a.as_str() == "default" {
                std::cmp::Ordering::Less
            } else if b.as_str() == "default" {
                std::cmp::Ordering::Greater
            } else {
                a.cmp(b)
            }
        });
        for name in names {
            if let Some(s) = styles.get(name) {
                style_list.push(json!({
                    "id": s.name,
                    "title": s.title,
                }));
            }
        }
    }

    let mut metadata = json!({
        "id": config.id,
        "title": config.title,
        "description": config.description,
        "dataType": "map",
        "tileMatrixSetLinks": tms_links,
        "styles": style_list,
        "links": [
            {
                "href": format!("{base_url}/tiles/collections/{}", config.id),
                "rel": "self",
                "type": "application/json",
                "title": config.title
            },
            {
                "href": format!("{base_url}/tiles/collections/{}/tiles", config.id),
                "rel": "tiles",
                "type": "application/json",
                "title": "Tilesets"
            }
        ]
    });

    if let Some(bbox) = info.spatial_extent {
        metadata["extent"] = json!({
            "spatial": {
                "bbox": [bbox],
                "crs": "http://www.opengis.net/def/crs/OGC/1.3/CRS84"
            }
        });
    }

    if !info.times.is_empty() {
        let first = info.times.first().map(|t| t.to_rfc3339());
        let last = info.times.last().map(|t| t.to_rfc3339());
        if let (Some(start), Some(end)) = (first, last) {
            if let Some(extent) = metadata.get_mut("extent") {
                extent["temporal"] = json!({
                    "interval": [[start, end]],
                    "trs": "http://www.opengis.net/def/uom/ISO-8601/0/Gregorian"
                });
            } else {
                metadata["extent"] = json!({
                    "temporal": {
                        "interval": [[start, end]],
                        "trs": "http://www.opengis.net/def/uom/ISO-8601/0/Gregorian"
                    }
                });
            }
        }
    }

    metadata
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /tiles/ — Landing page
pub async fn landing_page(State(state): State<AppState>) -> impl IntoResponse {
    let state = state.load_full();
    let base = &state.base_url;
    Json(json!({
        "title": "MeteoCore - Tiles",
        "description": "Metocean Data Server \u{2014} OGC API Tiles",
        "links": [
            {
                "href": format!("{base}/tiles/"),
                "rel": "self",
                "type": "application/json",
                "title": "This document"
            },
            {
                "href": format!("{base}/tiles/api"),
                "rel": "service-desc",
                "type": "application/vnd.oai.openapi+json;version=3.0",
                "title": "API definition"
            },
            {
                "href": format!("{base}/tiles/api/docs"),
                "rel": "service-doc",
                "type": "text/html",
                "title": "API documentation"
            },
            {
                "href": format!("{base}/tiles/conformance"),
                "rel": "conformance",
                "type": "application/json",
                "title": "Conformance classes"
            },
            {
                "href": format!("{base}/tiles/collections"),
                "rel": "data",
                "type": "application/json",
                "title": "Collections"
            },
            {
                "href": format!("{base}/tiles/tileMatrixSets"),
                "rel": "tiling-schemes",
                "type": "application/json",
                "title": "Tile matrix sets"
            }
        ]
    }))
}

/// GET /tiles/api — OpenAPI 3.0.3 definition
pub async fn api_definition(State(state): State<AppState>) -> impl IntoResponse {
    let state = state.load_full();
    let mut collection_paths = json!({});
    for config in state.collections.values() {
        let id = &config.id;

        collection_paths[format!("/tiles/collections/{id}")] = json!({
            "get": {
                "summary": format!("Get {} collection metadata", config.title),
                "operationId": format!("getCollection_{id}"),
                "tags": [id],
                "responses": {
                    "200": {"description": "Collection metadata"},
                    "404": {"description": "Collection not found"}
                }
            }
        });

        collection_paths[format!("/tiles/collections/{id}/tiles")] = json!({
            "get": {
                "summary": format!("List tilesets for {}", config.title),
                "operationId": format!("getTilesets_{id}"),
                "tags": [id],
                "responses": {
                    "200": {"description": "Available tilesets"}
                }
            }
        });

        collection_paths[format!("/tiles/collections/{id}/tiles/{{tileMatrixSetId}}/{{tileMatrix}}/{{tileRow}}/{{tileCol}}")] = json!({
            "get": {
                "summary": format!("Get tile for {}", config.title),
                "operationId": format!("getTile_{id}"),
                "tags": [id],
                "parameters": [
                    {
                        "name": "tileMatrixSetId",
                        "in": "path",
                        "required": true,
                        "schema": {"type": "string", "enum": SUPPORTED_TILE_MATRIX_SETS},
                        "description": "Tile matrix set identifier"
                    },
                    {
                        "name": "tileMatrix",
                        "in": "path",
                        "required": true,
                        "schema": {"type": "integer", "minimum": 0, "maximum": params::MAX_ZOOM_LEVEL},
                        "description": "Zoom level"
                    },
                    {
                        "name": "tileRow",
                        "in": "path",
                        "required": true,
                        "schema": {"type": "integer", "minimum": 0},
                        "description": "Row index"
                    },
                    {
                        "name": "tileCol",
                        "in": "path",
                        "required": true,
                        "schema": {"type": "integer", "minimum": 0},
                        "description": "Column index"
                    },
                    {"$ref": "#/components/parameters/datetime"},
                    {"$ref": "#/components/parameters/f"}
                ],
                "responses": {
                    "200": {
                        "description": "Tile image",
                        "content": {
                            "image/png": {"schema": {"type": "string", "format": "binary"}},
                            "image/jpeg": {"schema": {"type": "string", "format": "binary"}},
                            "image/webp": {"schema": {"type": "string", "format": "binary"}}
                        }
                    },
                    "400": {"description": "Bad request"},
                    "404": {"description": "Tile not found"},
                    "500": {"description": "Server error"}
                }
            }
        });
    }

    let mut paths = json!({
        "/tiles/": {
            "get": {
                "summary": "Landing page",
                "operationId": "getLandingPage",
                "responses": { "200": {"description": "Landing page"} }
            }
        },
        "/tiles/conformance": {
            "get": {
                "summary": "Conformance classes",
                "operationId": "getConformance",
                "responses": { "200": {"description": "Conformance classes"} }
            }
        },
        "/tiles/collections": {
            "get": {
                "summary": "List tile-enabled collections",
                "operationId": "getCollections",
                "responses": { "200": {"description": "List of collections"} }
            }
        },
        "/tiles/tileMatrixSets": {
            "get": {
                "summary": "List supported tile matrix sets",
                "operationId": "getTileMatrixSets",
                "responses": { "200": {"description": "List of tile matrix sets"} }
            }
        },
        "/tiles/tileMatrixSets/{tileMatrixSetId}": {
            "get": {
                "summary": "Get tile matrix set definition",
                "operationId": "getTileMatrixSet",
                "parameters": [{
                    "name": "tileMatrixSetId",
                    "in": "path",
                    "required": true,
                    "schema": {"type": "string"},
                    "description": "Tile matrix set identifier"
                }],
                "responses": {
                    "200": {"description": "Tile matrix set definition"},
                    "404": {"description": "Tile matrix set not found"}
                }
            }
        }
    });

    if let (Some(main_obj), Some(coll_obj)) = (paths.as_object_mut(), collection_paths.as_object())
    {
        for (k, v) in coll_obj {
            main_obj.insert(k.clone(), v.clone());
        }
    }

    let openapi = json!({
        "openapi": "3.0.3",
        "info": {
            "title": "MeteoCore - OGC API Tiles",
            "version": "1.0.0",
            "description": "OGC API - Tiles implementation"
        },
        "paths": paths,
        "components": {
            "parameters": {
                "datetime": {
                    "name": "datetime",
                    "in": "query",
                    "required": false,
                    "schema": {"type": "string"},
                    "description": "ISO 8601 timestamp"
                },
                "f": {
                    "name": "f",
                    "in": "query",
                    "required": false,
                    "schema": {
                        "type": "string",
                        "default": "image/png",
                        "enum": ["image/png", "image/jpeg", "image/webp"]
                    },
                    "description": "Output format"
                }
            }
        }
    });

    Json(openapi)
}

/// GET /tiles/api/docs — Swagger UI
pub async fn api_docs(State(state): State<AppState>) -> impl IntoResponse {
    let state = state.load_full();
    let spec_url = format!("{}/tiles/api", state.base_url);
    axum::response::Html(ds_core::openapi::swagger_ui_html(
        "MeteoCore - Tiles API",
        &spec_url,
    ))
}

/// GET /tiles/conformance
pub async fn conformance() -> impl IntoResponse {
    Json(json!({
        "conformsTo": [
            "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/core",
            "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/tileset",
            "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/tilesets-list",
            "http://www.opengis.net/spec/tms/2.0/conf/tilematrixset",
            "http://www.opengis.net/spec/tms/2.0/conf/json-tilematrixset",
            "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/png",
            "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/jpeg"
        ]
    }))
}

/// GET /tiles/tileMatrixSets — List supported tile matrix sets
pub async fn tile_matrix_sets(State(state): State<AppState>) -> impl IntoResponse {
    let state = state.load_full();
    let base = &state.base_url;
    let sets: Vec<serde_json::Value> = SUPPORTED_TILE_MATRIX_SETS
        .iter()
        .filter_map(|id| {
            let tms = tilematrixset::get_tile_matrix_set(id)?;
            Some(json!({
                "id": tms.id,
                "title": tms.title,
                "uri": format!("http://www.opengis.net/def/tilematrixset/OGC/1.0/{}", tms.id),
                "crs": tms.crs,
                "links": [{
                    "href": format!("{base}/tiles/tileMatrixSets/{}", tms.id),
                    "rel": "self",
                    "type": "application/json"
                }]
            }))
        })
        .collect();

    Json(json!({
        "tileMatrixSets": sets,
        "links": [{
            "href": format!("{base}/tiles/tileMatrixSets"),
            "rel": "self",
            "type": "application/json"
        }]
    }))
}

/// GET /tiles/tileMatrixSets/{tileMatrixSetId} — Get tile matrix set definition
pub async fn tile_matrix_set(Path(tms_id): Path<String>) -> Result<impl IntoResponse, TilesError> {
    let tms = tilematrixset::get_tile_matrix_set(&tms_id).ok_or_else(|| {
        TilesError::NotFound(format!(
            "TileMatrixSet '{tms_id}' not found. Available: {}",
            SUPPORTED_TILE_MATRIX_SETS.join(", ")
        ))
    })?;

    Ok(Json(tms.to_json()))
}

/// GET /tiles/collections — List tile-enabled collections
pub async fn collections(State(state): State<AppState>) -> impl IntoResponse {
    let state = state.load_full();
    let base = &state.base_url;
    let mut colls: Vec<serde_json::Value> = state
        .collections
        .values()
        .filter_map(|config| {
            let engine = state.map_engines.get(&config.id)?;
            let info = engine.raster_info();
            let styles = state.styles.get(&config.id);
            Some(build_collection_metadata(config, &info, styles, base))
        })
        .collect();

    colls.sort_by(|a, b| {
        a["id"]
            .as_str()
            .unwrap_or("")
            .cmp(b["id"].as_str().unwrap_or(""))
    });

    Json(json!({
        "collections": colls,
        "links": [{
            "href": format!("{base}/tiles/collections"),
            "rel": "self",
            "type": "application/json"
        }]
    }))
}

/// GET /tiles/collections/{id} — Collection detail
pub async fn collection(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, TilesError> {
    let state = state.load_full();
    let (engine, config) = lookup_engine(&state, &id)?;
    let info = engine.raster_info();
    let styles = state.styles.get(&id);
    Ok(Json(build_collection_metadata(
        config,
        &info,
        styles,
        &state.base_url,
    )))
}

/// GET /tiles/collections/{id}/tiles — List tilesets for a collection
pub async fn collection_tilesets(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, TilesError> {
    let state = state.load_full();
    let (engine, config) = lookup_engine(&state, &id)?;
    let info = engine.raster_info();
    let base = &state.base_url;

    let max_zoom = params::DEFAULT_MAX_ZOOM;

    let mut tilesets = Vec::new();
    for tms_id in SUPPORTED_TILE_MATRIX_SETS {
        let tms = match tilematrixset::get_tile_matrix_set(tms_id) {
            Some(t) => t,
            None => continue,
        };

        let limits = info
            .spatial_extent
            .map(|bbox| tms.limits_for_extent(bbox, max_zoom));

        let mut tileset = json!({
            "dataType": "map",
            "crs": tms.crs,
            "tileMatrixSetURI": format!("http://www.opengis.net/def/tilematrixset/OGC/1.0/{tms_id}"),
            "links": [
                {
                    "href": format!("{base}/tiles/tileMatrixSets/{tms_id}"),
                    "rel": "http://www.opengis.net/def/rel/ogc/1.0/tiling-scheme",
                    "type": "application/json"
                },
                {
                    "href": format!(
                        "{base}/tiles/collections/{}/tiles/{tms_id}/{{tileMatrix}}/{{tileRow}}/{{tileCol}}",
                        config.id
                    ),
                    "rel": "item",
                    "type": "image/png",
                    "templated": true
                }
            ]
        });

        if let Some(limits) = limits {
            tileset["tileMatrixSetLimits"] = json!(limits);
        }

        tilesets.push(tileset);
    }

    Ok(Json(json!({
        "tilesets": tilesets,
        "links": [{
            "href": format!("{base}/tiles/collections/{}/tiles", id),
            "rel": "self",
            "type": "application/json"
        }]
    })))
}

/// GET /tiles/collections/{id}/tiles/{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}
pub async fn get_tile(
    headers: HeaderMap,
    Path((id, tms_id, tile_matrix, tile_row, tile_col)): Path<(String, String, u32, u64, u64)>,
    Query(params): Query<TileQueryParams>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, TilesError> {
    render_tile(
        &id,
        "default",
        &tms_id,
        tile_matrix,
        tile_row,
        tile_col,
        params,
        headers,
        state,
    )
    .await
}

/// GET /tiles/collections/{id}/styles/{styleId}/tiles/{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}
pub async fn get_styled_tile(
    headers: HeaderMap,
    Path((id, style_id, tms_id, tile_matrix, tile_row, tile_col)): Path<(
        String,
        String,
        String,
        u32,
        u64,
        u64,
    )>,
    Query(params): Query<TileQueryParams>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, TilesError> {
    render_tile(
        &id,
        &style_id,
        &tms_id,
        tile_matrix,
        tile_row,
        tile_col,
        params,
        headers,
        state,
    )
    .await
}

/// Shared tile rendering logic.
#[allow(clippy::too_many_arguments)]
async fn render_tile(
    collection_id: &str,
    style_name: &str,
    tms_id: &str,
    zoom: u32,
    row: u64,
    col: u64,
    params: TileQueryParams,
    headers: HeaderMap,
    state: AppState,
) -> Result<impl IntoResponse, TilesError> {
    let state = state.load_full();
    let (engine, _config) = lookup_engine(&state, collection_id)?;

    // Validate TileMatrixSet
    let tms = tilematrixset::get_tile_matrix_set(tms_id).ok_or_else(|| {
        TilesError::BadRequest(format!(
            "TileMatrixSet '{tms_id}' is not supported. Supported: {}",
            SUPPORTED_TILE_MATRIX_SETS.join(", ")
        ))
    })?;

    // Validate zoom level
    if zoom > params::MAX_ZOOM_LEVEL {
        return Err(TilesError::BadRequest(format!(
            "Zoom level {zoom} exceeds maximum of {}",
            params::MAX_ZOOM_LEVEL
        )));
    }

    // Validate tile coordinates
    if !tms.validate_coords(zoom, row, col) {
        return Err(TilesError::NotFound(format!(
            "Tile {zoom}/{row}/{col} is outside the matrix bounds for {tms_id}"
        )));
    }

    // Compute bbox from tile coordinates
    let bbox = tms
        .tile_bbox(zoom, row, col)
        .ok_or_else(|| TilesError::Internal("Failed to compute tile bbox".into()))?;

    // Validate query params
    let validated = params.validate()?;

    // Look up style
    let layer_styles = state
        .styles
        .get(collection_id)
        .ok_or_else(|| TilesError::NotFound(format!("Collection '{collection_id}' not found")))?;

    let style_info = layer_styles.get(style_name).ok_or_else(|| {
        TilesError::NotFound(format!(
            "Style '{style_name}' not found for collection '{collection_id}'. Available: {}",
            layer_styles.keys().cloned().collect::<Vec<_>>().join(", ")
        ))
    })?;

    let colormap = style_info.colormap.clone();
    let content_type = validated.format.content_type();
    let has_explicit_time = validated.time.is_some();

    // Determine output CRS from TileMatrixSet
    let output_crs = match tms_id {
        "WebMercatorQuad" => ds_core::map_engine::OutputCrs::WebMercator,
        _ => ds_core::map_engine::OutputCrs::Wgs84,
    };
    let content_crs = match tms_id {
        "WebMercatorQuad" => crs_to_uri("EPSG:3857"),
        _ => crs_to_uri("CRS:84"),
    };

    // Resolve time: use explicit or default to latest
    let time = match validated.time {
        Some(t) => Some(t),
        None => {
            let info = engine.raster_info();
            info.times.last().copied()
        }
    };

    let tile_size = params::TILE_SIZE;

    // Build cache key
    let cache_key = CacheKey {
        layer: collection_id.to_string(),
        style: style_name.to_string(),
        format: match validated.format {
            ds_render::ImageFormat::Png => 0,
            ds_render::ImageFormat::Jpeg => 1,
            ds_render::ImageFormat::Webp => 2,
        },
        crs: tms_id.to_string(),
        bbox: ds_render::quantize_bbox(&bbox),
        width: tile_size,
        height: tile_size,
        time,
    };

    let etag = cache_key.etag();
    let cache_control = cache_control_value(has_explicit_time);

    // Check If-None-Match — return 304 before any cache lookup or rendering
    if let Some(inm) = headers.get(header::IF_NONE_MATCH) {
        if let Ok(inm_str) = inm.to_str() {
            if inm_str == etag || inm_str.trim_matches('"') == etag.trim_matches('"') {
                return Ok(axum::response::Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header(header::ETAG, &etag)
                    .header(header::CACHE_CONTROL, cache_control)
                    .body(axum::body::Body::empty())
                    .unwrap()
                    .into_response());
            }
        }
    }

    // Check rendered cache
    if let Some(cached) = state.rendered_cache.get(&cache_key) {
        return Ok(axum::response::Response::builder()
            .header(header::CONTENT_TYPE, content_type)
            .header(header::ETAG, &etag)
            .header(header::CACHE_CONTROL, cache_control)
            .header(header::HeaderName::from_static("content-crs"), content_crs)
            .header(
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff",
            )
            .header(header::HeaderName::from_static("x-cache"), "HIT")
            .body(axum::body::Body::from(cached))
            .unwrap()
            .into_response());
    }

    // Acquire render semaphore (with timeout to shed load under pressure)
    let _permit = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        state.render_semaphore.acquire(),
    )
    .await
    .map_err(|_| TilesError::ServiceUnavailable("Server busy, try again later".to_string()))?
    .map_err(|_| TilesError::Internal("Render semaphore closed".to_string()))?;

    // Render on a blocking thread
    let engine = engine.clone();
    let format = validated.format;
    let rendered_cache = state.rendered_cache.clone();

    // The blocking closure returns Ok(None) for empty (all-nodata) tiles,
    // or Ok(Some(bytes)) for tiles with data.
    let style_parameter = style_info.parameter.as_deref().map(String::from);

    let render_result = tokio::task::spawn_blocking(move || {
        let tile = engine.get_raster_tile(
            bbox,
            tile_size,
            tile_size,
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
    .map_err(|e| TilesError::Internal(format!("Render task failed: {e}")))?;

    let maybe_bytes = render_result.map_err(|e| {
        tracing::warn!("Tiles render error for collection '{}': {e}", collection_id);
        TilesError::Internal(format!("Render failed: {e}"))
    })?;

    // Empty tiles: return the pre-generated transparent PNG without caching.
    let (image_bytes, x_cache) = match maybe_bytes {
        None => (EMPTY_TILE_PNG.clone(), "EMPTY"),
        Some(bytes) => {
            let image_bytes = bytes::Bytes::from(bytes);
            rendered_cache.insert(cache_key, image_bytes.clone());
            (image_bytes, "MISS")
        }
    };

    Ok(axum::response::Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ETAG, &etag)
        .header(header::CACHE_CONTROL, cache_control)
        .header(header::HeaderName::from_static("content-crs"), content_crs)
        .header(
            header::HeaderName::from_static("x-content-type-options"),
            "nosniff",
        )
        .header(header::HeaderName::from_static("x-cache"), x_cache)
        .body(axum::body::Body::from(image_bytes))
        .unwrap()
        .into_response())
}
