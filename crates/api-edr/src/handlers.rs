use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

use ds_core::datetime::parse_datetime_interval;
use ds_core::engine::Engine;

use crate::params::LocationQueryParams;
use crate::response::{locations_to_geojson, query_result_to_coverage_json};

pub type AppState = Arc<dyn Engine>;

pub async fn landing_page() -> impl IntoResponse {
    Json(json!({
        "title": "Metocean Data Server - EDR",
        "description": "OGC API - Environmental Data Retrieval",
        "links": [
            {
                "href": "/edr/",
                "rel": "self",
                "type": "application/json",
                "title": "This document"
            },
            {
                "href": "/edr/conformance",
                "rel": "conformance",
                "type": "application/json",
                "title": "Conformance classes"
            },
            {
                "href": "/edr/collections",
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
            "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/core",
            "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/collections",
            "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/json",
            "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/edr-geojson",
            "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/covjson"
        ]
    }))
}

pub async fn collections(State(engine): State<AppState>) -> impl IntoResponse {
    let collection = build_collection_metadata(&*engine);
    Json(json!({
        "collections": [collection],
        "links": [
            {
                "href": "/edr/collections",
                "rel": "self",
                "type": "application/json"
            }
        ]
    }))
}

pub async fn collection(
    Path(id): Path<String>,
    State(engine): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    if id != "weather" {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "NotFound", "description": format!("Collection '{id}' not found") })),
        ));
    }
    Ok(Json(build_collection_metadata(&*engine)))
}

pub async fn locations(
    Path(id): Path<String>,
    State(engine): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    if id != "weather" {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "NotFound", "description": format!("Collection '{id}' not found") })),
        ));
    }
    let locs = engine.get_locations().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "code": "ServerError", "description": e.to_string() })),
        )
    })?;
    Ok(Json(locations_to_geojson(&locs)))
}

pub async fn location_query(
    Path((id, loc_id)): Path<(String, String)>,
    Query(params): Query<LocationQueryParams>,
    State(engine): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    if id != "weather" {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "NotFound", "description": format!("Collection '{id}' not found") })),
        ));
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

    let result = engine
        .query_location(
            &loc_id,
            datetime,
            param_names.as_deref(),
        )
        .map_err(|e| match &e {
            ds_core::error::DataServerError::LocationNotFound(_) => (
                StatusCode::NOT_FOUND,
                Json(json!({ "code": "NotFound", "description": e.to_string() })),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "code": "ServerError", "description": e.to_string() })),
            ),
        })?;

    Ok(Json(query_result_to_coverage_json(&result)))
}

fn build_collection_metadata(engine: &dyn Engine) -> serde_json::Value {
    let params = engine.get_parameters();
    let temporal = engine.get_temporal_extent();
    let spatial = engine.get_spatial_extent();

    let mut extent = serde_json::Map::new();
    if let Some(bbox) = spatial {
        extent.insert(
            "spatial".to_string(),
            json!({ "bbox": [bbox], "crs": "CRS84" }),
        );
    }
    if let Some((start, end)) = temporal {
        extent.insert(
            "temporal".to_string(),
            json!({ "interval": [[start.to_rfc3339(), end.to_rfc3339()]], "trs": "TIMECRS[\"DateTime\",TDATUM[\"Gregorian Calendar\"],CS[TemporalDateTime,1],AXIS[\"Time (T)\",future]]" }),
        );
    }

    let parameter_names: serde_json::Map<String, serde_json::Value> = params
        .iter()
        .map(|p| {
            (
                p.clone(),
                json!({
                    "type": "Parameter",
                    "observedProperty": {
                        "label": { "en": p.replace('_', " ") }
                    }
                }),
            )
        })
        .collect();

    json!({
        "id": "weather",
        "title": "Finnish Weather Observations",
        "description": "Hourly weather observations from Finnish weather stations",
        "links": [
            {
                "href": "/edr/collections/weather",
                "rel": "self",
                "type": "application/json"
            }
        ],
        "extent": extent,
        "data_queries": {
            "locations": {
                "link": {
                    "href": "/edr/collections/weather/locations",
                    "rel": "data",
                    "variables": {
                        "query_type": "locations",
                        "output_formats": ["CoverageJSON", "GeoJSON"]
                    }
                }
            }
        },
        "parameter_names": parameter_names,
        "output_formats": ["CoverageJSON", "GeoJSON"]
    })
}
