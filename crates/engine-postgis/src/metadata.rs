//! Per-collection metadata cache.
//!
//! Holds the station list, advertised parameters, temporal extent, and
//! spatial bbox behind an [`ArcSwap`]. Reads are lock-free via
//! `load_full()` and never acquire a pool connection — EDR metadata
//! endpoints (`/collections`, `/collections/{id}`) therefore scale
//! independently of DB health.
//!
//! Bootstrap: [`MetadataCache::refresh`] runs the location query (or queries)
//! and one temporal-extent query against the pool, then atomically swaps the
//! result in. It's called once at engine construction and then every
//! `metadata_refresh_secs` by `PostgisEngine::poll_loop` on the background
//! poll runtime — so the location list / extents / windowed "reporting" set
//! stay current without a manual reload.

use std::sync::Arc;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use deadpool_postgres::Pool;
use ds_core::feature::{FilterableProperties, PropertyValue};
use ds_core::model::{Location, LocationSnapshot, ParameterDescription};
use std::collections::{BTreeSet, HashMap, HashSet};
use thiserror::Error;
use tokio_postgres::types::Type;
use tokio_postgres::Row;

use crate::config::{PostgisEngineConfig, ValidatedParameter};
use crate::query::{build_locations, build_locations_from_observations, SqlParam};
use crate::schema::{LocationSource, ObservationSchema};

#[derive(Debug, Error)]
pub enum MetadataError {
    #[error("pool error: {0}")]
    Pool(String),
    #[error("database error: {0}")]
    Db(String),
    #[error("row decode error: {0}")]
    Decode(String),
}

/// A station row enriched with its mapped `property_cols` values already
/// coerced to [`PropertyValue`]. Drives the `FeatureEngine` impl so
/// feature lookups never touch the pool.
#[derive(Debug, Clone)]
pub struct FeatureStation {
    pub id: String,
    pub label: String,
    pub lat: f64,
    pub lon: f64,
    pub properties: Arc<HashMap<String, PropertyValue>>,
}

impl FeatureStation {
    pub fn to_location(&self) -> Location {
        Location {
            id: self.id.clone(),
            label: self.label.clone(),
            latitude: self.lat,
            longitude: self.lon,
        }
    }
}

/// Which read `[postgis] max_locations` cut (#1061).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationsCut {
    /// The location list itself: stations past the cap are not served.
    Stations,
    /// Mode B only: the stations-table registry, while every reporter fit.
    /// Reporters whose registry row was past the cap are served bare,
    /// without their label and properties.
    Registry,
}

impl LocationsCut {
    /// The operator-facing `/health` reason and log message. Names the
    /// configured cap, never a host or a database error.
    pub fn reason(self, max_locations: usize) -> String {
        match self {
            Self::Stations => format!(
                "station list truncated at max_locations = {max_locations}: stations past it \
                 are not served; raise [postgis] max_locations or narrow the station set"
            ),
            Self::Registry => format!(
                "station registry truncated at max_locations = {max_locations}: reporting \
                 stations whose registry row is past it are served without their label and \
                 properties; raise [postgis] max_locations or narrow the stations table"
            ),
        }
    }
}

/// The location list one refresh read, and whether `[postgis]
/// max_locations` cut it (#1061).
#[derive(Debug, Clone, Default)]
pub struct LocationSet {
    pub stations: Vec<FeatureStation>,
    /// `Some` when the cap cut a read: only the first `max_locations` rows
    /// are kept, in query order (stations table / per-table `ORDER BY id`).
    pub cut: Option<LocationsCut>,
}

impl LocationSet {
    /// A list read in full.
    pub fn complete(stations: Vec<FeatureStation>) -> Self {
        Self {
            stations,
            cut: None,
        }
    }

    /// The rows of a location query that fetched one past the cap
    /// ([`crate::query::location_fetch_rows`]): the first `cap` are kept in
    /// query order, and a row past them marks the list cut. Exactly `cap`
    /// rows is a complete list.
    pub fn first_of(mut stations: Vec<FeatureStation>, cap: usize) -> Self {
        let cut = (stations.len() > cap).then_some(LocationsCut::Stations);
        stations.truncate(cap);
        Self { stations, cut }
    }

    /// The decoded stations-table rows of one refresh, capped at the
    /// collection's `max_locations`: what `fetch_station_rows` returns.
    pub(crate) fn of_station_rows(cfg: &PostgisEngineConfig, rows: Vec<FeatureStation>) -> Self {
        Self::first_of(rows, cfg.max_locations)
    }
}

/// Snapshot of everything EDR + Features metadata endpoints need to
/// answer without hitting the pool. Cheap to clone (Arcs inside).
#[derive(Debug, Clone)]
pub struct CollectionMeta {
    /// Full station rows with property values. `locations` is derived
    /// from this list and kept in the same order, so `station_idx`
    /// is a valid index into both.
    pub feature_stations: Arc<Vec<FeatureStation>>,
    /// Features property-filter names: the configured
    /// `stations.property_cols` plus the names the cached rows carry
    /// (`station_filterables`, #1024).
    pub filterables: FilterableProperties,
    /// What `get_locations` hands every request, shared (#1083).
    pub locations: Arc<LocationSnapshot>,
    /// `id → index into feature_stations/locations` lookup built once
    /// per refresh. Constant-time station resolution for
    /// `query_location`, `query_area` (per-station), and
    /// `FeatureEngine::get_feature` — at 30 k stations the previous
    /// `iter().find()` was 12 M string compares per area request.
    pub station_idx: Arc<HashMap<String, usize>>,
    pub parameters: Arc<HashMap<String, ParameterDescription>>,
    pub temporal_extent: Option<(DateTime<Utc>, DateTime<Utc>)>,
    pub spatial_extent: Option<[f64; 4]>,
    /// Events shape only (#504): the WMS/Maps/Tiles TIME dimension —
    /// quantized steps ascending, rebuilt each metadata refresh so
    /// `raster_info()` stays O(1). Empty for station shapes.
    pub wms_times: Arc<Vec<DateTime<Utc>>>,
    /// `Some` when `[postgis] max_locations` cut the refresh's reads
    /// (#1061): stations past it are missing from `/locations`, Features
    /// and, in the observations-derived modes, from position and area
    /// queries, or, for a cut mode-B registry, are served without their
    /// metadata. Degrades `/health` and sets `postgis_locations_truncated`.
    pub locations_cut: Option<LocationsCut>,
    /// Monotonic version counter — bumped by each successful refresh.
    pub version: u64,
}

impl CollectionMeta {
    /// The snapshot before the first successful refresh: no stations yet,
    /// but the configured property columns are already filterable (#1024).
    fn empty(cfg: &PostgisEngineConfig) -> Self {
        Self {
            feature_stations: Arc::new(Vec::new()),
            filterables: station_filterables(cfg, &[]),
            locations: LocationSnapshot::empty(),
            station_idx: Arc::new(HashMap::new()),
            parameters: Arc::new(HashMap::new()),
            temporal_extent: None,
            spatial_extent: None,
            wms_times: Arc::new(Vec::new()),
            locations_cut: None,
            version: 0,
        }
    }

    /// Assemble a refreshed snapshot from what [`MetadataCache::refresh`]
    /// fetched. Pure, so the row-to-snapshot rules are testable without a
    /// database.
    pub(crate) fn build(
        cfg: &PostgisEngineConfig,
        locations: LocationSet,
        temporal_extent: Option<(DateTime<Utc>, DateTime<Utc>)>,
        version: u64,
    ) -> Self {
        let LocationSet {
            stations: feature_stations,
            cut: locations_cut,
        } = locations;
        let locations: Vec<Location> = feature_stations
            .iter()
            .map(FeatureStation::to_location)
            .collect();
        let station_idx = build_station_idx(&feature_stations);
        let parameters = build_parameter_descriptions(&cfg.parameters);
        // Events shape: the location list is empty, so the spatial extent
        // comes from config (`extent_bbox`) — never from an `ST_Extent`
        // full scan over the event table.
        let spatial_extent = match cfg.events_extent_bbox {
            Some(bbox) => Some(bbox),
            None => spatial_extent_from(&locations),
        };
        // Events shape (#504): the advertised WMS/Maps/Tiles TIME steps,
        // quantized and bounded, rebuilt per refresh so raster_info() is a
        // snapshot clone.
        let wms_times = if matches!(cfg.observations, ObservationSchema::Events(_)) {
            crate::maps::build_wms_times(temporal_extent)
        } else {
            Vec::new()
        };
        Self {
            filterables: station_filterables(cfg, &feature_stations),
            feature_stations: Arc::new(feature_stations),
            locations: Arc::new(LocationSnapshot::new(locations)),
            station_idx: Arc::new(station_idx),
            parameters: Arc::new(parameters),
            temporal_extent,
            spatial_extent,
            wms_times: Arc::new(wms_times),
            locations_cut,
            version,
        }
    }
}

/// The Features property-filter names of a station collection (#1024): the
/// configured `stations.property_cols`, then any further names the cached
/// rows carry. The configured columns are known before any row is read, so an
/// empty stations table, or a first refresh that failed because the database
/// was unreachable, still accepts a filter on one of them: 200 with zero
/// features, not a 400 that lasts until a row arrives. Every stations row
/// carries every property column (`fetch_station_rows`), so the union adds
/// nothing once data is loaded. The observations-only mode has no stations
/// table and the events shape no stations at all; both configure none.
fn station_filterables(
    cfg: &PostgisEngineConfig,
    stations: &[FeatureStation],
) -> FilterableProperties {
    let configured = cfg
        .location_source
        .stations()
        .map(|s| s.property_cols.as_slice())
        .unwrap_or_default();
    let names: BTreeSet<&str> = configured
        .iter()
        .map(String::as_str)
        .chain(
            stations
                .iter()
                .flat_map(|s| s.properties.keys().map(String::as_str)),
        )
        .collect();
    Arc::new(names.into_iter().map(String::from).collect())
}

fn build_station_idx(stations: &[FeatureStation]) -> HashMap<String, usize> {
    let mut idx = HashMap::with_capacity(stations.len());
    for (i, s) in stations.iter().enumerate() {
        // Last-write-wins on duplicate ids. The stations table has
        // station id as primary key in every documented layout, so
        // duplicates are schema-level impossible; the fallback just
        // means a quirky deployment doesn't panic.
        idx.insert(s.id.clone(), i);
    }
    idx
}

/// Thread-safe, reload-safe metadata cache. Reads are lock-free.
#[derive(Debug)]
pub struct MetadataCache {
    inner: ArcSwap<CollectionMeta>,
}

impl MetadataCache {
    /// An empty cache for `cfg`, its filterables already seeded from the
    /// configured `stations.property_cols` (#1024).
    pub fn new(cfg: &PostgisEngineConfig) -> Self {
        Self {
            inner: ArcSwap::from_pointee(CollectionMeta::empty(cfg)),
        }
    }

    pub fn load(&self) -> Arc<CollectionMeta> {
        self.inner.load_full()
    }

    /// Atomically replace the cache snapshot.
    pub fn store(&self, meta: CollectionMeta) {
        self.inner.store(Arc::new(meta));
    }

    /// Run one refresh against the pool. Replaces the cached snapshot
    /// on success, leaves it untouched on error. Caller logs the result.
    pub async fn refresh(
        &self,
        cfg: &PostgisEngineConfig,
        pool: &Pool,
    ) -> Result<(), MetadataError> {
        let locations = fetch_locations(cfg, pool).await?;
        let temporal = fetch_temporal_extent(cfg, pool).await?;
        let version = self.inner.load().version.wrapping_add(1);
        self.store(CollectionMeta::build(cfg, locations, temporal, version));
        Ok(())
    }
}

fn build_parameter_descriptions(
    params: &[ValidatedParameter],
) -> HashMap<String, ParameterDescription> {
    params
        .iter()
        .map(|p| {
            (
                p.name.clone(),
                ParameterDescription {
                    label: if p.label.is_empty() {
                        p.name.clone()
                    } else {
                        p.label.clone()
                    },
                    unit: p.unit.clone(),
                    observed_property: p.observed_property.clone(),
                    standard_name: None,
                },
            )
        })
        .collect()
}

fn spatial_extent_from(locations: &[Location]) -> Option<[f64; 4]> {
    if locations.is_empty() {
        return None;
    }
    let mut min_lon = f64::INFINITY;
    let mut min_lat = f64::INFINITY;
    let mut max_lon = f64::NEG_INFINITY;
    let mut max_lat = f64::NEG_INFINITY;
    for l in locations {
        min_lon = min_lon.min(l.longitude);
        max_lon = max_lon.max(l.longitude);
        min_lat = min_lat.min(l.latitude);
        max_lat = max_lat.max(l.latitude);
    }
    if min_lon.is_finite() && min_lat.is_finite() {
        Some([min_lon, min_lat, max_lon, max_lat])
    } else {
        None
    }
}

/// Build the cached location/feature set per the collection's
/// [`LocationSource`]:
/// - `Stations` — the stations table only (rich rows: label + properties); the
///   whole registry is advertised regardless of whether it has data.
/// - `Observations` — derived from the observations table's geometry (mode A):
///   one Point per distinct `station_fk` **reporting within the window**,
///   `label = id`, no properties.
/// - `StationsWithOrphans` (mode B) — **membership is the windowed obs reporters**
///   (same set as mode A); the stations table only supplies *metadata*
///   (label/properties/authoritative geometry) for the registered subset. A
///   registered-but-silent station is therefore NOT advertised — every listed
///   location has data within the window (use stations-only mode, i.e. omit
///   `observations.geom_col`, to advertise the full registry instead).
///
/// Every mode stops at `cfg.max_locations` stations and reports a list cut
/// there as truncated (#1061).
async fn fetch_locations(
    cfg: &PostgisEngineConfig,
    pool: &Pool,
) -> Result<LocationSet, MetadataError> {
    match &cfg.location_source {
        LocationSource::Stations(_) => fetch_station_rows(cfg, pool).await,
        LocationSource::Observations => fetch_observation_rows(cfg, pool).await,
        LocationSource::StationsWithOrphans(_) => {
            let reporters = fetch_observation_rows(cfg, pool).await?;
            let registry = fetch_station_rows(cfg, pool).await?;
            Ok(orphan_fallback(reporters, registry))
        }
        // Events shape: no station/location concept, nothing to fetch.
        LocationSource::None => Ok(LocationSet::default()),
    }
}

/// Rich station rows from the stations table (`build_locations`), each carrying
/// its `property_cols` coerced to [`PropertyValue`]. Only called for the
/// `Stations` / `StationsWithOrphans` variants, so the mapping is always present.
/// The query fetches one row past `max_locations`;
/// [`LocationSet::of_station_rows`] keeps the first `max_locations` and flags
/// the rest (#1061).
async fn fetch_station_rows(
    cfg: &PostgisEngineConfig,
    pool: &Pool,
) -> Result<LocationSet, MetadataError> {
    let stations = cfg.location_source.stations().ok_or_else(|| {
        MetadataError::Decode("fetch_station_rows called without a stations table".into())
    })?;
    let built = build_locations(cfg).map_err(|e| MetadataError::Decode(e.to_string()))?;
    let client = pool
        .get()
        .await
        .map_err(|e| MetadataError::Pool(e.to_string()))?;
    let stmt = client
        .prepare_cached(&built.sql)
        .await
        .map_err(|e| MetadataError::Db(e.to_string()))?;
    let param_refs = crate::query::params_as_refs(&built.params);
    let rows = client
        .query(&stmt, &param_refs)
        .await
        .map_err(|e| MetadataError::Db(e.to_string()))?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let id: String = row
            .try_get("id")
            .map_err(|e| MetadataError::Decode(e.to_string()))?;
        let label: String = row
            .try_get("label")
            .map_err(|e| MetadataError::Decode(e.to_string()))?;
        let lat: f64 = row
            .try_get("lat")
            .map_err(|e| MetadataError::Decode(e.to_string()))?;
        let lon: f64 = row
            .try_get("lon")
            .map_err(|e| MetadataError::Decode(e.to_string()))?;

        let mut properties = HashMap::with_capacity(stations.property_cols.len());
        for col_name in &stations.property_cols {
            let value = decode_property_by_name(row, col_name)?;
            properties.insert(col_name.clone(), value);
        }

        out.push(FeatureStation {
            id,
            label,
            lat,
            lon,
            properties: Arc::new(properties),
        });
    }
    Ok(LocationSet::of_station_rows(cfg, out))
}

/// Locations derived from the observations table's own geometry
/// (`build_locations_from_observations`). Orphans carry no station metadata:
/// `label = id`, empty properties.
///
/// One query per observation table (each small enough to stay under a read-only
/// role's `statement_timeout`); rows are merged by [`DerivedLocations`] — the
/// cross-table dedup a single `UNION`'s outer `DISTINCT ON (id)` used to do,
/// first occurrence winning — up to `max_locations` stations.
///
/// Buffering note: each per-table query fetches `max_locations + 1` rows, run
/// sequentially on one connection, so peak buffered rows ≈ one table's result
/// set; the merged set stops at `max_locations`, and reading stops at the
/// first distinct station past it. The *total* rows fetched across tables can
/// reach `tables × (max_locations + 1)` for a high-cardinality `per_parameter`
/// source (vs. the old single-UNION cap of one list) — narrow the source or
/// pre-materialize if that bites.
async fn fetch_observation_rows(
    cfg: &PostgisEngineConfig,
    pool: &Pool,
) -> Result<LocationSet, MetadataError> {
    // `Some(window)` ⇒ only stations seen since `now - window` (the default —
    // keeps the per-table DISTINCT ON on recent chunks); `None` ⇒ full history.
    // `checked_sub_signed` so an absurd window (e.g. a typo'd huge duration)
    // degrades to full history instead of panicking the background refresh.
    let since = cfg
        .locations_window
        .and_then(|w| Utc::now().checked_sub_signed(w));
    let queries = build_locations_from_observations(cfg, since)
        .map_err(|e| MetadataError::Decode(e.to_string()))?;
    let client = pool
        .get()
        .await
        .map_err(|e| MetadataError::Pool(e.to_string()))?;

    let mut merged = DerivedLocations::new(cfg.max_locations);
    for built in &queries {
        let stmt = client
            .prepare_cached(&built.sql)
            .await
            .map_err(|e| MetadataError::Db(e.to_string()))?;
        let param_refs = crate::query::params_as_refs(&built.params);
        let rows = client
            .query(&stmt, &param_refs)
            .await
            .map_err(|e| MetadataError::Db(e.to_string()))?;
        if !merged.merge_table(&rows, derived_row_id, derived_row_lat_lon)? {
            break; // a station past the cap cut the list: read no further table
        }
    }
    Ok(merged.finish())
}

fn derived_row_id(row: &Row) -> Result<String, MetadataError> {
    row.try_get("id")
        .map_err(|e| MetadataError::Decode(e.to_string()))
}

fn derived_row_lat_lon(row: &Row) -> Result<(f64, f64), MetadataError> {
    let decode = |col: &str| -> Result<f64, MetadataError> {
        row.try_get(col)
            .map_err(|e| MetadataError::Decode(e.to_string()))
    };
    Ok((decode("lat")?, decode("lon")?))
}

/// The observations-derived location list (modes A and B), merged across the
/// per-table queries in query order: the first occurrence of an id wins
/// (stations are assumed spatially stable), and the list holds at most `cap`
/// stations. A distinct station past them cuts it (#1061); a list of exactly
/// `cap` stations is complete.
pub(crate) struct DerivedLocations {
    cap: usize,
    seen: HashSet<String>,
    stations: Vec<FeatureStation>,
    cut: bool,
    no_properties: Arc<HashMap<String, PropertyValue>>,
}

impl DerivedLocations {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            cap,
            seen: HashSet::new(),
            stations: Vec::new(),
            cut: false,
            no_properties: Arc::new(HashMap::new()),
        }
    }

    /// Merge one table's result set in row order, as `fetch_observation_rows`
    /// does for each per-table query. A row whose id is already listed is
    /// skipped before its geometry is decoded. Returns `Ok(false)` when a new
    /// station arrives with the list full: the list is cut, the rest of the
    /// rows are not read, and the caller reads no further table.
    pub(crate) fn merge_table<R>(
        &mut self,
        rows: &[R],
        id_of: impl Fn(&R) -> Result<String, MetadataError>,
        lat_lon_of: impl Fn(&R) -> Result<(f64, f64), MetadataError>,
    ) -> Result<bool, MetadataError> {
        for row in rows {
            let id = id_of(row)?;
            if self.seen.contains(&id) {
                continue; // already seen in an earlier row or table: first geometry wins
            }
            let (lat, lon) = lat_lon_of(row)?;
            if !self.push(id, lat, lon) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Add one new reporting station as a bare location (`label = id`, no
    /// properties). Returns `false`, and adds nothing, when the list is
    /// already full: the list is cut.
    fn push(&mut self, id: String, lat: f64, lon: f64) -> bool {
        if self.stations.len() >= self.cap {
            self.cut = true;
            return false;
        }
        self.seen.insert(id.clone());
        self.stations.push(FeatureStation {
            id: id.clone(),
            label: id,
            lat,
            lon,
            properties: self.no_properties.clone(),
        });
        true
    }

    pub(crate) fn finish(self) -> LocationSet {
        LocationSet {
            stations: self.stations,
            cut: self.cut.then_some(LocationsCut::Stations),
        }
    }
}

/// Mode B: the windowed `reporters` enriched from the stations `registry`
/// ([`enrich_with_stations`]). Both reads stop at `max_locations`. A cut
/// reporter list misses stations: [`LocationsCut::Stations`]. A cut registry
/// matters only when a reporter finds no registry row, since that row may be
/// past the cap: [`LocationsCut::Registry`]. When every reporter found its
/// row, the cut dropped nothing that is served and the list is complete.
fn orphan_fallback(reporters: LocationSet, registry: LocationSet) -> LocationSet {
    let cut = if reporters.cut.is_some() {
        Some(LocationsCut::Stations)
    } else if registry.cut.is_some() && {
        let known: HashSet<&str> = registry.stations.iter().map(|s| s.id.as_str()).collect();
        reporters
            .stations
            .iter()
            .any(|r| !known.contains(r.id.as_str()))
    } {
        Some(LocationsCut::Registry)
    } else {
        None
    };
    LocationSet {
        stations: enrich_with_stations(reporters.stations, registry.stations),
        cut,
    }
}

/// Mode B membership = the windowed `reporters`; the stations table only
/// supplies *metadata*. Each reporter whose id is in the stations registry is
/// replaced by its rich stations row (label + properties + authoritative
/// geometry); reporters with no registry match stay as bare orphans. The result
/// is the reporters set (already capped at `max_locations`) — a
/// registered-but-silent station is intentionally absent, so every advertised
/// location has data within the window.
fn enrich_with_stations(
    reporters: Vec<FeatureStation>,
    stations: Vec<FeatureStation>,
) -> Vec<FeatureStation> {
    let registry: HashMap<&str, &FeatureStation> =
        stations.iter().map(|s| (s.id.as_str(), s)).collect();
    reporters
        .into_iter()
        .map(|r| {
            registry
                .get(r.id.as_str())
                .map(|s| (*s).clone())
                .unwrap_or(r)
        })
        .collect()
}

/// Coerce a column value to [`PropertyValue`] based on the column's
/// PostgreSQL type. Null values map to `PropertyValue::Null`. Unsupported
/// types (arrays, json, enums, etc.) return an error at decode time —
/// operators see the mismatch early rather than at HTTP-request time.
pub fn decode_property_by_name(row: &Row, name: &str) -> Result<PropertyValue, MetadataError> {
    let col_idx = row
        .columns()
        .iter()
        .position(|c| c.name() == name)
        .ok_or_else(|| MetadataError::Decode(format!("column '{name}' not in row")))?;
    decode_property(row, col_idx)
}

fn decode_property(row: &Row, col_idx: usize) -> Result<PropertyValue, MetadataError> {
    let col_type = row.columns()[col_idx].type_().clone();
    let decode_err = |e: tokio_postgres::Error| MetadataError::Decode(e.to_string());

    if col_type == Type::BOOL {
        let v: Option<bool> = row.try_get(col_idx).map_err(decode_err)?;
        return Ok(v.map(PropertyValue::Bool).unwrap_or(PropertyValue::Null));
    }
    if col_type == Type::INT2 {
        let v: Option<i16> = row.try_get(col_idx).map_err(decode_err)?;
        return Ok(v
            .map(|x| PropertyValue::Integer(x as i64))
            .unwrap_or(PropertyValue::Null));
    }
    if col_type == Type::INT4 {
        let v: Option<i32> = row.try_get(col_idx).map_err(decode_err)?;
        return Ok(v
            .map(|x| PropertyValue::Integer(x as i64))
            .unwrap_or(PropertyValue::Null));
    }
    if col_type == Type::INT8 {
        let v: Option<i64> = row.try_get(col_idx).map_err(decode_err)?;
        return Ok(v.map(PropertyValue::Integer).unwrap_or(PropertyValue::Null));
    }
    if col_type == Type::FLOAT4 {
        let v: Option<f32> = row.try_get(col_idx).map_err(decode_err)?;
        return Ok(v
            .map(|x| PropertyValue::Float(x as f64))
            .unwrap_or(PropertyValue::Null));
    }
    if col_type == Type::FLOAT8 {
        let v: Option<f64> = row.try_get(col_idx).map_err(decode_err)?;
        return Ok(v.map(PropertyValue::Float).unwrap_or(PropertyValue::Null));
    }
    if col_type == Type::TEXT
        || col_type == Type::VARCHAR
        || col_type == Type::BPCHAR
        || col_type == Type::NAME
    {
        let v: Option<String> = row.try_get(col_idx).map_err(decode_err)?;
        return Ok(v.map(PropertyValue::String).unwrap_or(PropertyValue::Null));
    }
    // NUMERIC has no direct f64 FromSql; users who need it should map it in
    // a VIEW or use one of the int/float types instead. Explicit rejection
    // is clearer than a confusing decode error.
    Err(MetadataError::Decode(format!(
        "unsupported column type '{}' for property coercion (supported: bool, int2/4/8, float4/8, text/varchar/bpchar/name)",
        col_type.name()
    )))
}

async fn fetch_temporal_extent(
    cfg: &PostgisEngineConfig,
    pool: &Pool,
) -> Result<Option<(DateTime<Utc>, DateTime<Utc>)>, MetadataError> {
    // Pick the first observation table to probe. For per_parameter we use
    // the first declared table; for long/wide it's the single table.
    let (table, time_col, tz) = match &cfg.observations {
        ObservationSchema::Long(l) => (&l.table, l.time_col.as_str(), l.time_col_tz.as_deref()),
        ObservationSchema::Wide(w) => (&w.table, w.time_col.as_str(), w.time_col_tz.as_deref()),
        ObservationSchema::PerParameter(pp) => {
            let Some(first) = pp.tables.first() else {
                return Ok(None);
            };
            (
                &first.table,
                first.time_col.as_str(),
                first.time_col_tz.as_deref(),
            )
        }
        ObservationSchema::Events(ev) => {
            // MIN/MAX over the event time column — index-only on any table
            // with a leading-time btree (the documented deployment shape).
            (&ev.table, ev.time_col.as_str(), ev.time_col_tz.as_deref())
        }
    };

    // Use quote_ident for every identifier — defense-in-depth even though
    // schema/table/column have already passed the whitelist regex at
    // config load.
    let time_col_quoted =
        crate::security::quote_ident(time_col).map_err(|e| MetadataError::Decode(e.to_string()))?;
    let schema_quoted = crate::security::quote_ident(&table.schema)
        .map_err(|e| MetadataError::Decode(e.to_string()))?;
    let table_quoted = crate::security::quote_ident(&table.table)
        .map_err(|e| MetadataError::Decode(e.to_string()))?;
    let time_expr = match tz {
        None => time_col_quoted.clone(),
        Some(tz) => format!(
            "({time_col_quoted} AT TIME ZONE '{}')",
            tz.replace('\'', "''")
        ),
    };
    // Compose the statement with the SELECT keyword in a plain string
    // literal (not in a format macro) so the check_sql_safety tripwire
    // stays meaningful.
    let mut sql = String::from("SELECT ");
    sql.push_str(&format!("MIN({time_expr})::timestamptz AS lo, "));
    sql.push_str(&format!("MAX({time_expr})::timestamptz AS hi "));
    sql.push_str(&format!("FROM {schema_quoted}.{table_quoted}"));

    let client = pool
        .get()
        .await
        .map_err(|e| MetadataError::Pool(e.to_string()))?;
    let empty_params: Vec<SqlParam> = vec![];
    let param_refs = crate::query::params_as_refs(&empty_params);
    let row = client
        .query_one(&sql, &param_refs)
        .await
        .map_err(|e| MetadataError::Db(e.to_string()))?;

    let lo: Option<DateTime<Utc>> = row
        .try_get("lo")
        .map_err(|e| MetadataError::Decode(e.to_string()))?;
    let hi: Option<DateTime<Utc>> = row
        .try_get("hi")
        .map_err(|e| MetadataError::Decode(e.to_string()))?;
    Ok(lo.zip(hi))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{LongShape, QualifiedTable, StationsMapping};

    fn mk_loc(id: &str, lon: f64, lat: f64) -> Location {
        Location {
            id: id.into(),
            label: id.into(),
            latitude: lat,
            longitude: lon,
        }
    }

    #[test]
    fn spatial_extent_empty_is_none() {
        assert!(spatial_extent_from(&[]).is_none());
    }

    #[test]
    fn spatial_extent_single_station_is_zero_box() {
        let locs = vec![mk_loc("s", 24.5, 60.2)];
        assert_eq!(spatial_extent_from(&locs), Some([24.5, 60.2, 24.5, 60.2]));
    }

    #[test]
    fn spatial_extent_covers_all_stations() {
        let locs = vec![
            mk_loc("a", 10.0, 40.0),
            mk_loc("b", 30.0, 45.0),
            mk_loc("c", 20.0, 35.0),
        ];
        assert_eq!(spatial_extent_from(&locs), Some([10.0, 35.0, 30.0, 45.0]));
    }

    fn stations_mapping(property_cols: &[&str]) -> StationsMapping {
        StationsMapping {
            table: QualifiedTable {
                schema: "public".into(),
                table: "stations".into(),
            },
            id_col: "id".into(),
            label_col: "name".into(),
            geom_col: "geom".into(),
            property_cols: property_cols.iter().map(|c| (*c).to_string()).collect(),
            where_clause: None,
        }
    }

    fn cfg_with(location_source: LocationSource) -> PostgisEngineConfig {
        PostgisEngineConfig {
            dsn: "postgres://user:pw@127.0.0.1:1/db".into(),
            dsn_was_literal: false,
            pool_size: 2,
            pool_label: None,
            metadata_refresh_secs: 300,
            max_locations: ds_core::config::POSTGIS_DEFAULT_MAX_LOCATIONS,
            location_source,
            observations: ObservationSchema::Long(LongShape {
                table: QualifiedTable {
                    schema: "public".into(),
                    table: "obs".into(),
                },
                station_fk_col: "station_id".into(),
                time_col: "time".into(),
                time_col_tz: None,
                param_col: "param".into(),
                value_col: "value".into(),
                geom_col: None,
            }),
            parameters: vec![],
            locations_window: None,
            events_default_window: None,
            events_extent_bbox: None,
        }
    }

    fn names(f: &FilterableProperties) -> Vec<&str> {
        f.iter().map(String::as_str).collect()
    }

    /// #1024: the configured property columns are filterable before any row
    /// is read, both in a fresh cache (the database was unreachable at boot)
    /// and after a refresh that found an empty stations table.
    #[test]
    fn configured_property_cols_are_filterable_without_rows() {
        let mapping = stations_mapping(&["territory", "elevation"]);
        for source in [
            LocationSource::Stations(mapping.clone()),
            LocationSource::StationsWithOrphans(mapping),
        ] {
            let cfg = cfg_with(source);
            let fresh = MetadataCache::new(&cfg).load();
            assert_eq!(names(&fresh.filterables), ["elevation", "territory"]);
            assert!(fresh.feature_stations.is_empty());
            assert_eq!(fresh.version, 0);

            let empty_table = CollectionMeta::build(&cfg, LocationSet::default(), None, 1);
            assert_eq!(names(&empty_table.filterables), ["elevation", "territory"]);
            assert!(empty_table.feature_stations.is_empty());
        }
    }

    /// No stations table ⇒ nothing configured to seed: the observations-only
    /// mode and the events shape keep an empty catalog.
    #[test]
    fn no_stations_table_seeds_no_filterables() {
        for source in [LocationSource::Observations, LocationSource::None] {
            let cfg = cfg_with(source);
            assert!(MetadataCache::new(&cfg).load().filterables.is_empty());
            let bare = mk_fs("0-246-0-101004", 24.9, 60.2, "0-246-0-101004", &[]);
            let meta = CollectionMeta::build(&cfg, LocationSet::complete(vec![bare]), None, 1);
            assert!(meta.filterables.is_empty());
        }
    }

    /// A refresh keeps the configured names and adds whatever the rows carry;
    /// a name no row holds stays filterable.
    #[test]
    fn refreshed_filterables_union_configured_and_row_names() {
        let cfg = cfg_with(LocationSource::StationsWithOrphans(stations_mapping(&[
            "territory",
            "elevation",
        ])));
        let rows = vec![
            mk_fs("reg", 25.0, 60.3, "Helsinki", &[("territory", "FI")]),
            mk_fs("other", 18.0, 59.3, "Stockholm", &[("network", "synop")]),
            mk_fs("orphan", 18.9, 12.1, "orphan", &[]),
        ];
        let meta = CollectionMeta::build(&cfg, LocationSet::complete(rows), None, 7);
        assert_eq!(
            names(&meta.filterables),
            ["elevation", "network", "territory"]
        );
        assert_eq!(meta.version, 7);
        assert_eq!(meta.locations.len(), 3);
        assert_eq!(meta.station_idx.get("other"), Some(&1));
        assert_eq!(meta.spatial_extent, Some([18.0, 12.1, 25.0, 60.3]));
    }

    #[test]
    fn cache_starts_empty_and_store_replaces() {
        let cache = MetadataCache::new(&cfg_with(LocationSource::Observations));
        let m0 = cache.load();
        assert_eq!(m0.version, 0);
        assert!(m0.locations.is_empty());

        let next = CollectionMeta {
            feature_stations: Arc::new(vec![]),
            filterables: Default::default(),
            locations: Arc::new(LocationSnapshot::new(vec![mk_loc("s", 1.0, 2.0)])),
            station_idx: Arc::new(HashMap::from([("s".to_string(), 0)])),
            parameters: Arc::new(HashMap::new()),
            temporal_extent: None,
            spatial_extent: Some([1.0, 2.0, 1.0, 2.0]),
            wms_times: Arc::new(Vec::new()),
            locations_cut: None,
            version: 1,
        };
        cache.store(next);
        let m1 = cache.load();
        assert_eq!(m1.version, 1);
        assert_eq!(m1.locations.len(), 1);
    }

    #[test]
    fn build_station_idx_maps_id_to_vec_index() {
        fn mk(id: &str) -> FeatureStation {
            FeatureStation {
                id: id.into(),
                label: id.into(),
                lat: 0.0,
                lon: 0.0,
                properties: Arc::new(HashMap::new()),
            }
        }
        let stations = vec![mk("a"), mk("b"), mk("c")];
        let idx = build_station_idx(&stations);
        assert_eq!(idx.get("a"), Some(&0));
        assert_eq!(idx.get("b"), Some(&1));
        assert_eq!(idx.get("c"), Some(&2));
        assert_eq!(idx.get("missing"), None);
    }

    #[test]
    fn build_station_idx_empty_returns_empty_map() {
        let idx = build_station_idx(&[]);
        assert!(idx.is_empty());
    }

    fn mk_fs(id: &str, lon: f64, lat: f64, label: &str, props: &[(&str, &str)]) -> FeatureStation {
        let mut m = HashMap::new();
        for (k, v) in props {
            m.insert((*k).to_string(), PropertyValue::String((*v).to_string()));
        }
        FeatureStation {
            id: id.into(),
            label: label.into(),
            lat,
            lon,
            properties: Arc::new(m),
        }
    }

    #[test]
    fn enrich_with_stations_membership_is_reporters_only() {
        // Reporters (windowed) = the membership. The registry has a rich row for
        // "reg" and also a silent station "silent" that did NOT report.
        let reporters = vec![
            mk_fs("reg", 24.9, 60.2, "reg", &[]), // bare obs row (obs geom)
            mk_fs("orphan", 18.9, 12.1, "orphan", &[]),
        ];
        let registry = vec![
            mk_fs("reg", 25.0, 60.3, "Helsinki", &[("territory", "FI")]),
            mk_fs("silent", 1.0, 2.0, "Nowhere", &[("territory", "XX")]),
        ];
        let out = enrich_with_stations(reporters, registry);

        // Exactly the two reporters — the silent registered station is NOT listed.
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|s| s.id != "silent"));

        // "reg" reported AND is registered ⇒ enriched with the registry row
        // (rich label + territory + authoritative geometry).
        let reg = out.iter().find(|s| s.id == "reg").unwrap();
        assert_eq!(reg.label, "Helsinki");
        assert_eq!(reg.lon, 25.0); // authoritative stations geom, not the obs geom
        assert_eq!(
            reg.properties.get("territory"),
            Some(&PropertyValue::String("FI".into()))
        );

        // "orphan" reported but isn't registered ⇒ bare (id-as-label, no props).
        let orphan = out.iter().find(|s| s.id == "orphan").unwrap();
        assert_eq!(orphan.label, "orphan");
        assert!(orphan.properties.is_empty());
    }

    fn ids(stations: &[FeatureStation]) -> Vec<&str> {
        stations.iter().map(|s| s.id.as_str()).collect()
    }

    /// #1061, stations-only mode: the query fetched `cap + 1` rows; one
    /// past the cap cuts the list and the first `cap` are kept in query
    /// order. Exactly `cap` rows, or fewer, is a complete list.
    #[test]
    fn location_set_first_of_flags_a_row_past_the_cap() {
        let row = |id: &str| mk_fs(id, 25.0, 60.0, id, &[("territory", "FI")]);
        let cut = LocationSet::first_of(["a", "b", "c", "d"].map(row).to_vec(), 3);
        assert_eq!(cut.cut, Some(LocationsCut::Stations));
        assert_eq!(ids(&cut.stations), ["a", "b", "c"]);
        for n in [0, 2, 3] {
            let rows: Vec<_> = ["a", "b", "c"].map(row).into_iter().take(n).collect();
            let set = LocationSet::first_of(rows, 3);
            assert_eq!(set.cut, None, "{n} rows");
            assert_eq!(set.stations.len(), n);
        }
    }

    /// One per-table result set as these tests write it: `(id, lat, lon)`.
    type DerivedRow = (&'static str, f64, f64);

    fn merge_rows(merged: &mut DerivedLocations, rows: &[DerivedRow]) -> bool {
        merged
            .merge_table(rows, |r| Ok(r.0.to_string()), |r| Ok((r.1, r.2)))
            .unwrap()
    }

    /// #1061, observations-derived modes, merged as `fetch_observation_rows`
    /// merges each per-table query: ids repeat within and across tables and
    /// count once, the first geometry winning. A list that reaches the cap
    /// is complete.
    #[test]
    fn derived_locations_count_distinct_ids_against_the_cap() {
        let mut merged = DerivedLocations::new(3);
        assert!(merge_rows(
            &mut merged,
            &[("a", 60.0, 25.0), ("b", 61.0, 25.0), ("a", 0.0, 0.0)]
        ));
        assert!(merge_rows(
            &mut merged,
            &[
                ("b", 0.0, 0.0),
                ("c", 62.0, 26.0),
                ("a", 0.0, 0.0),
                ("c", 0.0, 0.0)
            ]
        ));
        let set = merged.finish();
        assert_eq!(set.cut, None);
        assert_eq!(ids(&set.stations), ["a", "b", "c"]);
        assert_eq!((set.stations[0].lat, set.stations[0].lon), (60.0, 25.0));
        assert!(set
            .stations
            .iter()
            .all(|s| s.label == s.id && s.properties.is_empty()));
    }

    /// #1061: the first new id past the cap cuts the list. The merge stops
    /// there: later rows are not decoded, and a repeated id's geometry never
    /// is.
    #[test]
    fn derived_locations_stop_at_the_first_station_past_the_cap() {
        use std::cell::Cell;
        let mut merged = DerivedLocations::new(3);
        assert!(merge_rows(
            &mut merged,
            &[("a", 60.0, 25.0), ("b", 61.0, 25.0)]
        ));
        let (ids_read, geometries_read) = (Cell::new(0), Cell::new(0));
        let more = merged
            .merge_table(
                &[
                    ("a", 0.0, 0.0),
                    ("c", 62.0, 26.0),
                    ("d", 63.0, 27.0),
                    ("e", 64.0, 28.0),
                ],
                |r: &DerivedRow| {
                    ids_read.set(ids_read.get() + 1);
                    Ok(r.0.to_string())
                },
                |r: &DerivedRow| {
                    geometries_read.set(geometries_read.get() + 1);
                    Ok((r.1, r.2))
                },
            )
            .unwrap();
        assert!(!more, "the caller reads no further table");
        assert_eq!(ids_read.get(), 3, "a, c, d: not e");
        assert_eq!(geometries_read.get(), 2, "c, d: not the repeated a");
        let set = merged.finish();
        assert_eq!(set.cut, Some(LocationsCut::Stations));
        assert_eq!(ids(&set.stations), ["a", "b", "c"]);
    }

    /// #1061, mode B: a cut reporter list misses stations; a cut registry
    /// matters only when a reporter is left without its registry row, and
    /// is reported as a registry cut.
    #[test]
    fn orphan_fallback_reports_which_read_was_cut() {
        let cut_if = |cut: bool| cut.then_some(LocationsCut::Stations);
        let reporters = |cut, with_orphan: bool| LocationSet {
            stations: [("reg", 24.9, 60.2), ("late", 1.0, 2.0)]
                .into_iter()
                .take(if with_orphan { 2 } else { 1 })
                .map(|(id, lon, lat)| mk_fs(id, lon, lat, id, &[]))
                .collect(),
            cut: cut_if(cut),
        };
        let registry = |cut| LocationSet {
            stations: vec![mk_fs("reg", 25.0, 60.3, "Helsinki", &[("territory", "FI")])],
            cut: cut_if(cut),
        };
        for (reporters_cut, registry_cut, with_orphan, expected) in [
            (false, false, true, None),
            (true, false, true, Some(LocationsCut::Stations)),
            (true, true, true, Some(LocationsCut::Stations)),
            (false, true, true, Some(LocationsCut::Registry)),
            (false, true, false, None),
        ] {
            let out = orphan_fallback(
                reporters(reporters_cut, with_orphan),
                registry(registry_cut),
            );
            assert_eq!(
                out.cut, expected,
                "reporters cut {reporters_cut}, registry cut {registry_cut}, orphan {with_orphan}"
            );
            assert_eq!(out.stations[0].label, "Helsinki");
            assert_eq!(out.stations.len(), if with_orphan { 2 } else { 1 });
        }
    }

    /// #1061: the snapshot carries the cut; a fresh cache has none.
    #[test]
    fn snapshot_carries_the_cut() {
        let cfg = cfg_with(LocationSource::Observations);
        assert_eq!(MetadataCache::new(&cfg).load().locations_cut, None);
        let set = LocationSet {
            stations: vec![mk_fs("a", 25.0, 60.0, "a", &[])],
            cut: Some(LocationsCut::Registry),
        };
        let meta = CollectionMeta::build(&cfg, set, None, 1);
        assert_eq!(meta.locations_cut, Some(LocationsCut::Registry));
        assert_eq!(meta.locations.len(), 1);
    }

    /// The `/health` reasons name the cap and what is missing.
    #[test]
    fn cut_reasons_name_the_cap_and_the_read() {
        let stations = LocationsCut::Stations.reason(3);
        assert!(
            stations.starts_with(
                "station list truncated at max_locations = 3: stations past it are not served"
            ),
            "{stations}"
        );
        let registry = LocationsCut::Registry.reason(3);
        assert!(
            registry.starts_with("station registry truncated at max_locations = 3:"),
            "{registry}"
        );
        assert!(
            registry.contains("without their label and properties"),
            "{registry}"
        );
    }

    #[test]
    fn parameter_descriptions_fill_label_default() {
        let params = vec![
            ValidatedParameter {
                name: "t2m".into(),
                label: "".into(), // empty ⇒ fall back to name
                unit: "°C".into(),
                observed_property: "air_temperature".into(),
                source_key: "t2m".into(),
            },
            ValidatedParameter {
                name: "ws".into(),
                label: "Wind".into(),
                unit: "m/s".into(),
                observed_property: "wind_speed".into(),
                source_key: "ws".into(),
            },
        ];
        let descs = build_parameter_descriptions(&params);
        assert_eq!(descs["t2m"].label, "t2m");
        assert_eq!(descs["ws"].label, "Wind");
        assert_eq!(descs["t2m"].unit, "°C");
    }

    /// What one cached station costs (#1061): the measurement behind
    /// `POSTGIS_DEFAULT_MAX_LOCATIONS` and the ceiling.
    mod footprint {
        use super::*;
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        /// The system allocator, counting the live bytes each thread allocated,
        /// so tests running in parallel on other threads do not disturb a
        /// measurement. Requested sizes: the allocator's size-class rounding
        /// comes on top.
        struct Counting;

        thread_local! {
            static LIVE: Cell<isize> = const { Cell::new(0) };
        }

        fn count(delta: isize) {
            let _ = LIVE.try_with(|live| live.set(live.get() + delta));
        }

        fn live() -> isize {
            LIVE.with(Cell::get)
        }

        // SAFETY: every call forwards to `System` with the caller's arguments;
        // the counter is a const-initialised thread-local without a destructor,
        // so touching it never allocates or re-enters the allocator.
        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                let p = unsafe { System.alloc(layout) };
                if !p.is_null() {
                    count(layout.size() as isize);
                }
                p
            }
            unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
                let p = unsafe { System.alloc_zeroed(layout) };
                if !p.is_null() {
                    count(layout.size() as isize);
                }
                p
            }
            unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
                unsafe { System.dealloc(p, layout) };
                count(-(layout.size() as isize));
            }
            unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
                let q = unsafe { System.realloc(p, layout, new_size) };
                if !q.is_null() {
                    count(new_size as isize - layout.size() as isize);
                }
                q
            }
        }

        #[global_allocator]
        static GLOBAL: Counting = Counting;

        /// A WIGOS-style id, a station name and three properties of the common
        /// kinds (text, float, integer): what `fetch_station_rows` decodes.
        fn decoded_rows(n: usize) -> Vec<FeatureStation> {
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                let mut properties = HashMap::with_capacity(3);
                properties.insert("territory".to_string(), PropertyValue::String("FI".into()));
                properties.insert("elevation_m".to_string(), PropertyValue::Float(i as f64));
                properties.insert("wmo_id".to_string(), PropertyValue::Integer(i as i64));
                out.push(FeatureStation {
                    id: format!("0-20000-0-{i:05}"),
                    label: format!("Station name {i:06}"),
                    lat: 60.0 + i as f64 * 1e-5,
                    lon: 25.0 + i as f64 * 1e-5,
                    properties: Arc::new(properties),
                });
            }
            out
        }

        /// Bytes per station of the decoded rows and of the snapshot built
        /// from them (the rows plus the `Location` snapshot and the id
        /// indexes). Run with `--nocapture` to print them. The bound keeps
        /// the documented sizing honest: under 1 KiB a station, 200 000
        /// stations stay under 200 MiB resident and a refresh, which holds
        /// the old snapshot while it builds the next, under 400 MiB.
        ///
        /// What a request costs is pinned too (#1083): the `Location` list
        /// `get_locations` hands it, a lookup by id and a page of it allocate
        /// nothing.
        #[test]
        fn per_station_bytes_of_the_cached_snapshot() {
            const N: usize = 20_000;
            let cfg = cfg_with(LocationSource::Stations(stations_mapping(&[
                "territory",
                "elevation_m",
                "wmo_id",
            ])));

            let before = live();
            let rows = decoded_rows(N);
            let rows_bytes = live() - before;

            let started = std::time::Instant::now();
            let meta = CollectionMeta::build(&cfg, LocationSet::complete(rows), None, 1);
            let build_time = started.elapsed();
            let snapshot_bytes = live() - before;

            let before_request = live();
            let shared = Arc::clone(&meta.locations);
            let found = shared.by_id("0-20000-0-12345").map(|l| l.label.len());
            let page = shared[N - 100..].len();
            let request_bytes = live() - before_request;

            let per = |bytes: isize| bytes as f64 / N as f64;
            eprintln!(
                "per station: decoded rows {:.0} B, snapshot {:.0} B; per request {request_bytes} B; \
                 build {:.2} µs/station (debug build)",
                per(rows_bytes),
                per(snapshot_bytes),
                build_time.as_secs_f64() * 1e6 / N as f64,
            );
            assert_eq!(meta.feature_stations.len(), N);
            assert_eq!(found, Some("Station name 012345".len()));
            assert_eq!(page, 100);
            assert_eq!(
                request_bytes, 0,
                "a request must borrow the list, never copy it"
            );
            assert!(per(rows_bytes) > 100.0, "the counter must see the rows");
            assert!(
                per(snapshot_bytes) < 1024.0,
                "a cached station grew past 1 KiB: {:.0} B — revisit \
                 POSTGIS_DEFAULT_MAX_LOCATIONS and the engine-postgis README sizing",
                per(snapshot_bytes)
            );
            drop(shared);
            drop(meta);
        }
    }
}
