use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

use ds_core::config::CollectionConfig;
use ds_core::datetime::parse_datetime_interval;
use ds_core::engine::Engine;

use crate::params::{AreaQueryParams, LocationQueryParams, PositionQueryParams};
use crate::response::{area_query_result_to_json, locations_to_geojson, query_result_to_coverage_json, LocationsContext};

/// Shared state for the EDR API: a registry of collection engines + metadata.
#[derive(Clone)]
pub struct EdrState {
    pub engines: HashMap<String, Arc<dyn Engine>>,
    pub collections: HashMap<String, CollectionConfig>,
    /// Base URL for generating absolute links (e.g. "https://api.example.com").
    pub base_url: String,
}

pub type AppState = Arc<EdrState>;

fn lookup_collection<'a>(
    state: &'a EdrState,
    id: &str,
) -> Result<(&'a Arc<dyn Engine>, &'a CollectionConfig), (StatusCode, Json<serde_json::Value>)> {
    let engine = state.engines.get(id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "NotFound", "description": format!("Collection '{id}' not found") })),
        )
    })?;
    let config = state.collections.get(id).unwrap();
    Ok((engine, config))
}

pub async fn landing_page(State(state): State<AppState>) -> impl IntoResponse {
    let base = &state.base_url;
    Json(json!({
        "title": "Metocean Data Server - EDR",
        "description": "OGC API - Environmental Data Retrieval",
        "links": [
            {
                "href": format!("{base}/edr/"),
                "rel": "self",
                "type": "application/json",
                "title": "This document"
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

pub async fn conformance() -> impl IntoResponse {
    Json(json!({
        "conformsTo": [
            "http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/core",
            "http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/landing-page",
            "http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/oas30",
            "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/core",
            "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/collections",
            "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/json",
            "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/edr-geojson",
            "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/covjson"
        ]
    }))
}

pub async fn collections(State(state): State<AppState>) -> impl IntoResponse {
    let base = &state.base_url;
    let collections: Vec<serde_json::Value> = state
        .collections
        .values()
        .map(|config| build_collection_metadata(state.engines.get(&config.id).unwrap().as_ref(), config, base))
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

    let result = engine
        .query_location(&loc_id, datetime, param_names.as_deref())
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

    let body = serde_json::to_string(&query_result_to_coverage_json(&result)).unwrap();
    Ok((
        [(header::CONTENT_TYPE, "application/prs.coverage+json")],
        body,
    ))
}

pub async fn position_query(
    Path(id): Path<String>,
    Query(params): Query<PositionQueryParams>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
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

    let result = engine
        .query_position(&params.coords, datetime, param_names.as_deref())
        .map_err(|e| match &e {
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
        })?;

    let body = serde_json::to_string(&query_result_to_coverage_json(&result)).unwrap();
    Ok((
        [(header::CONTENT_TYPE, "application/prs.coverage+json")],
        body,
    ))
}

pub async fn area_query(
    Path(id): Path<String>,
    Query(params): Query<AreaQueryParams>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
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

    let result = engine
        .query_area(&params.coords, datetime, param_names.as_deref())
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

    let body = serde_json::to_string(&area_query_result_to_json(&result)).unwrap();
    Ok((
        [(header::CONTENT_TYPE, "application/prs.coverage+json")],
        body,
    ))
}

fn build_collection_metadata(engine: &dyn Engine, config: &CollectionConfig, base_url: &str) -> serde_json::Value {
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
        extent.insert(
            "temporal".to_string(),
            json!({ "interval": [[start.to_rfc3339(), end.to_rfc3339()]], "trs": "http://www.opengis.net/def/uom/ISO-8601/0/Gregorian" }),
        );
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
                json!(["CoverageJSON", "GeoJSON"]),
            ),
            "position" => (
                format!("{base_url}/edr/collections/{}/position", config.id),
                json!(["CoverageJSON"]),
            ),
            "area" => (
                format!("{base_url}/edr/collections/{}/area", config.id),
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
                        "output_formats": output_formats
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
        "output_formats": ["CoverageJSON", "GeoJSON"]
    })
}
