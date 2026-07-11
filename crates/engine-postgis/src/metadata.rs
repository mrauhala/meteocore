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
use ds_core::feature::PropertyValue;
use ds_core::model::{Location, ParameterDescription};
use std::collections::{HashMap, HashSet};
use thiserror::Error;
use tokio_postgres::types::Type;
use tokio_postgres::Row;

use crate::config::{PostgisEngineConfig, ValidatedParameter};
use crate::query::{build_locations, build_locations_from_observations, SqlParam, MAX_LOCATIONS};
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

/// Snapshot of everything EDR + Features metadata endpoints need to
/// answer without hitting the pool. Cheap to clone (Arcs inside).
#[derive(Debug, Clone)]
pub struct CollectionMeta {
    /// Full station rows with property values. `locations` is derived
    /// from this list and kept in the same order, so `station_idx`
    /// is a valid index into both.
    pub feature_stations: Arc<Vec<FeatureStation>>,
    pub locations: Arc<Vec<Location>>,
    /// `id → index into feature_stations/locations` lookup built once
    /// per refresh. Constant-time station resolution for
    /// `query_location`, `query_area` (per-station), and
    /// `FeatureEngine::get_feature` — at 30 k stations the previous
    /// `iter().find()` was 12 M string compares per area request.
    pub station_idx: Arc<HashMap<String, usize>>,
    pub parameters: Arc<HashMap<String, ParameterDescription>>,
    pub temporal_extent: Option<(DateTime<Utc>, DateTime<Utc>)>,
    pub spatial_extent: Option<[f64; 4]>,
    /// Monotonic version counter — bumped by each successful refresh.
    pub version: u64,
}

impl CollectionMeta {
    fn empty() -> Self {
        Self {
            feature_stations: Arc::new(Vec::new()),
            locations: Arc::new(Vec::new()),
            station_idx: Arc::new(HashMap::new()),
            parameters: Arc::new(HashMap::new()),
            temporal_extent: None,
            spatial_extent: None,
            version: 0,
        }
    }
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
    pub fn new_empty() -> Self {
        Self {
            inner: ArcSwap::from_pointee(CollectionMeta::empty()),
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
        let feature_stations = fetch_locations(cfg, pool).await?;
        let locations: Vec<Location> = feature_stations
            .iter()
            .map(FeatureStation::to_location)
            .collect();
        let station_idx = build_station_idx(&feature_stations);
        let parameters = build_parameter_descriptions(&cfg.parameters);
        // Events shape: the location list is empty, so the spatial extent
        // comes from config (`extent_bbox`) — never from an `ST_Extent`
        // full scan over the event table.
        let spatial = match cfg.events_extent_bbox {
            Some(bbox) => Some(bbox),
            None => spatial_extent_from(&locations),
        };
        let temporal = fetch_temporal_extent(cfg, pool).await?;

        let previous = self.inner.load();
        let next = CollectionMeta {
            feature_stations: Arc::new(feature_stations),
            locations: Arc::new(locations),
            station_idx: Arc::new(station_idx),
            parameters: Arc::new(parameters),
            temporal_extent: temporal,
            spatial_extent: spatial,
            version: previous.version.wrapping_add(1),
        };
        self.inner.store(Arc::new(next));
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
async fn fetch_locations(
    cfg: &PostgisEngineConfig,
    pool: &Pool,
) -> Result<Vec<FeatureStation>, MetadataError> {
    match &cfg.location_source {
        LocationSource::Stations(_) => fetch_station_rows(cfg, pool).await,
        LocationSource::Observations => fetch_observation_rows(cfg, pool).await,
        LocationSource::StationsWithOrphans(_) => {
            let reporters = fetch_observation_rows(cfg, pool).await?;
            let stations = fetch_station_rows(cfg, pool).await?;
            Ok(enrich_with_stations(reporters, stations))
        }
        // Events shape: no station/location concept, nothing to fetch.
        LocationSource::None => Ok(Vec::new()),
    }
}

/// Rich station rows from the stations table (`build_locations`), each carrying
/// its `property_cols` coerced to [`PropertyValue`]. Only called for the
/// `Stations` / `StationsWithOrphans` variants, so the mapping is always present.
async fn fetch_station_rows(
    cfg: &PostgisEngineConfig,
    pool: &Pool,
) -> Result<Vec<FeatureStation>, MetadataError> {
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
    Ok(out)
}

/// Locations derived from the observations table's own geometry
/// (`build_locations_from_observations`). Orphans carry no station metadata:
/// `label = id`, empty properties.
///
/// One query per observation table (each small enough to stay under a read-only
/// role's `statement_timeout`); rows are deduplicated by id across tables here —
/// the cross-table dedup a single `UNION`'s outer `DISTINCT ON (id)` used to do.
/// First occurrence wins (stations are assumed spatially stable). The merged set
/// is capped at [`MAX_LOCATIONS`].
///
/// Buffering note: each per-table query is itself `LIMIT MAX_LOCATIONS`, run
/// sequentially on one connection, so peak buffered rows ≈ one table's result
/// set; the accumulated set is bounded by the early `break` at `MAX_LOCATIONS`.
/// The *total* rows fetched across tables can reach `tables × MAX_LOCATIONS` for
/// a high-cardinality `per_parameter` source (vs. the old single-UNION cap of
/// one `MAX_LOCATIONS`) — narrow the source or pre-materialize if that bites.
async fn fetch_observation_rows(
    cfg: &PostgisEngineConfig,
    pool: &Pool,
) -> Result<Vec<FeatureStation>, MetadataError> {
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

    let empty = Arc::new(HashMap::new());
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<FeatureStation> = Vec::new();
    let mut truncated = false;

    'tables: for built in &queries {
        let stmt = client
            .prepare_cached(&built.sql)
            .await
            .map_err(|e| MetadataError::Db(e.to_string()))?;
        let param_refs = crate::query::params_as_refs(&built.params);
        let rows = client
            .query(&stmt, &param_refs)
            .await
            .map_err(|e| MetadataError::Db(e.to_string()))?;

        for row in &rows {
            let id: String = row
                .try_get("id")
                .map_err(|e| MetadataError::Decode(e.to_string()))?;
            if !seen.insert(id.clone()) {
                continue; // already seen in an earlier table — first geometry wins
            }
            let lat: f64 = row
                .try_get("lat")
                .map_err(|e| MetadataError::Decode(e.to_string()))?;
            let lon: f64 = row
                .try_get("lon")
                .map_err(|e| MetadataError::Decode(e.to_string()))?;
            out.push(FeatureStation {
                id: id.clone(),
                label: id,
                lat,
                lon,
                properties: empty.clone(),
            });
            if out.len() >= MAX_LOCATIONS {
                truncated = true;
                break 'tables;
            }
        }
    }

    if truncated {
        tracing::warn!(
            cap = MAX_LOCATIONS,
            tables = queries.len(),
            "postgis: observations-derived location list hit the {MAX_LOCATIONS} cap — some stations may be missing; narrow the data, add a stations table, or pre-materialize distinct locations"
        );
    }
    Ok(out)
}

/// Mode B membership = the windowed `reporters`; the stations table only
/// supplies *metadata*. Each reporter whose id is in the stations registry is
/// replaced by its rich stations row (label + properties + authoritative
/// geometry); reporters with no registry match stay as bare orphans. The result
/// is the reporters set (already capped at [`MAX_LOCATIONS`]) — a
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

    #[test]
    fn cache_starts_empty_and_store_replaces() {
        let cache = MetadataCache::new_empty();
        let m0 = cache.load();
        assert_eq!(m0.version, 0);
        assert!(m0.locations.is_empty());

        let next = CollectionMeta {
            feature_stations: Arc::new(vec![]),
            locations: Arc::new(vec![mk_loc("s", 1.0, 2.0)]),
            station_idx: Arc::new(HashMap::from([("s".to_string(), 0)])),
            parameters: Arc::new(HashMap::new()),
            temporal_extent: None,
            spatial_extent: Some([1.0, 2.0, 1.0, 2.0]),
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
}
