//! `PostgisEngine` — implements the `ds_core::engine::Engine` trait on top
//! of a [`deadpool_postgres`] pool and a [`MetadataCache`].
//!
//! DB work is async; the trait is sync. The bridge is
//! `tokio::task::block_in_place(|| Handle::current().block_on(..))` — safe
//! from axum handlers because axum runs on a multi-thread runtime. The
//! engine never calls `block_on` from an async context without first
//! entering `block_in_place`.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use deadpool_postgres::Pool;
use ds_core::engine::Engine;
use ds_core::error::DataServerError;
use ds_core::model::{
    AreaQueryResult, DomainDescription, Location, NdArray, ParameterDescription, QueryResult,
};
use tokio_postgres::Row;

use crate::config::PostgisEngineConfig;
use crate::metadata::{CollectionMeta, MetadataCache};
use crate::query::{
    build_location, build_position, params_as_refs, BuiltQuery, DEFAULT_POSITION_RADIUS_M,
    MAX_OBSERVATION_ROWS,
};
use crate::schema::ObservationSchema;

/// Time-ordered observation values for a single parameter. Exists to
/// keep the Clippy `type_complexity` lint happy without papering over
/// the intent.
type ParamSeries = Vec<(DateTime<Utc>, Option<f64>)>;

/// Collection-scoped engine. Cheap to clone (Arcs inside); axum handlers
/// hold it behind `Arc<dyn Engine>` as is done for every engine.
pub struct PostgisEngine {
    collection_id: String,
    config: Arc<PostgisEngineConfig>,
    pool: Arc<Pool>,
    cache: Arc<MetadataCache>,
}

impl std::fmt::Debug for PostgisEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgisEngine")
            .field("collection_id", &self.collection_id)
            .field("pool_max_size", &self.pool.status().max_size)
            .field("cache_version", &self.cache.load().version)
            .finish()
    }
}

impl PostgisEngine {
    pub fn new(
        collection_id: impl Into<String>,
        config: Arc<PostgisEngineConfig>,
        pool: Arc<Pool>,
    ) -> Self {
        Self {
            collection_id: collection_id.into(),
            config,
            pool,
            cache: Arc::new(MetadataCache::new_empty()),
        }
    }

    pub fn collection_id(&self) -> &str {
        &self.collection_id
    }

    pub fn pool(&self) -> &Arc<Pool> {
        &self.pool
    }

    pub fn config(&self) -> &PostgisEngineConfig {
        &self.config
    }

    pub fn cache(&self) -> &MetadataCache {
        &self.cache
    }

    /// Run a one-shot metadata refresh. Used at construction and by the
    /// `/admin/collections/reload` path.
    pub async fn refresh_metadata(&self) -> Result<(), DataServerError> {
        self.cache
            .refresh(&self.config, &self.pool)
            .await
            .map_err(|e| DataServerError::Engine(format!("metadata refresh failed: {e}")))
    }

    fn load_meta(&self) -> Arc<CollectionMeta> {
        self.cache.load()
    }
}

// ─── Engine trait ────────────────────────────────────────────────────────────

impl Engine for PostgisEngine {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok((*self.load_meta().locations).clone())
    }

    fn get_parameters(&self) -> Vec<String> {
        self.config
            .parameters
            .iter()
            .map(|p| p.name.clone())
            .collect()
    }

    fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
        (*self.load_meta().parameters).clone()
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        self.load_meta().temporal_extent
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        self.load_meta().spatial_extent
    }

    fn supported_query_types(&self) -> Vec<String> {
        // query_area (#106) and features (#107) land in follow-up PRs.
        vec![
            "locations".to_string(),
            "position".to_string(),
            "location".to_string(),
        ]
    }

    fn query_location(
        &self,
        location_id: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
    ) -> Result<QueryResult, DataServerError> {
        let source_keys = resolve_source_keys(&self.config, parameters)?;
        let key_refs: Vec<&str> = source_keys.iter().map(String::as_str).collect();

        let queries = build_location(&self.config, location_id, datetime, &key_refs)
            .map_err(|e| DataServerError::Engine(format!("build_location: {e}")))?;

        let (lon, lat) = lookup_station_coords(&self.load_meta(), location_id)?;
        let rows_per_query = run_queries_sync(&self.pool, &queries)?;
        assemble_query_result(
            &self.config,
            location_id,
            lon,
            lat,
            &queries,
            rows_per_query,
        )
    }

    fn query_position(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
    ) -> Result<QueryResult, DataServerError> {
        let (lon, lat) = parse_coords(coords)?;
        let station_id = resolve_nearest_station(&self.pool, &self.config, lon, lat)?;
        self.query_location(&station_id, datetime, parameters)
    }

    fn query_area(
        &self,
        _coords: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
    ) -> Result<AreaQueryResult, DataServerError> {
        // #106 ships area queries. Until then, surface a clear error
        // rather than the generic "not supported" default.
        Err(DataServerError::InvalidParameter(
            "area query for postgis engine is not yet implemented (#106)".into(),
        ))
    }
}

// ─── helpers ───────────────────────────────────────────────────────────────

fn resolve_source_keys(
    cfg: &PostgisEngineConfig,
    parameters: Option<&[String]>,
) -> Result<Vec<String>, DataServerError> {
    // None or empty-slice both mean "all configured parameters".
    let requested = parameters.unwrap_or(&[]);
    if requested.is_empty() {
        return Ok(cfg
            .parameters
            .iter()
            .map(|p| p.source_key.clone())
            .collect());
    }
    let mut out = Vec::with_capacity(requested.len());
    for name in requested {
        let p = cfg
            .parameters
            .iter()
            .find(|p| &p.name == name)
            .ok_or_else(|| {
                DataServerError::InvalidParameter(format!("unknown parameter: {name}"))
            })?;
        out.push(p.source_key.clone());
    }
    Ok(out)
}

fn source_key_to_param_name(cfg: &PostgisEngineConfig, key: &str) -> String {
    cfg.parameters
        .iter()
        .find(|p| p.source_key == key)
        .map(|p| p.name.clone())
        .unwrap_or_else(|| key.to_string())
}

fn lookup_station_coords(
    meta: &CollectionMeta,
    location_id: &str,
) -> Result<(f64, f64), DataServerError> {
    let station = meta
        .locations
        .iter()
        .find(|l| l.id == location_id)
        .ok_or_else(|| DataServerError::LocationNotFound(location_id.to_string()))?;
    Ok((station.longitude, station.latitude))
}

fn run_queries_sync(pool: &Pool, queries: &[BuiltQuery]) -> Result<Vec<Vec<Row>>, DataServerError> {
    let pool = pool.clone();
    let queries = queries.to_vec();
    block_on_async(async move {
        let client = pool
            .get()
            .await
            .map_err(|e| DataServerError::Engine(format!("pool acquire failed: {e}")))?;
        let mut out = Vec::with_capacity(queries.len());
        for q in &queries {
            let refs = params_as_refs(&q.params);
            let rows = client
                .query(&q.sql, &refs)
                .await
                .map_err(|e| map_pg_error(e, q))?;
            if rows.len() >= MAX_OBSERVATION_ROWS {
                return Err(DataServerError::Engine(format!(
                    "result row count hit cap {MAX_OBSERVATION_ROWS}; narrow bbox or time range"
                )));
            }
            out.push(rows);
        }
        Ok(out)
    })
}

fn resolve_nearest_station(
    pool: &Pool,
    cfg: &PostgisEngineConfig,
    lon: f64,
    lat: f64,
) -> Result<String, DataServerError> {
    let built = build_position(cfg, lon, lat, DEFAULT_POSITION_RADIUS_M)
        .map_err(|e| DataServerError::Engine(format!("build_position: {e}")))?;
    let pool = pool.clone();
    let sql = built.sql;
    let params = built.params;
    block_on_async(async move {
        let client = pool
            .get()
            .await
            .map_err(|e| DataServerError::Engine(format!("pool acquire failed: {e}")))?;
        let refs = params_as_refs(&params);
        let opt = client
            .query_opt(&sql, &refs)
            .await
            .map_err(|e| DataServerError::Engine(format!("query_position failed: {e}")))?;
        let Some(row) = opt else {
            return Err(DataServerError::LocationNotFound(format!(
                "no station within {DEFAULT_POSITION_RADIUS_M:.0} m of ({lon}, {lat})"
            )));
        };
        let id: String = row
            .try_get("id")
            .map_err(|e| DataServerError::Engine(format!("decode station id: {e}")))?;
        Ok(id)
    })
}

fn map_pg_error(e: tokio_postgres::Error, q: &BuiltQuery) -> DataServerError {
    // Keep the SQL template out of the client-facing message; only log it.
    tracing::warn!(
        sql = %q.sql,
        n_params = q.params.len(),
        error = %e,
        "postgis query failed"
    );
    DataServerError::Engine("database query failed".into())
}

// ─── row assembly ──────────────────────────────────────────────────────────

fn assemble_query_result(
    cfg: &PostgisEngineConfig,
    station_id: &str,
    lon: f64,
    lat: f64,
    queries: &[BuiltQuery],
    rows_per_query: Vec<Vec<Row>>,
) -> Result<QueryResult, DataServerError> {
    match &cfg.observations {
        ObservationSchema::Long(_) => {
            let rows = rows_per_query.into_iter().next().unwrap_or_default();
            assemble_long(cfg, station_id, lon, lat, rows)
        }
        ObservationSchema::Wide(_) => {
            let rows = rows_per_query.into_iter().next().unwrap_or_default();
            assemble_wide(cfg, lon, lat, rows)
        }
        ObservationSchema::PerParameter(_) => {
            assemble_per_parameter(cfg, lon, lat, queries, rows_per_query)
        }
    }
}

fn assemble_long(
    cfg: &PostgisEngineConfig,
    _station_id: &str,
    lon: f64,
    lat: f64,
    rows: Vec<Row>,
) -> Result<QueryResult, DataServerError> {
    // Long rows: (time, parameter, value). Group by param, collect times,
    // build per-param NdArray.
    let mut per_param: HashMap<String, ParamSeries> = HashMap::new();
    for row in &rows {
        let t: DateTime<Utc> = row
            .try_get("time")
            .map_err(|e| DataServerError::Engine(format!("decode time: {e}")))?;
        let p: String = row
            .try_get("parameter")
            .map_err(|e| DataServerError::Engine(format!("decode parameter: {e}")))?;
        let v: Option<f64> = row
            .try_get("value")
            .map_err(|e| DataServerError::Engine(format!("decode value: {e}")))?;
        per_param.entry(p).or_default().push((t, v));
    }

    // Union of all times across all parameters, sorted.
    let mut all_times: Vec<DateTime<Utc>> = per_param
        .values()
        .flat_map(|rows| rows.iter().map(|(t, _)| *t))
        .collect();
    all_times.sort_unstable();
    all_times.dedup();

    let domain = DomainDescription::PointSeries {
        x: lon,
        y: lat,
        t: all_times.clone(),
    };

    let mut param_descs = HashMap::new();
    let mut ranges = HashMap::new();
    for (source_key, rows) in per_param {
        let pname = source_key_to_param_name(cfg, &source_key);
        let desc = cfg
            .parameters
            .iter()
            .find(|p| p.name == pname)
            .map(|p| ParameterDescription {
                label: p.label.clone(),
                unit: p.unit.clone(),
                observed_property: p.observed_property.clone(),
            })
            .unwrap_or_else(|| ParameterDescription {
                label: pname.clone(),
                unit: String::new(),
                observed_property: pname.clone(),
            });
        let row_map: HashMap<DateTime<Utc>, Option<f64>> = rows.into_iter().collect();
        let values: Vec<Option<f64>> = all_times
            .iter()
            .map(|t| row_map.get(t).copied().unwrap_or(None))
            .collect();
        let n = values.len();
        ranges.insert(
            pname.clone(),
            NdArray {
                shape: vec![n],
                axis_names: vec!["t".into()],
                values,
            },
        );
        param_descs.insert(pname, desc);
    }

    Ok(QueryResult {
        domain,
        parameters: param_descs,
        ranges,
    })
}

fn assemble_wide(
    cfg: &PostgisEngineConfig,
    lon: f64,
    lat: f64,
    rows: Vec<Row>,
) -> Result<QueryResult, DataServerError> {
    // Wide rows: (time, <param_alias_1>::float8, <param_alias_2>::float8, ...).
    // The BuiltQuery SELECT projected each requested source_key as an alias
    // named after that source_key — walk `cfg.parameters` in order for the
    // column list, but we only know which were requested via the row's own
    // column schema. Iterate row columns to discover the set.
    let Some(first) = rows.first() else {
        // No rows → empty PointSeries.
        return Ok(empty_result(cfg, lon, lat));
    };

    let time_col_idx = first
        .columns()
        .iter()
        .position(|c| c.name() == "time")
        .ok_or_else(|| DataServerError::Engine("wide row missing 'time' column".into()))?;

    // Other columns are parameter aliases.
    let param_keys: Vec<String> = first
        .columns()
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            if i == time_col_idx {
                None
            } else {
                Some(c.name().to_string())
            }
        })
        .collect();

    let mut times: Vec<DateTime<Utc>> = Vec::with_capacity(rows.len());
    let mut per_param: HashMap<String, Vec<Option<f64>>> = param_keys
        .iter()
        .map(|k| (k.clone(), Vec::with_capacity(rows.len())))
        .collect();

    for row in &rows {
        let t: DateTime<Utc> = row
            .try_get::<_, DateTime<Utc>>(time_col_idx)
            .map_err(|e| DataServerError::Engine(format!("decode time: {e}")))?;
        times.push(t);
        for key in &param_keys {
            let v: Option<f64> = row
                .try_get(key.as_str())
                .map_err(|e| DataServerError::Engine(format!("decode {key}: {e}")))?;
            per_param.get_mut(key).unwrap().push(v);
        }
    }

    let domain = DomainDescription::PointSeries {
        x: lon,
        y: lat,
        t: times.clone(),
    };

    let mut param_descs = HashMap::new();
    let mut ranges = HashMap::new();
    for key in &param_keys {
        let pname = source_key_to_param_name(cfg, key);
        let desc = cfg
            .parameters
            .iter()
            .find(|p| p.name == pname)
            .map(|p| ParameterDescription {
                label: p.label.clone(),
                unit: p.unit.clone(),
                observed_property: p.observed_property.clone(),
            })
            .unwrap_or_else(|| ParameterDescription {
                label: pname.clone(),
                unit: String::new(),
                observed_property: pname.clone(),
            });
        let values = per_param.remove(key).unwrap();
        let n = values.len();
        ranges.insert(
            pname.clone(),
            NdArray {
                shape: vec![n],
                axis_names: vec!["t".into()],
                values,
            },
        );
        param_descs.insert(pname, desc);
    }

    Ok(QueryResult {
        domain,
        parameters: param_descs,
        ranges,
    })
}

fn assemble_per_parameter(
    cfg: &PostgisEngineConfig,
    lon: f64,
    lat: f64,
    queries: &[BuiltQuery],
    rows_per_query: Vec<Vec<Row>>,
) -> Result<QueryResult, DataServerError> {
    // Each query carries a `parameter` (source_key) tag. Each row is (time,
    // value). Union times across all queries.
    let mut per_param: HashMap<String, ParamSeries> = HashMap::new();
    for (q, rows) in queries.iter().zip(rows_per_query) {
        let Some(source_key) = q.parameter.as_ref() else {
            return Err(DataServerError::Engine(
                "per_parameter query missing parameter tag".into(),
            ));
        };
        let mut entries = Vec::with_capacity(rows.len());
        for row in &rows {
            let t: DateTime<Utc> = row
                .try_get("time")
                .map_err(|e| DataServerError::Engine(format!("decode time: {e}")))?;
            let v: Option<f64> = row
                .try_get("value")
                .map_err(|e| DataServerError::Engine(format!("decode value: {e}")))?;
            entries.push((t, v));
        }
        per_param.insert(source_key.clone(), entries);
    }

    let mut all_times: Vec<DateTime<Utc>> = per_param
        .values()
        .flat_map(|rows| rows.iter().map(|(t, _)| *t))
        .collect();
    all_times.sort_unstable();
    all_times.dedup();

    if all_times.is_empty() {
        return Ok(empty_result(cfg, lon, lat));
    }

    let domain = DomainDescription::PointSeries {
        x: lon,
        y: lat,
        t: all_times.clone(),
    };

    let mut param_descs = HashMap::new();
    let mut ranges = HashMap::new();
    for (source_key, rows) in per_param {
        let pname = source_key_to_param_name(cfg, &source_key);
        let desc = cfg
            .parameters
            .iter()
            .find(|p| p.name == pname)
            .map(|p| ParameterDescription {
                label: p.label.clone(),
                unit: p.unit.clone(),
                observed_property: p.observed_property.clone(),
            })
            .unwrap_or_else(|| ParameterDescription {
                label: pname.clone(),
                unit: String::new(),
                observed_property: pname.clone(),
            });
        let row_map: HashMap<DateTime<Utc>, Option<f64>> = rows.into_iter().collect();
        let values: Vec<Option<f64>> = all_times
            .iter()
            .map(|t| row_map.get(t).copied().unwrap_or(None))
            .collect();
        let n = values.len();
        ranges.insert(
            pname.clone(),
            NdArray {
                shape: vec![n],
                axis_names: vec!["t".into()],
                values,
            },
        );
        param_descs.insert(pname, desc);
    }

    Ok(QueryResult {
        domain,
        parameters: param_descs,
        ranges,
    })
}

fn empty_result(cfg: &PostgisEngineConfig, lon: f64, lat: f64) -> QueryResult {
    let mut param_descs = HashMap::new();
    let mut ranges = HashMap::new();
    for p in &cfg.parameters {
        let desc = ParameterDescription {
            label: p.label.clone(),
            unit: p.unit.clone(),
            observed_property: p.observed_property.clone(),
        };
        param_descs.insert(p.name.clone(), desc);
        ranges.insert(
            p.name.clone(),
            NdArray {
                shape: vec![0],
                axis_names: vec!["t".into()],
                values: vec![],
            },
        );
    }
    QueryResult {
        domain: DomainDescription::PointSeries {
            x: lon,
            y: lat,
            t: vec![],
        },
        parameters: param_descs,
        ranges,
    }
}

// ─── sync ↔ async bridge ───────────────────────────────────────────────────

fn block_on_async<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => {
            // No current runtime — spin up a temporary one. Used in unit
            // tests outside tokio, never in production where axum always
            // provides one.
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build tokio runtime")
                .block_on(fut)
        }
    }
}

// ─── coord parsing (WKT POINT or "lon,lat") ────────────────────────────────

fn parse_coords(coords: &str) -> Result<(f64, f64), DataServerError> {
    let coords = coords.trim();
    if let Some(inner) = coords
        .strip_prefix("POINT(")
        .or_else(|| coords.strip_prefix("POINT ("))
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.split_whitespace().collect();
        if parts.len() == 2 {
            let lon: f64 = parts[0].parse().map_err(|_| {
                DataServerError::InvalidParameter(format!("invalid longitude: {}", parts[0]))
            })?;
            let lat: f64 = parts[1].parse().map_err(|_| {
                DataServerError::InvalidParameter(format!("invalid latitude: {}", parts[1]))
            })?;
            return Ok((lon, lat));
        }
    }
    let parts: Vec<&str> = coords.split(',').collect();
    if parts.len() == 2 {
        let lon: f64 = parts[0].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("invalid longitude: {}", parts[0]))
        })?;
        let lat: f64 = parts[1].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("invalid latitude: {}", parts[1]))
        })?;
        return Ok((lon, lat));
    }
    Err(DataServerError::InvalidParameter(format!(
        "cannot parse coordinates: {coords}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_coords_wkt_point() {
        assert_eq!(parse_coords("POINT(24.9 60.2)").unwrap(), (24.9, 60.2));
        assert_eq!(parse_coords("POINT (24.9 60.2)").unwrap(), (24.9, 60.2));
    }

    #[test]
    fn parse_coords_comma() {
        assert_eq!(parse_coords("24.9,60.2").unwrap(), (24.9, 60.2));
        assert_eq!(parse_coords(" 24.9 , 60.2 ").unwrap(), (24.9, 60.2));
    }

    #[test]
    fn parse_coords_garbage_rejected() {
        assert!(parse_coords("garbage").is_err());
        assert!(parse_coords("POINT(24.9)").is_err());
    }

    #[test]
    fn resolve_source_keys_expands_none_to_all() {
        use crate::config::ValidatedParameter;
        let cfg = PostgisEngineConfig {
            dsn: "postgres://x/y".into(),
            dsn_was_literal: false,
            pool_size: 4,
            pool_label: None,
            metadata_refresh_secs: 300,
            stations: dummy_stations(),
            observations: dummy_long_obs(),
            parameters: vec![
                ValidatedParameter {
                    name: "t2m".into(),
                    label: "".into(),
                    unit: "°C".into(),
                    observed_property: "air_temperature".into(),
                    source_key: "TEMP".into(),
                },
                ValidatedParameter {
                    name: "ws".into(),
                    label: "".into(),
                    unit: "m/s".into(),
                    observed_property: "wind_speed".into(),
                    source_key: "WIND".into(),
                },
            ],
        };
        let keys = resolve_source_keys(&cfg, None).unwrap();
        assert_eq!(keys, vec!["TEMP", "WIND"]);

        let keys = resolve_source_keys(&cfg, Some(&["t2m".into()])).unwrap();
        assert_eq!(keys, vec!["TEMP"]);

        assert!(resolve_source_keys(&cfg, Some(&["unknown".into()])).is_err());
    }

    fn dummy_stations() -> crate::schema::StationsMapping {
        crate::schema::StationsMapping {
            table: crate::schema::QualifiedTable {
                schema: "public".into(),
                table: "stations".into(),
            },
            id_col: "id".into(),
            label_col: "name".into(),
            geom_col: "geom".into(),
            property_cols: vec![],
            where_clause: None,
        }
    }

    fn dummy_long_obs() -> crate::schema::ObservationSchema {
        crate::schema::ObservationSchema::Long(crate::schema::LongShape {
            table: crate::schema::QualifiedTable {
                schema: "public".into(),
                table: "obs".into(),
            },
            station_fk_col: "station_id".into(),
            time_col: "time".into(),
            time_col_tz: None,
            param_col: "param".into(),
            value_col: "value".into(),
            geom_col: None,
        })
    }
}
