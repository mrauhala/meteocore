use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
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

type HandlerError = (StatusCode, Json<serde_json::Value>);

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
fn with_vary(mut resp: Response) -> Response {
    // `append` (not `insert`) so a `Vary` set upstream (e.g. compression's
    // `Vary: Accept-Encoding`) isn't clobbered.
    resp.headers_mut()
        .append(header::VARY, HeaderValue::from_static("accept"));
    resp
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
    let title = "MeteoCore - Features";
    let description = "Metocean Data Server — OGC API Features";
    // (href, rel, type, title) — one source for both representations.
    let links = [
        (
            format!("{base}/features/"),
            "self",
            "application/json",
            "This document",
        ),
        (
            format!("{base}/features/api"),
            "service-desc",
            "application/vnd.oai.openapi+json;version=3.0",
            "API definition",
        ),
        (
            format!("{base}/features/api/docs"),
            "service-doc",
            "text/html",
            "API documentation",
        ),
        (
            format!("{base}/features/conformance"),
            "conformance",
            "application/json",
            "Conformance classes",
        ),
        (
            format!("{base}/features/collections"),
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
            // collection-detail HTML page).
            views.push(LinkView::new(
                format!("{base}/features/?f=json"),
                "alternate",
                Some("This document as JSON"),
            ));
            Html(ds_core::html::landing_html(title, description, &views)).into_response()
        }
    }))
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
        // OGC API – Common – Part 1: Core and Part 2: Geospatial Data. The
        // Features landing page, /conformance, /api, and
        // /collections{,/{id}} satisfy these structurally — the same
        // declaration #292 added for Maps and Tiles. The HTML class
        // (.../conf/html) is now declared — the HTML representation of the
        // metadata endpoints is served via `?f=html` / Accept.
        "http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/core",
        "http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/landing-page",
        "http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/oas30",
        "http://www.opengis.net/spec/ogcapi-common-2/1.0/conf/collections",
        "http://www.opengis.net/spec/ogcapi-common-2/1.0/conf/json",
        "http://www.opengis.net/spec/ogcapi-common-2/1.0/conf/html",
        // OGC API - Common - Part 4 (Discovery within many collections,
        // draft 25-046): /collections supports bbox/bbox-crs/datetime/q/
        // limit filtering + offset pagination. Builds on the Common Part 2
        // "collections" class declared just above.
        "http://www.opengis.net/spec/ogcapi-common-4/1.0/conf/searchable-collections",
        "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/core",
        "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/oas30",
        "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/geojson",
    ];
    Ok(with_vary(match wanted {
        Wanted::Json => Json(json!({ "conformsTo": classes })).into_response(),
        Wanted::Html => {
            let nav = [
                LinkView::new(format!("{base}/features/"), "up", Some("Landing page")),
                LinkView::new(
                    format!("{base}/features/conformance?f=json"),
                    "alternate",
                    Some("This document as JSON"),
                ),
            ];
            Html(ds_core::html::conformance_html(&classes, &nav)).into_response()
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
        let detail_path = format!("/features/collections/{id}");
        let items_path = format!("/features/collections/{id}/items");
        let item_path = format!("/features/collections/{id}/items/{{featureId}}");

        // Collection detail. OGC API – Common – Part 2 `conf/json` requires the
        // API definition to describe every collection resource, including
        // GET /collections/{id} — Maps and Tiles already do; Features was the
        // odd one out (review on #298).
        collection_paths[&detail_path] = json!({
            "get": {
                "summary": format!("Get {} collection metadata", config.title),
                "operationId": format!("getCollection_{id}"),
                "tags": [id],
                "parameters": [format_parameter()],
                "responses": {
                    "200": {
                        "description": "Collection metadata",
                        "content": {"application/json": {}}
                    },
                    "404": {"description": "Collection not found"}
                }
            }
        });

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
                "parameters": [format_parameter()],
                "responses": {
                    "200": {"description": "Landing page"}
                }
            }
        },
        "/features/conformance": {
            "get": {
                "summary": "Conformance classes",
                "operationId": "getConformance",
                "parameters": [format_parameter()],
                "responses": {
                    "200": {"description": "Conformance classes"}
                }
            }
        },
        "/features/collections": {
            "get": {
                "summary": "List collections",
                "operationId": "getCollections",
                "parameters": searchable_collections_parameters(),
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
            "title": "MeteoCore - OGC API Features",
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

pub async fn api_docs(State(state): State<AppState>) -> impl IntoResponse {
    let state = state.load_full();
    let spec_url = format!("{}/features/api", state.base_url);
    axum::response::Html(ds_core::openapi::swagger_ui_html(
        "MeteoCore - Features API",
        &spec_url,
    ))
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

    // (id, title, description, bbox, metadata, keywords, license) per
    // collection. Features carry no temporal extent today (`time: None`), so per
    // OGC API – Common – Part 4 §7.14.3 ("unknown extent ≡ unbounded") they
    // *match* any `datetime` filter rather than being excluded — see
    // `collection_search::matches`. keywords/license feed `?q=` and the HTML
    // cards. Tuple element types are inferred.
    let mut rows: Vec<_> = state
        .collections
        .values()
        .filter_map(|config| {
            let Some(engine) = state.engines.get(&config.id) else {
                tracing::warn!(
                    collection = %config.id,
                    "collection has no registered feature engine; omitting from /collections"
                );
                return None;
            };
            let value = build_collection_metadata(engine.as_ref(), config, base);
            Some((
                config.id.clone(),
                config.title.clone(),
                config.description.clone(),
                engine.spatial_extent(),
                value,
                config.keywords.clone(),
                config.license.as_ref().map(|l| l.card_label()),
            ))
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    let matches: Vec<CollectionMatch> = rows
        .iter()
        .map(|r| CollectionMatch {
            title: &r.1,
            description: &r.2,
            keywords: &r.5,
            bbox: r.3,
            time: None,
        })
        .collect();
    let result = search(&matches, &params);
    let href = |offset| {
        format!(
            "{base}/features/collections{}",
            sp.query_string(params.limit, offset)
        )
    };

    Ok(with_vary(match wanted {
        Wanted::Json => {
            let collections: Vec<serde_json::Value> =
                result.page.iter().map(|&i| rows[i].4.clone()).collect();
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
                    self_href: format!("{base}/features/collections/{}", rows[i].0),
                    keywords: rows[i].5.clone(),
                    license: rows[i].6.clone(),
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
                    "{base}/features/collections{}",
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
                self_href: format!("{base}/features/collections/{}", config.id),
                keywords: config.keywords.clone(),
                license: config.license.as_ref().map(|l| l.card_label()),
            };
            let links = [
                LinkView::new(
                    format!("{base}/features/collections/{}?f=json", config.id),
                    "alternate",
                    Some("JSON"),
                ),
                LinkView::new(
                    format!("{base}/features/collections"),
                    "collection",
                    Some("All collections"),
                ),
            ];
            Html(ds_core::html::collection_html(&card, &links)).into_response()
        }
    }))
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

    let mut links = vec![
        json!({
            "href": format!("{base_url}/features/collections/{}", config.id),
            "rel": "self",
            "type": "application/json",
            "title": config.title
        }),
        json!({
            "href": format!("{base_url}/features/collections/{}/items", config.id),
            "rel": "items",
            "type": "application/geo+json",
            "title": "Items"
        }),
    ];

    // If this collection is also exposed through OGC API Tiles, advertise the
    // tilesets list so clients can discover the vector-tile representation
    // without probing. Per OGC API – Tiles 1.0 §7.1, the `tilesets-vector`
    // relation targets the tilesets list resource (`application/json`), not a
    // tile URL template — the per-tile URL template lives one level deeper
    // inside the tilesets-list response as `rel: item`. Linking to the list
    // also avoids hardcoding `WebMercatorQuad`; the list enumerates every
    // supported TileMatrixSet.
    if config.apis.iter().any(|a| a == "tiles") {
        links.push(json!({
            "href": format!("{base_url}/tiles/collections/{}/tiles", config.id),
            "rel": "http://www.opengis.net/def/rel/ogc/1.0/tilesets-vector",
            "type": "application/json",
            "title": "Vector tilesets"
        }));
    }

    if let Some((title, url)) = config.license.as_ref().and_then(|l| l.card_link()) {
        // No `type`: an operator-supplied license URL may not be HTML, and OGC
        // API Common §6.5.2 wants the link's real media type — omitting is valid.
        links.push(json!({ "href": url, "rel": "license", "title": title }));
    }

    let mut metadata = json!({
        "id": config.id,
        "title": config.title,
        "description": config.description,
        "itemType": "feature",
        "crs": [
            "http://www.opengis.net/def/crs/OGC/1.3/CRS84"
        ],
        "storageCrs": "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
        "links": links,
        "numberItems": total
    });
    // OGC API – Common – Part 2 `keywords`: emit only when non-empty.
    if !config.keywords.is_empty() {
        metadata["keywords"] = json!(config.keywords);
    }

    // Add spatial extent if available. A Features collection contributes only
    // a bbox (no grid/temporal/vertical), but it shares the one extent builder
    // in `ds_core::ogc_extent` so the `/features` shape can't drift from
    // `/maps` and `/tiles` (issue #263).
    if let Some(bbox) = engine.spatial_extent() {
        if let Some(extent) = ds_core::ogc_extent::build_extent(Some(bbox), None, "", &[], None) {
            metadata["extent"] = serde_json::to_value(extent).expect("Extent serializes to JSON");
        }
    }

    metadata
}
