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

use std::collections::HashSet;

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
use ds_core::error::DataServerError;
use ds_core::feature::{Bbox, DatetimeInterval, FeatureQuery};
use ds_core::feature_engine::FeatureEngine;

use crate::handlers::{
    bad_request, execute_query, lookup_collection, map_query_error, request_base_url, server_error,
    with_format_vary, AppState, EdrState, HandlerError,
};
use crate::params::{negotiate_list_format, EdrFormat, NegotiatedFormat};

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

/// The collection's feature engine: 404 for an unknown collection, and for
/// one whose engine serves no features (the query does not exist for it,
/// as for every other unsupported query type, #668).
fn items_engine(
    state: &EdrState,
    id: &str,
) -> Result<std::sync::Arc<dyn FeatureEngine>, HandlerError> {
    lookup_collection(state, id)?;
    state.feature_engines.get(id).cloned().ok_or_else(|| {
        JsonError(
            StatusCode::NOT_FOUND,
            Json(json!({
                "code": "NotFound",
                "description": format!("Collection '{id}' does not support items queries")
            })),
        )
    })
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
    let engine = items_engine(&state, &id)?;
    let params = ItemsParams::from_pairs(pairs).map_err(|e| bad_request(&e))?;
    let format = items_format(params.f.as_deref(), &headers)?;
    let title = state
        .collections
        .get(&id)
        .map(|c| c.title.clone())
        .unwrap_or_default();
    // A collection whose features carry no time cannot filter by it: a 400,
    // not the full set with 200 — the Features API's rule (#682).
    if params.datetime.is_some() && !engine.has_time_dimension() {
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
        let page = engine
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
    let engine = items_engine(&state, &id)?;
    let f = parse_item_pairs(pairs).map_err(|e| bad_request(&e))?;
    let format = items_format(f.as_deref(), &headers)?;
    let title = state
        .collections
        .get(&id)
        .map(|c| c.title.clone())
        .unwrap_or_default();
    let root = edr_root(&state, &headers);
    let response = execute_query(false, move |_budget| {
        let feature = engine
            .get_feature(&item_id)
            .map_err(|e| map_query_error(&e, "Item"))?;
        let doc = feature_to_geojson(&feature, &id, &root, &ResponseCrs::default());
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
                "description": "The collection's features as a GeoJSON FeatureCollection, filtered by bbox and datetime and paged by limit",
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
        "schema": {"type": "string", "enum": ["GeoJSON", "application/geo+json", "json", "application/json", "HTML", "text/html"]},
        "description": "Output format: GeoJSON, the default, or HTML (case-insensitive; encode the plus sign as %2B in application/geo+json). Without f, the Accept header chooses."
    });
    let errors = |not_found: &str| {
        json!({
            "400": {"description": "Bad request"},
            "404": {"description": not_found},
            "500": {"description": "Server error"},
            "503": {"description": "Query capacity exhausted; retry later"},
            "504": {"description": "Query deadline exceeded"}
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
                    "description": "The collection's features, following OGC API - Features item access. Accepts only bbox, datetime, limit, offset and f; any other parameter is a 400.",
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
                        "description": "A page of features",
                        "content": {
                            "application/geo+json": {
                                "schema": {"$ref": "#/components/schemas/items-featureCollection"}
                            }
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
                        "description": "One feature",
                        "content": {
                            "application/geo+json": {
                                "schema": {"$ref": "#/components/schemas/items-feature"}
                            }
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

/// The `components.schemas` the items responses reference.
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
                "properties": {"type": "object", "nullable": true},
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
