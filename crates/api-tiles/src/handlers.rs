use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use arc_swap::ArcSwap;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use serde_json::json;

use ds_core::config::CollectionConfig;
use ds_core::feature::{Bbox, FeatureQuery};
use ds_core::feature_engine::FeatureEngine;
use ds_core::map_engine::MapEngine;
use ds_mvt::{
    encode_tile, properties_hash, CachedTile, PropertyAllowlist, TileEncodeOptions, TmsKind,
    VectorTileCache, VectorTileKey,
};
use ds_render::{CacheKey, RenderedCache, StyleInfo};

use crate::error::TilesError;
use crate::params::{self, TileQueryParams};
use crate::tilematrixset::{self, SUPPORTED_TILE_MATRIX_SETS};

/// Pre-generated 256x256 fully transparent PNG for empty (all-nodata) tiles.
/// Avoids running the colorization + encoding pipeline when a tile has no data.
static EMPTY_TILE_PNG: LazyLock<bytes::Bytes> = LazyLock::new(|| {
    let size = params::TILE_SIZE;
    let rgba = vec![0u8; (size * size * 4) as usize];
    bytes::Bytes::from(
        ds_render::encode_png(&rgba, size, size).expect("encoding empty tile PNG must not fail"),
    )
});

/// Companion to [`EMPTY_TILE_PNG`]: the same bytes wrapped in
/// `CachedRendered`, so the FNV-1a hash that produces the empty-tile
/// ETag is computed **once per process** instead of on every empty-tile
/// response. Both fields are `Clone`-cheap (`Bytes` is `Arc`-backed,
/// `String` is a 20-byte allocation done once), so cloning per request
/// is essentially free. `api-maps` and `api-wms` cannot use this trick
/// — they allocate fresh empty bytes per request to match the requested
/// dimensions — but Tiles is always 256×256.
static EMPTY_TILE_CACHED: LazyLock<ds_render::CachedRendered> =
    LazyLock::new(|| ds_render::CachedRendered::new(EMPTY_TILE_PNG.clone()));

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
    pub base_url: String,
}

pub type AppState = Arc<ArcSwap<TilesState>>;

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

fn cache_control_value(has_explicit_time: bool) -> &'static str {
    if has_explicit_time {
        // Tiles at fixed z/x/y + timestamp are truly immutable
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

fn build_collection_metadata(
    config: &CollectionConfig,
    raster_info: Option<&ds_core::map_engine::RasterInfo>,
    feature_extent: Option<[f64; 4]>,
    styles: Option<&HashMap<String, StyleInfo>>,
    base_url: &str,
) -> serde_json::Value {
    let mut tms_links = Vec::new();
    for tms_id in SUPPORTED_TILE_MATRIX_SETS {
        tms_links.push(json!({
            "tileMatrixSet": tms_id,
            "tileMatrixSetURI": format!("http://www.opengis.net/def/tilematrixset/OGC/1.0/{tms_id}"),
        }));
    }

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
                }));
            }
        }
    }

    // OGC API Tiles §7 constrains `dataType` to "map" (raster) or "vector".
    // A collection that supports both is advertised as "map" so existing raster
    // clients aren't surprised; MVT availability is discoverable via the tile
    // path's content negotiation (`?f=mvt`).
    let data_type = if raster_info.is_some() {
        "map"
    } else {
        "vector"
    };

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

    let mut links = vec![
        json!({
            "href": format!("{base_url}/tiles/collections/{}", config.id),
            "rel": "self",
            "type": "application/json",
            "title": config.title
        }),
        json!({
            "href": format!("{base_url}/tiles/collections/{}/tiles", config.id),
            "rel": "tiles",
            "type": "application/json",
            "title": "Tilesets"
        }),
    ];
    if let Some((title, url)) = config.license.as_ref().and_then(|l| l.card_link()) {
        // No `type`: an operator-supplied license URL may not be HTML, and OGC
        // API Common §6.5.2 wants the link's real media type — omitting is valid.
        links.push(json!({ "href": url, "rel": "license", "title": title }));
    }

    let mut metadata = json!({
        "id": config.id,
        "title": config.title,
        "description": config.description,
        "dataType": data_type,
        "crs": crs_uris,
        "tileMatrixSetLinks": tms_links,
        "styles": style_list,
        "links": links
    });
    // OGC API – Common – Part 2 `keywords`: emit only when non-empty.
    if !config.keywords.is_empty() {
        metadata["keywords"] = json!(config.keywords);
    }

    // No `itemType`: OGC API – Common – Part 2 §7.13 defines it as describing
    // the items reachable at /collections/{id}/items, but the Tiles router has
    // no /items route — tiles are fetched at /tiles/…. Emitting it (even
    // "feature" for vector collections) would be an over-claim a validator
    // probing /items would catch. A collection that is *also* a Features
    // collection advertises itemType on its /features representation, where an
    // /items resource actually exists (review on #298).

    if let Some(sc) = storage_crs {
        metadata["storageCrs"] = json!(sc);
    }

    if let Some(extent) = build_extent(raster_info, feature_extent) {
        metadata["extent"] = extent;
    }

    metadata
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
) -> Option<serde_json::Value> {
    let spatial_extent = raster_info
        .and_then(|i| i.spatial_extent)
        .or(feature_extent);
    // Temporal/vertical/grid come from the raster source only; vector-only
    // collections contribute just the (feature) bbox. An absent `native_crs`
    // is harmless: with `grid_size = None` the grid block is skipped anyway.
    let extent = ds_core::ogc_extent::build_extent(
        spatial_extent,
        raster_info.and_then(|i| i.grid_size),
        raster_info.map(|i| i.native_crs.as_str()).unwrap_or(""),
        raster_info.map(|i| i.times.as_slice()).unwrap_or(&[]),
        raster_info.and_then(|i| i.vertical.as_ref()),
    )?;
    Some(serde_json::to_value(extent).expect("Extent serializes to JSON"))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /tiles/ — Landing page
pub async fn landing_page(
    State(state): State<AppState>,
    Query(fp): Query<ds_core::html::FormatParams>,
    headers: HeaderMap,
) -> Result<Response, TilesError> {
    use ds_core::html::{LinkView, Wanted};
    let wanted = negotiate(fp.f.as_deref(), &headers)?;
    let state = state.load_full();
    let base = &state.base_url;
    let title = "MeteoCore - Tiles";
    let description = "Metocean Data Server \u{2014} OGC API Tiles";
    // (href, rel, type, title) — one source for both representations.
    let links = [
        (
            format!("{base}/tiles/"),
            "self",
            "application/json",
            "This document",
        ),
        (
            format!("{base}/tiles/api"),
            "service-desc",
            "application/vnd.oai.openapi+json;version=3.0",
            "API definition",
        ),
        (
            format!("{base}/tiles/api/docs"),
            "service-doc",
            "text/html",
            "API documentation",
        ),
        (
            format!("{base}/tiles/conformance"),
            "conformance",
            "application/json",
            "Conformance classes",
        ),
        (
            format!("{base}/tiles/collections"),
            "data",
            "application/json",
            "Collections",
        ),
        (
            format!("{base}/tiles/tileMatrixSets"),
            "tiling-schemes",
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
                format!("{base}/tiles/?f=json"),
                "alternate",
                Some("This document as JSON"),
            ));
            Html(ds_core::html::landing_html(title, description, &views)).into_response()
        }
    }))
}

/// OpenAPI `f` (output-format) query parameter, shared by the content-negotiated
/// metadata endpoints (landing, conformance, collections, collection detail).
fn format_parameter() -> serde_json::Value {
    json!({"name": "f", "in": "query", "required": false, "schema": {"type": "string", "enum": ["json", "html"]},
           "description": "Output format. 'json' (default) or 'html'; overrides the Accept header."})
}

/// OpenAPI `parameters` array for the OGC API – Common – Part 4 searchable
/// `/collections` query parameters (plus the shared `f` format selector).
fn searchable_collections_parameters() -> serde_json::Value {
    let mut params = json!([
        {"name": "bbox", "in": "query", "required": false, "schema": {"type": "string"},
         "description": "Filter to collections intersecting this CRS84 bbox: 4 (or 6) comma-separated numbers west,south,east,north."},
        {"name": "bbox-crs", "in": "query", "required": false, "schema": {"type": "string"},
         "description": "CRS of the bbox values. Only CRS84 is supported."},
        {"name": "datetime", "in": "query", "required": false, "schema": {"type": "string"},
         "description": "Filter to collections whose temporal extent intersects this RFC 3339 instant or interval (start/end, ../end, start/..)."},
        {"name": "q", "in": "query", "required": false, "schema": {"type": "string"},
         "description": "Free-text search (comma-separated terms, OR) over collection title, description, and keywords."},
        {"name": "limit", "in": "query", "required": false, "schema": {"type": "integer", "minimum": 1, "maximum": 1000},
         "description": "Maximum number of collections per page (default 1000)."},
        {"name": "offset", "in": "query", "required": false, "schema": {"type": "integer", "minimum": 0},
         "description": "Number of matching collections to skip (pagination cursor)."}
    ]);
    params
        .as_array_mut()
        .expect("searchable params is a JSON array")
        .push(format_parameter());
    params
}

/// GET /tiles/api — OpenAPI 3.0.3 definition
pub async fn api_definition(State(state): State<AppState>) -> impl IntoResponse {
    let state = state.load_full();
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

        collection_paths[format!("/tiles/collections/{id}")] = json!({
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

        collection_paths[format!("/tiles/collections/{id}/tiles")] = json!({
            "get": {
                "summary": format!("List tilesets for {}", config.title),
                "operationId": format!("getTilesets_{id}"),
                "tags": [id],
                "responses": {
                    "200": {"description": "Available tilesets"}
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

        collection_paths[format!("/tiles/collections/{id}/tiles/{{tileMatrixSetId}}/{{tileMatrix}}/{{tileRow}}/{{tileCol}}")] = json!({
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
                    {"$ref": "#/components/parameters/f"},
                    {"$ref": "#/components/parameters/elevation"}
                ],
                "responses": {
                    "200": {
                        "description": "Tile image or vector tile",
                        "content": content
                    },
                    "400": {"description": "Bad request"},
                    "404": {"description": "Tile not found"},
                    "422": {"description": "Tile too dense (feature count exceeds per-tile cap)"},
                    "500": {"description": "Server error"}
                }
            }
        });
    }

    let mut paths = json!({
        "/tiles/": {
            "get": {
                "summary": "Landing page",
                "operationId": "getLandingPage",
                "parameters": [format_parameter()],
                "responses": { "200": {"description": "Landing page"} }
            }
        },
        "/tiles/conformance": {
            "get": {
                "summary": "Conformance classes",
                "operationId": "getConformance",
                "parameters": [format_parameter()],
                "responses": { "200": {"description": "Conformance classes"} }
            }
        },
        "/tiles/collections": {
            "get": {
                "summary": "List tile-enabled collections",
                "operationId": "getCollections",
                "parameters": searchable_collections_parameters(),
                "responses": { "200": {"description": "List of collections"} }
            }
        },
        "/tiles/tileMatrixSets": {
            "get": {
                "summary": "List supported tile matrix sets",
                "operationId": "getTileMatrixSets",
                "responses": { "200": {"description": "List of tile matrix sets"} }
            }
        },
        "/tiles/tileMatrixSets/{tileMatrixSetId}": {
            "get": {
                "summary": "Get tile matrix set definition",
                "operationId": "getTileMatrixSet",
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
        "components": {
            "parameters": {
                "datetime": {
                    "name": "datetime",
                    "in": "query",
                    "required": false,
                    "schema": {"type": "string"},
                    "description": "ISO 8601 timestamp"
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
                }
            }
        }
    });

    Json(openapi)
}

/// GET /tiles/api/docs — Swagger UI
pub async fn api_docs(State(state): State<AppState>) -> impl IntoResponse {
    let state = state.load_full();
    let spec_url = format!("{}/tiles/api", state.base_url);
    axum::response::Html(ds_core::openapi::swagger_ui_html(
        "MeteoCore - Tiles API",
        &spec_url,
    ))
}

/// GET /tiles/conformance
pub async fn conformance(
    State(state): State<AppState>,
    Query(fp): Query<ds_core::html::FormatParams>,
    headers: HeaderMap,
) -> Result<Response, TilesError> {
    use ds_core::html::{LinkView, Wanted};
    let wanted = negotiate(fp.f.as_deref(), &headers)?;
    let state = state.load_full();
    let base = &state.base_url;
    let classes = [
        // OGC API - Common - Part 1: Core (landing page, /conformance,
        // /api) and Part 2: Geospatial Data (/collections + /collections/
        // {id}, JSON). Both are satisfied structurally; the HTML class
        // (.../common-2/.../conf/html) is now declared — the HTML
        // representation is served via `?f=html` / Accept.
        "http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/core",
        "http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/landing-page",
        "http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/oas30",
        "http://www.opengis.net/spec/ogcapi-common-2/1.0/conf/collections",
        "http://www.opengis.net/spec/ogcapi-common-2/1.0/conf/json",
        "http://www.opengis.net/spec/ogcapi-common-2/1.0/conf/html",
        // OGC API - Common - Part 4 (Discovery within many collections,
        // draft 25-046): /collections supports bbox/bbox-crs/datetime/q/
        // limit filtering + offset pagination.
        "http://www.opengis.net/spec/ogcapi-common-4/1.0/conf/searchable-collections",
        "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/core",
        "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/tileset",
        "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/tilesets-list",
        "http://www.opengis.net/spec/tms/2.0/conf/tilematrixset",
        "http://www.opengis.net/spec/tms/2.0/conf/json-tilematrixset",
        "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/png",
        "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/jpeg",
        "http://www.opengis.net/spec/ogcapi-tiles-1/1.0/conf/mvt",
    ];
    Ok(with_vary(match wanted {
        Wanted::Json => Json(json!({ "conformsTo": classes })).into_response(),
        Wanted::Html => {
            let nav = [
                LinkView::new(format!("{base}/tiles/"), "up", Some("Landing page")),
                LinkView::new(
                    format!("{base}/tiles/conformance?f=json"),
                    "alternate",
                    Some("This document as JSON"),
                ),
            ];
            Html(ds_core::html::conformance_html(&classes, &nav)).into_response()
        }
    }))
}

/// GET /tiles/tileMatrixSets — List supported tile matrix sets
pub async fn tile_matrix_sets(State(state): State<AppState>) -> impl IntoResponse {
    let state = state.load_full();
    let base = &state.base_url;
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
                    "href": format!("{base}/tiles/tileMatrixSets/{}", tms.id),
                    "rel": "self",
                    "type": "application/json"
                }]
            }))
        })
        .collect();

    Json(json!({
        "tileMatrixSets": sets,
        "links": [{
            "href": format!("{base}/tiles/tileMatrixSets"),
            "rel": "self",
            "type": "application/json"
        }]
    }))
}

/// GET /tiles/tileMatrixSets/{tileMatrixSetId} — Get tile matrix set definition
pub async fn tile_matrix_set(Path(tms_id): Path<String>) -> Result<impl IntoResponse, TilesError> {
    let tms = tilematrixset::get_tile_matrix_set(&tms_id).ok_or_else(|| {
        TilesError::NotFound(format!(
            "TileMatrixSet '{tms_id}' not found. Available: {}",
            SUPPORTED_TILE_MATRIX_SETS.join(", ")
        ))
    })?;

    Ok(Json(tms.to_json()))
}

/// GET /tiles/collections — List tile-enabled collections
pub async fn collections(
    State(state): State<AppState>,
    Query(sp): Query<ds_core::collection_search::SearchQueryParams>,
    headers: HeaderMap,
) -> Result<Response, TilesError> {
    use ds_core::collection_search::{search, CollectionMatch};
    use ds_core::html::Wanted;

    let wanted = negotiate(sp.f.as_deref(), &headers)?;
    let params = sp
        .parse()
        .map_err(|e| TilesError::BadRequest(e.to_string()))?;
    let state = state.load_full();
    let base = &state.base_url;

    // Surface every tile-enabled collection, regardless of which engine backs
    // it — a vector-only collection that lives in `feature_collections` would
    // otherwise be invisible. Rows are (id, title, description, bbox, time,
    // metadata); tuple element types are inferred (no extra chrono import).
    let mut seen = std::collections::HashSet::new();
    let mut rows: Vec<_> = Vec::new();
    for config in state
        .collections
        .values()
        .chain(state.feature_collections.values())
    {
        if !seen.insert(config.id.clone()) {
            continue;
        }
        let raster_info = state.map_engines.get(&config.id).map(|e| e.raster_info());
        let feature_extent = state
            .feature_engines
            .get(&config.id)
            .and_then(|e| e.spatial_extent());
        let styles = state.styles.get(&config.id);
        let value =
            build_collection_metadata(config, raster_info.as_ref(), feature_extent, styles, base);
        let bbox = raster_info
            .as_ref()
            .and_then(|i| i.spatial_extent)
            .or(feature_extent);
        let time = raster_info
            .as_ref()
            .and_then(|i| i.times.first().copied().zip(i.times.last().copied()));
        rows.push((
            config.id.clone(),
            config.title.clone(),
            config.description.clone(),
            bbox,
            time,
            value,
            config.keywords.clone(),
            config.license.as_ref().map(|l| l.card_label()),
        ));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    let matches: Vec<CollectionMatch> = rows
        .iter()
        .map(|r| CollectionMatch {
            title: &r.1,
            description: &r.2,
            keywords: &r.6,
            bbox: r.3,
            time: r.4,
        })
        .collect();
    let result = search(&matches, &params);
    let href = |offset| {
        format!(
            "{base}/tiles/collections{}",
            sp.query_string(params.limit, offset)
        )
    };

    Ok(with_vary(match wanted {
        Wanted::Json => {
            let colls: Vec<serde_json::Value> =
                result.page.iter().map(|&i| rows[i].5.clone()).collect();
            let number_returned = colls.len();

            let link = |rel: &str, offset: usize, title: Option<&str>| {
                let mut o = json!({ "href": href(offset), "rel": rel, "type": "application/json" });
                if let Some(t) = title {
                    o["title"] = json!(t);
                }
                o
            };
            let mut links = vec![link("self", params.offset, None)];
            if result.has_next {
                links.push(link("next", result.next_offset, Some("Next page")));
            }
            if result.has_prev {
                links.push(link("prev", result.prev_offset, Some("Previous page")));
            }

            Json(json!({
                "collections": colls,
                "numberMatched": result.number_matched,
                "numberReturned": number_returned,
                "links": links
            }))
            .into_response()
        }
        Wanted::Html => {
            use ds_core::html::{CollectionCard, LinkView};
            let cards: Vec<CollectionCard> = result
                .page
                .iter()
                .map(|&i| CollectionCard {
                    id: rows[i].0.clone(),
                    title: rows[i].1.clone(),
                    description: rows[i].2.clone(),
                    self_href: format!("{base}/tiles/collections/{}", rows[i].0),
                    keywords: rows[i].6.clone(),
                    license: rows[i].7.clone(),
                })
                .collect();
            let mut nav = vec![LinkView::new(
                href(params.offset),
                "self",
                Some("This page"),
            )];
            if result.has_next {
                nav.push(LinkView::new(
                    href(result.next_offset),
                    "next",
                    Some("Next page"),
                ));
            }
            if result.has_prev {
                nav.push(LinkView::new(
                    href(result.prev_offset),
                    "prev",
                    Some("Previous page"),
                ));
            }
            // rel="alternate" to the JSON representation, preserving the current
            // bbox/datetime/q/limit/offset filters (parity with the other HTML
            // metadata pages).
            nav.push(LinkView::new(
                format!(
                    "{base}/tiles/collections{}",
                    sp.query_string_with_format(params.limit, params.offset, "json")
                ),
                "alternate",
                Some("This page as JSON"),
            ));
            Html(ds_core::html::collections_html("Collections", &cards, &nav)).into_response()
        }
    }))
}

/// GET /tiles/collections/{id} — Collection detail
pub async fn collection(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Query(fp): Query<ds_core::html::FormatParams>,
    headers: HeaderMap,
) -> Result<Response, TilesError> {
    use ds_core::html::{CollectionCard, LinkView, Wanted};
    let wanted = negotiate(fp.f.as_deref(), &headers)?;
    let state = state.load_full();
    let raster_info = state.map_engines.get(&id).map(|e| e.raster_info());
    let feature_extent = state
        .feature_engines
        .get(&id)
        .and_then(|e| e.spatial_extent());
    let config = state
        .collections
        .get(&id)
        .or_else(|| state.feature_collections.get(&id))
        .ok_or_else(|| TilesError::NotFound(format!("Collection '{id}' not found")))?;
    if raster_info.is_none() && !state.feature_engines.contains_key(&id) {
        return Err(TilesError::NotFound(format!(
            "Collection '{id}' has no tile source"
        )));
    }
    let base = &state.base_url;
    Ok(with_vary(match wanted {
        Wanted::Json => {
            let styles = state.styles.get(&id);
            Json(build_collection_metadata(
                config,
                raster_info.as_ref(),
                feature_extent,
                styles,
                base,
            ))
            .into_response()
        }
        Wanted::Html => {
            let card = CollectionCard {
                id: config.id.clone(),
                title: config.title.clone(),
                description: config.description.clone(),
                self_href: format!("{base}/tiles/collections/{}", config.id),
                keywords: config.keywords.clone(),
                license: config.license.as_ref().map(|l| l.card_label()),
            };
            let links = [
                LinkView::new(
                    format!("{base}/tiles/collections/{}?f=json", config.id),
                    "alternate",
                    Some("JSON"),
                ),
                LinkView::new(
                    format!("{base}/tiles/collections"),
                    "collection",
                    Some("All collections"),
                ),
            ];
            Html(ds_core::html::collection_html(&card, &links)).into_response()
        }
    }))
}

/// GET /tiles/collections/{id}/tiles — List tilesets for a collection
pub async fn collection_tilesets(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, TilesError> {
    let state = state.load_full();
    let raster_info = state.map_engines.get(&id).map(|e| e.raster_info());
    let feature_extent = state
        .feature_engines
        .get(&id)
        .and_then(|e| e.spatial_extent());
    let config = state
        .collections
        .get(&id)
        .or_else(|| state.feature_collections.get(&id))
        .ok_or_else(|| TilesError::NotFound(format!("Collection '{id}' not found")))?;
    let has_raster = raster_info.is_some();
    let has_vector = state.feature_engines.contains_key(&id);
    if !has_raster && !has_vector {
        return Err(TilesError::NotFound(format!(
            "Collection '{id}' has no tile source"
        )));
    }
    let base = &state.base_url;

    let max_zoom = params::DEFAULT_MAX_ZOOM;
    let spatial_extent = raster_info
        .as_ref()
        .and_then(|i| i.spatial_extent)
        .or(feature_extent);

    let mut tilesets = Vec::new();
    for tms_id in SUPPORTED_TILE_MATRIX_SETS {
        let tms = match tilematrixset::get_tile_matrix_set(tms_id) {
            Some(t) => t,
            None => continue,
        };

        let limits = spatial_extent.map(|bbox| tms.limits_for_extent(bbox, max_zoom));

        let mut item_links = Vec::new();
        if has_raster {
            item_links.push(json!({
                "href": format!(
                    "{base}/tiles/collections/{}/tiles/{tms_id}/{{tileMatrix}}/{{tileRow}}/{{tileCol}}",
                    config.id
                ),
                "rel": "item",
                "type": "image/png",
                "templated": true
            }));
        }
        if has_vector {
            item_links.push(json!({
                "href": format!(
                    "{base}/tiles/collections/{}/tiles/{tms_id}/{{tileMatrix}}/{{tileRow}}/{{tileCol}}?f=mvt",
                    config.id
                ),
                "rel": "item",
                "type": MVT_CONTENT_TYPE,
                "templated": true
            }));
        }

        let mut links = vec![json!({
            "href": format!("{base}/tiles/tileMatrixSets/{tms_id}"),
            "rel": "http://www.opengis.net/def/rel/ogc/1.0/tiling-scheme",
            "type": "application/json"
        })];
        links.extend(item_links);

        let mut tileset = json!({
            "dataType": if has_raster { "map" } else { "vector" },
            "crs": tms.crs,
            "tileMatrixSetURI": format!("http://www.opengis.net/def/tilematrixset/OGC/1.0/{tms_id}"),
            "links": links,
        });

        if let Some(limits) = limits {
            tileset["tileMatrixSetLimits"] = json!(limits);
        }

        tilesets.push(tileset);
    }

    Ok(Json(json!({
        "tilesets": tilesets,
        "links": [{
            "href": format!("{base}/tiles/collections/{}/tiles", id),
            "rel": "self",
            "type": "application/json"
        }]
    })))
}

/// MVT MIME type registered with IANA.
const MVT_CONTENT_TYPE: &str = "application/vnd.mapbox-vector-tile";

/// Encode an MVT from a `FeatureEngine` and return it as an HTTP response.
///
/// Reached through content negotiation on the standard tile path:
/// `GET /collections/{id}/tiles/{tms}/{z}/{row}/{col}?f=mvt`.
/// Validation order mirrors `render_tile` (TMS → zoom → coords → engine
/// lookup) so error responses stay consistent across raster and vector
/// tile routes.
async fn render_vector_tile(
    headers: HeaderMap,
    id: &str,
    tms_id: &str,
    zoom: u32,
    row: u64,
    col: u64,
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
        datetime: None,
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
    let _permit = tokio::time::timeout(ds_render::RENDER_TIMEOUT, state.render_semaphore.acquire())
        .await
        .map_err(|_| TilesError::ServiceUnavailable("Server busy, try again later".to_string()))?
        .map_err(|_| TilesError::Internal("Render semaphore closed".to_string()))?;

    let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, ds_mvt::EncodeError> {
        let mut opts = TileEncodeOptions::new(layer_name, tms_kind);
        opts.properties = allowlist;
        encode_tile(&features, bbox, &opts)
    })
    .await
    .map_err(|e| TilesError::Internal(format!("Encode task failed: {e}")))?
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

/// GET /tiles/collections/{id}/tiles/{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}
///
/// Content-negotiated between raster (PNG/JPEG/WebP, default) and Mapbox
/// Vector Tile (`?f=mvt`). The latter routes through the `FeatureEngine`
/// registry; the former through `MapEngine` as before.
pub async fn get_tile(
    headers: HeaderMap,
    Path((id, tms_id, tile_matrix, tile_row, tile_col)): Path<(String, String, u32, u64, u64)>,
    Query(params): Query<TileQueryParams>,
    State(state): State<AppState>,
) -> Result<axum::response::Response, TilesError> {
    if params.is_mvt() {
        return render_vector_tile(
            headers,
            &id,
            &tms_id,
            tile_matrix,
            tile_row,
            tile_col,
            state,
        )
        .await;
    }
    render_tile(
        &id,
        "default",
        &tms_id,
        tile_matrix,
        tile_row,
        tile_col,
        params,
        headers,
        state,
    )
    .await
    .map(|r| r.into_response())
}

/// GET /tiles/collections/{id}/styles/{styleId}/tiles/{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}
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
    State(state): State<AppState>,
) -> Result<axum::response::Response, TilesError> {
    if params.is_mvt() {
        return Err(TilesError::BadRequest(
            "Vector tiles (?f=mvt) are not styled — request via /collections/{id}/tiles/...".into(),
        ));
    }
    render_tile(
        &id,
        &style_id,
        &tms_id,
        tile_matrix,
        tile_row,
        tile_col,
        params,
        headers,
        state,
    )
    .await
    .map(|r| r.into_response())
}

/// Shared tile rendering logic.
#[allow(clippy::too_many_arguments)]
async fn render_tile(
    collection_id: &str,
    style_name: &str,
    tms_id: &str,
    zoom: u32,
    row: u64,
    col: u64,
    params: TileQueryParams,
    headers: HeaderMap,
    state: AppState,
) -> Result<impl IntoResponse, TilesError> {
    let state = state.load_full();
    let (engine, _config) = lookup_engine(&state, collection_id)?;

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
    let validated = params.validate()?;

    // Look up style
    let layer_styles = state
        .styles
        .get(collection_id)
        .ok_or_else(|| TilesError::NotFound(format!("Collection '{collection_id}' not found")))?;

    let style_info = layer_styles.get(style_name).ok_or_else(|| {
        TilesError::NotFound(format!(
            "Style '{style_name}' not found for collection '{collection_id}'. Available: {}",
            layer_styles.keys().cloned().collect::<Vec<_>>().join(", ")
        ))
    })?;

    let colormap = style_info.colormap.clone();
    let content_type = validated.format.content_type();
    let has_explicit_time = validated.time.is_some();

    // Determine output CRS from TileMatrixSet
    let output_crs = match tms_id {
        "WebMercatorQuad" => ds_core::map_engine::OutputCrs::WebMercator,
        _ => ds_core::map_engine::OutputCrs::Wgs84,
    };
    let content_crs = match tms_id {
        "WebMercatorQuad" => crs_to_uri("EPSG:3857"),
        _ => crs_to_uri("CRS:84"),
    };

    // Single `raster_info()` call covers both default-time resolution and
    // parameter-name validation. Trait contract is O(1) but we still avoid
    // the redundant call.
    let raster_info = engine.raster_info();
    let time = validated.time.or_else(|| raster_info.times.last().copied());

    let tile_size = params::TILE_SIZE;

    // Parameter selection: ?parameter-name= wins over style.parameter. Mirror
    // the precedence and validation used by api-maps + api-wms so the SPA
    // dropdown works identically across all three raster routes. Engines
    // with an empty `raster_info().parameters` list (single-band GeoTIFF)
    // ignore the parameter at render time — we still accept the query.
    if let Some(pname) = validated.parameter_name.as_deref() {
        if !raster_info.parameters.is_empty()
            && !raster_info.parameters.iter().any(|(name, _)| name == pname)
        {
            let mut supported: Vec<&str> = raster_info
                .parameters
                .iter()
                .map(|(n, _)| n.as_str())
                .collect();
            supported.sort_unstable();
            return Err(TilesError::BadRequest(format!(
                "parameter-name '{pname}' is not available for collection '{collection_id}'. \
                 Available: {}",
                supported.join(", ")
            )));
        }
    }
    let effective_parameter = validated
        .parameter_name
        .clone()
        .or_else(|| style_info.parameter.clone());

    // Reject an `elevation` against a collection with no vertical axis.
    if validated.z.is_some() && raster_info.vertical.is_none() {
        return Err(TilesError::BadRequest(format!(
            "collection '{collection_id}' has no vertical dimension; \
             the `elevation` parameter is not supported"
        )));
    }

    // Build cache key
    let cache_key = CacheKey {
        layer: collection_id.to_string(),
        style: style_name.to_string(),
        format: match validated.format {
            ds_render::ImageFormat::Png => 0,
            ds_render::ImageFormat::Jpeg => 1,
            ds_render::ImageFormat::Webp => 2,
        },
        crs: tms_id.to_string(),
        bbox: ds_render::quantize_bbox(&bbox),
        width: tile_size,
        height: tile_size,
        time,
        parameter: effective_parameter.clone(),
        z: validated.z.map(ds_render::quantize_z),
        // Tiles `reference_time` query parameter is a follow-up (#337 Phase 4);
        // the handler always queries the engine's latest run for now.
        reference_time: None,
    };

    let cache_control = cache_control_value(has_explicit_time);
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|h| h.to_str().ok())
        .map(str::to_string);

    // Cache lookup runs BEFORE the If-None-Match check. The ETag is
    // content-derived (see `CachedRendered::new`), so a key-derived 304
    // short-circuit would be wrong: it would let a browser holding the
    // pre-fix entry keep getting 304 after the server starts producing
    // different pixels. Mirror the MVT path in `render_vector_tile` (the
    // bug #145 fixed for raster tiles).
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
                    .body(axum::body::Body::empty())
                    .unwrap()
                    .into_response());
            }
        }
        return Ok(axum::response::Response::builder()
            .header(header::CONTENT_TYPE, content_type)
            .header(header::ETAG, cached.etag())
            .header(header::CACHE_CONTROL, cache_control)
            .header(header::HeaderName::from_static("content-crs"), content_crs)
            .header(
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff",
            )
            .header(header::HeaderName::from_static("x-cache"), "HIT")
            .body(axum::body::Body::from(cached.into_bytes()))
            .unwrap()
            .into_response());
    }

    // Acquire render semaphore (with timeout to shed load under pressure)
    let _permit = tokio::time::timeout(ds_render::RENDER_TIMEOUT, state.render_semaphore.acquire())
        .await
        .map_err(|_| TilesError::ServiceUnavailable("Server busy, try again later".to_string()))?
        .map_err(|_| TilesError::Internal("Render semaphore closed".to_string()))?;

    // Render on a blocking thread
    let engine = engine.clone();
    let format = validated.format;
    let rendered_cache = state.rendered_cache.clone();

    // The blocking closure returns Ok(None) for empty (all-nodata) tiles,
    // or Ok(Some(bytes)) for tiles with data.
    let render_parameter = effective_parameter;
    let render_z = validated.z;

    let render_result = tokio::task::spawn_blocking(move || {
        let tile = engine.get_raster_tile(
            bbox,
            tile_size,
            tile_size,
            time,
            &output_crs,
            render_parameter.as_deref(),
            render_z,
            None,
        )?;

        // If every pixel is nodata, skip colorization + encoding entirely.
        if tile.is_empty() {
            return Ok(None);
        }

        ds_render::render_tile(&tile, colormap.as_ref(), format).map(Some)
    })
    .await
    .map_err(|e| TilesError::Internal(format!("Render task failed: {e}")))?;

    let maybe_bytes = render_result.map_err(|e| {
        use ds_core::error::DataServerError as DSE;
        // A client mistake (multi-parameter collection rendered without a
        // parameter, bad bbox/datetime) is a 400 with the engine's message,
        // not a 500 that hides it.
        match e {
            DSE::InvalidParameter(_) | DSE::InvalidBbox(_) | DSE::InvalidDatetime(_) => {
                // 4xx-class: DEBUG (not WARN) so a misconfigured client stays
                // diagnosable without flooding the warn stream.
                tracing::debug!(
                    "Tiles render bad-request for collection '{}': {e}",
                    collection_id
                );
                TilesError::BadRequest(e.to_string())
            }
            DSE::CollectionNotFound(_) | DSE::LocationNotFound(_) => {
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
        None => (EMPTY_TILE_CACHED.clone(), "EMPTY", "image/png"),
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
            return Ok(axum::response::Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::ETAG, cached.etag())
                .header(header::CACHE_CONTROL, cache_control)
                .header(header::HeaderName::from_static("x-cache"), x_cache)
                .body(axum::body::Body::empty())
                .unwrap()
                .into_response());
        }
    }

    Ok(axum::response::Response::builder()
        .header(header::CONTENT_TYPE, response_content_type)
        .header(header::ETAG, cached.etag())
        .header(header::CACHE_CONTROL, cache_control)
        .header(header::HeaderName::from_static("content-crs"), content_crs)
        .header(
            header::HeaderName::from_static("x-content-type-options"),
            "nosniff",
        )
        .header(header::HeaderName::from_static("x-cache"), x_cache)
        .body(axum::body::Body::from(cached.into_bytes()))
        .unwrap()
        .into_response())
}
