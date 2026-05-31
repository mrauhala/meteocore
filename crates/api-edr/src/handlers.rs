use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use ds_core::config::CollectionConfig;
use ds_core::datetime::parse_datetime_interval;
use ds_core::edr_engine::EdrEngine;

use ds_core::error::DataServerError;
use ds_core::model::CoverageResponse;
use ds_render::render_chart;

use crate::params::{
    parse_edr_format, parse_z, plot_dimensions, split_position_coords, AreaQueryParams, EdrFormat,
    LocationQueryParams, PositionQueryParams, TrajectoryQueryParams,
};
use crate::plot_convert::coverage_response_to_panels;
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

/// Reject a `z` selector against a collection with no vertical dimension.
/// Collections that have one let the engine resolve `z`; collections that
/// don't return HTTP 400 rather than silently ignoring the parameter.
fn reject_z_without_vertical(
    engine: &Arc<dyn EdrEngine>,
    z: &Option<Vec<f64>>,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if z.is_some() && engine.get_vertical_extent().is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "code": "BadRequest",
                "description": "This collection has no vertical dimension; \
                                the `z` query parameter is not supported"
            })),
        ));
    }
    Ok(())
}

pub async fn landing_page(State(state): State<AppState>) -> impl IntoResponse {
    let state = state.load_full();
    let base = &state.base_url;
    Json(json!({
        "title": "MeteoCore - EDR",
        "description": "Metocean Data Server — OGC API EDR",
        "links": [
            {
                "href": format!("{base}/edr/"),
                "rel": "self",
                "type": "application/json",
                "title": "This document"
            },
            {
                "href": format!("{base}/edr/api"),
                "rel": "service-desc",
                "type": "application/vnd.oai.openapi+json;version=3.0",
                "title": "API definition"
            },
            {
                "href": format!("{base}/edr/api/docs"),
                "rel": "service-doc",
                "type": "text/html",
                "title": "API documentation"
            },
            {
                "href": format!("{base}/edr/conformance"),
                "rel": "conformance",
                "type": "application/json",
                "title": "Conformance classes"
            },
            {
                "href": format!("{base}/edr/collections"),
                "rel": "data",
                "type": "application/json",
                "title": "Collections"
            }
        ]
    }))
}

pub async fn api_definition(State(state): State<AppState>) -> impl IntoResponse {
    let state = state.load_full();
    let mut collection_paths = json!({});
    for config in state.collections.values() {
        let id = &config.id;

        // Collection detail
        let detail_path = format!("/edr/collections/{id}");
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

        // Trajectory query (vertical cross-section). Engines that don't
        // support it return an `InvalidParameter` 400 — same precedent
        // as the unconditional `/area` and `/position` entries above.
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
                    {"$ref": "#/components/parameters/z-trajectory"}
                ],
                "responses": {
                    "200": {
                        "description": "Coverage data — CoverageJSON Section domain",
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
    }

    let mut paths = json!({
        "/edr/": {
            "get": {
                "summary": "Landing page",
                "operationId": "getLandingPage",
                "responses": {
                    "200": {"description": "Landing page"}
                }
            }
        },
        "/edr/conformance": {
            "get": {
                "summary": "Conformance classes",
                "operationId": "getConformance",
                "responses": {
                    "200": {"description": "Conformance classes"}
                }
            }
        },
        "/edr/collections": {
            "get": {
                "summary": "List collections",
                "operationId": "getCollections",
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
                    "description": "Height range for the cross-section z axis, in metres above the radar antenna. Forms: z=H (single-height slice), z=Hmin,Hmax (range bracketed by the default vertical step), z=H1,H2,H3,… (explicit levels). Absent → 0..coverage-top derived from the radar's highest sweep."
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

pub async fn conformance() -> impl IntoResponse {
    Json(json!({
        "conformsTo": [
            "http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/core",
            "http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/landing-page",
            "http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/oas30",
            "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/core",
            "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/collections",
            "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/json",
            "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/covjson"
        ]
    }))
}

pub async fn collections(State(state): State<AppState>) -> impl IntoResponse {
    let state = state.load_full();
    let base = &state.base_url;
    let collections: Vec<serde_json::Value> = state
        .collections
        .values()
        .map(|config| {
            build_collection_metadata(
                state.engines.get(&config.id).unwrap().as_ref(),
                config,
                base,
            )
        })
        .collect();

    Json(json!({
        "collections": collections,
        "links": [
            {
                "href": format!("{}/edr/collections", state.base_url),
                "rel": "self",
                "type": "application/json"
            }
        ]
    }))
}

pub async fn collection(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let state = state.load_full();
    let (engine, config) = lookup_collection(&state, &id)?;
    Ok(Json(build_collection_metadata(
        engine.as_ref(),
        config,
        &state.base_url,
    )))
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

    let z = parse_z(params.z.as_deref()).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "code": "BadRequest", "description": e.to_string() })),
        )
    })?;
    reject_z_without_vertical(engine, &z)?;

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

    let z = parse_z(params.z.as_deref()).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "code": "BadRequest", "description": e.to_string() })),
        )
    })?;
    reject_z_without_vertical(engine, &z)?;

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

    let z = parse_z(params.z.as_deref()).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "code": "BadRequest", "description": e.to_string() })),
        )
    })?;
    reject_z_without_vertical(engine, &z)?;

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

    let body = serde_json::to_string(&coverage_response_to_json(&result)).unwrap();
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
    let (engine, _config) = lookup_collection(&state, &id)?;

    // A Section is a 2-D cross-section — not a single plot.
    if parse_edr_format(params.f.as_deref()).map_err(|e| bad_request(&e))? == EdrFormat::Png {
        return Err(bad_request(&DataServerError::InvalidParameter(
            "PNG output is not available for trajectory queries".into(),
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

    let z = parse_z(params.z.as_deref()).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "code": "BadRequest", "description": e.to_string() })),
        )
    })?;
    // Trajectory `z` is a height range in metres, not a vertical-extent
    // pin against the collection's elevation-angle axis, so the standard
    // `reject_z_without_vertical` gate doesn't apply — the engine accepts
    // it whenever it can produce a Section.

    let result = engine
        .query_trajectory(
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
                tracing::error!("Trajectory query error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "code": "ServerError", "description": "Internal server error" })),
                )
            }
        })?;

    let body = serde_json::to_string(&coverage_response_to_json(&result)).unwrap();
    Ok((
        [(header::CONTENT_TYPE, "application/prs.coverage+json")],
        body,
    ))
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
    // what `z` values it may request. `vrs` is intentionally omitted:
    // vertical coordinate kinds like radar elevation angle have no
    // standard OGC vertical-CRS URI, and `vrs` is optional in OGC EDR.
    if let Some(vertical) = engine.get_vertical_extent() {
        let mut vertical_obj = serde_json::Map::new();
        if let Some((lo, hi)) = vertical.extent() {
            vertical_obj.insert("interval".to_string(), json!([[lo, hi]]));
        }
        vertical_obj.insert("values".to_string(), json!(vertical.levels));
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
                json!(["CoverageJSON"]),
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

    json!({
        "id": config.id,
        "title": config.title,
        "description": config.description,
        "links": [
            {
                "href": format!("{base_url}/edr/collections/{}", config.id),
                "rel": "self",
                "type": "application/json",
                "title": config.title
            }
        ],
        "extent": extent,
        "data_queries": data_queries,
        "crs": ["http://www.opengis.net/def/crs/OGC/1.3/CRS84"],
        "parameter_names": parameter_names,
        "output_formats": ["CoverageJSON", "PNG"]
    })
}
