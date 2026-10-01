use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use serde_json::json;

use api_common::JsonError;
use ds_core::config::CollectionConfig;
use ds_core::datetime::parse_datetime_interval;
use ds_core::edr_engine::{EdrEngine, TrajectoryShape};

use ds_core::error::DataServerError;
use ds_core::feature::MAX_AREA_VALUES;
use ds_core::model::CoverageResponse;
use ds_core::trajectory::{TrajectoryPath, MAX_TRAJECTORY_NODES, MAX_TRAJECTORY_SAMPLES};
use ds_render::{render_chart, render_heatmap};

use crate::params::{
    parse_edr_format, parse_limit, parse_locations_paging, parse_within_metres, parse_z,
    plot_dimensions, resolve_z_levels, split_position_coords, AreaQueryParams, EdrFormat,
    LocationQueryParams, PositionQueryParams, RadiusQueryParams, TrajectoryQueryParams, CRS84_WKT,
    DATA_QUERY_CRS, MAX_LIMIT, WITHIN_UNITS,
};
use crate::plot_convert::{coverage_response_to_panels, section_response_to_heatmaps};
use crate::response::{
    collection_parameter_json, coverage_response_to_json, locations_to_writer, LocationsContext,
    LocationsPage, COVERAGE_JSON_MEDIA_TYPE,
};

/// Converting through [`JsonError`] is what attaches the `ErrorReason` the
/// request log reads (#119); a `(StatusCode, Json)` tuple converts via `?`.
pub(crate) type HandlerError = JsonError;

/// The executor owns admission and keeps running work accounted for after a
/// client timeout. No engine-specific execution decisions belong in handlers.
pub(crate) async fn execute_query<T: Send + 'static>(
    blocking: bool,
    work: impl FnOnce(crate::executor::QueryBudget) -> Result<T, HandlerError> + Send + 'static,
) -> Result<T, HandlerError> {
    crate::executor::run(blocking, work).await.map_err(|e| match e {
        crate::executor::ExecutionError::Busy => JsonError(
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"code": "ServerBusy", "description": "EDR query capacity exhausted; retry later"})),
        ),
        crate::executor::ExecutionError::Timeout => query_timeout(),
        crate::executor::ExecutionError::Task(e) => {
            tracing::error!("EDR query task failed: {e}");
            server_error()
        }
    })?
}

fn query_timeout() -> HandlerError {
    JsonError(
        StatusCode::GATEWAY_TIMEOUT,
        Json(json!({"code": "Timeout", "description": "EDR query exceeded its time budget"})),
    )
}

/// Serialise a data-query result as a CoverageJSON response, typed
/// [`COVERAGE_JSON_MEDIA_TYPE`] (EDR 1.2, #920). Every data query's
/// CoverageJSON goes through here, so the media type cannot drift per
/// query type. `label` names the query in the (server-side only) log line.
fn coverage_json_response(
    result: &CoverageResponse,
    label: &str,
) -> Result<Response, HandlerError> {
    let body = serde_json::to_string(&coverage_response_to_json(result)).map_err(|e| {
        tracing::error!("{label} CoverageJSON serialise error: {e}");
        server_error()
    })?;
    Ok(([(header::CONTENT_TYPE, COVERAGE_JSON_MEDIA_TYPE)], body).into_response())
}

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
        EdrFormat::CoverageJson => coverage_json_response(&result, "EDR"),
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

/// Map an engine error from a data query to its HTTP response: request
/// errors → 400, absent resources → 404, everything else a generic 500
/// (logged under `label`). One home for the four data-query handlers so a
/// new `DataServerError` variant cannot map differently per query type.
pub(crate) fn map_query_error(e: &DataServerError, label: &str) -> HandlerError {
    match e {
        DataServerError::DeadlineExceeded => query_timeout(),
        DataServerError::ResourceExhausted => JsonError(
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"code": "ServerBusy", "description": "Server busy, try again later"})),
        ),
        DataServerError::InvalidParameter(_)
        | DataServerError::InvalidBbox(_)
        | DataServerError::InvalidDatetime(_)
        | DataServerError::QueryTooLarge(_) => JsonError(
            StatusCode::BAD_REQUEST,
            Json(json!({ "code": "BadRequest", "description": e.to_string() })),
        ),
        DataServerError::LocationNotFound(_)
        | DataServerError::CollectionNotFound(_)
        | DataServerError::FeatureNotFound(_)
        | DataServerError::ReferenceTimeNotFound(_) => JsonError(
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "NotFound", "description": e.to_string() })),
        ),
        _ => {
            tracing::error!("{label} query error: {e}");
            JsonError(
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "code": "ServerError", "description": "Internal server error" })),
            )
        }
    }
}

pub(crate) fn bad_request(e: &DataServerError) -> HandlerError {
    JsonError(
        StatusCode::BAD_REQUEST,
        Json(json!({ "code": "BadRequest", "description": e.to_string() })),
    )
}

/// The request's EDR 1.2 `limit` (`None` = no limit), or its 400.
fn request_limit(raw: Option<&str>) -> Result<Option<usize>, HandlerError> {
    parse_limit(raw).map_err(|e| bad_request(&e))
}

/// Apply `limit` to a data query's result (`/req/edr/REQ_rc-limit-response`):
/// at most `limit` top-level coverages of a CoverageCollection, in the
/// engine's order. A single Coverage is one top-level object and passes
/// through. CoverageJSON has no paging links, so the rest are dropped.
fn limit_coverages(result: CoverageResponse, limit: Option<usize>) -> CoverageResponse {
    match (result, limit) {
        (CoverageResponse::Collection(mut coverages), Some(limit)) => {
            coverages.truncate(limit);
            CoverageResponse::Collection(coverages)
        }
        (result, _) => result,
    }
}

pub(crate) fn server_error() -> HandlerError {
    JsonError(
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "code": "ServerError", "description": "Internal server error" })),
    )
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

/// Tag a content-negotiated response with `Vary: Accept` so shared caches
/// don't serve the JSON body to a client that asked for HTML (or vice versa).
/// Uses `append` (not `insert`) so it never clobbers a `Vary` an upstream layer
/// may have set (e.g. compression's `Vary: Accept-Encoding`).
fn with_vary(mut resp: Response) -> Response {
    resp.headers_mut()
        .append(header::VARY, axum::http::HeaderValue::from_static("accept"));
    resp
}

/// Attach the data-query `Cache-Control` policy (#499) to a success response:
/// a settled window (closed `datetime` interval entirely in the past) gets the
/// long policy, everything else the short one. `datetime` is the parsed
/// request interval; the `parse_datetime_interval` open-bound sentinels
/// (`MIN_UTC`/`MAX_UTC`) map back to "open" for
/// [`ds_core::http_cache::data_cache_control`]. Metadata endpoints don't call
/// this — they fall through to the middleware's short default
/// (`caching::conditional_get`).
fn with_data_cache_control(
    mut resp: Response,
    datetime: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
) -> Response {
    let (start, end) = match datetime {
        Some((s, e)) => (
            (s != chrono::DateTime::<chrono::Utc>::MIN_UTC).then_some(s),
            (e != chrono::DateTime::<chrono::Utc>::MAX_UTC).then_some(e),
        ),
        None => (None, None),
    };
    let cc = ds_core::http_cache::data_cache_control(start, end, chrono::Utc::now());
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static(cc),
    );
    resp
}

/// Shared state for the EDR API: a registry of collection engines + metadata.
#[derive(Clone)]
pub struct EdrState {
    pub engines: HashMap<String, Arc<dyn EdrEngine>>,
    /// The `FeatureEngine` of each EDR collection whose engine also
    /// implements one: the `items` query delegates to it (#928), and only
    /// these collections have `/items` or advertise it. Keyed like
    /// `engines`; the server fills it for collections that list `edr`
    /// whether or not they also list `features`.
    pub feature_engines: HashMap<String, Arc<dyn ds_core::feature_engine::FeatureEngine>>,
    pub collections: HashMap<String, CollectionConfig>,
    /// Resolved style maps per collection (same `StyleInfo` instances the
    /// WMS/Maps/Tiles registries hold — resolved once by the server through
    /// `ds_render::StyleContext`). The `f=png` cross-section plot uses the
    /// collection's `default` style; raw `[wms]` config is never re-resolved
    /// here.
    pub styles: HashMap<String, HashMap<String, ds_render::StyleInfo>>,
    /// Static fallback base URL for absolute links (e.g. "https://api.example.com").
    /// Used as-is unless `trust_proxy_headers` resolves a per-request value.
    pub base_url: String,
    /// Honour reverse-proxy forwarding headers when generating self-links (#12).
    pub trust_proxy_headers: bool,
}

pub type AppState = Arc<ArcSwap<EdrState>>;

/// Resolve the absolute base URL for the current request, honouring reverse-proxy
/// forwarding headers when `trust_proxy_headers` is enabled (#12).
pub(crate) fn request_base_url(state: &EdrState, headers: &HeaderMap) -> String {
    ds_core::proxy::resolve_base_url(&state.base_url, state.trust_proxy_headers, |name| {
        headers.get(name).and_then(|v| v.to_str().ok())
    })
}

#[allow(clippy::type_complexity)]
pub(crate) fn lookup_collection<'a>(
    state: &'a EdrState,
    id: &str,
) -> Result<(&'a Arc<dyn EdrEngine>, &'a CollectionConfig), HandlerError> {
    let engine = state.engines.get(id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "NotFound", "description": format!("Collection '{id}' not found") })),
        )
    })?;
    // engines and collections are built from the same config in admin.rs, but
    // a registration divergence must surface as a 500, not a request panic.
    let config = state.collections.get(id).ok_or_else(|| {
        tracing::error!(
            collection = id,
            "engine registered without a matching collection config"
        );
        server_error()
    })?;
    Ok((engine, config))
}

/// Resolve an optional `{instanceId}` path segment to a forecast model run.
///
/// `None` (the no-instance routes) ⇒ `Ok(None)` (the engine serves its latest
/// run). `Some(id)` ⇒ a run query, which requires the collection to actually
/// expose model runs: a collection whose engine has no instances returns **404**
/// (otherwise a non-forecast engine — which ignores `reference_time` — would
/// wrongly answer 200 with its latest data for any instance path). The id is
/// then parsed to a reference time ([`instances::parse_instance_id`]); an
/// unparseable id is 400.
///
/// The collection-level check is the **O(1)** [`EdrEngine::has_instances`], not
/// a `get_instances()` clone (CLAUDE.md #211). The *specific*-run existence is
/// left to the engine query, which returns
/// [`DataServerError::ReferenceTimeNotFound`] (→ 404) for an absent run — no
/// validate-then-query race.
fn resolve_instance(
    engine: &Arc<dyn EdrEngine>,
    instance_id: Option<&str>,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, HandlerError> {
    let Some(iid) = instance_id else {
        return Ok(None);
    };
    if !engine.has_instances() {
        return Err(JsonError(
            StatusCode::NOT_FOUND,
            Json(json!({
                "code": "NotFound",
                "description": "This collection has no model-run instances"
            })),
        ));
    }
    let rt = ds_core::instances::parse_instance_id(iid).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "code": "BadRequest",
                "description": format!("Invalid instance id '{iid}' (expected a reference time like 20260607T0600Z)")
            })),
        )
    })?;
    Ok(Some(rt))
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
) -> Result<Option<Vec<f64>>, HandlerError> {
    let Some(sel) = parse_z(z).map_err(|e| bad_request(&e))? else {
        return Ok(None);
    };
    let extent = engine.get_vertical_extent();
    if extent.is_none() {
        return Err(JsonError(
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
    let base = &request_base_url(&state, &headers);
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
            format!("{base}/edr/conformance"),
            api_common::rel::CONFORMANCE,
            "application/json",
            "Conformance classes",
        ),
        (
            format!("{base}/edr/collections"),
            "data",
            "application/json",
            "Collections",
        ),
        (
            format!("{base}/edr/collections"),
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
            Html(api_common::workbench::landing_html(
                api_common::workbench::Surface {
                    base,
                    root: &format!("{base}{}", api_common::mounts::EDR),
                    api: "edr",
                },
                title,
                description,
                &views,
            ))
            .into_response()
        }
    }))
}

/// OpenAPI `f` (output-format) query parameter, shared by the content-negotiated
/// metadata endpoints (landing, conformance, collections, collection detail).
fn format_parameter() -> serde_json::Value {
    json!({"name": "f", "in": "query", "required": false, "schema": {"type": "string", "enum": ["json", "html"]},
           "description": "Output format. 'json' (default) or 'html'; overrides the Accept header."})
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
        // Every data query is gated the same way, and its handler answers
        // 404 when the engine does not support it (#668).
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
                "parameters": [
                    {"$ref": "#/components/parameters/limit-locations"},
                    {"$ref": "#/components/parameters/offset-locations"}
                ],
                "responses": {
                    "200": {
                        "description": "Locations in GeoJSON format: the complete inventory, or with limit one page of it carrying numberMatched, numberReturned and self/next/prev links",
                        "content": {
                            "application/geo+json": {
                                "schema": {"type": "object"}
                            }
                        }
                    },
                    "400": {"description": "Bad request"},
                    "404": {"description": "Collection not found"},
                    "503": {"description": "Response budget exhausted; a complete inventory over the response limit can be paged with limit"}
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
                    },
                    {"$ref": "#/components/parameters/limit"}
                ],
                "responses": {
                    "200": {
                        "description": "Coverage data",
                        "content": {
                            COVERAGE_JSON_MEDIA_TYPE: {
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
        if supported.contains("position") {
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
                        },
                        {"$ref": "#/components/parameters/limit"}
                    ],
                    "responses": {
                        "200": {
                            "description": "Coverage data",
                            "content": {
                                COVERAGE_JSON_MEDIA_TYPE: {
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

        // Area query
        if supported.contains("area") {
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
                        {"$ref": "#/components/parameters/z"},
                        {"$ref": "#/components/parameters/limit"}
                    ],
                    "responses": {
                        "200": {
                            "description": "Coverage data",
                            "content": {
                                COVERAGE_JSON_MEDIA_TYPE: {
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

        // Radius query. Gated like trajectory: only engines advertising
        // `radius` get the path, matching `data_queries` and the handler's
        // 404 capability guard.
        if supported.contains("radius") {
            let radius_path = format!("/edr/collections/{id}/radius");
            collection_paths[&radius_path] = json!({
                "get": {
                    "summary": format!("Radius query for {}", config.title),
                    "operationId": format!("getRadius_{id}"),
                    "tags": [id],
                    "parameters": [
                        {"$ref": "#/components/parameters/coords-radius"},
                        {"$ref": "#/components/parameters/within"},
                        {"$ref": "#/components/parameters/within-units"},
                        {"$ref": "#/components/parameters/datetime"},
                        {"$ref": "#/components/parameters/parameter-name"},
                        {"$ref": "#/components/parameters/z"},
                        {"$ref": "#/components/parameters/limit"}
                    ],
                    "responses": {
                        "200": {
                            "description": "Coverage data",
                            "content": {
                                COVERAGE_JSON_MEDIA_TYPE: {
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

        // Trajectory query. Only advertised for engines that report
        // `trajectory` in `supported_query_types` — keeps the OpenAPI spec
        // consistent with `data_queries` in the collection metadata. A
        // client that calls the path on a non-trajectory engine gets a 404
        // from the handler's capability guard (the resource doesn't exist
        // for that collection). The engine's shape picks the variant:
        // along-path sampling (gridded) or a radar cross-section.
        if supported.contains("trajectory") {
            let trajectory_path = format!("/edr/collections/{id}/trajectory");
            let shape = state
                .engines
                .get(id)
                .map_or(TrajectoryShape::AlongPath, |e| e.trajectory_shape());
            collection_paths[&trajectory_path] = match shape {
                TrajectoryShape::AlongPath => json!({
                    "get": {
                        "summary": format!("Trajectory query for {}", config.title),
                        "description": "Values sampled along the path at about the source grid spacing (one sample per grid cell crossed, the path's vertices kept), interpolated the way a position query is. Segments follow the short great circle. A 2-D or Z path returns one CoverageJSON Trajectory coverage per timestep that `datetime` selects; a 2-D or M path one per level that `z` selects (all levels when omitted on a collection with a vertical extent). Samples at the same place and step appear once.",
                        "operationId": format!("getTrajectory_{id}"),
                        "tags": [id],
                        "parameters": [
                            {"$ref": "#/components/parameters/coords-trajectory"},
                            {"$ref": "#/components/parameters/datetime"},
                            {"$ref": "#/components/parameters/parameter-name"},
                            {"$ref": "#/components/parameters/z"}
                        ],
                        "responses": {
                            "200": {
                                "description": "Coverage data: a CoverageJSON Trajectory coverage, or a CoverageCollection of them",
                                "content": {
                                    COVERAGE_JSON_MEDIA_TYPE: {
                                        "schema": {"$ref": "#/components/schemas/coverageJSON"}
                                    }
                                }
                            },
                            "400": {"description": "Bad request"},
                            "404": {"description": "Not found"},
                            "500": {"description": "Server error"}
                        }
                    }
                }),
                TrajectoryShape::CrossSection => json!({
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
                                "description": "Coverage data — CoverageJSON Section domain or PNG heatmap. The Section domain carries the per-node lowest-beam coverage floor (metres above antenna) in the `meteocore:beamCoverage` foreign member; the PNG draws it as a hatched-below overlay line. Below the floor the volume is unobserved, not echo-free.",
                                "content": {
                                    COVERAGE_JSON_MEDIA_TYPE: {
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
                }),
            };
        }

        // Items (#928): only collections whose engine also serves features,
        // matching `data_queries` and the handler's 404.
        if state.feature_engines.contains_key(id) {
            for (path, item) in crate::items::openapi_paths(id, &config.title) {
                collection_paths[&path] = item;
            }
        }

        // Instances (forecast model runs; #337). Only advertised for engines
        // that expose runs, so the OpenAPI spec matches the `instances`
        // data_query in the collection metadata.
        let has_instances = state
            .engines
            .get(id)
            .map(|e| e.has_instances())
            .unwrap_or(false);
        if has_instances {
            let instance_id_param = json!({
                "name": "instanceId",
                "in": "path",
                "required": true,
                "description": "Forecast model run (reference time), e.g. 20260607T0600Z.",
                "schema": {"type": "string"}
            });
            let instances_path = format!("/edr/collections/{id}/instances");
            collection_paths[&instances_path] = json!({
                "get": {
                    "summary": format!("List model runs (instances) for {}", config.title),
                    "operationId": format!("getInstances_{id}"),
                    "tags": [id],
                    "responses": {
                        "200": {"description": "Available instances (model runs)"},
                        "404": {"description": "Collection not found"}
                    }
                }
            });
            let instance_path = format!("/edr/collections/{id}/instances/{{instanceId}}");
            collection_paths[&instance_path] = json!({
                "get": {
                    "summary": format!("Get one model run's metadata for {}", config.title),
                    "operationId": format!("getInstance_{id}"),
                    "tags": [id],
                    "parameters": [instance_id_param.clone()],
                    "responses": {
                        "200": {"description": "Instance (model run) metadata"},
                        "400": {"description": "Bad request"},
                        "404": {"description": "Instance not found"}
                    }
                }
            });
            if supported.contains("position") {
                let p = format!("/edr/collections/{id}/instances/{{instanceId}}/position");
                collection_paths[&p] = json!({
                    "get": {
                        "summary": format!("Position query against a model run for {}", config.title),
                        "operationId": format!("getInstancePosition_{id}"),
                        "tags": [id],
                        "parameters": [
                            instance_id_param.clone(),
                            {"$ref": "#/components/parameters/coords-point"},
                            {"$ref": "#/components/parameters/datetime"},
                            {"$ref": "#/components/parameters/parameter-name"},
                            {"$ref": "#/components/parameters/z"},
                            {
                                "name": "f",
                                "in": "query",
                                "required": false,
                                "schema": {"type": "string", "enum": ["CoverageJSON", "PNG"]}
                            },
                            {"$ref": "#/components/parameters/limit"}
                        ],
                        "responses": {
                            "200": {
                                "description": "Coverage data",
                                "content": {
                                    COVERAGE_JSON_MEDIA_TYPE: {
                                        "schema": {"$ref": "#/components/schemas/coverageJSON"}
                                    },
                                    "image/png": {"schema": {"type": "string", "format": "binary"}}
                                }
                            },
                            "400": {"description": "Bad request"},
                            "404": {"description": "Not found"},
                            "500": {"description": "Server error"}
                        }
                    }
                });
            }
            if supported.contains("radius") {
                let p = format!("/edr/collections/{id}/instances/{{instanceId}}/radius");
                collection_paths[&p] = json!({
                    "get": {
                        "summary": format!("Radius query against a model run for {}", config.title),
                        "operationId": format!("getInstanceRadius_{id}"),
                        "tags": [id],
                        "parameters": [
                            instance_id_param.clone(),
                            {"$ref": "#/components/parameters/coords-radius"},
                            {"$ref": "#/components/parameters/within"},
                            {"$ref": "#/components/parameters/within-units"},
                            {"$ref": "#/components/parameters/datetime"},
                            {"$ref": "#/components/parameters/parameter-name"},
                            {"$ref": "#/components/parameters/z"},
                            {"$ref": "#/components/parameters/limit"}
                        ],
                        "responses": {
                            "200": {
                                "description": "Coverage data",
                                "content": {
                                    COVERAGE_JSON_MEDIA_TYPE: {
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
            if supported.contains("area") {
                let p = format!("/edr/collections/{id}/instances/{{instanceId}}/area");
                collection_paths[&p] = json!({
                    "get": {
                        "summary": format!("Area query against a model run for {}", config.title),
                        "operationId": format!("getInstanceArea_{id}"),
                        "tags": [id],
                        "parameters": [
                            instance_id_param.clone(),
                            {"$ref": "#/components/parameters/coords-polygon"},
                            {"$ref": "#/components/parameters/datetime"},
                            {"$ref": "#/components/parameters/parameter-name"},
                            {"$ref": "#/components/parameters/z"},
                            {"$ref": "#/components/parameters/limit"}
                        ],
                        "responses": {
                            "200": {
                                "description": "Coverage data",
                                "content": {
                                    COVERAGE_JSON_MEDIA_TYPE: {
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
        }
    }

    let mut paths = json!({
        "/edr/": {
            "get": {
                "summary": "Landing page",
                "operationId": "getLandingPage",
                "tags": [api_common::openapi_tags::DISCOVERY],
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
                "tags": [api_common::openapi_tags::DISCOVERY],
                "parameters": [format_parameter()],
                "responses": {
                    "200": {"description": "Conformance classes"}
                }
            }
        },
        "/edr/collections": {"get": api_common::collection_operation()}
    });

    // Merge collection paths into main paths
    if let (Some(main_obj), Some(coll_obj)) = (paths.as_object_mut(), collection_paths.as_object())
    {
        for (k, v) in coll_obj {
            main_obj.insert(k.clone(), v.clone());
        }
    }

    let mut openapi = json!({
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
                    "description": "WKT POINT or MULTIPOINT geometry; at most 64 points and 16384 decoded bytes, finite CRS84 coordinates only. Combined response budget: 1000000 values; query deadline: 30 seconds. Examples: POINT(24.94 60.17), MULTIPOINT((24.94 60.17),(23.76 61.5)). Note: for a MULTIPOINT against a collection with a vertical extent, every point's coverages are flattened into one CoverageCollection — per-point grouping is not preserved."
                },
                "coords-polygon": {
                    "name": "coords",
                    "in": "query",
                    "required": true,
                    "schema": {"type": "string"},
                    "description": "WKT POLYGON geometry, e.g. POLYGON((24 60, 26 60, 26 61, 24 61, 24 60))"
                },
                "coords-radius": {
                    "name": "coords",
                    "in": "query",
                    "required": true,
                    "schema": {"type": "string"},
                    "description": "WKT POINT geometry at the centre of the circle, e.g. POINT(24.94 60.17). MULTIPOINT is not accepted for radius queries."
                },
                "within": {
                    "name": "within",
                    "in": "query",
                    "required": true,
                    "schema": {"type": "number"},
                    "description": "Defines radius of area around defined coordinates to include in the data selection. Must be positive; at most 1000 km. The circle is evaluated as a 64-vertex geodesic polygon, so the response is the same shape as the area query's: gridded engines return the circle's bbox as the Grid domain with cells outside the disc null."
                },
                "within-units": {
                    "name": "within-units",
                    "in": "query",
                    "required": true,
                    "schema": {"type": "string"},
                    "description": "Distance units for the within parameter: km, m or mi (case-insensitive)."
                },
                "coords-linestring": {
                    "name": "coords",
                    "in": "query",
                    "required": true,
                    "schema": {"type": "string"},
                    "description": "WKT LINESTRING geometry (lon lat, lon lat, …), the ground track of a vertical cross-section. A cross-section is 2-D: LINESTRING Z, M and ZM are not accepted."
                },
                "coords-trajectory": {
                    "name": "coords",
                    "in": "query",
                    "required": true,
                    "schema": {"type": "string"},
                    "description": format!("WKT LINESTRING, LINESTRING Z, LINESTRING M or LINESTRING ZM (also written LINESTRINGZ, LINESTRINGM, LINESTRINGZM), CRS84 lon/lat. Z is each vertex's level in the collection's vertical coordinate (`extent.vertical`), snapped to the nearest advertised level; outside the advertised range → 400; ignored by a collection without a vertical extent. M is each vertex's time in seconds since the Unix epoch; each sample, interpolated along the path, takes the nearest available timestep; a time outside the available range → 400. Z cannot be combined with `z`, nor M with `datetime`. At most {MAX_TRAJECTORY_SAMPLES} samples at the source grid spacing, {MAX_TRAJECTORY_NODES} nodes (coverages × samples) and {MAX_AREA_VALUES} values (× parameters) per response; MULTILINESTRING is not supported. Examples: LINESTRING(24 60, 25 61), LINESTRING Z(24 60 850, 25 61 500), LINESTRING M(24 60 1767225600, 25 61 1767247200).")
                },
                // EDR 1.2 `/req/edr/rc-limit-definition`, with the schema
                // describing this server: no default (absent = no limit, not
                // 10) and values above the maximum clamped, not rejected.
                "limit": {
                    "name": "limit",
                    "in": "query",
                    "required": false,
                    "schema": {"type": "integer", "minimum": 1, "maximum": MAX_LIMIT},
                    "style": "form",
                    "explode": false,
                    "description": format!("Maximum number of top-level coverages in a CoverageCollection response. A single Coverage is one object and is returned unchanged. A MULTIPOINT position keeps the first coverages in point order, then each point's own coverage order, and skips querying points past the limit. Values above {MAX_LIMIT} are clamped to {MAX_LIMIT}; zero, negative and non-integer values are 400. Absent: no limit, every other response budget still applies. CoverageJSON has no paging links: the remaining coverages are not returned.")
                },
                "limit-locations": {
                    "name": "limit",
                    "in": "query",
                    "required": false,
                    "schema": {"type": "integer", "minimum": 1, "maximum": MAX_LIMIT},
                    "style": "form",
                    "explode": false,
                    "description": format!("Page size of the location list, in the collection's inventory order. With limit the response carries numberMatched, numberReturned and self, next and prev links that repeat the other query parameters. Values above {MAX_LIMIT} are clamped to {MAX_LIMIT}; zero, negative and non-integer values are 400. Absent: the complete inventory in one response, without paging members.")
                },
                "offset-locations": {
                    "name": "offset",
                    "in": "query",
                    "required": false,
                    "schema": {"type": "integer", "minimum": 0, "default": 0},
                    "description": "Number of locations to skip before the page (offset pagination extension, as on /collections). Requires limit."
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
    // Items components (#928), named `items-*` so they cannot collide with
    // another query's parameters.
    if !state.feature_engines.is_empty() {
        for (section, entries) in [
            ("parameters", crate::items::openapi_parameters()),
            ("schemas", crate::items::openapi_schemas()),
        ] {
            if let (Some(target), Some(entries)) = (
                openapi["components"][section].as_object_mut(),
                entries.as_object(),
            ) {
                target.extend(entries.clone());
            }
        }
    }

    Json(openapi)
}

pub async fn api_docs(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let state = state.load_full();
    let spec_url = format!("{}/edr/api", request_base_url(&state, &headers));
    (
        [
            (
                header::CONTENT_SECURITY_POLICY,
                ds_core::openapi::SWAGGER_UI_CSP,
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        axum::response::Html(ds_core::openapi::swagger_ui_html(
            "MeteoCore - EDR API",
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

pub async fn conformance(
    State(state): State<AppState>,
    Query(fp): Query<ds_core::html::FormatParams>,
    headers: HeaderMap,
) -> Result<Response, HandlerError> {
    use ds_core::html::{LinkView, Wanted};
    let wanted = negotiate(fp.f.as_deref(), &headers)?;
    let state = state.load_full();
    let base = &request_base_url(&state, &headers);
    // EDR 1.1 puts every data query under `queries`; each collection's
    // data_queries identifies supported types. GeoJSON is only available for
    // /locations, so geojson/edr-geojson conformance is not declared.
    let classes = api_common::conformance_classes(&[
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/core",
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/collections",
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/queries",
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/json",
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/covjson",
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/html",
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/oas30",
    ]);
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
            Html(api_common::workbench::conformance_html(
                api_common::workbench::Surface {
                    base,
                    root: &format!("{base}{}", api_common::mounts::EDR),
                    api: "edr",
                },
                &classes,
                &nav,
            ))
            .into_response()
        }
    }))
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
                    "collection has no registered EDR engine; omitting from /collections"
                );
                return None;
            };
            Some(api_common::CollectionEntry {
                config,
                metadata: build_collection_metadata(
                    engine.as_ref(),
                    config,
                    base,
                    None,
                    state.feature_engines.contains_key(&config.id),
                ),
                bbox: engine.get_spatial_extent(),
                time: engine.get_temporal_extent(),
            })
        })
        .collect();
    api_common::collections_response(
        api_common::workbench::Surface {
            base,
            root: &format!("{base}{}", api_common::mounts::EDR),
            api: "edr",
        },
        request,
        entries,
    )
}

/// GET /edr/collections/{id} — Collection detail
pub async fn collection(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Query(fp): Query<ds_core::html::FormatParams>,
    headers: HeaderMap,
) -> Result<Response, HandlerError> {
    use ds_core::html::Wanted;
    let wanted = negotiate(fp.f.as_deref(), &headers)?;
    let state = state.load_full();
    let (engine, config) = lookup_collection(&state, &id)?;
    let base = &request_base_url(&state, &headers);
    let items = state.feature_engines.contains_key(&id);
    Ok(with_vary(match wanted {
        Wanted::Json => Json(build_collection_metadata(
            engine.as_ref(),
            config,
            base,
            None,
            items,
        ))
        .into_response(),
        Wanted::Html => {
            let metadata = build_collection_metadata(engine.as_ref(), config, base, None, items);
            Html(api_common::workbench::collection_html(
                api_common::workbench::Surface {
                    base,
                    root: &format!("{base}{}", api_common::mounts::EDR),
                    api: "edr",
                },
                &metadata,
                config.license.as_ref(),
            ))
            .into_response()
        }
    }))
}

/// `GET /collections/{id}/instances` — list the collection's forecast model runs
/// as OGC API - EDR instances. Empty `collections` for non-forecast engines.
pub async fn instances(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Query(fp): Query<ds_core::html::FormatParams>,
    headers: HeaderMap,
) -> Result<Response, HandlerError> {
    use ds_core::html::{CollectionCard, LinkView, Wanted};
    let wanted = negotiate(fp.f.as_deref(), &headers)?;
    let state = state.load_full();
    let (engine, config) = lookup_collection(&state, &id)?;
    let base = &request_base_url(&state, &headers);
    // A non-forecast collection (no instances) returns 200 with an empty
    // `instances` list rather than 404. This is the standard REST list
    // convention and matches `/collections` (empty ⇒ 200 `{"collections":[]}`),
    // while the *item* paths `/instances/{id}[/…]` 404 for such collections.
    // OGC EDR 1.1 §8.2.3 ("only applies to resources that have temporal
    // instances") permits a stricter 404 here; the lenient 200-empty is the
    // deliberate, self-consistent choice (the resource is never advertised for
    // non-forecast collections, so conformant clients don't reach it).
    let runs = engine.get_instances();
    let self_href = format!("{base}/edr/collections/{}/instances", config.id);
    Ok(with_vary(match wanted {
        Wanted::Json => {
            // Each instance doc rebuilds the run-invariant bits (parameters,
            // spatial extent) via build_collection_metadata. That's a handful
            // of redundant clones (run count is bounded — a few to a few
            // dozen) on a low-QPS discovery endpoint, not the `/collections`/
            // `/api` hot paths #211 guards — kept simple over threading a
            // precomputed-metadata variant through.
            let instances: Vec<serde_json::Value> = runs
                .iter()
                .map(|run| {
                    build_collection_metadata(engine.as_ref(), config, base, Some(run), false)
                })
                .collect();
            // OGC API - EDR 1.1 §8.2.3 `instancesJSON`: the array field is
            // `instances` (each item a collection-shaped instance), not
            // `collections`.
            Json(json!({
                "links": [{
                    "href": self_href,
                    "rel": "self",
                    "type": "application/json",
                    "title": format!("{} — instances", config.title)
                }],
                "instances": instances,
            }))
            .into_response()
        }
        Wanted::Html => {
            // EDR 1.1 `html` class: the instance resources negotiate like every
            // other metadata page (flagged on #669). One card per model run.
            let cards: Vec<CollectionCard> = runs
                .iter()
                .map(|run| instance_card(config, base, run))
                .collect();
            let nav = [
                LinkView::new(format!("{self_href}?f=json"), "alternate", Some("JSON")),
                LinkView::new(
                    format!("{base}/edr/collections/{}", config.id),
                    "collection",
                    Some(&config.title),
                ),
            ];
            Html(api_common::workbench::instances_html(
                api_common::workbench::Surface {
                    base,
                    root: &format!("{base}{}", api_common::mounts::EDR),
                    api: "edr",
                },
                &format!("{} — instances", config.title),
                &cards,
                &nav,
            ))
            .into_response()
        }
    }))
}

/// The HTML card for one model run: id = the instance id, title = the
/// reference time, description = the valid-time span.
fn instance_card(
    config: &CollectionConfig,
    base: &str,
    run: &ds_core::instances::RunInfo,
) -> ds_core::html::CollectionCard {
    let instance_id = run.instance_id();
    let description = match (run.valid_times.first(), run.valid_times.last()) {
        (Some(first), Some(last)) => format!(
            "{} valid times, {} – {}",
            run.valid_times.len(),
            first.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            last.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ),
        _ => "no valid times".to_string(),
    };
    ds_core::html::CollectionCard {
        id: instance_id.clone(),
        title: format!(
            "Run {}",
            run.reference_time
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ),
        description,
        self_href: format!(
            "{base}/edr/collections/{}/instances/{instance_id}",
            config.id
        ),
        keywords: Vec::new(),
        license: None,
    }
}

/// `GET /collections/{id}/instances/{instanceId}` — one model run's metadata.
pub async fn instance(
    Path((id, instance_id)): Path<(String, String)>,
    State(state): State<AppState>,
    Query(fp): Query<ds_core::html::FormatParams>,
    headers: HeaderMap,
) -> Result<Response, HandlerError> {
    use ds_core::html::Wanted;
    let wanted = negotiate(fp.f.as_deref(), &headers)?;
    let state = state.load_full();
    let (engine, config) = lookup_collection(&state, &id)?;
    let base = &request_base_url(&state, &headers);
    // A collection with no instances has no instance sub-resources at all, so
    // any id (parseable or not) is 404 — consistent with the query path's
    // `resolve_instance` guard, rather than 400 on an unparseable id.
    if !engine.has_instances() {
        return Err(JsonError(
            StatusCode::NOT_FOUND,
            Json(json!({
                "code": "NotFound",
                "description": "This collection has no model-run instances"
            })),
        ));
    }
    let rt = ds_core::instances::parse_instance_id(&instance_id).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "code": "BadRequest",
                "description": format!("Invalid instance id '{instance_id}' (expected a reference time like 20260607T0600Z)")
            })),
        )
    })?;
    // O(1) single-run lookup — avoids cloning every run's valid times just to
    // find one (the engine builds only the requested run's RunInfo).
    let run = engine.find_instance(rt).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({
                "code": "NotFound",
                "description": format!("Instance '{instance_id}' not found")
            })),
        )
    })?;
    Ok(with_vary(match wanted {
        Wanted::Json => Json(build_collection_metadata(
            engine.as_ref(),
            config,
            base,
            Some(&run),
            false,
        ))
        .into_response(),
        Wanted::Html => {
            let metadata =
                build_collection_metadata(engine.as_ref(), config, base, Some(&run), false);
            Html(api_common::workbench::collection_html(
                api_common::workbench::Surface {
                    base,
                    root: &format!("{base}{}", api_common::mounts::EDR),
                    api: "edr",
                },
                &metadata,
                config.license.as_ref(),
            ))
            .into_response()
        }
    }))
}

/// `GET /collections/{id}/locations` — the location inventory as GeoJSON.
/// Without `limit`, the complete inventory in one response (#533). With the
/// EDR 1.2 `limit` (+ the `offset` extension), one page of it in the engine's
/// inventory order, with `numberMatched`/`numberReturned` and `self`/`next`/
/// `prev` links paged like `/collections` (#922). Either way the encoded body
/// is admitted by the same byte budget.
pub async fn locations(
    Path(id): Path<String>,
    State(state): State<AppState>,
    query: Result<Query<Vec<(String, String)>>, axum::extract::rejection::QueryRejection>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, HandlerError> {
    let state = state.load_full();
    let (engine, _config) = lookup_collection(&state, &id)?;
    let Query(pairs) = query.map_err(|_| bad_request_msg("Invalid query string"))?;
    let paging = parse_locations_paging(pairs).map_err(|e| bad_request(&e))?;

    let base_url = request_base_url(&state, &headers);
    let query_engine = engine.clone();
    let (body, etag) = execute_query(false, move |budget| {
        let server_error = || {
            JsonError(
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "code": "ServerError", "description": "Internal server error" })),
            )
        };
        let locs = query_engine.get_locations().map_err(|_| server_error())?;
        let params = query_engine.get_parameters();
        let temporal = query_engine
            .get_temporal_extent()
            .map(|(s, e)| (s.to_rfc3339(), e.to_rfc3339()));
        let ctx = LocationsContext {
            collection_id: &id,
            parameter_names: &params,
            temporal_extent: temporal,
            base_url: &base_url,
        };
        // One page: the /collections paging arithmetic over the inventory.
        let mut links = Vec::new();
        let (page_locs, page) = match &paging {
            None => (&locs[..], None),
            Some(paging) => {
                let window = ds_core::collection_search::page_window(
                    locs.len(),
                    paging.offset,
                    paging.limit,
                );
                let href = format!("{base_url}/edr/collections/{id}/locations");
                links.push((paging.href(&href, paging.offset), "self", "This page"));
                if window.has_next {
                    links.push((paging.href(&href, window.next_offset), "next", "Next page"));
                }
                if window.has_prev {
                    links.push((
                        paging.href(&href, window.prev_offset),
                        "prev",
                        "Previous page",
                    ));
                }
                (&locs[window.range()], Some(locs.len()))
            }
        };
        let page = page.map(|number_matched| LocationsPage {
            number_matched,
            links: &links,
        });
        // Keep construction, serialization and hashing under the same worker
        // permit as retrieval, even if the request times out or disconnects.
        let cancelled = || budget.expired();
        let mut writer = crate::location_budget::Writer::new(&cancelled);
        locations_to_writer(page_locs, &ctx, page.as_ref(), &mut writer).map_err(|_| {
            match writer.failure {
                Some(crate::location_budget::Failure::Cancelled) => query_timeout(),
                Some(crate::location_budget::Failure::Limit) => JsonError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({"code": "ResponseLimit", "description": if page.is_some() {
                        "Location page exceeds the configured response limit; request a smaller limit"
                    } else {
                        "Complete location inventory exceeds the configured response limit; page it with limit"
                    }})),
                ),
                Some(crate::location_budget::Failure::Memory) => JsonError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({"code": "ServerBusy", "description": "Location response memory capacity exhausted; retry later"})),
                ),
                None => server_error(),
            }
        })?;
        let body = writer.into_bytes();
        let etag = ds_core::http_cache::etag_of(&body);
        Ok((body, etag))
    })
    .await?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/geo+json".to_owned()),
            (header::ETAG, etag),
        ],
        body,
    ))
}

pub async fn location_query(
    Path((id, loc_id)): Path<(String, String)>,
    Query(params): Query<LocationQueryParams>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, HandlerError> {
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
    let limit = request_limit(params.limit.as_deref())?;

    let engine = engine.clone();
    let result = execute_query(false, move |_budget| {
        engine
            .query_location(
                &loc_id,
                datetime,
                param_names.as_deref(),
                z.as_deref(),
                None,
            )
            .map_err(|e| map_query_error(&e, "Location"))
    })
    .await?;

    let format = parse_edr_format(params.f.as_deref()).map_err(|e| bad_request(&e))?;
    render_coverage_response(
        limit_coverages(result, limit),
        format,
        params.width,
        params.height,
    )
    .map(|r| with_data_cache_control(r, datetime))
}

/// The 404 for a data query the collection's engine does not support:
/// the resource does not exist for that collection, the same answer for
/// every query type, and consistent with `data_queries` and the OpenAPI
/// document (#668).
fn require_query_type(
    engine: &Arc<dyn EdrEngine>,
    id: &str,
    query: &str,
    label: &str,
) -> Result<(), HandlerError> {
    if engine.supported_query_types().iter().any(|q| q == query) {
        return Ok(());
    }
    Err(JsonError(
        StatusCode::NOT_FOUND,
        Json(json!({
            "code": "NotFound",
            "description": format!("Collection '{id}' does not support {label} queries")
        })),
    ))
}

pub async fn position_query(
    Path(id): Path<String>,
    Query(params): Query<PositionQueryParams>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, HandlerError> {
    run_position_query(id, None, params, state).await
}

/// `GET /collections/{id}/instances/{instanceId}/position` — position query
/// against a specific forecast model run.
pub async fn instance_position_query(
    Path((id, instance_id)): Path<(String, String)>,
    Query(params): Query<PositionQueryParams>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, HandlerError> {
    run_position_query(id, Some(instance_id), params, state).await
}

async fn run_position_query(
    id: String,
    instance_id: Option<String>,
    params: PositionQueryParams,
    state: AppState,
) -> Result<impl IntoResponse, HandlerError> {
    let state = state.load_full();
    let (engine, _config) = lookup_collection(&state, &id)?;
    require_query_type(engine, &id, "position", "position")?;
    let reference_time = resolve_instance(engine, instance_id.as_deref())?;

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
    // passed through as one point. Engines can share field reads across the
    // batch; the default implementation still queries each point in turn.
    let mut points = split_position_coords(&params.coords).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "code": "BadRequest", "description": e.to_string() })),
        )
    })?;

    let format = parse_edr_format(params.f.as_deref()).map_err(|e| bad_request(&e))?;
    // `limit` counts the top-level coverages of the flattened collection
    // (#922): point order, then each point's own coverages (one per step
    // for a vertical profile). The response shape follows the request, so
    // a MULTIPOINT stays a CoverageCollection however few coverages remain.
    let limit = request_limit(params.limit.as_deref())?.unwrap_or(usize::MAX);
    let single = points.len() == 1;
    // A point the engine answers yields at least one coverage (none is a
    // 404), so the points past the first `limit` cannot reach the response:
    // never query them.
    points.truncate(limit);
    let engine = engine.clone();
    execute_query(false, move |budget| {
        let mut coverages = Vec::with_capacity(points.len());
        let mut values = 0usize;
        let mut collection_response = !single;
        let mut emit = |response| {
            if budget.expired() {
                return Err(DataServerError::DeadlineExceeded);
            }
            collection_response |= matches!(&response, CoverageResponse::Collection(_));
            let mut batch = match response {
                CoverageResponse::Single(q) => vec![q],
                CoverageResponse::Collection(v) => v,
            };
            // Drop what `limit` excludes before it counts against the budget.
            batch.truncate(limit.saturating_sub(coverages.len()));
            for q in &batch {
                for range in q.ranges.values() {
                    values = values.saturating_add(range.values.len());
                }
            }
            if values > crate::params::MAX_POSITION_VALUES {
                return Err(DataServerError::QueryTooLarge(format!(
                    "Position response exceeds {} values",
                    crate::params::MAX_POSITION_VALUES
                )));
            }
            coverages.extend(batch);
            Ok(())
        };
        if budget.expired() {
            return Err(query_timeout());
        }
        engine
            .query_positions(
                &points,
                datetime,
                param_names.as_deref(),
                z.as_deref(),
                reference_time,
                &mut emit,
            )
            .map_err(|e| map_query_error(&e, "Position"))?;
        if budget.expired() {
            return Err(query_timeout());
        }
        let result = if !collection_response && coverages.len() == 1 {
            CoverageResponse::Single(coverages.remove(0))
        } else {
            CoverageResponse::Collection(coverages)
        };
        render_coverage_response(result, format, params.width, params.height)
            .map(|r| with_data_cache_control(r, datetime))
    })
    .await
}

pub async fn area_query(
    Path(id): Path<String>,
    Query(params): Query<AreaQueryParams>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, HandlerError> {
    run_area_query(id, None, params, state).await
}

/// `GET /collections/{id}/instances/{instanceId}/area` — area query against a
/// specific forecast model run.
pub async fn instance_area_query(
    Path((id, instance_id)): Path<(String, String)>,
    Query(params): Query<AreaQueryParams>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, HandlerError> {
    run_area_query(id, Some(instance_id), params, state).await
}

async fn run_area_query(
    id: String,
    instance_id: Option<String>,
    params: AreaQueryParams,
    state: AppState,
) -> Result<impl IntoResponse, HandlerError> {
    let state = state.load_full();
    let (engine, _config) = lookup_collection(&state, &id)?;
    require_query_type(engine, &id, "area", "area")?;

    // Static request-level checks before engine/instance resolution, so the
    // instance variant rejects the same way as the non-instance `area_query`
    // (e.g. `…/instances/x/area?f=png` → 400 "PNG not available", not a 404 on
    // the instance). An area result is gridded / multi-coverage, not a plot.
    if parse_edr_format(params.f.as_deref()).map_err(|e| bad_request(&e))? == EdrFormat::Png {
        return Err(bad_request(&DataServerError::InvalidParameter(
            "PNG output is not available for area queries".into(),
        )));
    }

    let reference_time = resolve_instance(engine, instance_id.as_deref())?;

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
    let limit = request_limit(params.limit.as_deref())?;

    let engine = engine.clone();
    let result = execute_query(false, move |_budget| {
        engine
            .query_area(
                &params.coords,
                datetime,
                param_names.as_deref(),
                z.as_deref(),
                reference_time,
            )
            .map_err(|e| map_query_error(&e, "Area"))
    })
    .await?;

    let result = limit_coverages(result, limit);
    Ok(with_data_cache_control(
        coverage_json_response(&result, "Area")?,
        datetime,
    ))
}

pub async fn radius_query(
    Path(id): Path<String>,
    Query(params): Query<RadiusQueryParams>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, HandlerError> {
    run_radius_query(id, None, params, state).await
}

/// `GET /collections/{id}/instances/{instanceId}/radius` — radius query
/// against a specific forecast model run.
pub async fn instance_radius_query(
    Path((id, instance_id)): Path<(String, String)>,
    Query(params): Query<RadiusQueryParams>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, HandlerError> {
    run_radius_query(id, Some(instance_id), params, state).await
}

/// OGC API - EDR `radius`: everything within `within` `within-units` of a
/// WKT `POINT`. The engine turns the circle into a polygon and answers it
/// as an area query (see `EdrEngine::query_radius`), so the response shape
/// and error mapping are the area query's.
async fn run_radius_query(
    id: String,
    instance_id: Option<String>,
    params: RadiusQueryParams,
    state: AppState,
) -> Result<impl IntoResponse, HandlerError> {
    let state = state.load_full();
    let (engine, _config) = lookup_collection(&state, &id)?;

    // Same capability guard as trajectory: an engine that does not advertise
    // `radius` has no such resource (404), and the live route stays
    // consistent with `data_queries` and the OpenAPI gating.
    require_query_type(engine, &id, "radius", "radius")?;

    if parse_edr_format(params.f.as_deref()).map_err(|e| bad_request(&e))? == EdrFormat::Png {
        return Err(bad_request(&DataServerError::InvalidParameter(
            "PNG output is not available for radius queries".into(),
        )));
    }

    let within_m =
        parse_within_metres(&params.within, &params.within_units).map_err(|e| bad_request(&e))?;

    let reference_time = resolve_instance(engine, instance_id.as_deref())?;

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
    let limit = request_limit(params.limit.as_deref())?;

    let engine = engine.clone();
    let result = execute_query(false, move |_budget| {
        engine
            .query_radius(
                &params.coords,
                within_m,
                datetime,
                param_names.as_deref(),
                z.as_deref(),
                reference_time,
            )
            .map_err(|e| map_query_error(&e, "Radius"))
    })
    .await?;

    let result = limit_coverages(result, limit);
    Ok(with_data_cache_control(
        coverage_json_response(&result, "Radius")?,
        datetime,
    ))
}

pub async fn trajectory_query(
    Path(id): Path<String>,
    Query(params): Query<TrajectoryQueryParams>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, HandlerError> {
    let state = state.load_full();
    let (engine, _config) = lookup_collection(&state, &id)?;

    // An engine that doesn't advertise `trajectory` has no such resource.
    // Return 404 (the resource doesn't exist for this collection) rather
    // than letting the default trait method answer 400 (which wrongly
    // implies the *request* was malformed). Keeps the live route
    // consistent with the `api_definition` OpenAPI gating and the
    // `data_queries` collection metadata. Flagged by claude-review.
    require_query_type(engine, &id, "trajectory", "trajectory")?;
    if params.limit.is_some() {
        return Err(bad_request_msg(
            "limit is not supported on trajectory queries: EDR 1.2 defines no limit for them",
        ));
    }
    let shape = engine.trajectory_shape();

    let format = parse_edr_format(params.f.as_deref()).map_err(|e| bad_request(&e))?;
    if format == EdrFormat::Png && shape == TrajectoryShape::AlongPath {
        return Err(bad_request(&DataServerError::InvalidParameter(
            "PNG output is only available for radar cross-section trajectories; this \
             collection answers CoverageJSON"
                .into(),
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

    // `z` selects levels from the collection's advertised vertical extent:
    // elevation angles bounding a radar cross-section's sweeps, or the
    // levels a 2-D / M path is sampled on. An interval expands to the
    // advertised levels in range.
    let z = resolve_request_z(engine, params.z.as_deref())?;

    if shape == TrajectoryShape::AlongPath {
        // Validate the whole path before dispatch. OGC API - EDR 1.2: a
        // path that carries its own levels (Z) or times (M) SHALL NOT be
        // combined with `z` or `datetime` (the trajectory query type's
        // error list; /conf/trajectory/coords-param-separate-z-*).
        let path = TrajectoryPath::parse(&params.coords).map_err(|e| bad_request(&e))?;
        if path.has_z && z.is_some() {
            return Err(bad_request_msg(&format!(
                "A {} carries each vertex's level; do not also pass `z`",
                path.keyword()
            )));
        }
        if path.has_m && datetime.is_some() {
            return Err(bad_request_msg(&format!(
                "A {} carries each vertex's time; do not also pass `datetime`",
                path.keyword()
            )));
        }
    }

    // A cross-section engine drives its remote reads through an explicit
    // runtime handle and needs a blocking thread; along-path sampling runs
    // on the query runtime's workers like position and area.
    let blocking = shape == TrajectoryShape::CrossSection;
    let engine = engine.clone();
    let coords = params.coords.clone();
    let result = execute_query(blocking, move |_budget| {
        engine
            .query_trajectory(
                &coords,
                datetime,
                param_names.as_deref(),
                z.as_deref(),
                None,
            )
            .map_err(|e| map_query_error(&e, "Trajectory"))
    })
    .await?;

    match format {
        EdrFormat::CoverageJson => Ok(with_data_cache_control(
            coverage_json_response(&result, "Trajectory")?,
            datetime,
        )),
        EdrFormat::Png => {
            // Render the cross-section as a colour-mapped heatmap using
            // the collection's resolved default style (or a data-scaled
            // viridis fallback when the collection has none).
            // A failure here is an internal inconsistency (the engine
            // already returned a Section for this trajectory query), not
            // a client mistake — log it and return a generic 500 rather
            // than leaking the internal message in a 400.
            let style = state.styles.get(&id).and_then(|m| m.get("default"));
            let (heatmaps, colormap) =
                section_response_to_heatmaps(&result, style).map_err(|e| {
                    tracing::error!("Trajectory PNG section conversion error: {e}");
                    server_error()
                })?;
            let (w, h) = plot_dimensions(params.width, params.height);
            let png = render_heatmap(&heatmaps, colormap.as_ref(), w, h).map_err(|e| {
                tracing::error!("Trajectory PNG render error: {e}");
                server_error()
            })?;
            Ok(with_data_cache_control(
                ([(header::CONTENT_TYPE, "image/png")], png).into_response(),
                datetime,
            ))
        }
    }
}

/// An EDR `extent.temporal` object: the interval, the Gregorian TRS and,
/// when known, the individual timesteps.
fn temporal_extent_json(
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    times: Option<&[chrono::DateTime<chrono::Utc>]>,
) -> serde_json::Value {
    let mut temporal = serde_json::Map::new();
    temporal.insert(
        "interval".to_string(),
        json!([[start.to_rfc3339(), end.to_rfc3339()]]),
    );
    temporal.insert(
        "trs".to_string(),
        json!("http://www.opengis.net/def/uom/ISO-8601/0/Gregorian"),
    );
    if let Some(times) = times {
        let values: Vec<String> = times.iter().map(|t| t.to_rfc3339()).collect();
        temporal.insert("values".to_string(), json!(values));
    }
    serde_json::Value::Object(temporal)
}

/// A data query's `data_queries.<type>.link.variables`, or `None` for a query
/// type this API has no route for.
///
/// EDR 1.2 requires `title`, `description`, `query_type`, `output_formats`,
/// `default_output_format` and `crs_details` in every one (the `*DataQuery`
/// schemas; all but `query_type` were optional in 1.1). `crs_details` lists
/// the one CRS data queries accept ([`DATA_QUERY_CRS`], until #84), radius
/// adds its accepted `within_units`. `output_formats` are the formats the
/// route answers: area and radius results are gridded or multi-coverage, so
/// they have no PNG plot. `trajectory` is the engine's
/// [`EdrEngine::trajectory_shape`]: an along-path trajectory (#926) is
/// CoverageJSON only, a radar cross-section also renders as a PNG.
fn data_query_variables(
    query_type: &str,
    trajectory: TrajectoryShape,
) -> Option<serde_json::Value> {
    let (title, description, output_formats): (&str, &str, &[&str]) = match query_type {
        "locations" => (
            "Locations query",
            "Lists the collection's named locations as GeoJSON; \
             /locations/{locationId} returns the data at one of them.",
            &["CoverageJSON", "PNG"],
        ),
        "position" => (
            "Position query",
            "Data at the WKT POINT or MULTIPOINT given in coords, \
             as CRS84 longitude and latitude.",
            &["CoverageJSON", "PNG"],
        ),
        "area" => (
            "Area query",
            "Data inside the WKT POLYGON given in coords, as CRS84 longitude and latitude.",
            &["CoverageJSON"],
        ),
        "radius" => (
            "Radius query",
            "Data within a distance of the WKT POINT given in coords, as CRS84 \
             longitude and latitude; within and within-units give the distance.",
            &["CoverageJSON"],
        ),
        "trajectory" => match trajectory {
            TrajectoryShape::AlongPath => (
                "Trajectory query",
                "Data sampled along the WKT LINESTRING, LINESTRING Z, LINESTRING M or \
                 LINESTRING ZM given in coords, as CRS84 longitude and latitude; Z is \
                 each vertex's level, M its time in seconds since the Unix epoch.",
                &["CoverageJSON"],
            ),
            TrajectoryShape::CrossSection => (
                "Trajectory query",
                "A vertical cross-section along the 2-D WKT LINESTRING given in coords, \
                 as CRS84 longitude and latitude.",
                &["CoverageJSON", "PNG"],
            ),
        },
        _ => return None,
    };
    let mut variables = json!({
        "title": title,
        "description": description,
        "query_type": query_type,
        "output_formats": output_formats,
        "default_output_format": "CoverageJSON",
        "crs_details": [{ "crs": DATA_QUERY_CRS, "wkt": CRS84_WKT }]
    });
    if query_type == "radius" {
        // EDR radius link variables carry the accepted `within-units`.
        variables["within_units"] = json!(WITHIN_UNITS);
    }
    Some(variables)
}

/// Build a collection (or instance) metadata document.
///
/// `instance = None` ⇒ the collection itself (un-pinned; latest run for forecast
/// engines). `instance = Some(run)` ⇒ that forecast model run as an OGC EDR
/// *instance*: `id`, temporal extent and data-query hrefs are scoped to the run
/// (`/collections/{id}/instances/{instanceId}/…`). See [`ds_core::instances`].
///
/// `items` ⇒ the collection serves the `items` query through its
/// `FeatureEngine` (#928); an instance document never advertises it.
fn build_collection_metadata(
    engine: &dyn EdrEngine,
    config: &CollectionConfig,
    base_url: &str,
    instance: Option<&ds_core::instances::RunInfo>,
    items: bool,
) -> serde_json::Value {
    let param_descs = engine.get_parameter_descriptions();
    // Advertise a CRS84-domain extent: engine bounds can be grid cell edges
    // past the domain, or an empty-accumulator sentinel.
    let spatial = engine
        .get_spatial_extent()
        .and_then(ds_core::geo::crs84_extent);

    let coll_id = &config.id;
    // The self id and the base path every data-query href hangs off — scoped to
    // the instance when one is given.
    let (self_id, query_base) = match instance {
        Some(run) => {
            let iid = run.instance_id();
            let base = format!("{base_url}/edr/collections/{coll_id}/instances/{iid}");
            (iid, base)
        }
        None => (
            coll_id.clone(),
            format!("{base_url}/edr/collections/{coll_id}"),
        ),
    };

    // Temporal extent + advertised timesteps: the run's for an instance, the
    // engine's (latest run) for the collection.
    let temporal = match instance {
        Some(run) => run.temporal_extent(),
        None => engine.get_temporal_extent(),
    };

    let mut extent = serde_json::Map::new();
    if let Some(bbox) = spatial {
        extent.insert(
            "spatial".to_string(),
            json!({ "bbox": [bbox], "crs": "http://www.opengis.net/def/crs/OGC/1.3/CRS84" }),
        );
    }
    if let Some((start, end)) = temporal {
        // Include individual timesteps if available (the run's valid times for
        // an instance, the engine's for the collection).
        let times = match instance {
            Some(run) => (!run.valid_times.is_empty()).then(|| run.valid_times.clone()),
            None => engine.get_available_times(),
        };
        extent.insert(
            "temporal".to_string(),
            temporal_extent_json(start, end, times.as_deref()),
        );
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

    // Sorted iteration: serde_json's workspace-enabled `preserve_order` makes
    // insertion order the wire order, and `get_parameter_descriptions` builds
    // a fresh HashMap per call — unsorted, byte-identical requests would
    // serialize differently and the content-derived ETag would never
    // revalidate (#499).
    let mut sorted_descs: Vec<_> = param_descs.iter().collect();
    sorted_descs.sort_by_key(|(name, _)| *name);
    let parameter_names: serde_json::Map<String, serde_json::Value> = sorted_descs
        .into_iter()
        .map(|(name, desc)| {
            let mut param = collection_parameter_json(desc);
            // A parameter on its own time axis (a satellite product) carries
            // its own temporal extent; the collection's is the union (#819).
            // Instances keep the run's axis.
            let own_times = instance
                .is_none()
                .then(|| engine.get_parameter_available_times(name))
                .flatten();
            if let Some(times) = own_times {
                if let (Some(&start), Some(&end)) = (times.first(), times.last()) {
                    param["extent"] =
                        json!({ "temporal": temporal_extent_json(start, end, Some(&times)) });
                }
            }
            (name.clone(), param)
        })
        .collect();

    // Data queries hang off `query_base` (instance-scoped when applicable).
    // Under an instance only the run-queryable types (position/area) get routes.
    let query_types: Vec<String> = if instance.is_some() {
        engine
            .supported_query_types()
            .into_iter()
            .filter(|qt| qt == "position" || qt == "area" || qt == "radius")
            .collect()
    } else {
        engine.supported_query_types()
    };
    let mut data_queries = serde_json::Map::new();
    for qt in &query_types {
        // Every routed query type's path segment is its name.
        let Some(variables) = data_query_variables(qt, engine.trajectory_shape()) else {
            continue;
        };
        data_queries.insert(
            qt.clone(),
            json!({
                "link": {
                    "href": format!("{query_base}/{qt}"),
                    "rel": "data",
                    "variables": variables
                }
            }),
        );
    }
    // The collection's features (#928), not scoped to a model run.
    if instance.is_none() && items {
        data_queries.insert("items".to_string(), crate::items::data_query(&query_base));
    }
    // Advertise the model runs (forecast reference times) as EDR instances on
    // the collection itself (not on an instance document).
    if instance.is_none() && engine.has_instances() {
        data_queries.insert(
            "instances".to_string(),
            json!({
                "link": {
                    "href": format!("{base_url}/edr/collections/{coll_id}/instances"),
                    "rel": "data",
                    "variables": { "query_type": "instances" }
                }
            }),
        );
    }

    let self_title = match instance {
        Some(run) => format!("{} — run {}", config.title, run.reference_time.to_rfc3339()),
        None => config.title.clone(),
    };
    let mut links = vec![json!({
        "href": query_base,
        "rel": "self",
        "type": "application/json",
        "title": self_title.clone()
    })];
    if instance.is_some() {
        // Link an instance document back to its parent collection.
        links.push(json!({
            "href": format!("{base_url}/edr/collections/{coll_id}"),
            "rel": "collection",
            "type": "application/json",
            "title": config.title
        }));
    }

    api_common::collection_metadata(
        config,
        json!({
            "id": self_id,
            "title": self_title,
            // No `itemType` and no `rel=items` link: EDR's `items` is a data
            // query (#928), advertised in `data_queries` like the others and
            // GeoJSON only, while Common Part 2 `itemType` and the workbench's
            // `items` link mean a Features-style resource with an HTML view.
            // EDR collections are also not all coverage data (CSV/PostGIS serve
            // discrete observations), so no single itemType applies. Omitted
            // rather than mislabelled (review on #298).
            "extent": extent,
            "data_queries": data_queries,
            "crs": ["http://www.opengis.net/def/crs/OGC/1.3/CRS84"],
            "parameter_names": parameter_names,
            "output_formats": ["CoverageJSON", "PNG"]
        }),
        links,
    )
}
