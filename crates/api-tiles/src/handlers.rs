use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use arc_swap::ArcSwap;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Extension;
use axum::Json;
use serde_json::json;

use api_common::map_frame;
use api_common::subset::{self, RequestedTime, TimeSelection};
use api_common::workbench::Surface;
use api_common::{rel, Mount};
use ds_core::config::CollectionConfig;
use ds_core::feature::{Bbox, FeatureQuery};
use ds_core::feature_engine::FeatureEngine;
use ds_core::map_engine::{MapEngine, RasterInfo};
use ds_executor::{RenderOutcome, RenderPhase, RenderPhases, RenderTiming};
use ds_mvt::{
    encode_tile, properties_hash, CachedTile, PropertyAllowlist, TileEncodeOptions, TmsKind,
    VectorTileCache, VectorTileKey,
};
use ds_render::{CacheKey, ColorMap, CompositeSpec, RenderedCache, StyleInfo};

use crate::error::TilesError;
use crate::params::{self, LegendFormat, LegendQueryParams, TileQueryParams};
use crate::tilematrixset::{self, SUPPORTED_TILE_MATRIX_SETS};

/// Pre-generated 256×256 fully transparent `CachedRendered` for empty
/// (all-nodata) tiles — the bytes and their FNV-1a ETag are computed once per
/// process instead of on every empty-tile response, and the colorization +
/// encoding pipeline is skipped when a tile has no data.
///
/// Sourced from the shared [`ds_render::empty_tile`] cache, which WMS and Maps
/// now use too (#171) — so all three raster APIs share one mechanism rather than
/// each rolling their own. Tiles is always 256×256, so a process-once `LazyLock`
/// keeps cloning per request essentially free (`Bytes` is `Arc`-backed).
static EMPTY_TILE_CACHED: LazyLock<ds_render::CachedRendered> = LazyLock::new(|| {
    ds_render::empty_tile(params::TILE_SIZE, params::TILE_SIZE)
        .expect("encoding empty tile PNG must not fail")
});

/// Shared state for the OGC API Tiles service.
#[derive(Clone)]
pub struct TilesState {
    /// Collections that can produce map tiles (raster rendering).
    pub map_engines: HashMap<String, Arc<dyn MapEngine>>,
    pub collections: HashMap<String, CollectionConfig>,
    pub styles: HashMap<String, HashMap<String, StyleInfo>>,
    /// Collections that can produce vector tiles (MVT). Keyed independently of
    /// `map_engines` — a collection may serve raster, vector, or both.
    pub feature_engines: HashMap<String, Arc<dyn FeatureEngine>>,
    pub feature_collections: HashMap<String, CollectionConfig>,
    pub render_semaphore: Arc<tokio::sync::Semaphore>,
    pub rendered_cache: Arc<RenderedCache>,
    pub vector_tile_cache: Arc<VectorTileCache>,
    /// Static fallback base URL for absolute links. Used as-is unless
    /// `trust_proxy_headers` resolves a per-request value.
    pub base_url: String,
    /// Honour reverse-proxy forwarding headers when generating self-links (#12).
    pub trust_proxy_headers: bool,
}

pub type AppState = Arc<ArcSwap<TilesState>>;

/// Resolve the absolute base URL for the current request, honouring reverse-proxy
/// forwarding headers when `trust_proxy_headers` is enabled (#12).
pub(crate) fn request_base_url(state: &TilesState, headers: &HeaderMap) -> String {
    ds_core::proxy::resolve_base_url(&state.base_url, state.trust_proxy_headers, |name| {
        headers.get(name).and_then(|v| v.to_str().ok())
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn lookup_engine<'a>(
    state: &'a TilesState,
    id: &str,
) -> Result<(&'a Arc<dyn MapEngine>, &'a CollectionConfig), TilesError> {
    let engine = state
        .map_engines
        .get(id)
        .ok_or_else(|| TilesError::NotFound(format!("Collection '{id}' not found")))?;
    let config = state
        .collections
        .get(id)
        .ok_or_else(|| TilesError::Internal("Collection config missing".into()))?;
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

/// How a tile render turns engine output into pixels.
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
/// parameter nor an RGB composite. Engines with an empty parameter list
/// (single-band GeoTIFF) ignore the name at render time, so it is accepted.
/// The message lists the valid names in order, so it is deterministic.
fn check_parameter_name(
    engine: &dyn MapEngine,
    info: &RasterInfo,
    collection_id: &str,
    parameter: Option<&str>,
) -> Result<(), TilesError> {
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
    Err(TilesError::BadRequest(format!(
        "parameter-name '{pname}' is not available for collection '{collection_id}'. \
         Available: {}",
        supported.join(", ")
    )))
}

/// The 404 for a style other than `default` on an RGB composite.
fn composite_style_not_found(collection_id: &str, composite: &str, style: &str) -> TilesError {
    TilesError::NotFound(format!(
        "Style '{style}' not found for parameter '{composite}' of collection \
         '{collection_id}'. Available: {}",
        ds_render::COMPOSITE_STYLE
    ))
}

/// `immutable` (24 h) only for an explicit timestamp that resolved to a
/// timestep, over content the engine never revises (`content_version == 0`):
/// a tile at fixed z/x/y + time is then truly immutable. "Latest", a
/// timestamp the engine has nothing to render for yet (resolved to `None`:
/// its catalog is still empty after a start or reload) and in-place-revised
/// content (a push-fed alert set) get 60 s + revalidation so a browser/CDN
/// holding a pre-revision tile asks again.
fn cache_control_value(pinned_time: bool, content_version: u64) -> &'static str {
    if pinned_time && content_version == 0 {
        "public, max-age=86400, immutable"
    } else {
        "public, max-age=60, must-revalidate"
    }
}

fn crs_to_uri(crs: &str) -> &'static str {
    match crs {
        "CRS:84" => "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
        "EPSG:3857" => "http://www.opengis.net/def/crs/EPSG/0/3857",
        _ => "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
    }
}

/// Resolve the requested representation from `?f=` + the `Accept` header.
fn negotiate(f: Option<&str>, headers: &HeaderMap) -> Result<ds_core::html::Wanted, TilesError> {
    let accept = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok());
    ds_core::html::negotiate(f, accept).map_err(|e| TilesError::BadRequest(e.to_string()))
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

/// How tiles are laid out below a collection.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Layout {
    /// The per-API `/tiles` service: both kinds under `…/tiles`, vector tiles
    /// selected by `?f=mvt`.
    PerApi,
    /// The shared OGC API root (#789): map tiles under `…/map/tiles`, vector
    /// tiles (MVT only) under `…/tiles`, as in Tiles Table 8.
    Shared,
}

/// Tiles' description of a collection: standard fields and tileset links,
/// without the `self` link.
pub(crate) fn collection_parts(
    sources: &TileSources<'_>,
    styles: Option<&HashMap<String, StyleInfo>>,
    layout: Layout,
    root: &str,
) -> (
    serde_json::Map<String, serde_json::Value>,
    Vec<serde_json::Value>,
) {
    let config = sources.config;
    let raster_info = sources.raster_info.as_deref();
    let id = &config.id;

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
            let Some(s) = styles.get(name) else { continue };
            let links = match layout {
                Layout::PerApi => {
                    let legend = format!("{root}/collections/{id}/styles/{}/legend", s.name);
                    json!([
                        {"href": legend, "rel": rel::LEGEND, "type": "application/json"}
                    ])
                }
                // The shared root's legend belongs to Maps; Tiles adds each
                // style's map tilesets (merged into the Maps style entry).
                Layout::Shared => json!([{
                    "href": format!("{root}/collections/{id}/styles/{}/map/tiles", s.name),
                    "rel": rel::TILESETS_MAP,
                    "type": "application/json",
                    "title": "Map tilesets in this style"
                }]),
            };
            style_list.push(json!({"id": s.name, "title": s.title, "links": links}));
        }
    }

    // `storageCrs`: native CRS of a raster source when it has a stable OGC URI
    // (omitted for vector collections and for projected grids with no canonical
    // URI). Resolved up-front because OGC API – Common – Part 2 §7.13.3 requires
    // it to be a member of `crs[]` below.
    let storage_crs = raster_info.and_then(|i| ds_core::geo::native_crs_uri(&i.native_crs));

    // OGC API – Common – Part 2 `crs` array (#296), for parity with Maps.
    // Tiles are delivered in their TileMatrixSet's CRS (EPSG:3857 for
    // WebMercatorQuad, CRS84 for WorldCRS84Quad), plus the native `storageCrs`
    // when present — §7.13.3 mandates `storageCrs ∈ crs[]`, and a projected
    // raster (EPSG:3067/3035) would otherwise violate it. CRS84 is listed first
    // for consistency with Maps and Features.
    const CRS84_URI: &str = "http://www.opengis.net/def/crs/OGC/1.3/CRS84";
    let mut crs_uris: Vec<&'static str> = Vec::new();
    for tms_id in SUPPORTED_TILE_MATRIX_SETS {
        // `if let` (not `expect`): this runs in the request-serving path and
        // there is no CatchPanicLayer, so a panic would drop the connection.
        // Divergence between SUPPORTED_TILE_MATRIX_SETS and `get_tile_matrix_set`
        // is instead caught in CI by `tilematrixset::tests::
        // every_supported_tms_resolves`, so `crs[]` is never silently shortened
        // in practice (review on #298).
        if let Some(def) = tilematrixset::get_tile_matrix_set(tms_id) {
            if !crs_uris.contains(&def.crs) {
                crs_uris.push(def.crs);
            }
        }
    }
    if let Some(sc) = storage_crs {
        if !crs_uris.contains(&sc) {
            crs_uris.push(sc);
        }
    }
    // CRS84 first (stable sort keeps the rest in order).
    crs_uris.sort_by_key(|c| *c != CRS84_URI);

    let mut fields = serde_json::Map::new();
    // `dataType` hints at the data's representation (Common Part 2): map for
    // rendered rasters, vector for features. Each tileset states its own kind.
    fields.insert(
        "dataType".into(),
        json!(if raster_info.is_some() {
            "map"
        } else {
            "vector"
        }),
    );
    fields.insert("crs".into(), json!(crs_uris));
    if layout == Layout::PerApi {
        let tms_links: Vec<_> = SUPPORTED_TILE_MATRIX_SETS
            .iter()
            .map(|tms_id| {
                json!({
                    "tileMatrixSet": tms_id,
                    "tileMatrixSetURI": format!("http://www.opengis.net/def/tilematrixset/OGC/1.0/{tms_id}"),
                })
            })
            .collect();
        fields.insert("tileMatrixSetLinks".into(), json!(tms_links));
    }
    fields.insert("styles".into(), json!(style_list));
    // The valid `parameter-name` values of the map tile routes (#279).
    if let (Some(info), Some(engine)) = (raster_info, sources.map_engine) {
        if let Some(parameters) =
            api_common::parameter_names(info, &engine.composites(), |p| engine.parameter_times(p))
        {
            fields.insert(api_common::PARAMETER_NAMES.into(), parameters);
        }
    }
    // No `itemType`: OGC API – Common – Part 2 §7.13 defines it as describing
    // the items reachable at /collections/{id}/items, which tiles are not.
    // Emitting it (even "feature" for vector collections) would be an
    // over-claim a validator probing /items would catch (review on #298).
    if let Some(sc) = storage_crs {
        fields.insert("storageCrs".into(), json!(sc));
    }
    if let Some(extent) = build_extent(raster_info, sources.feature_extent, sources.feature_time) {
        fields.insert("extent".into(), extent);
    }

    // Tiles Req 13 (geodata-tilesets): the registered relation names the kind
    // of tiles in the linked list.
    let mut links = Vec::new();
    let tileset_link = |href: String, relation: &str, title: &str| json!({"href": href, "rel": relation, "type": "application/json", "title": title});
    match layout {
        Layout::PerApi => {
            // One list may hold both kinds, as the standard allows; each
            // registered relation names a kind it holds.
            let tilesets = format!("{root}/collections/{id}/tiles");
            if raster_info.is_some() {
                links.push(tileset_link(
                    tilesets.clone(),
                    rel::TILESETS_MAP,
                    "Map tilesets",
                ));
            }
            if sources.has_vector {
                links.push(tileset_link(
                    tilesets,
                    rel::TILESETS_VECTOR,
                    "Vector tilesets",
                ));
            }
        }
        Layout::Shared => {
            if raster_info.is_some() {
                links.push(tileset_link(
                    format!("{root}/collections/{id}/map/tiles"),
                    rel::TILESETS_MAP,
                    "Map tilesets",
                ));
            }
            if sources.has_vector {
                links.push(tileset_link(
                    format!("{root}/collections/{id}/tiles"),
                    rel::TILESETS_VECTOR,
                    "Vector tilesets",
                ));
            }
        }
    }
    (fields, links)
}

fn build_collection_metadata(
    sources: &TileSources<'_>,
    styles: Option<&HashMap<String, StyleInfo>>,
    root: &str,
) -> serde_json::Value {
    let config = sources.config;
    let (fields, access) = collection_parts(sources, styles, Layout::PerApi, root);
    let mut links = vec![json!({
        "href": format!("{root}/collections/{}", config.id),
        "rel": "self",
        "type": "application/json",
        "title": config.title
    })];
    links.extend(access);
    api_common::collection_metadata(config, serde_json::Value::Object(fields), links)
}

/// Temporal extent precedence shared by discovery and metadata.
fn collection_time(
    raster_info: Option<&ds_core::map_engine::RasterInfo>,
    feature_time: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
) -> Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
    raster_info
        .and_then(|i| i.times.first().copied().zip(i.times.last().copied()))
        .or(feature_time)
}

/// Build the OGC API Common Part 2 `extent` object (spatial, temporal,
/// vertical) including the `grid` resolution descriptors. The spatial bbox
/// falls back to `feature_extent` for vector collections that have no
/// `RasterInfo`. Returns `None` when there is no extent to advertise.
///
/// The assembly lives in `ds_core::ogc_extent` so Maps, Tiles, and Features
/// share one definition (issue #263).
fn build_extent(
    raster_info: Option<&ds_core::map_engine::RasterInfo>,
    feature_extent: Option<[f64; 4]>,
    feature_time: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
) -> Option<serde_json::Value> {
    let spatial_extent = raster_info
        .and_then(|i| i.spatial_extent)
        .or(feature_extent);
    let raster_times = raster_info
        .map(|i| i.times.as_slice())
        .filter(|t| !t.is_empty());
    let feature_times: Vec<_> = feature_time
        .map(|(start, end)| vec![start, end])
        .unwrap_or_default();
    let mut extent = ds_core::ogc_extent::build_extent(
        spatial_extent,
        raster_info.and_then(|i| i.grid_size),
        raster_info.map(|i| i.native_crs.as_str()).unwrap_or(""),
        raster_times.unwrap_or(&feature_times),
        raster_info.and_then(|i| i.vertical.as_ref()),
    )?;
    // An interval's endpoints are bounds, not an inventory of sample times.
    if raster_times.is_none() {
        if let Some(temporal) = &mut extent.temporal {
            temporal.grid = None;
        }
    }
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
) -> Result<Response, TilesError> {
    use ds_core::html::{LinkView, Wanted};
    let wanted = negotiate(fp.f.as_deref(), &headers)?;
    let state = state.load_full();
    let base = &request_base_url(&state, &headers);
    let root = &mount.root(base);
    let title = "MeteoCore - Tiles";
    let description = "Metocean Data Server \u{2014} OGC API Tiles";
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
        (
            format!("{root}/tileMatrixSets"),
            rel::TILING_SCHEMES,
            "application/json",
            "Tile matrix sets",
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
                    api: "tiles",
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

/// Per-collection OpenAPI paths of the per-API layout, keyed below the mount.
pub(crate) fn collection_openapi_paths(
    state: &TilesState,
    m: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let mut collection_paths = json!({});
    // A collection may be raster-only, vector-only, or both. Iterate the
    // union of `collections` (raster) and `feature_collections` (vector),
    // then advertise the formats each one actually supports.
    let mut ids: Vec<&String> = state
        .collections
        .keys()
        .chain(state.feature_collections.keys())
        .collect();
    ids.sort();
    ids.dedup();
    for id in ids {
        let config = state
            .collections
            .get(id)
            .or_else(|| state.feature_collections.get(id));
        let Some(config) = config else { continue };
        let has_raster = state.map_engines.contains_key(id);
        let has_vector = state.feature_engines.contains_key(id);

        collection_paths[format!("{m}/collections/{id}")] = json!({
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

        collection_paths[format!("{m}/collections/{id}/tiles")] = json!({
            "get": {
                "summary": format!("List tilesets for {}", config.title),
                "operationId": format!("getTilesets_{id}"),
                "tags": [id],
                "responses": {
                    "200": {"description": "Available tilesets"}
                }
            }
        });

        collection_paths[format!("{m}/collections/{id}/tiles/{{tileMatrixSetId}}")] = json!({
            "get": {
                "summary": format!("Get tileset metadata for {}", config.title),
                "operationId": format!("getTileset_{id}"),
                "tags": [id],
                "parameters": [{
                    "name": "tileMatrixSetId",
                    "in": "path",
                    "required": true,
                    "schema": {"type": "string", "enum": SUPPORTED_TILE_MATRIX_SETS},
                    "description": "Tile matrix set identifier"
                }],
                "responses": {
                    "200": {"description": "Tileset metadata"},
                    "404": {"description": "Collection or tile matrix set not found"}
                }
            }
        });

        let mut content = serde_json::Map::new();
        if has_raster {
            content.insert(
                "image/png".into(),
                json!({"schema": {"type": "string", "format": "binary"}}),
            );
            content.insert(
                "image/jpeg".into(),
                json!({"schema": {"type": "string", "format": "binary"}}),
            );
            content.insert(
                "image/webp".into(),
                json!({"schema": {"type": "string", "format": "binary"}}),
            );
        }
        if has_vector {
            content.insert(
                MVT_CONTENT_TYPE.into(),
                json!({"schema": {"type": "string", "format": "binary"}}),
            );
        }

        let tile_path = format!(
            "{m}/collections/{id}/tiles/{{tileMatrixSetId}}/{{tileMatrix}}/{{tileRow}}/{{tileCol}}"
        );
        collection_paths[&tile_path] = json!({
            "get": {
                "summary": format!("Get tile for {}", config.title),
                "operationId": format!("getTile_{id}"),
                "tags": [id],
                "parameters": [
                    {
                        "name": "tileMatrixSetId",
                        "in": "path",
                        "required": true,
                        "schema": {"type": "string", "enum": SUPPORTED_TILE_MATRIX_SETS},
                        "description": "Tile matrix set identifier"
                    },
                    {
                        "name": "tileMatrix",
                        "in": "path",
                        "required": true,
                        "schema": {"type": "integer", "minimum": 0, "maximum": params::MAX_ZOOM_LEVEL},
                        "description": "Zoom level"
                    },
                    {
                        "name": "tileRow",
                        "in": "path",
                        "required": true,
                        "schema": {"type": "integer", "minimum": 0},
                        "description": "Row index"
                    },
                    {
                        "name": "tileCol",
                        "in": "path",
                        "required": true,
                        "schema": {"type": "integer", "minimum": 0},
                        "description": "Column index"
                    },
                    {"$ref": "#/components/parameters/datetime"},
                    {"$ref": "#/components/parameters/tile-subset"},
                    {"$ref": "#/components/parameters/f"},
                    {"$ref": "#/components/parameters/elevation"}
                ],
                "responses": {
                    "200": {
                        "description": "Tile image or vector tile",
                        "content": content
                    },
                    "204": {"description": "Empty tile: a map tile's time selects no time step, or a time subset lies outside the collection's temporal extent"},
                    "400": {"description": "Bad request"},
                    "404": {"description": "Tile not found"},
                    "422": {"description": "Tile too dense (feature count exceeds per-tile cap)"},
                    "500": {"description": "Server error"}
                }
            }
        });
        // Only map tiles render a selected parameter at an encoder quality.
        if has_raster {
            if let Some(parameters) =
                collection_paths[&tile_path]["get"]["parameters"].as_array_mut()
            {
                for name in MAP_TILE_COMPONENTS {
                    parameters.push(json!({"$ref": format!("#/components/parameters/{name}")}));
                }
            }
        }

        // Legends describe a raster style's palette — vector-only collections
        // have no styles registry entry and the route would 404, so only
        // advertise it where a MapEngine is registered.
        if has_raster {
            collection_paths[format!("{m}/collections/{id}/styles/{{styleId}}/tiles/{{tileMatrixSetId}}/{{tileMatrix}}/{{tileRow}}/{{tileCol}}")] = json!({
                "get": {
                    "summary": format!("Get styled map tile for {}", config.title),
                    "operationId": format!("getStyledTile_{id}"),
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
                            "name": "tileMatrixSetId",
                            "in": "path",
                            "required": true,
                            "schema": {"type": "string", "enum": SUPPORTED_TILE_MATRIX_SETS},
                            "description": "Tile matrix set identifier"
                        },
                        {
                            "name": "tileMatrix",
                            "in": "path",
                            "required": true,
                            "schema": {"type": "integer", "minimum": 0, "maximum": params::MAX_ZOOM_LEVEL},
                            "description": "Zoom level"
                        },
                        {
                            "name": "tileRow",
                            "in": "path",
                            "required": true,
                            "schema": {"type": "integer", "minimum": 0},
                            "description": "Row index"
                        },
                        {
                            "name": "tileCol",
                            "in": "path",
                            "required": true,
                            "schema": {"type": "integer", "minimum": 0},
                            "description": "Column index"
                        },
                        {"$ref": "#/components/parameters/datetime"},
                        {"$ref": "#/components/parameters/elevation"},
                        {"$ref": "#/components/parameters/parameter-name"},
                        {"$ref": "#/components/parameters/quality"},
                        {"$ref": "#/components/parameters/tile-subset"},
                        {"$ref": "#/components/parameters/tile-width"},
                        {"$ref": "#/components/parameters/tile-height"},
                        {"$ref": "#/components/parameters/tile-scale-denominator"},
                        {
                            "name": "f",
                            "in": "query",
                            "required": false,
                            "schema": {"type": "string", "default": "image/png", "enum": ["image/png", "image/jpeg", "image/webp"]},
                            "description": "Raster output format. Vector tiles are not styled; `mvt` returns 400."
                        }
                    ],
                    "responses": {
                        "200": {
                            "description": "Styled map tile",
                            "content": {
                                "image/png": {"schema": {"type": "string", "format": "binary"}},
                                "image/jpeg": {"schema": {"type": "string", "format": "binary"}},
                                "image/webp": {"schema": {"type": "string", "format": "binary"}}
                            }
                        },
                        "204": {"description": "Empty map tile: the requested time selects no time step"},
                        "400": {"description": "Bad request"},
                        "404": {"description": "Collection, style or tile not found"},
                        "500": {"description": "Server error"}
                    }
                }
            });
            collection_paths[format!("{m}/collections/{id}/styles/{{styleId}}/legend")] = json!({
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
                            "description": "Describe the style of this parameter's layer, matching `parameter-name` on the tile routes. Falls back to the collection-level style when the collection has no per-parameter layer."
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
    }

    match collection_paths {
        serde_json::Value::Object(paths) => paths,
        _ => serde_json::Map::new(),
    }
}

/// Tiling-scheme paths, keyed below the mount.
pub(crate) fn tile_matrix_set_openapi_paths(m: &str) -> serde_json::Map<String, serde_json::Value> {
    match json!({
        format!("{m}/tileMatrixSets"): {
            "get": {
                "summary": "List supported tile matrix sets",
                "operationId": "getTileMatrixSets",
                "tags": [api_common::openapi_tags::TILING_SCHEMES],
                "responses": { "200": {"description": "List of tile matrix sets"} }
            }
        },
        format!("{m}/tileMatrixSets/{{tileMatrixSetId}}"): {
            "get": {
                "summary": "Get tile matrix set definition",
                "operationId": "getTileMatrixSet",
                "tags": [api_common::openapi_tags::TILING_SCHEMES],
                "parameters": [{
                    "name": "tileMatrixSetId",
                    "in": "path",
                    "required": true,
                    "schema": {"type": "string"},
                    "description": "Tile matrix set identifier"
                }],
                "responses": {
                    "200": {"description": "Tile matrix set definition"},
                    "404": {"description": "Tile matrix set not found"}
                }
            }
        }
    }) {
        serde_json::Value::Object(paths) => paths,
        _ => serde_json::Map::new(),
    }
}

/// Map-tile-only parameter components, after `datetime`, `tile-subset`
/// and `elevation`, which the per-API mixed tile path lists for both kinds.
pub(crate) const MAP_TILE_COMPONENTS: [&str; 5] = [
    "parameter-name",
    "quality",
    "tile-width",
    "tile-height",
    "tile-scale-denominator",
];

/// OpenAPI components referenced by the per-API layout's paths.
pub(crate) fn openapi_components() -> serde_json::Value {
    json!({
        "parameters": {
            // OGC API - Tiles `/req/collections/rc-datetime-definition`, the
            // Tiles OpenAPI fragment's text and this server's rules.
            "datetime": {
                "name": "datetime",
                "in": "query",
                "description": "Either a date-time or an interval, half-bounded or bounded. Date and time expressions adhere to RFC 3339. Half-bounded intervals are expressed using double-dots.\n\nExamples:\n\n* A date-time: \"2018-02-12T23:20:50Z\"\n* A bounded interval: \"2018-02-12T00:00:00Z/2018-03-18T12:31:12Z\"\n* Half-bounded intervals: \"2018-02-12T00:00:00Z/..\" or \"../2018-03-18T12:31:12Z\"\n\nOn map tiles: an instant is snapped to an available time step (the last one at or before it); an interval renders the latest time step inside it, and is an empty tile (204) when it holds none. Without `datetime` or a `datetime` subset the tile shows the collection's default (normally latest) time. `OGCAPI-datetime` reports the instant rendered. Not with `subset=datetime(…)` (400).\n\nOn vector tiles: only features whose temporal geometry intersects the instant or interval, as Features' `datetime` selects them; features without a time always match. Without a time the tile holds the collection's default features.",
                "required": false,
                "schema": {"type": "string"},
                "style": "form",
                "explode": false
            },
            // The Tiles OpenAPI `subset` fragment; map and vector tiles take
            // the `datetime` axis (`/req/datetime/axis`).
            "tile-subset": {
                "name": "subset",
                "in": "query",
                "description": "Retrieve only part of the data by slicing or trimming along one or more axis\nFor trimming: {axisAbbrev}({low}:{high}) (preserves dimensionality)\n   An asterisk (`*`) can be used instead of {low} or {high} to indicate the minimum/maximum value.\nFor slicing:  {axisAbbrev}({value})      (reduces dimensionality)\n\nTiles take one axis, `datetime`, with double-quoted RFC 3339 values or the partial forms `yyyy`, `yyyy-mm`, `yyyy-mm-dd`, `yyyy-mm-ddThhZ` and `yyyy-mm-ddThh:mmZ`, and `*` for the first or last time step. On map tiles an instant is snapped like `datetime`, and an interval or a partial value renders the latest time step inside it. On vector tiles it selects features as `datetime` does. A value entirely outside the time axis (a vector collection's temporal extent), or a map tile interval holding no time step, is an empty tile (204). Any other axis is a 400. Not with `datetime` (400). Example: `subset=datetime(\"2018-02-12T00:00:00Z\":*)`.",
                "style": "form",
                "explode": false,
                "required": false,
                "schema": {
                    "type": "array",
                    "items": {"type": "string"}
                }
            },
            // OGC API - Maps Scaling on map tiles (Map Tilesets
            // `/req/tilesets/tiles-parameters`): the requirement texts'
            // fragments with this server's rules. Named apart from Maps'
            // `width`/`height`/`scale-denominator`, whose rules differ, so
            // the shared root keeps one definition per name.
            "tile-width": {
                "name": "width",
                "in": "query",
                "description": "Width of the viewport in pixel units to present the response (the map subset).\n\nOn a map tile it overrides the tile matrix's tileWidth; the tile matrix still sets the tile's area. A positive integer up to 8000, and `width` × `height` up to 64000000. Omitted with `height` given: the width that keeps pixels square; both omitted: the tile matrix's tileWidth and tileHeight. Not with `scale-denominator` (400).",
                "required": false,
                "style": "form",
                "schema": {"type": "number", "maximum": map_frame::MAX_MAP_DIMENSION}
            },
            "tile-height": {
                "name": "height",
                "in": "query",
                "description": "Height of the viewport in pixel units to present the response (the map subset).\n\nOn a map tile it overrides the tile matrix's tileHeight; the tile matrix still sets the tile's area. A positive integer up to 8000, and `width` × `height` up to 64000000. Omitted with `width` given: the height that keeps pixels square; both omitted: the tile matrix's tileWidth and tileHeight. Not with `scale-denominator` (400).",
                "required": false,
                "style": "form",
                "schema": {"type": "number", "maximum": map_frame::MAX_MAP_DIMENSION}
            },
            "tile-scale-denominator": {
                "name": "scale-denominator",
                "in": "query",
                "description": "Number of units in the real-world corresponding to one such unit on the display.\n\nA positive number, on the standard 0.28 mm pixel. On a map tile it sets the image's size over the tile's area: one pixel spans `scale-denominator` × 0.28 mm on the ground at the tile's centre (ground metres, not CRS units), within the 8000 and 64000000 caps. Not with `width` or `height` (400).",
                "required": false,
                "style": "form",
                "schema": {"type": "number"}
            },
            "f": {
                "name": "f",
                "in": "query",
                "required": false,
                "schema": {
                    "type": "string",
                    "default": "image/png",
                    "enum": [
                        "image/png",
                        "image/jpeg",
                        "image/webp",
                        "mvt",
                        "application/vnd.mapbox-vector-tile"
                    ]
                },
                "description": "Output format. `image/png` auto-emits an 8-bit indexed-palette PNG (~3–4× smaller) for colormap-rendered layers; falls back to 32-bit RGBA above 256 distinct colours. `mvt` selects Mapbox Vector Tile (only on collections with a FeatureEngine)."
            },
            "elevation": {
                "name": "elevation",
                "in": "query",
                "required": false,
                "schema": {"type": "number"},
                "description": "Vertical level (e.g. radar elevation angle). Only valid for collections with a vertical dimension."
            },
            "parameter-name": api_common::parameter_name_parameter(),
            "quality": api_common::quality_parameter(ds_render::DEFAULT_JPEG_QUALITY)
        },
        "schemas": {
            "legend": api_common::legend_schema()
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
    let mut collection_paths = serde_json::Value::Object(collection_openapi_paths(&state, m));
    if let Some(paths) = collection_paths.as_object_mut() {
        paths.extend(tile_matrix_set_openapi_paths(m));
    }
    let mut paths = json!({
        format!("{m}/"): {
            "get": {
                "summary": "Landing page",
                "operationId": "getLandingPage",
                "tags": [api_common::openapi_tags::DISCOVERY],
                "parameters": [format_parameter()],
                "responses": { "200": {"description": "Landing page"} }
            }
        },
        format!("{m}/conformance"): {
            "get": {
                "summary": "Conformance classes",
                "operationId": "getConformance",
                "tags": [api_common::openapi_tags::DISCOVERY],
                "parameters": [format_parameter()],
                "responses": { "200": {"description": "Conformance classes"} }
            }
        },
        format!("{m}/collections"): {"get": api_common::collection_operation()}
    });

    if let (Some(main_obj), Some(coll_obj)) = (paths.as_object_mut(), collection_paths.as_object())
    {
        for (k, v) in coll_obj {
            main_obj.insert(k.clone(), v.clone());
        }
    }

    let openapi = json!({
        "openapi": "3.0.3",
        "info": {
            "title": "MeteoCore - OGC API Tiles",
            "version": "1.0.0",
            "description": "OGC API - Tiles implementation"
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
            "MeteoCore - Tiles API",
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

/// OGC API - Tiles DateTime: map and vector tiles take `datetime` instants
/// and intervals (`/req/collections/rc-datetime-definition`) and
/// `subset=datetime(…)` (`/req/datetime/axis`), on both surfaces.
pub(crate) const TILES_DATETIME: &str =
    "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/datetime";

/// OGC API - Tiles and 2D TMS classes declared on either surface.
///
/// - DateTime ([`TILES_DATETIME`]) holds for every tile. A map tile snaps an
///   instant as `/per/datetime/closest` allows, renders an interval's latest
///   time step, and is an empty tile (204) when a selection holds none. A
///   vector tile holds only the features whose temporal geometry intersects
///   the selection, and those without one
///   (`/req/collections/rc-datetime-response` A and B, #794). A time subset
///   entirely outside the time axis is a 204 on either.
/// - OGC API - Maps "Map Tilesets" is a Maps class and only holds where map
///   tiles sit under the map resource, `{map}/tiles`: at the shared root,
///   where this block declares it through [`SHARED_CONFORMANCE`], never here.
pub(crate) const CONFORMANCE: &[&str] = &[
    "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/core",
    "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/tileset",
    "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/tilesets-list",
    "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/geodata-tilesets",
    TILES_DATETIME,
    "http://www.opengis.net/spec/tms/2.0/conf/tilematrixset",
    "http://www.opengis.net/spec/tms/2.0/conf/json-tilematrixset",
    "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/png",
    "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/jpeg",
    "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/mvt",
];

/// OGC API - Maps "Map Tilesets" (`/req/tilesets`).
pub const MAPS_TILESETS: &str = "http://www.opengis.net/spec/ogcapi-maps-1/1.0/conf/tilesets";

/// Classes the Tiles block adds at the shared root, where it serves map
/// tiles under the Maps block's map resources (#789, Tiles Table 8):
///
/// - Map Tilesets ([`MAPS_TILESETS`]): every map collection links
///   `{root}/collections/{id}/map/tiles` with `tilesets-map`
///   (`/req/tilesets/desc-links`), and each style its
///   `…/styles/{styleId}/map/tiles`. Map tiles honour the parameters of
///   every declared Maps class that `/req/tilesets/tiles-parameters` lists:
///   Scaling's `width`, `height` and `scale-denominator`. Spatial
///   Subsetting asks only for a vertical `h`/`z` subset, and only where the
///   spatial extent is three-dimensional, which none is here; Background,
///   Display Resolution and General Subsetting are not declared.
///
/// The per-API `/maps` service has no `{map}/tiles` (its `tilesets-map`
/// link points into `/tiles`), and `/tiles` serves no map resource, so
/// neither declares it (#259, #946).
pub(crate) const SHARED_CONFORMANCE: &[&str] = &[MAPS_TILESETS];

/// GET {mount}/conformance
pub async fn conformance(
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    Query(fp): Query<ds_core::html::FormatParams>,
    headers: HeaderMap,
) -> Result<Response, TilesError> {
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
                    api: "tiles",
                },
                &classes,
                &nav,
            ))
            .into_response()
        }
    }))
}

/// GET {mount}/tileMatrixSets — List supported tile matrix sets
pub async fn tile_matrix_sets(
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let state = state.load_full();
    let root = &mount.root(&request_base_url(&state, &headers));
    let sets: Vec<serde_json::Value> = SUPPORTED_TILE_MATRIX_SETS
        .iter()
        .filter_map(|id| {
            let tms = tilematrixset::get_tile_matrix_set(id)?;
            Some(json!({
                "id": tms.id,
                "title": tms.title,
                "uri": format!("http://www.opengis.net/def/tilematrixset/OGC/1.0/{}", tms.id),
                "crs": tms.crs,
                "links": [{
                    "href": format!("{root}/tileMatrixSets/{}", tms.id),
                    "rel": "self",
                    "type": "application/json"
                }]
            }))
        })
        .collect();

    Json(json!({
        "tileMatrixSets": sets,
        "links": [{
            "href": format!("{root}/tileMatrixSets"),
            "rel": "self",
            "type": "application/json"
        }]
    }))
}

/// GET {mount}/tileMatrixSets/{tileMatrixSetId} — Get tile matrix set definition
pub async fn tile_matrix_set(Path(tms_id): Path<String>) -> Result<impl IntoResponse, TilesError> {
    let tms = tilematrixset::get_tile_matrix_set(&tms_id).ok_or_else(|| {
        TilesError::NotFound(format!(
            "TileMatrixSet '{tms_id}' not found. Available: {}",
            SUPPORTED_TILE_MATRIX_SETS.join(", ")
        ))
    })?;

    Ok(Json(tms.to_json()))
}

/// GET {mount}/collections — List tile-enabled collections
pub async fn collections(
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    request: api_common::CollectionRequest,
    headers: HeaderMap,
) -> Response {
    let state = state.load_full();
    let base = &request_base_url(&state, &headers);
    let root = &mount.root(base);
    let entries = tile_collections(&state)
        .into_iter()
        .map(|sources| api_common::CollectionEntry {
            config: sources.config,
            metadata: build_collection_metadata(
                &sources,
                state.styles.get(&sources.config.id),
                root,
            ),
            bbox: sources.spatial_extent(),
            time: sources.time(),
        })
        .collect();
    api_common::collections_response(
        Surface {
            base,
            root,
            api: "tiles",
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
) -> Result<Response, TilesError> {
    use ds_core::html::Wanted;
    let wanted = negotiate(fp.f.as_deref(), &headers)?;
    let state = state.load_full();
    let sources = tile_sources(&state, &id)?;
    let base = &request_base_url(&state, &headers);
    let root = &mount.root(base);
    let metadata = build_collection_metadata(&sources, state.styles.get(&id), root);
    Ok(with_vary(match wanted {
        Wanted::Json => Json(metadata).into_response(),
        Wanted::Html => Html(api_common::workbench::collection_html(
            Surface {
                base,
                root,
                api: "tiles",
            },
            &metadata,
            sources.config.license.as_ref(),
        ))
        .into_response(),
    }))
}

/// The tile sources a collection registers with the Tiles service.
pub(crate) struct TileSources<'a> {
    pub(crate) config: &'a CollectionConfig,
    pub(crate) raster_info: Option<Arc<ds_core::map_engine::RasterInfo>>,
    /// The map engine `raster_info` is a snapshot of.
    map_engine: Option<&'a dyn MapEngine>,
    pub(crate) has_vector: bool,
    feature_extent: Option<[f64; 4]>,
    feature_time: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
}

/// The kind of tiles in a tileset (its Tiles `dataType`).
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum TileKind {
    Map,
    Vector,
}

impl TileSources<'_> {
    pub(crate) fn kinds(&self) -> impl Iterator<Item = TileKind> {
        let map = self.raster_info.is_some().then_some(TileKind::Map);
        let vector = self.has_vector.then_some(TileKind::Vector);
        map.into_iter().chain(vector)
    }

    /// The kind `…/tiles/{tileMatrixSetId}` describes. That path holds one
    /// tileset per tiling scheme, so a collection serving both kinds exposes
    /// its map tileset there; the shared root gives each kind its own tileset
    /// resource (#789 Phase 1: map tiles under `…/map/tiles`).
    fn resource_kind(&self) -> TileKind {
        if self.raster_info.is_some() {
            TileKind::Map
        } else {
            TileKind::Vector
        }
    }

    /// The CRS84-domain extent advertised for search and tile-matrix limits.
    pub(crate) fn spatial_extent(&self) -> Option<[f64; 4]> {
        self.raster_info
            .as_ref()
            .and_then(|i| i.spatial_extent)
            .or(self.feature_extent)
            .and_then(ds_core::geo::crs84_extent)
    }

    /// Temporal bounds for discovery, with the same precedence as `extent`.
    pub(crate) fn time(
        &self,
    ) -> Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
        collection_time(self.raster_info.as_deref(), self.feature_time)
    }
}

/// A configured collection's tile sources, or `None` when it serves no tiles.
fn sources_for<'a>(state: &'a TilesState, config: &'a CollectionConfig) -> Option<TileSources<'a>> {
    let map_engine = state.map_engines.get(&config.id).map(|e| e.as_ref());
    let raster_info = map_engine.map(|e| e.raster_info_shared());
    let feature = state.feature_engines.get(&config.id);
    if raster_info.is_none() && feature.is_none() {
        return None;
    }
    Some(TileSources {
        config,
        raster_info,
        map_engine,
        has_vector: feature.is_some(),
        feature_extent: feature.and_then(|e| e.spatial_extent()),
        feature_time: feature.and_then(|e| e.temporal_extent()),
    })
}

/// Every collection with a tile source, raster and vector registries merged.
pub(crate) fn tile_collections(state: &TilesState) -> Vec<TileSources<'_>> {
    let mut seen = std::collections::HashSet::new();
    state
        .collections
        .values()
        .chain(state.feature_collections.values())
        .filter(|config| seen.insert(config.id.as_str()))
        .filter_map(|config| sources_for(state, config))
        .collect()
}

/// Resolve a collection's tile sources, or 404 when it serves no tiles.
pub(crate) fn tile_sources<'a>(
    state: &'a TilesState,
    id: &str,
) -> Result<TileSources<'a>, TilesError> {
    let config = state
        .collections
        .get(id)
        .or_else(|| state.feature_collections.get(id))
        .ok_or_else(|| TilesError::NotFound(format!("Collection '{id}' not found")))?;
    sources_for(state, config)
        .ok_or_else(|| TilesError::NotFound(format!("Collection '{id}' has no tile source")))
}

/// Where a list of tilesets lives: tilesets are at `{url}/{tileMatrixSetId}`.
pub(crate) struct TilesetList<'a> {
    url: String,
    /// Query that selects vector tiles in tile URL templates (`?f=mvt` on the
    /// per-API service, where both kinds share one path).
    vector_query: &'static str,
    /// The style (id, definition) a styled map tileset renders.
    style: Option<(&'a str, &'a StyleInfo)>,
}

impl<'a> TilesetList<'a> {
    fn per_api(root: &str, id: &str) -> Self {
        Self {
            url: format!("{root}/collections/{id}/tiles"),
            vector_query: "?f=mvt",
            style: None,
        }
    }

    fn shared(root: &str, id: &str, kind: TileKind) -> Self {
        let path = match kind {
            TileKind::Map => "map/tiles",
            TileKind::Vector => "tiles",
        };
        Self {
            url: format!("{root}/collections/{id}/{path}"),
            vector_query: "",
            style: None,
        }
    }

    fn styled(root: &str, id: &str, style_id: &'a str, style: &'a StyleInfo) -> Self {
        Self {
            url: format!("{root}/collections/{id}/styles/{style_id}/map/tiles"),
            vector_query: "",
            style: Some((style_id, style)),
        }
    }

    fn json(&self, tilesets: Vec<serde_json::Value>) -> serde_json::Value {
        json!({
            "tilesets": tilesets,
            "links": [{"href": self.url, "rel": "self", "type": "application/json"}]
        })
    }
}

/// Tileset metadata for one tiling scheme and kind of tiles (Tiles Req 8):
/// the registered tiling scheme, the source collection and a templated link to
/// tiles of that kind. A tileset served at `{list}/{tileMatrixSetId}` also
/// carries the `self` link a tileset list entry needs (Req 10 B).
fn tileset_json(
    list: &TilesetList<'_>,
    with_self: bool,
    root: &str,
    sources: &TileSources<'_>,
    tms: &tilematrixset::TileMatrixSetDef,
    kind: TileKind,
) -> serde_json::Value {
    let id = &sources.config.id;
    let tms_id = tms.id;
    let tileset = format!("{}/{tms_id}", list.url);
    let mut links = Vec::new();
    if with_self {
        links.push(json!({
            "href": tileset,
            "rel": "self",
            "type": "application/json",
            "title": "This tileset"
        }));
    }
    links.push(json!({
        "href": format!("{root}/tileMatrixSets/{tms_id}"),
        "rel": rel::TILING_SCHEME,
        "type": "application/json"
    }));
    links.push(json!({
        "href": format!("{root}/collections/{id}"),
        "rel": rel::GEODATA,
        "type": "application/json",
        "title": sources.config.title
    }));
    let (data_type, query, media_type) = match kind {
        TileKind::Map => ("map", "", "image/png"),
        TileKind::Vector => ("vector", list.vector_query, MVT_CONTENT_TYPE),
    };
    links.push(json!({
        "href": format!("{tileset}/{{tileMatrix}}/{{tileRow}}/{{tileCol}}{query}"),
        "rel": "item",
        "type": media_type,
        "templated": true
    }));
    let style_title = list
        .style
        .map(|(_, style)| format!(", {}", style.title))
        .unwrap_or_default();
    let mut tileset = json!({
        "title": format!("{} ({}, {data_type} tiles{style_title})", sources.config.title, tms.title),
        "dataType": data_type,
        "crs": tms.crs,
        "tileMatrixSetURI": format!("http://www.opengis.net/def/tilematrixset/OGC/1.0/{tms_id}"),
        "links": links,
    });
    if let Some((style_id, style)) = list.style {
        tileset["style"] = json!({"id": style_id, "title": style.title});
    }
    if let Some(bbox) = sources.spatial_extent() {
        tileset["tileMatrixSetLimits"] =
            json!(tms.limits_for_extent(bbox, params::DEFAULT_MAX_ZOOM));
    }
    tileset
}

fn supported_tile_matrix_sets() -> impl Iterator<Item = &'static tilematrixset::TileMatrixSetDef> {
    SUPPORTED_TILE_MATRIX_SETS
        .iter()
        .filter_map(|tms_id| tilematrixset::get_tile_matrix_set(tms_id))
}

fn lookup_tile_matrix_set(
    tms_id: &str,
) -> Result<&'static tilematrixset::TileMatrixSetDef, TilesError> {
    tilematrixset::get_tile_matrix_set(tms_id).ok_or_else(|| {
        TilesError::NotFound(format!(
            "TileMatrixSet '{tms_id}' not found. Available: {}",
            SUPPORTED_TILE_MATRIX_SETS.join(", ")
        ))
    })
}

/// GET {mount}/collections/{id}/tiles — List tilesets for a collection: one
/// per tiling scheme and kind of tiles, so a collection serving map and
/// vector tiles lists both with their own `dataType`.
pub async fn collection_tilesets(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, TilesError> {
    let state = state.load_full();
    let sources = tile_sources(&state, &id)?;
    let root = &mount.root(&request_base_url(&state, &headers));
    let list = TilesetList::per_api(root, &id);
    let tilesets = supported_tile_matrix_sets()
        .flat_map(|tms| {
            sources
                .kinds()
                .map(|kind| {
                    let with_self = kind == sources.resource_kind();
                    tileset_json(&list, with_self, root, &sources, tms, kind)
                })
                .collect::<Vec<_>>()
        })
        .collect();
    Ok(Json(list.json(tilesets)))
}

/// GET {mount}/collections/{id}/tiles/{tileMatrixSetId} — Tileset metadata
pub async fn collection_tileset(
    Path((id, tms_id)): Path<(String, String)>,
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, TilesError> {
    let state = state.load_full();
    let sources = tile_sources(&state, &id)?;
    let tms = lookup_tile_matrix_set(&tms_id)?;
    let root = &mount.root(&request_base_url(&state, &headers));
    let list = TilesetList::per_api(root, &id);
    Ok(Json(tileset_json(
        &list,
        true,
        root,
        &sources,
        tms,
        sources.resource_kind(),
    )))
}

// ---------------------------------------------------------------------------
// Shared OGC API root layout (#789): map tiles under `…/map/tiles`, styled map
// tiles under `…/styles/{styleId}/map/tiles`, vector tiles under `…/tiles`.
// ---------------------------------------------------------------------------

/// Tile sources of a collection that serves `kind`, or 404.
fn sources_of_kind<'a>(
    state: &'a TilesState,
    id: &str,
    kind: TileKind,
) -> Result<TileSources<'a>, TilesError> {
    let sources = tile_sources(state, id)?;
    if sources.kinds().any(|k| k == kind) {
        Ok(sources)
    } else {
        Err(TilesError::NotFound(format!(
            "Collection '{id}' has no {} tiles",
            match kind {
                TileKind::Map => "map",
                TileKind::Vector => "vector",
            }
        )))
    }
}

/// The collection-level style `style_id` of a map-tiles collection, or 404.
fn map_style<'a>(
    state: &'a TilesState,
    id: &str,
    style_id: &str,
) -> Result<&'a StyleInfo, TilesError> {
    state
        .styles
        .get(id)
        .and_then(|styles| styles.get(style_id))
        .ok_or_else(|| {
            TilesError::NotFound(format!(
                "Style '{style_id}' not found for collection '{id}'"
            ))
        })
}

async fn shared_tilesets(
    state: AppState,
    mount: Mount,
    headers: &HeaderMap,
    id: &str,
    kind: TileKind,
    style_id: Option<&str>,
    tms_id: Option<&str>,
) -> Result<Response, TilesError> {
    let state = state.load_full();
    let sources = sources_of_kind(&state, id, kind)?;
    let root = &mount.root(&request_base_url(&state, headers));
    let list = match style_id {
        Some(style_id) => TilesetList::styled(root, id, style_id, map_style(&state, id, style_id)?),
        None => TilesetList::shared(root, id, kind),
    };
    Ok(Json(match tms_id {
        Some(tms_id) => tileset_json(
            &list,
            true,
            root,
            &sources,
            lookup_tile_matrix_set(tms_id)?,
            kind,
        ),
        None => list.json(
            supported_tile_matrix_sets()
                .map(|tms| tileset_json(&list, true, root, &sources, tms, kind))
                .collect(),
        ),
    })
    .into_response())
}

/// GET /collections/{id}/map/tiles — map tilesets
pub async fn map_tilesets(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    headers: HeaderMap,
) -> Result<Response, TilesError> {
    shared_tilesets(state, mount, &headers, &id, TileKind::Map, None, None).await
}

/// GET /collections/{id}/map/tiles/{tileMatrixSetId} — map tileset
pub async fn map_tileset(
    Path((id, tms_id)): Path<(String, String)>,
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    headers: HeaderMap,
) -> Result<Response, TilesError> {
    shared_tilesets(
        state,
        mount,
        &headers,
        &id,
        TileKind::Map,
        None,
        Some(&tms_id),
    )
    .await
}

/// GET /collections/{id}/styles/{styleId}/map/tiles — styled map tilesets
pub async fn styled_map_tilesets(
    Path((id, style_id)): Path<(String, String)>,
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    headers: HeaderMap,
) -> Result<Response, TilesError> {
    shared_tilesets(
        state,
        mount,
        &headers,
        &id,
        TileKind::Map,
        Some(&style_id),
        None,
    )
    .await
}

/// GET /collections/{id}/styles/{styleId}/map/tiles/{tileMatrixSetId}
pub async fn styled_map_tileset(
    Path((id, style_id, tms_id)): Path<(String, String, String)>,
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    headers: HeaderMap,
) -> Result<Response, TilesError> {
    shared_tilesets(
        state,
        mount,
        &headers,
        &id,
        TileKind::Map,
        Some(&style_id),
        Some(&tms_id),
    )
    .await
}

/// GET /collections/{id}/tiles — vector tilesets
pub async fn vector_tilesets(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    headers: HeaderMap,
) -> Result<Response, TilesError> {
    shared_tilesets(state, mount, &headers, &id, TileKind::Vector, None, None).await
}

/// GET /collections/{id}/tiles/{tileMatrixSetId} — vector tileset
pub async fn vector_tileset(
    Path((id, tms_id)): Path<(String, String)>,
    State(state): State<AppState>,
    Extension(mount): Extension<Mount>,
    headers: HeaderMap,
) -> Result<Response, TilesError> {
    shared_tilesets(
        state,
        mount,
        &headers,
        &id,
        TileKind::Vector,
        None,
        Some(&tms_id),
    )
    .await
}

/// GET /collections/{id}/map/tiles/{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}
pub async fn map_tile(
    headers: HeaderMap,
    Path((id, tms_id, tile_matrix, tile_row, tile_col)): Path<(String, String, u32, u64, u64)>,
    Query(params): Query<TileQueryParams>,
    RawQuery(query): RawQuery,
    State(state): State<AppState>,
) -> Result<Response, TilesError> {
    if params.is_mvt() {
        return Err(TilesError::BadRequest(format!(
            "Map tiles are images; vector tiles are served at /collections/{id}/tiles/…"
        )));
    }
    params::reject_unknown_parameters(query.as_deref(), params::MAP_TILE_PARAMETERS)?;
    let subsets = subset::query_values(query.as_deref(), "subset");
    render_tile(
        &id,
        "default",
        &tms_id,
        tile_matrix,
        tile_row,
        tile_col,
        params,
        &subsets,
        headers,
        state,
    )
    .await
}

/// GET /collections/{id}/tiles/{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}
/// — Mapbox Vector Tiles only; `f` may be omitted or name MVT.
pub async fn vector_tile(
    headers: HeaderMap,
    Path((id, tms_id, tile_matrix, tile_row, tile_col)): Path<(String, String, u32, u64, u64)>,
    Query(params): Query<TileQueryParams>,
    RawQuery(query): RawQuery,
    State(state): State<AppState>,
) -> Result<Response, TilesError> {
    if params.format.is_some() && !params.is_mvt() {
        return Err(TilesError::BadRequest(format!(
            "Vector tiles are served as {MVT_CONTENT_TYPE}; map tiles are at /collections/{id}/map/tiles/…"
        )));
    }
    // Vector tiles take a time and nothing else: the map tile parameters
    // and unknown ones are 400s, never ignored (#605).
    let subsets = subset::query_values(query.as_deref(), "subset");
    let time = params.validate_vector(query.as_deref(), &subsets)?;
    render_vector_tile(
        headers,
        &id,
        &tms_id,
        tile_matrix,
        tile_row,
        tile_col,
        time,
        state,
    )
    .await
}

/// MVT MIME type registered with IANA.
pub(crate) const MVT_CONTENT_TYPE: &str = "application/vnd.mapbox-vector-tile";

/// Encode an MVT from a `FeatureEngine` and return it as an HTTP response.
///
/// Reached through content negotiation on the standard tile path:
/// `GET /collections/{id}/tiles/{tms}/{z}/{row}/{col}?f=mvt`.
/// Validation order mirrors `render_tile` (TMS → zoom → coords → engine
/// lookup) so error responses stay consistent across raster and vector
/// tile routes.
///
/// `time` filters the features (OGC API - Tiles DateTime): only those whose
/// temporal geometry intersects it, as the engine applies Features'
/// `datetime` (`/req/collections/rc-datetime-response` A); features without
/// one always match (B). `None` keeps the engine's default selection.
#[allow(clippy::too_many_arguments)]
async fn render_vector_tile(
    headers: HeaderMap,
    id: &str,
    tms_id: &str,
    zoom: u32,
    row: u64,
    col: u64,
    time: Option<RequestedTime>,
    state: AppState,
) -> Result<axum::response::Response, TilesError> {
    let state = state.load_full();

    let tms_kind = TmsKind::from_id(tms_id).ok_or_else(|| {
        TilesError::BadRequest(format!(
            "TileMatrixSet '{tms_id}' is not supported. Supported: {}",
            SUPPORTED_TILE_MATRIX_SETS.join(", ")
        ))
    })?;
    let tms = tilematrixset::get_tile_matrix_set(tms_id)
        .ok_or_else(|| TilesError::Internal("TileMatrixSet lookup failed".into()))?;

    if zoom > params::MAX_ZOOM_LEVEL {
        return Err(TilesError::BadRequest(format!(
            "Zoom level {zoom} exceeds maximum of {}",
            params::MAX_ZOOM_LEVEL
        )));
    }
    if !tms.validate_coords(zoom, row, col) {
        return Err(TilesError::NotFound(format!(
            "Tile {zoom}/{row}/{col} is outside the matrix bounds for {tms_id}"
        )));
    }

    let engine = state
        .feature_engines
        .get(id)
        .ok_or_else(|| {
            TilesError::NotFound(format!("Collection '{id}' has no vector-tile source"))
        })?
        .clone();

    let bbox = tms
        .tile_bbox(zoom, row, col)
        .ok_or_else(|| TilesError::Internal("Failed to compute tile bbox".into()))?;

    // A time subset entirely outside the collection's temporal extent, the
    // `datetime` axis' valid values, is a 204
    // (`/req/collections/rc-subset-definition` C). Any other selection
    // filters: no matching feature is the usual empty tile.
    if let (
        Some(RequestedTime {
            selection,
            from_subset: true,
        }),
        Some((first, last)),
    ) = (&time, engine.temporal_extent())
    {
        if selection.outside(first, last) {
            return Ok(Response::builder()
                .status(StatusCode::NO_CONTENT)
                .header(header::CACHE_CONTROL, "public, max-age=300")
                .body(axum::body::Body::empty())
                .unwrap());
        }
    }
    // The interval the engine filters by, and the cache keys on: equal
    // selections (`datetime=t`, `subset=datetime("t")`) share an entry. A
    // collection without a time dimension has no temporal geometry to
    // filter, so every feature matches (`/req/collections/rc-datetime-response`
    // B): it gets no filter and shares the timeless tile, where Features
    // would answer 400.
    let datetime = time
        .filter(|_| engine.has_time_dimension())
        .map(|t| t.selection.interval());

    let allowlist = PropertyAllowlist::All;
    let props_hash = properties_hash(&allowlist);
    let cache_key = VectorTileKey {
        collection: id.to_string(),
        tms: tms_kind,
        z: zoom,
        x: col,
        y: row,
        properties_hash: props_hash,
        // Engines bump their data version on reload/refresh; folding it into
        // the ETag forces a fresh fetch instead of an infinite `304` loop.
        data_version: engine.data_version(),
        time: datetime.clone(),
    };
    let cache_control = "public, max-age=300";
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    // ETag is content-derived, so we must look at the cached bytes (or freshly
    // encoded bytes) before we can answer `If-None-Match`. A key-derived ETag
    // would let stale browser caches survive a server fix indefinitely.
    if let Some(cached) = state.vector_tile_cache.get(&cache_key) {
        if let Some(ref inm) = if_none_match {
            if ds_render::etag_matches(inm, cached.etag()) {
                return Ok(axum::response::Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header(header::ETAG, cached.etag())
                    .header(header::CACHE_CONTROL, cache_control)
                    .body(axum::body::Body::empty())
                    .unwrap()
                    .into_response());
            }
        }
        return Ok(axum::response::Response::builder()
            .header(header::CONTENT_TYPE, MVT_CONTENT_TYPE)
            .header(header::ETAG, cached.etag())
            .header(header::CACHE_CONTROL, cache_control)
            .header(
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff",
            )
            .header(header::HeaderName::from_static("x-cache"), "HIT")
            .body(axum::body::Body::from(cached.bytes))
            .unwrap()
            .into_response());
    }

    let query_bbox = Bbox::new(bbox[0], bbox[1], bbox[2], bbox[3])
        .map_err(|e| TilesError::BadRequest(format!("Invalid tile bbox: {e}")))?;
    // `limit` semantics differ across engines: `GeoJsonEngine` honours zero
    // literally (returns nothing), `PostgisEngine` treats zero as "no limit".
    // Asking for `MAX_FEATURES_PER_TILE + 1` is unambiguous: every engine
    // returns at most that many features, engines with native SQL limits can
    // stop early, and the density guard below fires cleanly when we hit the
    // cap.
    let query = FeatureQuery {
        bbox: Some(query_bbox),
        limit: params::MAX_FEATURES_PER_TILE + 1,
        offset: 0,
        datetime,
        // Vector tiles carry the whole tile's features and are rendered by
        // the client; a server-side order would cost a sort per tile and
        // change nothing the client can observe.
        sortby: Vec::new(),
        property_filters: Vec::new(),
    };

    let page = engine
        .get_features(&query)
        .map_err(|e| TilesError::Internal(format!("Feature query failed: {e}")))?;

    if page.features.len() > params::MAX_FEATURES_PER_TILE {
        // 422 (Unprocessable Content), not 400: the request itself is
        // well-formed — valid TMS, valid coords, registered collection —
        // and only the data exceeds the per-tile budget.
        return Err(TilesError::Unprocessable(format!(
            "tile-too-dense: {} features exceed maximum of {} — raise minzoom or narrow bbox",
            page.features.len(),
            params::MAX_FEATURES_PER_TILE
        )));
    }

    let features = page.features;
    let layer_name = id.to_string();
    let collection_label = id.to_string();

    // Share the raster semaphore — encoding is CPU-bound and a single budget
    // for tile production keeps DoS surface area minimal. Acquire here (just
    // before `spawn_blocking`) rather than around `get_features` so an engine
    // that does I/O during the feature query doesn't hold a render slot while
    // it waits.
    let job = ds_executor::RenderJob::acquire(state.render_semaphore.clone())
        .await
        .map_err(TilesError::from)?;

    let bytes = job
        .run(move || -> Result<Vec<u8>, ds_mvt::EncodeError> {
            let mut opts = TileEncodeOptions::new(layer_name, tms_kind);
            opts.properties = allowlist;
            encode_tile(&features, bbox, &opts)
        })
        .await
        .map_err(TilesError::from)?
        .map_err(|e| {
            tracing::warn!("MVT encode error for collection '{collection_label}': {e}");
            TilesError::Internal(format!("Encode failed: {e}"))
        })?;

    let cached = CachedTile::new(bytes::Bytes::from(bytes));
    state.vector_tile_cache.insert(cache_key, cached.clone());

    if let Some(ref inm) = if_none_match {
        if ds_render::etag_matches(inm, cached.etag()) {
            return Ok(axum::response::Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::ETAG, cached.etag())
                .header(header::CACHE_CONTROL, cache_control)
                .body(axum::body::Body::empty())
                .unwrap()
                .into_response());
        }
    }

    Ok(axum::response::Response::builder()
        .header(header::CONTENT_TYPE, MVT_CONTENT_TYPE)
        .header(header::ETAG, cached.etag())
        .header(header::CACHE_CONTROL, cache_control)
        .header(
            header::HeaderName::from_static("x-content-type-options"),
            "nosniff",
        )
        .header(header::HeaderName::from_static("x-cache"), "MISS")
        .body(axum::body::Body::from(cached.bytes))
        .unwrap()
        .into_response())
}

/// GET {mount}/collections/{id}/tiles/{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}
///
/// Content-negotiated between raster (PNG/JPEG/WebP, default) and Mapbox
/// Vector Tile (`?f=mvt`). The latter routes through the `FeatureEngine`
/// registry; the former through `MapEngine` as before.
pub async fn get_tile(
    headers: HeaderMap,
    Path((id, tms_id, tile_matrix, tile_row, tile_col)): Path<(String, String, u32, u64, u64)>,
    Query(params): Query<TileQueryParams>,
    RawQuery(query): RawQuery,
    State(state): State<AppState>,
) -> Result<axum::response::Response, TilesError> {
    if params.is_mvt() {
        // As the shared root's vector route: a time, and no map tile
        // parameter (`quality`, sizes, …) or unknown one (#605).
        let subsets = subset::query_values(query.as_deref(), "subset");
        let time = params.validate_vector(query.as_deref(), &subsets)?;
        return render_vector_tile(
            headers,
            &id,
            &tms_id,
            tile_matrix,
            tile_row,
            tile_col,
            time,
            state,
        )
        .await;
    }
    params::reject_unknown_parameters(query.as_deref(), params::MAP_TILE_PARAMETERS)?;
    let subsets = subset::query_values(query.as_deref(), "subset");
    render_tile(
        &id,
        "default",
        &tms_id,
        tile_matrix,
        tile_row,
        tile_col,
        params,
        &subsets,
        headers,
        state,
    )
    .await
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
) -> Result<Response, TilesError> {
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
    // a client draws matches the pixels the tile routes render for that same
    // parameter.
    let (_, layer_styles) =
        layer_style_map(&state.styles, &id, params.parameter_name.as_deref())
            .ok_or_else(|| TilesError::NotFound(format!("Collection '{id}' not found")))?;
    let style_info = layer_styles.get(&style_id).ok_or_else(|| {
        TilesError::NotFound(format!(
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
            .map_err(|e| TilesError::Internal(format!("Legend render failed: {e}")))?
            .map_err(|e| TilesError::Internal(format!("Legend render error: {e}")))?;

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
) -> Result<Response, TilesError> {
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
            .map_err(|e| TilesError::Internal(format!("Legend render failed: {e}")))?
            .map_err(|e| TilesError::Internal(format!("Legend render error: {e}")))?;
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

/// GET {mount}/collections/{id}/styles/{styleId}/tiles/{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}
pub async fn get_styled_tile(
    headers: HeaderMap,
    Path((id, style_id, tms_id, tile_matrix, tile_row, tile_col)): Path<(
        String,
        String,
        String,
        u32,
        u64,
        u64,
    )>,
    Query(params): Query<TileQueryParams>,
    RawQuery(query): RawQuery,
    State(state): State<AppState>,
) -> Result<axum::response::Response, TilesError> {
    if params.is_mvt() {
        return Err(TilesError::BadRequest(
            "Vector tiles (?f=mvt) are not styled — request via /collections/{id}/tiles/...".into(),
        ));
    }
    params::reject_unknown_parameters(query.as_deref(), params::MAP_TILE_PARAMETERS)?;
    let subsets = subset::query_values(query.as_deref(), "subset");
    render_tile(
        &id,
        &style_id,
        &tms_id,
        tile_matrix,
        tile_row,
        tile_col,
        params,
        &subsets,
        headers,
        state,
    )
    .await
}

/// Shared tile rendering logic. `subsets` are the `subset` values in request
/// order.
#[allow(clippy::too_many_arguments)]
async fn render_tile(
    collection_id: &str,
    style_name: &str,
    tms_id: &str,
    zoom: u32,
    row: u64,
    col: u64,
    params: TileQueryParams,
    subsets: &[String],
    headers: HeaderMap,
    state: AppState,
) -> Result<Response, TilesError> {
    let state = state.load_full();
    let (engine, config) = lookup_engine(&state, collection_id)?;

    // Validate TileMatrixSet
    let tms = tilematrixset::get_tile_matrix_set(tms_id).ok_or_else(|| {
        TilesError::BadRequest(format!(
            "TileMatrixSet '{tms_id}' is not supported. Supported: {}",
            SUPPORTED_TILE_MATRIX_SETS.join(", ")
        ))
    })?;

    // Validate zoom level
    if zoom > params::MAX_ZOOM_LEVEL {
        return Err(TilesError::BadRequest(format!(
            "Zoom level {zoom} exceeds maximum of {}",
            params::MAX_ZOOM_LEVEL
        )));
    }

    // Validate tile coordinates
    if !tms.validate_coords(zoom, row, col) {
        return Err(TilesError::NotFound(format!(
            "Tile {zoom}/{row}/{col} is outside the matrix bounds for {tms_id}"
        )));
    }

    // Compute bbox from tile coordinates
    let bbox = tms
        .tile_bbox(zoom, row, col)
        .ok_or_else(|| TilesError::Internal("Failed to compute tile bbox".into()))?;

    // Validate query params
    let validated = params.validate(subsets)?;
    // The image's size: the tile matrix's tileWidth × tileHeight unless the
    // Maps Scaling parameters set it over the tile's area (Map Tilesets
    // `/req/tilesets/tiles-parameters`).
    let frame = tms
        .frame(bbox)
        .ok_or_else(|| TilesError::Internal("Failed to compute tile frame".into()))?;
    let (width, height) = validated.size(&frame, (tms.tile_width, tms.tile_height))?;
    // The format as encoded: an explicit `quality`, else for WebP the
    // collection's `[wms] webp_quality`, else the format default (JPEG 85,
    // lossless WebP). It keys the rendered cache, so a lossy and a lossless
    // tile never alias.
    let format = validated
        .format
        .with_quality(validated.quality, config.webp_quality());

    // An RGB composite (#819) has no style map: its colours come from its
    // channels, and its one style is `default`.
    let composite = composite_parameter(engine.as_ref(), validated.parameter_name.as_deref());

    // Look up style. A `?parameter-name=` request styles from that
    // parameter's own layer when the collection registers one — otherwise a
    // per-parameter colormap would be unreachable through Tiles and every
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
                TilesError::NotFound(format!("Collection '{collection_id}' not found"))
            })?;

            let style_info = layer_styles.get(style_name).ok_or_else(|| {
                TilesError::NotFound(format!(
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
    // Only an instant pins the tile: intervals and `*` follow new data.
    let has_explicit_time = matches!(
        validated.time,
        Some(RequestedTime {
            selection: TimeSelection::Instant(_),
            ..
        })
    );

    // Determine output CRS from TileMatrixSet
    let output_crs = match tms_id {
        "WebMercatorQuad" => ds_core::map_engine::OutputCrs::WebMercator,
        _ => ds_core::map_engine::OutputCrs::Wgs84,
    };
    let content_crs = match tms_id {
        "WebMercatorQuad" => crs_to_uri("EPSG:3857"),
        _ => crs_to_uri("CRS:84"),
    };

    // Single `raster_info_shared()` call covers both default-time resolution and
    // parameter-name validation. Trait contract is O(1) but we still avoid
    // the redundant call.
    let raster_info = engine.raster_info_shared();

    // Parameter selection: ?parameter-name= wins over style.parameter. Mirror
    // the precedence and validation used by api-maps + api-wms so the SPA
    // dropdown works identically across all three raster routes. Engines
    // with an empty `raster_info().parameters` list (single-band GeoTIFF)
    // ignore the parameter at render time — we still accept the query.
    check_parameter_name(
        engine.as_ref(),
        &raster_info,
        collection_id,
        validated.parameter_name.as_deref(),
    )?;
    // For a composite, its name: the engine resolves its time axis, the
    // scans every band has, like a parameter's.
    let effective_parameter = validated.parameter_name.clone().or(style_parameter);

    // Reject an `elevation` against a collection with no vertical axis.
    if validated.z.is_some() && raster_info.vertical.is_none() {
        return Err(TilesError::BadRequest(format!(
            "collection '{collection_id}' has no vertical dimension; \
             the `elevation` parameter is not supported"
        )));
    }

    // The instant to render, shared with Maps ([`subset::render_time`]):
    // omitted, the engine's default, else the parameter's (else the
    // collection's) latest time; an instant as given, snapped below; an
    // interval, a partial date or `*` the latest time step inside it. One
    // that selects no time step is an empty tile, 204: a subset entirely
    // outside the time axis (`/req/collections/rc-subset-definition` C), or
    // an interval holding no data (`/req/core/tc-error` B).
    let time = match subset::render_time(
        validated.time.as_ref(),
        engine.as_ref(),
        &raster_info,
        effective_parameter.as_deref(),
    ) {
        Ok(time) => time,
        Err(none) => {
            tracing::debug!(
                "Tiles: no time step for collection '{collection_id}': {}",
                none.0
            );
            return Ok(Response::builder()
                .status(StatusCode::NO_CONTENT)
                .header(header::CACHE_CONTROL, cache_control_value(false, 0))
                .body(axum::body::Body::empty())
                .unwrap());
        }
    };
    // #521: resolve the run axis to the CONCRETE run the engine will render
    // before the cache key is built (see api-maps for the full rationale —
    // the no-TTL rendered cache keyed on `None` would keep serving the
    // first-rendered run's pixels after a newer run re-covers the same valid
    // times; asking the engine preserves GRIB's cross-run fallback). Engines
    // without runs keep the identity default (`None` stays `None`). The run
    // is the parameter's (#1005), as in api-maps.
    let reference_time =
        engine.resolve_parameter_reference_time(effective_parameter.as_deref(), time, None);
    // #507: snap to the exact timestep the engine will render before the
    // cache key is built — a not-yet-ingested datetime must cache the
    // previous timestep's pixels under the PREVIOUS timestep's key. A
    // parameter with its own time axis snaps on that axis.
    let time = engine.resolve_parameter_time(effective_parameter.as_deref(), time, reference_time);
    // `OGCAPI-datetime` (`/rec/datetime/actual-datetime`) on a collection with
    // a temporal extent: the instant rendered, not the one requested.
    let actual_datetime = time
        .filter(|_| !raster_info.times.is_empty())
        .map(subset::rfc3339);
    let with_datetime = |builder: axum::http::response::Builder| match &actual_datetime {
        Some(value) => builder.header(
            header::HeaderName::from_static("ogcapi-datetime"),
            value.as_str(),
        ),
        None => builder,
    };

    // Build cache key
    let cache_key = CacheKey {
        // The RESOLVED style-layer key — see api-maps render_map: prevents a
        // parameter-layer style aliasing a same-named collection style.
        layer: style_layer_key,
        style: style_name.to_string(),
        format,
        crs: tms_id.to_string(),
        bbox: ds_render::quantize_bbox(&bbox),
        width,
        height,
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
    // different pixels. Mirror the MVT path in `render_vector_tile` (the
    // bug #145 fixed for raster tiles). The render-latency clock (#466)
    // starts at this lookup, so hits and cold renders each report their
    // own tail.
    let render_start = std::time::Instant::now();
    if let Some(cached) = state.rendered_cache.get(&cache_key) {
        if let Some(ref inm) = if_none_match {
            if ds_render::etag_matches(inm, cached.etag()) {
                // 304 from the cache-HIT branch. The `x-cache: HIT` header
                // lets the regression test (and curious clients) distinguish
                // this from a post-render MISS→304, which the handler also
                // serves.
                return Ok(with_datetime(axum::response::Response::builder())
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
        return Ok(with_datetime(axum::response::Response::builder())
            .header(header::CONTENT_TYPE, content_type)
            .header(header::ETAG, cached.etag())
            .header(header::CACHE_CONTROL, cache_control)
            .header(header::HeaderName::from_static("content-crs"), content_crs)
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
        width,
        height,
        planes,
    )
    .await
    .map_err(TilesError::from)?;
    let worker_memory = memory_permit.clone();
    // Where the render's time goes (#147): admission here, the engine read
    // and encode in the worker.
    let mut phases = RenderPhases::default();
    phases.add(RenderPhase::Queue, queue_start.elapsed());

    // Render on a blocking thread
    let engine = engine.clone();
    let rendered_cache = state.rendered_cache.clone();

    // The blocking closure returns Ok(None) for empty (all-nodata) tiles,
    // or Ok(Some(bytes)) for tiles with data.
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
        .map_err(TilesError::from)?;

    let (maybe_bytes, phases) = render_result.map_err(|e| {
        use ds_core::error::DataServerError as DSE;
        // A client mistake (multi-parameter collection rendered without a
        // parameter, bad bbox/datetime) is a 400 with the engine's message,
        // not a 500 that hides it.
        match e {
            DSE::ResourceExhausted | DSE::DeadlineExceeded => {
                TilesError::ServiceUnavailable(e.to_string())
            }
            DSE::InvalidParameter(_)
            | DSE::InvalidBbox(_)
            | DSE::InvalidDatetime(_)
            | DSE::QueryTooLarge(_) => {
                // 4xx-class: DEBUG (not WARN) so a misconfigured client stays
                // diagnosable without flooding the warn stream.
                tracing::debug!(
                    "Tiles render bad-request for collection '{}': {e}",
                    collection_id
                );
                TilesError::BadRequest(e.to_string())
            }
            // ReferenceTimeNotFound is documented to map to 404 (EDR already
            // does); reachable when a pinned run is pruned between resolution
            // and render — routine for nowcast generations (#522).
            DSE::CollectionNotFound(_)
            | DSE::LocationNotFound(_)
            | DSE::ReferenceTimeNotFound(_) => {
                tracing::debug!(
                    "Tiles render not-found for collection '{}': {e}",
                    collection_id
                );
                TilesError::NotFound(e.to_string())
            }
            _ => {
                tracing::warn!("Tiles render error for collection '{}': {e}", collection_id);
                TilesError::Internal(format!("Render failed: {e}"))
            }
        }
    })?;

    // Empty tiles return the pre-generated transparent PNG without caching;
    // populated tiles get cached. Wrap both in `CachedRendered` so the
    // response ETag is FNV-1a over the actual bytes — different pixels
    // produce different ETags (#145). Track the actual Content-Type per
    // branch so the header never lies about the payload (#162). Empty
    // tiles reuse the global `EMPTY_TILE_CACHED` so the FNV-1a hash is
    // computed once per process instead of per request.
    // Each arm produces a `CachedRendered` ready to serve. Only the
    // populated `Some(_)` path inserts into the rendered cache; the
    // EMPTY fast-path intentionally doesn't (the global
    // `EMPTY_TILE_CACHED` already serves as the deterministic empty
    // response).
    let (cached, x_cache, response_content_type) = match maybe_bytes {
        // A tile the Scaling parameters sized gets an empty image of its
        // own size, from the same shared memo.
        None if (width, height) == (params::TILE_SIZE, params::TILE_SIZE) => {
            (EMPTY_TILE_CACHED.clone(), "EMPTY", "image/png")
        }
        None => {
            let empty = ds_render::empty_tile(width, height)
                .map_err(|e| TilesError::Internal(format!("Failed to encode empty tile: {e}")))?;
            (empty, "EMPTY", "image/png")
        }
        Some(bytes) => {
            let cached = ds_render::CachedRendered::new(bytes::Bytes::from(bytes));
            rendered_cache.insert(cache_key, cached.clone());
            (cached, "MISS", content_type)
        }
    };

    // Content-derived ETag now available — do the `If-None-Match`
    // comparison here, after the (cheap) empty-tile clone or fresh
    // encode. Forward the same `x_cache` label the 200 response would
    // carry (`"MISS"` or `"EMPTY"`) so revalidations look the same
    // on dashboards as initial fetches — a client revalidating a
    // cached transparent-tile response sees `304 x-cache: EMPTY`,
    // not a misleading `MISS`. This matters for any tile viewer
    // panning over out-of-coverage areas.
    if let Some(ref inm) = if_none_match {
        if ds_render::etag_matches(inm, cached.etag()) {
            return Ok(with_datetime(axum::response::Response::builder())
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

    Ok(with_datetime(axum::response::Response::builder())
        .header(header::CONTENT_TYPE, response_content_type)
        .header(header::ETAG, cached.etag())
        .header(header::CACHE_CONTROL, cache_control)
        .header(header::HeaderName::from_static("content-crs"), content_crs)
        .header(
            header::HeaderName::from_static("x-content-type-options"),
            "nosniff",
        )
        .header(header::HeaderName::from_static("x-cache"), x_cache)
        .extension(
            RenderTiming::since(collection_id, RenderOutcome::Cold, render_start)
                .with_phases(phases),
        )
        .body(axum::body::Body::from(cached.into_bytes()))
        .unwrap()
        .into_response())
}

impl From<ds_executor::ExecutionError> for TilesError {
    fn from(error: ds_executor::ExecutionError) -> Self {
        match error {
            ds_executor::ExecutionError::Task(e) => Self::Internal(e.to_string()),
            other => Self::ServiceUnavailable(other.to_string()),
        }
    }
}
