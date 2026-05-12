use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use arc_swap::ArcSwap;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

use ds_core::config::CollectionConfig;
use ds_core::feature::{Bbox, FeatureQuery};
use ds_core::feature_engine::FeatureEngine;
use ds_core::map_engine::MapEngine;
use ds_mvt::{
    encode_tile, properties_hash, PropertyAllowlist, TileEncodeOptions, TmsKind, VectorTileCache,
    VectorTileKey,
};
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
    /// Collections that can produce vector tiles (MVT). Keyed independently of
    /// `map_engines` — a collection may serve raster, vector, or both.
    pub feature_engines: HashMap<String, Arc<dyn FeatureEngine>>,
    pub feature_collections: HashMap<String, CollectionConfig>,
    pub render_semaphore: Arc<tokio::sync::Semaphore>,
    pub rendered_cache: Arc<RenderedCache>,
    pub vector_tile_cache: Arc<VectorTileCache>,
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
    raster_info: Option<&ds_core::map_engine::RasterInfo>,
    feature_extent: Option<[f64; 4]>,
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

    // OGC API Tiles §7 constrains `dataType` to "map" (raster) or "vector".
    // A collection that supports both is advertised as "map" so existing raster
    // clients aren't surprised; MVT availability is discoverable via the tile
    // path's content negotiation (`?f=mvt`).
    let data_type = if raster_info.is_some() {
        "map"
    } else {
        "vector"
    };

    let mut metadata = json!({
        "id": config.id,
        "title": config.title,
        "description": config.description,
        "dataType": data_type,
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

    let spatial_extent = raster_info
        .and_then(|i| i.spatial_extent)
        .or(feature_extent);
    if let Some(bbox) = spatial_extent {
        metadata["extent"] = json!({
            "spatial": {
                "bbox": [bbox],
                "crs": "http://www.opengis.net/def/crs/OGC/1.3/CRS84"
            }
        });
    }

    if let Some(info) = raster_info {
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
    // A collection may be raster-only, vector-only, or both. Iterate the
    // union of `collections` (raster) and `feature_collections` (vector),
    // then advertise the formats each one actually supports.
    let mut ids: Vec<&String> = state
        .collections
        .keys()
        .chain(state.feature_collections.keys())
        .collect();
    ids.sort();
    ids.dedup();
    for id in ids {
        let config = state
            .collections
            .get(id)
            .or_else(|| state.feature_collections.get(id));
        let Some(config) = config else { continue };
        let has_raster = state.map_engines.contains_key(id);
        let has_vector = state.feature_engines.contains_key(id);

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

        let mut content = serde_json::Map::new();
        if has_raster {
            content.insert(
                "image/png".into(),
                json!({"schema": {"type": "string", "format": "binary"}}),
            );
            content.insert(
                "image/jpeg".into(),
                json!({"schema": {"type": "string", "format": "binary"}}),
            );
            content.insert(
                "image/webp".into(),
                json!({"schema": {"type": "string", "format": "binary"}}),
            );
        }
        if has_vector {
            content.insert(
                MVT_CONTENT_TYPE.into(),
                json!({"schema": {"type": "string", "format": "binary"}}),
            );
        }

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
                        "description": "Tile image or vector tile",
                        "content": content
                    },
                    "400": {"description": "Bad request"},
                    "404": {"description": "Tile not found"},
                    "422": {"description": "Tile too dense (feature count exceeds per-tile cap)"},
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
                        "enum": [
                            "image/png",
                            "image/jpeg",
                            "image/webp",
                            "mvt",
                            "application/vnd.mapbox-vector-tile"
                        ]
                    },
                    "description": "Output format. `mvt` selects Mapbox Vector Tile (only on collections with a FeatureEngine)."
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
    // Surface every tile-enabled collection, regardless of which engine
    // backs it — a vector-only collection that lives in `feature_collections`
    // would otherwise be invisible at the discovery endpoint.
    let mut seen = std::collections::HashSet::new();
    let mut colls: Vec<serde_json::Value> = Vec::new();
    for config in state
        .collections
        .values()
        .chain(state.feature_collections.values())
    {
        if !seen.insert(config.id.clone()) {
            continue;
        }
        let raster_info = state.map_engines.get(&config.id).map(|e| e.raster_info());
        let feature_extent = state
            .feature_engines
            .get(&config.id)
            .and_then(|e| e.spatial_extent());
        let styles = state.styles.get(&config.id);
        colls.push(build_collection_metadata(
            config,
            raster_info.as_ref(),
            feature_extent,
            styles,
            base,
        ));
    }

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
    let raster_info = state.map_engines.get(&id).map(|e| e.raster_info());
    let feature_extent = state
        .feature_engines
        .get(&id)
        .and_then(|e| e.spatial_extent());
    let config = state
        .collections
        .get(&id)
        .or_else(|| state.feature_collections.get(&id))
        .ok_or_else(|| TilesError::NotFound(format!("Collection '{id}' not found")))?;
    if raster_info.is_none() && !state.feature_engines.contains_key(&id) {
        return Err(TilesError::NotFound(format!(
            "Collection '{id}' has no tile source"
        )));
    }
    let styles = state.styles.get(&id);
    Ok(Json(build_collection_metadata(
        config,
        raster_info.as_ref(),
        feature_extent,
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
    let raster_info = state.map_engines.get(&id).map(|e| e.raster_info());
    let feature_extent = state
        .feature_engines
        .get(&id)
        .and_then(|e| e.spatial_extent());
    let config = state
        .collections
        .get(&id)
        .or_else(|| state.feature_collections.get(&id))
        .ok_or_else(|| TilesError::NotFound(format!("Collection '{id}' not found")))?;
    let has_raster = raster_info.is_some();
    let has_vector = state.feature_engines.contains_key(&id);
    if !has_raster && !has_vector {
        return Err(TilesError::NotFound(format!(
            "Collection '{id}' has no tile source"
        )));
    }
    let base = &state.base_url;

    let max_zoom = params::DEFAULT_MAX_ZOOM;
    let spatial_extent = raster_info
        .as_ref()
        .and_then(|i| i.spatial_extent)
        .or(feature_extent);

    let mut tilesets = Vec::new();
    for tms_id in SUPPORTED_TILE_MATRIX_SETS {
        let tms = match tilematrixset::get_tile_matrix_set(tms_id) {
            Some(t) => t,
            None => continue,
        };

        let limits = spatial_extent.map(|bbox| tms.limits_for_extent(bbox, max_zoom));

        let mut item_links = Vec::new();
        if has_raster {
            item_links.push(json!({
                "href": format!(
                    "{base}/tiles/collections/{}/tiles/{tms_id}/{{tileMatrix}}/{{tileRow}}/{{tileCol}}",
                    config.id
                ),
                "rel": "item",
                "type": "image/png",
                "templated": true
            }));
        }
        if has_vector {
            item_links.push(json!({
                "href": format!(
                    "{base}/tiles/collections/{}/tiles/{tms_id}/{{tileMatrix}}/{{tileRow}}/{{tileCol}}?f=mvt",
                    config.id
                ),
                "rel": "item",
                "type": MVT_CONTENT_TYPE,
                "templated": true
            }));
        }

        let mut links = vec![json!({
            "href": format!("{base}/tiles/tileMatrixSets/{tms_id}"),
            "rel": "http://www.opengis.net/def/rel/ogc/1.0/tiling-scheme",
            "type": "application/json"
        })];
        links.extend(item_links);

        let mut tileset = json!({
            "dataType": if has_raster { "map" } else { "vector" },
            "crs": tms.crs,
            "tileMatrixSetURI": format!("http://www.opengis.net/def/tilematrixset/OGC/1.0/{tms_id}"),
            "links": links,
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

/// MVT MIME type registered with IANA.
const MVT_CONTENT_TYPE: &str = "application/vnd.mapbox-vector-tile";

/// Encode an MVT from a `FeatureEngine` and return it as an HTTP response.
///
/// Reached through content negotiation on the standard tile path:
/// `GET /collections/{id}/tiles/{tms}/{z}/{row}/{col}?f=mvt`.
/// Validation order mirrors `render_tile` (TMS → zoom → coords → engine
/// lookup) so error responses stay consistent across raster and vector
/// tile routes.
async fn render_vector_tile(
    headers: HeaderMap,
    id: &str,
    tms_id: &str,
    zoom: u32,
    row: u64,
    col: u64,
    state: AppState,
) -> Result<axum::response::Response, TilesError> {
    let state = state.load_full();

    let tms_kind = TmsKind::from_id(tms_id).ok_or_else(|| {
        TilesError::BadRequest(format!(
            "TileMatrixSet '{tms_id}' is not supported. Supported: {}",
            SUPPORTED_TILE_MATRIX_SETS.join(", ")
        ))
    })?;
    let tms = tilematrixset::get_tile_matrix_set(tms_id)
        .ok_or_else(|| TilesError::Internal("TileMatrixSet lookup failed".into()))?;

    if zoom > params::MAX_ZOOM_LEVEL {
        return Err(TilesError::BadRequest(format!(
            "Zoom level {zoom} exceeds maximum of {}",
            params::MAX_ZOOM_LEVEL
        )));
    }
    if !tms.validate_coords(zoom, row, col) {
        return Err(TilesError::NotFound(format!(
            "Tile {zoom}/{row}/{col} is outside the matrix bounds for {tms_id}"
        )));
    }

    let engine = state
        .feature_engines
        .get(id)
        .ok_or_else(|| {
            TilesError::NotFound(format!("Collection '{id}' has no vector-tile source"))
        })?
        .clone();

    let bbox = tms
        .tile_bbox(zoom, row, col)
        .ok_or_else(|| TilesError::Internal("Failed to compute tile bbox".into()))?;

    let allowlist = PropertyAllowlist::All;
    let props_hash = properties_hash(&allowlist);
    let cache_key = VectorTileKey {
        collection: id.to_string(),
        tms: tms_kind,
        z: zoom,
        x: col,
        y: row,
        properties_hash: props_hash,
        // Engines bump their data version on reload/refresh; folding it into
        // the ETag forces a fresh fetch instead of an infinite `304` loop.
        data_version: engine.data_version(),
    };
    let etag = cache_key.etag();
    let cache_control = "public, max-age=300";

    if let Some(inm) = headers.get(header::IF_NONE_MATCH) {
        if let Ok(s) = inm.to_str() {
            if ds_render::etag_matches(s, &etag) {
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

    if let Some(cached) = state.vector_tile_cache.get(&cache_key) {
        return Ok(axum::response::Response::builder()
            .header(header::CONTENT_TYPE, MVT_CONTENT_TYPE)
            .header(header::ETAG, &etag)
            .header(header::CACHE_CONTROL, cache_control)
            .header(
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff",
            )
            .header(header::HeaderName::from_static("x-cache"), "HIT")
            .body(axum::body::Body::from(cached))
            .unwrap()
            .into_response());
    }

    let query_bbox = Bbox::new(bbox[0], bbox[1], bbox[2], bbox[3])
        .map_err(|e| TilesError::BadRequest(format!("Invalid tile bbox: {e}")))?;
    // `limit` semantics differ across engines: `GeoJsonEngine` honours zero
    // literally (returns nothing), `PostgisEngine` treats zero as "no limit".
    // Asking for `MAX_FEATURES_PER_TILE + 1` is unambiguous: every engine
    // returns at most that many features, engines with native SQL limits can
    // stop early, and the density guard below fires cleanly when we hit the
    // cap.
    let query = FeatureQuery {
        bbox: Some(query_bbox),
        limit: params::MAX_FEATURES_PER_TILE + 1,
        offset: 0,
        datetime: None,
    };

    let page = engine
        .get_features(&query)
        .map_err(|e| TilesError::Internal(format!("Feature query failed: {e}")))?;

    if page.features.len() > params::MAX_FEATURES_PER_TILE {
        // 422 (Unprocessable Content), not 400: the request itself is
        // well-formed — valid TMS, valid coords, registered collection —
        // and only the data exceeds the per-tile budget.
        return Err(TilesError::Unprocessable(format!(
            "tile-too-dense: {} features exceed maximum of {} — raise minzoom or narrow bbox",
            page.features.len(),
            params::MAX_FEATURES_PER_TILE
        )));
    }

    let features = page.features;
    let layer_name = id.to_string();
    let collection_label = id.to_string();

    // Share the raster semaphore — encoding is CPU-bound and a single budget
    // for tile production keeps DoS surface area minimal. Acquire here (just
    // before `spawn_blocking`) rather than around `get_features` so an engine
    // that does I/O during the feature query doesn't hold a render slot while
    // it waits.
    let _permit = tokio::time::timeout(ds_render::RENDER_TIMEOUT, state.render_semaphore.acquire())
        .await
        .map_err(|_| TilesError::ServiceUnavailable("Server busy, try again later".to_string()))?
        .map_err(|_| TilesError::Internal("Render semaphore closed".to_string()))?;

    let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, ds_mvt::EncodeError> {
        let mut opts = TileEncodeOptions::new(layer_name, tms_kind);
        opts.properties = allowlist;
        encode_tile(&features, bbox, &opts)
    })
    .await
    .map_err(|e| TilesError::Internal(format!("Encode task failed: {e}")))?
    .map_err(|e| {
        tracing::warn!("MVT encode error for collection '{collection_label}': {e}");
        TilesError::Internal(format!("Encode failed: {e}"))
    })?;

    let bytes = bytes::Bytes::from(bytes);
    state.vector_tile_cache.insert(cache_key, bytes.clone());

    Ok(axum::response::Response::builder()
        .header(header::CONTENT_TYPE, MVT_CONTENT_TYPE)
        .header(header::ETAG, &etag)
        .header(header::CACHE_CONTROL, cache_control)
        .header(
            header::HeaderName::from_static("x-content-type-options"),
            "nosniff",
        )
        .header(header::HeaderName::from_static("x-cache"), "MISS")
        .body(axum::body::Body::from(bytes))
        .unwrap()
        .into_response())
}

/// GET /tiles/collections/{id}/tiles/{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}
///
/// Content-negotiated between raster (PNG/JPEG/WebP, default) and Mapbox
/// Vector Tile (`?f=mvt`). The latter routes through the `FeatureEngine`
/// registry; the former through `MapEngine` as before.
pub async fn get_tile(
    headers: HeaderMap,
    Path((id, tms_id, tile_matrix, tile_row, tile_col)): Path<(String, String, u32, u64, u64)>,
    Query(params): Query<TileQueryParams>,
    State(state): State<AppState>,
) -> Result<axum::response::Response, TilesError> {
    if params.is_mvt() {
        return render_vector_tile(
            headers,
            &id,
            &tms_id,
            tile_matrix,
            tile_row,
            tile_col,
            state,
        )
        .await;
    }
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
    .map(|r| r.into_response())
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
) -> Result<axum::response::Response, TilesError> {
    if params.is_mvt() {
        return Err(TilesError::BadRequest(
            "Vector tiles (?f=mvt) are not styled — request via /collections/{id}/tiles/...".into(),
        ));
    }
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
    .map(|r| r.into_response())
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
            if ds_render::etag_matches(inm_str, &etag) {
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
    let _permit = tokio::time::timeout(ds_render::RENDER_TIMEOUT, state.render_semaphore.acquire())
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
