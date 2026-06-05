use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use serde_json::json;

use ds_core::config::CollectionConfig;
use ds_core::datetime::parse_datetime_interval;
use ds_core::edr_engine::EdrEngine;

use ds_core::error::DataServerError;
use ds_core::model::CoverageResponse;
use ds_render::{render_chart, render_heatmap};

use crate::params::{
    parse_edr_format, parse_z, plot_dimensions, resolve_z_levels, split_position_coords,
    AreaQueryParams, EdrFormat, LocationQueryParams, PositionQueryParams, TrajectoryQueryParams,
};
use crate::plot_convert::{coverage_response_to_panels, section_response_to_heatmaps};
use crate::response::{coverage_response_to_json, locations_to_geojson, LocationsContext};

type HandlerError = (StatusCode, Json<serde_json::Value>);

/// Serialise an EDR coverage response in the requested output format.
///
/// `CoverageJSON` is the default; `PNG` renders a vertical-profile or
/// time-series plot (one stacked panel per parameter). A response that can't
/// be plotted (a gridded/area result) maps to 400.
fn render_coverage_response(
    result: CoverageResponse,
    format: EdrFormat,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<Response, HandlerError> {
    match format {
        EdrFormat::CoverageJson => {
            let body = serde_json::to_string(&coverage_response_to_json(&result)).map_err(|e| {
                tracing::error!("EDR CoverageJSON serialise error: {e}");
                server_error()
            })?;
            Ok((
                [(header::CONTENT_TYPE, "application/prs.coverage+json")],
                body,
            )
                .into_response())
        }
        EdrFormat::Png => {
            let panels = coverage_response_to_panels(&result).map_err(|e| bad_request(&e))?;
            let (w, h) = plot_dimensions(width, height);
            let png = render_chart(&panels, w, h).map_err(|e| {
                tracing::error!("EDR plot render error: {e}");
                server_error()
            })?;
            Ok(([(header::CONTENT_TYPE, "image/png")], png).into_response())
        }
    }
}

fn bad_request(e: &DataServerError) -> HandlerError {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "code": "BadRequest", "description": e.to_string() })),
    )
}

fn server_error() -> HandlerError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "code": "ServerError", "description": "Internal server error" })),
    )
}

/// A 400 from a plain message (used for `?f=` content negotiation errors).
fn bad_request_msg(msg: &str) -> HandlerError {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "code": "BadRequest", "description": msg })),
    )
}

/// Resolve the requested representation from `?f=` + the `Accept` header.
fn negotiate(f: Option<&str>, headers: &HeaderMap) -> Result<ds_core::html::Wanted, HandlerError> {
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok());
    ds_core::html::negotiate(f, accept).map_err(|e| bad_request_msg(&e.to_string()))
}

/// Tag a content-negotiated response with `Vary: Accept` so shared caches
/// don't serve the JSON body to a client that asked for HTML (or vice versa).
/// Uses `append` (not `insert`) so it never clobbers a `Vary` an upstream layer
/// may have set (e.g. compression's `Vary: Accept-Encoding`).
fn with_vary(mut resp: Response) -> Response {
    resp.headers_mut()
        .append(header::VARY, axum::http::HeaderValue::from_static("accept"));
    resp
}

/// Shared state for the EDR API: a registry of collection engines + metadata.
#[derive(Clone)]
pub struct EdrState {
    pub engines: HashMap<String, Arc<dyn EdrEngine>>,
    pub collections: HashMap<String, CollectionConfig>,
    /// Base URL for generating absolute links (e.g. "https://api.example.com").
    pub base_url: String,
}

pub type AppState = Arc<ArcSwap<EdrState>>;

#[allow(clippy::type_complexity)]
fn lookup_collection<'a>(
    state: &'a EdrState,
    id: &str,
) -> Result<(&'a Arc<dyn EdrEngine>, &'a CollectionConfig), (StatusCode, Json<serde_json::Value>)> {
    let engine = state.engines.get(id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "NotFound", "description": format!("Collection '{id}' not found") })),
        )
    })?;
    let config = state.collections.get(id).unwrap();
    Ok((engine, config))
}

/// Parse and resolve the request `z` parameter into the concrete level
/// list an engine samples.
///
/// - Absent / blank → `None` (whole vertical extent).
/// - A `z` against a collection with no vertical dimension → 400 (rather
///   than silently ignored).
/// - An interval (`z=min/max`) is expanded against the collection's
///   advertised levels; a list passes through for the engine to snap.
fn resolve_request_z(
    engine: &Arc<dyn EdrEngine>,
    z: Option<&str>,
) -> Result<Option<Vec<f64>>, (StatusCode, Json<serde_json::Value>)> {
    let Some(sel) = parse_z(z).map_err(|e| bad_request(&e))? else {
        return Ok(None);
    };
    let extent = engine.get_vertical_extent();
    if extent.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "code": "BadRequest",
                "description": "This collection has no vertical dimension; \
                                the `z` query parameter is not supported"
            })),
        ));
    }
    let levels = resolve_z_levels(&sel, extent.as_ref()).map_err(|e| bad_request(&e))?;
    Ok(Some(levels))
}

pub async fn landing_page(
    State(state): State<AppState>,
    Query(fp): Query<ds_core::html::FormatParams>,
    headers: HeaderMap,
) -> Result<Response, HandlerError> {
    use ds_core::html::{LinkView, Wanted};
    let wanted = negotiate(fp.f.as_deref(), &headers)?;
    let state = state.load_full();
    let base = &state.base_url;
    let title = "MeteoCore - EDR";
    let description = "Metocean Data Server — OGC API EDR";
    // (href, rel, type, title) — one source for both representations.
    let links = [
        (
            format!("{base}/edr/"),
            "self",
            "application/json",
            "This document",
        ),
        (
            format!("{base}/edr/api"),
            "service-desc",
            "application/vnd.oai.openapi+json;version=3.0",
            "API definition",
        ),
        (
            format!("{base}/edr/api/docs"),
            "service-doc",
            "text/html",
            "API documentation",
        ),
        (
            format!("{base}/edr/conformance"),
            "conformance",
            "application/json",
            "Conformance classes",
        ),
        (
            format!("{base}/edr/collections"),
            "data",
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
            // collection-detail HTML page), so the HTML landing page links to
            // its machine-readable twin.
            views.push(LinkView::new(
                format!("{base}/edr/?f=json"),
                "alternate",
                Some("This document as JSON"),
            ));
            Html(ds_core::html::landing_html(title, description, &views)).into_response()
        }
    }))
}

/// OpenAPI `f` (output-format) query parameter, shared by the content-negotiated
/// metadata endpoints (landing, conformance, collections, collection detail).
fn format_parameter() -> serde_json::Value {
    json!({"name": "f", "in": "query", "required": false, "schema": {"type": "string", "enum": ["json", "html"]},
           "description": "Output format. 'json' (default) or 'html'; overrides the Accept header."})
}

/// OpenAPI `parameters` array for the OGC API – Common – Part 4 searchable
/// `/collections` query parameters (plus the shared `f` format selector).
/// Documented per the CLAUDE.md rule and Part 4 §5.5.
fn searchable_collections_parameters() -> serde_json::Value {
    let mut params = json!([
        {"name": "bbox", "in": "query", "required": false, "schema": {"type": "string"},
         "description": "Filter to collections intersecting this CRS84 bbox: 4 (or 6) comma-separated numbers west,south,east,north."},
        {"name": "bbox-crs", "in": "query", "required": false, "schema": {"type": "string"},
         "description": "CRS of the bbox values. Only CRS84 is supported."},
        {"name": "datetime", "in": "query", "required": false, "schema": {"type": "string"},
         "description": "Filter to collections whose temporal extent intersects this RFC 3339 instant or interval (start/end, ../end, start/..)."},
        {"name": "q", "in": "query", "required": false, "schema": {"type": "string"},
         "description": "Free-text search (comma-separated terms, OR) over collection title, description, and keywords."},
        {"name": "limit", "in": "query", "required": false, "schema": {"type": "integer", "minimum": 1, "maximum": 1000},
         "description": "Maximum number of collections per page (default 1000)."},
        {"name": "offset", "in": "query", "required": false, "schema": {"type": "integer", "minimum": 0},
         "description": "Number of matching collections to skip (pagination cursor)."}
    ]);
    params
        .as_array_mut()
        .expect("searchable params is a JSON array")
        .push(format_parameter());
    params
}

pub async fn api_definition(State(state): State<AppState>) -> impl IntoResponse {
    let state = state.load_full();
    let mut collection_paths = json!({});
    for config in state.collections.values() {
        let id = &config.id;
        // Per-collection supported-query-types so the OpenAPI spec only
        // advertises endpoints the engine actually implements. The
        // `data_queries` block in `build_collection_metadata` already
        // gates on this; without the same gate here the two discovery
        // surfaces disagree (an OGC CITE crawl following /api would hit
        // the default `InvalidParameter → 400` arm on an unsupported
        // engine, while /collections/{id} omits the link entirely).
        // The legacy /position and /area entries below stay unconditional
        // for back-compat; trajectory ships gated from day one.
        let supported: std::collections::HashSet<String> = state
            .engines
            .get(id)
            .map(|e| e.supported_query_types().into_iter().collect())
            .unwrap_or_default();

        // Collection detail
        let detail_path = format!("/edr/collections/{id}");
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

        // Locations list
        let locations_path = format!("/edr/collections/{id}/locations");
        collection_paths[&locations_path] = json!({
            "get": {
                "summary": format!("Get locations for {}", config.title),
                "operationId": format!("getLocations_{id}"),
                "tags": [id],
                "responses": {
                    "200": {
                        "description": "Locations in GeoJSON format",
                        "content": {
                            "application/geo+json": {
                                "schema": {"type": "object"}
                            }
                        }
                    },
                    "404": {"description": "Collection not found"}
                }
            }
        });

        // Location data query
        let location_path = format!("/edr/collections/{id}/locations/{{locationId}}");
        collection_paths[&location_path] = json!({
            "get": {
                "summary": format!("Get data for a location in {}", config.title),
                "operationId": format!("getLocationData_{id}"),
                "tags": [id],
                "parameters": [
                    {
                        "name": "locationId",
                        "in": "path",
                        "required": true,
                        "schema": {"type": "string"}
                    },
                    {"$ref": "#/components/parameters/datetime"},
                    {"$ref": "#/components/parameters/parameter-name"},
                    {"$ref": "#/components/parameters/z"},
                    {
                        "name": "f",
                        "in": "query",
                        "description": "Output format: CoverageJSON (default) or PNG (a vertical-profile / time-series plot).",
                        "required": false,
                        "schema": {"type": "string", "enum": ["CoverageJSON", "PNG"]}
                    }
                ],
                "responses": {
                    "200": {
                        "description": "Coverage data",
                        "content": {
                            "application/prs.coverage+json": {
                                "schema": {"$ref": "#/components/schemas/coverageJSON"}
                            },
                            "image/png": {
                                "schema": {"type": "string", "format": "binary"}
                            }
                        }
                    },
                    "400": {"description": "Bad request"},
                    "404": {"description": "Location not found"},
                    "500": {"description": "Server error"}
                }
            }
        });

        // Position query
        let position_path = format!("/edr/collections/{id}/position");
        collection_paths[&position_path] = json!({
            "get": {
                "summary": format!("Position query for {}", config.title),
                "operationId": format!("getPosition_{id}"),
                "tags": [id],
                "parameters": [
                    {"$ref": "#/components/parameters/coords-point"},
                    {"$ref": "#/components/parameters/datetime"},
                    {"$ref": "#/components/parameters/parameter-name"},
                    {"$ref": "#/components/parameters/z"},
                    {
                        "name": "f",
                        "in": "query",
                        "description": "Output format: CoverageJSON (default) or PNG (a vertical-profile / time-series plot).",
                        "required": false,
                        "schema": {"type": "string", "enum": ["CoverageJSON", "PNG"]}
                    }
                ],
                "responses": {
                    "200": {
                        "description": "Coverage data",
                        "content": {
                            "application/prs.coverage+json": {
                                "schema": {"$ref": "#/components/schemas/coverageJSON"}
                            },
                            "image/png": {
                                "schema": {"type": "string", "format": "binary"}
                            }
                        }
                    },
                    "400": {"description": "Bad request"},
                    "404": {"description": "Not found"},
                    "500": {"description": "Server error"}
                }
            }
        });

        // Area query
        let area_path = format!("/edr/collections/{id}/area");
        collection_paths[&area_path] = json!({
            "get": {
                "summary": format!("Area query for {}", config.title),
                "operationId": format!("getArea_{id}"),
                "tags": [id],
                "parameters": [
                    {"$ref": "#/components/parameters/coords-polygon"},
                    {"$ref": "#/components/parameters/datetime"},
                    {"$ref": "#/components/parameters/parameter-name"},
                    {"$ref": "#/components/parameters/z"}
                ],
                "responses": {
                    "200": {
                        "description": "Coverage data",
                        "content": {
                            "application/prs.coverage+json": {
                                "schema": {"$ref": "#/components/schemas/coverageJSON"}
                            }
                        }
                    },
                    "400": {"description": "Bad request"},
                    "404": {"description": "Not found"},
                    "500": {"description": "Server error"}
                }
            }
        });

        // Trajectory query (vertical cross-section). Only advertised
        // for engines that report `trajectory` in
        // `supported_query_types` — keeps the OpenAPI spec consistent
        // with `data_queries` in the collection metadata. A client that
        // calls the path on a non-trajectory engine gets a 404 from the
        // handler's capability guard (the resource doesn't exist for
        // that collection).
        if supported.contains("trajectory") {
            let trajectory_path = format!("/edr/collections/{id}/trajectory");
            collection_paths[&trajectory_path] = json!({
                "get": {
                    "summary": format!("Trajectory cross-section for {}", config.title),
                    "operationId": format!("getTrajectory_{id}"),
                    "tags": [id],
                    "parameters": [
                        {"$ref": "#/components/parameters/coords-linestring"},
                        {"$ref": "#/components/parameters/datetime"},
                        {"$ref": "#/components/parameters/parameter-name"},
                        {"$ref": "#/components/parameters/z-trajectory"},
                        {
                            "name": "f",
                            "in": "query",
                            "description": "Output format: CoverageJSON (default) or PNG (a colour-mapped distance×height cross-section heatmap).",
                            "required": false,
                            "schema": {"type": "string", "enum": ["CoverageJSON", "PNG"]}
                        }
                    ],
                    "responses": {
                        "200": {
                            "description": "Coverage data — CoverageJSON Section domain or PNG heatmap",
                            "content": {
                                "application/prs.coverage+json": {
                                    "schema": {"$ref": "#/components/schemas/coverageJSON"}
                                },
                                "image/png": {
                                    "schema": {"type": "string", "format": "binary"}
                                }
                            }
                        },
                        "400": {"description": "Bad request"},
                        "404": {"description": "Not found"},
                        "500": {"description": "Server error"}
                    }
                }
            });
        }
    }

    let mut paths = json!({
        "/edr/": {
            "get": {
                "summary": "Landing page",
                "operationId": "getLandingPage",
                "parameters": [format_parameter()],
                "responses": {
                    "200": {"description": "Landing page"}
                }
            }
        },
        "/edr/conformance": {
            "get": {
                "summary": "Conformance classes",
                "operationId": "getConformance",
                "parameters": [format_parameter()],
                "responses": {
                    "200": {"description": "Conformance classes"}
                }
            }
        },
        "/edr/collections": {
            "get": {
                "summary": "List collections",
                "operationId": "getCollections",
                "parameters": searchable_collections_parameters(),
                "responses": {
                    "200": {"description": "List of EDR collections"}
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
            "title": "MeteoCore - OGC API EDR",
            "version": "1.0.0",
            "description": "OGC API - Environmental Data Retrieval implementation"
        },
        "paths": paths,
        "components": {
            "parameters": {
                "datetime": {
                    "name": "datetime",
                    "in": "query",
                    "required": false,
                    "schema": {"type": "string"},
                    "description": "RFC 3339 datetime or interval (start/end, ../end, start/..)"
                },
                "parameter-name": {
                    "name": "parameter-name",
                    "in": "query",
                    "required": false,
                    "schema": {"type": "string"},
                    "description": "Comma-separated list of parameter names to include"
                },
                "z": {
                    "name": "z",
                    "in": "query",
                    "required": false,
                    "schema": {"type": "string"},
                    "description": "Vertical level selector — a single value or a comma-separated list (e.g. z=0.5 or z=850,700,500). Only valid for collections that advertise a vertical extent. Each requested value is snapped to the nearest level in the collection's advertised vertical extent; the response domain reports the snapped level."
                },
                "coords-point": {
                    "name": "coords",
                    "in": "query",
                    "required": true,
                    "schema": {"type": "string"},
                    "description": "WKT POINT or MULTIPOINT geometry. Examples: POINT(24.94 60.17), MULTIPOINT((24.94 60.17),(23.76 61.5)). Note: for a MULTIPOINT against a collection with a vertical extent, every point's coverages are flattened into one CoverageCollection — per-point grouping is not preserved."
                },
                "coords-polygon": {
                    "name": "coords",
                    "in": "query",
                    "required": true,
                    "schema": {"type": "string"},
                    "description": "WKT POLYGON geometry, e.g. POLYGON((24 60, 26 60, 26 61, 24 61, 24 60))"
                },
                "coords-linestring": {
                    "name": "coords",
                    "in": "query",
                    "required": true,
                    "schema": {"type": "string"},
                    "description": "WKT LINESTRING geometry (lon lat, lon lat, …). LINESTRINGZ/M variants are not accepted — per-node z and time will arrive in a follow-up."
                },
                "z-trajectory": {
                    "name": "z",
                    "in": "query",
                    "required": false,
                    "schema": {"type": "string"},
                    "description": "Elevation-angle selection for the cross-section, matching the collection's advertised vertical extent (sweep angles in degrees). Forms: z=5 (one sweep), z=0.5,1.5,5 (a list), or z=0.3/15 (a min/max interval → every advertised angle in range). The selected angle window bounds which sweeps build the RHI; the rendered z axis is derived height above the antenna (metres). Absent → all sweeps."
                }
            },
            "schemas": {
                "coverageJSON": {
                    "type": "object",
                    "description": "OGC CoverageJSON 1.0 Coverage object",
                    "required": ["type", "domain", "parameters", "ranges"],
                    "properties": {
                        "type": {"type": "string", "enum": ["Coverage"]},
                        "domain": {"type": "object"},
                        "parameters": {"type": "object"},
                        "ranges": {"type": "object"}
                    }
                }
            }
        }
    });

    Json(openapi)
}

pub async fn api_docs(State(state): State<AppState>) -> impl IntoResponse {
    let state = state.load_full();
    let spec_url = format!("{}/edr/api", state.base_url);
    axum::response::Html(ds_core::openapi::swagger_ui_html(
        "MeteoCore - EDR API",
        &spec_url,
    ))
}

pub async fn conformance(
    State(state): State<AppState>,
    Query(fp): Query<ds_core::html::FormatParams>,
    headers: HeaderMap,
) -> Result<Response, HandlerError> {
    use ds_core::html::{LinkView, Wanted};
    let wanted = negotiate(fp.f.as_deref(), &headers)?;
    let state = state.load_full();
    let base = &state.base_url;
    let classes = [
        "http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/core",
        "http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/landing-page",
        "http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/oas30",
        // OGC API - Common - Part 2: Geospatial Data (20-024). /collections +
        // /collections/{id} satisfy the Collections, JSON, and (now) HTML
        // classes — the HTML representation is served via `?f=html` / Accept.
        "http://www.opengis.net/spec/ogcapi-common-2/1.0/conf/collections",
        "http://www.opengis.net/spec/ogcapi-common-2/1.0/conf/json",
        "http://www.opengis.net/spec/ogcapi-common-2/1.0/conf/html",
        // OGC API - Common - Part 4 (Discovery within many collections,
        // draft 25-046): /collections supports bbox/bbox-crs/datetime/q/
        // limit filtering + offset pagination (numberMatched/Returned +
        // next/prev links). Sortable/Filterable/Hierarchical not declared.
        "http://www.opengis.net/spec/ogcapi-common-4/1.0/conf/searchable-collections",
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/core",
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/collections",
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/json",
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/covjson",
    ];
    Ok(with_vary(match wanted {
        Wanted::Json => Json(json!({ "conformsTo": classes })).into_response(),
        Wanted::Html => {
            let nav = [
                LinkView::new(format!("{base}/edr/"), "up", Some("Landing page")),
                LinkView::new(
                    format!("{base}/edr/conformance?f=json"),
                    "alternate",
                    Some("This document as JSON"),
                ),
            ];
            Html(ds_core::html::conformance_html(&classes, &nav)).into_response()
        }
    }))
}

pub async fn collections(
    State(state): State<AppState>,
    Query(sp): Query<ds_core::collection_search::SearchQueryParams>,
    headers: HeaderMap,
) -> Result<Response, HandlerError> {
    use ds_core::collection_search::{search, CollectionMatch};
    use ds_core::html::Wanted;

    let wanted = negotiate(sp.f.as_deref(), &headers)?;
    let params = sp.parse().map_err(|e| bad_request_msg(&e.to_string()))?;
    let state = state.load_full();
    let base = &state.base_url;

    // (id, title, description, bbox, time, metadata, keywords, license) per
    // collection. Tuple element types are inferred, so no extra chrono import is
    // needed. keywords/license feed `?q=` search and the HTML cards.
    let mut rows: Vec<_> = state
        .collections
        .values()
        .filter_map(|config| {
            let Some(engine) = state.engines.get(&config.id) else {
                // A registered collection with no engine (e.g. a partial
                // reload) would otherwise vanish from /collections silently.
                tracing::warn!(
                    collection = %config.id,
                    "collection has no registered EDR engine; omitting from /collections"
                );
                return None;
            };
            let value = build_collection_metadata(engine.as_ref(), config, base);
            Some((
                config.id.clone(),
                config.title.clone(),
                config.description.clone(),
                engine.get_spatial_extent(),
                engine.get_temporal_extent(),
                value,
                config.keywords.clone(),
                config.license.as_ref().and_then(|l| l.card_link()),
            ))
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    let matches: Vec<CollectionMatch> = rows
        .iter()
        .map(|r| CollectionMatch {
            title: &r.1,
            description: &r.2,
            keywords: &r.6,
            bbox: r.3,
            time: r.4,
        })
        .collect();
    let result = search(&matches, &params);
    let href = |offset| {
        format!(
            "{base}/edr/collections{}",
            sp.query_string(params.limit, offset)
        )
    };

    Ok(with_vary(match wanted {
        Wanted::Json => {
            let collections: Vec<serde_json::Value> =
                result.page.iter().map(|&i| rows[i].5.clone()).collect();
            let number_returned = collections.len();
            let link = |rel: &str, offset: usize, title: Option<&str>| {
                let mut o = json!({ "href": href(offset), "rel": rel, "type": "application/json" });
                if let Some(t) = title {
                    o["title"] = json!(t);
                }
                o
            };
            let mut links = vec![link("self", params.offset, None)];
            if result.has_next {
                links.push(link("next", result.next_offset, Some("Next page")));
            }
            if result.has_prev {
                links.push(link("prev", result.prev_offset, Some("Previous page")));
            }
            Json(json!({
                "collections": collections,
                "numberMatched": result.number_matched,
                "numberReturned": number_returned,
                "links": links
            }))
            .into_response()
        }
        Wanted::Html => {
            use ds_core::html::{CollectionCard, LinkView};
            let cards: Vec<CollectionCard> = result
                .page
                .iter()
                .map(|&i| CollectionCard {
                    id: rows[i].0.clone(),
                    title: rows[i].1.clone(),
                    description: rows[i].2.clone(),
                    self_href: format!("{base}/edr/collections/{}", rows[i].0),
                    keywords: rows[i].6.clone(),
                    license: rows[i].7.clone(),
                })
                .collect();
            let mut nav = vec![LinkView::new(
                href(params.offset),
                "self",
                Some("This page"),
            )];
            if result.has_next {
                nav.push(LinkView::new(
                    href(result.next_offset),
                    "next",
                    Some("Next page"),
                ));
            }
            if result.has_prev {
                nav.push(LinkView::new(
                    href(result.prev_offset),
                    "prev",
                    Some("Previous page"),
                ));
            }
            // rel="alternate" to the JSON representation, preserving the current
            // bbox/datetime/q/limit/offset filters (parity with the other HTML
            // metadata pages).
            nav.push(LinkView::new(
                format!(
                    "{base}/edr/collections{}",
                    sp.query_string_with_format(params.limit, params.offset, "json")
                ),
                "alternate",
                Some("This page as JSON"),
            ));
            Html(ds_core::html::collections_html("Collections", &cards, &nav)).into_response()
        }
    }))
}

pub async fn collection(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Query(fp): Query<ds_core::html::FormatParams>,
    headers: HeaderMap,
) -> Result<Response, HandlerError> {
    use ds_core::html::{CollectionCard, LinkView, Wanted};
    let wanted = negotiate(fp.f.as_deref(), &headers)?;
    let state = state.load_full();
    let (engine, config) = lookup_collection(&state, &id)?;
    let base = &state.base_url;
    Ok(with_vary(match wanted {
        Wanted::Json => {
            Json(build_collection_metadata(engine.as_ref(), config, base)).into_response()
        }
        Wanted::Html => {
            let card = CollectionCard {
                id: config.id.clone(),
                title: config.title.clone(),
                description: config.description.clone(),
                self_href: format!("{base}/edr/collections/{}", config.id),
                keywords: config.keywords.clone(),
                license: config.license.as_ref().and_then(|l| l.card_link()),
            };
            let links = [
                LinkView::new(
                    format!("{base}/edr/collections/{}?f=json", config.id),
                    "alternate",
                    Some("JSON"),
                ),
                LinkView::new(
                    format!("{base}/edr/collections"),
                    "collection",
                    Some("All collections"),
                ),
            ];
            Html(ds_core::html::collection_html(&card, &links)).into_response()
        }
    }))
}

pub async fn locations(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let state = state.load_full();
    let (engine, _config) = lookup_collection(&state, &id)?;

    let locs = engine.get_locations().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "code": "ServerError", "description": "Internal server error" })),
        )
    })?;
    let params = engine.get_parameters();
    let temporal = engine
        .get_temporal_extent()
        .map(|(s, e)| (s.to_rfc3339(), e.to_rfc3339()));
    let ctx = LocationsContext {
        collection_id: &id,
        parameter_names: &params,
        temporal_extent: temporal,
        base_url: &state.base_url,
    };
    let body = serde_json::to_string(&locations_to_geojson(&locs, &ctx)).unwrap();
    Ok(([(header::CONTENT_TYPE, "application/geo+json")], body))
}

pub async fn location_query(
    Path((id, loc_id)): Path<(String, String)>,
    Query(params): Query<LocationQueryParams>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let state = state.load_full();
    let (engine, _config) = lookup_collection(&state, &id)?;

    let datetime = params
        .datetime
        .as_deref()
        .map(parse_datetime_interval)
        .transpose()
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "code": "BadRequest", "description": e.to_string() })),
            )
        })?;

    let param_names: Option<Vec<String>> = params
        .parameter_name
        .as_deref()
        .map(|s| s.split(',').map(|p| p.trim().to_string()).collect());

    let z = resolve_request_z(engine, params.z.as_deref())?;

    let result = engine
        .query_location(&loc_id, datetime, param_names.as_deref(), z.as_deref())
        .map_err(|e| match &e {
            ds_core::error::DataServerError::LocationNotFound(_) => (
                StatusCode::NOT_FOUND,
                Json(json!({ "code": "NotFound", "description": e.to_string() })),
            ),
            ds_core::error::DataServerError::InvalidParameter(_) => (
                StatusCode::BAD_REQUEST,
                Json(json!({ "code": "BadRequest", "description": e.to_string() })),
            ),
            _ => {
                tracing::error!("Location query error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "code": "ServerError", "description": "Internal server error" })),
                )
            }
        })?;

    let format = parse_edr_format(params.f.as_deref()).map_err(|e| bad_request(&e))?;
    render_coverage_response(result, format, params.width, params.height)
}

pub async fn position_query(
    Path(id): Path<String>,
    Query(params): Query<PositionQueryParams>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let state = state.load_full();
    let (engine, _config) = lookup_collection(&state, &id)?;

    let datetime = params
        .datetime
        .as_deref()
        .map(parse_datetime_interval)
        .transpose()
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "code": "BadRequest", "description": e.to_string() })),
            )
        })?;

    let param_names: Option<Vec<String>> = params
        .parameter_name
        .as_deref()
        .map(|s| s.split(',').map(|p| p.trim().to_string()).collect());

    let z = resolve_request_z(engine, params.z.as_deref())?;

    // Split coords into one or more POINT(lon lat) strings. A single POINT is
    // passed through to the engine as-is; MULTIPOINT is fanned out into one
    // query per point and assembled into a CoverageCollection.
    let points = split_position_coords(&params.coords).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "code": "BadRequest", "description": e.to_string() })),
        )
    })?;

    let map_engine_error = |e: &ds_core::error::DataServerError| match e {
        ds_core::error::DataServerError::InvalidParameter(_)
        | ds_core::error::DataServerError::InvalidBbox(_)
        | ds_core::error::DataServerError::InvalidDatetime(_) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "code": "BadRequest", "description": e.to_string() })),
        ),
        ds_core::error::DataServerError::LocationNotFound(_)
        | ds_core::error::DataServerError::CollectionNotFound(_)
        | ds_core::error::DataServerError::FeatureNotFound(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "NotFound", "description": e.to_string() })),
        ),
        _ => {
            tracing::error!("Position query error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "code": "ServerError", "description": "Internal server error" })),
            )
        }
    };

    let format = parse_edr_format(params.f.as_deref()).map_err(|e| bad_request(&e))?;

    if points.len() == 1 {
        let result = engine
            .query_position(&points[0], datetime, param_names.as_deref(), z.as_deref())
            .map_err(|e| map_engine_error(&e))?;
        return render_coverage_response(result, format, params.width, params.height);
    }

    // MULTIPOINT — fan out one query per point and flatten every point's
    // coverages into a single CoverageCollection.
    let mut coverages = Vec::with_capacity(points.len());
    for point in &points {
        let qr = engine
            .query_position(point, datetime, param_names.as_deref(), z.as_deref())
            .map_err(|e| map_engine_error(&e))?;
        match qr {
            CoverageResponse::Single(q) => coverages.push(q),
            CoverageResponse::Collection(v) => coverages.extend(v),
        }
    }

    render_coverage_response(
        CoverageResponse::Collection(coverages),
        format,
        params.width,
        params.height,
    )
}

pub async fn area_query(
    Path(id): Path<String>,
    Query(params): Query<AreaQueryParams>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let state = state.load_full();
    let (engine, _config) = lookup_collection(&state, &id)?;

    // An area result is gridded / multi-coverage, not a single line plot.
    if parse_edr_format(params.f.as_deref()).map_err(|e| bad_request(&e))? == EdrFormat::Png {
        return Err(bad_request(&DataServerError::InvalidParameter(
            "PNG output is not available for area queries".into(),
        )));
    }

    let datetime = params
        .datetime
        .as_deref()
        .map(parse_datetime_interval)
        .transpose()
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "code": "BadRequest", "description": e.to_string() })),
            )
        })?;

    let param_names: Option<Vec<String>> = params
        .parameter_name
        .as_deref()
        .map(|s| s.split(',').map(|p| p.trim().to_string()).collect());

    let z = resolve_request_z(engine, params.z.as_deref())?;

    let result = engine
        .query_area(
            &params.coords,
            datetime,
            param_names.as_deref(),
            z.as_deref(),
        )
        .map_err(|e| match &e {
            ds_core::error::DataServerError::InvalidParameter(_)
            | ds_core::error::DataServerError::InvalidBbox(_)
            | ds_core::error::DataServerError::InvalidDatetime(_) => (
                StatusCode::BAD_REQUEST,
                Json(json!({ "code": "BadRequest", "description": e.to_string() })),
            ),
            ds_core::error::DataServerError::LocationNotFound(_)
            | ds_core::error::DataServerError::CollectionNotFound(_) => (
                StatusCode::NOT_FOUND,
                Json(json!({ "code": "NotFound", "description": e.to_string() })),
            ),
            _ => {
                tracing::error!("Area query error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "code": "ServerError", "description": "Internal server error" })),
                )
            }
        })?;

    let body = serde_json::to_string(&coverage_response_to_json(&result)).map_err(|e| {
        tracing::error!("Area CoverageJSON serialise error: {e}");
        server_error()
    })?;
    Ok((
        [(header::CONTENT_TYPE, "application/prs.coverage+json")],
        body,
    ))
}

pub async fn trajectory_query(
    Path(id): Path<String>,
    Query(params): Query<TrajectoryQueryParams>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let state = state.load_full();
    let (engine, config) = lookup_collection(&state, &id)?;

    // An engine that doesn't advertise `trajectory` has no cross-section
    // capability. Return 404 (the resource doesn't exist for this
    // collection) rather than letting the default trait method answer
    // 400 (which wrongly implies the *request* was malformed). Keeps the
    // live route consistent with the `api_definition` OpenAPI gating and
    // the `data_queries` collection metadata. Flagged by claude-review.
    if !engine
        .supported_query_types()
        .iter()
        .any(|q| q == "trajectory")
    {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({
                "code": "NotFound",
                "description": format!(
                    "Collection '{id}' does not support trajectory (cross-section) queries"
                )
            })),
        ));
    }

    let format = parse_edr_format(params.f.as_deref()).map_err(|e| bad_request(&e))?;

    let datetime = params
        .datetime
        .as_deref()
        .map(parse_datetime_interval)
        .transpose()
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "code": "BadRequest", "description": e.to_string() })),
            )
        })?;

    let param_names: Option<Vec<String>> = params
        .parameter_name
        .as_deref()
        .map(|s| s.split(',').map(|p| p.trim().to_string()).collect());

    // Trajectory `z` selects elevation angles from the collection's
    // advertised vertical extent (the cross-section is built from those
    // sweeps); an interval `z=0.3/15` expands to the angles in range.
    let z = resolve_request_z(engine, params.z.as_deref())?;

    // The cross-section sampler is the heaviest EDR path — up to
    // nodes × z-levels × quantities × timesteps `sample_polar_slant`
    // iterations (millions, seconds of CPU). Run it on the blocking pool
    // so it doesn't park a request-serving worker and head-of-line-block
    // other requests. spawn_blocking is also *required* for correctness under
    // lazy pixel loading: an S3 cache miss now fetches via `DataStore::get_on`
    // (`handle.block_on`), the valid bridge on a `spawn_blocking` pool thread —
    // the plain `DataStore::get` uses `block_in_place`, which *panics* on a
    // spawn_blocking thread (it is only valid on a multi-thread runtime worker).
    // The still-direct position/area/locations queries keep `DataStore::get`
    // precisely because they run on a worker; offloading them is tracked in #178.
    let engine = engine.clone();
    let coords = params.coords.clone();
    let result = tokio::task::spawn_blocking(move || {
        engine.query_trajectory(&coords, datetime, param_names.as_deref(), z.as_deref())
    })
    .await
    .map_err(|e| {
        tracing::error!("Trajectory query task join error: {e}");
        server_error()
    })?
    .map_err(|e| match &e {
        ds_core::error::DataServerError::InvalidParameter(_)
        | ds_core::error::DataServerError::InvalidBbox(_)
        | ds_core::error::DataServerError::InvalidDatetime(_) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "code": "BadRequest", "description": e.to_string() })),
        ),
        ds_core::error::DataServerError::LocationNotFound(_)
        | ds_core::error::DataServerError::CollectionNotFound(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "NotFound", "description": e.to_string() })),
        ),
        _ => {
            tracing::error!("Trajectory query error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "code": "ServerError", "description": "Internal server error" })),
            )
        }
    })?;

    match format {
        EdrFormat::CoverageJson => {
            let body = serde_json::to_string(&coverage_response_to_json(&result)).map_err(|e| {
                tracing::error!("Trajectory CoverageJSON serialise error: {e}");
                server_error()
            })?;
            Ok((
                [(header::CONTENT_TYPE, "application/prs.coverage+json")],
                body,
            )
                .into_response())
        }
        EdrFormat::Png => {
            // Render the cross-section as a colour-mapped heatmap using
            // the collection's WMS colormap (or a data-scaled viridis
            // fallback).
            // A failure here is an internal inconsistency (the engine
            // already returned a Section for this trajectory query), not
            // a client mistake — log it and return a generic 500 rather
            // than leaking the internal message in a 400.
            let (heatmaps, colormap) = section_response_to_heatmaps(&result, config.wms.as_ref())
                .map_err(|e| {
                tracing::error!("Trajectory PNG section conversion error: {e}");
                server_error()
            })?;
            let (w, h) = plot_dimensions(params.width, params.height);
            let png = render_heatmap(&heatmaps, colormap.as_ref(), w, h).map_err(|e| {
                tracing::error!("Trajectory PNG render error: {e}");
                server_error()
            })?;
            Ok(([(header::CONTENT_TYPE, "image/png")], png).into_response())
        }
    }
}

fn build_collection_metadata(
    engine: &dyn EdrEngine,
    config: &CollectionConfig,
    base_url: &str,
) -> serde_json::Value {
    let param_descs = engine.get_parameter_descriptions();
    let temporal = engine.get_temporal_extent();
    let spatial = engine.get_spatial_extent();

    let mut extent = serde_json::Map::new();
    if let Some(bbox) = spatial {
        extent.insert(
            "spatial".to_string(),
            json!({ "bbox": [bbox], "crs": "http://www.opengis.net/def/crs/OGC/1.3/CRS84" }),
        );
    }
    if let Some((start, end)) = temporal {
        let mut temporal_obj = serde_json::Map::new();
        temporal_obj.insert(
            "interval".to_string(),
            json!([[start.to_rfc3339(), end.to_rfc3339()]]),
        );
        temporal_obj.insert(
            "trs".to_string(),
            json!("http://www.opengis.net/def/uom/ISO-8601/0/Gregorian"),
        );

        // Include individual timesteps if the engine provides them
        if let Some(times) = engine.get_available_times() {
            let values: Vec<String> = times.iter().map(|t| t.to_rfc3339()).collect();
            temporal_obj.insert("values".to_string(), json!(values));
        }

        extent.insert("temporal".to_string(), json!(temporal_obj));
    }

    // Vertical extent — advertise the available levels so a client knows
    // what `z` values it may request.
    //
    // OGC EDR 1.1 requires `interval` items, `values` items, and `vrs`
    // — and `interval`/`values` are typed as STRINGS in the schema
    // (lines 670–676 of `schemas/ogcapi-edr-1.1-bundled.json`), not
    // numbers. Floats round-trip through `Display` so a client can
    // parse them back when needed. `vrs` is taken from the kind's
    // built-in WKT/URI so a radar collection still validates against
    // the EDR schema.
    if let Some(vertical) = engine.get_vertical_extent() {
        let mut vertical_obj = serde_json::Map::new();
        if let Some((lo, hi)) = vertical.extent() {
            vertical_obj.insert(
                "interval".to_string(),
                json!([[lo.to_string(), hi.to_string()]]),
            );
        }
        let values: Vec<String> = vertical.levels.iter().map(|v| v.to_string()).collect();
        vertical_obj.insert("values".to_string(), json!(values));
        vertical_obj.insert("vrs".to_string(), json!(vertical.kind.vrs()));
        extent.insert("vertical".to_string(), json!(vertical_obj));
    }

    let parameter_names: serde_json::Map<String, serde_json::Value> = param_descs
        .iter()
        .map(|(name, desc)| {
            let mut param = json!({
                "type": "Parameter",
                "observedProperty": {
                    "label": { "en": desc.label }
                }
            });
            if !desc.unit.is_empty() {
                param["unit"] = json!({
                    "label": { "en": desc.unit },
                    "symbol": {
                        "value": desc.unit,
                        "type": "http://www.opengis.net/def/uom/UCUM/"
                    }
                });
            }
            (name.clone(), param)
        })
        .collect();

    let query_types = engine.supported_query_types();
    let mut data_queries = serde_json::Map::new();
    for qt in &query_types {
        let (endpoint, output_formats) = match qt.as_str() {
            "locations" => (
                format!("{base_url}/edr/collections/{}/locations", config.id),
                json!(["CoverageJSON", "PNG"]),
            ),
            "position" => (
                format!("{base_url}/edr/collections/{}/position", config.id),
                json!(["CoverageJSON", "PNG"]),
            ),
            "area" => (
                format!("{base_url}/edr/collections/{}/area", config.id),
                json!(["CoverageJSON"]),
            ),
            "trajectory" => (
                format!("{base_url}/edr/collections/{}/trajectory", config.id),
                json!(["CoverageJSON", "PNG"]),
            ),
            _ => continue,
        };
        data_queries.insert(
            qt.clone(),
            json!({
                "link": {
                    "href": endpoint,
                    "rel": "data",
                    "variables": {
                        "query_type": qt,
                        "output_formats": output_formats,
                        "default_output_format": "CoverageJSON"
                    }
                }
            }),
        );
    }

    let mut links = vec![json!({
        "href": format!("{base_url}/edr/collections/{}", config.id),
        "rel": "self",
        "type": "application/json",
        "title": config.title
    })];
    if let Some((title, url)) = config.license.as_ref().and_then(|l| l.card_link()) {
        links.push(json!({ "href": url, "rel": "license", "type": "text/html", "title": title }));
    }

    let mut metadata = json!({
        "id": config.id,
        "title": config.title,
        "description": config.description,
        // No `itemType`: OGC API – Common – Part 2 registers only "feature"
        // and "record", and the field describes a /collections/{id}/items
        // sub-resource — which EDR has no equivalent of (data is reached via
        // /position, /area, /trajectory, …). EDR collections are also not all
        // coverage data (CSV/PostGIS serve discrete observations), so no single
        // itemType applies. Omitted rather than mislabelled (review on #298).
        "links": links,
        "extent": extent,
        "data_queries": data_queries,
        "crs": ["http://www.opengis.net/def/crs/OGC/1.3/CRS84"],
        "parameter_names": parameter_names,
        "output_formats": ["CoverageJSON", "PNG"]
    });
    // OGC API – Common – Part 2 `keywords`: emit only when non-empty.
    if !config.keywords.is_empty() {
        metadata["keywords"] = json!(config.keywords);
    }
    metadata
}
