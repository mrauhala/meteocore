use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use serde_json::json;

use api_common::JsonError;
use ds_core::config::CollectionConfig;
use ds_core::edr_engine::{EdrEngine, TrajectoryShape};

use ds_core::error::DataServerError;
use ds_core::feature::MAX_AREA_VALUES;
use ds_core::model::CoverageResponse;
use ds_core::trajectory::{TrajectoryPath, MAX_TRAJECTORY_NODES, MAX_TRAJECTORY_SAMPLES};
use ds_render::{render_chart, render_heatmap};

use crate::geojson::{
    encode_path_segment, FeatureIdentity, GeoJsonError, GeoJsonLink, LocationIndex,
};
use crate::params::{
    check_crs, negotiate_edr_format, negotiate_list_format, parse_cube_bbox, parse_datetime,
    parse_edr_format, parse_limit, parse_locations_query, parse_resolution, parse_within_metres,
    parse_z, plot_dimensions, query_formats, resolve_z_levels, split_location_ids,
    split_position_coords, AreaQueryParams, CubeQueryParams, DatetimeSelector, EdrFormat,
    LocationQueryParams, NegotiatedFormat, PositionQueryParams, RadiusQueryParams,
    TrajectoryQueryParams, ZSelector, CRS84_WKT, DATA_QUERY_CRS, LOCATIONS_FORMATS, MAX_LIMIT,
    MAX_LOCATION_IDS, MAX_LOCATION_LOOKUPS, MAX_LOCATION_VALUES, WITHIN_UNITS,
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

/// What a data query's GeoJSON representation needs besides the result: the
/// engine (to name each station) and the request (for its `links`).
struct GeoJsonRequest {
    engine: Arc<dyn EdrEngine>,
    base: String,
    collection_id: String,
    collection_title: String,
    /// The resource path below `/edr`, e.g. `/collections/obs/position`.
    path: String,
    raw_query: Option<String>,
    /// The formats this query offers: one `alternate` link each.
    offered: Vec<EdrFormat>,
    /// The `/locations/{locationId}` ids the coverages belong to: one names
    /// every coverage, a list's names coverage `i` by entry `i` (#923).
    /// Empty: each coverage is named by its coordinates.
    location_ids: Vec<String>,
}

impl GeoJsonRequest {
    /// This query's URL with `f` set to `format`, every other parameter as
    /// the client sent it.
    fn url(&self, format: EdrFormat) -> String {
        let f = format!("f={}", format.name());
        let query: Vec<&str> = self
            .raw_query
            .as_deref()
            .unwrap_or("")
            .split('&')
            .filter(|pair| !pair.is_empty() && pair.split('=').next() != Some("f"))
            .chain(std::iter::once(f.as_str()))
            .collect();
        format!("{}/edr{}?{}", self.base, self.path, query.join("&"))
    }

    /// `/req/edr-geojson/content` B (`/req/core/rc-collection-info-links`):
    /// `self`, an `alternate` per other offered format, and a link to the
    /// collection, each with `rel` and `type`.
    fn links(&self) -> Vec<GeoJsonLink> {
        self.links_as(EdrFormat::GeoJson)
    }

    /// The links of this query's `current` representation: see [`Self::links`].
    fn links_as(&self, current: EdrFormat) -> Vec<GeoJsonLink> {
        let mut links = vec![GeoJsonLink {
            href: self.url(current),
            rel: "self",
            kind: current.media_type(),
            title: "This document".into(),
        }];
        for &format in &self.offered {
            if format != current {
                links.push(GeoJsonLink {
                    href: self.url(format),
                    rel: "alternate",
                    kind: format.media_type(),
                    title: format!("This document as {}", format.name()),
                });
            }
        }
        links.push(GeoJsonLink {
            href: format!("{}/edr/collections/{}", self.base, self.collection_id),
            rel: "collection",
            kind: "application/json",
            title: self.collection_title.clone(),
        });
        links
    }

    /// This query's HTML page context (#971): its links as HTML, the
    /// default format as the page's JSON switch.
    fn html_page(&self) -> crate::html::DataPage<'_> {
        let segment = self.path.rsplit('/').next().unwrap_or_default();
        let title = if self.path.contains("/locations/") {
            let ids = percent_encoding::percent_decode_str(segment).decode_utf8_lossy();
            format!("Location data: {ids}")
        } else {
            let mut query = segment.to_owned();
            if let Some(first) = query.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            format!("{query} query")
        };
        crate::html::DataPage {
            base: &self.base,
            collection_id: &self.collection_id,
            collection_title: &self.collection_title,
            title,
            raw_query: self.raw_query.as_deref(),
            json_url: self.url(
                self.offered
                    .first()
                    .copied()
                    .unwrap_or(EdrFormat::CoverageJson),
            ),
            links: self
                .links_as(EdrFormat::Html)
                .into_iter()
                .map(|l| crate::html::Link {
                    href: l.href,
                    rel: l.rel.into(),
                    kind: l.kind.into(),
                    title: l.title,
                })
                .collect(),
        }
    }
}

/// Encode a station-series result as EDR GeoJSON (#929), naming each
/// coverage's station: the requested location for `/locations/{id}`, else
/// the one location at the coverage's exact coordinates.
fn render_station_geojson(
    result: &CoverageResponse,
    number_matched: Option<usize>,
    req: &GeoJsonRequest,
) -> Result<Response, HandlerError> {
    let index = LocationIndex::new(
        req.engine
            .get_locations()
            .map_err(|e| map_query_error(&e, "GeoJSON locations"))?,
    );
    let named = |i: usize| {
        let id = match req.location_ids.as_slice() {
            [] => return None,
            [only] => only,
            each => each.get(i)?,
        };
        Some(FeatureIdentity {
            id,
            label: index.by_id(id).map_or(id.as_str(), |l| l.label.as_str()),
        })
    };
    let mut body = Vec::new();
    crate::geojson::write_station_series(
        result,
        |i, q| {
            named(i).or_else(|| match q.domain {
                ds_core::model::DomainDescription::PointSeries { x, y, .. } => {
                    index.at(x, y).map(|l| FeatureIdentity {
                        id: &l.id,
                        label: &l.label,
                    })
                }
                _ => None,
            })
        },
        &format!("{}/edr/collections/{}", req.base, req.collection_id),
        &req.links(),
        number_matched,
        &mut body,
    )
    .map_err(|e| match e {
        GeoJsonError::ReservedParameterName(name) => bad_request_msg(&format!(
            "Parameter '{name}' has no GeoJSON encoding: its name is one of the feature's own \
             properties ({}). Leave it out of parameter-name, or use f=CoverageJSON",
            crate::geojson::RESERVED_PROPERTIES.join(", ")
        )),
        GeoJsonError::NotStationSeries(kind) => {
            tracing::error!(
                collection = %req.collection_id,
                "EDR GeoJSON: a station-series engine answered a {kind} coverage"
            );
            server_error()
        }
        GeoJsonError::Json(e) => {
            tracing::error!("EDR GeoJSON serialise error: {e}");
            server_error()
        }
    })?;
    Ok((
        [(header::CONTENT_TYPE, EdrFormat::GeoJson.media_type())],
        body,
    )
        .into_response())
}

/// Serialise an EDR coverage response in the requested output format.
///
/// `CoverageJSON` is the default; `GeoJSON` encodes station series (only
/// offered where the engine serves them), with `number_matched` (the
/// top-level coverages before `limit`, `None` when uncounted) as its
/// `numberMatched`; `PNG` renders a vertical-profile or time-series plot
/// (one stacked panel per parameter); `HTML` is a page of the CoverageJSON
/// (#971). A response that can't be plotted (a gridded/area result) maps
/// to 400.
fn render_coverage_response(
    result: CoverageResponse,
    number_matched: Option<usize>,
    format: EdrFormat,
    width: Option<u32>,
    height: Option<u32>,
    geojson: &GeoJsonRequest,
) -> Result<Response, HandlerError> {
    match format {
        EdrFormat::CoverageJson => coverage_json_response(&result, "EDR"),
        EdrFormat::GeoJson => render_station_geojson(&result, number_matched, geojson),
        EdrFormat::Png => {
            let panels = coverage_response_to_panels(&result).map_err(|e| bad_request(&e))?;
            let (w, h) = plot_dimensions(width, height);
            let png = render_chart(&panels, w, h).map_err(|e| {
                tracing::error!("EDR plot render error: {e}");
                server_error()
            })?;
            Ok(([(header::CONTENT_TYPE, "image/png")], png).into_response())
        }
        EdrFormat::Html => Ok(crate::html::response(crate::html::coverage_page(
            &result,
            &geojson.html_page(),
        ))),
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

/// The 400 for a data query's `crs` naming a CRS other than the CRS84 every
/// `crs_details` lists (EDR 1.2 `/req/edr/REQ_rc-crs-response` C).
fn request_crs(raw: Option<&str>) -> Result<(), HandlerError> {
    check_crs(raw).map_err(|e| bad_request(&e))
}

/// The number of top-level coverages a result has before `limit`: the
/// `numberMatched` of its GeoJSON representation (#929).
fn coverage_count(result: &CoverageResponse) -> usize {
    match result {
        CoverageResponse::Single(_) => 1,
        CoverageResponse::Collection(coverages) => coverages.len(),
    }
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

/// The formats `query_type` offers on `engine`: [`query_formats`] fed with
/// the engine's traits, so negotiation and `data_queries` agree.
fn engine_query_formats(engine: &dyn EdrEngine, query_type: &str) -> &'static [EdrFormat] {
    query_formats(
        query_type,
        engine.serves_station_series(),
        engine.trajectory_shape(),
    )
}

/// A data query's output format: `f`, else the `Accept` header, among the
/// formats `query_type` offers on this engine (400 for an `f` it does not).
fn data_query_format(
    engine: &Arc<dyn EdrEngine>,
    query_type: &str,
    f: Option<&str>,
    headers: &HeaderMap,
    what: &str,
) -> Result<NegotiatedFormat, HandlerError> {
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok());
    let offered = engine_query_formats(engine.as_ref(), query_type);
    negotiate_edr_format(f, accept, offered, what).map_err(|e| bad_request(&e))
}

/// `Vary: Accept` on a data response whose format the `Accept` header chose.
pub(crate) fn with_format_vary(resp: Response, format: NegotiatedFormat) -> Response {
    if format.vary_accept {
        with_vary(resp)
    } else {
        resp
    }
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
/// request interval, or the span of a datetime list
/// ([`DatetimeSelector::envelope`]); the `parse_datetime_interval`
/// open-bound sentinels (`MIN_UTC`/`MAX_UTC`) map back to "open" for
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
                "description": format!("Invalid instance id '{iid}' (expected an RFC 3339 reference time like 2026-06-07T06:00:00Z)")
            })),
        )
    })?;
    Ok(Some(rt))
}

/// The `{instanceId}` segment of a link to an instance resource: the
/// canonical RFC 3339 id of the run the request named, whichever accepted
/// form it used (#947). Its colons stay unencoded, as RFC 3986 `pchar`
/// allows; an id that does not parse is percent-encoded verbatim.
fn instance_path_segment(iid: &str) -> String {
    match ds_core::instances::parse_instance_id(iid) {
        Some(rt) => ds_core::instances::format_instance_id(rt),
        None => encode_path_segment(iid),
    }
}

/// Parse and resolve the request `z` parameter into the concrete level
/// list an engine samples.
///
/// - Absent / blank → `None` (whole vertical extent).
/// - The value is parsed first, so a malformed `z` is a 400 on every
///   collection.
/// - A well-formed `z` against a collection with no vertical dimension is
///   ignored → `None`: EDR 1.2 `/req/edr/z-response` A says it SHALL be.
///   WMS `ELEVATION` and Maps/Tiles `elevation` keep rejecting it; they
///   follow their own standards.
/// - An interval (`z=min/max`, `../max`, `min/..`) is expanded against the
///   collection's advertised levels; a list — including the levels a
///   recurring `Rn/min/step` expands to — passes through for the engine.
fn resolve_request_z(
    engine: &Arc<dyn EdrEngine>,
    z: Option<&str>,
) -> Result<Option<Vec<f64>>, HandlerError> {
    let Some(sel) = parse_z(z).map_err(|e| bad_request(&e))? else {
        return Ok(None);
    };
    resolve_z_selector(engine, &sel)
}

/// [`resolve_request_z`] for an already parsed selector — also the vertical
/// pair of a cube's six-number `bbox`, which is ignored the same way on a
/// collection without a vertical dimension.
fn resolve_z_selector(
    engine: &Arc<dyn EdrEngine>,
    sel: &ZSelector,
) -> Result<Option<Vec<f64>>, HandlerError> {
    let Some(extent) = engine.get_vertical_extent() else {
        return Ok(None);
    };
    let levels = resolve_z_levels(sel, Some(&extent)).map_err(|e| bad_request(&e))?;
    Ok(Some(levels))
}

/// Parse the request `datetime` (an instant, an interval, or an EDR 1.2
/// list of instants) into a 400 on failure.
fn request_datetime(raw: Option<&str>) -> Result<Option<DatetimeSelector>, HandlerError> {
    parse_datetime(raw).map_err(|e| bad_request(&e))
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
            OPENAPI_MEDIA_TYPE,
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
/// metadata endpoints (landing, conformance, collection detail, instances).
fn format_parameter() -> serde_json::Value {
    json!({"name": "f", "in": "query", "required": false, "schema": {"type": "string", "enum": ["json", "html"]},
           "description": "Output format. 'json' (default) or 'html'; overrides the Accept header."})
}

/// The media type `/api` is served as: the type the landing page's
/// `service-desc` link names (`/req/core/api-definition-success` C).
const OPENAPI_MEDIA_TYPE: &str = "application/vnd.oai.openapi+json;version=3.0";

/// The shared responses of `/api` (`components.responses`): status, name
/// and default description. Operations reference them through
/// [`responses`] and [`document_router_responses`], so an operation lists
/// every status its route can answer (EDR 1.2 `/req/oas/completeness`,
/// `/req/oas/exceptions-codes`).
const SHARED_RESPONSES: [(u16, &str, &str); 6] = [
    (
        304,
        "NotModified",
        "Not modified: If-None-Match names the representation's current ETag. No body.",
    ),
    (
        400,
        "BadRequest",
        "Bad request: an invalid, unknown or repeated query parameter, or an unsupported f. \
         A query string that does not parse into the operation's parameters at all, such as a \
         missing coords, is answered as text/plain.",
    ),
    (
        404,
        "NotFound",
        "Not found: the collection, a resource within it, a query type it does not support, \
         or data for the request.",
    ),
    (
        500,
        "ServerError",
        "Internal server error. The body, when there is one, carries no details.",
    ),
    (
        503,
        "ServiceUnavailable",
        "The query executor is at capacity (ServerBusy), or a response would exceed its \
         configured budget (ResponseLimit); retry later, or narrow the request.",
    ),
    (
        504,
        "GatewayTimeout",
        "The query exceeded its time budget, queue time included (Timeout).",
    ),
];

/// Error statuses of a content-negotiated metadata resource: an unknown `f`
/// (400) and an unknown collection or instance (404).
const METADATA_ERRORS: &[u16] = &[400, 404];

/// Error statuses of a data query: an invalid request (400); an unknown
/// collection, a query type it does not support or no data (404); and the
/// query executor's capacity (503) and deadline (504), from
/// [`execute_query`] and [`map_query_error`].
const QUERY_ERRORS: &[u16] = &[400, 404, 503, 504];

/// Statuses every route of this router can answer, whatever its handler:
/// 304 to a matching `If-None-Match` (`caching::conditional_get`, #499) and
/// 500.
const ROUTER_RESPONSES: [u16; 2] = [304, 500];

/// The OpenAPI response of an error `status`: `description` and the body
/// every JSON error carries, the `exception` schema. A 400 may also be the
/// framework's text/plain message (see [`SHARED_RESPONSES`]).
pub(crate) fn error_response(status: u16, description: &str) -> serde_json::Value {
    let mut content = json!({
        "application/json": {"schema": {"$ref": "#/components/schemas/exception"}}
    });
    if status == 400 {
        content["text/plain"] = json!({"schema": {"type": "string"}});
    }
    json!({"description": description, "content": content})
}

/// A reference to the shared response of `status` (dangling for a status
/// outside [`SHARED_RESPONSES`], which the `/api` tests catch).
fn shared_response(status: u16) -> serde_json::Value {
    let name = SHARED_RESPONSES
        .iter()
        .find(|(s, _, _)| *s == status)
        .map_or_else(|| status.to_string(), |(_, name, _)| name.to_string());
    json!({"$ref": format!("#/components/responses/{name}")})
}

/// `components.responses`: [`SHARED_RESPONSES`] as response objects.
fn shared_responses_component() -> serde_json::Value {
    let responses: serde_json::Map<String, serde_json::Value> = SHARED_RESPONSES
        .iter()
        .map(|&(status, name, description)| {
            let response = match status {
                304 => json!({"description": description}),
                _ => error_response(status, description),
            };
            (name.to_string(), response)
        })
        .collect();
    serde_json::Value::Object(responses)
}

/// An operation's `responses`: `ok` as its 200 and the shared response of
/// each of `errors`. [`document_router_responses`] adds [`ROUTER_RESPONSES`].
fn responses(ok: serde_json::Value, errors: &[u16]) -> serde_json::Value {
    let mut responses = serde_json::Map::new();
    responses.insert("200".into(), ok);
    for &status in errors {
        responses.insert(status.to_string(), shared_response(status));
    }
    serde_json::Value::Object(responses)
}

/// Add [`ROUTER_RESPONSES`] to every operation in `paths` that does not
/// describe them itself, so a new route cannot leave them out.
fn document_router_responses(paths: &mut serde_json::Value) {
    let operations = paths
        .as_object_mut()
        .into_iter()
        .flat_map(|paths| paths.values_mut())
        .filter_map(serde_json::Value::as_object_mut)
        .flat_map(|item| item.values_mut());
    for operation in operations {
        if let Some(responses) = operation
            .get_mut("responses")
            .and_then(serde_json::Value::as_object_mut)
        {
            for status in ROUTER_RESPONSES {
                responses
                    .entry(status.to_string())
                    .or_insert_with(|| shared_response(status));
            }
        }
    }
}

/// The `200` of a metadata resource: JSON, or HTML with `f=html`.
fn metadata_ok(description: &str) -> serde_json::Value {
    json!({
        "description": description,
        "content": {
            "application/json": {"schema": {"type": "object"}},
            "text/html": {"schema": {"type": "string"}}
        }
    })
}

/// OpenAPI `f` parameter of a data query offering `formats` (#929): the one
/// place its `enum` is built.
fn data_format_parameter(formats: &[EdrFormat]) -> serde_json::Value {
    let described: Vec<&str> = formats
        .iter()
        .map(|f| match f {
            EdrFormat::CoverageJson => "CoverageJSON (the default)",
            EdrFormat::GeoJson => {
                "GeoJSON (EDR GeoJSON: one feature per station, its series as \
                 `time` and per-parameter property arrays)"
            }
            EdrFormat::Png => "PNG (a vertical-profile / time-series plot)",
            EdrFormat::Html => "HTML (a page of the response)",
        })
        .collect();
    let names: Vec<&str> = formats.iter().map(|f| f.name()).collect();
    json!({
        "name": "f",
        "in": "query",
        "required": false,
        "description": format!(
            "Output format: {}. Case-insensitive; the media types are accepted too \
             (encode + as %2B). Without f, the Accept header chooses among them.",
            described.join(", ")
        ),
        "schema": {"type": "string", "enum": names}
    })
}

/// OpenAPI `f` parameter of `/locations/{locationId}`: a list of ids
/// answers every format but PNG.
fn locations_format_parameter(formats: &[EdrFormat]) -> serde_json::Value {
    let mut param = data_format_parameter(formats);
    if let Some(description) = param["description"].as_str() {
        param["description"] = json!(format!(
            "{description} PNG plots one location: for a list of locations it is a 400, \
             and every other format, GeoJSON included, answers the whole list."
        ));
    }
    param
}

/// OpenAPI `200` content of a data query offering `formats`.
fn data_response_content(formats: &[EdrFormat]) -> serde_json::Value {
    let mut content = serde_json::Map::new();
    for format in formats {
        let schema = match format {
            EdrFormat::CoverageJson => json!({"$ref": "#/components/schemas/coverageJSON"}),
            EdrFormat::GeoJson => {
                json!({"$ref": "#/components/schemas/edrFeatureCollectionGeoJSON"})
            }
            EdrFormat::Png => json!({"type": "string", "format": "binary"}),
            EdrFormat::Html => json!({"type": "string"}),
        };
        content.insert(format.media_type().into(), json!({ "schema": schema }));
    }
    serde_json::Value::Object(content)
}

/// The OpenAPI operation of a cube query (#925), on the collection or, with
/// `instance_id_param`, on one of its model runs. The parameters are the
/// EDR 1.2 cube parameters; the description states what MeteoCore accepts
/// of each.
fn cube_operation(
    summary: String,
    operation_id: String,
    tag: &str,
    instance_id_param: Option<serde_json::Value>,
    formats: &[EdrFormat],
) -> serde_json::Value {
    let mut parameters: Vec<serde_json::Value> = instance_id_param.into_iter().collect();
    parameters.extend([
        json!({"$ref": "#/components/parameters/cube-bbox"}),
        json!({"$ref": "#/components/parameters/cube-z"}),
        json!({"$ref": "#/components/parameters/datetime"}),
        json!({"$ref": "#/components/parameters/parameter-name"}),
        json!({"$ref": "#/components/parameters/resolution-x"}),
        json!({"$ref": "#/components/parameters/resolution-y"}),
        json!({"$ref": "#/components/parameters/resolution-z"}),
        json!({"$ref": "#/components/parameters/crs"}),
        data_format_parameter(formats),
    ]);
    json!({
        "get": {
            "summary": summary,
            "description": format!(
                "Return the data values for the data cube defined by the query parameters, as a \
                 CoverageJSON Grid with x, y, z and t axes. bbox is CRS84: four numbers, or six \
                 whose vertical pair is a z interval that an explicit z overrides. z takes a \
                 level, a list, a closed or open interval or a recurring Rn/min/step sequence of \
                 the advertised levels; without it every level is returned. A datetime list runs \
                 the cube once per instant and joins the grids along t. resolution-x, \
                 resolution-y and resolution-z ask for that many evenly spaced positions from the \
                 bbox edges (the lowest and highest selected level for z), both included, each \
                 taking the nearest native value; 0 or absent is the native resolution, and the \
                 largest accepted value is {max}. A response holds at most {max} values across \
                 timesteps, levels, cells and parameters. crs accepts CRS84 only. Unknown or \
                 repeated query parameters return 400.",
                max = crate::params::MAX_RESOLUTION
            ),
            "operationId": operation_id,
            "tags": [tag],
            "parameters": parameters,
            "responses": responses(
                json!({"description": "Coverage data", "content": data_response_content(formats)}),
                QUERY_ERRORS,
            )
        }
    })
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
        let station_series = state
            .engines
            .get(id)
            .is_some_and(|e| e.serves_station_series());
        let trajectory_shape = state
            .engines
            .get(id)
            .map_or(TrajectoryShape::AlongPath, |e| e.trajectory_shape());
        let formats =
            |query_type: &str| query_formats(query_type, station_series, trajectory_shape);

        // Collection detail
        let detail_path = format!("/edr/collections/{id}");
        collection_paths[&detail_path] = json!({
            "get": {
                "summary": format!("Get {} collection metadata", config.title),
                "operationId": format!("getCollection_{id}"),
                "tags": [id],
                "parameters": [format_parameter()],
                "responses": responses(metadata_ok("Collection metadata"), METADATA_ERRORS)
            }
        });

        // Locations, gated like every data query: an engine without
        // location support has neither path, and both routes answer 404.
        if supported.contains("locations") {
            let locations_path = format!("/edr/collections/{id}/locations");
            let mut list_responses = responses(
                json!({
                    "description": "Locations in GeoJSON format: the complete inventory, or the locations inside bbox with an observation in datetime; with limit one page of that list carrying numberMatched, numberReturned and self/next/prev links",
                    "content": {
                        "application/geo+json": {
                            "schema": {"$ref": "#/components/schemas/edrFeatureCollectionGeoJSON"}
                        },
                        "text/html": {"schema": {"type": "string"}}
                    }
                }),
                QUERY_ERRORS,
            );
            list_responses["503"] = error_response(
                503,
                "The query executor is at capacity (ServerBusy), or the list is over the response budget (ResponseLimit): page a list over it with limit, or request a smaller limit",
            );
            collection_paths[&locations_path] = json!({
                "get": {
                    "summary": format!("Get locations for {}", config.title),
                    "operationId": format!("getLocations_{id}"),
                    "tags": [id],
                    "parameters": [
                        {"$ref": "#/components/parameters/bbox-locations"},
                        {"$ref": "#/components/parameters/datetime-locations"},
                        {"$ref": "#/components/parameters/limit-locations"},
                        {"$ref": "#/components/parameters/offset-locations"},
                        {"$ref": "#/components/parameters/f-locations"}
                    ],
                    "responses": list_responses
                }
            });

            // Location data query. `locationId` per EDR 1.2
            // `/req/edr/REQ_rc-locationid-definition` (#923), as the 1.2
            // bundle writes it: the requirement's fragment puts
            // `style`/`explode` in the schema and says `required: false`,
            // neither valid for an OpenAPI path parameter.
            let location_path = format!("/edr/collections/{id}/locations/{{locationId}}");
            let mut location_responses = responses(
                json!({
                    "description": "Coverage data",
                    "content": data_response_content(formats("locations"))
                }),
                QUERY_ERRORS,
            );
            location_responses["204"] = json!({"description": "A list of locations, none of which has data in the requested window"});
            location_responses["400"] = error_response(
                400,
                &format!("Bad request, including an empty element in or more than {MAX_LOCATION_IDS} ids in locationId, more than {MAX_LOCATION_LOOKUPS} ids × datetime instants, and PNG for a list"),
            );
            location_responses["404"] = error_response(
                404,
                "Location not found: an unknown id, or one id without data in the requested window",
            );
            collection_paths[&location_path] = json!({
                "get": {
                    "summary": format!("Get data for one or more locations in {}", config.title),
                    "operationId": format!("getLocationData_{id}"),
                    "tags": [id],
                    "parameters": [
                        {
                            "name": "locationId",
                            "in": "path",
                            "required": true,
                            "description": format!("Comma-delimited list of location ids (EGLL or EGLL,EFHK), from the /locations inventory. At most {MAX_LOCATION_IDS}, and with a datetime list at most {MAX_LOCATION_LOOKUPS} ids × instants; a repeated id is answered once. A literal comma separates ids, so a comma inside an id is sent encoded as %2C. One id answers as before: a Coverage or CoverageCollection, 404 when it has no data in the window. A list answers one CoverageCollection with every id's coverages in request order, or on a station collection one EDR GeoJSON FeatureCollection with every id's features in request order, each named by its id; an id without data in the window contributes none; any unknown id is a 404 naming it; PNG is a 400 for a list."),
                            "schema": {"type": "string"},
                            "style": "simple",
                            "explode": false
                        },
                        {"$ref": "#/components/parameters/datetime"},
                        {"$ref": "#/components/parameters/parameter-name"},
                        {"$ref": "#/components/parameters/z"},
                        {"$ref": "#/components/parameters/crs"},
                        locations_format_parameter(formats("locations")),
                        {"$ref": "#/components/parameters/limit"}
                    ],
                    "responses": location_responses
                }
            });
        }

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
                        {"$ref": "#/components/parameters/crs"},
                        data_format_parameter(formats("position")),
                        {"$ref": "#/components/parameters/limit"}
                    ],
                    "responses": responses(
                        json!({"description": "Coverage data", "content": data_response_content(formats("position"))}),
                        QUERY_ERRORS,
                    )
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
                        {"$ref": "#/components/parameters/crs"},
                        data_format_parameter(formats("area")),
                        {"$ref": "#/components/parameters/limit"}
                    ],
                    "responses": responses(
                        json!({"description": "Coverage data", "content": data_response_content(formats("area"))}),
                        QUERY_ERRORS,
                    )
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
                        {"$ref": "#/components/parameters/crs"},
                        data_format_parameter(formats("radius")),
                        {"$ref": "#/components/parameters/limit"}
                    ],
                    "responses": responses(
                        json!({"description": "Coverage data", "content": data_response_content(formats("radius"))}),
                        QUERY_ERRORS,
                    )
                }
            });
        }

        // Cube query (#925). Gated like every data query: only engines
        // advertising `cube` get the path (#668).
        if supported.contains("cube") {
            let cube_path = format!("/edr/collections/{id}/cube");
            collection_paths[&cube_path] = cube_operation(
                format!("Cube query for {}", config.title),
                format!("getCube_{id}"),
                id,
                None,
                formats("cube"),
            );
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
            // The formats this shape offers, the ones the handler negotiates.
            let trajectory_formats = formats("trajectory");
            let trajectory_responses = |description: &str| {
                responses(
                    json!({"description": description, "content": data_response_content(trajectory_formats)}),
                    QUERY_ERRORS,
                )
            };
            collection_paths[&trajectory_path] = match trajectory_shape {
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
                            {"$ref": "#/components/parameters/z"},
                            {"$ref": "#/components/parameters/crs"},
                            data_format_parameter(trajectory_formats)
                        ],
                        "responses": trajectory_responses(
                            "Coverage data: a CoverageJSON Trajectory coverage, or a CoverageCollection of them",
                        )
                    }
                }),
                TrajectoryShape::CrossSection => {
                    let mut format = data_format_parameter(trajectory_formats);
                    format["description"] = json!("Output format: CoverageJSON (default), PNG (a colour-mapped distance×height cross-section heatmap) or HTML (a page of the response). Case-insensitive; the media types are accepted too (encode + as %2B). Without f, the Accept header chooses among them.");
                    json!({
                        "get": {
                            "summary": format!("Trajectory cross-section for {}", config.title),
                            "operationId": format!("getTrajectory_{id}"),
                            "tags": [id],
                            "parameters": [
                                {"$ref": "#/components/parameters/coords-linestring"},
                                {"$ref": "#/components/parameters/datetime"},
                                {"$ref": "#/components/parameters/parameter-name"},
                                {"$ref": "#/components/parameters/z-trajectory"},
                                {"$ref": "#/components/parameters/crs"},
                                format
                            ],
                            "responses": trajectory_responses(
                                "Coverage data — CoverageJSON Section domain or PNG heatmap. The Section domain carries the per-node lowest-beam coverage floor (metres above antenna) in the `meteocore:beamCoverage` foreign member; the PNG draws it as a hatched-below overlay line. Below the floor the volume is unobserved, not echo-free.",
                            )
                        }
                    })
                }
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
                "description": "Forecast model run: its reference time in RFC 3339, e.g. 2026-06-07T06:00:00Z. The compact form 20260607T0600Z is also accepted.",
                "schema": {"type": "string"},
                "example": "2026-06-07T06:00:00Z"
            });
            let instances_path = format!("/edr/collections/{id}/instances");
            collection_paths[&instances_path] = json!({
                "get": {
                    "summary": format!("List model runs (instances) for {}", config.title),
                    "operationId": format!("getInstances_{id}"),
                    "tags": [id],
                    "parameters": [format_parameter()],
                    "responses": responses(
                        metadata_ok("Available instances (model runs)"),
                        METADATA_ERRORS,
                    )
                }
            });
            let instance_path = format!("/edr/collections/{id}/instances/{{instanceId}}");
            collection_paths[&instance_path] = json!({
                "get": {
                    "summary": format!("Get one model run's metadata for {}", config.title),
                    "operationId": format!("getInstance_{id}"),
                    "tags": [id],
                    "parameters": [instance_id_param.clone(), format_parameter()],
                    "responses": responses(
                        metadata_ok("Instance (model run) metadata"),
                        METADATA_ERRORS,
                    )
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
                            {"$ref": "#/components/parameters/crs"},
                            data_format_parameter(formats("position")),
                            {"$ref": "#/components/parameters/limit"}
                        ],
                        "responses": responses(
                            json!({"description": "Coverage data", "content": data_response_content(formats("position"))}),
                            QUERY_ERRORS,
                        )
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
                            {"$ref": "#/components/parameters/crs"},
                            data_format_parameter(formats("radius")),
                            {"$ref": "#/components/parameters/limit"}
                        ],
                        "responses": responses(
                            json!({"description": "Coverage data", "content": data_response_content(formats("radius"))}),
                            QUERY_ERRORS,
                        )
                    }
                });
            }
            if supported.contains("cube") {
                let p = format!("/edr/collections/{id}/instances/{{instanceId}}/cube");
                collection_paths[&p] = cube_operation(
                    format!("Cube query against a model run for {}", config.title),
                    format!("getInstanceCube_{id}"),
                    id,
                    Some(instance_id_param.clone()),
                    formats("cube"),
                );
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
                            {"$ref": "#/components/parameters/crs"},
                            data_format_parameter(formats("area")),
                            {"$ref": "#/components/parameters/limit"}
                        ],
                        "responses": responses(
                            json!({"description": "Coverage data", "content": data_response_content(formats("area"))}),
                            QUERY_ERRORS,
                        )
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
                "responses": responses(metadata_ok("Landing page"), &[400])
            }
        },
        "/edr/conformance": {
            "get": {
                "summary": "Conformance classes",
                "operationId": "getConformance",
                "tags": [api_common::openapi_tags::DISCOVERY],
                "parameters": [format_parameter()],
                "responses": responses(metadata_ok("Conformance classes"), &[400])
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
    document_router_responses(&mut paths);

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
                    "style": "form",
                    "explode": false,
                    "description": "Either a date-time, an interval (open or closed), a list of date-times, or a repeating interval. Date and time expressions adhere to RFC 3339; open intervals use double dots. Examples: 2018-02-12T23:20:50Z; 2018-02-12T00:00:00Z/2018-03-18T12:31:12Z; 2018-02-12T00:00:00Z/.. or ../2018-03-18T12:31:12Z; 2018-02-12T00:00:00Z,2018-02-12T01:00:00Z,2018-02-14T12:00:00Z; R4/2018-02-12T00:00:00Z/PT6H. A list names at most 16 instants, each matched exactly as a request for that instant alone would be; the answers merge into one response (a series gains every instant's steps) bounded to 1000000 values, and an instant with no data contributes nothing. A repeating interval Rn/date-time/duration is the list of its n instants, the start and then one ISO 8601 duration apart: R4/2018-02-12T00:00:00Z/PT6H is 00:00, 06:00, 12:00 and 18:00 on 2018-02-12. n is 1 to 16; the duration is positive and in weeks or days, hours, minutes and whole seconds, since calendar years and months have no fixed length."
                },
                "parameter-name": {
                    "name": "parameter-name",
                    "in": "query",
                    "required": false,
                    "schema": {"type": "string"},
                    "style": "form",
                    "explode": false,
                    "description": "Comma-separated list of parameter names to include"
                },
                "z": {
                    "name": "z",
                    "in": "query",
                    "required": false,
                    "schema": {"type": "string"},
                    "style": "form",
                    "explode": false,
                    "description": format!("Vertical level selector. Forms: z=850 (one level); z=10,80,200 (a list); z=100/550 (every advertised level between and including the two); z=../850 or z=500/.. (open intervals, reaching the lowest or highest advertised level); z=R20/100/50 (20 levels 50 apart starting at 100, treated as a list). A list or recurring interval names at most {} levels; more is a 400. A single level or list is matched against the collection's advertised vertical extent by the collection's engine, which keeps only the levels it has: an exact level, or on radar volumes a sweep within 0.05° of the requested elevation angle (the response domain reports the level served). An interval or list that matches no level is a 400. A collection with no vertical extent ignores z, but a malformed z is still a 400.", crate::params::MAX_Z_LEVELS)
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
                // The cube parameters, copied from the OGC API - EDR 1.2
                // OpenAPI (`cube-bbox`, `cube-z`, `resolution-x/-y/-z`,
                // `crs`). The resolution parameters add the `style`/`explode`
                // their requirement classes declare; `crs` gives CRS84, the
                // one accepted value, as its example instead of `native`.
                "cube-bbox": {
                    "name": "bbox",
                    "in": "query",
                    "description": "Only features that have a geometry that intersects the bounding box are selected.\nThe bounding box is provided as four numbers:\n* Lower left corner, coordinate axis 1\n* Lower left corner, coordinate axis 2\n* Upper right corner, coordinate axis 1\n* Upper right corner, coordinate axis 2\n\nFor WGS 84 longitude/latitude the values are in most cases the sequence of\nminimum longitude, minimum latitude, maximum longitude and maximum latitude.\nHowever, in cases where the box spans the antimeridian the first value\n(west-most box edge) is larger than the third value (east-most box edge).\nIf a feature has multiple spatial geometry properties, it is the decision of the\nserver whether only a single spatial geometry property is used to determine\nthe extent or all relevant geometries.",
                    "required": true,
                    "schema": {
                        "oneOf": [
                            {"items": {"type": "number"}, "type": "array", "minItems": 4, "maxItems": 4},
                            {"items": {"type": "number"}, "type": "array", "minItems": 6, "maxItems": 6}
                        ]
                    },
                    "style": "form",
                    "explode": false
                },
                "cube-z": {
                    "name": "z",
                    "in": "query",
                    "description": "Define the vertical levels to return data from \n\nThe value will override any vertical values defined in the BBOX query parameter \n\nA range to return data for all levels between and including 2 defined levels\n\ni.e. z=minimum value/maximum value\n\nfor instance if all values between and including 10m and 100m\n\nz=10/100\n\nA list of height values can be specified\ni.e. z=value1,value2,value3\n\nfor instance if values at 2m, 10m and 80m are required\n\nz=2,10,80\n\nAn Arithmetic sequence using Recurring height intervals, the difference is the number of recurrences is defined at the start \nand the amount to increment the height by is defined at the end\n\ni.e. z=Rn/min height/height interval\n\nso if the request was for 20 height levels 50m apart starting at 100m:\n\nz=R20/100/50\n\nWhen not specified data from all available heights SHOULD be returned\n",
                    "required": false,
                    "schema": {"type": "string"}
                },
                "resolution-x": {
                    "name": "resolution-x",
                    "in": "query",
                    "description": "Defined if the user requires data at a different resolution from the native resolution of the data along the x-axis\n\nThis is a single value it denotes the number of intervals to retrieve data for along the x-axis\n  \n  i.e. resolution-x=10 \n  \nwould retrieve 10 values along the x-axis from the minimum x coordinate to maximum x coordinate (i.e. a value at both the minimum x and maximum x coordinates and 8 values between).\n",
                    "required": false,
                    "schema": {"type": "string"},
                    "style": "form",
                    "explode": false
                },
                "resolution-y": {
                    "name": "resolution-y",
                    "in": "query",
                    "description": "Defined if the user requires data at a different resolution from the native resolution of the data along the y-axis\n\nThis is a single value it denotes the number of intervals to retrieve data for along the y-axis\n  \n  i.e. resolution-y=10 \n  \nwould retrieve 10 values along the y-axis from the minimum y coordinate to maximum y coordinate (i.e. a value at both the minimum y and maximum y coordinates and 8 values between).\n",
                    "required": false,
                    "schema": {"type": "string"},
                    "style": "form",
                    "explode": false
                },
                "resolution-z": {
                    "name": "resolution-z",
                    "in": "query",
                    "description": "Defined if the user requires data at a different resolution from the native resolution of the data along the z-axis\n\nThis is a single value it denotes the number of intervals to retrieve data for along the z-axis\n  \n  i.e. resolution-z=10 \n  \nwould retrieve 10 values along the z-axis from the minimum z coordinate to maximum z  coordinate (i.e. a value at both the minimum z and maximum z coordinates and 8 values between).\n",
                    "required": false,
                    "schema": {"type": "string"},
                    "style": "form",
                    "explode": false
                },
                // EDR 1.2 `/req/edr/REQ_rc-crs-definition`, on every data
                // query: the requirement's `style` and `explode`, the
                // bundle's description, and what this server accepts.
                "crs": {
                    "name": "crs",
                    "in": "query",
                    "description": "identifier (id) of the coordinate system to return data in list of valid crs identifiers for the chosen collection are defined in the metadata responses.  If not supplied the coordinate reference system will default to WGS84. This server serves CRS84 only, the crs_details of every data query: its OGC URI, CRS84 or OGC:CRS84 are accepted, any other value is a 400.",
                    "required": false,
                    "example": crate::params::CRS84,
                    "schema": {"type": "string"},
                    "style": "form",
                    "explode": false
                },
                // `f` on `/locations`: GeoJSON or its HTML page (#971).
                "f-locations": {
                    "name": "f",
                    "in": "query",
                    "required": false,
                    "schema": {"type": "string", "enum": LOCATIONS_FORMATS},
                    "description": "Output format: GeoJSON, the default, or HTML (case-insensitive; encode the plus sign as %2B in application/geo+json). Without f, the Accept header chooses. Any other value is a 400."
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
                // The EDR 1.2 `bbox` and `datetime` parameters of
                // `/locations`, schema, `style` and `explode` copied from the
                // 1.2 OpenAPI; the descriptions say what this server does
                // with them (CRS84 only, heights ignored, the observation
                // rule of `location_time_filter`).
                "bbox-locations": {
                    "name": "bbox",
                    "in": "query",
                    "description": "Only locations whose point lies inside the bounding box, edges included, are listed; the list is filtered before it is paged, so numberMatched and the paging links count the locations inside the box.\nThe bounding box is provided as four or six numbers:\n* Lower left corner, coordinate axis 1\n* Lower left corner, coordinate axis 2\n* Minimum value, coordinate axis 3 (optional)\n* Upper right corner, coordinate axis 1\n* Upper right corner, coordinate axis 2\n* Maximum value, coordinate axis 3 (optional)\nThe coordinate reference system of the values is WGS 84 longitude/latitude (http://www.opengis.net/def/crs/OGC/1.3/CRS84); bbox-crs is not supported.\nFor WGS 84 longitude/latitude the values are in most cases the sequence of\nminimum longitude, minimum latitude, maximum longitude and maximum latitude.\nHowever, in cases where the box spans the antimeridian the first value\n(west-most box edge) is larger than the third value (east-most box edge).\nLocations are points without a height: the third and sixth numbers of a six-number box must be numbers and are otherwise ignored. A malformed box is a 400.",
                    "required": false,
                    "schema": {
                        "oneOf": [
                            {"items": {"type": "number"}, "type": "array", "minItems": 4, "maxItems": 4},
                            {"items": {"type": "number"}, "type": "array", "minItems": 6, "maxItems": 6}
                        ]
                    },
                    "style": "form",
                    "explode": false
                },
                "datetime-locations": {
                    "name": "datetime",
                    "in": "query",
                    "description": "Either a date-time, an interval (open or closed), or a list of date-times, in the grammar of the data queries' datetime. Date and time expressions adhere to RFC 3339; open intervals use double dots. Examples: 2018-02-12T23:20:50Z; 2018-02-12T00:00:00Z/2018-03-18T12:31:12Z; 2018-02-12T00:00:00Z/.. or ../2018-03-18T12:31:12Z; 2018-02-12T00:00:00Z,2018-02-12T01:00:00Z. Only locations with at least one observation in the interval are listed, each date-time of a list matched exactly; the list is filtered before it is paged, so numberMatched and the paging links count those locations, and the links repeat datetime. A malformed value, or an interval that ends before it starts, is a 400, and so is any datetime on a collection whose locations carry no per-location time.",
                    "required": false,
                    "schema": {"type": "string"},
                    "style": "form",
                    "explode": false
                },
                "z-trajectory": {
                    "name": "z",
                    "in": "query",
                    "required": false,
                    "schema": {"type": "string"},
                    "style": "form",
                    "explode": false,
                    "description": "Elevation-angle selection for the cross-section, matching the collection's advertised vertical extent (sweep angles in degrees). Forms: z=5 (one sweep), z=0.5,1.5,5 (a list), z=0.3/15 (a min/max interval → every advertised angle in range), z=../5 or z=5/.. (open intervals, reaching the lowest or highest advertised angle), or z=R4/0.5/1 (4 angles 1° apart from 0.5°, treated as a list). The selected angle window bounds which sweeps build the RHI; the rendered z axis is derived height above the antenna (metres). Absent → all sweeps."
                }
            },
            "responses": shared_responses_component(),
            "schemas": {
                // The body of every JSON error this API sends: `code` and
                // `description` always, as `JsonError` builds them. The
                // EDR 1.2 bundle's `exception` requires only `code`.
                "exception": {
                    "type": "object",
                    "required": ["code", "description"],
                    "properties": {
                        "code": {
                            "type": "string",
                            "description": "The error kind: BadRequest, NotFound, ServerError, ServerBusy, ResponseLimit or Timeout"
                        },
                        "description": {
                            "type": "string",
                            "description": "What went wrong, for a person; a 500 carries no internal details"
                        }
                    }
                },
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
                },
                "edrFeatureCollectionGeoJSON": {
                    "type": "object",
                    "description": "EDR GeoJSON FeatureCollection: one Point feature per location, whose properties carry the EDR members (datetime, label, parameter-name, edrqueryendpoint). A station series adds its RFC 3339 instants as `time` and one array per parameter aligned with `time` (null where there is no value); the /locations list has neither.",
                    "required": ["type", "features"],
                    "properties": {
                        "type": {"type": "string", "enum": ["FeatureCollection"]},
                        "features": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "required": ["type", "geometry", "properties"],
                                "properties": {
                                    "type": {"type": "string", "enum": ["Feature"]},
                                    "id": {"type": "string"},
                                    "geometry": {"type": "object"},
                                    "properties": {
                                        "type": "object",
                                        "required": ["datetime", "parameter-name", "label", "edrqueryendpoint"],
                                        "properties": {
                                            "datetime": {"type": "string"},
                                            "parameter-name": {"type": "array", "items": {"type": "string"}},
                                            "label": {"type": "string"},
                                            "edrqueryendpoint": {"type": "string"},
                                            "time": {"type": "array", "items": {"type": "string"}}
                                        }
                                    }
                                }
                            }
                        },
                        "parameters": {"type": "array", "items": {"type": "object"}},
                        "links": {"type": "array", "items": {"type": "object"}},
                        "numberMatched": {"type": "integer", "minimum": 0},
                        "numberReturned": {"type": "integer", "minimum": 0}
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

    ([(header::CONTENT_TYPE, OPENAPI_MEDIA_TYPE)], Json(openapi))
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
    // data_queries identifies supported types. Feature content is GeoJSON:
    // the /locations list, and the point queries of station collections,
    // whose `output_formats` list GeoJSON (#929).
    let classes = api_common::conformance_classes(&[
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/core",
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/collections",
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/queries",
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/json",
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/geojson",
        "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/edr-geojson",
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
            let metadata = html_document(build_collection_metadata(
                engine.as_ref(),
                config,
                base,
                None,
                items,
            ));
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
            // Each run's data queries, anchored under its card (#971).
            let docs: Vec<serde_json::Value> = runs
                .iter()
                .map(|run| {
                    build_collection_metadata(engine.as_ref(), config, base, Some(run), false)
                })
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
                &docs,
                &nav,
            ))
            .into_response()
        }
    }))
}

/// The HTML card for one model run: id = the instance id, title = the run's
/// reference time (the same RFC 3339 string), description = the valid-time
/// span.
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
        title: format!("Run {instance_id}"),
        description,
        // The id's colons stay unencoded: RFC 3986 `pchar` allows `:`.
        self_href: format!(
            "{base}/edr/collections/{}/instances/{instance_id}",
            config.id
        ),
        id: instance_id,
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
                "description": format!("Invalid instance id '{instance_id}' (expected an RFC 3339 reference time like 2026-06-07T06:00:00Z)")
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
            let metadata = html_document(build_collection_metadata(
                engine.as_ref(),
                config,
                base,
                Some(&run),
                false,
            ));
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
/// `prev` links paged like `/collections` (#922). `bbox` and `datetime`
/// filter the list before it is paged, so the counts and links describe the
/// filtered list (#932). Either way the encoded body is admitted by the same
/// byte budget, and so is its HTML page (#971).
pub async fn locations(
    Path(id): Path<String>,
    State(state): State<AppState>,
    query: Result<Query<Vec<(String, String)>>, axum::extract::rejection::QueryRejection>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
) -> Result<Response, HandlerError> {
    let state = state.load_full();
    let (engine, config) = lookup_collection(&state, &id)?;
    require_query_type(engine, &id, "locations", "location")?;
    let Query(pairs) = query.map_err(|_| bad_request_msg("Invalid query string"))?;
    let request = parse_locations_query(pairs).map_err(|e| bad_request(&e))?;
    let f = request
        .preserved
        .iter()
        .find(|(name, _)| name == "f")
        .map(|(_, value)| value.as_str());
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok());
    let format =
        negotiate_list_format(f, accept, "the location list").map_err(|e| bad_request(&e))?;
    let collection_title = config.title.clone();

    let base_url = request_base_url(&state, &headers);
    let query_engine = engine.clone();
    let (body, etag) = execute_query(false, move |budget| {
        let server_error = || {
            JsonError(
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "code": "ServerError", "description": "Internal server error" })),
            )
        };
        let mut locs = query_engine.get_locations().map_err(|_| server_error())?;
        // Filter first, then page: `numberMatched` and the links count the
        // locations left. `contains` handles `west > east`; the box goes
        // first, so the time filter probes only the locations inside it.
        if let Some(bbox) = &request.bbox {
            locs.retain(|loc| bbox.contains(loc.longitude, loc.latitude));
        }
        if let Some(datetime) = &request.datetime {
            let intervals = datetime.intervals();
            // Built after `get_locations`, never around it: the filter may
            // hold a read guard on the engine's index.
            let Some(has_data) = query_engine.location_time_filter(&intervals) else {
                return Err(JsonError(
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "code": "BadRequest",
                        "description": format!(
                            "Collection '{id}' cannot filter its locations by datetime: \
                             they carry no per-location time. Leave datetime out to list them all"
                        )
                    })),
                ));
            };
            locs.retain(|loc| has_data(loc));
        }
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
        let href = format!("{base_url}/edr/collections/{id}/locations");
        let mut links = Vec::new();
        let (page_locs, number_matched) = match &request.paging {
            // The whole list. A filtered one names its query in `self`; an
            // unfiltered one keeps the complete inventory's body.
            None => {
                if request.is_filtered() {
                    links.push((request.href(&href, 0), "self", "Locations"));
                }
                (&locs[..], None)
            }
            // One page: the /collections paging arithmetic over the list.
            Some(paging) => {
                let window = ds_core::collection_search::page_window(
                    locs.len(),
                    paging.offset,
                    paging.limit,
                );
                links.push((request.href(&href, paging.offset), "self", "This page"));
                if window.has_next {
                    links.push((request.href(&href, window.next_offset), "next", "Next page"));
                }
                if window.has_prev {
                    links.push((
                        request.href(&href, window.prev_offset),
                        "prev",
                        "Previous page",
                    ));
                }
                (&locs[window.range()], Some(locs.len()))
            }
        };
        // What a `ResponseLimit` names: a page, a filtered list or the
        // complete inventory (#961).
        let too_large = if request.paging.is_some() {
            "Location page exceeds the configured response limit; request a smaller limit"
        } else if request.is_filtered() {
            "Filtered location list exceeds the configured response limit; page it with limit"
        } else {
            "Complete location inventory exceeds the configured response limit; page it with limit"
        };
        // Keep construction, serialization and hashing under the same worker
        // permit as retrieval, even if the request times out or disconnects.
        let cancelled = || budget.expired();
        let mut writer = crate::location_budget::Writer::new(&cancelled);
        let written = if format.format == EdrFormat::Html {
            let page = locations_html_page(
                &href,
                &links,
                raw_query.as_deref(),
                &ctx,
                &collection_title,
            );
            let counts = number_matched.map(|matched| (matched, page_locs.len()));
            crate::html::write_locations(page_locs, &ctx, &page, counts, &mut writer)
        } else {
            let page = (!links.is_empty()).then_some(LocationsPage {
                number_matched,
                links: &links,
            });
            locations_to_writer(page_locs, &ctx, page.as_ref(), &mut writer).map_err(Into::into)
        };
        written.map_err(|_| {
            match writer.failure {
                Some(crate::location_budget::Failure::Cancelled) => query_timeout(),
                Some(crate::location_budget::Failure::Limit) => JsonError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({"code": "ResponseLimit", "description": too_large})),
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
    let content_type = match format.format {
        EdrFormat::Html => crate::html::CONTENT_TYPE,
        _ => "application/geo+json",
    };
    Ok(with_format_vary(
        (
            [
                (header::CONTENT_TYPE, content_type.to_owned()),
                (header::ETAG, etag),
            ],
            body,
        )
            .into_response(),
        format,
    ))
}

/// The `/locations` HTML page's context (#971): the GeoJSON's `self`,
/// `next` and `prev` links as HTML pages, the GeoJSON as `alternate`.
fn locations_html_page<'a>(
    href: &str,
    links: &[(String, &'static str, &'static str)],
    raw_query: Option<&'a str>,
    ctx: &LocationsContext<'a>,
    collection_title: &'a str,
) -> crate::html::DataPage<'a> {
    let json_url = links
        .iter()
        .find(|(_, rel, _)| *rel == "self")
        .map_or(href, |(href, _, _)| href.as_str());
    let json_url = api_common::workbench::with_format(json_url, "GeoJSON");
    let mut page_links = vec![crate::html::Link {
        href: json_url.clone(),
        rel: "alternate".into(),
        kind: "application/geo+json".into(),
        title: "This document as GeoJSON".into(),
    }];
    let own: Vec<(String, &str, &str)> = if links.is_empty() {
        vec![(href.to_owned(), "self", "Locations")]
    } else {
        links.to_vec()
    };
    for (href, rel, title) in own {
        page_links.push(crate::html::Link {
            href: api_common::workbench::with_format(&href, "html"),
            rel: rel.into(),
            kind: "text/html".into(),
            title: title.into(),
        });
    }
    crate::html::DataPage {
        base: ctx.base_url,
        collection_id: ctx.collection_id,
        collection_title,
        title: "Locations".into(),
        raw_query,
        json_url,
        links: page_links,
    }
}

/// `GET /collections/{id}/locations/{locationId}`: the data at one named
/// location or, EDR 1.2 (#923), at each of a comma-delimited list of them,
/// answered by `query_location_list`. A station collection answers both as
/// EDR GeoJSON too (#929): a list is one FeatureCollection, its features in
/// request order, each named by its own id.
pub async fn location_query(
    Path((id, loc_id)): Path<(String, String)>,
    Query(params): Query<LocationQueryParams>,
    RawQuery(raw_query): RawQuery,
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Result<Response, HandlerError> {
    let state = state.load_full();
    let (engine, config) = lookup_collection(&state, &id)?;
    require_query_type(engine, &id, "locations", "location")?;

    // Split the segment as it arrived: a literal comma separates ids, `%2C`
    // belongs to one. The decoded `loc_id` can no longer tell them apart.
    // `{locationId}` is the route's last segment.
    let segment = uri.path().rsplit('/').next().unwrap_or_default();
    let mut ids = if segment.contains(',') {
        split_location_ids(segment).map_err(|e| bad_request(&e))?
    } else {
        vec![loc_id]
    };
    let list = ids.len() > 1;

    // The plot labels series by index, which says nothing about which
    // location each one is: no PNG for a list. Its other formats stay
    // offered, GeoJSON included on a station collection, so `Accept` never
    // picks PNG for a list.
    if list && parse_edr_format(params.f.as_deref()).is_ok_and(|f| f == EdrFormat::Png) {
        return Err(bad_request_msg(
            "PNG output plots one location: request a single location id, or CoverageJSON \
             (GeoJSON on a station collection) for a list",
        ));
    }
    let offered: Vec<EdrFormat> = engine_query_formats(engine.as_ref(), "locations")
        .iter()
        .copied()
        .filter(|f| !(list && *f == EdrFormat::Png))
        .collect();
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok());
    let format = negotiate_edr_format(params.f.as_deref(), accept, &offered, "location queries")
        .map_err(|e| bad_request(&e))?;
    request_crs(params.crs.as_deref())?;

    let datetime = request_datetime(params.datetime.as_deref())?;
    let window = datetime.as_ref().map(DatetimeSelector::envelope);
    // Every listed instant re-queries every listed id: cap the product
    // before any engine call, as MULTIPOINT points × instants is.
    if let Some(DatetimeSelector::Instants(instants)) = &datetime {
        let lookups = ids.len().saturating_mul(instants.len());
        if lookups > MAX_LOCATION_LOOKUPS {
            return Err(bad_request(&DataServerError::QueryTooLarge(format!(
                "{} locations × {} datetime instants is {lookups} location lookups; \
                 the limit is {MAX_LOCATION_LOOKUPS} — name fewer locations or instants",
                ids.len(),
                instants.len(),
            ))));
        }
    }

    let param_names: Option<Vec<String>> = params
        .parameter_name
        .as_deref()
        .map(|s| s.split(',').map(|p| p.trim().to_string()).collect());

    let z = resolve_request_z(engine, params.z.as_deref())?;
    let limit = request_limit(params.limit.as_deref())?;

    let mut geojson = GeoJsonRequest {
        engine: engine.clone(),
        base: request_base_url(&state, &headers),
        collection_id: id.clone(),
        collection_title: config.title.clone(),
        // A list's links repeat its segment as the client sent it.
        path: match &ids[..] {
            [one] => format!("/collections/{id}/locations/{}", encode_path_segment(one)),
            _ => format!("/collections/{id}/locations/{segment}"),
        },
        raw_query,
        offered,
        location_ids: Vec::new(),
    };
    let engine = engine.clone();
    if !list {
        // One id, a repeat-only list included: the response it always was.
        let loc_id = ids.pop().unwrap_or_default();
        geojson.location_ids = vec![loc_id.clone()];
        // Rendering stays on the query executor: GeoJSON reads the engine's
        // location inventory to label the station.
        let response = execute_query(false, move |budget| {
            let result = crate::datetime_list::run(
                datetime.as_ref(),
                || budget.expired(),
                |datetime| {
                    engine.query_location(
                        &loc_id,
                        datetime,
                        param_names.as_deref(),
                        z.as_deref(),
                        None,
                    )
                },
            )
            .map_err(|e| map_query_error(&e, "Location"))?;
            let matched = coverage_count(&result);
            render_coverage_response(
                limit_coverages(result, limit),
                Some(matched),
                format.format,
                params.width,
                params.height,
                &geojson,
            )
        })
        .await?;
        return Ok(with_format_vary(
            with_data_cache_control(response, window),
            format,
        ));
    }

    let limit = limit.unwrap_or(usize::MAX);
    let response = execute_query(false, move |budget| {
        let expired = || budget.expired();
        let found = query_location_list(
            engine.as_ref(),
            &ids,
            datetime.as_ref(),
            param_names.as_deref(),
            z.as_deref(),
            limit,
            &expired,
        )?;
        if found.coverages.is_empty() {
            // `/req/edr/REQ_rc-locationid-response` C.
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
        // `limit` hid coverages it never counted (ids past it are not
        // queried): GeoJSON then leaves `numberMatched` out.
        let matched = (!found.dropped).then_some(found.coverages.len());
        geojson.location_ids = found.owners.iter().map(|&i| ids[i].clone()).collect();
        render_coverage_response(
            CoverageResponse::Collection(found.coverages),
            matched,
            format.format,
            params.width,
            params.height,
            &geojson,
        )
    })
    .await?;
    Ok(with_format_vary(
        with_data_cache_control(response, window),
        format,
    ))
}

/// What [`query_location_list`] found: every id's coverages in request
/// order, the index in `ids` of the location each belongs to, and whether
/// `limit` left any coverage out before it could be counted.
struct LocationList {
    coverages: Vec<ds_core::model::QueryResult>,
    owners: Vec<usize>,
    dropped: bool,
}

/// The locations query over a list of ids (EDR 1.2
/// `/req/edr/REQ_rc-locationid-response`, #923): every id's coverages in
/// request order, flattened into one CoverageCollection. Each id goes
/// through the engine's own `query_location`, one at a time, and a
/// `datetime` list through `datetime_list::run` per id, exactly as that id
/// alone: at most [`MAX_LOCATION_IDS`] ids and [`MAX_LOCATION_LOOKUPS`]
/// engine calls, the deadline checked before each one as between
/// MULTIPOINT points. `limit` counts the flattened coverages, and once it is
/// reached the remaining ids are not queried. [`MAX_LOCATION_VALUES`] caps
/// the values combined.
///
/// Engines answer `LocationNotFound` both for an id they do not have and for
/// one with no data in the window, at any listed instant. The collection's
/// inventory tells them apart, read only when some id needs it: an unknown
/// id fails the whole list with a 404 naming it, ids past `limit` included.
/// No coverages means every id is known and none has data, the 204.
fn query_location_list(
    engine: &dyn EdrEngine,
    ids: &[String],
    datetime: Option<&DatetimeSelector>,
    parameters: Option<&[String]>,
    z: Option<&[f64]>,
    limit: usize,
    expired: &dyn Fn() -> bool,
) -> Result<LocationList, HandlerError> {
    let mut known = None;
    let mut found = LocationList {
        coverages: Vec::new(),
        owners: Vec::new(),
        dropped: false,
    };
    let mut values = 0usize;
    for (i, id) in ids.iter().enumerate() {
        if expired() {
            return Err(query_timeout());
        }
        if found.coverages.len() >= limit {
            require_known_location(engine, &mut known, id)?;
            found.dropped = true;
            continue;
        }
        let response = crate::datetime_list::run(datetime, expired, |window| {
            engine.query_location(id, window, parameters, z, None)
        });
        let response = match response {
            Ok(response) => response,
            Err(DataServerError::LocationNotFound(_)) => {
                require_known_location(engine, &mut known, id)?;
                continue;
            }
            Err(e) => return Err(map_query_error(&e, "Location")),
        };
        let mut batch = match response {
            CoverageResponse::Single(q) => vec![q],
            CoverageResponse::Collection(v) => v,
        };
        // Drop what `limit` excludes before it counts against the budget.
        let produced = batch.len();
        batch.truncate(limit - found.coverages.len());
        found.dropped |= batch.len() < produced;
        for q in &batch {
            for range in q.ranges.values() {
                values = values.saturating_add(range.values.len());
            }
        }
        if values > MAX_LOCATION_VALUES {
            return Err(bad_request(&DataServerError::QueryTooLarge(format!(
                "Locations response exceeds {MAX_LOCATION_VALUES} values combined; \
                 list fewer locations or narrow datetime"
            ))));
        }
        found.owners.extend(std::iter::repeat_n(i, batch.len()));
        found.coverages.extend(batch);
    }
    if expired() {
        return Err(query_timeout());
    }
    Ok(found)
}

/// The 404 for a listed location id the collection does not have. `known`
/// caches the inventory's ids across one list. An engine whose inventory is
/// itself not found (a radar site that dropped out) knows no ids.
fn require_known_location(
    engine: &dyn EdrEngine,
    known: &mut Option<std::collections::HashSet<String>>,
    id: &str,
) -> Result<(), HandlerError> {
    if known.is_none() {
        *known = Some(match engine.get_locations() {
            Ok(locations) => locations.into_iter().map(|l| l.id).collect(),
            Err(DataServerError::LocationNotFound(_)) => Default::default(),
            Err(e) => return Err(map_query_error(&e, "Location")),
        });
    }
    if known.as_ref().is_some_and(|known| known.contains(id)) {
        return Ok(());
    }
    Err(map_query_error(
        &DataServerError::LocationNotFound(id.to_string()),
        "Location",
    ))
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
    RawQuery(raw_query): RawQuery,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, HandlerError> {
    let request = DataRequest {
        instance_id: None,
        raw_query,
        headers,
    };
    run_position_query(id, request, params, state).await
}

/// `GET /collections/{id}/instances/{instanceId}/position` — position query
/// against a specific forecast model run.
pub async fn instance_position_query(
    Path((id, instance_id)): Path<(String, String)>,
    Query(params): Query<PositionQueryParams>,
    RawQuery(raw_query): RawQuery,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, HandlerError> {
    let request = DataRequest {
        instance_id: Some(instance_id),
        raw_query,
        headers,
    };
    run_position_query(id, request, params, state).await
}

/// The parts of a data-query request its response format depends on.
struct DataRequest {
    instance_id: Option<String>,
    raw_query: Option<String>,
    headers: HeaderMap,
}

impl DataRequest {
    /// The GeoJSON context of a `query_type` query on collection `id`.
    fn geojson(
        self,
        state: &EdrState,
        engine: &Arc<dyn EdrEngine>,
        config: &CollectionConfig,
        query_type: &str,
    ) -> GeoJsonRequest {
        let path = match &self.instance_id {
            Some(iid) => format!(
                "/collections/{}/instances/{}/{query_type}",
                config.id,
                instance_path_segment(iid)
            ),
            None => format!("/collections/{}/{query_type}", config.id),
        };
        GeoJsonRequest {
            engine: engine.clone(),
            base: request_base_url(state, &self.headers),
            collection_id: config.id.clone(),
            collection_title: config.title.clone(),
            path,
            raw_query: self.raw_query,
            offered: engine_query_formats(engine.as_ref(), query_type).to_vec(),
            location_ids: Vec::new(),
        }
    }
}

async fn run_position_query(
    id: String,
    request: DataRequest,
    params: PositionQueryParams,
    state: AppState,
) -> Result<impl IntoResponse, HandlerError> {
    let state = state.load_full();
    let (engine, config) = lookup_collection(&state, &id)?;
    require_query_type(engine, &id, "position", "position")?;
    let reference_time = resolve_instance(engine, request.instance_id.as_deref())?;

    let datetime = request_datetime(params.datetime.as_deref())?;
    let window = datetime.as_ref().map(DatetimeSelector::envelope);

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

    if let Some(DatetimeSelector::Instants(instants)) = &datetime {
        let lookups = points.len().saturating_mul(instants.len());
        if lookups > crate::params::MAX_POSITION_LOOKUPS {
            return Err(bad_request(&DataServerError::QueryTooLarge(format!(
                "{} points × {} datetime instants is {lookups} position lookups; the limit is {} — \
                 name fewer points or instants",
                points.len(),
                instants.len(),
                crate::params::MAX_POSITION_LOOKUPS
            ))));
        }
    }
    let format = data_query_format(
        engine,
        "position",
        params.f.as_deref(),
        &request.headers,
        "position queries",
    )?;
    request_crs(params.crs.as_deref())?;
    // `limit` counts the top-level coverages of the flattened collection
    // (#922): point order, then each point's own coverages (one per step
    // for a vertical profile). The response shape follows the request, so
    // a MULTIPOINT stays a CoverageCollection however few coverages remain.
    let limit = request_limit(params.limit.as_deref())?.unwrap_or(usize::MAX);
    let single = points.len() == 1;
    // A point the engine answers yields at least one coverage (none is a
    // 404), so the points past the first `limit` cannot reach the response:
    // never query them. Their coverages are then uncounted, so a GeoJSON
    // response omits `numberMatched`.
    let skipped_points = points.len() > limit;
    points.truncate(limit);
    let geojson = request.geojson(&state, engine, config, "position");
    let engine = engine.clone();
    let response = execute_query(false, move |budget| {
        // Shared by every instant of a datetime list, so the budget bounds
        // the whole response.
        let mut values = 0usize;
        // Whether `limit` dropped a coverage before the final cap (a point
        // never queried, or an instant's batch trimmed): the matches are then
        // uncounted, and a GeoJSON response leaves `numberMatched` out.
        let mut dropped = skipped_points;
        if budget.expired() {
            return Err(query_timeout());
        }
        // One engine batch over every point for one datetime window.
        let mut query = |datetime| -> Result<CoverageResponse, DataServerError> {
            let values_before = values;
            let mut coverages = Vec::with_capacity(points.len());
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
                let produced = batch.len();
                batch.truncate(limit.saturating_sub(coverages.len()));
                dropped |= batch.len() < produced;
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
            let queried = engine.query_positions(
                &points,
                datetime,
                param_names.as_deref(),
                z.as_deref(),
                reference_time,
                &mut emit,
            );
            if let Err(e) = queried {
                // A datetime-list instant without data is skipped: its
                // partial batch no longer counts against the budget.
                values = values_before;
                return Err(e);
            }
            Ok(if !collection_response && coverages.len() == 1 {
                CoverageResponse::Single(coverages.remove(0))
            } else {
                CoverageResponse::Collection(coverages)
            })
        };
        let result = crate::datetime_list::run(datetime.as_ref(), || budget.expired(), &mut query)
            .map_err(|e| map_query_error(&e, "Position"))?;
        if budget.expired() {
            return Err(query_timeout());
        }
        // Each instant is capped above; coverages that a datetime list's merge
        // could not join are capped again on the whole response. The merged
        // coverages before that cap are the GeoJSON `numberMatched`.
        let matched = (!dropped).then(|| coverage_count(&result));
        let result = limit_coverages(result, Some(limit));
        render_coverage_response(
            result,
            matched,
            format.format,
            params.width,
            params.height,
            &geojson,
        )
    })
    .await?;
    Ok(with_format_vary(
        with_data_cache_control(response, window),
        format,
    ))
}

pub async fn area_query(
    Path(id): Path<String>,
    Query(params): Query<AreaQueryParams>,
    RawQuery(raw_query): RawQuery,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, HandlerError> {
    let request = DataRequest {
        instance_id: None,
        raw_query,
        headers,
    };
    run_area_query(id, request, params, state).await
}

/// `GET /collections/{id}/instances/{instanceId}/area` — area query against a
/// specific forecast model run.
pub async fn instance_area_query(
    Path((id, instance_id)): Path<(String, String)>,
    Query(params): Query<AreaQueryParams>,
    RawQuery(raw_query): RawQuery,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, HandlerError> {
    let request = DataRequest {
        instance_id: Some(instance_id),
        raw_query,
        headers,
    };
    run_area_query(id, request, params, state).await
}

async fn run_area_query(
    id: String,
    request: DataRequest,
    params: AreaQueryParams,
    state: AppState,
) -> Result<impl IntoResponse, HandlerError> {
    let state = state.load_full();
    let (engine, config) = lookup_collection(&state, &id)?;
    require_query_type(engine, &id, "area", "area")?;

    // Static request-level checks before engine/instance resolution, so the
    // instance variant rejects the same way as the non-instance `area_query`
    // (e.g. `…/instances/x/area?f=png` → 400 "PNG not available", not a 404 on
    // the instance). An area result is gridded / multi-coverage, not a plot,
    // and area is not a point query, so no GeoJSON either (#929): CoverageJSON
    // or its HTML page (#971).
    let format = data_query_format(
        engine,
        "area",
        params.f.as_deref(),
        &request.headers,
        "area queries",
    )?;
    request_crs(params.crs.as_deref())?;

    let reference_time = resolve_instance(engine, request.instance_id.as_deref())?;

    let datetime = request_datetime(params.datetime.as_deref())?;
    let window = datetime.as_ref().map(DatetimeSelector::envelope);

    let param_names: Option<Vec<String>> = params
        .parameter_name
        .as_deref()
        .map(|s| s.split(',').map(|p| p.trim().to_string()).collect());

    let z = resolve_request_z(engine, params.z.as_deref())?;
    let limit = request_limit(params.limit.as_deref())?;

    let page = request.geojson(&state, engine, config, "area");
    let engine = engine.clone();
    let response = execute_query(false, move |budget| {
        let result = crate::datetime_list::run(
            datetime.as_ref(),
            || budget.expired(),
            |datetime| {
                engine.query_area(
                    &params.coords,
                    datetime,
                    param_names.as_deref(),
                    z.as_deref(),
                    reference_time,
                )
            },
        )
        .map_err(|e| map_query_error(&e, "Area"))?;
        render_coverage_response(
            limit_coverages(result, limit),
            None,
            format.format,
            None,
            None,
            &page,
        )
    })
    .await?;

    Ok(with_format_vary(
        with_data_cache_control(response, window),
        format,
    ))
}

pub async fn radius_query(
    Path(id): Path<String>,
    Query(params): Query<RadiusQueryParams>,
    RawQuery(raw_query): RawQuery,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, HandlerError> {
    let request = DataRequest {
        instance_id: None,
        raw_query,
        headers,
    };
    run_radius_query(id, request, params, state).await
}

/// `GET /collections/{id}/instances/{instanceId}/radius` — radius query
/// against a specific forecast model run.
pub async fn instance_radius_query(
    Path((id, instance_id)): Path<(String, String)>,
    Query(params): Query<RadiusQueryParams>,
    RawQuery(raw_query): RawQuery,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, HandlerError> {
    let request = DataRequest {
        instance_id: Some(instance_id),
        raw_query,
        headers,
    };
    run_radius_query(id, request, params, state).await
}

/// OGC API - EDR `radius`: everything within `within` `within-units` of a
/// WKT `POINT`. The engine turns the circle into a polygon and answers it
/// as an area query (see `EdrEngine::query_radius`), so the response shape
/// and error mapping are the area query's.
async fn run_radius_query(
    id: String,
    request: DataRequest,
    params: RadiusQueryParams,
    state: AppState,
) -> Result<impl IntoResponse, HandlerError> {
    let state = state.load_full();
    let (engine, config) = lookup_collection(&state, &id)?;

    // Same capability guard as trajectory: an engine that does not advertise
    // `radius` has no such resource (404), and the live route stays
    // consistent with `data_queries` and the OpenAPI gating.
    require_query_type(engine, &id, "radius", "radius")?;

    // Never PNG (a multi-coverage result); GeoJSON for station series (#929).
    let format = data_query_format(
        engine,
        "radius",
        params.f.as_deref(),
        &request.headers,
        "radius queries",
    )?;
    request_crs(params.crs.as_deref())?;

    let within_m =
        parse_within_metres(&params.within, &params.within_units).map_err(|e| bad_request(&e))?;

    let reference_time = resolve_instance(engine, request.instance_id.as_deref())?;

    let datetime = request_datetime(params.datetime.as_deref())?;
    let window = datetime.as_ref().map(DatetimeSelector::envelope);

    let param_names: Option<Vec<String>> = params
        .parameter_name
        .as_deref()
        .map(|s| s.split(',').map(|p| p.trim().to_string()).collect());

    let z = resolve_request_z(engine, params.z.as_deref())?;
    let limit = request_limit(params.limit.as_deref())?;

    let geojson = request.geojson(&state, engine, config, "radius");
    let engine = engine.clone();
    // Rendering stays on the query executor: GeoJSON reads the engine's
    // location inventory to name the stations.
    let response = execute_query(false, move |budget| {
        let result = crate::datetime_list::run(
            datetime.as_ref(),
            || budget.expired(),
            |datetime| {
                engine.query_radius(
                    &params.coords,
                    within_m,
                    datetime,
                    param_names.as_deref(),
                    z.as_deref(),
                    reference_time,
                )
            },
        )
        .map_err(|e| map_query_error(&e, "Radius"))?;
        let matched = coverage_count(&result);
        render_coverage_response(
            limit_coverages(result, limit),
            Some(matched),
            format.format,
            None,
            None,
            &geojson,
        )
    })
    .await?;
    Ok(with_format_vary(
        with_data_cache_control(response, window),
        format,
    ))
}

/// The raw query pairs of a data query whose parameters are validated by
/// name ([`CubeQueryParams::from_pairs`]); a malformed query string is the
/// same JSON 400 as any other invalid parameter.
type QueryPairs = Result<Query<Vec<(String, String)>>, axum::extract::rejection::QueryRejection>;

pub async fn cube_query(
    Path(id): Path<String>,
    query: QueryPairs,
    RawQuery(raw_query): RawQuery,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, HandlerError> {
    let request = DataRequest {
        instance_id: None,
        raw_query,
        headers,
    };
    run_cube_query(id, request, query, state).await
}

/// `GET /collections/{id}/instances/{instanceId}/cube` — cube query against
/// a specific forecast model run.
pub async fn instance_cube_query(
    Path((id, instance_id)): Path<(String, String)>,
    query: QueryPairs,
    RawQuery(raw_query): RawQuery,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, HandlerError> {
    let request = DataRequest {
        instance_id: Some(instance_id),
        raw_query,
        headers,
    };
    run_cube_query(id, request, query, state).await
}

/// OGC API - EDR `cube` (#925): the parameters over a CRS84 `bbox`, at the
/// `z` levels, over the `datetime` window, optionally resampled to
/// `resolution-x`/`-y`/`-z` positions per axis. Every request parameter is
/// validated here — an unknown, repeated or unsupported one (`crs` other
/// than CRS84, `f=PNG`) is a 400 — before the engine runs.
async fn run_cube_query(
    id: String,
    request: DataRequest,
    query: QueryPairs,
    state: AppState,
) -> Result<impl IntoResponse, HandlerError> {
    let state = state.load_full();
    let (engine, config) = lookup_collection(&state, &id)?;
    require_query_type(engine, &id, "cube", "cube")?;

    let Query(pairs) = query.map_err(|_| bad_request_msg("Invalid cube query string"))?;
    let params = CubeQueryParams::from_pairs(pairs).map_err(|e| bad_request(&e))?;
    // Cube is CoverageJSON or its HTML page (#971): no PNG plot of a 4-D
    // grid, and no GeoJSON (not a point query, #929).
    let format = data_query_format(
        engine,
        "cube",
        params.f.as_deref(),
        &request.headers,
        "cube queries",
    )?;
    request_crs(params.crs.as_deref())?;
    // EDR `/req/edr/rc-cube` D: a cube without a bbox is a 400.
    let raw_bbox = params
        .bbox
        .as_deref()
        .ok_or_else(|| bad_request_msg("Cube queries require a bbox"))?;
    let (bbox, bbox_z) = parse_cube_bbox(raw_bbox).map_err(|e| bad_request(&e))?;
    let resolution = ds_core::cube::CubeResolution {
        x: parse_resolution("resolution-x", params.resolution_x.as_deref())
            .map_err(|e| bad_request(&e))?,
        y: parse_resolution("resolution-y", params.resolution_y.as_deref())
            .map_err(|e| bad_request(&e))?,
        z: parse_resolution("resolution-z", params.resolution_z.as_deref())
            .map_err(|e| bad_request(&e))?,
    };
    if resolution.z.is_some() && engine.get_vertical_extent().is_none() {
        return Err(bad_request_msg(
            "This collection has no vertical dimension; resolution-z is not supported",
        ));
    }

    let reference_time = resolve_instance(engine, request.instance_id.as_deref())?;

    // A datetime list runs the cube once per instant; the per-instant
    // `[t, z, y, x]` grids share x, y and z, so the merge joins them along t.
    let datetime = request_datetime(params.datetime.as_deref())?;
    let window = datetime.as_ref().map(DatetimeSelector::envelope);

    let param_names: Option<Vec<String>> = params
        .parameter_name
        .as_deref()
        .map(|s| s.split(',').map(|p| p.trim().to_string()).collect());

    // An explicit `z` overrides the vertical pair of a six-number bbox; on
    // a collection without a vertical dimension both are ignored, as `z`
    // is on every query (EDR 1.2 `/req/edr/z-response` A).
    let z = match (params.z.as_deref(), bbox_z) {
        (Some(z), _) => resolve_request_z(engine, Some(z))?,
        (None, Some(sel)) => resolve_z_selector(engine, &sel)?,
        (None, None) => None,
    };

    let page = request.geojson(&state, engine, config, "cube");
    let engine = engine.clone();
    let response = execute_query(false, move |budget| {
        let result = crate::datetime_list::run(
            datetime.as_ref(),
            || budget.expired(),
            |datetime| {
                engine.query_cube(
                    &bbox,
                    datetime,
                    param_names.as_deref(),
                    z.as_deref(),
                    resolution,
                    reference_time,
                )
            },
        )
        .map_err(|e| map_query_error(&e, "Cube"))?;
        render_coverage_response(result, None, format.format, None, None, &page)
    })
    .await?;

    Ok(with_format_vary(
        with_data_cache_control(response, window),
        format,
    ))
}

pub async fn trajectory_query(
    Path(id): Path<String>,
    Query(params): Query<TrajectoryQueryParams>,
    RawQuery(raw_query): RawQuery,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, HandlerError> {
    let state = state.load_full();
    let (engine, config) = lookup_collection(&state, &id)?;

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

    // The formats `query_formats` offers for this engine's trajectory shape:
    // CoverageJSON along a path, also PNG for a radar cross-section. An `f`
    // it does not offer (PNG along a path, GeoJSON on any trajectory) is a
    // 400 before the query runs.
    let negotiated = data_query_format(
        engine,
        "trajectory",
        params.f.as_deref(),
        &headers,
        "trajectory queries",
    )?;
    let format = negotiated.format;
    request_crs(params.crs.as_deref())?;

    let datetime = request_datetime(params.datetime.as_deref())?;
    let window = datetime.as_ref().map(DatetimeSelector::envelope);

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
        // error list; /conf/trajectory/coords-param-separate-z-*). A
        // `datetime` list is a `datetime` too.
        let path = TrajectoryPath::parse(&params.coords).map_err(|e| bad_request(&e))?;
        // The request's own `z`, even where a collection without a vertical
        // extent would ignore it: the exclusion is about the request.
        let z_given = params.z.as_deref().is_some_and(|z| !z.trim().is_empty());
        if path.has_z && z_given {
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
    // The HTML page (#971) renders on the query worker, as every query's does.
    let page = (format == EdrFormat::Html).then(|| {
        DataRequest {
            instance_id: None,
            raw_query,
            headers,
        }
        .geojson(&state, engine, config, "trajectory")
    });
    let engine = engine.clone();
    let coords = params.coords.clone();
    let (result, html) = execute_query(blocking, move |budget| {
        // A `datetime` list runs one query per instant and merges them
        // (one coverage per instant for an along-path 2-D or Z path; an M
        // path never gets here with a `datetime`).
        let result = crate::datetime_list::run(
            datetime.as_ref(),
            || budget.expired(),
            |datetime| {
                engine.query_trajectory(
                    &coords,
                    datetime,
                    param_names.as_deref(),
                    z.as_deref(),
                    None,
                )
            },
        )
        .map_err(|e| map_query_error(&e, "Trajectory"))?;
        let html = page.map(|page| crate::html::coverage_page(&result, &page.html_page()));
        Ok((result, html))
    })
    .await?;

    let response = match format {
        EdrFormat::CoverageJson => {
            with_data_cache_control(coverage_json_response(&result, "Trajectory")?, window)
        }
        // `query_formats` never offers GeoJSON for a trajectory.
        EdrFormat::GeoJson => {
            tracing::error!("EDR trajectory: GeoJSON negotiated for a trajectory");
            return Err(server_error());
        }
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
            with_data_cache_control(
                ([(header::CONTENT_TYPE, "image/png")], png).into_response(),
                window,
            )
        }
        EdrFormat::Html => {
            let Some(html) = html else {
                tracing::error!("EDR trajectory: HTML negotiated but not rendered");
                return Err(server_error());
            };
            with_data_cache_control(crate::html::response(html), window)
        }
    };
    Ok(with_format_vary(response, negotiated))
}

/// The data queries with an `/instances/{instanceId}/…` route, so the only
/// ones an instance document advertises.
const INSTANCE_QUERY_TYPES: [&str; 4] = ["position", "area", "radius", "cube"];

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
/// adds its accepted `within_units`, locations `multiple_locations` (EDR
/// 1.2's optional boolean, #923). `output_formats` are the formats the
/// route answers, [`query_formats`] for the engine (`station_series`:
/// `EdrEngine::serves_station_series`; `trajectory`:
/// `EdrEngine::trajectory_shape`): area and radius results are gridded or
/// multi-coverage, so they have no PNG plot, the point queries of a station
/// collection add GeoJSON (#929), and only a radar cross-section trajectory
/// renders as a PNG — an along-path one (#926) is CoverageJSON only.
fn data_query_variables(
    query_type: &str,
    station_series: bool,
    trajectory: TrajectoryShape,
) -> Option<serde_json::Value> {
    let (title, description) = match query_type {
        "locations" => (
            "Locations query",
            "Lists the collection's named locations as GeoJSON; \
             /locations/{locationId} returns the data at one of them, \
             or at each of a comma-delimited list of them.",
        ),
        "position" => (
            "Position query",
            "Data at the WKT POINT or MULTIPOINT given in coords, \
             as CRS84 longitude and latitude.",
        ),
        "area" => (
            "Area query",
            "Data inside the WKT POLYGON given in coords, as CRS84 longitude and latitude.",
        ),
        "radius" => (
            "Radius query",
            "Data within a distance of the WKT POINT given in coords, as CRS84 \
             longitude and latitude; within and within-units give the distance.",
        ),
        "trajectory" => match trajectory {
            TrajectoryShape::AlongPath => (
                "Trajectory query",
                "Data sampled along the WKT LINESTRING, LINESTRING Z, LINESTRING M or \
                 LINESTRING ZM given in coords, as CRS84 longitude and latitude; Z is \
                 each vertex's level, M its time in seconds since the Unix epoch.",
            ),
            TrajectoryShape::CrossSection => (
                "Trajectory query",
                "A vertical cross-section along the 2-D WKT LINESTRING given in coords, \
                 as CRS84 longitude and latitude.",
            ),
        },
        "cube" => (
            "Cube query",
            "Data inside the bbox given as west,south,east,north in CRS84 longitude and \
             latitude, at the levels z selects; resolution-x, -y and -z resample it.",
        ),
        _ => return None,
    };
    // The same list the handler negotiates over (#929).
    let output_formats: Vec<&str> = query_formats(query_type, station_series, trajectory)
        .iter()
        .map(|f| f.name())
        .collect();
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
    if query_type == "locations" {
        // EDR 1.2 `/req/edr/rc-locations-variables` B: `locationId` may list
        // several ids (#923). The handler fans the list out over the
        // engine's own `query_location`, so every engine serving locations
        // supports it. Absent, clients must assume it is not supported.
        variables["multiple_locations"] = json!(true);
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
            // RFC 3339 (#947); its colons are valid in a path segment (RFC 3986
            // `pchar`), so the hrefs carry them unencoded.
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
    let vertical_extent = engine.get_vertical_extent();
    if let Some(vertical) = &vertical_extent {
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
    // Under an instance only the run-queryable types get routes.
    let query_types: Vec<String> = if instance.is_some() {
        engine
            .supported_query_types()
            .into_iter()
            .filter(|qt| INSTANCE_QUERY_TYPES.contains(&qt.as_str()))
            .collect()
    } else {
        engine.supported_query_types()
    };
    let station_series = engine.serves_station_series();
    let mut data_queries = serde_json::Map::new();
    for qt in &query_types {
        // Every routed query type's path segment is its name.
        let Some(mut variables) =
            data_query_variables(qt, station_series, engine.trajectory_shape())
        else {
            continue;
        };
        if qt == "cube" {
            // EDR 1.2 `/req/edr/rc-cube-variables` B: the units `z` is given
            // in — the collection's vertical axis unit.
            let units: Vec<&str> = vertical_extent.iter().map(|v| v.unit()).collect();
            variables["height_units"] = json!(units);
        }
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
    // Every query link names the media type its end point answers
    // (`/req/core/rc-md-query-links` B).
    for (query_type, query) in data_queries.iter_mut() {
        query["link"]["type"] = json!(query_media_type(query_type));
    }

    let self_title = match instance {
        Some(_) => format!("{} — run {self_id}", config.title),
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
    // EDR 1.2 `/req/core/rc-collection-info-links` A and
    // `/req/core/rc-md-query-links` A: the collection's own `links` name its
    // query end points, and a forecast collection's instances, not only
    // `data_queries`. Copied from it, so the two cannot disagree.
    links.extend(
        data_queries
            .iter()
            .map(|(query_type, query)| data_link(query_type, &query["link"])),
    );
    // `/req/edr/rc-collection-info` J: with a radius link in `links`, the
    // collection lists the `within-units` it accepts.
    let radius = data_queries.contains_key("radius");

    let mut fields = json!({
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
        "output_formats": if station_series {
            json!(["CoverageJSON", "GeoJSON", "PNG", "HTML"])
        } else {
            json!(["CoverageJSON", "PNG", "HTML"])
        }
    });
    if radius {
        fields["within_units"] = json!(WITHIN_UNITS);
    }
    api_common::collection_metadata(config, fields, links)
}

/// The media type the end point of a `data_queries` entry answers by
/// default: the `/locations` list and `items` are GeoJSON, the instances
/// list JSON, every other data query CoverageJSON.
fn query_media_type(query_type: &str) -> &'static str {
    match query_type {
        "locations" | "items" => EdrFormat::GeoJson.media_type(),
        "instances" => "application/json",
        _ => EdrFormat::CoverageJson.media_type(),
    }
}

/// The collection's `rel=data` link to one `data_queries` end point: the
/// same href and type, titled like its variables.
fn data_link(query_type: &str, link: &serde_json::Value) -> serde_json::Value {
    let title = match query_type {
        "instances" => "Instances (forecast model runs)",
        _ => link["variables"]["title"].as_str().unwrap_or(query_type),
    };
    json!({"href": link["href"], "rel": "data", "type": link["type"], "title": title})
}

/// The collection or instance document the HTML page renders, without its
/// `rel=data` links: the page already lists the same end points from
/// `data_queries`, each with its documentation, where a link list would open
/// them bare as `?f=html`.
fn html_document(mut metadata: serde_json::Value) -> serde_json::Value {
    if let Some(links) = metadata["links"].as_array_mut() {
        links.retain(|link| link["rel"] != "data");
    }
    metadata
}
