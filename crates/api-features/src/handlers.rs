use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::{Extension, Json};
use chrono::Utc;
use serde_json::{json, Map, Value};

use api_common::{JsonError, JsonQuery, Mount};

use ds_core::config::CollectionConfig;
use ds_core::discovery::Discovery;
use ds_core::feature::FeatureQuery;
use ds_core::feature_engine::FeatureEngine;

use crate::crs::{FeatureCrs, ResponseCrs};
use crate::params::{
    parse_bbox_values, parse_datetime, parse_sortby, ItemQueryParams, ItemsQueryParams,
    DEFAULT_LIMIT, MAX_LIMIT,
};
use crate::response::{feature_page_to_geojson, feature_to_geojson, preserved_query, Annotations};

/// Shared state for the Features API: a registry of collection engines + metadata.
#[derive(Clone)]
pub struct FeaturesState {
    pub engines: HashMap<String, Arc<dyn FeatureEngine>>,
    pub collections: HashMap<String, CollectionConfig>,
    /// What collection descriptions read beyond their configs: the WIS2
    /// discovery catalogue (#1356).
    pub discovery: Discovery,
    /// Static fallback base URL for absolute links (e.g. "https://api.example.com").
    /// Used as-is unless `trust_proxy_headers` resolves a per-request value.
    pub base_url: String,
    /// Honour reverse-proxy forwarding headers when generating self-links (#12).
    pub trust_proxy_headers: bool,
    /// Collections the Tiles service encodes as vector tiles. The
    /// `tilesets-vector` link is advertised only for these, so it never names
    /// a tileset list that does not exist or holds map tiles (#789).
    pub vector_tileset_ids: HashSet<String>,
    /// Pages of the collections a feature may link to as related (#1143),
    /// as paths below the base URL, by collection id: a radar site
    /// inventory's per-site collections. A collection missing here gets no
    /// link.
    pub collection_pages: HashMap<String, String>,
}

pub type AppState = Arc<ArcSwap<FeaturesState>>;

/// Features classes this crate implements, on every surface: Part 1, and
/// Part 2 CRS by reference (`crs`, `bbox-crs`; [`crate::crs`]).
pub const CONFORMANCE: &[&str] = &[
    "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/core",
    "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/oas30",
    "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/geojson",
    "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/html",
    "http://www.opengis.net/spec/ogcapi-features-2/1.0/conf/crs",
];

/// Resolve the absolute base URL for the current request, honouring reverse-proxy
/// forwarding headers when `trust_proxy_headers` is enabled (#12).
pub(crate) fn request_base_url(state: &FeaturesState, headers: &HeaderMap) -> String {
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

/// [`lookup_collection`] for the `/items` resources. At the shared OGC
/// API root a collection can exist for Maps or Tiles and still have no
/// feature items; this service cannot tell, so its 404 says both (#811).
#[allow(clippy::type_complexity)]
fn lookup_items_collection<'a>(
    state: &'a FeaturesState,
    id: &str,
) -> Result<(&'a Arc<dyn FeatureEngine>, &'a CollectionConfig), HandlerError> {
    lookup_collection(state, id).map_err(|JsonError(status, body)| {
        if status != StatusCode::NOT_FOUND {
            return JsonError(status, body);
        }
        JsonError(
            status,
            Json(json!({
                "code": "NotFound",
                "description": format!("Collection '{id}' not found, or it has no feature items")
            })),
        )
    })
}

#[allow(clippy::type_complexity)]
fn lookup_collection<'a>(
    state: &'a FeaturesState,
    id: &str,
) -> Result<(&'a Arc<dyn FeatureEngine>, &'a CollectionConfig), HandlerError> {
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

/// The OpenAPI response of an error: the JSON body every error carries, or,
/// for a request that asked for HTML, a page with the same status (#1132).
fn error_response(description: &str) -> Value {
    json!({
        "description": description,
        "content": {
            "application/json": {"schema": {
                "type": "object",
                "required": ["code", "description"],
                "properties": {"code": {"type": "string"}, "description": {"type": "string"}}
            }},
            "text/html": {"schema": {"type": "string"}}
        }
    })
}

/// Converting through [`JsonError`] is what attaches the `ErrorReason` the
/// request log reads (#119); a `(StatusCode, Json)` tuple converts via `?`.
type HandlerError = JsonError;

/// The response to an engine's error on `/items` or `/items/{featureId}`,
/// by its [`ErrorClass`], as every API answers it (#17). A request the
/// engine turns down (`QueryTooLarge`: a station grid's analysis cap) is a
/// 400 with its message, an absent resource a 404, an engine that is not
/// ready or busy (`ResourceExhausted`,
/// as a station grid whose residuals are still being computed answers, or
/// `DeadlineExceeded`) a 503 with `Retry-After: 1`, a data source that kept
/// refusing the read (`Unavailable`, #1355) the shared `SourceUnavailable`
/// 503, and anything else a 500 that names nothing internal.
fn engine_failure(e: &ds_core::error::DataServerError, route: &str) -> Response {
    use ds_core::error::{DataServerError as E, ErrorClass};
    let (status, code, description) = match e.class() {
        ErrorClass::BadRequest => (StatusCode::BAD_REQUEST, "BadRequest", e.to_string()),
        // The status is the class's; a missing feature keeps the wording
        // `/items/{featureId}` has always answered it with.
        ErrorClass::NotFound => (
            StatusCode::NOT_FOUND,
            "NotFound",
            match e {
                E::FeatureNotFound(_) => "Feature not found".to_string(),
                _ => e.to_string(),
            },
        ),
        // `JsonError` adds the busy answer's `Retry-After: 1`.
        ErrorClass::Busy | ErrorClass::DeadlineExceeded => (
            StatusCode::SERVICE_UNAVAILABLE,
            "ServerBusy",
            "Not ready or busy, try again later".to_string(),
        ),
        ErrorClass::SourceUnavailable => {
            tracing::warn!("Features {route} error: {e}");
            return api_common::source_unavailable().into_response();
        }
        ErrorClass::Internal => {
            tracing::error!("Features {route} error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "ServerError",
                "Internal server error".to_string(),
            )
        }
    };
    JsonError(
        status,
        Json(json!({ "code": code, "description": description })),
    )
    .into_response()
}

/// A 400 from a plain message (used for `?f=` content negotiation errors).
fn bad_request_msg(msg: &str) -> HandlerError {
    JsonError(
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

/// The response CRS from `crs` (Part 2). The HTML view draws its map and
/// labels coordinates in longitude/latitude, so another CRS is a 400 there
/// rather than a page that ignores it; the GeoJSON representation takes any.
fn response_crs(
    crs: Option<&str>,
    wanted: ds_core::html::Wanted,
) -> Result<ResponseCrs, HandlerError> {
    let requested = crs
        .map(|value| FeatureCrs::parse(value, "crs"))
        .transpose()
        .map_err(|e| bad_request_msg(&e.to_string()))?;
    if wanted == ds_core::html::Wanted::Html && requested.is_some_and(|c| c != FeatureCrs::Crs84) {
        return Err(bad_request_msg(
            "the HTML representation is CRS84 only; request another crs as GeoJSON (f=json)",
        ));
    }
    Ok(ResponseCrs::new(requested))
}

/// `Content-Crs` on every items and item response, CRS84 included (Part 2).
fn with_content_crs(mut resp: Response, crs: &ResponseCrs) -> Response {
    resp.headers_mut().insert(
        header::HeaderName::from_static("content-crs"),
        HeaderValue::from_str(&crs.content_crs()).expect("a CRS URI is a valid header value"),
    );
    resp
}

pub async fn landing_page(
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    JsonQuery(fp): JsonQuery<ds_core::html::FormatParams>,
    headers: HeaderMap,
) -> Result<Response, HandlerError> {
    use ds_core::html::{LinkView, Wanted};
    let wanted = negotiate(fp.f.as_deref(), &headers)?;
    let state = state.load_full();
    let base = &request_base_url(&state, &headers);
    let root = &mount.root(base);
    let title = "MeteoCore - Features";
    let description = "Metocean Data Server — OGC API Features";
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
            api_common::rel::CONFORMANCE,
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
            api_common::rel::DATA,
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
                .map(|(h, r, t, ti)| LinkView::new(h.clone(), *r, Some(ti)).with_type(*t))
                .collect();
            // rel="alternate" to the JSON representation (parity with the
            // collection-detail HTML page).
            views.push(
                LinkView::new(
                    format!("{root}/?f=json"),
                    "alternate",
                    Some("This document as JSON"),
                )
                .with_type("application/json"),
            );
            Html(api_common::workbench::landing_html(
                api_common::workbench::Surface {
                    base,
                    root,
                    api: "features",
                },
                title,
                description,
                &views,
            ))
            .into_response()
        }
    }))
}

pub async fn conformance(
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    JsonQuery(fp): JsonQuery<ds_core::html::FormatParams>,
    headers: HeaderMap,
) -> Result<Response, HandlerError> {
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
                api_common::workbench::Surface {
                    base,
                    root,
                    api: "features",
                },
                &classes,
                &nav,
            ))
            .into_response()
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

/// Names an OpenAPI component of this crate. The per-API definition uses the
/// names as they are ([`plain_name`]); the shared root prefixes them, because
/// Maps already defines a different `bbox`, `datetime` and `link` (#789).
pub(crate) type ComponentName = fn(&str) -> String;

pub(crate) fn plain_name(name: &str) -> String {
    name.to_owned()
}

fn component_ref(section: &str, name: ComponentName, component: &str) -> Value {
    json!({"$ref": format!("#/components/{section}/{}", name(component))})
}

/// The items and single-item operations of every feature collection, keyed
/// below the mount `m`. Shared by the per-API definition and the shared root.
pub(crate) fn items_openapi_paths(
    state: &FeaturesState,
    m: &str,
    name: ComponentName,
) -> Map<String, Value> {
    let mut configs: Vec<&CollectionConfig> = state.collections.values().collect();
    configs.sort_by(|a, b| a.id.cmp(&b.id));
    let mut paths = Map::new();
    for config in configs {
        let id = &config.id;
        let mut parameters = vec![
            component_ref("parameters", name, "bbox"),
            component_ref("parameters", name, "bbox-crs"),
            component_ref("parameters", name, "crs"),
            component_ref("parameters", name, "limit"),
            component_ref("parameters", name, "offset"),
            component_ref("parameters", name, "datetime"),
            component_ref("parameters", name, "sortby"),
            feature_format_parameter(),
        ];
        // Part 1 §7.15.5–6 uses ordinary named query parameters, not CQL2.
        // The catalog is a cheap snapshot, including producer-defined names.
        if let Some(engine) = state.engines.get(id) {
            for filterable in engine
                .filterables()
                .iter()
                .filter(|n| !crate::params::is_reserved_parameter(n))
            {
                parameters.push(json!({
                    "name": filterable,
                    "in": "query",
                    "required": false,
                    "description": "Exact, case-sensitive property equality (OGC API Features Part 1 §7.15.5–6). Lists match any element; numbers and booleans use canonical string form. Numeric values also accept comma-separated alternatives (OR within that predicate, no spaces); commas in strings remain literal. Null/missing never match. All predicates, including repeated names, are ANDed before paging.",
                    "style": "form",
                    "explode": false,
                    "schema": {"type": "string"}
                }));
            }
        }
        paths.insert(
            format!("{m}/collections/{id}/items"),
            json!({
                "get": {
                    "summary": format!("Get features from {}", config.title),
                    "operationId": format!("getFeatures_{id}"),
                    "tags": [id],
                    "parameters": parameters,
                    "responses": {
                        "200": {
                            "description": "Features in GeoJSON or HTML format",
                            "content": {
                                "application/geo+json": {
                                    "schema": component_ref("schemas", name, "featureCollectionGeoJSON")
                                },
                                "text/html": {"schema": {"type": "string"}}
                            }
                        },
                        "400": error_response("Bad request. An HTML request gets the items page again, the field at fault marked."),
                        "404": error_response("Collection not found"),
                        "500": error_response("Server error"),
                        "503": error_response("Not ready or busy: retry after the Retry-After seconds")
                    }
                }
            }),
        );
        paths.insert(
            format!("{m}/collections/{id}/items/{{featureId}}"),
            json!({
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
                        component_ref("parameters", name, "crs"),
                        feature_format_parameter()
                    ],
                    "responses": {
                        "200": {
                            "description": "A single feature in GeoJSON or HTML format",
                            "content": {
                                "application/geo+json": {
                                    "schema": component_ref("schemas", name, "featureGeoJSON")
                                },
                                "text/html": {"schema": {"type": "string"}}
                            }
                        },
                        "400": error_response("Bad request"),
                        "404": error_response("Feature not found"),
                        "500": error_response("Server error"),
                        "503": error_response("Not ready or busy: retry after the Retry-After seconds")
                    }
                }
            }),
        );
    }
    paths
}

/// This crate's OpenAPI components, named by `name`.
pub(crate) fn openapi_components(name: ComponentName) -> Value {
    let parameters: Map<String, Value> = [
        (
            "bbox",
            json!({
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
            }),
        ),
        // `limit` and `datetime` follow OGC API - Features Part 1's own
        // definitions (§7.15.3, §7.15.4), including `style: form` and
        // `explode: false`, which its test suite checks.
        (
            "limit",
            json!({
                "name": "limit",
                "in": "query",
                "required": false,
                "style": "form",
                "explode": false,
                "schema": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 1000,
                    "default": 100
                }
            }),
        ),
        (
            "offset",
            json!({
                "name": "offset",
                "in": "query",
                "required": false,
                "schema": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 0
                }
            }),
        ),
        (
            "datetime",
            json!({
                "name": "datetime",
                "in": "query",
                "required": false,
                "style": "form",
                "explode": false,
                "schema": {"type": "string"},
                "description": "RFC 3339 datetime or interval (start/end, ../end, start/..)"
            }),
        ),
        // `crs` and `bbox-crs` reproduced verbatim from OGC API - Features
        // Part 2 (OGC 18-058): a URI, `style: form`, `explode: false`. The
        // valid values are each collection's `crs`.
        (
            "crs",
            json!({
                "name": "crs",
                "in": "query",
                "required": false,
                "schema": {"type": "string", "format": "uri"},
                "style": "form",
                "explode": false,
                "description": format!(
                    "CRS of the response geometry, in that CRS's axis order (EPSG:4326 is latitude, longitude; EPSG:3035 northing, easting). One of the collection's `crs` values: {}. Default CRS84. The HTML representation is CRS84 only.",
                    crate::crs::supported_uris().join(", ")
                )
            }),
        ),
        (
            "bbox-crs",
            json!({
                "name": "bbox-crs",
                "in": "query",
                "required": false,
                "schema": {"type": "string", "format": "uri"},
                "style": "form",
                "explode": false,
                "description": "CRS of `bbox`, whose corners follow that CRS's axis order. One of the collection's `crs` values. Default CRS84."
            }),
        ),
        // Schema reproduced verbatim from OGC API - Features Part 8:
        // Sorting (draft 24-030). Do not "simplify" it to a plain
        // string: `style: form` + `explode: false` is what makes the
        // comma-separated form normative rather than incidental.
        (
            "sortby",
            json!({
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
            }),
        ),
    ]
    .into_iter()
    .map(|(key, definition)| (name(key), definition))
    .collect();
    let schemas: Map<String, Value> = [
        (
            "featureCollectionGeoJSON",
            json!({
                "type": "object",
                "required": ["type", "features"],
                "properties": {
                    "type": {"type": "string", "enum": ["FeatureCollection"]},
                    "features": {"type": "array", "items": component_ref("schemas", name, "featureGeoJSON")},
                    "numberMatched": {"type": "integer"},
                    "numberReturned": {"type": "integer"},
                    "timeStamp": {"type": "string", "format": "date-time"},
                    "links": {"type": "array", "items": component_ref("schemas", name, "link")}
                }
            }),
        ),
        (
            "featureGeoJSON",
            json!({
                "type": "object",
                "required": ["type", "geometry", "properties"],
                "properties": {
                    "type": {"type": "string", "enum": ["Feature"]},
                    "id": {"oneOf": [{"type": "string"}, {"type": "number"}]},
                    "geometry": {"nullable": true},
                    "properties": {"type": "object", "nullable": true},
                    "links": {"type": "array", "items": component_ref("schemas", name, "link")}
                }
            }),
        ),
        (
            "link",
            json!({
                "type": "object",
                "required": ["href"],
                "properties": {
                    "href": {"type": "string"},
                    "rel": {"type": "string"},
                    "type": {"type": "string"},
                    "title": {"type": "string"}
                }
            }),
        ),
    ]
    .into_iter()
    .map(|(key, definition)| (name(key), definition))
    .collect();
    json!({"parameters": parameters, "schemas": schemas})
}

pub async fn api_definition(
    State(state): State<AppState>,
    Extension(Mount(m)): Extension<Mount>,
) -> impl IntoResponse {
    let state = state.load_full();
    let mut paths = Map::new();
    for (path, summary, operation_id) in [
        (format!("{m}/"), "Landing page", "getLandingPage"),
        (
            format!("{m}/conformance"),
            "Conformance classes",
            "getConformance",
        ),
    ] {
        paths.insert(
            path,
            json!({"get": {"summary": summary, "operationId": operation_id,
                "tags": [api_common::openapi_tags::DISCOVERY],
                "parameters": [format_parameter()],
                "responses": {"200": {"description": summary},
                              "400": api_common::format_error_response()}}}),
        );
    }
    paths.insert(
        format!("{m}/collections"),
        json!({"get": api_common::collection_operation()}),
    );
    let mut configs: Vec<&CollectionConfig> = state.collections.values().collect();
    configs.sort_by(|a, b| a.id.cmp(&b.id));
    for config in configs {
        let id = &config.id;
        // Collection detail. OGC API – Common – Part 2 `conf/json` requires the
        // API definition to describe every collection resource, including
        // GET /collections/{id} — Maps and Tiles already do; Features was the
        // odd one out (review on #298).
        paths.insert(
            format!("{m}/collections/{id}"),
            json!({
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
                        "400": api_common::format_error_response(),
                        "404": {"description": "Collection not found"}
                    }
                }
            }),
        );
    }
    paths.extend(items_openapi_paths(&state, m, plain_name));

    Json(json!({
        "openapi": "3.0.3",
        "info": {
            "title": "MeteoCore - OGC API Features",
            "version": "1.0.0",
            "description": "OGC API - Features implementation"
        },
        "paths": paths,
        "components": openapi_components(plain_name)
    }))
}

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
                    "collection has no registered feature engine; omitting from /collections"
                );
                return None;
            };
            Some(api_common::CollectionEntry {
                config,
                metadata: build_collection_metadata(
                    engine.as_ref(),
                    config,
                    &state.discovery,
                    state.vector_tileset_ids.contains(&config.id),
                    base,
                    root,
                ),
                bbox: engine.spatial_extent(),
                time: engine.temporal_extent(),
            })
        })
        .collect();
    api_common::collections_response(
        api_common::workbench::Surface {
            base,
            root,
            api: "features",
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
    JsonQuery(fp): JsonQuery<ds_core::html::FormatParams>,
    headers: HeaderMap,
) -> Result<Response, HandlerError> {
    use ds_core::html::Wanted;
    let wanted = negotiate(fp.f.as_deref(), &headers)?;
    let state = state.load_full();
    let (engine, config) = lookup_collection(&state, &id)?;
    let base = &request_base_url(&state, &headers);
    let root = &mount.root(base);
    let metadata = build_collection_metadata(
        engine.as_ref(),
        config,
        &state.discovery,
        state.vector_tileset_ids.contains(&id),
        base,
        root,
    );
    Ok(with_vary(match wanted {
        Wanted::Json => Json(metadata).into_response(),
        Wanted::Html => Html(api_common::workbench::collection_page(
            api_common::workbench::Surface {
                base,
                root,
                api: "features",
            },
            &metadata,
            config.license.as_ref(),
            Some(&items_overview(engine.as_ref())),
            &api_common::workbench::Services::default(),
        ))
        .into_response(),
    }))
}

/// The properties `/items` takes as filters: the engine's, less any name
/// an API control takes.
fn filterable_names(engine: &dyn FeatureEngine) -> Vec<String> {
    engine
        .filterables()
        .iter()
        .filter(|name| !crate::params::is_reserved_parameter(name))
        .cloned()
        .collect()
}

fn owned(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn html_controls(engine: &dyn FeatureEngine) -> crate::html::FeatureControls {
    crate::html::FeatureControls {
        filterables: filterable_names(engine),
        sortables: owned(engine.sortables()),
        summary: owned(engine.summary_properties()),
        // The control `/items` accepts `datetime` with (#1143), not whether
        // the collection advertises a span.
        temporal: engine.has_time_dimension(),
    }
}

/// What a collection's HTML overview says of its items (#1143).
pub(crate) fn items_overview(engine: &dyn FeatureEngine) -> api_common::workbench::ItemsOverview {
    api_common::workbench::ItemsOverview {
        description: engine.items_description().map(str::to_owned),
        filterables: filterable_names(engine),
        sortables: owned(engine.sortables()),
    }
}

/// What the features of collection `config` carry beyond the engine's
/// properties (#1143): their labels and related links.
fn annotations<'a>(
    state: &'a FeaturesState,
    engine: &'a dyn FeatureEngine,
    config: &CollectionConfig,
    base: &'a str,
) -> Annotations<'a> {
    Annotations {
        engine: Some(engine),
        base,
        collection_pages: Some(&state.collection_pages),
        edr: config.apis.iter().any(|api| api == "edr"),
    }
}

pub async fn items(
    Path(id): Path<String>,
    Query(pairs): Query<Vec<(String, String)>>,
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    headers: HeaderMap,
) -> Result<Response, HandlerError> {
    let state = state.load_full();
    let base = &request_base_url(&state, &headers);
    let root = &mount.root(base);
    let (engine, config) = lookup_items_collection(&state, &id)?;
    // The representation is negotiated before any other parameter is parsed,
    // from the raw `f` and `Accept`, so a request for the HTML page whose
    // filters fail gets the page back with the field marked (#1132).
    let f = pairs
        .iter()
        .find(|(k, _)| k == "f")
        .map(|(_, v)| v.as_str());
    let wanted = match negotiate_feature(f, &headers) {
        Ok(wanted) => wanted,
        // A bad `f` is no HTML request: its 400 names the first fault in
        // the order it always has, the parameters' shape before the format.
        Err(error) => {
            return Err(match ItemsQueryParams::from_pairs(pairs) {
                Err(e) => bad_request_msg(&e.error.to_string()),
                Ok(_) => error,
            })
        }
    };
    let ItemsRequest {
        query,
        crs,
        limit,
        offset,
        cache_control,
    } = match items_request(engine.as_ref(), &id, pairs.clone(), wanted) {
        Ok(request) => request,
        Err(rejected)
            if wanted == ds_core::html::Wanted::Html
                && rejected.error.0 == StatusCode::BAD_REQUEST =>
        {
            return Ok(rejected_items(
                engine.as_ref(),
                &config.title,
                &id,
                &pairs,
                base,
                root,
                rejected,
            ));
        }
        Err(rejected) => return Err(rejected.error),
    };

    let page = match engine.get_features(&query) {
        Ok(page) => page,
        Err(e) => return Ok(engine_failure(&e, "items")),
    };

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
        root,
        &crs,
        &annotations(&state, engine.as_ref(), config, base),
    );
    crate::html::representation_links(&mut doc, wanted);
    let time_stamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    // Hash the selected representation with the volatile timestamp blanked.
    // The HTML page is rendered once: the hash covers that render, and the
    // generation time is written into it afterwards (#1135).
    let (etag, mut resp) = match wanted {
        ds_core::html::Wanted::Json => {
            let blank = serde_json::to_string(&doc).expect("GeoJSON Value serializes");
            let etag = ds_core::http_cache::etag_of(blank.as_bytes());
            doc["timeStamp"] = json!(time_stamp);
            (etag, GeoJsonResponse(doc).into_response())
        }
        ds_core::html::Wanted::Html => {
            let mut page = crate::html::features_html(
                &doc,
                &config.title,
                &id,
                base,
                root,
                &html_controls(engine.as_ref()),
            );
            let etag = ds_core::http_cache::etag_of(page.as_bytes());
            crate::html::stamp_generation_time(&mut page, &time_stamp);
            (etag, Html(page).into_response())
        }
    };
    resp.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&etag).expect("quoted-hex etag is a valid header value"),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    Ok(with_vary(with_content_crs(resp, &crs)))
}

/// A validated `/items` request.
struct ItemsRequest {
    query: FeatureQuery,
    crs: ResponseCrs,
    limit: usize,
    offset: usize,
    cache_control: &'static str,
}

/// An `/items` request rejected before the engine ran: the parameter at
/// fault, when one is, and the error response.
struct Rejected {
    parameter: Option<String>,
    error: HandlerError,
}

impl Rejected {
    fn at(parameter: &str, error: HandlerError) -> Self {
        Self {
            parameter: Some(parameter.to_owned()),
            error,
        }
    }
}

/// Validate the `/items` query `pairs` of collection `id` for the
/// representation `wanted`. Every 400 names the parameter it is about.
fn items_request(
    engine: &dyn FeatureEngine,
    id: &str,
    pairs: Vec<(String, String)>,
    wanted: ds_core::html::Wanted,
) -> Result<ItemsRequest, Rejected> {
    let bad_request = |e: ds_core::error::DataServerError| bad_request_msg(&e.to_string());
    let named = |e: crate::params::ParameterError| Rejected::at(&e.parameter, bad_request(e.error));
    let params = ItemsQueryParams::from_pairs(pairs).map_err(named)?;
    params
        .validate_filters(&engine.filterables())
        .map_err(named)?;
    let crs = response_crs(params.crs.as_deref(), wanted).map_err(|e| Rejected::at("crs", e))?;

    // `bbox-crs` is validated even without a `bbox`; the engine always gets a
    // CRS84 box, which the pagination links then carry.
    let bbox_crs = params
        .bbox_crs
        .as_deref()
        .map(|value| FeatureCrs::parse(value, "bbox-crs"))
        .transpose()
        .map_err(|e| Rejected::at("bbox-crs", bad_request(e)))?
        .unwrap_or(FeatureCrs::Crs84);
    let bbox = params
        .bbox
        .as_deref()
        .map(|value| bbox_crs.bbox_to_crs84(parse_bbox_values(value)?))
        .transpose()
        .map_err(|e| Rejected::at("bbox", bad_request(e)))?;

    let datetime = params
        .datetime
        .as_deref()
        .map(parse_datetime)
        .transpose()
        .map_err(|e| Rejected::at("datetime", bad_request_msg(&e.to_string())))?;
    // A collection whose features carry no time cannot filter by it: a 400,
    // not the full set with 200 (#682).
    if datetime.is_some() && !engine.has_time_dimension() {
        return Err(Rejected::at(
            "datetime",
            bad_request_msg(&format!(
                "Collection '{id}' has no time dimension; datetime is not supported"
            )),
        ));
    }

    // Validated against what this engine can actually sort on, so an unknown
    // property is a 400 naming the valid ones rather than a parameter that
    // quietly does nothing (OGC API - Features Part 8, draft 24-030).
    let sortby = params
        .sortby
        .as_deref()
        .map(|s| parse_sortby(s, engine.sortables()))
        .transpose()
        .map_err(|e| Rejected::at("sortby", bad_request(e)))?
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

    Ok(ItemsRequest {
        query: FeatureQuery {
            bbox,
            limit,
            offset,
            datetime,
            sortby,
            property_filters: params.property_filters,
        },
        crs,
        limit,
        offset,
        cache_control,
    })
}

/// The 400 of an HTML items request whose parameters were rejected (#1132):
/// the items page with the data request form holding what the request gave,
/// the field at fault marked with the message, and no results.
fn rejected_items(
    engine: &dyn FeatureEngine,
    title: &str,
    id: &str,
    pairs: &[(String, String)],
    base: &str,
    root: &str,
    rejected: Rejected,
) -> Response {
    let JsonError(status, Json(body)) = rejected.error;
    let message = body["description"].as_str().unwrap_or("Invalid request");
    let mut query = form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs.iter().filter(|(key, _)| key != "f") {
        query.append_pair(key, value);
    }
    let query = query.finish();
    let items = format!("{root}/collections/{}/items", crate::html::path_segment(id));
    let href = if query.is_empty() {
        items
    } else {
        format!("{items}?{query}")
    };
    let mut doc = json!({
        "type": "FeatureCollection",
        "features": [],
        "links": [{"href": href, "rel": "self"}],
    });
    crate::html::representation_links(&mut doc, ds_core::html::Wanted::Html);
    let html = crate::html::rejected_items_html(
        &doc,
        title,
        id,
        base,
        root,
        &html_controls(engine),
        crate::html::Invalid {
            parameter: rejected.parameter.as_deref(),
            message,
        },
    );
    let mut response = (status, Html(html)).into_response();
    response
        .extensions_mut()
        .insert(ds_core::error::ErrorReason(format!(
            "{}: {message}",
            body["code"].as_str().unwrap_or("BadRequest")
        )));
    with_vary(response)
}

pub async fn item(
    Path((id, feature_id)): Path<(String, String)>,
    Query(pairs): Query<Vec<(String, String)>>,
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, HandlerError> {
    let state = state.load_full();
    let (engine, config) = lookup_items_collection(&state, &id)?;

    // Negotiated first, from the raw `f` and `Accept` (#1132). A bad `f`
    // still reports a bad parameter first, as it always has.
    let f = pairs
        .iter()
        .find(|(k, _)| k == "f")
        .map(|(_, v)| v.as_str());
    let wanted = match negotiate_feature(f, &headers) {
        Ok(wanted) => wanted,
        Err(error) => {
            return Err(match ItemQueryParams::from_pairs(pairs) {
                Err(e) => bad_request_msg(&e.error.to_string()),
                Ok(_) => error,
            })
        }
    };
    // A single feature takes only `f` and `crs`. Anything else
    // (`properties`, a filter, a typo) would be ignored with a 200, so it is
    // a 400 naming it (#681; root CLAUDE.md: never silently ignore a
    // parameter). Its HTML error page names the parameter too.
    let params = match ItemQueryParams::from_pairs(pairs) {
        Ok(params) => params,
        Err(e) => {
            let detail = api_common::errors::ErrorDetail {
                parameter: Some(e.parameter),
                ..Default::default()
            };
            return Ok(detail.attach(bad_request_msg(&e.error.to_string())));
        }
    };
    let crs = response_crs(params.crs.as_deref(), wanted)?;
    let feature = match engine.get_feature(&feature_id) {
        Ok(feature) => feature,
        Err(e) => return Ok(engine_failure(&e, "item")),
    };

    let base = &request_base_url(&state, &headers);
    let root = &mount.root(base);
    let mut doc = feature_to_geojson(
        &feature,
        &id,
        root,
        &crs,
        &annotations(&state, engine.as_ref(), config, base),
    );
    crate::html::representation_links(&mut doc, wanted);
    let resp = match wanted {
        ds_core::html::Wanted::Json => GeoJsonResponse(doc).into_response(),
        ds_core::html::Wanted::Html => Html(crate::html::features_html(
            &doc,
            &config.title,
            &id,
            base,
            root,
            &html_controls(engine.as_ref()),
        ))
        .into_response(),
    };
    Ok(with_vary(with_content_crs(resp, &crs)))
}

/// Which surface a collection description is for: the per-API `/features`
/// service keeps its historical shape; the shared OGC API root adds what a
/// collection with several access mechanisms needs (#789).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layout {
    PerApi,
    Shared,
}

/// Features' description of a collection: its standard fields and `items`
/// links, without `self`. Shared by the per-API service and the shared root.
pub(crate) fn collection_parts(
    engine: &dyn FeatureEngine,
    config: &CollectionConfig,
    root: &str,
    layout: Layout,
) -> (Map<String, Value>, Vec<Value>) {
    let items = format!("{root}/collections/{}/items", config.id);
    let mut links = vec![json!({
        "href": items,
        "rel": "items",
        "type": "application/geo+json",
        "title": "Items"
    })];
    let mut fields = Map::new();
    fields.insert("itemType".into(), json!("feature"));
    if layout == Layout::Shared {
        // One `items` link per encoding (Common Part 2 §6.2); the Features
        // test suite picks the GeoJSON one by type.
        links.push(json!({
            "href": format!("{items}?f=html"),
            "rel": "items",
            "type": "text/html",
            "title": "Items as HTML"
        }));
        fields.insert("dataType".into(), json!("vector"));
    }
    // Part 2: exactly the CRSs `/items` accepts in `crs` and `bbox-crs`.
    fields.insert("crs".into(), json!(crate::crs::supported_uris()));
    fields.insert("storageCrs".into(), json!(FeatureCrs::Crs84.uri()));
    fields.insert("numberItems".into(), json!(engine.feature_count()));

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
        ds_core::ogc_extent::build_extent(engine.spatial_extent(), None, "", None, &times, None)
    {
        // Extent endpoints describe bounds, not two regularly sampled observations.
        if let Some(temporal) = &mut extent.temporal {
            temporal.grid = None;
        }
        fields.insert(
            "extent".into(),
            serde_json::to_value(extent).expect("Extent serializes to JSON"),
        );
    }
    (fields, links)
}

fn build_collection_metadata(
    engine: &dyn FeatureEngine,
    config: &CollectionConfig,
    discovery: &Discovery,
    vector_tilesets: bool,
    base_url: &str,
    root: &str,
) -> Value {
    let (fields, parts) = collection_parts(engine, config, root, Layout::PerApi);
    let mut links = vec![json!({
        "href": format!("{root}/collections/{}", config.id),
        "rel": "self",
        "type": "application/json",
        "title": config.title
    })];
    links.extend(parts);

    // If the Tiles service also encodes this collection as vector tiles,
    // advertise the tilesets list so clients can discover that representation
    // without probing. Per OGC API – Tiles 1.0 §7.1, the `tilesets-vector`
    // relation targets the tilesets list resource (`application/json`), not a
    // tile URL template — the per-tile URL template lives one level deeper
    // inside the tilesets-list response as `rel: item`. Linking to the list
    // also avoids hardcoding `WebMercatorQuad`; the list enumerates every
    // supported TileMatrixSet.
    if vector_tilesets {
        links.push(json!({
            "href": format!(
                "{base_url}{}/collections/{}/tiles",
                api_common::mounts::TILES,
                config.id
            ),
            "rel": api_common::rel::TILESETS_VECTOR,
            "type": "application/json",
            "title": "Vector tilesets"
        }));
    }

    api_common::collection_metadata(config, discovery, Value::Object(fields), links)
}
