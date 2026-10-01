use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use api_common::map_frame::MapCrs;
use api_common::subset::{self, TimeSelection};
use arc_swap::ArcSwap;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Extension;
use axum::Json;
use serde_json::json;

use api_common::workbench::Surface;
use api_common::{mounts, rel, Mount};
use ds_core::config::CollectionConfig;
use ds_core::map_engine::{MapEngine, RasterInfo};
use ds_executor::{RenderOutcome, RenderPhase, RenderPhases, RenderTiming};
use ds_render::{CacheKey, ColorMap, CompositeSpec, RenderedCache, StyleInfo};

use crate::error::MapsError;
use crate::params::{LegendFormat, LegendQueryParams, MapQueryParams, MapRequest, MapTime};

/// Shared state for the OGC API Maps service.
#[derive(Clone)]
pub struct MapsState {
    pub engines: HashMap<String, Arc<dyn MapEngine>>,
    pub collections: HashMap<String, CollectionConfig>,
    /// Map of collection_id -> style_name -> StyleInfo.
    pub styles: HashMap<String, HashMap<String, StyleInfo>>,
    pub render_semaphore: Arc<tokio::sync::Semaphore>,
    pub rendered_cache: Arc<RenderedCache>,
    /// Static fallback base URL for absolute links. Used as-is unless
    /// `trust_proxy_headers` resolves a per-request value.
    pub base_url: String,
    /// Honour reverse-proxy forwarding headers when generating self-links (#12).
    pub trust_proxy_headers: bool,
    /// Collections the Tiles service renders as map tiles. The `tilesets-map`
    /// link is advertised only for these, so it never names a tileset list
    /// that does not exist (`apis` alone cannot tell, #789).
    pub map_tileset_ids: HashSet<String>,
}

pub type AppState = Arc<ArcSwap<MapsState>>;

/// Resolve the absolute base URL for the current request, honouring reverse-proxy
/// forwarding headers when `trust_proxy_headers` is enabled (#12).
pub(crate) fn request_base_url(state: &MapsState, headers: &HeaderMap) -> String {
    ds_core::proxy::resolve_base_url(&state.base_url, state.trust_proxy_headers, |name| {
        headers.get(name).and_then(|v| v.to_str().ok())
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn lookup_engine<'a>(
    state: &'a MapsState,
    id: &str,
) -> Result<(&'a Arc<dyn MapEngine>, &'a CollectionConfig), MapsError> {
    let engine = state
        .engines
        .get(id)
        .ok_or_else(|| MapsError::NotFound(format!("Collection '{id}' not found")))?;
    let config = state
        .collections
        .get(id)
        .ok_or_else(|| MapsError::Internal("Collection config missing".into()))?;
    Ok((engine, config))
}

/// Resolve the style map a request should be styled from.
///
/// Multi-parameter collections register one style map per parameter under
/// `"{collection}/{param}"` alongside the collection-level map, and only the
/// per-parameter map carries that parameter's colormap (from
/// `[[wms.parameters]]`, a bundle parameter entry, or a built-in parameter
/// default). A request that names a parameter must therefore resolve its
/// style there first, falling back to the collection map when the collection
/// has no such layer (single-parameter engines, unknown parameter names that
/// the engine ignores). Mirrors how WMS GetMap resolves `LAYERS=coll/param`.
fn layer_style_map<'a>(
    styles: &'a HashMap<String, HashMap<String, StyleInfo>>,
    collection_id: &str,
    parameter: Option<&str>,
) -> Option<(String, &'a HashMap<String, StyleInfo>)> {
    if let Some(p) = parameter {
        let key = format!("{collection_id}/{p}");
        if let Some(m) = styles.get(&key) {
            return Some((key, m));
        }
    }
    styles
        .get(collection_id)
        .map(|m| (collection_id.to_string(), m))
}

/// How a map render turns engine output into pixels.
enum Paint {
    /// One parameter through its style's colormap.
    Colormap(Arc<dyn ColorMap>),
    /// An RGB composite (#819): its bands from `get_raster_tiles`, composed
    /// by its channels. It has one style, `default`.
    Composite(Arc<CompositeSpec>),
}

/// The RGB composite `parameter-name` names, when the engine serves one
/// (#819).
fn composite_parameter(
    engine: &dyn MapEngine,
    parameter: Option<&str>,
) -> Option<Arc<CompositeSpec>> {
    let name = parameter?;
    engine
        .composites()
        .iter()
        .find(|c| c.name == name)
        .map(|def| Arc::new(CompositeSpec::from(def)))
}

/// Reject a `parameter-name` the collection does not serve: neither a
/// parameter nor an RGB composite. A collection without a parameter list
/// has one parameter and ignores the name, matching `get_raster_tile`.
/// The message lists the valid names in order, so it is deterministic.
fn check_parameter_name(
    engine: &dyn MapEngine,
    info: &RasterInfo,
    collection_id: &str,
    parameter: Option<&str>,
) -> Result<(), MapsError> {
    let Some(pname) = parameter else {
        return Ok(());
    };
    if info.parameters.is_empty() || info.parameters.iter().any(|p| p.name == pname) {
        return Ok(());
    }
    let composites = engine.composites();
    if composites.iter().any(|c| c.name == pname) {
        return Ok(());
    }
    let mut supported: Vec<&str> = info
        .parameters
        .iter()
        .map(|p| p.name.as_str())
        .chain(composites.iter().map(|c| c.name.as_str()))
        .collect();
    supported.sort_unstable();
    Err(MapsError::BadRequest(format!(
        "parameter-name '{pname}' is not available for collection '{collection_id}'. \
         Available: {}",
        supported.join(", ")
    )))
}

/// The 404 for a style other than `default` on an RGB composite.
fn composite_style_not_found(collection_id: &str, composite: &str, style: &str) -> MapsError {
    MapsError::NotFound(format!(
        "Style '{style}' not found for parameter '{composite}' of collection \
         '{collection_id}'. Available: {}",
        ds_render::COMPOSITE_STYLE
    ))
}

/// Cache-Control header value: `immutable` (24 h) only for an explicit
/// `time` that resolved to a timestep, over content the engine never revises
/// (`content_version == 0`); "latest", a `time` the engine has nothing to
/// render for yet (resolved to `None`: its catalog is still empty after a
/// start or reload) and in-place-revised content (a push-fed alert set) get
/// 60 s + revalidation so a browser/CDN holding a pre-revision image asks
/// again.
fn cache_control_value(pinned_time: bool, content_version: u64) -> &'static str {
    if pinned_time && content_version == 0 {
        "public, max-age=86400, immutable"
    } else {
        "public, max-age=60, must-revalidate"
    }
}

/// Resolve the requested representation from `?f=` + the `Accept` header.
fn negotiate(f: Option<&str>, headers: &HeaderMap) -> Result<ds_core::html::Wanted, MapsError> {
    let accept = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok());
    ds_core::html::negotiate(f, accept).map_err(|e| MapsError::BadRequest(e.to_string()))
}

/// Tag a content-negotiated response with `Vary: Accept` so shared caches
/// don't serve the JSON body to a client that asked for HTML (or vice versa).
fn with_vary(mut resp: Response) -> Response {
    // `append` (not `insert`) so a `Vary` set upstream (e.g. compression's
    // `Vary: Accept-Encoding`) isn't clobbered.
    resp.headers_mut().append(
        axum::http::header::VARY,
        axum::http::HeaderValue::from_static("accept"),
    );
    resp
}

/// The link entries advertised for one style: the styled-map endpoint and the
/// machine-readable legend. One builder so the `/collections/{id}` and
/// `/collections/{id}/styles` representations can't drift. `root` is the
/// absolute API root (base URL + mount).
///
/// Relations are the registered OGC ones only (Maps Req 53 styled-map links,
/// the legend recommendation): the short `map`/`legend` forms were dropped once
/// no client depended on them (a bare unregistered relation type is not an
/// RFC 8288 extension relation).
fn style_links(collection_id: &str, style_name: &str, root: &str) -> serde_json::Value {
    let map = format!("{root}/collections/{collection_id}/styles/{style_name}/map");
    let legend = format!("{root}/collections/{collection_id}/styles/{style_name}/legend");
    json!([
        {"href": map, "rel": rel::MAP, "type": "image/png"},
        {"href": legend, "rel": rel::LEGEND, "type": "application/json"}
    ])
}

/// Maps' description of a collection: its standard fields and data-access
/// links (map, styles), without the `self` link. Shared by the per-API
/// service and the shared OGC API root (#789).
pub(crate) fn collection_parts(
    config: &CollectionConfig,
    engine: &dyn MapEngine,
    info: &ds_core::map_engine::RasterInfo,
    styles: Option<&HashMap<String, StyleInfo>>,
    root: &str,
) -> (
    serde_json::Map<String, serde_json::Value>,
    Vec<serde_json::Value>,
) {
    // Every CRS a map renders in, and `bbox-crs`/`subset-crs`/`center-crs`
    // accept.
    let crs_uris = MapCrs::uris();

    let mut style_list = Vec::new();
    if let Some(styles) = styles {
        let mut names: Vec<&String> = styles.keys().collect();
        names.sort_by(|a, b| {
            if a.as_str() == "default" {
                std::cmp::Ordering::Less
            } else if b.as_str() == "default" {
                std::cmp::Ordering::Greater
            } else {
                a.cmp(b)
            }
        });
        for name in names {
            if let Some(s) = styles.get(name) {
                style_list.push(json!({
                    "id": s.name,
                    "title": s.title,
                    "links": style_links(&config.id, &s.name, root)
                }));
            }
        }
    }

    let links = vec![
        // The registered relation Maps Req 46 requires (and the Maps test
        // suite looks for); `…/styles` is the Styles draft's.
        json!({
            "href": format!("{root}/collections/{}/map", config.id),
            "rel": rel::MAP,
            "type": "image/png",
            "title": "Map"
        }),
        json!({
            "href": format!("{root}/collections/{}/styles", config.id),
            "rel": rel::STYLES,
            "type": "application/json",
            "title": "Styles"
        }),
    ];

    let mut fields = serde_json::Map::new();
    fields.insert("dataType".into(), json!("map"));
    fields.insert("crs".into(), json!(crs_uris));
    fields.insert("styles".into(), json!(style_list));
    // The valid `parameter-name` values of the map routes (#279).
    if let Some(parameters) =
        api_common::parameter_names(info, &engine.composites(), |p| engine.parameter_times(p))
    {
        fields.insert(api_common::PARAMETER_NAMES.into(), parameters);
    }
    // Only advertise `storageCrs` when the native CRS has a stable OGC URI.
    // Engines label projected/rotated grids with internal names ("TM",
    // "LAEA", "projected", "rotated_ll", …) that have no URI; emitting CRS84
    // for those would mislabel the storage grid, so omit it instead.
    if let Some(storage_crs) = ds_core::geo::native_crs_uri(&info.native_crs) {
        fields.insert("storageCrs".into(), json!(storage_crs));
    }
    if let Some(extent) = build_extent(info) {
        fields.insert("extent".into(), extent);
    }
    (fields, links)
}

fn build_collection_metadata(
    config: &CollectionConfig,
    engine: &dyn MapEngine,
    info: &ds_core::map_engine::RasterInfo,
    styles: Option<&HashMap<String, StyleInfo>>,
    map_tilesets: bool,
    base_url: &str,
    root: &str,
) -> serde_json::Value {
    let (fields, access) = collection_parts(config, engine, info, styles, root);
    let mut links = vec![json!({
        "href": format!("{root}/collections/{}", config.id),
        "rel": "self",
        "type": "application/json",
        "title": config.title
    })];
    links.extend(access);

    // Map tilesets — rendered (raster) tiles are an OGC API Maps "map
    // tileset", discoverable from the maps collection via the `tilesets-map`
    // relation. Only advertise it when the Tiles service actually registered
    // this collection for raster tiles (the per-API `/tiles` router serves it).
    if map_tilesets {
        links.push(json!({
            "href": format!("{base_url}{}/collections/{}/tiles", mounts::TILES, config.id),
            "rel": rel::TILESETS_MAP,
            "type": "application/json",
            "title": "Map tilesets"
        }));
    }

    api_common::collection_metadata(config, serde_json::Value::Object(fields), links)
}

/// Build the OGC API Common Part 2 `extent` object (spatial, temporal,
/// vertical) including the `grid` resolution descriptors. Returns `None` when
/// the collection advertises no spatial, temporal, or vertical extent.
///
/// The assembly lives in `ds_core::ogc_extent` so Maps, Tiles, and Features
/// share one definition (issue #263).
fn build_extent(info: &ds_core::map_engine::RasterInfo) -> Option<serde_json::Value> {
    let extent = ds_core::ogc_extent::build_extent(
        info.spatial_extent,
        info.grid_size,
        &info.native_crs,
        &info.times,
        info.vertical.as_ref(),
    )?;
    Some(serde_json::to_value(extent).expect("Extent serializes to JSON"))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET {mount}/ — Landing page
pub async fn landing_page(
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    Query(fp): Query<ds_core::html::FormatParams>,
    headers: HeaderMap,
) -> Result<Response, MapsError> {
    use ds_core::html::{LinkView, Wanted};
    let wanted = negotiate(fp.f.as_deref(), &headers)?;
    let state = state.load_full();
    let base = &request_base_url(&state, &headers);
    let root = &mount.root(base);
    let title = "MeteoCore - Maps";
    let description = "Metocean Data Server — OGC API Maps";
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
            rel::CONFORMANCE,
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
            rel::DATA,
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
                format!("{root}/?f=json"),
                "alternate",
                Some("This document as JSON"),
            ));
            Html(api_common::workbench::landing_html(
                Surface {
                    base,
                    root,
                    api: "maps",
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

/// The query parameters of both map routes, in the order Swagger UI lists
/// them.
fn map_parameters() -> Vec<serde_json::Value> {
    [
        "bbox",
        "bbox-crs",
        "subset",
        "subset-crs",
        "center",
        "center-crs",
        "width",
        "height",
        "scale-denominator",
        "crs",
        MAP_DATETIME,
        "transparent",
        "f",
        "quality",
        "elevation",
        "parameter-name",
    ]
    .into_iter()
    .map(|name| json!({"$ref": format!("#/components/parameters/{name}")}))
    .collect()
}

/// Maps' `datetime` component. It differs from Tiles' instant-only
/// `datetime`, and the shared root keeps one component per name.
const MAP_DATETIME: &str = "map-datetime";

/// The responses of both map routes, with the headers of
/// `/req/core/map-response`.
fn map_responses(not_found: &str) -> serde_json::Value {
    let binary = json!({"schema": {"type": "string", "format": "binary"}});
    json!({
        "200": {
            "description": "Map image",
            "headers": {
                "Content-Crs": {
                    "description": "URI of the CRS the map is rendered in",
                    "schema": {"type": "string"}
                },
                "Content-Bbox": {
                    "description": "The rendered map's lower-left and upper-right corners in its CRS, in the CRS's axis order, comma-separated. A geographic box crossing the antimeridian has its first longitude larger than its second.",
                    "schema": {"type": "string"}
                },
                "Content-Datetime": {
                    "description": "The instant rendered (RFC 3339, UTC), on collections with a temporal extent",
                    "schema": {"type": "string"}
                }
            },
            "content": {
                "image/png": binary,
                "image/jpeg": binary,
                "image/webp": binary
            }
        },
        "400": {"description": "Bad request"},
        "404": {"description": not_found},
        "500": {"description": "Server error"}
    })
}

/// Per-collection OpenAPI paths (detail, map, styles, styled map, legend),
/// keyed below the mount `m`.
pub(crate) fn collection_openapi_paths(
    state: &MapsState,
    m: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let mut collection_paths = json!({});
    for config in state.collections.values() {
        let id = &config.id;

        // GET {mount}/collections/{id}
        let detail_path = format!("{m}/collections/{id}");
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

        // GET {mount}/collections/{id}/map
        let map_path = format!("{m}/collections/{id}/map");
        collection_paths[&map_path] = json!({
            "get": {
                "summary": format!("Get map for {}", config.title),
                "operationId": format!("getMap_{id}"),
                "tags": [id],
                "parameters": map_parameters(),
                "responses": map_responses("Collection not found, or no data for the requested time or subset")
            }
        });

        // GET {mount}/collections/{id}/styles
        let styles_path = format!("{m}/collections/{id}/styles");
        collection_paths[&styles_path] = json!({
            "get": {
                "summary": format!("List styles for {}", config.title),
                "operationId": format!("getStyles_{id}"),
                "tags": [id],
                "responses": {
                    "200": {
                        "description": "List of styles",
                        "content": {
                            "application/json": {
                                "schema": {"$ref": "#/components/schemas/styleList"}
                            }
                        }
                    },
                    "404": {"description": "Collection not found"},
                    "500": {"description": "Server error"}
                }
            }
        });

        // GET {mount}/collections/{id}/styles/{styleId}/map
        let styled_map_path = format!("{m}/collections/{id}/styles/{{styleId}}/map");
        let styled_parameters: Vec<serde_json::Value> = std::iter::once(json!({
            "name": "styleId",
            "in": "path",
            "required": true,
            "schema": {"type": "string"},
            "description": "Style identifier"
        }))
        .chain(map_parameters())
        .collect();
        collection_paths[&styled_map_path] = json!({
            "get": {
                "summary": format!("Get styled map for {}", config.title),
                "operationId": format!("getStyledMap_{id}"),
                "tags": [id],
                "parameters": styled_parameters,
                "responses": map_responses("Collection or style not found, or no data for the requested time or subset")
            }
        });

        // GET {mount}/collections/{id}/styles/{styleId}/legend
        let legend_path = format!("{m}/collections/{id}/styles/{{styleId}}/legend");
        collection_paths[&legend_path] = json!({
            "get": {
                "summary": format!("Get style legend for {}", config.title),
                "operationId": format!("getStyleLegend_{id}"),
                "tags": [id],
                "parameters": [
                    {
                        "name": "styleId",
                        "in": "path",
                        "required": true,
                        "schema": {"type": "string"},
                        "description": "Style identifier"
                    },
                    {
                        "name": "f",
                        "in": "query",
                        "required": false,
                        "schema": {"type": "string", "default": "json", "enum": ["json", "application/json", "png", "image/png"]},
                        "description": "Legend representation: the machine-readable description (default) or a rendered legend image."
                    },
                    {
                        "name": "parameter-name",
                        "in": "query",
                        "required": false,
                        "schema": {"type": "string"},
                        "description": "Describe the style of this parameter's layer, matching `parameter-name` on the map routes. Falls back to the collection-level style when the collection has no per-parameter layer."
                    }
                ],
                "responses": {
                    "200": {
                        "description": "Legend description or image",
                        "content": {
                            "application/json": {
                                "schema": {"$ref": "#/components/schemas/legend"}
                            },
                            "image/png": {
                                "schema": {"type": "string", "format": "binary"}
                            }
                        }
                    },
                    "400": {"description": "Bad request"},
                    "404": {"description": "Collection or style not found"},
                    "500": {"description": "Server error"}
                }
            }
        });
    }

    match collection_paths {
        serde_json::Value::Object(paths) => paths,
        _ => serde_json::Map::new(),
    }
}

/// OpenAPI components (parameters, schemas) referenced by the Maps paths.
pub(crate) fn openapi_components() -> serde_json::Value {
    json!({
        "parameters": {
            // The standard's own OpenAPI fragments (OGC 20-058 requirement
            // texts; `subset` and `center` from its building blocks), copied
            // verbatim, each description followed by this server's rules.
            "bbox": {
                "name": "bbox",
                "in": "query",
                "description": "Bounding box of the rendered map. The bounding box is provided as four or six coordinates\n\n* Lower left corner, coordinate axis 1\n* Lower left corner, coordinate axis 2\n* Minimum value, coordinate axis 3 (optional)\n* Upper right corner, coordinate axis 1\n* Upper right corner, coordinate axis 2\n* Maximum value, coordinate axis 3 (optional)\n\nThe coordinate reference system and axis order of the values are indicated in the `bbox-crs` parameter or if the parameter is missing in https://www.opengis.net/def/crs/OGC/1.3/CRS84\n\nThis server's maps are two-dimensional: give four values. For WGS 84 longitude/latitude the values are in most cases the sequence of minimum longitude, minimum latitude, maximum longitude and maximum latitude. However, in cases where the box spans the antimeridian the first value (west-most box edge) is larger than the third value (east-most box edge). Not with `center` or a spatial `subset` (400). Without `bbox`, `center` or a spatial `subset` the map covers the collection's whole spatial extent.",
                "required": false,
                "schema": {
                    "type": "array",
                    "oneOf": [
                        {"minItems": 4, "maxItems": 4},
                        {"minItems": 6, "maxItems": 6}
                    ],
                    "items": {"type": "number", "format": "double"}
                },
                "style": "form",
                "explode": false
            },
            "bbox-crs": {
                "name": "bbox-crs",
                "in": "query",
                "description": "A URI (or safe CURIE) of the coordinate reference system for the coordinates specified in the `bbox` parameter. The valid values are [OGC:CRS84], the native (storage) CRS (if different), or the output `crs` (if specified).\n\nThis server accepts every CRS a collection lists in `crs`, as URI, safe CURIE or short identifier (`CRS:84`, `EPSG:3857`, …); the values follow the CRS's axis order (EPSG:4326 latitude first, EPSG:3035 northing first). Ignored without `bbox`.",
                "required": false,
                "schema": {"type": "string"},
                "example": "https://www.opengis.net/def/crs/OGC/1.3/CRS84"
            },
            "subset": {
                "name": "subset",
                "in": "query",
                "description": "Retrieve only part of the data by slicing or trimming along one or more axis\nFor trimming: {axisAbbrev}({low}:{high}) (preserves dimensionality)\nFor slicing:  {axisAbbrev}({value})      (reduces dimensionality)\nAn asterisk (`*`) can be used instead of {low} or {high} to indicate the minimum/maximum value.\nFor a temporal dimension, a single asterisk can be used to indicate the high value.\nSupport for `*` is required for time, but optional for spatial and other dimensions.\n\nAxes: `Lon` and `Lat` (also `Long`, `Longitude`, `Latitude`) in a geographic `subset-crs`, `E` and `N` (also `X`, `Easting`, `Y`, `Northing`) in a projected one, each trimmed with an interval; `*` is the collection extent's edge, and an axis left out keeps the extent. A `Lon` low greater than its high crosses the antimeridian. `time` (also `t`) takes double-quoted RFC 3339 values or the partial forms `yyyy`, `yyyy-mm`, `yyyy-mm-dd`, `yyyy-mm-ddThhZ` and `yyyy-mm-ddThh:mmZ`: an instant is snapped like `datetime`, an interval, a partial value or `*` renders the latest time inside it. Any other axis is a 400; an interval entirely outside its axis' valid values, or holding no time, a 404. Spatial axes not with `bbox` or `center`, `time` not with `datetime` (400). Example: `subset=Lon(19:32),Lat(59:70)`.",
                "style": "form",
                "explode": false,
                "required": false,
                "schema": {
                    "type": "array",
                    "items": {"type": "string"}
                }
            },
            "subset-crs": {
                "name": "subset-crs",
                "in": "query",
                "description": "A URI (or safe CURIE) of the coordinate reference system for the coordinates specified in the `subset` parameter. The valid values are [OGC:CRS84], the native (storage) CRS (if different), or the output `crs` (if specified).\n\nThis server accepts every CRS a collection lists in `crs`, as URI, safe CURIE or short identifier. Ignored without a spatial `subset` axis.",
                "required": false,
                "schema": {"type": "string"},
                "example": "https://www.opengis.net/def/crs/OGC/1.3/CRS84"
            },
            "center": {
                "name": "center",
                "in": "query",
                "description": "Coordinates of center point for subsetting, in conjunction with the `width` and/or `height` parameters, taking into consideration the scale and display resolution of the map. The center coordinates are comma-separated and interpreted as [ogc:CRS84], unless the `center-crs` parameter specifies otherwise.\n\nThe coordinates follow the `center-crs` axis order. Without `scale-denominator` the map is at the collection's native resolution. An omitted `width` or `height` is the other's value, both omitted 1024. Not with `bbox` or a spatial `subset` (400).",
                "required": false,
                "style": "form",
                "explode": false,
                "schema": {
                    "type": "array",
                    "minItems": 2,
                    "maxItems": 2,
                    "items": {"type": "number"}
                }
            },
            "center-crs": {
                "name": "center-crs",
                "in": "query",
                "description": "A URI (or safe CURIE) of the coordinate reference system for the coordinates specified in the `center` parameter. The valid values are [OGC:CRS84], the native (storage) CRS (if different), or the output `crs` (if specified).\n\nThis server accepts every CRS a collection lists in `crs`, as URI, safe CURIE or short identifier. Ignored without `center`.",
                "required": false,
                "schema": {"type": "string"},
                "example": "https://www.opengis.net/def/crs/OGC/1.3/CRS84"
            },
            "width": {
                "name": "width",
                "in": "query",
                "description": "Width of the viewport in pixel units to present the response (the map subset).\n\nA positive integer up to 8000, and `width` × `height` up to 64000000. Omitted over an area (`bbox`, a spatial `subset` or the collection's extent): the width that keeps pixels square, 1024 for the longer side when `height` is omitted too, or the width `scale-denominator` gives. With `center`, or `scale-denominator` and no area, it sets the map's extent at that scale; omitted, it is `height`'s value, else 1024.",
                "required": false,
                "style": "form",
                "schema": {"type": "number", "maximum": 8000}
            },
            "height": {
                "name": "height",
                "in": "query",
                "description": "Height of the viewport in pixel units to present the response (the map subset).\n\nA positive integer up to 8000, and `width` × `height` up to 64000000. Omitted over an area (`bbox`, a spatial `subset` or the collection's extent): the height that keeps pixels square, 1024 for the longer side when `width` is omitted too, or the height `scale-denominator` gives. With `center`, or `scale-denominator` and no area, it sets the map's extent at that scale; omitted, it is `width`'s value, else 1024.",
                "required": false,
                "style": "form",
                "schema": {"type": "number", "maximum": 8000}
            },
            "scale-denominator": {
                "name": "scale-denominator",
                "in": "query",
                "description": "Number of units in the real-world corresponding to one such unit on the display.\n\nA positive number, on the standard 0.28 mm pixel: one pixel spans `scale-denominator` × 0.28 mm on the ground at the map's centre (ground metres, not CRS units). With `bbox` or a spatial `subset` it sets the map's size, and is a 400 together with `width` or `height`; otherwise it sets the map's extent around `center`, or the centre of the collection's extent.",
                "required": false,
                "style": "form",
                "schema": {"type": "number"}
            },
            "crs": {
                "name": "crs",
                "in": "query",
                "description": "A coordinate reference system of the map response. A list of all supported CRS values can be found under the collection metadata.\n\nA URI the collection lists in `crs`, its safe CURIE, or a short identifier: `CRS:84`, `EPSG:4326`, `EPSG:3857`, `EPSG:3067`, `EPSG:3035`. Default: the collection's `storageCrs` when it is one of these, else CRS84. `Content-Crs` names the CRS of the response.",
                "required": false,
                "schema": {"type": "string"},
                "example": "https://www.opengis.net/def/crs/OGC/1.3/CRS84"
            },
            "map-datetime": {
                "name": "datetime",
                "in": "query",
                "description": "Either a date-time or an interval. Date and time expressions adhere to RFC 3339, section 5.6. Intervals may be bounded or half-bounded (double-dots at start or end).\n\nAn instant is snapped to an available time step; an interval renders the latest time step inside it, and is a 404 when it holds none. Without `datetime` or a `time` subset the map shows the collection's default (normally latest) time. `Content-Datetime` reports the instant rendered. Not with `subset=time(…)` (400). Examples: `2018-02-12T23:20:50Z`, `2018-02-12T00:00:00Z/2018-03-18T12:31:12Z`, `2018-02-12T00:00:00Z/..`.",
                "required": false,
                "schema": {"type": "string"},
                "style": "form",
                "explode": false
            },
            "transparent": {
                "name": "transparent",
                "in": "query",
                "required": false,
                "schema": {"type": "string"},
                "description": "Transparency support"
            },
            "f": {
                "name": "f",
                "in": "query",
                "required": false,
                "schema": {
                    "type": "string",
                    "default": "image/png",
                    "enum": ["image/png", "image/jpeg", "image/webp"]
                },
                "description": "Output format. `image/png` auto-emits an 8-bit indexed-palette PNG (~3–4× smaller) for colormap-rendered layers; falls back to 32-bit RGBA above 256 distinct colours."
            },
            "elevation": {
                "name": "elevation",
                "in": "query",
                "required": false,
                "schema": {"type": "number"},
                "description": "Vertical level (e.g. radar elevation angle). Only valid for collections with a vertical dimension."
            },
            "quality": api_common::quality_parameter(ds_render::DEFAULT_JPEG_QUALITY),
            "parameter-name": api_common::parameter_name_parameter()
        },
        "schemas": {
            "styleList": {
                "type": "object",
                "properties": {
                    "styles": {
                        "type": "array",
                        "items": {"$ref": "#/components/schemas/style"}
                    },
                    "links": {"type": "array", "items": {"$ref": "#/components/schemas/link"}}
                }
            },
            "style": {
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "title": {"type": "string"},
                    "links": {"type": "array", "items": {"$ref": "#/components/schemas/link"}}
                }
            },
            "legend": api_common::legend_schema(),
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
    })
}

/// GET {mount}/api — OpenAPI 3.0.3 definition. Path keys include the mount.
pub async fn api_definition(
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
) -> impl IntoResponse {
    let state = state.load_full();
    let m = mount.0;
    let collection_paths = serde_json::Value::Object(collection_openapi_paths(&state, m));
    let mut paths = json!({
        format!("{m}/"): {
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
        format!("{m}/conformance"): {
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
        format!("{m}/collections"): {"get": api_common::collection_operation()}
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
            "title": "MeteoCore - OGC API Maps",
            "version": "1.0.0",
            "description": "OGC API - Maps implementation"
        },
        "paths": paths,
        "components": openapi_components()
    });

    Json(openapi)
}

/// GET {mount}/api/docs — Swagger UI
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
            "MeteoCore - Maps API",
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

/// OGC API - Maps classes this implementation declares, on either surface.
pub(crate) const CONFORMANCE: &[&str] = &[
    "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/core",
    "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/collection-map",
    "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/styled-map",
    "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/spatial-subsetting",
    "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/scaling",
    "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/datetime",
    "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/crs",
    "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/png",
    "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/jpeg",
];

/// GET {mount}/conformance
pub async fn conformance(
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    Query(fp): Query<ds_core::html::FormatParams>,
    headers: HeaderMap,
) -> Result<Response, MapsError> {
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
                Surface {
                    base,
                    root,
                    api: "maps",
                },
                &classes,
                &nav,
            ))
            .into_response()
        }
    }))
}

/// GET {mount}/collections
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
                    "collection has no registered map engine; omitting from /collections"
                );
                return None;
            };
            let info = engine.raster_info_shared();
            let metadata = build_collection_metadata(
                config,
                engine.as_ref(),
                &info,
                state.styles.get(&config.id),
                state.map_tileset_ids.contains(&config.id),
                base,
                root,
            );
            Some(api_common::CollectionEntry {
                config,
                metadata,
                bbox: info.spatial_extent,
                time: info.times.first().copied().zip(info.times.last().copied()),
            })
        })
        .collect();
    api_common::collections_response(
        Surface {
            base,
            root,
            api: "maps",
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
    Query(fp): Query<ds_core::html::FormatParams>,
    headers: HeaderMap,
) -> Result<Response, MapsError> {
    use ds_core::html::Wanted;
    let wanted = negotiate(fp.f.as_deref(), &headers)?;
    let state = state.load_full();
    let (engine, config) = lookup_engine(&state, &id)?;
    let base = &request_base_url(&state, &headers);
    let root = &mount.root(base);
    Ok(with_vary(match wanted {
        Wanted::Json => {
            let info = engine.raster_info_shared();
            let styles = state.styles.get(&id);
            let map_tilesets = state.map_tileset_ids.contains(&id);
            Json(build_collection_metadata(
                config,
                engine.as_ref(),
                &info,
                styles,
                map_tilesets,
                base,
                root,
            ))
            .into_response()
        }
        Wanted::Html => {
            let metadata = build_collection_metadata(
                config,
                engine.as_ref(),
                &engine.raster_info_shared(),
                state.styles.get(&id),
                state.map_tileset_ids.contains(&id),
                base,
                root,
            );
            Html(api_common::workbench::collection_html(
                Surface {
                    base,
                    root,
                    api: "maps",
                },
                &metadata,
                config.license.as_ref(),
            ))
            .into_response()
        }
    }))
}

/// GET {mount}/collections/{id}/styles
pub async fn styles(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, MapsError> {
    let state = state.load_full();
    let (_engine, config) = lookup_engine(&state, &id)?;
    let root = &mount.root(&request_base_url(&state, &headers));

    let mut style_list = Vec::new();
    if let Some(layer_styles) = state.styles.get(&id) {
        let mut names: Vec<&String> = layer_styles.keys().collect();
        names.sort_by(|a, b| {
            if a.as_str() == "default" {
                std::cmp::Ordering::Less
            } else if b.as_str() == "default" {
                std::cmp::Ordering::Greater
            } else {
                a.cmp(b)
            }
        });
        for name in names {
            if let Some(s) = layer_styles.get(name) {
                style_list.push(json!({
                    "id": s.name,
                    "title": s.title,
                    "links": style_links(&config.id, &s.name, root)
                }));
            }
        }
    }

    Ok(Json(json!({
        "styles": style_list,
        "links": [
            {
                "href": format!("{root}/collections/{}/styles", id),
                "rel": "self",
                "type": "application/json"
            }
        ]
    })))
}

/// GET {mount}/collections/{id}/styles/{styleId}/legend
///
/// `?f=json` (the default) returns the machine-readable legend — palette
/// stops, value range, interpolation — so a client can draw its own legend;
/// `?f=png` returns the rendered legend image. Cacheable for a day but NOT
/// immutable: palettes are hot-reloadable.
pub async fn style_legend(
    Path((id, style_id)): Path<(String, String)>,
    Query(params): Query<LegendQueryParams>,
    State(state): State<AppState>,
) -> Result<Response, MapsError> {
    let state = state.load_full();
    let (engine, _config) = lookup_engine(&state, &id)?;
    let format = params.validate()?;

    // An RGB composite's legend is its channel list (#819).
    if let Some(spec) = composite_parameter(engine.as_ref(), params.parameter_name.as_deref()) {
        if style_id != ds_render::COMPOSITE_STYLE {
            return Err(composite_style_not_found(&id, &spec.name, &style_id));
        }
        return composite_legend(engine.as_ref(), spec, format).await;
    }

    // `?parameter-name=` selects the per-parameter style layer, so the legend
    // a client draws matches the pixels the map/tile routes render for that
    // same parameter.
    let (_, layer_styles) =
        layer_style_map(&state.styles, &id, params.parameter_name.as_deref())
            .ok_or_else(|| MapsError::NotFound(format!("Collection '{id}' not found")))?;
    let style_info = layer_styles.get(&style_id).ok_or_else(|| {
        MapsError::NotFound(format!(
            "Style '{style_id}' not found for collection '{id}'. Available: {}",
            layer_styles.keys().cloned().collect::<Vec<_>>().join(", ")
        ))
    })?;

    let info = engine.raster_info_shared();
    // Mirror the render path's validation: an unknown `parameter-name` must
    // 400 with the available list, not fall back to the collection style and
    // emit a legend labelled with a parameter that doesn't exist.
    check_parameter_name(
        engine.as_ref(),
        &info,
        &id,
        params.parameter_name.as_deref(),
    )?;
    let (parameter, unit) =
        ds_render::legend_parameter_unit(style_info, &info, params.parameter_name.as_deref());

    match format {
        LegendFormat::Json => {
            let body = ds_render::legend_json(style_info, parameter.as_deref(), unit.as_deref());
            let mut response = Json(body).into_response();
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static(ds_render::LEGEND_CACHE_CONTROL),
            );
            response.headers_mut().insert(
                header::HeaderName::from_static("x-content-type-options"),
                axum::http::HeaderValue::from_static("nosniff"),
            );
            Ok(response)
        }
        LegendFormat::Png => {
            let colormap = style_info.colormap.clone();
            let (min, max) = (style_info.min, style_info.max);
            let title = ds_render::legend_title(style_info, parameter.as_deref(), unit.as_deref());
            let bytes = tokio::task::spawn_blocking(move || {
                ds_render::render_legend(
                    colormap.as_ref(),
                    min,
                    max,
                    ds_render::LEGEND_DEFAULT_WIDTH,
                    ds_render::LEGEND_DEFAULT_HEIGHT,
                    ds_render::ImageFormat::Png,
                    title.as_deref(),
                )
            })
            .await
            .map_err(|e| MapsError::Internal(format!("Legend render failed: {e}")))?
            .map_err(|e| MapsError::Internal(format!("Legend render error: {e}")))?;

            Ok((
                [
                    (header::CONTENT_TYPE, "image/png"),
                    (header::CACHE_CONTROL, ds_render::LEGEND_CACHE_CONTROL),
                    (
                        header::HeaderName::from_static("x-content-type-options"),
                        "nosniff",
                    ),
                ],
                bytes,
            )
                .into_response())
        }
    }
}

/// The legend of an RGB composite (#819): its channel list, each channel's
/// bands, range, gamma and unit, with no colour bar. The units are the
/// bands' own.
async fn composite_legend(
    engine: &dyn MapEngine,
    spec: Arc<CompositeSpec>,
    format: LegendFormat,
) -> Result<Response, MapsError> {
    let info = engine.raster_info_shared();
    let units: Vec<Option<String>> = ds_render::composite_units(&spec, &info)
        .into_iter()
        .map(|unit| unit.map(str::to_string))
        .collect();
    let (content_type, body) = match format {
        LegendFormat::Json => {
            let units: Vec<Option<&str>> = units.iter().map(Option::as_deref).collect();
            let mut response =
                Json(ds_render::composite_legend_json(&spec, &units)).into_response();
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static(ds_render::LEGEND_CACHE_CONTROL),
            );
            response.headers_mut().insert(
                header::HeaderName::from_static("x-content-type-options"),
                axum::http::HeaderValue::from_static("nosniff"),
            );
            return Ok(response);
        }
        LegendFormat::Png => {
            let bytes = tokio::task::spawn_blocking(move || {
                let units: Vec<Option<&str>> = units.iter().map(Option::as_deref).collect();
                ds_render::render_composite_legend(
                    &spec,
                    &units,
                    ds_render::LEGEND_DEFAULT_WIDTH,
                    ds_render::LEGEND_DEFAULT_HEIGHT,
                    ds_render::ImageFormat::Png,
                )
            })
            .await
            .map_err(|e| MapsError::Internal(format!("Legend render failed: {e}")))?
            .map_err(|e| MapsError::Internal(format!("Legend render error: {e}")))?;
            ("image/png", bytes)
        }
    };
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, ds_render::LEGEND_CACHE_CONTROL),
            (
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff",
            ),
        ],
        body,
    )
        .into_response())
}

/// GET {mount}/collections/{id}/map — render map with default style
pub async fn get_map(
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<MapQueryParams>,
    RawQuery(query): RawQuery,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, MapsError> {
    let subsets = subset::query_values(query.as_deref(), "subset");
    render_map(&id, "default", params, &subsets, headers, state).await
}

/// GET {mount}/collections/{id}/styles/{styleId}/map — render map with named style
pub async fn get_styled_map(
    headers: HeaderMap,
    Path((id, style_id)): Path<(String, String)>,
    Query(params): Query<MapQueryParams>,
    RawQuery(query): RawQuery,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, MapsError> {
    let subsets = subset::query_values(query.as_deref(), "subset");
    render_map(&id, &style_id, params, &subsets, headers, state).await
}

/// The instant a map renders for, before the engine snaps it (#507).
///
/// - No time: the engine's default, else the parameter's (else the
///   collection's) latest time.
/// - An instant: as given; the engine snaps it to a timestep
///   (`/per/datetime/closest`). From `subset=time(…)`, one outside the
///   time axis is a 404 (`/req/datetime/subset-definition` D).
/// - An interval (`datetime=a/b`, `subset=time("a":"b")`, a partial date,
///   `*`): the latest time inside it; none inside is a 404. On a collection
///   with no time axis it selects nothing, like the instant it ignores.
fn requested_time(
    request: &MapRequest,
    engine: &dyn MapEngine,
    info: &RasterInfo,
    parameter: Option<&str>,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, MapsError> {
    let parameter_axis = parameter.and_then(|p| engine.parameter_times(p));
    let axis: &[chrono::DateTime<chrono::Utc>] = parameter_axis.as_deref().unwrap_or(&info.times);
    let outside = |what: String| {
        MapsError::NotFound(format!(
            "No data for {what}: the collection's time axis has none"
        ))
    };
    match request.time {
        None => Ok(ds_core::map_engine::default_request_time(
            engine, info, parameter,
        )),
        Some(MapTime {
            selection: TimeSelection::Instant(t),
            from_subset,
        }) => {
            if let (true, Some(first), Some(last)) = (from_subset, axis.first(), axis.last()) {
                if t < *first || t > *last {
                    return Err(outside(format!("subset time {}", subset::rfc3339(t))));
                }
            }
            Ok(Some(t))
        }
        Some(_) if axis.is_empty() => Ok(ds_core::map_engine::default_request_time(
            engine, info, parameter,
        )),
        Some(MapTime { selection, .. }) => selection
            .latest_in(axis)
            .map(Some)
            .ok_or_else(|| outside("the requested time interval".to_string())),
    }
}

/// Shared rendering logic for get_map and get_styled_map.
async fn render_map(
    collection_id: &str,
    style_name: &str,
    params: MapQueryParams,
    subsets: &[String],
    headers: HeaderMap,
    state: AppState,
) -> Result<impl IntoResponse, MapsError> {
    let state = state.load_full();
    let (engine, config) = lookup_engine(&state, collection_id)?;

    let validated = params.validate(subsets)?;
    // The format as encoded: an explicit `quality`, else for WebP the
    // collection's `[wms] webp_quality`, else the format default (JPEG 85,
    // lossless WebP). It keys the rendered cache, so a lossy and a lossless
    // image of one view never alias.
    let format = validated
        .format
        .with_quality(validated.quality, config.webp_quality());

    // An RGB composite (#819) has no style map: its colours come from its
    // channels, and its one style is `default`.
    let composite = composite_parameter(engine.as_ref(), validated.parameter_name.as_deref());

    // Look up style. A `?parameter-name=` request styles from that
    // parameter's own layer when the collection registers one — otherwise a
    // per-parameter colormap would be unreachable through Maps and every
    // parameter would render with the collection-level default.
    let (style_layer_key, paint, style_parameter) = match &composite {
        Some(spec) => {
            if style_name != ds_render::COMPOSITE_STYLE {
                return Err(composite_style_not_found(
                    collection_id,
                    &spec.name,
                    style_name,
                ));
            }
            (
                format!("{collection_id}/{}", spec.name),
                Paint::Composite(spec.clone()),
                None,
            )
        }
        None => {
            let (style_layer_key, layer_styles) = layer_style_map(
                &state.styles,
                collection_id,
                validated.parameter_name.as_deref(),
            )
            .ok_or_else(|| {
                MapsError::NotFound(format!("Collection '{collection_id}' not found"))
            })?;

            let style_info = layer_styles.get(style_name).ok_or_else(|| {
                MapsError::NotFound(format!(
                    "Style '{style_name}' not found for collection '{collection_id}'. Available: {}",
                    layer_styles.keys().cloned().collect::<Vec<_>>().join(", ")
                ))
            })?;
            (
                style_layer_key,
                Paint::Colormap(style_info.colormap.clone()),
                style_info.parameter.clone(),
            )
        }
    };

    let content_type = format.content_type();
    // Only an instant pins the image: `*` and intervals follow new data.
    let has_explicit_time = matches!(
        validated.time,
        Some(MapTime {
            selection: TimeSelection::Instant(_),
            ..
        })
    );

    // Share one metadata snapshot across default-time resolution, the map
    // area and parameter-name validation.
    let raster_info = engine.raster_info_shared();

    // Parameter selection precedence: ?parameter-name= wins over style.parameter.
    // Validate against the engine's advertised parameters and composites
    // when the query supplied one — passing through unrecognised names
    // produces a confusing "default parameter rendered with wrong colormap"
    // rather than a clear 400.
    check_parameter_name(
        engine.as_ref(),
        &raster_info,
        collection_id,
        validated.parameter_name.as_deref(),
    )?;
    // For a composite, its name: the engine resolves its time axis, the
    // scans every band has, like a parameter's.
    let effective_parameter = validated.parameter_name.clone().or(style_parameter);

    let time = requested_time(
        &validated,
        engine.as_ref(),
        &raster_info,
        effective_parameter.as_deref(),
    )?;
    // #521: resolve the run axis to the CONCRETE run the engine will render
    // before the cache key is built. The Maps `reference_time` query
    // parameter is still a follow-up (#337 Phase 4) — the handler never pins
    // a run — but the no-TTL rendered cache must key on the run actually
    // rendered: keyed as `None`, the first-rendered run's pixels would keep
    // serving after a newer run re-covers the same valid times (acute for
    // nowcast generations, latent for NWP). Asking the engine (not
    // `reference_times.last()`) preserves GRIB's cross-run fallback when the
    // newest run doesn't cover the valid time yet. Engines without runs keep
    // the identity default (`None` stays `None`).
    let reference_time = engine.resolve_reference_time(time, None);
    // #507: snap to the exact timestep the engine will render before the
    // cache key is built — a not-yet-ingested datetime must cache the
    // previous timestep's pixels under the PREVIOUS timestep's key. A
    // parameter with its own time axis snaps on that axis.
    let time = engine.resolve_parameter_time(effective_parameter.as_deref(), time, reference_time);

    // Reject an `elevation` against a collection with no vertical axis
    // rather than silently rendering the default layer.
    if validated.z.is_some() && raster_info.vertical.is_none() {
        return Err(MapsError::BadRequest(format!(
            "collection '{collection_id}' has no vertical dimension; \
             the `elevation` parameter is not supported"
        )));
    }

    // The area and size, and the headers that report them
    // (`/req/core/map-response`): computed from the request and the
    // resolved time, so a cache hit carries the same values as the render.
    let view = validated.view(&raster_info, collection_id)?;
    let output = view.frame.crs();
    let content_crs = output.uri();
    let content_bbox = view.content_bbox.clone();
    // `Content-Datetime` on a collection with a temporal extent: the instant
    // rendered, not the one requested.
    let content_datetime = time
        .filter(|_| !raster_info.times.is_empty())
        .map(subset::rfc3339);

    // Build cache key
    let cache_key = CacheKey {
        // The RESOLVED style-layer key ("{coll}" or "{coll}/{param}"), so a
        // parameter-layer style and a collection style with the same name
        // and data parameter can never alias one cached image.
        layer: style_layer_key,
        style: style_name.to_string(),
        format,
        crs: output.code().to_string(),
        // Projected output renders over the projected-metres bbox carried in
        // `output_crs`, not the WGS84 envelope in `view.bbox`; key on the
        // metres so two projected requests sharing an envelope don't collide
        // (#267 review).
        bbox: match &view.output_crs {
            ds_core::map_engine::OutputCrs::Projected { bbox, .. } => {
                ds_render::quantize_bbox(bbox)
            }
            _ => ds_render::quantize_bbox(&view.bbox),
        },
        width: view.width,
        height: view.height,
        time,
        parameter: effective_parameter.clone(),
        z: validated.z.map(ds_render::quantize_z),
        // The engine's latest run, pinned above (#521); a `reference_time`
        // query parameter is a follow-up (#337 Phase 4).
        reference_time,
        // Content revised in place under the same instant (a push-fed alert
        // set) must not hit a stale entry.
        content_version: engine.content_version(),
        // No opaque-background option on this API: output keeps its alpha.
        background: None,
    };

    let cache_control = cache_control_value(
        has_explicit_time && time.is_some(),
        cache_key.content_version,
    );
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|h| h.to_str().ok())
        .map(str::to_string);

    // Cache lookup runs BEFORE the If-None-Match check. The ETag is
    // content-derived (see `CachedRendered::new`), so a key-derived 304
    // short-circuit would be wrong: it would let a browser holding the
    // pre-fix entry keep getting 304 after the server starts producing
    // different pixels. Mirror the MVT path in `render_vector_tile`
    // (the bug #145 fixed for raster tiles). The render-latency clock
    // (#466) starts at this lookup, so hits and cold renders each report
    // their own tail.
    let render_start = std::time::Instant::now();
    if let Some(cached) = state.rendered_cache.get(&cache_key) {
        if let Some(ref inm) = if_none_match {
            if ds_render::etag_matches(inm, cached.etag()) {
                // 304 from the cache-HIT branch. The `x-cache: HIT` header
                // lets the regression test (and curious clients) distinguish
                // this from a post-render MISS→304, which the handler also
                // serves.
                return Ok(axum::response::Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header(header::ETAG, cached.etag())
                    .header(header::CACHE_CONTROL, cache_control)
                    .header(header::HeaderName::from_static("x-cache"), "HIT")
                    .extension(RenderTiming::since(
                        collection_id,
                        RenderOutcome::Hit,
                        render_start,
                    ))
                    .body(axum::body::Body::empty())
                    .unwrap()
                    .into_response());
            }
        }
        return Ok(map_headers(
            axum::response::Response::builder(),
            content_crs,
            &content_bbox,
            content_datetime.as_deref(),
        )
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ETAG, cached.etag())
        .header(header::CACHE_CONTROL, cache_control)
        .header(
            header::HeaderName::from_static("x-content-type-options"),
            "nosniff",
        )
        .header(header::HeaderName::from_static("x-cache"), "HIT")
        .extension(RenderTiming::since(
            collection_id,
            RenderOutcome::Hit,
            render_start,
        ))
        .body(axum::body::Body::from(cached.into_bytes()))
        .unwrap()
        .into_response());
    }

    // Acquire render semaphore (with timeout to shed load under pressure).
    // A composite holds one value plane per band.
    let queue_start = std::time::Instant::now();
    let planes = match &paint {
        Paint::Colormap(_) => 1,
        Paint::Composite(spec) => spec.parameters.len(),
    };
    let (job, memory_permit) = ds_executor::RenderJob::acquire_raster_planes(
        state.render_semaphore.clone(),
        view.width,
        view.height,
        planes,
    )
    .await
    .map_err(MapsError::from)?;
    let worker_memory = memory_permit.clone();
    // Where the render's time goes (#147): admission here, the engine read
    // and encode in the worker.
    let mut phases = RenderPhases::default();
    phases.add(RenderPhase::Queue, queue_start.elapsed());

    // Render on a blocking thread
    let engine = engine.clone();
    let bbox = view.bbox;
    let width = view.width;
    let height = view.height;
    let output_crs = view.output_crs;
    let rendered_cache = state.rendered_cache.clone();

    let render_parameter = effective_parameter;
    let render_z = validated.z;

    let render_result = job
        .run(move || {
            let _memory_permit = worker_memory;
            let mut phases = phases;

            let colormap = match &paint {
                Paint::Colormap(colormap) => colormap,
                Paint::Composite(spec) => {
                    // Bands that share no scan resolve no time: nothing to
                    // draw, and nothing to cache under a key naming none.
                    if time.is_none() {
                        return Ok((None, phases));
                    }
                    // Every band from the one timestep the cache key names
                    // (#507), in the plane order the spec reads.
                    let bands: Vec<&str> = spec.parameters.iter().map(String::as_str).collect();
                    let engine_start = std::time::Instant::now();
                    let tiles = engine.get_raster_tiles(
                        bbox,
                        width,
                        height,
                        time,
                        &output_crs,
                        &bands,
                        render_z,
                        reference_time,
                    )?;
                    phases.add(RenderPhase::Engine, engine_start.elapsed());
                    let encode_start = std::time::Instant::now();
                    let bytes = ds_render::render_composite_tiles(&tiles, spec, format, None)?;
                    if bytes.is_some() {
                        phases.add(RenderPhase::Encode, encode_start.elapsed());
                    }
                    return Ok((bytes, phases));
                }
            };

            let engine_start = std::time::Instant::now();
            let tile = engine.get_raster_tile(
                bbox,
                width,
                height,
                time,
                &output_crs,
                render_parameter.as_deref(),
                render_z,
                // The run pinned before keying (#521): `Some(latest)` renders the
                // same pixels as `None` by the engine contract, but survives a
                // run swap mid-render without mixing runs in one response.
                reference_time,
            )?;
            phases.add(RenderPhase::Engine, engine_start.elapsed());
            // If every pixel is nodata, skip colorization + encoding entirely.
            if tile.is_empty() {
                return Ok((None, phases));
            }
            let encode_start = std::time::Instant::now();
            let bytes = ds_render::render_tile(&tile, colormap.as_ref(), format)?;
            phases.add(RenderPhase::Encode, encode_start.elapsed());
            Ok::<_, ds_core::error::DataServerError>((Some(bytes), phases))
        })
        .await
        .map_err(MapsError::from)?;
    let phases = render_result
        .as_ref()
        .map_or(RenderPhases::default(), |&(_, phases)| phases);

    // The EMPTY fast path skips the format-aware encoder and emits PNG
    // bytes directly. Track the actual Content-Type per branch so the
    // header never lies about the payload (#162). Wrap every branch in
    // `CachedRendered` so the response ETag is FNV-1a over the actual
    // bytes — different pixels, different ETag — regardless of which
    // exit we take (#145).
    // Each arm produces a `CachedRendered` ready to serve. Only the
    // populated `Ok(Some(_))` path inserts into the rendered cache; the
    // EMPTY fast-path intentionally doesn't (its bytes are deterministic
    // for fixed dimensions). Engine errors bail with 500 before this
    // match.
    let (cached, x_cache, response_content_type) = match render_result {
        Ok((Some(bytes), _)) => {
            let cached = ds_render::CachedRendered::new(bytes::Bytes::from(bytes));
            rendered_cache.insert(cache_key, cached.clone());
            (cached, "MISS", content_type)
        }
        Ok((None, _)) => {
            // Empty tile: a transparent PNG, encoded once per (w,h) and shared
            // across WMS/Maps/Tiles (#171). Not inserted into the rendered cache.
            let cached = ds_render::empty_tile(width, height)
                .map_err(|e| MapsError::Internal(format!("Failed to encode empty tile: {e}")))?;
            (cached, "EMPTY", "image/png")
        }
        Err(e) => {
            use ds_core::error::DataServerError as DSE;
            // A client mistake (e.g. a multi-parameter PVOL collection
            // rendered without a `<site>:<quantity>` parameter, or a bad
            // bbox/datetime) is a 400 with the engine's helpful message —
            // not a 500 that hides it behind "Internal server error".
            return Err(match e {
                DSE::ResourceExhausted | DSE::DeadlineExceeded => {
                    MapsError::ServiceUnavailable(e.to_string())
                }
                DSE::InvalidParameter(_)
                | DSE::InvalidBbox(_)
                | DSE::InvalidDatetime(_)
                | DSE::QueryTooLarge(_) => {
                    // 4xx-class: traced at DEBUG (not WARN) so a misconfigured
                    // client is still diagnosable server-side without inflating
                    // the warn stream with routine bad requests.
                    tracing::debug!(
                        "Maps render bad-request for collection '{}': {e}",
                        collection_id
                    );
                    MapsError::BadRequest(e.to_string())
                }
                // ReferenceTimeNotFound is documented to map to 404 (EDR
                // already does); reachable here when a pinned run is pruned
                // between resolution and render — routine for nowcast
                // generations (#522), not an internal error.
                DSE::CollectionNotFound(_)
                | DSE::LocationNotFound(_)
                | DSE::ReferenceTimeNotFound(_) => {
                    tracing::debug!(
                        "Maps render not-found for collection '{}': {e}",
                        collection_id
                    );
                    MapsError::NotFound(e.to_string())
                }
                _ => {
                    tracing::warn!("Maps render error for collection '{}': {e}", collection_id);
                    MapsError::Internal(format!("Render failed: {e}"))
                }
            });
        }
    };

    // Content-derived ETag now available — do the `If-None-Match`
    // comparison here, after rendering. Same flow as `render_vector_tile`
    // in api-tiles. Forward the same `x_cache` label the 200 response
    // would carry (`"MISS"` or `"EMPTY"`) so revalidations look the
    // same on dashboards as initial fetches — a client revalidating a
    // cached transparent-tile response sees `304 x-cache: EMPTY`, not
    // a misleading `MISS`.
    if let Some(ref inm) = if_none_match {
        if ds_render::etag_matches(inm, cached.etag()) {
            return Ok(axum::response::Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::ETAG, cached.etag())
                .header(header::CACHE_CONTROL, cache_control)
                .header(header::HeaderName::from_static("x-cache"), x_cache)
                .extension(
                    RenderTiming::since(collection_id, RenderOutcome::Cold, render_start)
                        .with_phases(phases),
                )
                .body(axum::body::Body::empty())
                .unwrap()
                .into_response());
        }
    }

    Ok(map_headers(
        axum::response::Response::builder(),
        content_crs,
        &content_bbox,
        content_datetime.as_deref(),
    )
    .header(header::CONTENT_TYPE, response_content_type)
    .header(header::ETAG, cached.etag())
    .header(header::CACHE_CONTROL, cache_control)
    .header(
        header::HeaderName::from_static("x-content-type-options"),
        "nosniff",
    )
    .header(header::HeaderName::from_static("x-cache"), x_cache)
    .extension(
        RenderTiming::since(collection_id, RenderOutcome::Cold, render_start).with_phases(phases),
    )
    .body(axum::body::Body::from(cached.into_bytes()))
    .unwrap()
    .into_response())
}

/// The headers `/req/core/map-response` requires of a map: `Content-Crs`
/// (sent for CRS84 too, `/rec/core/content-crs`), `Content-Bbox`, and
/// `Content-Datetime` when the collection has a temporal extent. One
/// builder for the render and the cache hit, so they never differ.
fn map_headers(
    builder: axum::http::response::Builder,
    content_crs: &str,
    content_bbox: &str,
    content_datetime: Option<&str>,
) -> axum::http::response::Builder {
    let builder = builder
        .header(header::HeaderName::from_static("content-crs"), content_crs)
        .header(
            header::HeaderName::from_static("content-bbox"),
            content_bbox,
        );
    match content_datetime {
        Some(datetime) => builder.header(
            header::HeaderName::from_static("content-datetime"),
            datetime,
        ),
        None => builder,
    }
}

impl From<ds_executor::ExecutionError> for MapsError {
    fn from(error: ds_executor::ExecutionError) -> Self {
        match error {
            ds_executor::ExecutionError::Task(e) => Self::Internal(e.to_string()),
            other => Self::ServiceUnavailable(other.to_string()),
        }
    }
}
