//! Built-in preview UI surface.
//!
//! Two responsibilities:
//!
//! * `GET /preview/manifest.json` — aggregated discovery JSON consumed by
//!   the SPA. Replaces five separate `/{api}/collections` probes per page
//!   load and reconciles per-API schema drift server-side.
//! * `GET /preview` (and `/preview/{*path}`) — serve the SPA's static
//!   assets (HTML, JS, CSS, vendored MapLibre) embedded at build time via
//!   `rust-embed`. No external requests at runtime; the binary is a
//!   self-contained demo.

use std::collections::BTreeSet;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::{json, Value};

use ds_core::config::CollectionConfig;

use crate::admin::AdminState;

// ---------------------------------------------------------------------------
// Static-asset embedding
// ---------------------------------------------------------------------------

/// Everything under `crates/server/preview/` gets baked into the binary at
/// compile time. The `manifest.json` route is *not* served from here; it
/// has its own handler that reads live `ServerState`.
#[derive(RustEmbed)]
#[folder = "preview/"]
#[exclude = "*.map"]
struct PreviewAssets;

/// Locked-down CSP for the SPA shell. All resources are same-origin; the
/// only exceptions are MapLibre's WebGL worker (which uses `blob:` URLs
/// internally) and `data:`/`blob:` image URIs MapLibre generates for raster
/// tiles in some code paths. No inline scripts and no `unsafe-eval` — every
/// dynamic value in `app.js` reaches the DOM via `.textContent`, which CSP
/// doesn't restrict. Defence in depth for future phases.
///
/// **Phase 3 caveat:** when external basemap tile providers land,
/// `connect-src 'self'` will silently block the basemap fetch. Either
/// relax it to a specific allowlist (e.g. `connect-src 'self'
/// https://basemaps.example.com`) or expose basemap tiles through this
/// server's own routes. Same caveat applies to `img-src` if tiles are
/// rendered as `<img>` rather than fetched.
const PREVIEW_CSP: &str = "default-src 'self'; \
                           style-src 'self'; \
                           script-src 'self'; \
                           img-src 'self' data: blob:; \
                           worker-src blob:; \
                           connect-src 'self'; \
                           object-src 'none'; \
                           base-uri 'self'; \
                           frame-ancestors 'none'";

/// Format the `[u8; 32]` SHA-256 hash that `rust-embed` exposes per asset
/// into a quoted hex ETag (`"hexhexhex..."`, 64 hex chars + quotes).
/// Stable across binary rebuilds for the same asset bytes; changes the
/// moment the embedded content does.
///
/// `write!` straight onto the `String` avoids the 32 temporary
/// `format!(...)` heap allocations the obvious push-loop would do.
fn sha256_etag(hash: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(2 + 64);
    out.push('"');
    for b in hash {
        // `write!` into a String never fails — `Write` impl for String is
        // infallible — so unwrapping is safe and the compiler optimises
        // the panic away.
        write!(&mut out, "{b:02x}").expect("writing to String cannot fail");
    }
    out.push('"');
    out
}

/// `GET /preview` — serve the SPA shell. Equivalent to `/preview/index.html`.
pub async fn index_handler(headers: axum::http::HeaderMap) -> Response {
    serve_asset("index.html", &headers)
}

/// `GET /preview/{*path}` — serve any embedded asset by relative path.
pub async fn asset_handler(Path(path): Path<String>, headers: axum::http::HeaderMap) -> Response {
    serve_asset(&path, &headers)
}

fn serve_asset(path: &str, request_headers: &axum::http::HeaderMap) -> Response {
    let Some(content) = PreviewAssets::get(path) else {
        // Generic body — `path` is user-controlled. Reflecting it isn't an
        // XSS vector here (`text/plain`) but matches the project's
        // "no internal detail in client errors" guideline. `nosniff` on
        // the 404 matches the 200 path so a browser can't sniff this
        // `text/plain` response as something else under any edge case.
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .header(
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff",
            )
            .body(Body::from("Not Found"))
            .expect("static 404 response headers are valid");
    };
    let mime = mime_for(path);
    let is_html = mime.starts_with("text/html");
    // Cache strategy splits by asset class:
    //
    // * `index.html` — entry point with a stable URL. `no-cache` forces the
    //   browser to revalidate on every load so a binary upgrade ships the
    //   new shell immediately. The ETag below lets that revalidation come
    //   back as `304 Not Modified` (~0 bytes on the wire) when nothing
    //   changed; without it every reload re-fetches the body.
    // * `vendor/*` — version-pinned via `scripts/vendor-maplibre.sh` but
    //   the URL itself is **not** content-addressed (path doesn't carry
    //   the version), so `immutable` is wrong: after a vendor bump
    //   clients would keep the old bundle until the entry expired. Keep
    //   the long `max-age` for cheap reuse and rely on the ETag to
    //   short-circuit revalidation when nothing changed.
    // * Everything else (`app.js`, `app.css`, etc.) — non-fingerprinted
    //   stable URLs that change with every binary; `max-age=300` +
    //   `must-revalidate` gives browsers cheap reuse during a session
    //   without pinning them across deploys.
    let cache_control = if is_html {
        "no-cache"
    } else if path.starts_with("vendor/") {
        "public, max-age=86400"
    } else {
        "public, max-age=300, must-revalidate"
    };
    let etag = sha256_etag(&content.metadata.sha256_hash());
    // Conditional GET: short-circuit to `304 Not Modified` when the client
    // already has the same body. Saves the full payload on every reload of
    // a `no-cache`-marked asset.
    if let Some(inm) = request_headers.get(header::IF_NONE_MATCH) {
        if let Ok(inm_str) = inm.to_str() {
            if ds_render::etag_matches(inm_str, &etag) {
                // `nosniff` here is mostly cosmetic — browsers fold a 304's
                // headers into the cached 200, which already carried this
                // value — but keeping the two paths symmetric removes a
                // class of "did we cover that header in every branch?"
                // bookkeeping when the response shape evolves.
                return Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header(header::ETAG, &etag)
                    .header(header::CACHE_CONTROL, cache_control)
                    .header(
                        header::HeaderName::from_static("x-content-type-options"),
                        "nosniff",
                    )
                    .body(Body::empty())
                    .expect("static 304 response headers are valid");
            }
        }
    }
    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, cache_control)
        .header(header::ETAG, &etag)
        .header(
            header::HeaderName::from_static("x-content-type-options"),
            "nosniff",
        );
    if is_html {
        builder = builder.header(
            header::HeaderName::from_static("content-security-policy"),
            PREVIEW_CSP,
        );
    }
    builder
        .body(Body::from(content.data.into_owned()))
        .expect("static preview response headers are valid")
}

fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Maximum number of explicit timestamps emitted per collection.
/// Datasets with more timesteps set `temporal_extent.truncated = true`.
const MAX_TEMPORAL_VALUES: usize = 1000;

/// Soft byte limit above which we warn the operator that the manifest is large.
/// Doesn't reject — operators can paginate or filter the collection list.
const MANIFEST_WARN_BYTES: usize = 1_048_576;

/// Default and maximum number of collections returned per page.
const DEFAULT_PAGE_LIMIT: usize = 100;
const MAX_PAGE_LIMIT: usize = 1000;

/// Query parameters for `GET /preview/manifest.json`.
///
/// Pagination defaults are conservative — a 1000-collection deployment can
/// fetch additional pages without ever returning a manifest large enough to
/// stall the browser.
#[derive(Debug, Deserialize, Default)]
pub struct ManifestParams {
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

impl ManifestParams {
    fn resolved(&self) -> (usize, usize) {
        let offset = self.offset.unwrap_or(0);
        let limit = self
            .limit
            .unwrap_or(DEFAULT_PAGE_LIMIT)
            .clamp(1, MAX_PAGE_LIMIT);
        (offset, limit)
    }
}

/// `GET /preview/manifest.json` — aggregated collection inventory for the UI.
pub async fn manifest_handler(
    State(state): State<AdminState>,
    Query(params): Query<ManifestParams>,
) -> impl IntoResponse {
    let (offset, limit) = params.resolved();
    let manifest = build_manifest(&state, offset, limit);

    // Size guard — log only, never reject; pagination defaults already prevent
    // pathological responses, and the soft warn lets operators see drift early.
    //
    // `serde_json::to_vec` on a `serde_json::Value` essentially never fails
    // (the input is already validated JSON), but if it does we log and fall
    // back to `{}` rather than 500-ing the request — preview UIs can keep
    // rendering whatever cached snapshot they hold while operators see the
    // error in logs.
    let body = serde_json::to_vec(&manifest).unwrap_or_else(|e| {
        tracing::error!(error = %e, "preview manifest JSON serialisation failed");
        b"{}".to_vec()
    });
    if body.len() > MANIFEST_WARN_BYTES {
        tracing::warn!(
            "preview manifest body is {} bytes ({} collections, offset={}, limit={}); \
             consider filtering or shrinking temporal extents",
            body.len(),
            manifest["pagination"]["returned"].as_u64().unwrap_or(0),
            offset,
            limit
        );
    }

    // `no-store`: the manifest mirrors live `ArcSwap` state. After a
    // `POST /admin/collections/reload` any cached copy (browser, CDN,
    // intermediary) would mask the new state until its TTL expires.
    // Cheaper to re-fetch every load than to debug stale-cache reports.
    // `nosniff` is paranoia for `application/json` (no browser MIME-sniffs
    // JSON) but matching `serve_asset`'s 200/304/404 paths keeps the
    // security-header surface uniform across the entire `/preview` route.
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
            (
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff",
            ),
        ],
        body,
    )
}

/// Build a denormalized inventory across every per-API `*State`.
///
/// Pure: takes `&AdminState`, returns JSON. Used by the handler and by tests.
///
/// **Snapshot semantics.** The five `load_full()` calls below are *not*
/// atomic — a concurrent `POST /admin/collections/reload` racing between
/// them can produce a torn snapshot where a collection is in EDR's view
/// but not Tiles' (or vice versa). The cost would be momentarily-wrong
/// `apis[]`/extents on the next request after a reload; the LRU clears
/// once all five `ArcSwap`s settle. This is an accepted trade-off of
/// the per-API `ArcSwap` pattern — wrapping all five in a single outer
/// `ArcSwap` would fix it but is out of scope for this PR.
pub(crate) fn build_manifest(state: &AdminState, offset: usize, limit: usize) -> Value {
    let edr = state.edr.load_full();
    let features = state.features.load_full();
    let maps = state.maps.load_full();
    let tiles = state.tiles.load_full();
    let wms = state.wms.load_full();

    // All five `*State.base_url` fields are initialised from the same
    // `config.server.base_url()` at load time, so any of them is correct
    // in practice. Sourcing from `tiles.base_url` makes the dependency
    // explicit: this function builds tile URL templates, so it reads
    // base_url from the state that owns those URLs. A tile-only
    // deployment with no EDR collections would now stay correct if
    // per-API base-URL overrides ever land.
    let base_url = tiles.base_url.clone();

    // Build the canonical id ordering: union of all *State.collections keys,
    // sorted lexicographically. Stable across reloads when configs are stable.
    let mut ids: BTreeSet<&str> = BTreeSet::new();
    ids.extend(edr.collections.keys().map(String::as_str));
    ids.extend(features.collections.keys().map(String::as_str));
    ids.extend(maps.collections.keys().map(String::as_str));
    ids.extend(tiles.collections.keys().map(String::as_str));
    ids.extend(tiles.feature_collections.keys().map(String::as_str));
    ids.extend(wms.collections.keys().map(String::as_str));

    let total = ids.len();
    let returned: Vec<&str> = ids.into_iter().skip(offset).take(limit).collect();
    let returned_count = returned.len();

    let entries: Vec<Value> = returned
        .into_iter()
        .map(|id| build_entry(id, &base_url, &edr, &features, &maps, &tiles, &wms))
        .collect();

    let next = if offset + returned_count < total {
        Some(offset + returned_count)
    } else {
        None
    };

    json!({
        "collections": entries,
        "pagination": {
            "offset": offset,
            "limit": limit,
            "total": total,
            "returned": returned_count,
            "next": next
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn build_entry(
    id: &str,
    base_url: &str,
    edr: &api_edr::handlers::EdrState,
    features: &api_features::handlers::FeaturesState,
    maps: &api_maps::handlers::MapsState,
    tiles: &api_tiles::handlers::TilesState,
    wms: &api_wms::handlers::WmsState,
) -> Value {
    // Pull a CollectionConfig from whichever state was the first to register
    // this id. They all carry the same `apis`/`title`/`description` because
    // they're cloned from the same source CollectionConfig at load time.
    let config = first_config(id, edr, features, maps, tiles, wms);

    let title = config.map(|c| c.title.as_str()).unwrap_or(id);
    let description = config.map(|c| c.description.as_str()).unwrap_or("");
    let apis: Vec<&str> = config
        .map(|c| c.apis.iter().map(String::as_str).collect())
        .unwrap_or_default();

    let mut entry = json!({
        "id": id,
        "title": title,
        "description": description,
        "apis": apis,
    });

    if let Some(extent) = resolve_spatial_extent(id, edr, features, maps, tiles, wms) {
        entry["spatial_extent"] = json!(extent);
    }

    if let Some(temporal) = resolve_temporal_extent(id, edr, maps, tiles, wms) {
        entry["temporal_extent"] = temporal;
    }

    // Style source precedence is shared between the top-level `styles[]`
    // list and the `default_style` inside the raster tile descriptor —
    // otherwise a `default_style` could reference a style absent from
    // `styles[]`, and a UI client would build a 404-producing styled-tile
    // URL. `maps` first (it owns the canonical Maps styles), `tiles`
    // second (independent style set when only Tiles is wired), `wms`
    // third (WMS-only collections still expose colormaps via `[wms]`).
    let effective_styles = maps
        .styles
        .get(id)
        .or_else(|| tiles.styles.get(id))
        .or_else(|| wms.styles.get(id));

    // Tile representations — emit only what's actually wired up so the UI
    // doesn't render a layer toggle for a dead endpoint.
    let mut tile_block = serde_json::Map::new();
    if tiles.feature_engines.contains_key(id) {
        tile_block.insert("vector".into(), vector_tile_descriptor(id, base_url));
    }
    if tiles.map_engines.contains_key(id) {
        tile_block.insert(
            "raster".into(),
            raster_tile_descriptor(id, base_url, effective_styles),
        );
    }
    if !tile_block.is_empty() {
        entry["tiles"] = Value::Object(tile_block);
    }

    if let Some(styles) = effective_styles {
        entry["styles"] = json!(style_list(styles));
    }

    entry
}

fn first_config<'a>(
    id: &str,
    edr: &'a api_edr::handlers::EdrState,
    features: &'a api_features::handlers::FeaturesState,
    maps: &'a api_maps::handlers::MapsState,
    tiles: &'a api_tiles::handlers::TilesState,
    wms: &'a api_wms::handlers::WmsState,
) -> Option<&'a CollectionConfig> {
    edr.collections
        .get(id)
        .or_else(|| features.collections.get(id))
        .or_else(|| maps.collections.get(id))
        .or_else(|| tiles.collections.get(id))
        .or_else(|| tiles.feature_collections.get(id))
        .or_else(|| wms.collections.get(id))
}

// TODO: `resolve_spatial_extent` and `resolve_temporal_extent` both call
// `raster_info()` on every maps / tiles / wms engine, which clones the
// engine's `times: Vec<DateTime<Utc>>`. At default `limit=100` with
// archive-sized collections (hundreds of timesteps) this is ~2× the
// necessary clone work. Out of scope for the initial Phase 1 PR; the fix
// is to call `raster_info()` once per entry in `build_entry` and pass
// references down to both resolvers.
#[allow(clippy::too_many_arguments)]
fn resolve_spatial_extent(
    id: &str,
    edr: &api_edr::handlers::EdrState,
    features: &api_features::handlers::FeaturesState,
    maps: &api_maps::handlers::MapsState,
    tiles: &api_tiles::handlers::TilesState,
    wms: &api_wms::handlers::WmsState,
) -> Option<[f64; 4]> {
    if let Some(engine) = edr.engines.get(id) {
        if let Some(bbox) = engine.get_spatial_extent() {
            return Some(bbox);
        }
    }
    if let Some(engine) = maps.engines.get(id) {
        if let Some(bbox) = engine.raster_info().spatial_extent {
            return Some(bbox);
        }
    }
    if let Some(engine) = tiles.map_engines.get(id) {
        if let Some(bbox) = engine.raster_info().spatial_extent {
            return Some(bbox);
        }
    }
    if let Some(engine) = features.engines.get(id) {
        if let Some(bbox) = engine.spatial_extent() {
            return Some(bbox);
        }
    }
    if let Some(engine) = tiles.feature_engines.get(id) {
        if let Some(bbox) = engine.spatial_extent() {
            return Some(bbox);
        }
    }
    // WMS-only collection — same `MapEngine` interface as Maps/Tiles.
    if let Some(engine) = wms.engines.get(id) {
        if let Some(bbox) = engine.raster_info().spatial_extent {
            return Some(bbox);
        }
    }
    None
}

fn resolve_temporal_extent(
    id: &str,
    edr: &api_edr::handlers::EdrState,
    maps: &api_maps::handlers::MapsState,
    tiles: &api_tiles::handlers::TilesState,
    wms: &api_wms::handlers::WmsState,
) -> Option<Value> {
    // EDR is the canonical temporal source — it carries both interval and
    // explicit instants. Maps/Tiles/WMS `raster_info().times` is the
    // fallback when a collection isn't EDR-enabled (e.g. radar collections
    // exposed only via WMS).
    if let Some(engine) = edr.engines.get(id) {
        let interval = engine.get_temporal_extent();
        let values = engine.get_available_times();
        if interval.is_some() || values.is_some() {
            return Some(serialize_temporal(interval, values.as_deref()));
        }
    }

    let times_from_raster = maps
        .engines
        .get(id)
        .map(|e| e.raster_info().times)
        .or_else(|| tiles.map_engines.get(id).map(|e| e.raster_info().times))
        .or_else(|| wms.engines.get(id).map(|e| e.raster_info().times));
    if let Some(times) = times_from_raster {
        if !times.is_empty() {
            let interval = times.first().zip(times.last()).map(|(a, b)| (*a, *b));
            return Some(serialize_temporal(interval, Some(&times)));
        }
    }
    None
}

fn serialize_temporal(
    interval: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
    values: Option<&[chrono::DateTime<chrono::Utc>]>,
) -> Value {
    let mut obj = serde_json::Map::new();
    if let Some((start, end)) = interval {
        obj.insert("start".into(), json!(start.to_rfc3339()));
        obj.insert("end".into(), json!(end.to_rfc3339()));
    }
    // Always emit `values` / `truncated` / `total_values`, even when the
    // engine doesn't override `get_available_times()`. This keeps the
    // manifest shape stable so UI clients don't have to null-guard each
    // field individually. The `Engine` trait's default impl returns
    // `None` for several engines (CSV, PostGIS, QueryData) — without
    // this fallback those collections would emit only `start`/`end`.
    let (slice, truncated, total): (&[_], bool, usize) = match values {
        Some(vs) => {
            let total = vs.len();
            if total > MAX_TEMPORAL_VALUES {
                (&vs[..MAX_TEMPORAL_VALUES], true, total)
            } else {
                (vs, false, total)
            }
        }
        None => (&[], false, 0),
    };
    let serialized: Vec<String> = slice.iter().map(|t| t.to_rfc3339()).collect();
    obj.insert("values".into(), json!(serialized));
    obj.insert("truncated".into(), json!(truncated));
    obj.insert("total_values".into(), json!(total));
    Value::Object(obj)
}

// Template placeholders match the axum route variables registered by
// api-tiles (`/collections/{id}/tiles/{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}`).
// Using `{tms}` and `{z}` here would yield a path that the server rejects
// with a 404 — even though the URL looks "tile-shaped" it doesn't match
// the registered route names.
fn vector_tile_descriptor(id: &str, base_url: &str) -> Value {
    json!({
        "tile_matrix_sets": ["WebMercatorQuad", "WorldCRS84Quad"],
        "url_template": format!(
            "{base_url}/tiles/collections/{id}/tiles/{{tileMatrixSetId}}/{{tileMatrix}}/{{tileRow}}/{{tileCol}}?f=mvt"
        ),
        "media_type": "application/vnd.mapbox-vector-tile"
    })
}

fn raster_tile_descriptor(
    id: &str,
    base_url: &str,
    styles: Option<&std::collections::HashMap<String, ds_render::StyleInfo>>,
) -> Value {
    let mut desc = json!({
        "tile_matrix_sets": ["WebMercatorQuad", "WorldCRS84Quad"],
        "url_template": format!(
            "{base_url}/tiles/collections/{id}/tiles/{{tileMatrixSetId}}/{{tileMatrix}}/{{tileRow}}/{{tileCol}}"
        ),
        "media_type": "image/png"
    });

    // Only advertise the styled-tile URL when the collection actually has
    // styles registered — otherwise a SPA following `styled_url_template`
    // with `default_style` would hit a 404. Default-style preference: a
    // style literally named "default", else the first style by sorted name.
    if let Some(styles) = styles {
        if let Some(default_style) = styles.get("default").map(|s| s.name.as_str()).or_else(|| {
            let mut names: Vec<&String> = styles.keys().collect();
            names.sort();
            names
                .first()
                .and_then(|n| styles.get(*n))
                .map(|s| s.name.as_str())
        }) {
            desc["default_style"] = json!(default_style);
            desc["styled_url_template"] = json!(format!(
                "{base_url}/tiles/collections/{id}/styles/{{styleId}}/tiles/{{tileMatrixSetId}}/{{tileMatrix}}/{{tileRow}}/{{tileCol}}"
            ));
        }
    }
    desc
}

fn style_list(styles: &std::collections::HashMap<String, ds_render::StyleInfo>) -> Vec<Value> {
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
    names
        .into_iter()
        .filter_map(|name| {
            styles.get(name).map(|s| {
                // `id` is the HashMap key (`name`), not `s.name`. The client
                // round-trips this back as `{styleId}` and the API resolves
                // it via `layer_styles.get(...)` — i.e. by key. These two
                // strings happen to be identical in every config today, but
                // using the iterated key here removes the implicit
                // "name field must equal map key" invariant: a future
                // refactor where they diverge would otherwise 404 silently.
                json!({
                    "id": name,
                    "title": s.title,
                })
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};

    use arc_swap::ArcSwap;
    use chrono::{DateTime, Utc};
    use ds_core::engine::Engine;
    use ds_core::error::DataServerError;
    use ds_core::feature::{Feature, FeaturePage, FeatureQuery};
    use ds_core::feature_engine::FeatureEngine;
    use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
    use ds_core::model::{Location, QueryResult};

    use crate::admin::ServerState;

    // ---- Mock engines (only the methods touched by the manifest builder) ----

    struct EdrMock {
        extent: Option<[f64; 4]>,
        times: Vec<DateTime<Utc>>,
    }

    impl Engine for EdrMock {
        fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
            Ok(Vec::new())
        }
        fn query_location(
            &self,
            _location_id: &str,
            _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
            _parameters: Option<&[String]>,
        ) -> Result<QueryResult, DataServerError> {
            unimplemented!()
        }
        fn get_parameters(&self) -> Vec<String> {
            Vec::new()
        }
        fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
            Some((*self.times.first()?, *self.times.last()?))
        }
        fn get_available_times(&self) -> Option<Vec<DateTime<Utc>>> {
            Some(self.times.clone())
        }
        fn get_spatial_extent(&self) -> Option<[f64; 4]> {
            self.extent
        }
    }

    /// Mock MapEngine that rebuilds RasterInfo on each call (it isn't `Clone`).
    struct RasterMock {
        spatial_extent: Option<[f64; 4]>,
        times: Vec<DateTime<Utc>>,
        parameter: String,
        unit: String,
    }

    impl MapEngine for RasterMock {
        fn get_raster_tile(
            &self,
            _bbox: [f64; 4],
            _w: u32,
            _h: u32,
            _t: Option<DateTime<Utc>>,
            _crs: &OutputCrs,
            _param: Option<&str>,
        ) -> Result<RasterTile, DataServerError> {
            unimplemented!()
        }
        fn raster_info(&self) -> RasterInfo {
            RasterInfo {
                native_crs: "EPSG:3857".into(),
                spatial_extent: self.spatial_extent,
                times: self.times.clone(),
                parameter: self.parameter.clone(),
                unit: self.unit.clone(),
                parameters: vec![],
            }
        }
    }

    struct PointFeatureMock {
        extent: Option<[f64; 4]>,
    }

    impl FeatureEngine for PointFeatureMock {
        fn get_features(&self, _query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
            unimplemented!()
        }
        fn get_feature(&self, _id: &str) -> Result<Feature, DataServerError> {
            unimplemented!()
        }
        fn feature_count(&self) -> usize {
            0
        }
        fn spatial_extent(&self) -> Option<[f64; 4]> {
            self.extent
        }
    }

    // ---- Empty/seed helpers ----

    fn empty_edr() -> api_edr::handlers::EdrState {
        api_edr::handlers::EdrState {
            engines: HashMap::new(),
            collections: HashMap::new(),
            base_url: String::new(),
        }
    }

    fn empty_features() -> api_features::handlers::FeaturesState {
        api_features::handlers::FeaturesState {
            engines: HashMap::new(),
            collections: HashMap::new(),
            base_url: String::new(),
        }
    }

    fn empty_wms() -> api_wms::handlers::WmsState {
        api_wms::handlers::WmsState {
            engines: HashMap::new(),
            collections: HashMap::new(),
            styles: HashMap::new(),
            render_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
            rendered_cache: Arc::new(ds_render::RenderedCache::new(1)),
            base_url: String::new(),
        }
    }

    fn empty_maps() -> api_maps::handlers::MapsState {
        api_maps::handlers::MapsState {
            engines: HashMap::new(),
            collections: HashMap::new(),
            styles: HashMap::new(),
            render_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
            rendered_cache: Arc::new(ds_render::RenderedCache::new(1)),
            base_url: String::new(),
        }
    }

    fn empty_tiles() -> api_tiles::TilesState {
        api_tiles::TilesState {
            map_engines: HashMap::new(),
            collections: HashMap::new(),
            styles: HashMap::new(),
            feature_engines: HashMap::new(),
            feature_collections: HashMap::new(),
            render_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
            rendered_cache: Arc::new(ds_render::RenderedCache::new(1)),
            vector_tile_cache: Arc::new(ds_mvt::VectorTileCache::new(1)),
            base_url: String::new(),
        }
    }

    fn make_state(
        edr: api_edr::handlers::EdrState,
        features: api_features::handlers::FeaturesState,
        maps: api_maps::handlers::MapsState,
        tiles: api_tiles::TilesState,
        wms: api_wms::handlers::WmsState,
    ) -> AdminState {
        Arc::new(ServerState {
            edr: Arc::new(ArcSwap::from_pointee(edr)),
            features: Arc::new(ArcSwap::from_pointee(features)),
            wms: Arc::new(ArcSwap::from_pointee(wms)),
            maps: Arc::new(ArcSwap::from_pointee(maps)),
            tiles: Arc::new(ArcSwap::from_pointee(tiles)),
            config_path: String::new(),
            health: RwLock::new(Vec::new()),
            geotiff_engines: RwLock::new(Vec::new()),
            querydata_engines: RwLock::new(Vec::new()),
            grib_engines: RwLock::new(Vec::new()),
            postgis_engines: RwLock::new(Vec::new()),
            reload_lock: tokio::sync::Mutex::new(()),
            admin_token: None,
        })
    }

    fn config(id: &str, apis: &[&str]) -> CollectionConfig {
        CollectionConfig {
            id: id.into(),
            title: format!("{id} title"),
            description: format!("{id} description"),
            data_path: None,
            apis: apis.iter().map(|s| s.to_string()).collect(),
            engine_type: "mock".into(),
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            postgis: None,
        }
    }

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    // ---- Tests ----

    #[test]
    fn manifest_is_empty_when_no_collections_registered() {
        let state = make_state(
            empty_edr(),
            empty_features(),
            empty_maps(),
            empty_tiles(),
            empty_wms(),
        );
        let m = build_manifest(&state, 0, 100);
        assert_eq!(m["collections"].as_array().unwrap().len(), 0);
        assert_eq!(m["pagination"]["total"], 0);
        assert!(m["pagination"]["next"].is_null());
    }

    #[test]
    fn manifest_aggregates_edr_collection_with_temporal_extent() {
        let mut edr = empty_edr();
        let engine: Arc<dyn Engine> = Arc::new(EdrMock {
            extent: Some([10.0, 55.0, 30.0, 70.0]),
            times: vec![t("2024-01-01T00:00:00Z"), t("2024-01-02T00:00:00Z")],
        });
        edr.engines.insert("weather".into(), engine);
        edr.collections
            .insert("weather".into(), config("weather", &["edr"]));

        let state = make_state(
            edr,
            empty_features(),
            empty_maps(),
            empty_tiles(),
            empty_wms(),
        );
        let m = build_manifest(&state, 0, 100);

        let collections = m["collections"].as_array().unwrap();
        assert_eq!(collections.len(), 1);
        let c = &collections[0];
        assert_eq!(c["id"], "weather");
        assert_eq!(c["apis"], serde_json::json!(["edr"]));
        assert_eq!(
            c["spatial_extent"],
            serde_json::json!([10.0, 55.0, 30.0, 70.0])
        );
        let temporal = &c["temporal_extent"];
        assert_eq!(temporal["start"], "2024-01-01T00:00:00+00:00");
        assert_eq!(temporal["end"], "2024-01-02T00:00:00+00:00");
        assert_eq!(temporal["values"].as_array().unwrap().len(), 2);
        assert_eq!(temporal["truncated"], false);
        assert_eq!(temporal["total_values"], 2);
    }

    #[test]
    fn manifest_truncates_temporal_values_at_cap() {
        let mut edr = empty_edr();
        let times: Vec<DateTime<Utc>> = (0..MAX_TEMPORAL_VALUES + 50)
            .map(|i| DateTime::<Utc>::from_timestamp(1_700_000_000 + i as i64 * 60, 0).unwrap())
            .collect();
        let engine: Arc<dyn Engine> = Arc::new(EdrMock {
            extent: None,
            times: times.clone(),
        });
        edr.engines.insert("obs".into(), engine);
        edr.collections
            .insert("obs".into(), config("obs", &["edr"]));

        let state = make_state(
            edr,
            empty_features(),
            empty_maps(),
            empty_tiles(),
            empty_wms(),
        );
        let m = build_manifest(&state, 0, 100);

        let temporal = &m["collections"][0]["temporal_extent"];
        assert_eq!(
            temporal["values"].as_array().unwrap().len(),
            MAX_TEMPORAL_VALUES
        );
        assert_eq!(temporal["truncated"], true);
        assert_eq!(temporal["total_values"], times.len());
    }

    #[test]
    fn manifest_emits_vector_and_raster_tile_descriptors_when_wired() {
        let mut tiles = empty_tiles();
        // Seed a realistic base_url so the assertions can verify the full
        // `https://…` prefix. An empty base would silently absorb a future
        // regression that drops the base from the URL template.
        tiles.base_url = "https://api.example.com".into();
        let raster: Arc<dyn MapEngine> = Arc::new(RasterMock {
            spatial_extent: Some([-180.0, -85.0, 180.0, 85.0]),
            times: vec![],
            parameter: "reflectivity".into(),
            unit: "dBZ".into(),
        });
        let feature_engine: Arc<dyn FeatureEngine> = Arc::new(PointFeatureMock {
            extent: Some([20.0, 60.0, 30.0, 70.0]),
        });
        tiles.map_engines.insert("radar".into(), raster);
        tiles
            .collections
            .insert("radar".into(), config("radar", &["tiles"]));
        tiles
            .feature_engines
            .insert("stations".into(), feature_engine);
        tiles.feature_collections.insert(
            "stations".into(),
            config("stations", &["features", "tiles"]),
        );

        let state = make_state(
            empty_edr(),
            empty_features(),
            empty_maps(),
            tiles,
            empty_wms(),
        );
        let m = build_manifest(&state, 0, 100);

        let by_id: HashMap<&str, &Value> = m["collections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["id"].as_str().unwrap(), c))
            .collect();

        // Raster-only collection: tiles.raster present, tiles.vector absent.
        // URL template must carry the absolute base + the *route's* literal
        // path variable names — `{tileMatrixSetId}`/`{tileMatrix}`, not the
        // generic `{tms}`/`{z}` that don't match the axum route.
        let radar = by_id["radar"];
        let raster_url = radar["tiles"]["raster"]["url_template"].as_str().unwrap();
        assert!(
            raster_url.starts_with("https://api.example.com/tiles/collections/radar/tiles/"),
            "raster url_template must carry the absolute base, got: {raster_url}"
        );
        assert!(
            raster_url.contains("{tileMatrixSetId}") && raster_url.contains("{tileMatrix}"),
            "raster url_template must use the axum route's placeholder names, got: {raster_url}"
        );
        // `radar` has no styles registered in this fixture, so the
        // styled-tile fields must be absent — otherwise we'd advertise a
        // URL the server can't honour. Locks in the conditional emit in
        // `raster_tile_descriptor`.
        assert!(radar["tiles"]["raster"]
            .get("styled_url_template")
            .is_none());
        assert!(radar["tiles"]["raster"].get("default_style").is_none());
        assert!(radar["tiles"].get("vector").is_none());

        // Vector-only collection: tiles.vector present with ?f=mvt; tiles.raster absent.
        let stations = by_id["stations"];
        let vector_url = stations["tiles"]["vector"]["url_template"]
            .as_str()
            .unwrap();
        assert!(
            vector_url.starts_with("https://api.example.com/tiles/collections/stations/tiles/"),
            "vector url_template must carry the absolute base, got: {vector_url}"
        );
        assert!(
            vector_url.contains("{tileMatrixSetId}") && vector_url.contains("{tileMatrix}"),
            "vector url_template must use the axum route's placeholder names, got: {vector_url}"
        );
        assert!(vector_url.contains("f=mvt"));
        assert!(stations["tiles"].get("raster").is_none());
    }

    #[test]
    fn pagination_skips_offset_and_caps_at_limit() {
        let mut edr = empty_edr();
        for id in ["alpha", "beta", "gamma", "delta", "epsilon"] {
            edr.collections.insert(id.into(), config(id, &["edr"]));
        }

        let state = make_state(
            edr,
            empty_features(),
            empty_maps(),
            empty_tiles(),
            empty_wms(),
        );
        let m = build_manifest(&state, 1, 2);

        assert_eq!(m["pagination"]["total"], 5);
        assert_eq!(m["pagination"]["offset"], 1);
        assert_eq!(m["pagination"]["limit"], 2);
        assert_eq!(m["pagination"]["returned"], 2);
        assert_eq!(m["pagination"]["next"], 3);

        // Ids appear in sorted order — alpha, beta, delta, epsilon, gamma —
        // so offset=1 skips alpha and returns [beta, delta].
        let ids: Vec<&str> = m["collections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["beta", "delta"]);
    }

    #[test]
    fn pagination_next_is_null_on_last_page() {
        let mut edr = empty_edr();
        edr.collections
            .insert("only".into(), config("only", &["edr"]));

        let state = make_state(
            edr,
            empty_features(),
            empty_maps(),
            empty_tiles(),
            empty_wms(),
        );
        let m = build_manifest(&state, 0, 100);
        assert!(m["pagination"]["next"].is_null());
    }

    #[test]
    fn collection_in_multiple_apis_appears_once() {
        let mut edr = empty_edr();
        let mut features = empty_features();
        edr.collections
            .insert("dual".into(), config("dual", &["edr", "features"]));
        features
            .collections
            .insert("dual".into(), config("dual", &["edr", "features"]));

        let state = make_state(edr, features, empty_maps(), empty_tiles(), empty_wms());
        let m = build_manifest(&state, 0, 100);
        assert_eq!(m["collections"].as_array().unwrap().len(), 1);
        assert_eq!(
            m["collections"][0]["apis"],
            serde_json::json!(["edr", "features"])
        );
    }

    #[test]
    fn temporal_extent_shape_is_stable_when_engine_omits_available_times() {
        // Regression: engines that implement `get_temporal_extent()` but
        // leave `get_available_times()` at its default `None` (CSV,
        // PostGIS, QueryData) used to produce `{"start","end"}` only,
        // breaking UI clients that read `.values`/`.truncated`/
        // `.total_values` without a null-guard. Manifest now always emits
        // those three fields with empty defaults.
        struct IntervalOnlyMock {
            interval: (DateTime<Utc>, DateTime<Utc>),
        }
        impl Engine for IntervalOnlyMock {
            fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
                Ok(Vec::new())
            }
            fn query_location(
                &self,
                _: &str,
                _: Option<(DateTime<Utc>, DateTime<Utc>)>,
                _: Option<&[String]>,
            ) -> Result<QueryResult, DataServerError> {
                unimplemented!()
            }
            fn get_parameters(&self) -> Vec<String> {
                Vec::new()
            }
            fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
                Some(self.interval)
            }
            fn get_spatial_extent(&self) -> Option<[f64; 4]> {
                None
            }
            // Note: no `get_available_times` override — falls back to default `None`.
        }
        let mut edr = empty_edr();
        let engine: Arc<dyn Engine> = Arc::new(IntervalOnlyMock {
            interval: (
                "2024-01-01T00:00:00Z".parse().unwrap(),
                "2024-01-02T00:00:00Z".parse().unwrap(),
            ),
        });
        edr.engines.insert("obs".into(), engine);
        edr.collections
            .insert("obs".into(), config("obs", &["edr"]));
        let state = make_state(
            edr,
            empty_features(),
            empty_maps(),
            empty_tiles(),
            empty_wms(),
        );
        let m = build_manifest(&state, 0, 100);
        let temporal = &m["collections"][0]["temporal_extent"];
        assert_eq!(temporal["start"], "2024-01-01T00:00:00+00:00");
        assert_eq!(temporal["end"], "2024-01-02T00:00:00+00:00");
        assert_eq!(
            temporal["values"],
            serde_json::json!([]),
            "values key must be present (as empty array) even when get_available_times() is None"
        );
        assert_eq!(temporal["truncated"], false);
        assert_eq!(temporal["total_values"], 0);
    }

    #[test]
    fn wms_only_collection_surfaces_extent_and_styles() {
        // Regression guard for the discovery gap flagged in review: a
        // collection with `apis = ["wms"]` only (no EDR/Maps/Tiles
        // surface) must still get its extent + temporal + styles fields
        // populated from the WMS state.
        let mut wms = empty_wms();
        let times = vec![
            "2024-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap(),
            "2024-01-01T01:00:00Z".parse::<DateTime<Utc>>().unwrap(),
        ];
        let engine: Arc<dyn MapEngine> = Arc::new(RasterMock {
            spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
            times: times.clone(),
            parameter: "reflectivity".into(),
            unit: "dBZ".into(),
        });
        wms.engines.insert("radar-wms".into(), engine);
        wms.collections
            .insert("radar-wms".into(), config("radar-wms", &["wms"]));
        let mut styles = HashMap::new();
        styles.insert(
            "default".to_string(),
            ds_render::StyleInfo {
                name: "default".to_string(),
                title: "Default".to_string(),
                colormap: Arc::new(ds_render::LutColorMap::from_builtin(
                    ds_render::BuiltinColormap::Viridis,
                    0.0,
                    1.0,
                )),
                min: 0.0,
                max: 1.0,
                parameter: None,
            },
        );
        wms.styles.insert("radar-wms".into(), styles);

        let state = make_state(
            empty_edr(),
            empty_features(),
            empty_maps(),
            empty_tiles(),
            wms,
        );
        let m = build_manifest(&state, 0, 100);
        let c = &m["collections"][0];
        assert_eq!(c["id"], "radar-wms");
        assert_eq!(c["apis"], serde_json::json!(["wms"]));
        assert_eq!(
            c["spatial_extent"],
            serde_json::json!([10.0, 55.0, 30.0, 70.0])
        );
        let temporal = &c["temporal_extent"];
        assert!(
            temporal["start"].is_string(),
            "WMS-only collection should expose temporal start, got: {temporal}"
        );
        assert_eq!(temporal["total_values"], 2);
        let styles_array = c["styles"].as_array().expect("styles must be present");
        assert_eq!(styles_array.len(), 1);
        assert_eq!(styles_array[0]["id"], "default");
        // WMS-only collections have no `tiles` block (no MVT or raster-tile
        // route exists). Worth asserting because a future refactor might
        // accidentally synthesise one.
        assert!(c.get("tiles").is_none());
    }

    // -----------------------------------------------------------------------
    // Static-asset handler tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn index_handler_returns_html() {
        let resp = index_handler(axum::http::HeaderMap::new()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(body_str.contains("MeteoCore"));
        assert!(body_str.contains("maplibre-gl.js"));
    }

    #[tokio::test]
    async fn index_html_uses_no_cache_and_carries_csp() {
        // Stable URL → must revalidate every load so a binary upgrade ships
        // the new shell immediately. CSP is the SPA's only browser-side
        // safety net (defence in depth even though `app.js` only writes via
        // `textContent`).
        let resp = index_handler(axum::http::HeaderMap::new()).await;
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-cache"
        );
        let csp = resp
            .headers()
            .get("content-security-policy")
            .expect("CSP must be set on index.html")
            .to_str()
            .unwrap();
        assert!(csp.contains("default-src 'self'"));
        assert!(
            csp.contains("worker-src blob:"),
            "MapLibre needs worker-src blob:"
        );
        assert!(csp.contains("frame-ancestors 'none'"));
        assert_eq!(
            resp.headers().get("x-content-type-options").unwrap(),
            "nosniff"
        );
    }

    #[tokio::test]
    async fn vendored_maplibre_js_is_cacheable_but_not_immutable() {
        let resp = asset_handler(
            axum::extract::Path("vendor/maplibre-gl.js".into()),
            axum::http::HeaderMap::new(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let (parts, body) = resp.into_parts();
        assert_eq!(
            parts.headers.get(header::CONTENT_TYPE).unwrap(),
            "application/javascript; charset=utf-8"
        );
        // Long `max-age` for cheap reuse, but **not** `immutable` —
        // `/preview/vendor/maplibre-gl.js` is a stable URL (no version
        // segment), so `immutable` would pin clients to the old bundle
        // after a vendor bump.
        assert_eq!(
            parts.headers.get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=86400"
        );
        // CSP is NOT applied to non-HTML assets — that header only matters
        // when the browser parses the response as a document.
        assert!(parts.headers.get("content-security-policy").is_none());
        // Sanity: the MapLibre UMD bundle is hundreds of KB; serving an
        // empty stub would mean the vendor script didn't run.
        let bytes = axum::body::to_bytes(body, 16 * 1024 * 1024).await.unwrap();
        assert!(
            bytes.len() > 100_000,
            "maplibre-gl.js should be hundreds of KB; got {} bytes",
            bytes.len()
        );
    }

    #[tokio::test]
    async fn app_js_uses_short_must_revalidate_cache() {
        // Non-fingerprinted JS bundled with the binary: short cache, must
        // revalidate. `immutable` would pin clients across deploys.
        let resp = asset_handler(
            axum::extract::Path("app.js".into()),
            axum::http::HeaderMap::new(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=300, must-revalidate"
        );
    }

    #[tokio::test]
    async fn app_js_rewrites_ogc_tile_placeholders_for_maplibre() {
        // Regression guard for #134 review: `tileUrlFor()` must rewrite
        // *every* OGC API Tiles placeholder name emitted by the manifest
        // (see `tile_descriptors`) into the `{z}/{x}/{y}` form MapLibre
        // raster sources substitute. Missing any one of the four turns
        // every tile request into a 404 because the path is left with
        // literal `{tile…}` segments that no route matches.
        let resp = asset_handler(
            axum::extract::Path("app.js".into()),
            axum::http::HeaderMap::new(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body_str = std::str::from_utf8(&body).unwrap();
        for needle in [
            "'{tileMatrixSetId}', 'WebMercatorQuad'",
            "'{tileMatrix}', '{z}'",
            "'{tileRow}', '{y}'",
            "'{tileCol}', '{x}'",
        ] {
            assert!(
                body_str.contains(needle),
                "app.js missing tile placeholder substitution: {needle}"
            );
        }
        // Negative guard: the legacy `{tms}` and `{z}` shortcuts must NOT
        // reappear in the substitution targets — the manifest no longer
        // uses them, so any code referencing them is a stale-doc bug.
        assert!(
            !body_str.contains("'{tms}'"),
            "app.js still references legacy `{{tms}}` placeholder (manifest emits `{{tileMatrixSetId}}`)"
        );
    }

    #[tokio::test]
    async fn app_js_does_not_pass_collection_title_as_maplibre_attribution() {
        // Regression guard for #134 review: MapLibre's AttributionControl
        // injects the `attribution` field of raster sources via `innerHTML`.
        // Passing a server-controlled title — e.g.
        // `title = "<img src=x onerror=alert(1)>"` from a malicious
        // collection config — would execute script in the preview page.
        // The title already appears in the sidebar (escaped via
        // `textContent`), so the source attribution is redundant.
        let resp = asset_handler(
            axum::extract::Path("app.js".into()),
            axum::http::HeaderMap::new(),
        )
        .await;
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(
            !body_str.contains("attribution:"),
            "app.js sets a raster-source `attribution:` — MapLibre renders \
             that via innerHTML and would execute script from a malicious \
             collection title. Drop the field or HTML-escape the value."
        );
    }

    #[tokio::test]
    async fn unknown_asset_returns_generic_404_body() {
        // User-supplied path must NOT be reflected (review feedback Phase 2).
        let resp = asset_handler(
            axum::extract::Path("does-not-exist.txt".into()),
            axum::http::HeaderMap::new(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let body_str = std::str::from_utf8(&body).unwrap();
        assert_eq!(body_str, "Not Found");
        assert!(
            !body_str.contains("does-not-exist"),
            "404 body must not echo the requested path"
        );
    }

    #[tokio::test]
    async fn matching_if_none_match_yields_304_with_empty_body() {
        // First request — learn the ETag.
        let first = index_handler(axum::http::HeaderMap::new()).await;
        let etag = first
            .headers()
            .get(header::ETAG)
            .expect("ETag must be set on 200 responses")
            .clone();
        // Replay with `If-None-Match`. The handler must short-circuit to
        // 304 and return no body — saves the full HTML payload on every
        // reload of the `no-cache` shell.
        let mut req_headers = axum::http::HeaderMap::new();
        req_headers.insert(header::IF_NONE_MATCH, etag.clone());
        let second = index_handler(req_headers).await;
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(second.headers().get(header::ETAG).unwrap(), &etag);
        // 304 must still carry Cache-Control so the browser refreshes its
        // freshness clock, and nosniff for parity with the 200 path so a
        // future refactor can't accidentally drop the header from one
        // branch only.
        assert_eq!(
            second.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-cache"
        );
        assert_eq!(
            second.headers().get("x-content-type-options").unwrap(),
            "nosniff"
        );
        let body = axum::body::to_bytes(second.into_body(), 1024)
            .await
            .unwrap();
        assert!(
            body.is_empty(),
            "304 must have an empty body, got {} bytes",
            body.len()
        );
    }

    #[tokio::test]
    async fn manifest_handler_emits_no_store_cache_control() {
        use axum::extract::{Query, State};
        // `manifest_handler` returns live `ArcSwap` state; caches must not
        // hold it across an `/admin/collections/reload`.
        let state = make_state(
            empty_edr(),
            empty_features(),
            empty_maps(),
            empty_tiles(),
            empty_wms(),
        );
        let resp = manifest_handler(State(state), Query(ManifestParams::default()))
            .await
            .into_response();
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        assert_eq!(
            resp.headers().get("x-content-type-options").unwrap(),
            "nosniff",
            "manifest_handler must mirror nosniff applied by serve_asset"
        );
    }

    #[tokio::test]
    async fn mismatched_if_none_match_returns_full_body() {
        // If the client's cached ETag doesn't match, the server must serve
        // the full body — otherwise a client with a stale ETag would be
        // stuck with stale content.
        let mut req_headers = axum::http::HeaderMap::new();
        req_headers.insert(
            header::IF_NONE_MATCH,
            axum::http::HeaderValue::from_static("\"deadbeef\""),
        );
        let resp = index_handler(req_headers).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 16 * 1024)
            .await
            .unwrap();
        assert!(
            !body.is_empty(),
            "stale-ETag client must get full body back"
        );
    }

    #[test]
    fn mime_inference_covers_common_extensions() {
        assert_eq!(mime_for("a/b/index.html"), "text/html; charset=utf-8");
        assert_eq!(mime_for("app.js"), "application/javascript; charset=utf-8");
        assert_eq!(mime_for("app.css"), "text/css; charset=utf-8");
        assert_eq!(mime_for("data.json"), "application/json");
        assert_eq!(mime_for("icon.svg"), "image/svg+xml");
        assert_eq!(mime_for("README"), "application/octet-stream");
    }
}
