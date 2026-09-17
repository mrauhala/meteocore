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

use crate::params::{
    parse_bbox, parse_datetime, parse_sortby, ItemsQueryParams, DEFAULT_LIMIT, MAX_LIMIT,
};
use crate::response::{feature_page_to_geojson, feature_to_geojson, preserved_query};

/// Shared state for the Features API: a registry of collection engines + metadata.
#[derive(Clone)]
pub struct FeaturesState {
    pub engines: HashMap<String, Arc<dyn FeatureEngine>>,
    pub collections: HashMap<String, CollectionConfig>,
    /// Static fallback base URL for absolute links (e.g. "https://api.example.com").
    /// Used as-is unless `trust_proxy_headers` resolves a per-request value.
    pub base_url: String,
    /// Honour reverse-proxy forwarding headers when generating self-links (#12).
    pub trust_proxy_headers: bool,
}

pub type AppState = Arc<ArcSwap<FeaturesState>>;

/// Resolve the absolute base URL for the current request, honouring reverse-proxy
/// forwarding headers when `trust_proxy_headers` is enabled (#12).
fn request_base_url(state: &FeaturesState, headers: &HeaderMap) -> String {
    ds_core::proxy::resolve_base_url(&state.base_url, state.trust_proxy_headers, |name| {
        headers.get(name).and_then(|v| v.to_str().ok())
    })
}

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

/// Feature clients also use the GeoJSON media type as the format identifier.
fn negotiate_feature(
    f: Option<&str>,
    headers: &HeaderMap,
) -> Result<ds_core::html::Wanted, HandlerError> {
    let normalized = f.map(str::trim).map(|value| {
        if value.eq_ignore_ascii_case("application/geo+json")
            || value.eq_ignore_ascii_case("application/json")
        {
            "json"
        } else if value.eq_ignore_ascii_case("text/html") {
            "html"
        } else {
            value
        }
    });
    negotiate(normalized, headers)
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
    let base = &request_base_url(&state, &headers);
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
    let base = &request_base_url(&state, &headers);
    let classes = api_common::conformance_classes(&[
        "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/core",
        "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/oas30",
        "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/geojson",
        "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/html",
    ]);
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
/// metadata and feature endpoints.
fn format_parameter() -> serde_json::Value {
    json!({"name": "f", "in": "query", "required": false, "schema": {"type": "string", "enum": ["json", "html"]},
           "description": "Output format. 'json' (default) or 'html'; overrides the Accept header."})
}

fn feature_format_parameter() -> serde_json::Value {
    json!({"name": "f", "in": "query", "required": false,
        "schema": {"type": "string", "enum": ["json", "html", "application/geo+json", "application/json", "text/html"]},
        "description": "Output format, case-insensitive; overrides Accept. JSON aliases return GeoJSON. Encode the plus sign as %2B in application/geo+json."})
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
                    {"$ref": "#/components/parameters/datetime"},
                    {"$ref": "#/components/parameters/sortby"},
                    feature_format_parameter()
                ],
                "responses": {
                    "200": {
                        "description": "Features in GeoJSON or HTML format",
                        "content": {
                            "application/geo+json": {
                                "schema": {"$ref": "#/components/schemas/featureCollectionGeoJSON"}
                            },
                            "text/html": {"schema": {"type": "string"}}
                        }
                    },
                    "400": {"description": "Bad request"},
                    "404": {"description": "Collection not found"},
                    "500": {"description": "Server error"}
                }
            }
        });
        // Part 1 §7.15.5–6 uses ordinary named query parameters, not CQL2.
        // The catalog is a cheap snapshot, including producer-defined names.
        if let Some(engine) = state.engines.get(id) {
            let parameters = collection_paths[&items_path]["get"]["parameters"]
                .as_array_mut()
                .unwrap();
            for name in engine
                .filterables()
                .iter()
                .filter(|n| !crate::params::is_reserved_parameter(n))
            {
                parameters.push(json!({
                    "name": name,
                    "in": "query",
                    "required": false,
                    "description": "Exact, case-sensitive property equality (OGC API Features Part 1 §7.15.5–6). Lists match any element; numbers and booleans use canonical string form. Numeric values also accept comma-separated alternatives (OR within that predicate, no spaces); commas in strings remain literal. Null/missing never match. All predicates, including repeated names, are ANDed before paging.",
                    "style": "form",
                    "explode": false,
                    "schema": {"type": "string"}
                }));
            }
        }
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
                    },
                    feature_format_parameter()
                ],
                "responses": {
                    "200": {
                        "description": "A single feature in GeoJSON or HTML format",
                        "content": {
                            "application/geo+json": {
                                "schema": {"$ref": "#/components/schemas/featureGeoJSON"}
                            },
                            "text/html": {"schema": {"type": "string"}}
                        }
                    },
                    "400": {"description": "Bad request"},
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
        "/features/collections": {"get": api_common::collection_operation()}
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
                },
                // Schema reproduced verbatim from OGC API - Features Part 8:
                // Sorting (draft 24-030). Do not "simplify" it to a plain
                // string: `style: form` + `explode: false` is what makes the
                // comma-separated form normative rather than incidental.
                "sortby": {
                    "name": "sortby",
                    "in": "query",
                    "required": false,
                    "schema": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "string",
                            "pattern": "[+|-]?[A-Za-z_].*"
                        }
                    },
                    "style": "form",
                    "explode": false,
                    "description": "Comma-separated sort properties, '-' for descending ('+' or no prefix for ascending). Valid properties are collection-specific; an unsupported one is rejected with 400."
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

pub async fn api_docs(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let state = state.load_full();
    let spec_url = format!("{}/features/api", request_base_url(&state, &headers));
    (
        [
            (
                header::CONTENT_SECURITY_POLICY,
                ds_core::openapi::SWAGGER_UI_CSP,
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        axum::response::Html(ds_core::openapi::swagger_ui_html(
            "MeteoCore - Features API",
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

pub async fn collections(
    State(state): State<AppState>,
    request: api_common::CollectionRequest,
    headers: HeaderMap,
) -> Response {
    let state = state.load_full();
    let base = &request_base_url(&state, &headers);
    let entries = state
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
            Some(api_common::CollectionEntry {
                config,
                metadata: build_collection_metadata(engine.as_ref(), config, base),
                bbox: engine.spatial_extent(),
                time: engine.temporal_extent(),
            })
        })
        .collect();
    api_common::collections_response(&format!("{base}/features/collections"), request, entries)
}

/// GET /features/collections/{id} — Collection detail
pub async fn collection(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Query(fp): Query<ds_core::html::FormatParams>,
    headers: HeaderMap,
) -> Result<Response, HandlerError> {
    use ds_core::html::{LinkView, Wanted};
    let wanted = negotiate(fp.f.as_deref(), &headers)?;
    let state = state.load_full();
    let (engine, config) = lookup_collection(&state, &id)?;
    let base = &request_base_url(&state, &headers);
    Ok(with_vary(match wanted {
        Wanted::Json => {
            Json(build_collection_metadata(engine.as_ref(), config, base)).into_response()
        }
        Wanted::Html => {
            let card = api_common::collection_card(
                config,
                format!("{base}/features/collections/{}", config.id),
            );
            let links = [
                LinkView::new(
                    format!("{base}/features/collections/{}/items?f=html", config.id),
                    "items",
                    Some("Browse features"),
                ),
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
    Query(pairs): Query<Vec<(String, String)>>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let state = state.load_full();
    let (engine, config) = lookup_collection(&state, &id)?;
    let bad_request = |e: ds_core::error::DataServerError| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "code": "BadRequest", "description": e.to_string() })),
        )
    };
    let params = ItemsQueryParams::from_pairs(pairs).map_err(bad_request)?;
    let wanted = negotiate_feature(params.f.as_deref(), &headers)?;
    params
        .validate_filters(&engine.filterables())
        .map_err(bad_request)?;

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

    // Validated against what this engine can actually sort on, so an unknown
    // property is a 400 naming the valid ones rather than a parameter that
    // quietly does nothing (OGC API - Features Part 8, draft 24-030).
    let sortby = params
        .sortby
        .as_deref()
        .map(|s| parse_sortby(s, engine.sortables()))
        .transpose()
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "code": "BadRequest", "description": e.to_string() })),
            )
        })?
        .unwrap_or_default();

    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = params.offset.unwrap_or(0);

    // Cache-Control policy (#499): a settled window (closed datetime interval
    // entirely in the past) gets the long policy, everything else the short
    // one — captured before `datetime` moves into the query.
    let (window_start, window_end) = datetime
        .as_ref()
        .map(|d| (d.start, d.end))
        .unwrap_or((None, None));
    let cache_control =
        ds_core::http_cache::data_cache_control(window_start, window_end, Utc::now());

    let query = FeatureQuery {
        bbox,
        limit,
        offset,
        datetime,
        sortby,
        property_filters: params.property_filters,
    };

    let page = engine.get_features(&query).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "code": "ServerError", "description": "Internal server error" })),
        )
    })?;

    // Hash the ETag over the document with an empty `timeStamp` placeholder:
    // the field is the response *generation* time, so a hash over the final
    // body would change on every request and `If-None-Match` would never
    // match. The `caching::conditional_get` middleware honours this
    // precomputed ETag instead of hashing the body.
    // Carry the caller's filters and ordering onto the pagination links:
    // following `rel="next"` is the OGC-recommended pattern, and a next link
    // that drops them silently serves page 2 unfiltered and unsorted.
    let filters = preserved_query(
        query.bbox.as_ref(),
        query.datetime.as_ref(),
        &query.sortby,
        &query.property_filters,
    );
    let mut doc = feature_page_to_geojson(
        &page,
        &id,
        limit,
        offset,
        &filters,
        "",
        &request_base_url(&state, &headers),
    );
    crate::html::representation_links(&mut doc, wanted);
    let render = |doc: &serde_json::Value| match wanted {
        ds_core::html::Wanted::Json => {
            serde_json::to_string(doc).expect("GeoJSON Value serializes")
        }
        ds_core::html::Wanted::Html => {
            crate::html::features_html(doc, &config.title, &id, &request_base_url(&state, &headers))
        }
    };
    // Hash the selected representation with the volatile timestamp blanked.
    let etag = ds_core::http_cache::etag_of(render(&doc).as_bytes());
    doc["timeStamp"] = json!(Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    let mut resp = match wanted {
        ds_core::html::Wanted::Json => GeoJsonResponse(doc).into_response(),
        ds_core::html::Wanted::Html => Html(render(&doc)).into_response(),
    };
    resp.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&etag).expect("quoted-hex etag is a valid header value"),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    Ok(with_vary(resp))
}

pub async fn item(
    Path((id, feature_id)): Path<(String, String)>,
    Query(fp): Query<ds_core::html::FormatParams>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let state = state.load_full();
    let (engine, config) = lookup_collection(&state, &id)?;

    let wanted = negotiate_feature(fp.f.as_deref(), &headers)?;
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

    let base = request_base_url(&state, &headers);
    let mut doc = feature_to_geojson(&feature, &id, &base);
    crate::html::representation_links(&mut doc, wanted);
    Ok(with_vary(match wanted {
        ds_core::html::Wanted::Json => GeoJsonResponse(doc).into_response(),
        ds_core::html::Wanted::Html => {
            Html(crate::html::features_html(&doc, &config.title, &id, &base)).into_response()
        }
    }))
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

    let mut metadata = api_common::collection_metadata(
        config,
        json!({
            "itemType": "feature",
            "crs": [
                "http://www.opengis.net/def/crs/OGC/1.3/CRS84"
            ],
            "storageCrs": "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
            "numberItems": total
        }),
        links,
    );

    // Add spatial + temporal extent if available. A Features collection
    // contributes a bbox and (for time-aware engines like CAP) a temporal
    // interval, but shares the one extent builder in `ds_core::ogc_extent` so the
    // `/features` shape can't drift from `/maps` and `/tiles` (issue #263). The
    // builder emits a temporal extent from the interval's endpoints, so pass them
    // as the two-element `times` slice.
    let times: Vec<chrono::DateTime<chrono::Utc>> = engine
        .temporal_extent()
        .map(|(start, end)| vec![start, end])
        .unwrap_or_default();
    if let Some(mut extent) =
        ds_core::ogc_extent::build_extent(engine.spatial_extent(), None, "", &times, None)
    {
        // Extent endpoints describe bounds, not two regularly sampled observations.
        if let Some(temporal) = &mut extent.temporal {
            temporal.grid = None;
        }
        metadata["extent"] = serde_json::to_value(extent).expect("Extent serializes to JSON");
    }

    metadata
}
