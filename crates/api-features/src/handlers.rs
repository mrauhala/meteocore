use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

use ds_core::feature::FeatureQuery;
use ds_core::feature_engine::FeatureEngine;

use crate::params::{parse_bbox, ItemsQueryParams, DEFAULT_LIMIT, MAX_LIMIT};
use crate::response::{feature_page_to_geojson, feature_to_geojson};

pub type AppState = Arc<dyn FeatureEngine>;

pub async fn landing_page() -> impl IntoResponse {
    Json(json!({
        "title": "Metocean Data Server - Features",
        "description": "OGC API - Features",
        "links": [
            {
                "href": "/features/",
                "rel": "self",
                "type": "application/json",
                "title": "This document"
            },
            {
                "href": "/features/conformance",
                "rel": "conformance",
                "type": "application/json",
                "title": "Conformance classes"
            },
            {
                "href": "/features/collections",
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
            "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/core",
            "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/oas30",
            "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/geojson"
        ]
    }))
}

pub async fn collections(State(engine): State<AppState>) -> impl IntoResponse {
    let collection = build_collection_metadata(&*engine);
    Json(json!({
        "collections": [collection],
        "links": [
            {
                "href": "/features/collections",
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
            Json(json!({ "code": "NotFound", "description": "Collection not found" })),
        ));
    }
    Ok(Json(build_collection_metadata(&*engine)))
}

pub async fn items(
    Path(id): Path<String>,
    Query(params): Query<ItemsQueryParams>,
    State(engine): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    if id != "weather" {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "NotFound", "description": "Collection not found" })),
        ));
    }

    let bbox = params
        .bbox
        .as_deref()
        .map(parse_bbox)
        .transpose()
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "code": "BadRequest", "description": e.to_string() })),
            )
        })?;

    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let offset = params.offset.unwrap_or(0);

    let query = FeatureQuery {
        bbox,
        limit,
        offset,
        datetime: None,
    };

    let page = engine.get_features(&query).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "code": "ServerError", "description": "Internal server error" })),
        )
    })?;

    Ok(Json(feature_page_to_geojson(&page, &id, limit, offset)))
}

pub async fn item(
    Path((id, feature_id)): Path<(String, String)>,
    State(engine): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    if id != "weather" {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "NotFound", "description": "Collection not found" })),
        ));
    }

    let feature = engine.get_feature(&feature_id).map_err(|e| match &e {
        ds_core::error::DataServerError::FeatureNotFound(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "NotFound", "description": "Feature not found" })),
        ),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "code": "ServerError", "description": "Internal server error" })),
        ),
    })?;

    Ok(Json(feature_to_geojson(&feature, &id)))
}

fn build_collection_metadata(engine: &dyn FeatureEngine) -> serde_json::Value {
    // Get spatial extent from a full feature query
    let page = engine
        .get_features(&FeatureQuery {
            limit: 0,
            ..Default::default()
        })
        .ok();

    let total = page.as_ref().map(|p| p.number_matched).unwrap_or(0);

    json!({
        "id": "weather",
        "title": "Finnish Weather Observations",
        "description": "Weather station locations as point features",
        "itemType": "feature",
        "links": [
            {
                "href": "/features/collections/weather",
                "rel": "self",
                "type": "application/json"
            },
            {
                "href": "/features/collections/weather/items",
                "rel": "items",
                "type": "application/geo+json"
            }
        ],
        "numberItems": total
    })
}
