//! Station-symbol map layers for the station shapes (#1063): one WMS/Maps/
//! Tiles layer per configured parameter, each station drawn as a marker
//! coloured by its value.
//!
//! - **Frame.** A frame at `TIME` (floored to `[postgis.map] time_step`,
//!   default 10 minutes, the step the layers advertise over the last 6 h;
//!   no `TIME` ⇒ the latest advertised step) shows each cached
//!   station's latest non-null value in `[TIME - max_age, TIME]`. Stations
//!   without one are left out, or drawn as nodata markers with
//!   `[postgis.map] draw_missing = true`. `resolve_time` and
//!   `resolve_parameter_time` return the same instant the render draws
//!   ([`crate::maps::resolve_window_end`], #507).
//! - **Fetch.** One frame is ONE statement per observation table, the
//!   set-based area statement of #1062 (`<fk> = ANY($1)` over every cached
//!   station, the window, the one parameter), streamed and reduced to a value
//!   a station as rows arrive. The meta-tile loop calls `get_raster_tile` up
//!   to ~200 times a viewport, so frames are cached ([`frame_cache`],
//!   byte-bounded, single-flight) like the lightning windows. The fetch runs
//!   on the render executor's blocking thread, through `Handle::block_on`
//!   (see `maps.rs`), and holds one of the pool key's area permits.
//! - **Late data.** Observations for a past instant arrive late or are
//!   corrected, so a frame can change after it was first drawn. Every
//!   metadata refresh fingerprints the rows the advertised frames read
//!   ([`frame_version`]); [`PostgisEngine::frame_version`] is the
//!   `MapEngine::content_version`, so the API caches and the frame cache
//!   re-key when it moves, and explicit-`TIME` responses revalidate.
//! - **Drawing.** The shared station-symbol helper (#1204):
//!   `ds_render::station_symbols` orders the markers (**the most extreme
//!   value on top**, from the midpoint the server hands the engine as
//!   [`StationSymbols`]), draws them as discs projected per station, never
//!   per output pixel, and caches the frames ([`frame_cache`]);
//!   `ds_core::station_map` holds the settings, the time axis and the
//!   [`LatestValues`] reduction. CSV and BUFR station collections draw
//!   through the same helper.
//! - **Nodata.** A nodata marker's pixels hold [`NODATA_MARKER`]; the server
//!   wraps the layer's colormaps in `ds_render::SymbolColorMap`, which
//!   draws it in the palette's `nodata_color` and keeps the ground
//!   transparent.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use deadpool_postgres::Pool;
use ds_core::error::DataServerError;
use ds_core::map_engine::{OutputCrs, RasterTile};
use ds_core::model::Location;
use ds_core::station_map::{frame_window, LatestValues, MAX_FRAME_ROWS};
use ds_render::station_symbols::{draw_markers, frame_cache, frame_markers, FrameKey, Marker};
pub use ds_render::station_symbols::{station_frame_cache_metrics, StationSymbols, NODATA_MARKER};
use futures::TryStreamExt;

use crate::config::{PostgisEngineConfig, ValidatedParameter};
use crate::engine::{fanout_limiter, fanout_width, map_pg_error, PostgisEngine};
use crate::metadata::CollectionMeta;
use crate::query::{build_area_set, build_frame_fingerprint, params_as_refs};
use crate::schema::ObservationSchema;

/// The parameter a layer draws: the requested one, else the first
/// configured. An unknown name is the client's error.
pub(crate) fn layer_parameter<'a>(
    cfg: &'a PostgisEngineConfig,
    parameter: Option<&str>,
) -> Result<&'a ValidatedParameter, DataServerError> {
    match parameter {
        None => cfg
            .parameters
            .first()
            .ok_or_else(|| DataServerError::Engine("station collection has no parameters".into())),
        Some(name) => cfg
            .parameters
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| DataServerError::InvalidParameter(format!("unknown parameter: {name}"))),
    }
}

/// The station layer's raster for one request: the frame at the resolved
/// instant, drawn over `bbox`.
#[allow(clippy::too_many_arguments)] // mirrors get_raster_tile
pub(crate) fn render(
    engine: &PostgisEngine,
    bbox: [f64; 4],
    width: u32,
    height: u32,
    time: Option<DateTime<Utc>>,
    output_crs: &OutputCrs,
    parameter: Option<&str>,
) -> Result<RasterTile, DataServerError> {
    let parameter = layer_parameter(engine.config(), parameter)?;
    let meta = engine.cache().load();
    let step = crate::maps::time_step(engine.config());
    let end = crate::maps::resolve_window_end(&meta, time, step).ok_or_else(|| {
        DataServerError::Engine("no observation data available for the requested time".into())
    })?;
    let markers = fetch_frame(engine, &meta, parameter, end)?;
    Ok(draw_markers(
        &markers,
        bbox,
        width,
        height,
        output_crs,
        engine.config().station_map.marker_size,
    ))
}

/// Fetch (or reuse) the frame at `end` for `parameter`, its markers in draw
/// order. Single-flight per key: the meta-tiles of one frame share one
/// statement.
fn fetch_frame(
    engine: &PostgisEngine,
    meta: &CollectionMeta,
    parameter: &ValidatedParameter,
    end: DateTime<Utc>,
) -> Result<Arc<Vec<Marker>>, DataServerError> {
    let settings = engine.config().station_map;
    let midpoint = engine
        .station_symbols()
        .map_or(0.0, |s| s.midpoint(&parameter.name));
    let key: FrameKey = (
        engine.instance_id(),
        parameter.name.clone(),
        end,
        engine.frame_version(),
        midpoint.to_bits(),
    );
    frame_cache().get_or_insert_with(&key, || {
        let stations = distinct_stations(&meta.locations);
        if stations.is_empty() {
            return Ok(Arc::new(Vec::new()));
        }
        let window = frame_window(end, settings.max_age);
        let latest = read_frame(engine, &stations, window, &parameter.source_key)?;
        Ok(Arc::new(frame_markers(
            &stations,
            &latest,
            midpoint,
            settings.draw_missing,
        )))
    })
}

/// The cached locations with each id once, at its first place: a stations
/// view without a unique id can list one twice, and its rows match the
/// first.
fn distinct_stations(locations: &[Location]) -> Vec<&Location> {
    let mut seen = std::collections::HashSet::with_capacity(locations.len());
    locations
        .iter()
        .filter(|l| seen.insert(l.id.as_str()))
        .collect()
}

/// Column of a frame row's value: the set-based statement leads with the
/// station id and the time, then `long` has the parameter before its value;
/// `wide` selects the one parameter's column and `per_parameter` its table's
/// value.
fn value_column(cfg: &PostgisEngineConfig) -> usize {
    let time = crate::query::first_observation_column(true);
    match cfg.observations {
        ObservationSchema::Long(_) => time + 2,
        _ => time + 1,
    }
}

/// Read the frame window for `stations` with the #1062 set-based statement
/// and reduce its rows to each station's latest value as they stream in.
fn read_frame(
    engine: &PostgisEngine,
    stations: &[&Location],
    window: (DateTime<Utc>, DateTime<Utc>),
    source_key: &str,
) -> Result<LatestValues, DataServerError> {
    engine.require_validated()?;
    let cfg = engine.config();
    let ids: Arc<Vec<String>> = Arc::new(stations.iter().map(|s| s.id.clone()).collect());
    let mut queries = build_area_set(cfg, &ids, Some(window), &[source_key], MAX_FRAME_ROWS + 1)
        .map_err(|e| DataServerError::Engine(format!("build_area_set: {e}")))?;
    let mut index: HashMap<&str, usize> = HashMap::with_capacity(stations.len());
    for (at, station) in stations.iter().enumerate() {
        index.entry(station.id.as_str()).or_insert(at);
    }
    let time_column = crate::query::first_observation_column(true);
    let value_column = value_column(cfg);
    let pool: &Pool = engine.pool();
    let limiter = fanout_limiter(engine.pool_key_label(), fanout_width(pool));
    let mut latest = LatestValues::new(stations.len());
    let latest_ref = &mut latest;
    let unmatched = crate::maps::block_on_from_blocking(async move {
        let _permit = limiter
            .acquire()
            .await
            .map_err(|_| DataServerError::Engine("fanout limiter closed".into()))?;
        let client = pool
            .get()
            .await
            .map_err(|e| DataServerError::Engine(format!("pool acquire failed: {e}")))?;
        let mut read = 0usize;
        let mut unmatched = 0usize;
        for q in &mut queries {
            q.set_row_limit(MAX_FRAME_ROWS + 1 - read);
            let rows = client
                .query_raw(&q.sql, params_as_refs(&q.params))
                .await
                .map_err(|e| map_pg_error(e, q))?;
            futures::pin_mut!(rows);
            while let Some(row) = rows.try_next().await.map_err(|e| map_pg_error(e, q))? {
                read += 1;
                if read > MAX_FRAME_ROWS {
                    return Err(DataServerError::QueryTooLarge(format!(
                        "a station map frame reads more than {MAX_FRAME_ROWS} observation rows — \
                         shorten [postgis.map] max_age"
                    )));
                }
                let id: &str = row
                    .try_get(0)
                    .map_err(|e| DataServerError::Engine(format!("decode station: {e}")))?;
                let Some(&at) = index.get(id) else {
                    unmatched += 1;
                    continue;
                };
                let time: DateTime<Utc> = row
                    .try_get(time_column)
                    .map_err(|e| DataServerError::Engine(format!("decode time: {e}")))?;
                let value: Option<f64> = row
                    .try_get(value_column)
                    .map_err(|e| DataServerError::Engine(format!("decode value: {e}")))?;
                latest_ref.push(at, time, value);
            }
        }
        Ok(unmatched)
    })??;
    if unmatched > 0 {
        // As on the set-based area path: `<fk>::text` spelt no cached id.
        tracing::warn!(
            collection = %engine.collection_id(),
            unmatched,
            "postgis: station map rows whose station id matched no cached station were skipped"
        );
    }
    Ok(latest)
}

/// The content version of a station collection's map layers after a
/// metadata refresh: a hash of the cached stations and of the fingerprint of
/// every row the advertised frames read, from the first advertised step's
/// window start on ([`build_frame_fingerprint`]). It moves when a row in
/// that range arrives late, is corrected or deleted, when new data moves the
/// advertised steps, and when the station list changes; it holds still
/// over a refresh that found nothing new. Never `0`, which means "never
/// revised" to the API layers. Runs on the background poll runtime.
pub(crate) async fn frame_version(
    cfg: &PostgisEngineConfig,
    pool: &Pool,
    meta: &CollectionMeta,
) -> Result<u64, DataServerError> {
    let stations = distinct_stations(&meta.locations);
    let mut fingerprint = Vec::new();
    let since = meta
        .wms_times
        .first()
        .map(|first| frame_window(*first, cfg.station_map.max_age).0);
    if let (Some(since), false) = (since, stations.is_empty()) {
        let ids: Arc<Vec<String>> = Arc::new(stations.iter().map(|s| s.id.clone()).collect());
        let queries = build_frame_fingerprint(cfg, &ids, since)
            .map_err(|e| DataServerError::Engine(format!("build_frame_fingerprint: {e}")))?;
        let client = pool
            .get()
            .await
            .map_err(|e| DataServerError::Engine(format!("pool acquire failed: {e}")))?;
        for q in &queries {
            let row = client
                .query_one(&q.sql, &params_as_refs(&q.params))
                .await
                .map_err(|e| map_pg_error(e, q))?;
            let mut sums = Vec::with_capacity(row.len());
            let count: i64 = row
                .try_get(0)
                .map_err(|e| DataServerError::Engine(format!("decode count: {e}")))?;
            sums.push(Some(count));
            for i in 1..row.len() {
                let sum: Option<i64> = row
                    .try_get(i)
                    .map_err(|e| DataServerError::Engine(format!("decode fingerprint: {e}")))?;
                sums.push(sum);
            }
            fingerprint.push(sums);
        }
    }
    Ok(version_of(&stations, since, &fingerprint))
}

/// [`frame_version`]'s hash: the stations, in order, with their positions,
/// the fingerprint's start, and every aggregate it read. Odd, so never `0`.
pub(crate) fn version_of(
    stations: &[&Location],
    since: Option<DateTime<Utc>>,
    fingerprint: &[Vec<Option<i64>>],
) -> u64 {
    let mut hasher = DefaultHasher::new();
    stations.len().hash(&mut hasher);
    for s in stations {
        s.id.hash(&mut hasher);
        s.longitude.to_bits().hash(&mut hasher);
        s.latitude.to_bits().hash(&mut hasher);
    }
    since.hash(&mut hasher);
    fingerprint.hash(&mut hasher);
    hasher.finish() | 1
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A stations-only `long` collection of two parameters, `t2m` first.
    pub(crate) fn station_config() -> PostgisEngineConfig {
        use crate::schema::{LocationSource, LongShape, QualifiedTable, StationsMapping};
        let table = |name: &str| QualifiedTable {
            schema: "public".into(),
            table: name.into(),
        };
        PostgisEngineConfig {
            dsn: "postgres://user:pw@127.0.0.1:1/db".into(),
            dsn_was_literal: false,
            pool_size: 2,
            pool_label: None,
            metadata_refresh_secs: 300,
            max_locations: ds_core::config::POSTGIS_DEFAULT_MAX_LOCATIONS,
            location_source: LocationSource::Stations(StationsMapping {
                table: table("stations"),
                id_col: "id".into(),
                label_col: "name".into(),
                geom_col: "geom".into(),
                property_cols: vec![],
                where_clause: None,
            }),
            observations: ObservationSchema::Long(LongShape {
                table: table("obs"),
                station_fk_col: "station_id".into(),
                time_col: "time".into(),
                time_col_tz: None,
                param_col: "param".into(),
                value_col: "value".into(),
                geom_col: None,
            }),
            parameters: vec![
                ValidatedParameter {
                    name: "t2m".into(),
                    label: "Air temperature".into(),
                    unit: "°C".into(),
                    observed_property: "air_temperature".into(),
                    source_key: "TA".into(),
                    filter: Default::default(),
                },
                ValidatedParameter {
                    name: "rh".into(),
                    label: "Humidity".into(),
                    unit: "%".into(),
                    observed_property: "relative_humidity".into(),
                    source_key: "RH".into(),
                    filter: Default::default(),
                },
            ],
            locations_window: None,
            events_default_window: None,
            events_extent_bbox: None,
            station_map: Default::default(),
        }
    }

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn loc(id: &str, lon: f64, lat: f64) -> Location {
        Location {
            id: id.into(),
            label: id.into(),
            latitude: lat,
            longitude: lon,
        }
    }

    /// The content version moves with a late row (a fingerprint count) and
    /// with the station list, holds still when nothing changed, and is never
    /// zero.
    #[test]
    fn version_moves_with_late_rows_and_stations_only() {
        let a = loc("a", 25.0, 60.0);
        let b = loc("b", 24.0, 61.0);
        let since = Some(ts("2026-10-18T00:00:00Z"));
        let before = vec![vec![Some(100), Some(7), Some(-3)]];
        let late = vec![vec![Some(101), Some(9), Some(4)]];
        let base = version_of(&[&a, &b], since, &before);
        assert_ne!(base, 0);
        assert_eq!(base, version_of(&[&a, &b], since, &before));
        assert_ne!(base, version_of(&[&a, &b], since, &late));
        assert_ne!(base, version_of(&[&a], since, &before));
        let moved = loc("b", 24.5, 61.0);
        assert_ne!(base, version_of(&[&a, &moved], since, &before));
        assert_ne!(version_of(&[], None, &[]), 0);
    }

    #[test]
    fn the_layer_parameter_defaults_to_the_first_and_rejects_unknown_names() {
        let cfg = station_config();
        assert_eq!(layer_parameter(&cfg, None).unwrap().name, "t2m");
        assert_eq!(layer_parameter(&cfg, Some("rh")).unwrap().source_key, "RH");
        assert!(matches!(
            layer_parameter(&cfg, Some("nope")),
            Err(DataServerError::InvalidParameter(_))
        ));
    }
}
