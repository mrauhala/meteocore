use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use chrono::Utc;
use serde_json::json;

use ds_core::config::CollectionConfig;
use ds_core::feature::FeatureQuery;
use ds_core::feature_engine::FeatureEngine;

use crate::params::{parse_bbox, parse_datetime, ItemsQueryParams, DEFAULT_LIMIT, MAX_LIMIT};
use crate::response::{feature_page_to_geojson, feature_to_geojson};

/// Shared state for the Features API: a registry of collection engines + metadata.
#[derive(Clone)]
pub struct FeaturesState {
    pub engines: HashMap<String, Arc<dyn FeatureEngine>>,
    pub collections: HashMap<String, CollectionConfig>,
    /// Base URL for generating absolute links (e.g. "https://api.example.com").
    pub base_url: String,
}

pub type AppState = Arc<ArcSwap<FeaturesState>>;

/// Custom response type for GeoJSON with correct Content-Type.
pub struct GeoJsonResponse(pub serde_json::Value);

impl IntoResponse for GeoJsonResponse {
    fn into_response(self) -> axum::response::Response {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/geo+json"),
        );
        (headers, Json(self.0)).into_response()
    }
}

#[allow(clippy::type_complexity)]
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
    let config = state.collections.get(id).ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "code": "ServerError", "description": "Internal server error" })),
        )
    })?;
    Ok((engine, config))
}

pub async fn landing_page(State(state): State<AppState>) -> impl IntoResponse {
    let state = state.load_full();
    let base = &state.base_url;
    Json(json!({
        "title": "Metocean Data Server - Features",
        "description": "OGC API - Features",
        "links": [
            {
                "href": format!("{base}/features/"),
                "rel": "self",
                "type": "application/json",
                "title": "This document"
            },
            {
                "href": format!("{base}/features/api"),
                "rel": "service-desc",
                "type": "application/vnd.oai.openapi+json;version=3.0",
                "title": "API definition"
            },
            {
                "href": format!("{base}/features/conformance"),
                "rel": "conformance",
                "type": "application/json",
                "title": "Conformance classes"
            },
            {
                "href": format!("{base}/features/collections"),
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

pub async fn api_definition(State(state): State<AppState>) -> impl IntoResponse {
    let state = state.load_full();
    let mut collection_paths = json!({});
    for config in state.collections.values() {
        let id = &config.id;
        let items_path = format!("/features/collections/{id}/items");
        let item_path = format!("/features/collections/{id}/items/{{featureId}}");

        collection_paths[&items_path] = json!({
            "get": {
                "summary": format!("Get features from {}", config.title),
                "operationId": format!("getFeatures_{id}"),
                "tags": [id],
                "parameters": [
                    {"$ref": "#/components/parameters/bbox"},
                    {"$ref": "#/components/parameters/limit"},
                    {"$ref": "#/components/parameters/offset"},
                    {"$ref": "#/components/parameters/datetime"}
                ],
                "responses": {
                    "200": {
                        "description": "Features in GeoJSON format",
                        "content": {
                            "application/geo+json": {
                                "schema": {"$ref": "#/components/schemas/featureCollectionGeoJSON"}
                            }
                        }
                    },
                    "400": {"description": "Bad request"},
                    "404": {"description": "Collection not found"},
                    "500": {"description": "Server error"}
                }
            }
        });
        collection_paths[&item_path] = json!({
            "get": {
                "summary": format!("Get a single feature from {}", config.title),
                "operationId": format!("getFeature_{id}"),
                "tags": [id],
                "parameters": [
                    {
                        "name": "featureId",
                        "in": "path",
                        "required": true,
                        "schema": {"type": "string"}
                    }
                ],
                "responses": {
                    "200": {
                        "description": "A single feature in GeoJSON format",
                        "content": {
                            "application/geo+json": {
                                "schema": {"$ref": "#/components/schemas/featureGeoJSON"}
                            }
                        }
                    },
                    "404": {"description": "Feature not found"},
                    "500": {"description": "Server error"}
                }
            }
        });
    }

    let mut paths = json!({
        "/features/": {
            "get": {
                "summary": "Landing page",
                "operationId": "getLandingPage",
                "responses": {
                    "200": {"description": "Landing page"}
                }
            }
        },
        "/features/conformance": {
            "get": {
                "summary": "Conformance classes",
                "operationId": "getConformance",
                "responses": {
                    "200": {"description": "Conformance classes"}
                }
            }
        },
        "/features/collections": {
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
            "title": "Metocean Data Server - OGC API Features",
            "version": "1.0.0",
            "description": "OGC API - Features implementation"
        },
        "paths": paths,
        "components": {
            "parameters": {
                "bbox": {
                    "name": "bbox",
                    "in": "query",
                    "required": false,
                    "schema": {
                        "type": "array",
                        "items": {"type": "number"},
                        "minItems": 4,
                        "maxItems": 6
                    },
                    "style": "form",
                    "explode": false
                },
                "limit": {
                    "name": "limit",
                    "in": "query",
                    "required": false,
                    "schema": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 1000,
                        "default": 100
                    }
                },
                "offset": {
                    "name": "offset",
                    "in": "query",
                    "required": false,
                    "schema": {
                        "type": "integer",
                        "minimum": 0,
                        "default": 0
                    }
                },
                "datetime": {
                    "name": "datetime",
                    "in": "query",
                    "required": false,
                    "schema": {"type": "string"},
                    "description": "RFC 3339 datetime or interval (start/end, ../end, start/..)"
                }
            },
            "schemas": {
                "featureCollectionGeoJSON": {
                    "type": "object",
                    "required": ["type", "features"],
                    "properties": {
                        "type": {"type": "string", "enum": ["FeatureCollection"]},
                        "features": {"type": "array", "items": {"$ref": "#/components/schemas/featureGeoJSON"}},
                        "numberMatched": {"type": "integer"},
                        "numberReturned": {"type": "integer"},
                        "timeStamp": {"type": "string", "format": "date-time"},
                        "links": {"type": "array", "items": {"$ref": "#/components/schemas/link"}}
                    }
                },
                "featureGeoJSON": {
                    "type": "object",
                    "required": ["type", "geometry", "properties"],
                    "properties": {
                        "type": {"type": "string", "enum": ["Feature"]},
                        "id": {"oneOf": [{"type": "string"}, {"type": "number"}]},
                        "geometry": {"nullable": true},
                        "properties": {"type": "object", "nullable": true},
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

pub async fn collections(State(state): State<AppState>) -> impl IntoResponse {
    let state = state.load_full();
    let base = &state.base_url;
    let mut collections: Vec<serde_json::Value> = state
        .collections
        .values()
        .filter_map(|config| {
            let engine = state.engines.get(&config.id)?;
            Some(build_collection_metadata(engine.as_ref(), config, base))
        })
        .collect();

    // Sort by id for deterministic ordering
    collections.sort_by(|a, b| {
        a["id"]
            .as_str()
            .unwrap_or("")
            .cmp(b["id"].as_str().unwrap_or(""))
    });

    Json(json!({
        "collections": collections,
        "links": [
            {
                "href": format!("{base}/features/collections"),
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

pub async fn items(
    Path(id): Path<String>,
    Query(params): Query<ItemsQueryParams>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let state = state.load_full();
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

    let datetime = params
        .datetime
        .as_deref()
        .map(parse_datetime)
        .transpose()
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "code": "BadRequest", "description": e.to_string() })),
            )
        })?;

    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = params.offset.unwrap_or(0);

    let query = FeatureQuery {
        bbox,
        limit,
        offset,
        datetime,
    };

    let page = engine.get_features(&query).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "code": "ServerError", "description": "Internal server error" })),
        )
    })?;

    let timestamp = Utc::now().to_rfc3339();
    Ok(GeoJsonResponse(feature_page_to_geojson(
        &page,
        &id,
        limit,
        offset,
        &timestamp,
        &state.base_url,
    )))
}

pub async fn item(
    Path((id, feature_id)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let state = state.load_full();
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

    Ok(GeoJsonResponse(feature_to_geojson(
        &feature,
        &id,
        &state.base_url,
    )))
}

fn build_collection_metadata(
    engine: &dyn FeatureEngine,
    config: &CollectionConfig,
    base_url: &str,
) -> serde_json::Value {
    let total = engine.feature_count();

    let mut metadata = json!({
        "id": config.id,
        "title": config.title,
        "description": config.description,
        "itemType": "feature",
        "crs": [
            "http://www.opengis.net/def/crs/OGC/1.3/CRS84"
        ],
        "storageCrs": "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
        "links": [
            {
                "href": format!("{base_url}/features/collections/{}", config.id),
                "rel": "self",
                "type": "application/json",
                "title": config.title
            },
            {
                "href": format!("{base_url}/features/collections/{}/items", config.id),
                "rel": "items",
                "type": "application/geo+json",
                "title": "Items"
            }
        ],
        "numberItems": total
    });

    // Add spatial extent if available
    if let Some(bbox) = engine.spatial_extent() {
        metadata["extent"] = json!({
            "spatial": {
                "bbox": [bbox],
                "crs": "http://www.opengis.net/def/crs/OGC/1.3/CRS84"
            }
        });
    }

    metadata
}
