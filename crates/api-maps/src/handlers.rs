use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

use ds_core::config::CollectionConfig;
use ds_core::map_engine::MapEngine;
use ds_render::{CacheKey, RenderedCache, StyleInfo};

use crate::error::MapsError;
use crate::params::{self, MapQueryParams};

/// Shared state for the OGC API Maps service.
#[derive(Clone)]
pub struct MapsState {
    pub engines: HashMap<String, Arc<dyn MapEngine>>,
    pub collections: HashMap<String, CollectionConfig>,
    /// Map of collection_id -> style_name -> StyleInfo.
    pub styles: HashMap<String, HashMap<String, StyleInfo>>,
    pub render_semaphore: Arc<tokio::sync::Semaphore>,
    pub rendered_cache: Arc<RenderedCache>,
    pub base_url: String,
}

pub type AppState = Arc<ArcSwap<MapsState>>;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn lookup_engine<'a>(
    state: &'a MapsState,
    id: &str,
) -> Result<(&'a Arc<dyn MapEngine>, &'a CollectionConfig), MapsError> {
    let engine = state
        .engines
        .get(id)
        .ok_or_else(|| MapsError::NotFound(format!("Collection '{id}' not found")))?;
    let config = state
        .collections
        .get(id)
        .ok_or_else(|| MapsError::Internal("Collection config missing".into()))?;
    Ok((engine, config))
}

/// Map CRS identifier to OGC URI for Content-Crs header.
fn crs_to_uri(crs: &str) -> &'static str {
    match crs {
        "CRS:84" => "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
        "EPSG:4326" => "http://www.opengis.net/def/crs/EPSG/0/4326",
        "EPSG:3857" => "http://www.opengis.net/def/crs/EPSG/0/3857",
        "EPSG:3067" => "http://www.opengis.net/def/crs/EPSG/0/3067",
        "EPSG:3035" => "http://www.opengis.net/def/crs/EPSG/0/3035",
        _ => "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
    }
}

/// Cache-Control header value.
fn cache_control_value(has_explicit_time: bool) -> &'static str {
    if has_explicit_time {
        "public, max-age=86400, immutable"
    } else {
        "public, max-age=60, must-revalidate"
    }
}

fn build_collection_metadata(
    config: &CollectionConfig,
    info: &ds_core::map_engine::RasterInfo,
    styles: Option<&HashMap<String, StyleInfo>>,
    base_url: &str,
) -> serde_json::Value {
    let mut crs_list: Vec<&str> = params::supported_crs_list().to_vec();
    // Deduplicate
    crs_list.dedup();
    let crs_uris: Vec<String> = crs_list
        .iter()
        .map(|c| match *c {
            "CRS:84" => "http://www.opengis.net/def/crs/OGC/1.3/CRS84".to_string(),
            "EPSG:4326" => "http://www.opengis.net/def/crs/EPSG/0/4326".to_string(),
            "EPSG:3857" => "http://www.opengis.net/def/crs/EPSG/0/3857".to_string(),
            "EPSG:3067" => "http://www.opengis.net/def/crs/EPSG/0/3067".to_string(),
            "EPSG:3035" => "http://www.opengis.net/def/crs/EPSG/0/3035".to_string(),
            other => other.to_string(),
        })
        .collect();

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
                    "links": [
                        {
                            "href": format!("{base_url}/maps/collections/{}/styles/{}/map", config.id, s.name),
                            "rel": "map",
                            "type": "image/png"
                        }
                    ]
                }));
            }
        }
    }

    let mut links = vec![
        json!({
            "href": format!("{base_url}/maps/collections/{}", config.id),
            "rel": "self",
            "type": "application/json",
            "title": config.title
        }),
        json!({
            "href": format!("{base_url}/maps/collections/{}/map", config.id),
            "rel": "map",
            "type": "image/png",
            "title": "Map"
        }),
        json!({
            "href": format!("{base_url}/maps/collections/{}/styles", config.id),
            "rel": "styles",
            "type": "application/json",
            "title": "Styles"
        }),
    ];

    // Map tilesets — rendered (raster) tiles are an OGC API Maps "map
    // tileset", discoverable from the maps collection via the `tilesets-map`
    // relation. Only advertise it when the operator exposed this collection
    // through the Tiles API (the standalone `/tiles` router still serves it).
    if config.apis.iter().any(|a| a == "tiles") {
        links.push(json!({
            "href": format!("{base_url}/tiles/collections/{}/tiles", config.id),
            "rel": "http://www.opengis.net/def/rel/ogc/1.0/tilesets-map",
            "type": "application/json",
            "title": "Map tilesets"
        }));
    }

    let mut metadata = json!({
        "id": config.id,
        "title": config.title,
        "description": config.description,
        "dataType": "map",
        "crs": crs_uris,
        "styles": style_list,
        "links": links
    });

    // Only advertise `storageCrs` when the native CRS has a stable OGC URI.
    // Engines label projected/rotated grids with internal names ("TM",
    // "LAEA", "projected", "rotated_ll", …) that have no URI; emitting CRS84
    // for those would mislabel the storage grid, so omit it instead.
    if let Some(storage_crs) = native_crs_to_uri(&info.native_crs) {
        metadata["storageCrs"] = json!(storage_crs);
    }

    if let Some(extent) = build_extent(info) {
        metadata["extent"] = extent;
    }

    metadata
}

/// Map an engine's native CRS label to an OGC URI for `storageCrs`, or `None`
/// when there is no stable URI for it. Unlike [`crs_to_uri`] (which serves the
/// `Content-Crs` header for request-validated CRS values and defaults to
/// CRS84), this never falls back: engines populate `RasterInfo.native_crs`
/// with internal labels ("TM", "LAEA", "projected", "rotated_ll", …) for
/// projections that have no canonical URI, and a wrong `storageCrs` is worse
/// than an absent one.
fn native_crs_to_uri(crs: &str) -> Option<&'static str> {
    match crs {
        "CRS:84" => Some("http://www.opengis.net/def/crs/OGC/1.3/CRS84"),
        "EPSG:4326" => Some("http://www.opengis.net/def/crs/EPSG/0/4326"),
        "EPSG:3857" => Some("http://www.opengis.net/def/crs/EPSG/0/3857"),
        "EPSG:3067" => Some("http://www.opengis.net/def/crs/EPSG/0/3067"),
        "EPSG:3035" => Some("http://www.opengis.net/def/crs/EPSG/0/3035"),
        _ => None,
    }
}

/// Build the OGC API Common Part 2 `extent` object (spatial, temporal,
/// vertical) including the `grid` resolution descriptors. Returns `None` when
/// the collection advertises no spatial, temporal, or vertical extent.
fn build_extent(info: &ds_core::map_engine::RasterInfo) -> Option<serde_json::Value> {
    let mut extent = serde_json::Map::new();

    if let Some(bbox) = info.spatial_extent {
        let mut spatial = json!({
            "bbox": [bbox],
            "crs": "http://www.opengis.net/def/crs/OGC/1.3/CRS84"
        });
        // Per-axis grid resolution in CRS84 degrees, derived from the native
        // cell counts and the WGS84 bbox [west, south, east, north].
        if let Some([nx, ny]) = info.grid_size {
            if nx > 0 && ny > 0 {
                let [w, s, e, n] = bbox;
                spatial["grid"] = json!([
                    { "cellsCount": nx, "resolution": (e - w) / nx as f64 },
                    { "cellsCount": ny, "resolution": (n - s) / ny as f64 }
                ]);
            }
        }
        extent.insert("spatial".to_string(), spatial);
    }

    if !info.times.is_empty() {
        let start = info.times.first().map(|t| t.to_rfc3339());
        let end = info.times.last().map(|t| t.to_rfc3339());
        if let (Some(start), Some(end)) = (start, end) {
            let mut temporal = json!({
                "interval": [[start, end]],
                "trs": "http://www.opengis.net/def/uom/ISO-8601/0/Gregorian"
            });
            if let Some(grid) = temporal_grid(&info.times) {
                temporal["grid"] = grid;
            }
            extent.insert("temporal".to_string(), temporal);
        }
    }

    // Vertical — keep `interval` + `values` (back-compat) and additively add
    // the OGC API Common Part 2 form (`unit` + `grid.coordinates`). `vrs` is
    // omitted: the only kind in use (radar elevation angle) has no standard
    // OGC vertical-CRS URI.
    if let Some(vertical) = &info.vertical {
        let levels = &vertical.levels;
        extent.insert(
            "vertical".to_string(),
            json!({
                "interval": vertical.extent().map(|(lo, hi)| [[lo, hi]]),
                "values": levels,
                "unit": vertical.unit(),
                "grid": { "coordinates": levels }
            }),
        );
    }

    if extent.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(extent))
    }
}

/// Build the `extent.temporal.grid` descriptor from the ordered timestamps. A
/// regular step yields `{cellsCount, resolution}` (ISO 8601 duration); an
/// irregular series yields `{cellsCount, coordinates}`. Returns `None` for
/// fewer than two timestamps.
fn temporal_grid(times: &[chrono::DateTime<chrono::Utc>]) -> Option<serde_json::Value> {
    if times.len() < 2 {
        return None;
    }
    let cells = times.len();
    let gaps: Vec<i64> = times
        .windows(2)
        .map(|w| (w[1] - w[0]).num_seconds())
        .collect();
    let min = *gaps.iter().min().unwrap();
    let max = *gaps.iter().max().unwrap();
    // Regular when every gap agrees within a 2-second spread (absorbs ±1 s
    // rounding/leap-second jitter on any interval, not just the first) and is
    // strictly positive. Anchor the advertised resolution to the rounded mean
    // step so a jittered endpoint can't bias it.
    if min > 0 && max - min <= 2 {
        let count = gaps.len() as i64;
        let avg = (gaps.iter().sum::<i64>() + count / 2) / count;
        if let Some(resolution) = ds_core::datetime::format_iso8601_duration(avg) {
            return Some(json!({ "cellsCount": cells, "resolution": resolution }));
        }
    }
    let coordinates: Vec<String> = times.iter().map(|t| t.to_rfc3339()).collect();
    Some(json!({ "cellsCount": cells, "coordinates": coordinates }))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /maps/ — Landing page
pub async fn landing_page(State(state): State<AppState>) -> impl IntoResponse {
    let state = state.load_full();
    let base = &state.base_url;
    Json(json!({
        "title": "MeteoCore - Maps",
        "description": "Metocean Data Server — OGC API Maps",
        "links": [
            {
                "href": format!("{base}/maps/"),
                "rel": "self",
                "type": "application/json",
                "title": "This document"
            },
            {
                "href": format!("{base}/maps/api"),
                "rel": "service-desc",
                "type": "application/vnd.oai.openapi+json;version=3.0",
                "title": "API definition"
            },
            {
                "href": format!("{base}/maps/api/docs"),
                "rel": "service-doc",
                "type": "text/html",
                "title": "API documentation"
            },
            {
                "href": format!("{base}/maps/conformance"),
                "rel": "conformance",
                "type": "application/json",
                "title": "Conformance classes"
            },
            {
                "href": format!("{base}/maps/collections"),
                "rel": "data",
                "type": "application/json",
                "title": "Collections"
            }
        ]
    }))
}

/// GET /maps/api — OpenAPI 3.0.3 definition
pub async fn api_definition(State(state): State<AppState>) -> impl IntoResponse {
    let state = state.load_full();
    let mut collection_paths = json!({});
    for config in state.collections.values() {
        let id = &config.id;

        // GET /maps/collections/{id}
        let detail_path = format!("/maps/collections/{id}");
        collection_paths[&detail_path] = json!({
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

        // GET /maps/collections/{id}/map
        let map_path = format!("/maps/collections/{id}/map");
        collection_paths[&map_path] = json!({
            "get": {
                "summary": format!("Get map for {}", config.title),
                "operationId": format!("getMap_{id}"),
                "tags": [id],
                "parameters": [
                    {"$ref": "#/components/parameters/bbox"},
                    {"$ref": "#/components/parameters/width"},
                    {"$ref": "#/components/parameters/height"},
                    {"$ref": "#/components/parameters/crs"},
                    {"$ref": "#/components/parameters/datetime"},
                    {"$ref": "#/components/parameters/transparent"},
                    {"$ref": "#/components/parameters/f"},
                    {"$ref": "#/components/parameters/bbox-crs"},
                    {"$ref": "#/components/parameters/elevation"}
                ],
                "responses": {
                    "200": {
                        "description": "Map image",
                        "content": {
                            "image/png": {
                                "schema": {"type": "string", "format": "binary"}
                            },
                            "image/jpeg": {
                                "schema": {"type": "string", "format": "binary"}
                            },
                            "image/webp": {
                                "schema": {"type": "string", "format": "binary"}
                            }
                        }
                    },
                    "400": {"description": "Bad request"},
                    "404": {"description": "Collection not found"},
                    "500": {"description": "Server error"}
                }
            }
        });

        // GET /maps/collections/{id}/styles
        let styles_path = format!("/maps/collections/{id}/styles");
        collection_paths[&styles_path] = json!({
            "get": {
                "summary": format!("List styles for {}", config.title),
                "operationId": format!("getStyles_{id}"),
                "tags": [id],
                "responses": {
                    "200": {
                        "description": "List of styles",
                        "content": {
                            "application/json": {
                                "schema": {"$ref": "#/components/schemas/styleList"}
                            }
                        }
                    },
                    "404": {"description": "Collection not found"},
                    "500": {"description": "Server error"}
                }
            }
        });

        // GET /maps/collections/{id}/styles/{styleId}/map
        let styled_map_path = format!("/maps/collections/{id}/styles/{{styleId}}/map");
        collection_paths[&styled_map_path] = json!({
            "get": {
                "summary": format!("Get styled map for {}", config.title),
                "operationId": format!("getStyledMap_{id}"),
                "tags": [id],
                "parameters": [
                    {
                        "name": "styleId",
                        "in": "path",
                        "required": true,
                        "schema": {"type": "string"},
                        "description": "Style identifier"
                    },
                    {"$ref": "#/components/parameters/bbox"},
                    {"$ref": "#/components/parameters/width"},
                    {"$ref": "#/components/parameters/height"},
                    {"$ref": "#/components/parameters/crs"},
                    {"$ref": "#/components/parameters/datetime"},
                    {"$ref": "#/components/parameters/transparent"},
                    {"$ref": "#/components/parameters/f"},
                    {"$ref": "#/components/parameters/bbox-crs"},
                    {"$ref": "#/components/parameters/elevation"}
                ],
                "responses": {
                    "200": {
                        "description": "Map image",
                        "content": {
                            "image/png": {
                                "schema": {"type": "string", "format": "binary"}
                            },
                            "image/jpeg": {
                                "schema": {"type": "string", "format": "binary"}
                            },
                            "image/webp": {
                                "schema": {"type": "string", "format": "binary"}
                            }
                        }
                    },
                    "400": {"description": "Bad request"},
                    "404": {"description": "Collection or style not found"},
                    "500": {"description": "Server error"}
                }
            }
        });
    }

    let mut paths = json!({
        "/maps/": {
            "get": {
                "summary": "Landing page",
                "operationId": "getLandingPage",
                "responses": {
                    "200": {"description": "Landing page"}
                }
            }
        },
        "/maps/conformance": {
            "get": {
                "summary": "Conformance classes",
                "operationId": "getConformance",
                "responses": {
                    "200": {"description": "Conformance classes"}
                }
            }
        },
        "/maps/collections": {
            "get": {
                "summary": "List collections",
                "operationId": "getCollections",
                "responses": {
                    "200": {"description": "List of collections"}
                }
            }
        }
    });

    // Merge collection paths into main paths
    if let (Some(main_obj), Some(coll_obj)) = (paths.as_object_mut(), collection_paths.as_object())
    {
        for (k, v) in coll_obj {
            main_obj.insert(k.clone(), v.clone());
        }
    }

    let openapi = json!({
        "openapi": "3.0.3",
        "info": {
            "title": "MeteoCore - OGC API Maps",
            "version": "1.0.0",
            "description": "OGC API - Maps implementation"
        },
        "paths": paths,
        "components": {
            "parameters": {
                "bbox": {
                    "name": "bbox",
                    "in": "query",
                    "required": true,
                    "schema": {"type": "string"},
                    "description": "Bounding box: west,south,east,north"
                },
                "width": {
                    "name": "width",
                    "in": "query",
                    "required": false,
                    "schema": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 8000,
                        "default": 256
                    },
                    "description": "Image width in pixels"
                },
                "height": {
                    "name": "height",
                    "in": "query",
                    "required": false,
                    "schema": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 8000,
                        "default": 256
                    },
                    "description": "Image height in pixels"
                },
                "crs": {
                    "name": "crs",
                    "in": "query",
                    "required": false,
                    "schema": {
                        "type": "string",
                        "default": "CRS:84",
                        "enum": ["CRS:84", "EPSG:4326", "EPSG:3857", "EPSG:3067", "EPSG:3035"]
                    },
                    "description": "Coordinate reference system"
                },
                "datetime": {
                    "name": "datetime",
                    "in": "query",
                    "required": false,
                    "schema": {"type": "string"},
                    "description": "ISO 8601 timestamp"
                },
                "transparent": {
                    "name": "transparent",
                    "in": "query",
                    "required": false,
                    "schema": {"type": "string"},
                    "description": "Transparency support"
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
                    "description": "Output format. `image/png` auto-emits an 8-bit indexed-palette PNG (~3–4× smaller) for colormap-rendered layers; falls back to 32-bit RGBA above 256 distinct colours."
                },
                "bbox-crs": {
                    "name": "bbox-crs",
                    "in": "query",
                    "required": false,
                    "schema": {"type": "string"},
                    "description": "CRS for bbox coordinates. Only CRS:84 supported."
                },
                "elevation": {
                    "name": "elevation",
                    "in": "query",
                    "required": false,
                    "schema": {"type": "number"},
                    "description": "Vertical level (e.g. radar elevation angle). Only valid for collections with a vertical dimension."
                }
            },
            "schemas": {
                "styleList": {
                    "type": "object",
                    "properties": {
                        "styles": {
                            "type": "array",
                            "items": {"$ref": "#/components/schemas/style"}
                        },
                        "links": {"type": "array", "items": {"$ref": "#/components/schemas/link"}}
                    }
                },
                "style": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "title": {"type": "string"},
                        "links": {"type": "array", "items": {"$ref": "#/components/schemas/link"}}
                    }
                },
                "link": {
                    "type": "object",
                    "required": ["href"],
                    "properties": {
                        "href": {"type": "string"},
                        "rel": {"type": "string"},
                        "type": {"type": "string"},
                        "title": {"type": "string"}
                    }
                }
            }
        }
    });

    Json(openapi)
}

/// GET /maps/api/docs — Swagger UI
pub async fn api_docs(State(state): State<AppState>) -> impl IntoResponse {
    let state = state.load_full();
    let spec_url = format!("{}/maps/api", state.base_url);
    axum::response::Html(ds_core::openapi::swagger_ui_html(
        "MeteoCore - Maps API",
        &spec_url,
    ))
}

/// GET /maps/conformance
pub async fn conformance() -> impl IntoResponse {
    Json(json!({
        "conformsTo": [
            "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/core",
            "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/collection-map",
            "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/styled-map",
            "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/spatial-subsetting",
            "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/scaling",
            "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/datetime",
            "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/crs",
            "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/png",
            "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/jpeg",
            "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/tilesets"
        ]
    }))
}

/// GET /maps/collections
pub async fn collections(State(state): State<AppState>) -> impl IntoResponse {
    let state = state.load_full();
    let base = &state.base_url;
    let mut colls: Vec<serde_json::Value> = state
        .collections
        .values()
        .filter_map(|config| {
            let engine = state.engines.get(&config.id)?;
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
        "links": [
            {
                "href": format!("{base}/maps/collections"),
                "rel": "self",
                "type": "application/json"
            }
        ]
    }))
}

/// GET /maps/collections/{id}
pub async fn collection(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, MapsError> {
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

/// GET /maps/collections/{id}/styles
pub async fn styles(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, MapsError> {
    let state = state.load_full();
    let (_engine, config) = lookup_engine(&state, &id)?;
    let base = &state.base_url;

    let mut style_list = Vec::new();
    if let Some(layer_styles) = state.styles.get(&id) {
        let mut names: Vec<&String> = layer_styles.keys().collect();
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
            if let Some(s) = layer_styles.get(name) {
                style_list.push(json!({
                    "id": s.name,
                    "title": s.title,
                    "links": [
                        {
                            "href": format!("{base}/maps/collections/{}/styles/{}/map", config.id, s.name),
                            "rel": "map",
                            "type": "image/png"
                        }
                    ]
                }));
            }
        }
    }

    Ok(Json(json!({
        "styles": style_list,
        "links": [
            {
                "href": format!("{base}/maps/collections/{}/styles", id),
                "rel": "self",
                "type": "application/json"
            }
        ]
    })))
}

/// GET /maps/collections/{id}/map — render map with default style
pub async fn get_map(
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<MapQueryParams>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, MapsError> {
    render_map(&id, "default", params, headers, state).await
}

/// GET /maps/collections/{id}/styles/{styleId}/map — render map with named style
pub async fn get_styled_map(
    headers: HeaderMap,
    Path((id, style_id)): Path<(String, String)>,
    Query(params): Query<MapQueryParams>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, MapsError> {
    render_map(&id, &style_id, params, headers, state).await
}

/// Shared rendering logic for get_map and get_styled_map.
async fn render_map(
    collection_id: &str,
    style_name: &str,
    params: MapQueryParams,
    headers: HeaderMap,
    state: AppState,
) -> Result<impl IntoResponse, MapsError> {
    let state = state.load_full();
    let (engine, _config) = lookup_engine(&state, collection_id)?;

    let validated = params.validate()?;

    // Look up style
    let layer_styles = state
        .styles
        .get(collection_id)
        .ok_or_else(|| MapsError::NotFound(format!("Collection '{collection_id}' not found")))?;

    let style_info = layer_styles.get(style_name).ok_or_else(|| {
        MapsError::NotFound(format!(
            "Style '{style_name}' not found for collection '{collection_id}'. Available: {}",
            layer_styles.keys().cloned().collect::<Vec<_>>().join(", ")
        ))
    })?;

    let colormap = style_info.colormap.clone();
    let content_type = validated.format.content_type();
    let has_explicit_time = validated.time.is_some();
    let content_crs = crs_to_uri(&validated.crs);

    // Single `raster_info()` call covers both default-time resolution and
    // parameter-name validation. The trait now documents this as O(1), but
    // hoisting still saves one Arc-clone-sized allocation per request on
    // engines that materialise `RasterInfo` lazily.
    let raster_info = engine.raster_info();
    let time = validated.time.or_else(|| raster_info.times.last().copied());

    // Parameter selection precedence: ?parameter-name= wins over style.parameter.
    // Validate against the engine's advertised list when the query supplied one
    // — passing through unrecognised names produces a confusing "default
    // parameter rendered with wrong colormap" rather than a clear 400.
    // `raster_info().parameters` is empty for single-parameter engines (GeoTIFF);
    // in that case we just accept the query value and let the engine ignore it,
    // matching `get_raster_tile`'s documented behavior.
    if let Some(pname) = validated.parameter_name.as_deref() {
        if !raster_info.parameters.is_empty()
            && !raster_info.parameters.iter().any(|(name, _)| name == pname)
        {
            let mut supported: Vec<&str> = raster_info
                .parameters
                .iter()
                .map(|(n, _)| n.as_str())
                .collect();
            // Sort so the error message is deterministic. `raster_info()`
            // returns parameters in engine-defined order — fine for GRIB
            // today but a future HashMap-backed engine would surface a
            // different ordering per request, confusing both log greppers
            // and clients that try to match against the hint.
            supported.sort_unstable();
            return Err(MapsError::BadRequest(format!(
                "parameter-name '{pname}' is not available for collection '{collection_id}'. \
                 Available: {}",
                supported.join(", ")
            )));
        }
    }
    let effective_parameter = validated
        .parameter_name
        .clone()
        .or_else(|| style_info.parameter.clone());

    // Reject an `elevation` against a collection with no vertical axis
    // rather than silently rendering the default layer.
    if validated.z.is_some() && raster_info.vertical.is_none() {
        return Err(MapsError::BadRequest(format!(
            "collection '{collection_id}' has no vertical dimension; \
             the `elevation` parameter is not supported"
        )));
    }

    // Build cache key
    let cache_key = CacheKey {
        layer: collection_id.to_string(),
        style: style_name.to_string(),
        format: match validated.format {
            ds_render::ImageFormat::Png => 0,
            ds_render::ImageFormat::Jpeg => 1,
            ds_render::ImageFormat::Webp => 2,
        },
        crs: validated.crs.clone(),
        bbox: ds_render::quantize_bbox(&validated.bbox),
        width: validated.width,
        height: validated.height,
        time,
        parameter: effective_parameter.clone(),
        z: validated.z.map(ds_render::quantize_z),
    };

    let cache_control = cache_control_value(has_explicit_time);
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|h| h.to_str().ok())
        .map(str::to_string);

    // Cache lookup runs BEFORE the If-None-Match check. The ETag is
    // content-derived (see `CachedRendered::new`), so a key-derived 304
    // short-circuit would be wrong: it would let a browser holding the
    // pre-fix entry keep getting 304 after the server starts producing
    // different pixels. Mirror the MVT path in `render_vector_tile`
    // (the bug #145 fixed for raster tiles).
    if let Some(cached) = state.rendered_cache.get(&cache_key) {
        if let Some(ref inm) = if_none_match {
            if ds_render::etag_matches(inm, cached.etag()) {
                // 304 from the cache-HIT branch. The `x-cache: HIT` header
                // lets the regression test (and curious clients) distinguish
                // this from a post-render MISS→304, which the handler also
                // serves.
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
            .header(header::HeaderName::from_static("content-crs"), content_crs)
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
    let _permit = tokio::time::timeout(ds_render::RENDER_TIMEOUT, state.render_semaphore.acquire())
        .await
        .map_err(|_| MapsError::ServiceUnavailable("Server busy, try again later".to_string()))?
        .map_err(|_| MapsError::Internal("Render semaphore closed".to_string()))?;

    // Render on a blocking thread
    let engine = engine.clone();
    let bbox = validated.bbox;
    let width = validated.width;
    let height = validated.height;
    let output_crs = validated.output_crs;
    let format = validated.format;
    let rendered_cache = state.rendered_cache.clone();

    let render_parameter = effective_parameter;
    let render_z = validated.z;

    let render_result = tokio::task::spawn_blocking(move || {
        let tile = engine.get_raster_tile(
            bbox,
            width,
            height,
            time,
            &output_crs,
            render_parameter.as_deref(),
            render_z,
        )?;
        // If every pixel is nodata, skip colorization + encoding entirely.
        if tile.is_empty() {
            return Ok(None);
        }
        ds_render::render_tile(&tile, colormap.as_ref(), format).map(Some)
    })
    .await
    .map_err(|e| MapsError::Internal(format!("Render task failed: {e}")))?;

    // The EMPTY fast path skips the format-aware encoder and emits PNG
    // bytes directly. Track the actual Content-Type per branch so the
    // header never lies about the payload (#162). Wrap every branch in
    // `CachedRendered` so the response ETag is FNV-1a over the actual
    // bytes — different pixels, different ETag — regardless of which
    // exit we take (#145).
    // Each arm produces a `CachedRendered` ready to serve. Only the
    // populated `Ok(Some(_))` path inserts into the rendered cache; the
    // EMPTY fast-path intentionally doesn't (its bytes are deterministic
    // for fixed dimensions). Engine errors bail with 500 before this
    // match.
    let (cached, x_cache, response_content_type) = match render_result {
        Ok(Some(bytes)) => {
            let cached = ds_render::CachedRendered::new(bytes::Bytes::from(bytes));
            rendered_cache.insert(cache_key, cached.clone());
            (cached, "MISS", content_type)
        }
        Ok(None) => {
            // Empty tile: transparent PNG, never cached.
            let rgba = vec![0u8; (width * height * 4) as usize];
            let png = ds_render::encode_png(&rgba, width, height)
                .map_err(|e| MapsError::Internal(format!("Failed to encode empty tile: {e}")))?;
            let cached = ds_render::CachedRendered::new(bytes::Bytes::from(png));
            (cached, "EMPTY", "image/png")
        }
        Err(e) => {
            tracing::warn!("Maps render error for collection '{}': {e}", collection_id);
            return Err(MapsError::Internal(format!("Render failed: {e}")));
        }
    };

    // Content-derived ETag now available — do the `If-None-Match`
    // comparison here, after rendering. Same flow as `render_vector_tile`
    // in api-tiles. Forward the same `x_cache` label the 200 response
    // would carry (`"MISS"` or `"EMPTY"`) so revalidations look the
    // same on dashboards as initial fetches — a client revalidating a
    // cached transparent-tile response sees `304 x-cache: EMPTY`, not
    // a misleading `MISS`.
    if let Some(ref inm) = if_none_match {
        if ds_render::etag_matches(inm, cached.etag()) {
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
        .header(header::HeaderName::from_static("content-crs"), content_crs)
        .header(
            header::HeaderName::from_static("x-content-type-options"),
            "nosniff",
        )
        .header(header::HeaderName::from_static("x-cache"), x_cache)
        .body(axum::body::Body::from(cached.into_bytes()))
        .unwrap()
        .into_response())
}
