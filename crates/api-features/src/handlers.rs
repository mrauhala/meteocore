use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

use ds_core::config::CollectionConfig;
use ds_core::feature::FeatureQuery;
use ds_core::feature_engine::FeatureEngine;

use crate::params::{parse_bbox, ItemsQueryParams, DEFAULT_LIMIT, MAX_LIMIT};
use crate::response::{feature_page_to_geojson, feature_to_geojson};

/// Shared state for the Features API: a registry of collection engines + metadata.
#[derive(Clone)]
pub struct FeaturesState {
    pub engines: HashMap<String, Arc<dyn FeatureEngine>>,
    pub collections: HashMap<String, CollectionConfig>,
}

pub type AppState = Arc<FeaturesState>;

fn lookup_collection<'a>(
    state: &'a FeaturesState,
    id: &str,
) -> Result<(&'a Arc<dyn FeatureEngine>, &'a CollectionConfig), (StatusCode, Json<serde_json::Value>)>
{
    let engine = state.engines.get(id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "NotFound", "description": "Collection not found" })),
        )
    })?;
    let config = state.collections.get(id).unwrap();
    Ok((engine, config))
}

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

pub async fn collections(State(state): State<AppState>) -> impl IntoResponse {
    let collections: Vec<serde_json::Value> = state
        .collections
        .values()
        .map(|config| build_collection_metadata(state.engines.get(&config.id).unwrap().as_ref(), config))
        .collect();

    Json(json!({
        "collections": collections,
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
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let (engine, config) = lookup_collection(&state, &id)?;
    Ok(Json(build_collection_metadata(engine.as_ref(), config)))
}

pub async fn items(
    Path(id): Path<String>,
    Query(params): Query<ItemsQueryParams>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let (engine, _config) = lookup_collection(&state, &id)?;

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
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let (engine, _config) = lookup_collection(&state, &id)?;

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

fn build_collection_metadata(engine: &dyn FeatureEngine, config: &CollectionConfig) -> serde_json::Value {
    let page = engine
        .get_features(&FeatureQuery {
            limit: 0,
            ..Default::default()
        })
        .ok();

    let total = page.as_ref().map(|p| p.number_matched).unwrap_or(0);

    json!({
        "id": config.id,
        "title": config.title,
        "description": config.description,
        "itemType": "feature",
        "links": [
            {
                "href": format!("/features/collections/{}", config.id),
                "rel": "self",
                "type": "application/json"
            },
            {
                "href": format!("/features/collections/{}/items", config.id),
                "rel": "items",
                "type": "application/geo+json"
            }
        ],
        "numberItems": total
    })
}
