//! The EDR `items` query (#928): a collection's features as GeoJSON.
//!
//! EDR 1.2 defines items by reference to OGC API - Features, so this module
//! adds no engine method and no second implementation of it. It serves the
//! collection's `FeatureEngine` (the one [`EdrState::feature_engines`] holds)
//! through the Features API's own pieces: `bbox` and `datetime` parsing, the
//! GeoJSON encoding and the `self`/`next`/`prev` links that carry the filters.
//! What differs is EDR's: the `limit` definition (`/req/edr/rc-limit-*`:
//! default 10, maximum 10 000, larger values clamped, anything that is not a
//! positive integer a 400), links under the EDR mount, GeoJSON or its HTML
//! page (#971), and execution on the bounded EDR query executor.
//!
//! Only `bbox`, `datetime`, `limit`, `offset` (the paging links' position)
//! and `f` are accepted; any other parameter, or one given twice, is a 400
//! naming the valid ones (root CLAUDE.md: never silently ignore a parameter).
//!
//! The body is EDR GeoJSON (`/req/edr-geojson/content` A): every feature's
//! `properties` gains the four `edrProperties` members ([`EdrMembers`]) next
//! to the engine's own. They are added here, never in the Features API's
//! `/items`, whose features are not EDR features.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Utc;
use serde_json::{json, Value};

use api_common::JsonError;
use api_features::crs::ResponseCrs;
use api_features::params::{parse_bbox, parse_datetime};
use api_features::response::{feature_page_to_geojson, feature_to_geojson, preserved_query};
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::{Bbox, DatetimeInterval, Feature, FeatureQuery, Geometry, PropertyValue};
use ds_core::feature_engine::FeatureEngine;
use ds_core::model::Location;

use crate::geojson::encode_path_segment;
use crate::handlers::{
    bad_request, error_response, execute_query, lookup_collection, map_query_error,
    request_base_url, server_error, with_format_vary, AppState, EdrState, HandlerError,
};
use crate::params::{
    negotiate_list_format, EdrFormat, NegotiatedFormat, LOCATIONS_FORMATS, MAX_WITHIN_M,
};

/// Page size without `limit` (`/req/edr/rc-limit-definition`).
pub const DEFAULT_LIMIT: usize = 10;
/// Largest page: a larger `limit` is served as this, not rejected
/// (`/req/edr/rc-limit-response` C).
pub const MAX_LIMIT: usize = 10_000;

/// The parameters of `/items`, in the order a 400 lists them.
pub const LIST_PARAMETERS: [&str; 5] = ["bbox", "datetime", "limit", "offset", "f"];

/// The default output format, as `data_queries` advertises it.
pub const OUTPUT_FORMAT: &str = "GeoJSON";

/// Every output format, as `data_queries` advertises them: GeoJSON and its
/// HTML page (#971).
pub const OUTPUT_FORMATS: [&str; 2] = [OUTPUT_FORMAT, "HTML"];

// The WKT every data query advertises in `crs_details` (#918).
use ds_core::geo::CRS84_WKT;

/// A parsed `/items` query.
#[derive(Debug)]
pub struct ItemsParams {
    pub bbox: Option<Bbox>,
    pub datetime: Option<DatetimeInterval>,
    pub limit: usize,
    pub offset: usize,
    /// `f`, checked by [`parse_format`]; negotiated with `Accept`.
    pub f: Option<String>,
}

impl ItemsParams {
    /// Parse the decoded query pairs. Unknown and repeated parameters, and
    /// malformed values, are `InvalidParameter`/`InvalidBbox`/
    /// `InvalidDatetime` naming the problem.
    pub fn from_pairs(pairs: Vec<(String, String)>) -> Result<Self, DataServerError> {
        let mut params = Self {
            bbox: None,
            datetime: None,
            limit: DEFAULT_LIMIT,
            offset: 0,
            f: None,
        };
        let mut seen = HashSet::new();
        for (name, value) in pairs {
            if !LIST_PARAMETERS.contains(&name.as_str()) {
                return Err(DataServerError::InvalidParameter(format!(
                    "unsupported parameter '{name}' on items; valid parameters: {}",
                    LIST_PARAMETERS.join(", ")
                )));
            }
            if !seen.insert(name.clone()) {
                return Err(DataServerError::InvalidParameter(format!(
                    "duplicate parameter '{name}'"
                )));
            }
            match name.as_str() {
                "bbox" => params.bbox = Some(parse_bbox(&value)?),
                "datetime" => params.datetime = Some(parse_datetime(&value)?),
                "limit" => params.limit = parse_limit(&value)?,
                "offset" => {
                    params.offset = value.parse().map_err(|_| {
                        DataServerError::InvalidParameter(format!(
                            "offset must be a non-negative integer, got '{value}'"
                        ))
                    })?
                }
                _ => {
                    parse_format(&value)?;
                    params.f = Some(value);
                }
            }
        }
        Ok(params)
    }
}

/// `limit`: a positive integer; above [`MAX_LIMIT`] (however large) it is
/// [`MAX_LIMIT`]. Zero, a sign, a fraction or anything else is a 400.
pub fn parse_limit(value: &str) -> Result<usize, DataServerError> {
    let invalid = || {
        DataServerError::InvalidParameter(format!(
            "limit must be an integer of at least 1, got '{value}'"
        ))
    };
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    // All digits: parsing can only fail by overflowing, which is above the
    // maximum like any other large value.
    let n = value.parse::<u64>().unwrap_or(u64::MAX);
    if n == 0 {
        return Err(invalid());
    }
    Ok(usize::try_from(n).map_or(MAX_LIMIT, |n| n.min(MAX_LIMIT)))
}

/// `f` on items: GeoJSON, by its EDR name or media type, or as `json`; or
/// HTML (#971). An unencoded `+` in `application/geo+json` arrives as a
/// space.
pub fn parse_format(value: &str) -> Result<EdrFormat, DataServerError> {
    negotiate_list_format(Some(value), None, "items").map(|n| n.format)
}

/// The `/items/{itemId}` query: `f` at most once, nothing else.
fn parse_item_pairs(pairs: Vec<(String, String)>) -> Result<Option<String>, DataServerError> {
    let mut format = None;
    for (name, value) in pairs {
        if name != "f" {
            return Err(DataServerError::InvalidParameter(format!(
                "unsupported parameter '{name}' on a single item; only 'f' is accepted"
            )));
        }
        if format.is_some() {
            return Err(DataServerError::InvalidParameter(
                "duplicate parameter 'f'".into(),
            ));
        }
        parse_format(&value)?;
        format = Some(value);
    }
    Ok(format)
}

/// The response format of an items request: `f`, else `Accept`.
fn items_format(f: Option<&str>, headers: &HeaderMap) -> Result<NegotiatedFormat, HandlerError> {
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok());
    negotiate_list_format(f, accept, "items").map_err(|e| bad_request(&e))
}

/// The HTML page context of an items response (#971): its links as HTML.
fn html_page<'a>(
    doc: &Value,
    root: &'a str,
    collection_id: &'a str,
    collection_title: &'a str,
    raw_query: Option<&'a str>,
) -> crate::html::DataPage<'a> {
    let links = crate::html::feature_page_links(&doc["links"]);
    let json_url = links
        .iter()
        .find(|l| l.rel == "alternate")
        .map(|l| l.href.clone())
        .unwrap_or_default();
    crate::html::DataPage {
        base: root.strip_suffix(api_common::mounts::EDR).unwrap_or(root),
        collection_id,
        collection_title,
        title: match doc["id"].as_str() {
            Some(id) => format!("Item {id}"),
            None => "Items".into(),
        },
        raw_query,
        json_url,
        links,
    }
}

/// A collection's EDR engine and the feature engine its items come from.
type ItemsEngines = (Arc<dyn EdrEngine>, Arc<dyn FeatureEngine>);

/// The collection's EDR and feature engines: 404 for an unknown collection,
/// and for one whose engine serves no features (the query does not exist
/// for it, as for every other unsupported query type, #668).
fn items_engines(state: &EdrState, id: &str) -> Result<ItemsEngines, HandlerError> {
    let (edr, _) = lookup_collection(state, id)?;
    let features = state.feature_engines.get(id).cloned().ok_or_else(|| {
        JsonError(
            StatusCode::NOT_FOUND,
            Json(json!({
                "code": "NotFound",
                "description": format!("Collection '{id}' does not support items queries")
            })),
        )
    })?;
    Ok((edr.clone(), features))
}

/// The `edrProperties` members every item carries (`datetime`,
/// `parameter-name`, `label`, `edrqueryendpoint`), required by
/// `/req/edr-geojson/content` A. An item that is one of the collection's
/// locations (a CSV, BUFR or PostGIS station) gets what its `/locations`
/// feature says: the location's label, its `/locations/{id}` query, and the
/// collection's parameters and temporal extent. Any other item (a nowcast
/// cell) gets the radius query the engine sizes for it
/// ([`EdrEngine::item_radius`]), else a position query at its point, else
/// the collection. Built on the query executor, since it reads
/// `get_locations()`.
struct EdrMembers<'a> {
    engine: &'a dyn EdrEngine,
    /// `start/end` of the collection's temporal extent, empty without one.
    datetime: String,
    parameter_names: Vec<String>,
    /// The page's items that are locations, by id.
    locations: HashMap<String, Location>,
    /// `{edr root}/collections/{id}`.
    collection_url: String,
    serves_position: bool,
    serves_radius: bool,
}

impl<'a> EdrMembers<'a> {
    /// The members for the items `ids` of collection `id`, under `root`.
    fn new(
        engine: &'a dyn EdrEngine,
        root: &str,
        id: &str,
        ids: &[&str],
    ) -> Result<Self, DataServerError> {
        let query_types = engine.supported_query_types();
        let serves = |query_type: &str| query_types.iter().any(|q| q == query_type);
        let locations = if serves("locations") {
            let wanted: HashSet<&str> = ids.iter().copied().collect();
            engine
                .get_locations()?
                .into_iter()
                .filter(|l| wanted.contains(l.id.as_str()))
                .map(|l| (l.id.clone(), l))
                .collect()
        } else {
            HashMap::new()
        };
        Ok(Self {
            engine,
            datetime: engine
                .get_temporal_extent()
                .map(|(start, end)| format!("{}/{}", start.to_rfc3339(), end.to_rfc3339()))
                .unwrap_or_default(),
            parameter_names: engine.get_parameters(),
            locations,
            collection_url: format!("{root}/collections/{id}"),
            serves_position: serves("position"),
            serves_radius: serves("radius"),
        })
    }

    /// Add the members to `json`, the GeoJSON of `feature`. They replace an
    /// engine property of the same name: their meaning is EDR's.
    fn apply(&self, feature: &Feature, json: &mut Value) {
        let (label, endpoint, datetime) = match self.locations.get(&feature.id) {
            Some(location) => (
                location.label.clone(),
                format!(
                    "{}/locations/{}",
                    self.collection_url,
                    encode_path_segment(&location.id)
                ),
                None,
            ),
            None => {
                let (endpoint, datetime) = self.query_at(feature);
                let label = match feature.properties.get("name") {
                    Some(PropertyValue::String(name)) => name.clone(),
                    _ => feature.id.clone(),
                };
                (label, endpoint, datetime)
            }
        };
        if let Some(properties) = json["properties"].as_object_mut() {
            properties.insert(
                "datetime".into(),
                json!(datetime.as_deref().unwrap_or(&self.datetime)),
            );
            properties.insert("parameter-name".into(), json!(self.parameter_names));
            properties.insert("label".into(), json!(label));
            properties.insert("edrqueryendpoint".into(), json!(endpoint));
        }
    }

    /// The query of an item that is not a location, and its own `datetime`
    /// when it has one: the engine's radius query around its point, else a
    /// position query at its point, else the collection, whose
    /// `data_queries` list its queries.
    fn query_at(&self, feature: &Feature) -> (String, Option<String>) {
        let Geometry::Point { x, y } = *feature.geometry else {
            return (self.collection_url.clone(), None);
        };
        let radius = self
            .serves_radius
            .then(|| self.engine.item_radius(feature))
            .flatten()
            .filter(|r| r.within_km.is_finite() && r.within_km > 0.0);
        if let Some(radius) = radius {
            // Up to the next 100 m, so the URL stays short and the circle
            // never narrows; within the radius cap.
            let within = ((radius.within_km * 10.0).ceil() / 10.0).min(MAX_WITHIN_M / 1000.0);
            let datetime = radius
                .datetime
                .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true));
            return (
                format!(
                    "{}/radius?coords=POINT({x}%20{y})&within={within}&within-units=km",
                    self.collection_url
                ),
                datetime,
            );
        }
        if self.serves_position {
            return (
                format!("{}/position?coords=POINT({x}%20{y})", self.collection_url),
                None,
            );
        }
        (self.collection_url.clone(), None)
    }
}

/// The absolute root the item links hang off: the EDR mount.
fn edr_root(state: &EdrState, headers: &HeaderMap) -> String {
    format!(
        "{}{}",
        request_base_url(state, headers),
        api_common::mounts::EDR
    )
}

fn geojson_response(body: String) -> Response {
    ([(header::CONTENT_TYPE, "application/geo+json")], body).into_response()
}

fn serialize(doc: &Value) -> Result<String, HandlerError> {
    serde_json::to_string(doc).map_err(|e| {
        tracing::error!("EDR items GeoJSON serialise error: {e}");
        server_error()
    })
}

/// `GET /collections/{id}/items`: a GeoJSON `FeatureCollection` page with
/// `numberMatched`, `numberReturned` and `self`/`next`/`prev` links.
pub async fn items(
    Path(id): Path<String>,
    Query(pairs): Query<Vec<(String, String)>>,
    RawQuery(raw_query): RawQuery,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, HandlerError> {
    let state = state.load_full();
    let (edr, features) = items_engines(&state, &id)?;
    let params = ItemsParams::from_pairs(pairs).map_err(|e| bad_request(&e))?;
    let format = items_format(params.f.as_deref(), &headers)?;
    let title = state
        .collections
        .get(&id)
        .map(|c| c.title.clone())
        .unwrap_or_default();
    // A collection whose features carry no time cannot filter by it: a 400,
    // not the full set with 200 — the Features API's rule (#682).
    if params.datetime.is_some() && !features.has_time_dimension() {
        return Err(bad_request(&DataServerError::InvalidParameter(format!(
            "Collection '{id}' has no time dimension; datetime is not supported"
        ))));
    }
    // Cache-Control (#499): a closed window entirely in the past gets the
    // long policy, everything else the short one.
    let (start, end) = params
        .datetime
        .as_ref()
        .map_or((None, None), |d| (d.start, d.end));
    let cache_control = ds_core::http_cache::data_cache_control(start, end, Utc::now());
    // Following `next` must keep the caller's filters (api-features rule).
    let filters = preserved_query(params.bbox.as_ref(), params.datetime.as_ref(), &[], &[]);
    let root = edr_root(&state, &headers);
    let query = FeatureQuery {
        bbox: params.bbox,
        limit: params.limit,
        offset: params.offset,
        datetime: params.datetime,
        sortby: Vec::new(),
        property_filters: Vec::new(),
    };

    let (body, etag) = execute_query(false, move |_budget| {
        let page = features
            .get_features(&query)
            .map_err(|e| map_query_error(&e, "Items"))?;
        let mut doc = feature_page_to_geojson(
            &page,
            &id,
            query.limit,
            query.offset,
            &filters,
            "",
            &root,
            &ResponseCrs::default(),
        );
        let ids: Vec<&str> = page.features.iter().map(|f| f.id.as_str()).collect();
        let members = EdrMembers::new(edr.as_ref(), &root, &id, &ids)
            .map_err(|e| map_query_error(&e, "Items"))?;
        if let Some(json) = doc["features"].as_array_mut() {
            for (feature, json) in page.features.iter().zip(json) {
                members.apply(feature, json);
            }
        }
        // `timeStamp` is the generation time: hash the page without it, or
        // `If-None-Match` could never match (the Features `items` rule).
        let time_stamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        if format.format == EdrFormat::Html {
            let page = html_page(&doc, &root, &id, &title, raw_query.as_deref());
            let mut html = crate::html::features_page(&doc, &page, true);
            let etag = ds_core::http_cache::etag_of(html.as_bytes());
            // Filled in place: no second copy of the page.
            if let Some(at) = html.find(crate::html::TIMESTAMP_SLOT) {
                let filled = format!("<time data-generated>{time_stamp}</time>");
                html.replace_range(at..at + crate::html::TIMESTAMP_SLOT.len(), &filled);
            }
            return Ok((crate::html::response(html), etag));
        }
        let etag = ds_core::http_cache::etag_of(serialize(&doc)?.as_bytes());
        doc["timeStamp"] = json!(time_stamp);
        Ok((geojson_response(serialize(&doc)?), etag))
    })
    .await?;

    let mut resp = with_format_vary(body, format);
    resp.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&etag).expect("quoted-hex etag is a valid header value"),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    Ok(resp)
}

/// `GET /collections/{id}/items/{itemId}`: one GeoJSON `Feature`; an
/// unknown id is a 404.
pub async fn item(
    Path((id, item_id)): Path<(String, String)>,
    Query(pairs): Query<Vec<(String, String)>>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, HandlerError> {
    let state = state.load_full();
    let (edr, features) = items_engines(&state, &id)?;
    let f = parse_item_pairs(pairs).map_err(|e| bad_request(&e))?;
    let format = items_format(f.as_deref(), &headers)?;
    let title = state
        .collections
        .get(&id)
        .map(|c| c.title.clone())
        .unwrap_or_default();
    let root = edr_root(&state, &headers);
    let response = execute_query(false, move |_budget| {
        let feature = features
            .get_feature(&item_id)
            .map_err(|e| map_query_error(&e, "Item"))?;
        let mut doc = feature_to_geojson(&feature, &id, &root, &ResponseCrs::default());
        EdrMembers::new(edr.as_ref(), &root, &id, &[feature.id.as_str()])
            .map_err(|e| map_query_error(&e, "Item"))?
            .apply(&feature, &mut doc);
        if format.format == EdrFormat::Html {
            let page = html_page(&doc, &root, &id, &title, None);
            return Ok(crate::html::response(crate::html::features_page(
                &doc, &page, false,
            )));
        }
        Ok(geojson_response(serialize(&doc)?))
    })
    .await?;
    Ok(with_format_vary(response, format))
}

/// The `data_queries.items` entry of a collection whose engine serves
/// features. The link variables carry every field EDR 1.2 requires of a
/// data query (`/req/edr/rc-variables-common`), items has no others.
pub fn data_query(query_base: &str) -> Value {
    json!({
        "link": {
            "href": format!("{query_base}/items"),
            "rel": "data",
            "variables": {
                "title": "Items query",
                "description": "The collection's features as an EDR GeoJSON FeatureCollection, filtered by bbox and datetime and paged by limit",
                "query_type": "items",
                "output_formats": OUTPUT_FORMATS,
                "default_output_format": OUTPUT_FORMAT,
                "crs_details": [{"crs": "CRS84", "wkt": CRS84_WKT}]
            }
        }
    })
}

/// The OpenAPI path items of one items-capable collection, keyed below
/// `/edr`. Parameters reference [`openapi_parameters`].
pub fn openapi_paths(id: &str, title: &str) -> Vec<(String, Value)> {
    let format = json!({
        "name": "f",
        "in": "query",
        "required": false,
        "schema": {"type": "string", "enum": LOCATIONS_FORMATS},
        "description": "Output format: GeoJSON, the default, or HTML (case-insensitive; encode the plus sign as %2B in application/geo+json). Without f, the Accept header chooses."
    });
    let errors = |not_found: &str| {
        json!({
            "400": error_response(400, "Bad request"),
            "404": error_response(404, not_found),
            "500": error_response(500, "Server error"),
            "503": error_response(503, "Query capacity exhausted; retry later"),
            "504": error_response(504, "Query deadline exceeded")
        })
    };
    let with_ok = |ok: Value, not_found: &str| {
        let mut responses = errors(not_found);
        responses["200"] = ok;
        responses
    };
    vec![
        (
            format!("/edr/collections/{id}/items"),
            json!({
                "get": {
                    "summary": format!("Items query for {title}"),
                    "description": "The collection's features as EDR GeoJSON, following OGC API - Features item access: each feature's properties carry the EDR members datetime, parameter-name, label and edrqueryendpoint next to its own. Accepts only bbox, datetime, limit, offset and f; any other parameter is a 400.",
                    "operationId": format!("getItems_{id}"),
                    "tags": [id],
                    "parameters": [
                        {"$ref": "#/components/parameters/items-bbox"},
                        {"$ref": "#/components/parameters/items-datetime"},
                        {"$ref": "#/components/parameters/items-limit"},
                        {"$ref": "#/components/parameters/items-offset"},
                        format.clone()
                    ],
                    "responses": with_ok(json!({
                        "description": "A page of EDR GeoJSON features",
                        "content": {
                            "application/geo+json": {
                                "schema": {"$ref": "#/components/schemas/items-featureCollection"}
                            },
                            "text/html": {"schema": {"type": "string"}}
                        }
                    }), "Collection not found, or it has no items")
                }
            }),
        ),
        (
            format!("/edr/collections/{id}/items/{{itemId}}"),
            json!({
                "get": {
                    "summary": format!("One item of {title}"),
                    "operationId": format!("getItem_{id}"),
                    "tags": [id],
                    "parameters": [
                        {
                            "name": "itemId",
                            "in": "path",
                            "required": true,
                            "description": "Retrieve data from the collection using a unique identifier.",
                            "schema": {"type": "string"}
                        },
                        format
                    ],
                    "responses": with_ok(json!({
                        "description": "One EDR GeoJSON feature",
                        "content": {
                            "application/geo+json": {
                                "schema": {"$ref": "#/components/schemas/items-feature"}
                            },
                            "text/html": {"schema": {"type": "string"}}
                        }
                    }), "Collection or item not found")
                }
            }),
        ),
    ]
}

/// The `components.parameters` the items paths reference. `bbox` and
/// `datetime` are EDR 1.2's own definitions, `limit` is
/// `/req/edr/rc-limit-definition`'s, each with `style: form` and
/// `explode: false`. Namespaced `items-` because other queries define a
/// differently described `datetime`.
pub fn openapi_parameters() -> Value {
    json!({
        "items-bbox": {
            "name": "bbox",
            "in": "query",
            "description": "Only features that have a geometry that intersects the bounding box are selected.\nThe bounding box is provided as four or six numbers, depending on whether the\ncoordinate reference system includes a vertical axis (height or depth):\n* Lower left corner, coordinate axis 1\n* Lower left corner, coordinate axis 2\n* Minimum value, coordinate axis 3 (optional)\n* Upper right corner, coordinate axis 1\n* Upper right corner, coordinate axis 2\n* Maximum value, coordinate axis 3 (optional)\nThe coordinate reference system of the values is WGS 84 longitude/latitude\n(https://www.opengis.net/def/crs/OGC/1.3/CRS84) unless a different coordinate\nreference system is specified in the parameter `bbox-crs`.\nFor WGS 84 longitude/latitude the values are in most cases the sequence of\nminimum longitude, minimum latitude, maximum longitude and maximum latitude.\nHowever, in cases where the box spans the antimeridian the first value\n(west-most box edge) is larger than the third value (east-most box edge).\nIf the vertical axis is included, the third and the sixth number are the\nbottom and the top of the 3-dimensional bounding box.\nIf a feature has multiple spatial geometry properties, it is the decision of the\nserver whether only a single spatial geometry property is used to determine\nthe extent or all relevant geometries.\nThis server accepts CRS84 only (no `bbox-crs`); heights are ignored.",
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
        "items-datetime": {
            "name": "datetime",
            "in": "query",
            "description": "Either a date-time or an interval, open or closed. Date and time expressions adhere to RFC 3339. Open intervals are expressed using double-dots.\nExamples:\n* A date-time: \"2018-02-12T23:20:50Z\" * A closed interval: \"2018-02-12T00:00:00Z/2018-03-18T12:31:12Z\" * Open intervals: \"2018-02-12T00:00:00Z/..\" or \"../2018-03-18T12:31:12Z\"\nOnly features that have a temporal property that intersects the value of `datetime` are selected.\nIf a feature has multiple temporal properties, it is the decision of the server whether only a single temporal property is used to determine the extent or all relevant temporal properties.\nA collection whose features carry no time answers datetime with 400; an interval that ends before it starts is 400.",
            "required": false,
            "schema": {"type": "string"},
            "style": "form",
            "explode": false
        },
        "items-limit": {
            "name": "limit",
            "in": "query",
            "description": format!("Defines the maximum number of features to return in a request. Default {DEFAULT_LIMIT}; a larger value than {MAX_LIMIT} returns at most {MAX_LIMIT}; zero or a value that is not an integer is 400."),
            "required": false,
            "schema": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_LIMIT,
                "default": DEFAULT_LIMIT
            },
            "style": "form",
            "explode": false
        },
        "items-offset": {
            "name": "offset",
            "in": "query",
            "description": "Number of matching features to skip: the position the next and prev links page with. Not part of EDR; follow the links rather than building it.",
            "required": false,
            "schema": {"type": "integer", "minimum": 0, "default": 0}
        }
    })
}

/// The `components.schemas` the items responses reference: EDR GeoJSON
/// (`edrFeatureCollectionGeoJSON`, `featureGeoJSON`), whose feature
/// properties require the `edrProperties` members and keep the engine's own.
pub fn openapi_schemas() -> Value {
    let link = json!({
        "type": "object",
        "required": ["href"],
        "properties": {
            "href": {"type": "string"},
            "rel": {"type": "string"},
            "type": {"type": "string"},
            "title": {"type": "string"}
        }
    });
    json!({
        "items-featureCollection": {
            "type": "object",
            "required": ["type", "features"],
            "properties": {
                "type": {"type": "string", "enum": ["FeatureCollection"]},
                "features": {"type": "array", "items": {"$ref": "#/components/schemas/items-feature"}},
                "numberMatched": {"type": "integer", "minimum": 0},
                "numberReturned": {"type": "integer", "minimum": 0},
                "timeStamp": {"type": "string", "format": "date-time"},
                "links": {"type": "array", "items": link.clone()}
            }
        },
        "items-feature": {
            "type": "object",
            "required": ["type", "geometry", "properties"],
            "properties": {
                "type": {"type": "string", "enum": ["Feature"]},
                "id": {"oneOf": [{"type": "string"}, {"type": "integer"}]},
                "geometry": {"type": "object", "nullable": true},
                "properties": {
                    "type": "object",
                    "required": ["datetime", "parameter-name", "label", "edrqueryendpoint"],
                    "properties": {
                        "datetime": {"type": "string", "description": "The collection's temporal extent, start/end (RFC 3339), or empty without one"},
                        "parameter-name": {"type": "array", "items": {"type": "string"}, "description": "The collection's parameter ids"},
                        "label": {"type": "string", "description": "The location's label, else the item's name or id"},
                        "edrqueryendpoint": {"type": "string", "description": "The item's location query, /locations/{locationId}"}
                    }
                },
                "links": {"type": "array", "items": link}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(q: &[(&str, &str)]) -> Vec<(String, String)> {
        q.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn limit_is_a_positive_integer_clamped_to_the_maximum() {
        assert_eq!(parse_limit("1").unwrap(), 1);
        assert_eq!(parse_limit("10000").unwrap(), MAX_LIMIT);
        assert_eq!(parse_limit("10001").unwrap(), MAX_LIMIT);
        assert_eq!(
            parse_limit("999999999999999999999999999999").unwrap(),
            MAX_LIMIT
        );
        for bad in ["0", "000", "-1", "+5", "1.5", "1e3", "", " 5", "ten"] {
            assert!(parse_limit(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn defaults_and_accepted_parameters() {
        let p = ItemsParams::from_pairs(Vec::new()).unwrap();
        assert_eq!((p.limit, p.offset), (DEFAULT_LIMIT, 0));
        assert!(p.bbox.is_none() && p.datetime.is_none());
        let p = ItemsParams::from_pairs(pairs(&[
            ("bbox", "170,-10,-170,10"),
            ("datetime", "2026-01-01T00:00:00Z/.."),
            ("limit", "3"),
            ("offset", "6"),
            ("f", "application/geo json"),
        ]))
        .unwrap();
        assert!(p.bbox.unwrap().crosses_antimeridian());
        assert!(p.datetime.unwrap().end.is_none());
        assert_eq!((p.limit, p.offset), (3, 6));
    }

    #[test]
    fn unknown_duplicate_and_malformed_parameters_are_rejected() {
        for q in [
            vec![("sortby", "name")],
            vec![("crs", "CRS84")],
            vec![("bbox-crs", "CRS84")],
            vec![("name", "Helsinki")],
            vec![("limit", "1"), ("limit", "2")],
            vec![("offset", "-1")],
            vec![("bbox", "1,2,3")],
            vec![("datetime", "2026-01-02T00:00:00Z/2026-01-01T00:00:00Z")],
            vec![("f", "foo")],
            vec![("f", "CoverageJSON")],
        ] {
            assert!(ItemsParams::from_pairs(pairs(&q)).is_err(), "{q:?}");
        }
        let err = ItemsParams::from_pairs(pairs(&[("sortby", "x")]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("bbox, datetime, limit, offset, f"), "{err}");
    }

    #[test]
    fn a_single_item_takes_only_f() {
        assert!(parse_item_pairs(Vec::new()).is_ok());
        assert!(parse_item_pairs(pairs(&[("f", "GeoJSON")])).is_ok());
        assert!(parse_item_pairs(pairs(&[("f", "json"), ("f", "json")])).is_err());
        assert!(parse_item_pairs(pairs(&[("limit", "1")])).is_err());
        assert!(parse_item_pairs(pairs(&[("f", "PNG")])).is_err());
        assert_eq!(
            parse_item_pairs(pairs(&[("f", "html")]))
                .unwrap()
                .as_deref(),
            Some("html")
        );
    }
}
