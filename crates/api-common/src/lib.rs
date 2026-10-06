//! Shared OGC API Common HTTP plumbing and HTML representations. Pure search
//! and extent policy stays in ds-core; adapters supply metadata and engine facets.

pub mod caching;
pub mod map_frame;
pub mod shared;
pub mod subset;
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
use ds_core::map_engine::{CompositeDef, RasterInfo};
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

/// A JSON error response: a status and a `{"code", "description"}` body.
/// Converting it into a response also attaches `code: description` as a
/// [`ds_core::error::ErrorReason`], which the server's request-logging
/// middleware writes as the log line's `error` field (#119). The reason
/// mirrors the client-visible body, so a 5xx whose body carries only a
/// generic description (root CLAUDE.md Critical Rule 11) logs only that too.
pub struct JsonError(pub StatusCode, pub Json<Value>);

impl From<(StatusCode, Json<Value>)> for JsonError {
    fn from((status, body): (StatusCode, Json<Value>)) -> Self {
        Self(status, body)
    }
}

impl IntoResponse for JsonError {
    fn into_response(self) -> Response {
        let Self(status, body) = self;
        let field = |name| body.get(name).and_then(Value::as_str).unwrap_or_default();
        let reason =
            ds_core::error::ErrorReason(format!("{}: {}", field("code"), field("description")));
        let mut response = (status, body).into_response();
        response.extensions_mut().insert(reason);
        response
    }
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
    JsonError(
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

    // One link list for both representations: the HTML page lists it too
    // (OGC API - EDR `/req/html/content` A).
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
    let mut response = match request.wanted {
        Wanted::Json => {
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
                &Value::Array(links),
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
    if let Some(license) = &config.license {
        // Every link carries a `type` (EDR 1.2 `/req/core/rc-collection-info-
        // links` B): `text/html` unless the operator says what the URL serves.
        if let Some((title, url)) = license.card_link() {
            links.push(
                json!({"href": url, "rel": "license", "type": license.link_type(), "title": title}),
            );
        }
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
///
/// The collection's RGB composites (`MapEngine::composites`, #819) are
/// `parameter-name` values too. Their entries have no unit, since a
/// composite has no numeric values, and a `description` naming what each
/// channel reads; their `extent.temporal` is the scans every band has.
pub fn parameter_names(
    info: &RasterInfo,
    composites: &[CompositeDef],
    parameter_times: impl Fn(&str) -> Option<Arc<[DateTime<Utc>]>>,
) -> Option<Value> {
    if info.parameters.is_empty() {
        return None;
    }
    let label = |name: &'_ str, title: &'_ str| -> String {
        if title.trim().is_empty() {
            name.to_string()
        } else {
            title.to_string()
        }
    };
    let extent = |name: &str| {
        parameter_times(name)
            .and_then(|times| ds_core::ogc_extent::build_extent(None, None, "", &times, None))
            .map(|extent| serde_json::to_value(extent).expect("Extent serializes to JSON"))
    };
    let mut entries: Vec<(String, Value)> = Vec::new();
    for p in &info.parameters {
        let mut entry = json!({
            "type": "Parameter",
            "observedProperty": {"label": {"en": label(&p.name, &p.title)}}
        });
        let unit = p.unit.trim();
        if !unit.is_empty() {
            entry["unit"] = json!({
                "label": {"en": unit},
                "symbol": {"value": unit, "type": "http://www.opengis.net/def/uom/UCUM/"}
            });
        }
        if let Some(extent) = extent(&p.name) {
            entry["extent"] = extent;
        }
        entries.push((p.name.clone(), entry));
    }
    for c in composites {
        let channels: Vec<String> = ["red", "green", "blue"]
            .iter()
            .zip(&c.channels)
            .map(|(name, channel)| {
                let bands: Vec<&str> = channel.parameters().collect();
                format!("{name} {}", bands.join(" - "))
            })
            .collect();
        let mut entry = json!({
            "type": "Parameter",
            "description": format!("RGB composite: {}", channels.join(", ")),
            "observedProperty": {"label": {"en": label(&c.name, &c.title)}}
        });
        if let Some(extent) = extent(&c.name) {
            entry["extent"] = extent;
        }
        entries.push((c.name.clone(), entry));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Some(Value::Object(entries.into_iter().collect()))
}

/// The `parameter-name` query parameter of the Maps and Tiles render routes,
/// one definition so the blocks of the shared root declare one component.
pub fn parameter_name_parameter() -> Value {
    json!({
        "name": "parameter-name", "in": "query", "required": false,
        "schema": {"type": "string"},
        "description": "Parameter of a multi-parameter collection to render: one of the keys of the collection's `parameter_names`; an unknown name returns 400. Without it, the style's parameter or else the collection's default is rendered. An RGB composite listed there renders with the `default` style only. A collection that advertises no `parameter_names` has one parameter and ignores this."
    })
}

/// The `quality` query parameter of the Maps and Tiles image routes, a
/// MeteoCore extension; one definition so the blocks of the shared root
/// declare one component. `jpeg_default` is `ds_render::DEFAULT_JPEG_QUALITY`
/// (api-common does not depend on ds-render).
pub fn quality_parameter(jpeg_default: u8) -> Value {
    json!({
        "name": "quality", "in": "query", "required": false,
        "schema": {"type": "integer", "minimum": 1, "maximum": 100},
        "description": format!("Encoder quality for `image/webp` and `image/jpeg`, an integer from 1 to 100 (MeteoCore extension). For WebP, 1–99 is lossy encoding at that quality and 100 is lossless; without it WebP uses the collection's default quality when the server configures one, else lossless. JPEG defaults to {jpeg_default}. Not accepted with `image/png`: 400.")
    })
}

/// The `legend` schema of the Maps and Tiles legend routes: a parameter
/// style's palette legend (`ds_render::legend_json`), or an RGB composite's
/// channel list (`ds_render::composite_legend_json`, #819). One definition
/// so the blocks of the shared root declare one identical component.
pub fn legend_schema() -> Value {
    json!({
        "oneOf": [
            {
                "type": "object",
                "description": "Palette legend of a parameter style",
                "required": ["style", "title", "min", "max", "interpolation", "stops"],
                "properties": {
                    "style": {"type": "string", "description": "Style identifier"},
                    "title": {"type": "string", "description": "Human-readable style title"},
                    "parameter": {"type": "string", "description": "Data parameter the style renders. Omitted when unknown."},
                    "unit": {"type": "string", "description": "Unit of the rendered values. Omitted when unknown."},
                    "min": {"type": "number", "description": "Low end of the value range the colours span"},
                    "max": {"type": "number", "description": "High end of the value range the colours span"},
                    "interpolation": {"type": "string", "enum": ["linear", "step"],
                                      "description": "How colours are produced between stops"},
                    "nodataColor": {"type": "string", "description": "Colour for no-data pixels, when the palette defines one."},
                    "stops": {
                        "type": "array",
                        "description": "Palette colour stops, ascending by value",
                        "items": {
                            "type": "object",
                            "required": ["value", "color"],
                            "properties": {
                                "value": {"type": "number"},
                                "color": {"type": "string", "description": "#RRGGBB, or #RRGGBBAA when not fully opaque"}
                            }
                        }
                    }
                }
            },
            {
                "type": "object",
                "description": "Channel list of an RGB composite, which has no colour bar",
                "required": ["style", "parameter", "title", "channels"],
                "properties": {
                    "style": {"type": "string", "description": "Style identifier, always `default`"},
                    "parameter": {"type": "string", "description": "The composite's `parameter-name`"},
                    "title": {"type": "string", "description": "Human-readable composite title"},
                    "channels": {
                        "type": "array",
                        "description": "Red, green and blue, in that order",
                        "minItems": 3,
                        "maxItems": 3,
                        "items": {
                            "type": "object",
                            "required": ["channel", "label", "parameters", "min", "max", "gamma"],
                            "properties": {
                                "channel": {"type": "string", "enum": ["red", "green", "blue"]},
                                "label": {"type": "string", "description": "What the channel reads: a parameter, or `a - b` for a difference"},
                                "parameters": {"type": "array", "minItems": 1, "maxItems": 2, "items": {"type": "string"},
                                               "description": "The parameters read; two for a difference, first minus second"},
                                "min": {"type": "number", "description": "Value that gives intensity 0"},
                                "max": {"type": "number", "description": "Value that gives full intensity; below `min` inverts the channel"},
                                "gamma": {"type": "number", "description": "Intensity is ((v - min) / (max - min)) ^ (1 / gamma), clamped to 0..1"},
                                "unit": {"type": "string", "description": "Unit of min and max, when every parameter read shares a known one"}
                            }
                        }
                    }
                }
            }
        ]
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
    use ds_core::error::ErrorReason;
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

    /// Every link of a collection document carries `rel` and `type` (EDR 1.2
    /// `/req/core/rc-collection-info-links` B), the license link included:
    /// `text/html` unless the license configures the `type` of its `url`.
    #[test]
    fn license_link_carries_a_type() {
        let links = |license: Value| {
            let config: CollectionConfig = serde_json::from_value(
                json!({"id": "c", "title": "C", "description": "", "license": license}),
            )
            .unwrap();
            let self_link =
                json!({"href": "https://x/c", "rel": "self", "type": "application/json"});
            collection_metadata(&config, json!({}), vec![self_link])["links"].clone()
        };
        let license = |links: &Value| {
            let links = links.as_array().unwrap();
            for link in links {
                assert!(
                    link["rel"].is_string() && link["type"].is_string(),
                    "{link}"
                );
            }
            links
                .iter()
                .find(|l| l["rel"] == "license")
                .unwrap()
                .clone()
        };
        let spdx = license(&links(json!({"title": "CC-BY-4.0"})));
        assert_eq!(spdx["href"], "https://spdx.org/licenses/CC-BY-4.0.html");
        assert_eq!(spdx["type"], "text/html");
        let page = license(&links(json!({"title": "Terms", "url": "https://x/terms"})));
        assert_eq!(page["type"], "text/html");
        let pdf = license(&links(
            json!({"title": "Terms", "url": "https://x/terms.pdf", "type": "application/pdf"}),
        ));
        assert_eq!(pdf["href"], "https://x/terms.pdf");
        assert_eq!(pdf["type"], "application/pdf");
    }

    #[test]
    fn single_parameter_rasters_list_no_parameter_names() {
        assert_eq!(parameter_names(&raster(&[]), &[], |_| None), None);
    }

    #[test]
    fn parameter_names_label_untitled_parameters_by_name_and_omit_blank_units() {
        let names = parameter_names(
            &raster(&[("t", "Temperature", " K "), ("x", " ", " ")]),
            &[],
            |_| None,
        )
        .unwrap();
        assert_eq!(names["t"]["observedProperty"]["label"]["en"], "Temperature");
        assert_eq!(names["t"]["unit"]["symbol"]["value"], "K");
        assert_eq!(names["x"]["observedProperty"]["label"]["en"], "x");
        assert!(names["x"].get("unit").is_none());
    }

    /// RGB composites (#819) are listed with the parameters, in name order:
    /// no unit, a description of the channels and their own time axis.
    #[test]
    fn parameter_names_list_composites_without_a_unit() {
        use ds_core::map_engine::CompositeChannel;
        let channel = |parameter: &str, minus: Option<&str>| CompositeChannel {
            parameter: parameter.into(),
            minus: minus.map(String::from),
            min: 0.0,
            max: 1.0,
            gamma: 1.0,
        };
        let composite = CompositeDef {
            name: "m".into(),
            title: "Mix".into(),
            channels: [
                channel("t", Some("x")),
                channel("x", None),
                channel("t", None),
            ],
        };
        let t0: DateTime<Utc> = "2026-09-25T19:00:00Z".parse().unwrap();
        let names = parameter_names(
            &raster(&[("t", "Temperature", "K"), ("x", "X", "K")]),
            std::slice::from_ref(&composite),
            |name| (name == "m").then(|| Arc::from([t0])),
        )
        .unwrap();
        let keys: Vec<&str> = names
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["m", "t", "x"]);
        let m = &names["m"];
        assert_eq!(m["type"], "Parameter");
        assert_eq!(m["observedProperty"]["label"]["en"], "Mix");
        assert_eq!(
            m["description"],
            "RGB composite: red t - x, green x, blue t"
        );
        assert!(m.get("unit").is_none());
        assert_eq!(
            m["extent"]["temporal"]["interval"],
            json!([[t0.to_rfc3339(), t0.to_rfc3339()]])
        );
        assert!(names["t"].get("description").is_none());
    }

    /// Locks in the contract the request-logging middleware depends on:
    /// every `JsonError` → response carries an `ErrorReason` extension. EDR
    /// and Features route every handler error through this type, so dropping
    /// the `extensions_mut().insert(...)` call would silently re-empty the
    /// `error` field of their log lines (#119).
    #[test]
    fn into_response_attaches_error_reason_extension() {
        let err = JsonError::from((
            StatusCode::NOT_FOUND,
            Json(json!({"code": "NotFound", "description": "Collection 'foo' not found"})),
        ));
        let response = err.into_response();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let reason = response
            .extensions()
            .get::<ErrorReason>()
            .expect("ErrorReason must be attached so request_logging_middleware can pick it up");
        assert_eq!(reason.0, "NotFound: Collection 'foo' not found");
    }

    #[test]
    fn server_error_reason_is_the_redacted_body_text() {
        // Handlers log the underlying error themselves and send a generic
        // 500 body; the reason carries that same text and nothing more.
        let response = JsonError(
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"code": "ServerError", "description": "Internal server error"})),
        )
        .into_response();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let reason = response
            .extensions()
            .get::<ErrorReason>()
            .expect("5xx must also attach ErrorReason");
        assert_eq!(reason.0, "ServerError: Internal server error");
    }

    #[test]
    fn collection_request_rejection_attaches_error_reason() {
        let response = bad_request("Unsupported collection query parameter 'x'");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let reason = response
            .extensions()
            .get::<ErrorReason>()
            .expect("attached");
        assert_eq!(
            reason.0,
            "BadRequest: Unsupported collection query parameter 'x'"
        );
    }
}
