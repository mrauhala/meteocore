//! Shared OGC API Common HTTP plumbing and HTML representations. Pure search
//! and extent policy stays in ds-core; adapters supply metadata and engine facets.

pub mod caching;
pub mod shared;
pub mod workbench;

use std::sync::Arc;

use axum::extract::{FromRequestParts, Query};
use axum::http::{header, request::Parts, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use ds_core::collection_search::{
    search, CollectionMatch, CollectionParameter, SearchParams, SearchQueryParams, DEFAULT_LIMIT,
    MAX_LIMIT,
};
use ds_core::config::CollectionConfig;
use ds_core::html::{self, LinkView, Wanted};
use ds_core::map_engine::RasterInfo;
use serde_json::{json, Value};

/// Existing Common declarations, centralized to keep all API surfaces aligned.
/// Part 4 is intentionally absent: draft 25-046 retrieved 2026-09-17 requires
/// sd/resolution in addition to the supported search controls. See the
/// repository's docs/ogc-api-common-matrix.md for remaining class-level gaps.
pub const CONFORMANCE_CLASSES: &[&str] = &[
    "http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/core",
    "http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/landing-page",
    "http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/oas30",
    "http://www.opengis.net/spec/ogcapi-common-2/1.0/conf/collections",
    "http://www.opengis.net/spec/ogcapi-common-2/1.0/conf/json",
    "http://www.opengis.net/spec/ogcapi-common-2/1.0/conf/html",
];

/// Paths at which the per-API services are mounted below the external base URL.
/// The server nests each router here; cross-API links target these services.
/// OpenAPI tags. Swagger UI groups operations by tag, and an untagged
/// operation lands in a catch-all "default" group. Data-access operations
/// are tagged with their collection id; these name the rest.
pub mod openapi_tags {
    /// Landing page, conformance declaration and the collection catalog.
    pub const DISCOVERY: &str = "Discovery";
    pub const DISCOVERY_DESCRIPTION: &str =
        "Landing page, conformance declaration and collection catalog (OGC API - Common)";
    /// Tile matrix sets (Tiles `/tileMatrixSets`).
    pub const TILING_SCHEMES: &str = "Tiling schemes";
    pub const TILING_SCHEMES_DESCRIPTION: &str = "Tile matrix sets (OGC 2D TileMatrixSet 2.0)";
}

pub mod mounts {
    pub const EDR: &str = "/edr";
    pub const FEATURES: &str = "/features";
    pub const MAPS: &str = "/maps";
    pub const TILES: &str = "/tiles";
}

/// Registered OGC link relation types. Standards print some as `https://`
/// aliases; the register's canonical `http://` form is what their test suites
/// match. Features and EDR also require the short `conformance` and `data`
/// relations on the landing page, so those are emitted in both forms.
pub mod rel {
    pub const CONFORMANCE: &str = "http://www.opengis.net/def/rel/ogc/1.0/conformance";
    pub const DATA: &str = "http://www.opengis.net/def/rel/ogc/1.0/data";
    pub const MAP: &str = "http://www.opengis.net/def/rel/ogc/1.0/map";
    pub const STYLES: &str = "http://www.opengis.net/def/rel/ogc/1.0/styles";
    pub const LEGEND: &str = "http://www.opengis.net/def/rel/ogc/1.0/legend";
    pub const TILESETS_MAP: &str = "http://www.opengis.net/def/rel/ogc/1.0/tilesets-map";
    pub const TILESETS_VECTOR: &str = "http://www.opengis.net/def/rel/ogc/1.0/tilesets-vector";
    pub const TILING_SCHEME: &str = "http://www.opengis.net/def/rel/ogc/1.0/tiling-scheme";
    pub const TILING_SCHEMES: &str = "http://www.opengis.net/def/rel/ogc/1.0/tiling-schemes";
    pub const GEODATA: &str = "http://www.opengis.net/def/rel/ogc/1.0/geodata";
}

/// Mount path of an API router below the external base URL, supplied to its
/// handlers as a request extension. Links are built from base URL + mount, so
/// one handler set can serve both a per-API service and a shared root (#789).
#[derive(Clone, Copy, Debug)]
pub struct Mount(pub &'static str);

impl Mount {
    /// Absolute URL of the API root (landing page), without a trailing slash.
    pub fn root(self, base: &str) -> String {
        format!("{base}{}", self.0)
    }
}

/// The API a response belongs to, recorded as a response extension for
/// request logs and metrics: routes of a shared root carry no API segment.
#[derive(Clone, Copy, Debug)]
pub struct ApiKind(pub &'static str);

/// Tag every response of `router` with `kind` (see [`ApiKind`]).
pub fn tag_api_kind(router: axum::Router, kind: &'static str) -> axum::Router {
    router.layer(axum::middleware::map_response(
        move |mut response: Response| async move {
            response.extensions_mut().insert(ApiKind(kind));
            response
        },
    ))
}

pub fn conformance_classes(api_classes: &[&'static str]) -> Vec<&'static str> {
    CONFORMANCE_CLASSES
        .iter()
        .chain(api_classes)
        .copied()
        .collect()
}

/// Validated collection request. Reject unsupported/duplicate parameters before
/// accessing engines and return the same structured 400 on every API surface.
pub struct CollectionRequest {
    query: SearchQueryParams,
    search: SearchParams,
    wanted: Wanted,
}

impl<S: Send + Sync> FromRequestParts<S> for CollectionRequest {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Query(pairs) = Query::<Vec<(String, String)>>::from_request_parts(parts, state)
            .await
            .map_err(|_| bad_request("Invalid collection query string"))?;
        let query =
            SearchQueryParams::from_pairs(pairs).map_err(|e| bad_request(&e.to_string()))?;
        let search = query.parse().map_err(|e| bad_request(&e.to_string()))?;
        let accept = parts
            .headers
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok());
        let wanted =
            html::negotiate(query.f.as_deref(), accept).map_err(|e| bad_request(&e.to_string()))?;
        Ok(Self {
            query,
            search,
            wanted,
        })
    }
}

fn bad_request(description: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"code": "BadRequest", "description": description})),
    )
        .into_response()
}

/// One API adapter's view of a collection. Metadata retains API-specific fields;
/// extents must describe the same data as that metadata. Unknown extents remain
/// searchable, per the draft. A single registry snapshot supplies all entries.
pub struct CollectionEntry<'a> {
    pub config: &'a CollectionConfig,
    pub metadata: Value,
    pub bbox: Option<[f64; 4]>,
    pub time: Option<(DateTime<Utc>, DateTime<Utc>)>,
}

/// Filter, page and represent a collection list at `{surface.root}/collections`.
/// The root is the externally resolved absolute API root (including any proxy
/// prefix and the API mount).
pub fn collections_response(
    surface: workbench::Surface<'_>,
    request: CollectionRequest,
    mut entries: Vec<CollectionEntry<'_>>,
) -> Response {
    let url = &format!("{}/collections", surface.root);
    entries.sort_by(|a, b| a.config.id.cmp(&b.config.id));
    let facets: Vec<_> = entries
        .iter()
        .map(|entry| CollectionMatch {
            title: &entry.config.title,
            description: &entry.config.description,
            keywords: &entry.config.keywords,
            // Search the extent a description may advertise: CRS84-normalized,
            // none for a box that describes no area.
            bbox: entry.bbox.and_then(ds_core::geo::crs84_extent),
            time: entry.time,
        })
        .collect();
    let result = search(&facets, &request.search);
    // Explicit formats make both Accept-negotiated HTML and JSON links usable
    // without reproducing the original request headers.
    let (format, alternate, media_type, alternate_type) = match request.wanted {
        Wanted::Json => ("json", "html", "application/json", "text/html"),
        Wanted::Html => ("html", "json", "text/html", "application/json"),
    };
    let href = |offset, f| {
        format!(
            "{url}{}",
            request
                .query
                .query_string_with_format(request.search.limit, offset, f)
        )
    };
    let mut nav = vec![LinkView::new(
        href(request.search.offset, format),
        "self",
        Some("This page"),
    )];
    if result.has_next {
        nav.push(LinkView::new(
            href(result.next_offset, format),
            "next",
            Some("Next page"),
        ));
    }
    if result.has_prev {
        nav.push(LinkView::new(
            href(result.prev_offset, format),
            "prev",
            Some("Previous page"),
        ));
    }
    nav.push(LinkView::new(
        href(request.search.offset, alternate),
        "alternate",
        Some(&format!("This page as {}", alternate.to_uppercase())),
    ));

    let mut response = match request.wanted {
        Wanted::Json => {
            let links: Vec<_> = nav
                .iter()
                .map(|link| {
                    let media_type = if link.rel == "alternate" {
                        alternate_type
                    } else {
                        media_type
                    };
                    json!({
                        "href": link.href, "rel": link.rel, "title": link.title,
                        "type": media_type
                    })
                })
                .collect();
            let collections: Vec<_> = result.page.iter().map(|&i| &entries[i].metadata).collect();
            Json(json!({
                "collections": collections, "numberMatched": result.number_matched,
                "numberReturned": collections.len(), "links": links
            }))
            .into_response()
        }
        Wanted::Html => {
            let metadata: Vec<_> = result
                .page
                .iter()
                .map(|&i| workbench::CollectionView {
                    metadata: &entries[i].metadata,
                    license: entries[i].config.license.as_ref(),
                })
                .collect();
            Html(workbench::collections_html(
                surface,
                &request.query,
                &request.search,
                result.number_matched,
                &metadata,
                &nav,
            ))
            .into_response()
        }
    };
    response
        .headers_mut()
        .append(header::VARY, HeaderValue::from_static("accept"));
    response
}

/// Assemble shared collection fields and representation/license links. Explicit
/// API fields override defaults (EDR instances have their own id/title). The
/// caller supplies the self link and API-specific access links, plus fields such
/// as extent, crs, data_queries or styles. All callers pass a JSON object.
pub fn collection_metadata(
    config: &CollectionConfig,
    fields: Value,
    mut links: Vec<Value>,
) -> Value {
    if let Some(href) = links
        .iter()
        .find(|l| l["rel"] == "self")
        .and_then(|l| l["href"].as_str())
        .map(str::to_owned)
    {
        links.push(
            json!({"href": format!("{href}?f=html"), "rel": "alternate", "type": "text/html", "title": "This collection as HTML"}),
        );
    }
    if let Some((title, url)) = config.license.as_ref().and_then(|l| l.card_link()) {
        // Operator-supplied URLs need not serve HTML; do not invent their type.
        links.push(json!({"href": url, "rel": "license", "title": title}));
    }
    let mut metadata = json!({
        "id": config.id, "title": config.title, "description": config.description,
        "links": links
    });
    if !config.keywords.is_empty() {
        metadata["keywords"] = json!(config.keywords);
    }
    if let (Some(target), Value::Object(fields)) = (metadata.as_object_mut(), fields) {
        target.extend(fields);
    }
    metadata
}

/// Collection member listing the parameters a multi-parameter raster renders:
/// the valid `parameter-name` values of the Maps and Tiles render routes
/// (#279). Neither standard has a parameter concept, so the member and its
/// entries are EDR's, which the selector already borrows: the same data
/// described the same way on every surface.
pub const PARAMETER_NAMES: &str = "parameter_names";

/// The [`PARAMETER_NAMES`] object of a multi-parameter raster, or `None` when
/// `info.parameters` is empty: a single-parameter collection, whose renders
/// ignore `parameter-name`.
///
/// Entries are EDR `Parameter` objects keyed by name, in name order so equal
/// snapshots serialize to equal bytes (ETags, #499). A parameter on its own
/// time axis (`MapEngine::parameter_times`, #819) adds its `extent.temporal`
/// in the collection extent's Common shape; the collection's is the union.
pub fn parameter_names(
    info: &RasterInfo,
    parameter_times: impl Fn(&str) -> Option<Arc<[DateTime<Utc>]>>,
) -> Option<Value> {
    if info.parameters.is_empty() {
        return None;
    }
    let mut parameters: Vec<_> = info.parameters.iter().collect();
    parameters.sort_by(|a, b| a.name.cmp(&b.name));
    let entries = parameters.into_iter().map(|p| {
        let label = if p.title.trim().is_empty() {
            &p.name
        } else {
            &p.title
        };
        let mut entry = json!({"type": "Parameter", "observedProperty": {"label": {"en": label}}});
        let unit = p.unit.trim();
        if !unit.is_empty() {
            entry["unit"] = json!({
                "label": {"en": unit},
                "symbol": {"value": unit, "type": "http://www.opengis.net/def/uom/UCUM/"}
            });
        }
        let extent = parameter_times(&p.name)
            .and_then(|times| ds_core::ogc_extent::build_extent(None, None, "", &times, None));
        if let Some(extent) = extent {
            entry["extent"] = serde_json::to_value(extent).expect("Extent serializes to JSON");
        }
        (p.name.clone(), entry)
    });
    Some(Value::Object(entries.collect()))
}

/// The `parameter-name` query parameter of the Maps and Tiles render routes,
/// one definition so the blocks of the shared root declare one component.
pub fn parameter_name_parameter() -> Value {
    json!({
        "name": "parameter-name", "in": "query", "required": false,
        "schema": {"type": "string"},
        "description": "Parameter of a multi-parameter collection to render: one of the keys of the collection's `parameter_names`; an unknown name returns 400. Without it, the style's parameter or else the collection's default is rendered. A collection that advertises no `parameter_names` has one parameter and ignores this."
    })
}

/// OpenAPI descriptions draw names from the same inventory used by validation.
/// Pure bounds/defaults come from ds-core, not copies in each API crate.
pub fn collection_parameters() -> Value {
    Value::Array(CollectionParameter::ALL.iter().map(|p| {
        let (schema, description) = match p {
            CollectionParameter::Bbox => (json!({"type": "array", "minItems": 4, "maxItems": 6, "items": {"type": "number"}}),
                "Filter collections by horizontal CRS84 extent intersection. Four or six comma-separated numbers; vertical bounds are ignored."),
            CollectionParameter::BboxCrs => (json!({"type": "string", "default": "http://www.opengis.net/def/crs/OGC/1.3/CRS84"}),
                "CRS of bbox. Only CRS84 is supported."),
            CollectionParameter::Datetime => (json!({"type": "string"}),
                "Filter by temporal extent overlap with an RFC 3339 instant or interval (start/end, ../end, start/..). Unknown extents remain eligible."),
            CollectionParameter::Q => (json!({"type": "array", "minItems": 1, "items": {"type": "string"}}),
                "Case-insensitive text search over title, description and keywords. Comma-separated terms are OR; phrases match whole words with normalized whitespace within one property."),
            CollectionParameter::Query => (json!({"type": "array", "minItems": 1, "items": {"type": "string"}}),
                "Text search with comma-separated OR alternatives. Within each alternative, + requires and - excludes the following term or phrase; required terms may match different properties. Phrases match within one property with normalized whitespace. Operators only apply at the start or after whitespace; internal signs are literal. Encode + as %2B. Empty alternatives or operators without an attached term return 400. Combined with q and other filters using AND."),
            CollectionParameter::Limit => (json!({"type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": DEFAULT_LIMIT}),
                "Maximum collections per page. Values above the maximum are clamped."),
            CollectionParameter::Offset => (json!({"type": "integer", "minimum": 0, "default": 0}),
                "Number of matching collections to skip (offset pagination extension)."),
            CollectionParameter::Format => (json!({"type": "string", "enum": ["json", "html"]}),
                "Output representation; overrides Accept. Without f, Accept is used, defaulting to JSON."),
        };
        let mut definition = json!({"name": p.name(), "in": "query", "required": false, "schema": schema, "description": description});
        if matches!(p, CollectionParameter::Bbox | CollectionParameter::Q | CollectionParameter::Query) {
            definition["style"] = json!("form");
            definition["explode"] = json!(false);
        }
        definition
    }).collect())
}

/// Shared collection operation, including representations and validation errors.
pub fn collection_operation() -> Value {
    json!({
        "summary": "List collections",
        "operationId": "getCollections",
        "tags": [openapi_tags::DISCOVERY],
        "description": "Search and page the collections exposed by this API. Unsupported or duplicate query parameters return 400. Supports a subset of OGC API Common Part 4 draft 25-046 (2026-09-17); full Searchable Collections conformance is not claimed.",
        "parameters": collection_parameters(),
        "responses": {
            "200": {
                "description": "Matching collections and representation/pagination links",
                "content": {
                    "application/json": {"schema": {"type": "object", "required": ["collections", "links", "numberMatched", "numberReturned"], "properties": {
                        "collections": {"type": "array", "items": {"type": "object"}},
                        "links": {"type": "array", "items": {"type": "object"}},
                        "numberMatched": {"type": "integer", "minimum": 0},
                        "numberReturned": {"type": "integer", "minimum": 0}
                    }}},
                    "text/html": {"schema": {"type": "string"}}
                }
            },
            "400": {
                "description": "Unsupported, duplicate or invalid collection query parameter",
                "content": {"application/json": {"schema": {"type": "object", "required": ["code", "description"], "properties": {
                    "code": {"type": "string"}, "description": {"type": "string"}
                }}}}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::map_engine::ParameterInfo;

    fn raster(parameters: &[(&str, &str, &str)]) -> RasterInfo {
        RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: None,
            times: vec![],
            parameter: "t".into(),
            unit: "K".into(),
            parameters: parameters
                .iter()
                .map(|&(name, title, unit)| ParameterInfo {
                    name: name.into(),
                    title: title.into(),
                    unit: unit.into(),
                })
                .collect(),
            vertical: None,
            grid_size: None,
            layer_subtitle: None,
            reference_times: vec![],
        }
    }

    #[test]
    fn single_parameter_rasters_list_no_parameter_names() {
        assert_eq!(parameter_names(&raster(&[]), |_| None), None);
    }

    #[test]
    fn parameter_names_label_untitled_parameters_by_name_and_omit_blank_units() {
        let names = parameter_names(
            &raster(&[("t", "Temperature", " K "), ("x", " ", " ")]),
            |_| None,
        )
        .unwrap();
        assert_eq!(names["t"]["observedProperty"]["label"]["en"], "Temperature");
        assert_eq!(names["t"]["unit"]["symbol"]["value"], "K");
        assert_eq!(names["x"]["observedProperty"]["label"]["en"], "x");
        assert!(names["x"].get("unit").is_none());
    }
}
