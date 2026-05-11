//! Built-in preview UI surface.
//!
//! v1 exposes a single endpoint, `GET /preview/manifest.json`, that aggregates
//! every collection's discovery metadata into one denormalized JSON shape. The
//! preview SPA consumes it as its single source of truth so the browser
//! doesn't have to probe five separate `/collections` endpoints (EDR, Features,
//! Maps, Tiles, WMS) per page load and reconcile their drift.
//!
//! Asset embedding (`/preview` + `/preview/{*path}`) ships in Phase 2.

use std::collections::BTreeSet;

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{json, Value};

use ds_core::config::CollectionConfig;

use crate::admin::AdminState;

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
    let body = serde_json::to_vec(&manifest).unwrap_or_else(|_| b"{}".to_vec());
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

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
}

/// Build a denormalized inventory across every per-API `*State`.
///
/// Pure: takes `&AdminState`, returns JSON. Used by the handler and by tests.
pub fn build_manifest(state: &AdminState, offset: usize, limit: usize) -> Value {
    let edr = state.edr.load_full();
    let features = state.features.load_full();
    let maps = state.maps.load_full();
    let tiles = state.tiles.load_full();
    let wms = state.wms.load_full();

    let base_url = edr.base_url.clone();

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

    if let Some(extent) = resolve_spatial_extent(id, edr, features, maps, tiles) {
        entry["spatial_extent"] = json!(extent);
    }

    if let Some(temporal) = resolve_temporal_extent(id, edr, maps, tiles) {
        entry["temporal_extent"] = temporal;
    }

    // Tile representations — emit only what's actually wired up so the UI
    // doesn't render a layer toggle for a dead endpoint.
    let mut tile_block = serde_json::Map::new();
    if tiles.feature_engines.contains_key(id) {
        tile_block.insert("vector".into(), vector_tile_descriptor(id, base_url));
    }
    if tiles.map_engines.contains_key(id) {
        tile_block.insert(
            "raster".into(),
            raster_tile_descriptor(id, base_url, tiles.styles.get(id)),
        );
    }
    if !tile_block.is_empty() {
        entry["tiles"] = Value::Object(tile_block);
    }

    if let Some(styles) = maps.styles.get(id) {
        entry["styles"] = json!(style_list(styles));
    } else if let Some(styles) = tiles.styles.get(id) {
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

fn resolve_spatial_extent(
    id: &str,
    edr: &api_edr::handlers::EdrState,
    features: &api_features::handlers::FeaturesState,
    maps: &api_maps::handlers::MapsState,
    tiles: &api_tiles::handlers::TilesState,
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
    None
}

fn resolve_temporal_extent(
    id: &str,
    edr: &api_edr::handlers::EdrState,
    maps: &api_maps::handlers::MapsState,
    tiles: &api_tiles::handlers::TilesState,
) -> Option<Value> {
    // EDR is the canonical temporal source — it carries both interval and
    // explicit instants. Maps/Tiles raster_info().times is the fallback when
    // a collection isn't EDR-enabled (e.g. radar collections without an EDR
    // surface).
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
        .or_else(|| tiles.map_engines.get(id).map(|e| e.raster_info().times));
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
    if let Some(values) = values {
        let total = values.len();
        let (slice, truncated) = if total > MAX_TEMPORAL_VALUES {
            (&values[..MAX_TEMPORAL_VALUES], true)
        } else {
            (values, false)
        };
        let serialized: Vec<String> = slice.iter().map(|t| t.to_rfc3339()).collect();
        obj.insert("values".into(), json!(serialized));
        obj.insert("truncated".into(), json!(truncated));
        obj.insert("total_values".into(), json!(total));
    }
    Value::Object(obj)
}

fn vector_tile_descriptor(id: &str, base_url: &str) -> Value {
    json!({
        "tile_matrix_sets": ["WebMercatorQuad", "WorldCRS84Quad"],
        "url_template": format!(
            "{base_url}/tiles/collections/{id}/tiles/{{tms}}/{{z}}/{{tileRow}}/{{tileCol}}?f=mvt"
        ),
        "media_type": "application/vnd.mapbox-vector-tile"
    })
}

fn raster_tile_descriptor(
    id: &str,
    base_url: &str,
    styles: Option<&std::collections::HashMap<String, ds_render::StyleInfo>>,
) -> Value {
    let default_style = styles
        .and_then(|s| s.get("default"))
        .map(|s| s.name.as_str())
        .unwrap_or("default");
    json!({
        "tile_matrix_sets": ["WebMercatorQuad", "WorldCRS84Quad"],
        "url_template": format!(
            "{base_url}/tiles/collections/{id}/tiles/{{tms}}/{{z}}/{{tileRow}}/{{tileCol}}"
        ),
        "styled_url_template": format!(
            "{base_url}/tiles/collections/{id}/styles/{{styleId}}/tiles/{{tms}}/{{z}}/{{tileRow}}/{{tileCol}}"
        ),
        "default_style": default_style,
        "media_type": "image/png"
    })
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
                json!({
                    "id": s.name,
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
        let radar = by_id["radar"];
        assert!(radar["tiles"]["raster"]["url_template"]
            .as_str()
            .unwrap()
            .contains("/tiles/collections/radar/tiles/"));
        assert!(radar["tiles"].get("vector").is_none());

        // Vector-only collection: tiles.vector present with ?f=mvt; tiles.raster absent.
        let stations = by_id["stations"];
        let vector_url = stations["tiles"]["vector"]["url_template"]
            .as_str()
            .unwrap();
        assert!(vector_url.contains("/tiles/collections/stations/tiles/"));
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
}
