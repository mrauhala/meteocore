use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Extension;
use axum::Json;
use serde_json::json;

use api_common::workbench::Surface;
use api_common::{mounts, rel, Mount};
use ds_core::config::CollectionConfig;
use ds_core::map_engine::MapEngine;
use ds_render::{CacheKey, RenderedCache, StyleInfo};

use crate::error::MapsError;
use crate::params::{self, LegendFormat, LegendQueryParams, MapQueryParams};

/// Shared state for the OGC API Maps service.
#[derive(Clone)]
pub struct MapsState {
    pub engines: HashMap<String, Arc<dyn MapEngine>>,
    pub collections: HashMap<String, CollectionConfig>,
    /// Map of collection_id -> style_name -> StyleInfo.
    pub styles: HashMap<String, HashMap<String, StyleInfo>>,
    pub render_semaphore: Arc<tokio::sync::Semaphore>,
    pub rendered_cache: Arc<RenderedCache>,
    /// Static fallback base URL for absolute links. Used as-is unless
    /// `trust_proxy_headers` resolves a per-request value.
    pub base_url: String,
    /// Honour reverse-proxy forwarding headers when generating self-links (#12).
    pub trust_proxy_headers: bool,
    /// Collections the Tiles service renders as map tiles. The `tilesets-map`
    /// link is advertised only for these, so it never names a tileset list
    /// that does not exist (`apis` alone cannot tell, #789).
    pub map_tileset_ids: HashSet<String>,
}

pub type AppState = Arc<ArcSwap<MapsState>>;

/// Resolve the absolute base URL for the current request, honouring reverse-proxy
/// forwarding headers when `trust_proxy_headers` is enabled (#12).
pub(crate) fn request_base_url(state: &MapsState, headers: &HeaderMap) -> String {
    ds_core::proxy::resolve_base_url(&state.base_url, state.trust_proxy_headers, |name| {
        headers.get(name).and_then(|v| v.to_str().ok())
    })
}

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

/// Resolve the style map a request should be styled from.
///
/// Multi-parameter collections register one style map per parameter under
/// `"{collection}/{param}"` alongside the collection-level map, and only the
/// per-parameter map carries that parameter's colormap (from
/// `[[wms.parameters]]`, a bundle parameter entry, or a built-in parameter
/// default). A request that names a parameter must therefore resolve its
/// style there first, falling back to the collection map when the collection
/// has no such layer (single-parameter engines, unknown parameter names that
/// the engine ignores). Mirrors how WMS GetMap resolves `LAYERS=coll/param`.
fn layer_style_map<'a>(
    styles: &'a HashMap<String, HashMap<String, StyleInfo>>,
    collection_id: &str,
    parameter: Option<&str>,
) -> Option<(String, &'a HashMap<String, StyleInfo>)> {
    if let Some(p) = parameter {
        let key = format!("{collection_id}/{p}");
        if let Some(m) = styles.get(&key) {
            return Some((key, m));
        }
    }
    styles
        .get(collection_id)
        .map(|m| (collection_id.to_string(), m))
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

/// Cache-Control header value: `immutable` (24 h) only for an explicit
/// `time` over content the engine never revises (`content_version == 0`);
/// "latest" and in-place-revised content (a push-fed alert set) get 60 s +
/// revalidation so a browser/CDN holding a pre-revision image asks again.
fn cache_control_value(has_explicit_time: bool, content_version: u64) -> &'static str {
    if has_explicit_time && content_version == 0 {
        "public, max-age=86400, immutable"
    } else {
        "public, max-age=60, must-revalidate"
    }
}

/// Resolve the requested representation from `?f=` + the `Accept` header.
fn negotiate(f: Option<&str>, headers: &HeaderMap) -> Result<ds_core::html::Wanted, MapsError> {
    let accept = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok());
    ds_core::html::negotiate(f, accept).map_err(|e| MapsError::BadRequest(e.to_string()))
}

/// Tag a content-negotiated response with `Vary: Accept` so shared caches
/// don't serve the JSON body to a client that asked for HTML (or vice versa).
fn with_vary(mut resp: Response) -> Response {
    // `append` (not `insert`) so a `Vary` set upstream (e.g. compression's
    // `Vary: Accept-Encoding`) isn't clobbered.
    resp.headers_mut().append(
        axum::http::header::VARY,
        axum::http::HeaderValue::from_static("accept"),
    );
    resp
}

/// The link entries advertised for one style: the styled-map endpoint and the
/// machine-readable legend. One builder so the `/collections/{id}` and
/// `/collections/{id}/styles` representations can't drift. `root` is the
/// absolute API root (base URL + mount).
///
/// Relations are the registered OGC ones only (Maps Req 53 styled-map links,
/// the legend recommendation): the short `map`/`legend` forms were dropped once
/// no client depended on them (a bare unregistered relation type is not an
/// RFC 8288 extension relation).
fn style_links(collection_id: &str, style_name: &str, root: &str) -> serde_json::Value {
    let map = format!("{root}/collections/{collection_id}/styles/{style_name}/map");
    let legend = format!("{root}/collections/{collection_id}/styles/{style_name}/legend");
    json!([
        {"href": map, "rel": rel::MAP, "type": "image/png"},
        {"href": legend, "rel": rel::LEGEND, "type": "application/json"}
    ])
}

/// Maps' description of a collection: its standard fields and data-access
/// links (map, styles), without the `self` link. Shared by the per-API
/// service and the shared OGC API root (#789).
pub(crate) fn collection_parts(
    config: &CollectionConfig,
    info: &ds_core::map_engine::RasterInfo,
    styles: Option<&HashMap<String, StyleInfo>>,
    root: &str,
) -> (
    serde_json::Map<String, serde_json::Value>,
    Vec<serde_json::Value>,
) {
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
                    "links": style_links(&config.id, &s.name, root)
                }));
            }
        }
    }

    let links = vec![
        // The registered relation Maps Req 46 requires (and the Maps test
        // suite looks for); `…/styles` is the Styles draft's.
        json!({
            "href": format!("{root}/collections/{}/map", config.id),
            "rel": rel::MAP,
            "type": "image/png",
            "title": "Map"
        }),
        json!({
            "href": format!("{root}/collections/{}/styles", config.id),
            "rel": rel::STYLES,
            "type": "application/json",
            "title": "Styles"
        }),
    ];

    let mut fields = serde_json::Map::new();
    fields.insert("dataType".into(), json!("map"));
    fields.insert("crs".into(), json!(crs_uris));
    fields.insert("styles".into(), json!(style_list));
    // Only advertise `storageCrs` when the native CRS has a stable OGC URI.
    // Engines label projected/rotated grids with internal names ("TM",
    // "LAEA", "projected", "rotated_ll", …) that have no URI; emitting CRS84
    // for those would mislabel the storage grid, so omit it instead.
    if let Some(storage_crs) = ds_core::geo::native_crs_uri(&info.native_crs) {
        fields.insert("storageCrs".into(), json!(storage_crs));
    }
    if let Some(extent) = build_extent(info) {
        fields.insert("extent".into(), extent);
    }
    (fields, links)
}

fn build_collection_metadata(
    config: &CollectionConfig,
    info: &ds_core::map_engine::RasterInfo,
    styles: Option<&HashMap<String, StyleInfo>>,
    map_tilesets: bool,
    base_url: &str,
    root: &str,
) -> serde_json::Value {
    let (fields, access) = collection_parts(config, info, styles, root);
    let mut links = vec![json!({
        "href": format!("{root}/collections/{}", config.id),
        "rel": "self",
        "type": "application/json",
        "title": config.title
    })];
    links.extend(access);

    // Map tilesets — rendered (raster) tiles are an OGC API Maps "map
    // tileset", discoverable from the maps collection via the `tilesets-map`
    // relation. Only advertise it when the Tiles service actually registered
    // this collection for raster tiles (the per-API `/tiles` router serves it).
    if map_tilesets {
        links.push(json!({
            "href": format!("{base_url}{}/collections/{}/tiles", mounts::TILES, config.id),
            "rel": rel::TILESETS_MAP,
            "type": "application/json",
            "title": "Map tilesets"
        }));
    }

    api_common::collection_metadata(config, serde_json::Value::Object(fields), links)
}

/// Build the OGC API Common Part 2 `extent` object (spatial, temporal,
/// vertical) including the `grid` resolution descriptors. Returns `None` when
/// the collection advertises no spatial, temporal, or vertical extent.
///
/// The assembly lives in `ds_core::ogc_extent` so Maps, Tiles, and Features
/// share one definition (issue #263).
fn build_extent(info: &ds_core::map_engine::RasterInfo) -> Option<serde_json::Value> {
    let extent = ds_core::ogc_extent::build_extent(
        info.spatial_extent,
        info.grid_size,
        &info.native_crs,
        &info.times,
        info.vertical.as_ref(),
    )?;
    Some(serde_json::to_value(extent).expect("Extent serializes to JSON"))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET {mount}/ — Landing page
pub async fn landing_page(
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    Query(fp): Query<ds_core::html::FormatParams>,
    headers: HeaderMap,
) -> Result<Response, MapsError> {
    use ds_core::html::{LinkView, Wanted};
    let wanted = negotiate(fp.f.as_deref(), &headers)?;
    let state = state.load_full();
    let base = &request_base_url(&state, &headers);
    let root = &mount.root(base);
    let title = "MeteoCore - Maps";
    let description = "Metocean Data Server — OGC API Maps";
    // (href, rel, type, title) — one source for both representations.
    let links = [
        (
            format!("{root}/"),
            "self",
            "application/json",
            "This document",
        ),
        (
            format!("{root}/api"),
            "service-desc",
            "application/vnd.oai.openapi+json;version=3.0",
            "API definition",
        ),
        (
            format!("{root}/api/docs"),
            "service-doc",
            "text/html",
            "API documentation",
        ),
        (
            format!("{root}/conformance"),
            "conformance",
            "application/json",
            "Conformance classes",
        ),
        (
            format!("{root}/conformance"),
            rel::CONFORMANCE,
            "application/json",
            "Conformance classes",
        ),
        (
            format!("{root}/collections"),
            "data",
            "application/json",
            "Collections",
        ),
        (
            format!("{root}/collections"),
            rel::DATA,
            "application/json",
            "Collections",
        ),
    ];
    Ok(with_vary(match wanted {
        Wanted::Json => {
            let json_links: Vec<_> = links
                .iter()
                .map(|(h, r, t, ti)| json!({ "href": h, "rel": r, "type": t, "title": ti }))
                .collect();
            Json(json!({ "title": title, "description": description, "links": json_links }))
                .into_response()
        }
        Wanted::Html => {
            let mut views: Vec<LinkView> = links
                .iter()
                .map(|(h, r, _, ti)| LinkView::new(h.clone(), *r, Some(ti)))
                .collect();
            // rel="alternate" to the JSON representation (parity with the
            // collection-detail HTML page).
            views.push(LinkView::new(
                format!("{root}/?f=json"),
                "alternate",
                Some("This document as JSON"),
            ));
            Html(api_common::workbench::landing_html(
                Surface {
                    base,
                    root,
                    api: "maps",
                },
                title,
                description,
                &views,
            ))
            .into_response()
        }
    }))
}

/// OpenAPI `f` (output-format) query parameter, shared by the content-negotiated
/// metadata endpoints (landing, conformance, collections, collection detail).
fn format_parameter() -> serde_json::Value {
    json!({"name": "f", "in": "query", "required": false, "schema": {"type": "string", "enum": ["json", "html"]},
           "description": "Output format. 'json' (default) or 'html'; overrides the Accept header."})
}

/// Per-collection OpenAPI paths (detail, map, styles, styled map, legend),
/// keyed below the mount `m`.
pub(crate) fn collection_openapi_paths(
    state: &MapsState,
    m: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let mut collection_paths = json!({});
    for config in state.collections.values() {
        let id = &config.id;

        // GET {mount}/collections/{id}
        let detail_path = format!("{m}/collections/{id}");
        collection_paths[&detail_path] = json!({
            "get": {
                "summary": format!("Get {} collection metadata", config.title),
                "operationId": format!("getCollection_{id}"),
                "tags": [id],
                "parameters": [format_parameter()],
                "responses": {
                    "200": {"description": "Collection metadata"},
                    "404": {"description": "Collection not found"}
                }
            }
        });

        // GET {mount}/collections/{id}/map
        let map_path = format!("{m}/collections/{id}/map");
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

        // GET {mount}/collections/{id}/styles
        let styles_path = format!("{m}/collections/{id}/styles");
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

        // GET {mount}/collections/{id}/styles/{styleId}/map
        let styled_map_path = format!("{m}/collections/{id}/styles/{{styleId}}/map");
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

        // GET {mount}/collections/{id}/styles/{styleId}/legend
        let legend_path = format!("{m}/collections/{id}/styles/{{styleId}}/legend");
        collection_paths[&legend_path] = json!({
            "get": {
                "summary": format!("Get style legend for {}", config.title),
                "operationId": format!("getStyleLegend_{id}"),
                "tags": [id],
                "parameters": [
                    {
                        "name": "styleId",
                        "in": "path",
                        "required": true,
                        "schema": {"type": "string"},
                        "description": "Style identifier"
                    },
                    {
                        "name": "f",
                        "in": "query",
                        "required": false,
                        "schema": {"type": "string", "default": "json", "enum": ["json", "application/json", "png", "image/png"]},
                        "description": "Legend representation: the machine-readable description (default) or a rendered legend image."
                    },
                    {
                        "name": "parameter-name",
                        "in": "query",
                        "required": false,
                        "schema": {"type": "string"},
                        "description": "Describe the style of this parameter's layer, matching `parameter-name` on the map routes. Falls back to the collection-level style when the collection has no per-parameter layer."
                    }
                ],
                "responses": {
                    "200": {
                        "description": "Legend description or image",
                        "content": {
                            "application/json": {
                                "schema": {"$ref": "#/components/schemas/legend"}
                            },
                            "image/png": {
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

    match collection_paths {
        serde_json::Value::Object(paths) => paths,
        _ => serde_json::Map::new(),
    }
}

/// OpenAPI components (parameters, schemas) referenced by the Maps paths.
pub(crate) fn openapi_components() -> serde_json::Value {
    json!({
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
            "legend": {
                "type": "object",
                "required": ["style", "title", "min", "max", "interpolation", "stops"],
                "properties": {
                    "style": {"type": "string", "description": "Style identifier"},
                    "title": {"type": "string", "description": "Human-readable style title"},
                    "parameter": {"type": "string", "description": "Data parameter the style renders. Omitted when unknown."},
                    "unit": {"type": "string", "description": "Unit of the rendered values. Omitted when unknown."},
                    "min": {"type": "number", "description": "Low end of the value range the colours span"},
                    "max": {"type": "number", "description": "High end of the value range the colours span"},
                    "interpolation": {"type": "string", "enum": ["linear", "step"],
                                      "description": "How colours are produced between stops"},
                    "nodataColor": {"type": "string", "description": "Colour for no-data pixels, when the palette defines one."},
                    "stops": {
                        "type": "array",
                        "description": "Palette colour stops, ascending by value",
                        "items": {
                            "type": "object",
                            "required": ["value", "color"],
                            "properties": {
                                "value": {"type": "number"},
                                "color": {"type": "string", "description": "#RRGGBB, or #RRGGBBAA when not fully opaque"}
                            }
                        }
                    }
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
    })
}

/// GET {mount}/api — OpenAPI 3.0.3 definition. Path keys include the mount.
pub async fn api_definition(
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
) -> impl IntoResponse {
    let state = state.load_full();
    let m = mount.0;
    let collection_paths = serde_json::Value::Object(collection_openapi_paths(&state, m));
    let mut paths = json!({
        format!("{m}/"): {
            "get": {
                "summary": "Landing page",
                "operationId": "getLandingPage",
                "tags": [api_common::openapi_tags::DISCOVERY],
                "parameters": [format_parameter()],
                "responses": {
                    "200": {"description": "Landing page"}
                }
            }
        },
        format!("{m}/conformance"): {
            "get": {
                "summary": "Conformance classes",
                "operationId": "getConformance",
                "tags": [api_common::openapi_tags::DISCOVERY],
                "parameters": [format_parameter()],
                "responses": {
                    "200": {"description": "Conformance classes"}
                }
            }
        },
        format!("{m}/collections"): {"get": api_common::collection_operation()}
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
        "components": openapi_components()
    });

    Json(openapi)
}

/// GET {mount}/api/docs — Swagger UI
pub async fn api_docs(
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let state = state.load_full();
    let spec_url = format!("{}/api", mount.root(&request_base_url(&state, &headers)));
    (
        [
            (
                header::CONTENT_SECURITY_POLICY,
                ds_core::openapi::SWAGGER_UI_CSP,
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        axum::response::Html(ds_core::openapi::swagger_ui_html(
            "MeteoCore - Maps API",
            &spec_url,
        )),
    )
}

/// Pinned Swagger assets embedded in ds-core, available under each API root.
pub async fn api_docs_asset(Path(asset): Path<String>) -> Response {
    match ds_core::openapi::swagger_ui_asset(&asset) {
        Some((content_type, bytes)) => (
            [
                (header::CONTENT_TYPE, content_type),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (header::CACHE_CONTROL, "public, max-age=3600"),
            ],
            bytes,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// OGC API - Maps classes this implementation declares, on either surface.
pub(crate) const CONFORMANCE: &[&str] = &[
    "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/core",
    "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/collection-map",
    "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/styled-map",
    "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/spatial-subsetting",
    "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/scaling",
    "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/datetime",
    "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/crs",
    "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/png",
    "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/jpeg",
];

/// GET {mount}/conformance
pub async fn conformance(
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    Query(fp): Query<ds_core::html::FormatParams>,
    headers: HeaderMap,
) -> Result<Response, MapsError> {
    use ds_core::html::{LinkView, Wanted};
    let wanted = negotiate(fp.f.as_deref(), &headers)?;
    let state = state.load_full();
    let base = &request_base_url(&state, &headers);
    let root = &mount.root(base);
    let classes = api_common::conformance_classes(CONFORMANCE);
    Ok(with_vary(match wanted {
        Wanted::Json => Json(json!({ "conformsTo": classes })).into_response(),
        Wanted::Html => {
            let nav = [
                LinkView::new(format!("{root}/"), "up", Some("Landing page")),
                LinkView::new(
                    format!("{root}/conformance?f=json"),
                    "alternate",
                    Some("This document as JSON"),
                ),
            ];
            Html(api_common::workbench::conformance_html(
                Surface {
                    base,
                    root,
                    api: "maps",
                },
                &classes,
                &nav,
            ))
            .into_response()
        }
    }))
}

/// GET {mount}/collections
pub async fn collections(
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    request: api_common::CollectionRequest,
    headers: HeaderMap,
) -> Response {
    let state = state.load_full();
    let base = &request_base_url(&state, &headers);
    let root = &mount.root(base);
    let entries = state
        .collections
        .values()
        .filter_map(|config| {
            let Some(engine) = state.engines.get(&config.id) else {
                tracing::warn!(
                    collection = %config.id,
                    "collection has no registered map engine; omitting from /collections"
                );
                return None;
            };
            let info = engine.raster_info_shared();
            let metadata = build_collection_metadata(
                config,
                &info,
                state.styles.get(&config.id),
                state.map_tileset_ids.contains(&config.id),
                base,
                root,
            );
            Some(api_common::CollectionEntry {
                config,
                metadata,
                bbox: info.spatial_extent,
                time: info.times.first().copied().zip(info.times.last().copied()),
            })
        })
        .collect();
    api_common::collections_response(
        Surface {
            base,
            root,
            api: "maps",
        },
        request,
        entries,
    )
}

/// GET {mount}/collections/{id} — Collection detail
pub async fn collection(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    Query(fp): Query<ds_core::html::FormatParams>,
    headers: HeaderMap,
) -> Result<Response, MapsError> {
    use ds_core::html::Wanted;
    let wanted = negotiate(fp.f.as_deref(), &headers)?;
    let state = state.load_full();
    let (engine, config) = lookup_engine(&state, &id)?;
    let base = &request_base_url(&state, &headers);
    let root = &mount.root(base);
    Ok(with_vary(match wanted {
        Wanted::Json => {
            let info = engine.raster_info_shared();
            let styles = state.styles.get(&id);
            let map_tilesets = state.map_tileset_ids.contains(&id);
            Json(build_collection_metadata(
                config,
                &info,
                styles,
                map_tilesets,
                base,
                root,
            ))
            .into_response()
        }
        Wanted::Html => {
            let metadata = build_collection_metadata(
                config,
                &engine.raster_info_shared(),
                state.styles.get(&id),
                state.map_tileset_ids.contains(&id),
                base,
                root,
            );
            Html(api_common::workbench::collection_html(
                Surface {
                    base,
                    root,
                    api: "maps",
                },
                &metadata,
                config.license.as_ref(),
            ))
            .into_response()
        }
    }))
}

/// GET {mount}/collections/{id}/styles
pub async fn styles(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, MapsError> {
    let state = state.load_full();
    let (_engine, config) = lookup_engine(&state, &id)?;
    let root = &mount.root(&request_base_url(&state, &headers));

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
                    "links": style_links(&config.id, &s.name, root)
                }));
            }
        }
    }

    Ok(Json(json!({
        "styles": style_list,
        "links": [
            {
                "href": format!("{root}/collections/{}/styles", id),
                "rel": "self",
                "type": "application/json"
            }
        ]
    })))
}

/// GET {mount}/collections/{id}/styles/{styleId}/legend
///
/// `?f=json` (the default) returns the machine-readable legend — palette
/// stops, value range, interpolation — so a client can draw its own legend;
/// `?f=png` returns the rendered legend image. Cacheable for a day but NOT
/// immutable: palettes are hot-reloadable.
pub async fn style_legend(
    Path((id, style_id)): Path<(String, String)>,
    Query(params): Query<LegendQueryParams>,
    State(state): State<AppState>,
) -> Result<Response, MapsError> {
    let state = state.load_full();
    let (engine, _config) = lookup_engine(&state, &id)?;
    let format = params.validate()?;

    // `?parameter-name=` selects the per-parameter style layer, so the legend
    // a client draws matches the pixels the map/tile routes render for that
    // same parameter.
    let (_, layer_styles) =
        layer_style_map(&state.styles, &id, params.parameter_name.as_deref())
            .ok_or_else(|| MapsError::NotFound(format!("Collection '{id}' not found")))?;
    let style_info = layer_styles.get(&style_id).ok_or_else(|| {
        MapsError::NotFound(format!(
            "Style '{style_id}' not found for collection '{id}'. Available: {}",
            layer_styles.keys().cloned().collect::<Vec<_>>().join(", ")
        ))
    })?;

    let info = engine.raster_info_shared();
    // Mirror the render path's validation: an unknown `parameter-name` must
    // 400 with the available list, not fall back to the collection style and
    // emit a legend labelled with a parameter that doesn't exist.
    if let Some(pname) = params.parameter_name.as_deref() {
        if !info.parameters.is_empty() && !info.parameters.iter().any(|p| p.name == pname) {
            let mut supported: Vec<&str> =
                info.parameters.iter().map(|p| p.name.as_str()).collect();
            supported.sort_unstable();
            return Err(MapsError::BadRequest(format!(
                "parameter-name '{pname}' is not available for collection '{id}'. \
                 Available: {}",
                supported.join(", ")
            )));
        }
    }
    let (parameter, unit) =
        ds_render::legend_parameter_unit(style_info, &info, params.parameter_name.as_deref());

    match format {
        LegendFormat::Json => {
            let body = ds_render::legend_json(style_info, parameter.as_deref(), unit.as_deref());
            let mut response = Json(body).into_response();
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static(ds_render::LEGEND_CACHE_CONTROL),
            );
            response.headers_mut().insert(
                header::HeaderName::from_static("x-content-type-options"),
                axum::http::HeaderValue::from_static("nosniff"),
            );
            Ok(response)
        }
        LegendFormat::Png => {
            let colormap = style_info.colormap.clone();
            let (min, max) = (style_info.min, style_info.max);
            let title = ds_render::legend_title(style_info, parameter.as_deref(), unit.as_deref());
            let bytes = tokio::task::spawn_blocking(move || {
                ds_render::render_legend(
                    colormap.as_ref(),
                    min,
                    max,
                    ds_render::LEGEND_DEFAULT_WIDTH,
                    ds_render::LEGEND_DEFAULT_HEIGHT,
                    ds_render::ImageFormat::Png,
                    title.as_deref(),
                )
            })
            .await
            .map_err(|e| MapsError::Internal(format!("Legend render failed: {e}")))?
            .map_err(|e| MapsError::Internal(format!("Legend render error: {e}")))?;

            Ok((
                [
                    (header::CONTENT_TYPE, "image/png"),
                    (header::CACHE_CONTROL, ds_render::LEGEND_CACHE_CONTROL),
                    (
                        header::HeaderName::from_static("x-content-type-options"),
                        "nosniff",
                    ),
                ],
                bytes,
            )
                .into_response())
        }
    }
}

/// GET {mount}/collections/{id}/map — render map with default style
pub async fn get_map(
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<MapQueryParams>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, MapsError> {
    render_map(&id, "default", params, headers, state).await
}

/// GET {mount}/collections/{id}/styles/{styleId}/map — render map with named style
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

    // Look up style. A `?parameter-name=` request styles from that
    // parameter's own layer when the collection registers one — otherwise a
    // per-parameter colormap would be unreachable through Maps and every
    // parameter would render with the collection-level default.
    let (style_layer_key, layer_styles) = layer_style_map(
        &state.styles,
        collection_id,
        validated.parameter_name.as_deref(),
    )
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

    // Share one metadata snapshot across default-time resolution and
    // parameter-name validation.
    let raster_info = engine.raster_info_shared();
    let time = validated
        .time
        .or_else(|| engine.default_time())
        .or_else(|| raster_info.times.last().copied());
    // #521: resolve the run axis to the CONCRETE run the engine will render
    // before the cache key is built. The Maps `reference_time` query
    // parameter is still a follow-up (#337 Phase 4) — the handler never pins
    // a run — but the no-TTL rendered cache must key on the run actually
    // rendered: keyed as `None`, the first-rendered run's pixels would keep
    // serving after a newer run re-covers the same valid times (acute for
    // nowcast generations, latent for NWP). Asking the engine (not
    // `reference_times.last()`) preserves GRIB's cross-run fallback when the
    // newest run doesn't cover the valid time yet. Engines without runs keep
    // the identity default (`None` stays `None`).
    let reference_time = engine.resolve_reference_time(time, None);
    // #507: snap to the exact timestep the engine will render before the
    // cache key is built — a not-yet-ingested datetime must cache the
    // previous timestep's pixels under the PREVIOUS timestep's key.
    let time = engine.resolve_time(time, reference_time);

    // Parameter selection precedence: ?parameter-name= wins over style.parameter.
    // Validate against the engine's advertised list when the query supplied one
    // — passing through unrecognised names produces a confusing "default
    // parameter rendered with wrong colormap" rather than a clear 400.
    // `raster_info().parameters` is empty for single-parameter engines (GeoTIFF);
    // in that case we just accept the query value and let the engine ignore it,
    // matching `get_raster_tile`'s documented behavior.
    if let Some(pname) = validated.parameter_name.as_deref() {
        if !raster_info.parameters.is_empty()
            && !raster_info.parameters.iter().any(|p| p.name == pname)
        {
            let mut supported: Vec<&str> = raster_info
                .parameters
                .iter()
                .map(|p| p.name.as_str())
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
        // The RESOLVED style-layer key ("{coll}" or "{coll}/{param}"), so a
        // parameter-layer style and a collection style with the same name
        // and data parameter can never alias one cached image.
        layer: style_layer_key,
        style: style_name.to_string(),
        format: match validated.format {
            ds_render::ImageFormat::Png => 0,
            ds_render::ImageFormat::Jpeg => 1,
            ds_render::ImageFormat::Webp => 2,
        },
        crs: validated.crs.clone(),
        // Projected output renders over the projected-metres bbox carried in
        // `output_crs`, not the WGS84 envelope in `validated.bbox`; key on the
        // metres so two projected requests sharing an envelope don't collide
        // (#267 review).
        bbox: match &validated.output_crs {
            ds_core::map_engine::OutputCrs::Projected { bbox, .. } => {
                ds_render::quantize_bbox(bbox)
            }
            _ => ds_render::quantize_bbox(&validated.bbox),
        },
        width: validated.width,
        height: validated.height,
        time,
        parameter: effective_parameter.clone(),
        z: validated.z.map(ds_render::quantize_z),
        // The engine's latest run, pinned above (#521); a `reference_time`
        // query parameter is a follow-up (#337 Phase 4).
        reference_time,
        // Content revised in place under the same instant (a push-fed alert
        // set) must not hit a stale entry.
        content_version: engine.content_version(),
    };

    let cache_control = cache_control_value(has_explicit_time, cache_key.content_version);
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
    let (job, memory_permit) = ds_executor::RenderJob::acquire_raster(
        state.render_semaphore.clone(),
        validated.width,
        validated.height,
    )
    .await
    .map_err(MapsError::from)?;
    let worker_memory = memory_permit.clone();

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

    let render_result = job
        .run(move || {
            let _memory_permit = worker_memory;

            let tile = engine.get_raster_tile(
                bbox,
                width,
                height,
                time,
                &output_crs,
                render_parameter.as_deref(),
                render_z,
                // The run pinned before keying (#521): `Some(latest)` renders the
                // same pixels as `None` by the engine contract, but survives a
                // run swap mid-render without mixing runs in one response.
                reference_time,
            )?;
            // If every pixel is nodata, skip colorization + encoding entirely.
            if tile.is_empty() {
                return Ok(None);
            }
            ds_render::render_tile(&tile, colormap.as_ref(), format).map(Some)
        })
        .await
        .map_err(MapsError::from)?;

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
            // Empty tile: a transparent PNG, encoded once per (w,h) and shared
            // across WMS/Maps/Tiles (#171). Not inserted into the rendered cache.
            let cached = ds_render::empty_tile(width, height)
                .map_err(|e| MapsError::Internal(format!("Failed to encode empty tile: {e}")))?;
            (cached, "EMPTY", "image/png")
        }
        Err(e) => {
            use ds_core::error::DataServerError as DSE;
            // A client mistake (e.g. a multi-parameter PVOL collection
            // rendered without a `<site>:<quantity>` parameter, or a bad
            // bbox/datetime) is a 400 with the engine's helpful message —
            // not a 500 that hides it behind "Internal server error".
            return Err(match e {
                DSE::ResourceExhausted | DSE::DeadlineExceeded => {
                    MapsError::ServiceUnavailable(e.to_string())
                }
                DSE::InvalidParameter(_)
                | DSE::InvalidBbox(_)
                | DSE::InvalidDatetime(_)
                | DSE::QueryTooLarge(_) => {
                    // 4xx-class: traced at DEBUG (not WARN) so a misconfigured
                    // client is still diagnosable server-side without inflating
                    // the warn stream with routine bad requests.
                    tracing::debug!(
                        "Maps render bad-request for collection '{}': {e}",
                        collection_id
                    );
                    MapsError::BadRequest(e.to_string())
                }
                // ReferenceTimeNotFound is documented to map to 404 (EDR
                // already does); reachable here when a pinned run is pruned
                // between resolution and render — routine for nowcast
                // generations (#522), not an internal error.
                DSE::CollectionNotFound(_)
                | DSE::LocationNotFound(_)
                | DSE::ReferenceTimeNotFound(_) => {
                    tracing::debug!(
                        "Maps render not-found for collection '{}': {e}",
                        collection_id
                    );
                    MapsError::NotFound(e.to_string())
                }
                _ => {
                    tracing::warn!("Maps render error for collection '{}': {e}", collection_id);
                    MapsError::Internal(format!("Render failed: {e}"))
                }
            });
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

impl From<ds_executor::ExecutionError> for MapsError {
    fn from(error: ds_executor::ExecutionError) -> Self {
        match error {
            ds_executor::ExecutionError::Task(e) => Self::Internal(e.to_string()),
            other => Self::ServiceUnavailable(other.to_string()),
        }
    }
}
