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
//! - **Drawing.** Discs of `marker_size` output pixels on a transparent
//!   ground, projected per station through [`OutputCrs::world_to_fraction`],
//!   never per output pixel (Critical Rule 5). Overlaps: **the most extreme
//!   value on top**. Markers are drawn in ascending order of
//!   `|value - midpoint|`, the midpoint being the middle of the layer's
//!   default style range (style `min`/`max`, else the palette's first and
//!   last stops), which the server hands the engine
//!   ([`StationSymbols`]). Nodata markers go under every value.
//! - **Nodata.** A nodata marker's pixels hold [`NODATA_MARKER`]; the
//!   server wraps the layer's colormaps in `ds_render::SymbolColorMap`, which
//!   draws it in the palette's `nodata_color` and keeps the ground
//!   transparent.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Duration, Utc};
use deadpool_postgres::Pool;
use ds_cache::ByteBoundedCache;
use ds_core::error::DataServerError;
use ds_core::map_engine::{OutputCrs, RasterTile};
use ds_core::model::Location;
use futures::TryStreamExt;

use crate::config::{PostgisEngineConfig, ValidatedParameter};
use crate::engine::{fanout_limiter, fanout_width, map_pg_error, PostgisEngine};
use crate::metadata::CollectionMeta;
use crate::query::{build_area_set, build_frame_fingerprint, params_as_refs};
use crate::schema::ObservationSchema;

/// The value a nodata marker's pixels hold: a station drawn without a value
/// (`[postgis.map] draw_missing`). No measurement is `f64::MIN`; the server's
/// `ds_render::SymbolColorMap` matches it exactly and draws it in the
/// palette's `nodata_color`.
pub const NODATA_MARKER: f64 = f64::MIN;

/// Observation rows one frame may read: every row of every station in the
/// window, not one a station, so this is stations × steps in `max_age`.
/// Rows are reduced as they stream in, never held; the cap bounds the
/// database work. 2 000 000 is 20 000 stations at 10-minute steps over
/// 16 hours. Past it the frame is a `QueryTooLarge` naming `max_age`.
pub const MAX_FRAME_ROWS: usize = 2_000_000;

/// The style facts of a station collection's map layers, from the server's
/// style resolution: each parameter layer's midpoint, the middle of its
/// default style's value range. Set by the server whenever it wires the
/// layers ([`PostgisEngine::set_station_symbols`]); its presence also turns
/// on the per-refresh frame fingerprint.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StationSymbols {
    midpoints: HashMap<String, f64>,
}

impl StationSymbols {
    pub fn new(midpoints: HashMap<String, f64>) -> Self {
        Self { midpoints }
    }

    /// The midpoint the overlap order measures from: `0` for a parameter
    /// without a style.
    pub fn midpoint(&self, parameter: &str) -> f64 {
        self.midpoints
            .get(parameter)
            .copied()
            .filter(|m| m.is_finite())
            .unwrap_or(0.0)
    }
}

/// One station to draw: its cached position and its value in the frame,
/// `None` for a nodata marker.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Marker {
    pub lon: f64,
    pub lat: f64,
    pub value: Option<f64>,
}

/// Frame cache key: the engine instance (a rebuilt engine never reads its
/// predecessor's frames), the parameter, the frame instant, the content
/// version the frame was read under, and the midpoint's bits (the draw order
/// is baked into the frame).
type FrameKey = (u64, String, DateTime<Utc>, u64, u64);

/// Cache weight of one frame: the key and one `Marker` per slot the `Vec`
/// allocated.
fn frame_weight(k: &FrameKey, v: &Arc<Vec<Marker>>) -> u64 {
    (k.1.len() + 64 + v.capacity() * std::mem::size_of::<Marker>()) as u64
}

/// Process-global station-frame cache (`MC_STATION_FRAME_CACHE_MB`, default
/// 32 MiB). An entry is about 32 bytes a station, so 20 000 stations is
/// 640 KB a frame; the 1 MiB entry estimate keeps the default cache one
/// shard, so a frame of the 200 000-station ceiling (6.4 MB) still fits.
pub(crate) fn frame_cache() -> &'static ByteBoundedCache<FrameKey, Arc<Vec<Marker>>> {
    static CACHE: OnceLock<ByteBoundedCache<FrameKey, Arc<Vec<Marker>>>> = OnceLock::new();
    CACHE.get_or_init(|| {
        ByteBoundedCache::from_env("MC_STATION_FRAME_CACHE_MB", 32, 1024 * 1024, frame_weight)
    })
}

/// Snapshot of the station-frame cache counters for the `/metrics` scrape.
pub fn station_frame_cache_metrics() -> ds_cache::CacheMetrics {
    frame_cache().metrics()
}

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

/// The observation window a frame at `end` reads: `[end - max_age, end]`,
/// both ends included.
pub(crate) fn frame_window(
    end: DateTime<Utc>,
    max_age: Duration,
) -> (DateTime<Utc>, DateTime<Utc>) {
    (end - max_age, end)
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

/// Each station's latest non-null, finite value in the window, by its place
/// in `stations`: the reduction the frame statement's rows stream through.
#[derive(Debug, Clone, Default)]
pub(crate) struct LatestValues {
    latest: Vec<Option<(DateTime<Utc>, f64)>>,
}

impl LatestValues {
    pub(crate) fn new(stations: usize) -> Self {
        Self {
            latest: vec![None; stations],
        }
    }

    /// One row of station `at`. A null or non-finite value is no value; of
    /// two values the later time wins, and of two at one time the later row,
    /// as the EDR series keeps it.
    pub(crate) fn push(&mut self, at: usize, time: DateTime<Utc>, value: Option<f64>) {
        let Some(value) = value.filter(|v| v.is_finite()) else {
            return;
        };
        let slot = &mut self.latest[at];
        if slot.is_none_or(|(t, _)| time >= t) {
            *slot = Some((time, value));
        }
    }

    pub(crate) fn value(&self, at: usize) -> Option<f64> {
        self.latest[at].map(|(_, v)| v)
    }
}

/// The frame's markers in draw order: nodata markers first (only with
/// `draw_missing`), in station order, then the stations with a value in
/// ascending `|value - midpoint|`, ties in station order, so the most extreme
/// value is drawn last and stays on top.
pub(crate) fn frame_markers(
    stations: &[&Location],
    latest: &LatestValues,
    midpoint: f64,
    draw_missing: bool,
) -> Vec<Marker> {
    let mut missing = Vec::new();
    let mut valued = Vec::new();
    for (at, station) in stations.iter().enumerate() {
        let marker = Marker {
            lon: station.longitude,
            lat: station.latitude,
            value: latest.value(at),
        };
        match marker.value {
            Some(_) => valued.push(marker),
            None if draw_missing => missing.push(marker),
            None => {}
        }
    }
    let distance = |m: &Marker| m.value.map_or(0.0, |v| (v - midpoint).abs());
    // Stable: equal distances keep station order.
    valued.sort_by(|a, b| distance(a).total_cmp(&distance(b)));
    missing.extend(valued);
    missing.shrink_to_fit();
    missing
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

/// Draw `markers` in order, each a disc `marker_size` output pixels across,
/// later markers over earlier ones; the ground stays `None`. A pixel is in
/// a disc when its centre lies within the radius of the station's exact
/// position, and the pixel holding the position always is, so a 1-pixel
/// marker never vanishes between pixel centres. One
/// [`OutputCrs::world_to_fraction`] per station, never per output pixel.
///
/// The lon/lat pre-filter is the meta-tile loop's cost guard, as for the
/// lightning splat: a frame covers the whole network and this runs once per
/// covering tile. Its margin is four times the box's mean degrees per pixel
/// times the radius. A projected output's `bbox` is the WGS84 envelope of
/// its projected box; its local scale can stray further from the mean, so
/// the margin there is also at least half the envelope's span, and its
/// longitude is filtered only where that widened box stays clear of ±180°.
pub(crate) fn draw_markers(
    markers: &[Marker],
    bbox: [f64; 4],
    width: u32,
    height: u32,
    output_crs: &OutputCrs,
    marker_size: u32,
) -> RasterTile {
    let (w, h) = (width as i64, height as i64);
    let mut values: Vec<Option<f64>> = vec![None; (width as usize) * (height as usize)];
    let radius = f64::from(marker_size.max(1)) / 2.0;
    let [west, south, east, north] = bbox;
    let projected = matches!(output_crs, OutputCrs::Projected { .. });
    let reach = (radius + 1.0) * 4.0;
    let mut margin_lon = (east - west) / f64::from(width.max(1)) * reach;
    let mut margin_lat = (north - south) / f64::from(height.max(1)) * reach;
    if projected {
        margin_lon = margin_lon.max((east - west) / 2.0);
        margin_lat = margin_lat.max((north - south) / 2.0);
    }
    let prefilter = bbox.iter().all(|v| v.is_finite());
    // A projected box's envelope is not shifted across ±180°: filter its
    // longitude only where the widened box stays clear of the seam.
    let lon_filter = !projected || (west - margin_lon >= -180.0 && east + margin_lon <= 180.0);
    for marker in markers {
        // A viewport past ±180° (a Maps bbox across the antimeridian arrives
        // unwrapped, #828) shows a station at −175° at 185°: take the world
        // copy that falls in view. A projected output wraps in its forward.
        let lon = if projected || !prefilter {
            marker.lon
        } else if marker.lon < west - margin_lon {
            marker.lon + 360.0
        } else if marker.lon > east + margin_lon {
            marker.lon - 360.0
        } else {
            marker.lon
        };
        if prefilter
            && (marker.lat < south - margin_lat
                || marker.lat > north + margin_lat
                || (lon_filter && (lon < west - margin_lon || lon > east + margin_lon)))
        {
            continue;
        }
        let (fx, fy) = output_crs.world_to_fraction(bbox, lon, marker.lat);
        if !fx.is_finite() || !fy.is_finite() {
            continue;
        }
        let (px, py) = (fx * w as f64, fy * h as f64);
        if px < -radius - 1.0
            || py < -radius - 1.0
            || px > w as f64 + radius + 1.0
            || py > h as f64 + radius + 1.0
        {
            continue;
        }
        let value = marker.value.unwrap_or(NODATA_MARKER);
        let (cx, cy) = (px.floor() as i64, py.floor() as i64);
        let (x0, x1) = ((px - radius).floor() as i64, (px + radius).floor() as i64);
        let (y0, y1) = ((py - radius).floor() as i64, (py + radius).floor() as i64);
        for y in y0.max(0)..=y1.min(h - 1) {
            for x in x0.max(0)..=x1.min(w - 1) {
                let (dx, dy) = (x as f64 + 0.5 - px, y as f64 + 0.5 - py);
                if dx * dx + dy * dy <= radius * radius || (x == cx && y == cy) {
                    values[(y * w + x) as usize] = Some(value);
                }
            }
        }
    }
    RasterTile {
        width,
        height,
        values: values.into(),
    }
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
                },
                ValidatedParameter {
                    name: "rh".into(),
                    label: "Humidity".into(),
                    unit: "%".into(),
                    observed_property: "relative_humidity".into(),
                    source_key: "RH".into(),
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

    fn marker(lon: f64, lat: f64, value: Option<f64>) -> Marker {
        Marker { lon, lat, value }
    }

    /// The painted pixels of `tile`, as `(column, row)`, in row order.
    fn painted(tile: &RasterTile) -> Vec<(u32, u32)> {
        (0..tile.height)
            .flat_map(|row| (0..tile.width).map(move |col| (col, row)))
            .filter(|&(col, row)| {
                tile.values
                    .value_at((row * tile.width + col) as usize)
                    .is_some()
            })
            .collect()
    }

    /// CRS:84 is linear in degrees: 0.1° a pixel, so (2.55°E, 7.55°N) is the
    /// centre of column 25, row 24 (north up), and a 1-pixel marker paints
    /// that pixel alone.
    #[test]
    fn markers_land_on_their_pixels_in_crs84() {
        let tile = draw_markers(
            &[marker(2.55, 7.55, Some(4.0))],
            [0.0, 0.0, 10.0, 10.0],
            100,
            100,
            &OutputCrs::Wgs84,
            1,
        );
        assert_eq!(painted(&tile), vec![(25, 24)]);
        assert_eq!(tile.values.value_at(24 * 100 + 25), Some(4.0));
    }

    /// EPSG:3857 rows follow Mercator northing, not latitude. Over 0°–60°N
    /// in 100 rows, 30.3°N is row 49 in CRS:84 but row 57 in Web Mercator.
    /// Reference northings from PROJ (`cs2cs EPSG:4326 EPSG:3857`): 60°N is
    /// 8 399 737.8898 m, 30.3°N 3 542 170.6276 m, the equator 0.
    #[test]
    fn markers_land_on_their_pixels_in_epsg3857() {
        let (north, station): (f64, f64) = (8_399_737.889_8, 3_542_170.627_6);
        let expected_row = ((north - station) / north * 100.0).floor() as u32;
        assert_eq!(expected_row, 57, "the reference itself");
        let markers = [marker(4.05, 30.3, Some(1.0))];
        let bbox = [0.0, 0.0, 10.0, 60.0];
        let mercator = draw_markers(&markers, bbox, 100, 100, &OutputCrs::WebMercator, 1);
        assert_eq!(painted(&mercator), vec![(40, expected_row)]);
        let linear = draw_markers(&markers, bbox, 100, 100, &OutputCrs::Wgs84, 1);
        assert_eq!(painted(&linear), vec![(40, 49)]);
    }

    /// EPSG:3035 (ETRS89-LAEA) lays pixels out in its own metres. References
    /// from PROJ (`cs2cs EPSG:4326 EPSG:3035`): the projection origin
    /// 52°N 10°E is (4 321 000, 3 210 000) by definition, and 60°N 25°E is
    /// (5 152 827.2539, 4 188 383.6111). 100 m pixels, the box offset by 50 m,
    /// so each station is a pixel centre.
    #[test]
    fn markers_land_on_their_pixels_in_epsg3035() {
        let crs = ds_core::geo::projected_output_crs("EPSG:3035").unwrap();
        for (lon, lat, e, n) in [
            (10.0, 52.0, 4_321_000.0, 3_210_000.0),
            (25.0, 60.0, 5_152_827.253_9, 4_188_383.611_1),
        ] {
            // 100 × 100 pixels of 100 m, the station at the centre of pixel
            // (30, 70): 30.5 pixels east of the west edge, 70.5 below north.
            let west = e - 3_050.0;
            let north = n + 7_050.0;
            let projected = [west, north - 10_000.0, west + 10_000.0, north];
            let output = OutputCrs::Projected {
                crs: crs.clone(),
                bbox: projected,
            };
            let envelope = ds_core::geo::wgs84_envelope(&crs, projected).unwrap();
            let tile = draw_markers(
                &[marker(lon, lat, Some(2.0))],
                envelope,
                100,
                100,
                &output,
                1,
            );
            assert_eq!(painted(&tile), vec![(30, 70)], "{lon}°E {lat}°N");
        }
        // A 6-pixel marker one pixel west of a box still paints its edge,
        // past the pre-filter on the box's WGS84 envelope; one far away
        // does not.
        let projected = [4_321_100.0, 3_205_000.0, 4_331_100.0, 3_215_000.0];
        let output = OutputCrs::Projected {
            crs: crs.clone(),
            bbox: projected,
        };
        let envelope = ds_core::geo::wgs84_envelope(&crs, projected).unwrap();
        let markers = [marker(10.0, 52.0, Some(1.0)), marker(30.0, 52.0, Some(2.0))];
        let pixels = painted(&draw_markers(&markers, envelope, 100, 100, &output, 6));
        assert!(pixels.contains(&(0, 50)), "{pixels:?}");
        assert!(pixels.iter().all(|&(col, _)| col <= 1), "{pixels:?}");
    }

    /// A 6-pixel marker centred on a pixel corner is the disc 6 pixels
    /// across around it: 32 pixels in 6 rows, 6 across the middle four and 4
    /// across the top and bottom ones.
    #[test]
    fn a_six_pixel_marker_is_a_disc_six_pixels_across() {
        let tile = draw_markers(
            &[marker(5.0, 5.0, Some(1.0))],
            [0.0, 0.0, 10.0, 10.0],
            100,
            100,
            &OutputCrs::Wgs84,
            6,
        );
        let pixels = painted(&tile);
        assert_eq!(pixels.len(), 32);
        let row = |r: u32| pixels.iter().filter(|p| p.1 == r).count();
        let rows: Vec<usize> = (46..=53).map(row).collect();
        assert_eq!(rows, vec![0, 4, 6, 6, 6, 6, 4, 0]);
    }

    /// The overlap rule: the value farthest from the midpoint is drawn last
    /// and covers the others; nodata markers go under every value.
    #[test]
    fn the_most_extreme_value_is_drawn_on_top() {
        let stations = [
            loc("mild", 5.0, 5.0),
            loc("cold", 5.0, 5.0),
            loc("silent", 5.0, 5.0),
            loc("hot", 5.0, 5.0),
        ];
        let stations: Vec<&Location> = stations.iter().collect();
        let mut latest = LatestValues::new(4);
        let t = ts("2026-10-18T06:00:00Z");
        latest.push(0, t, Some(12.0));
        latest.push(1, t, Some(-15.0));
        latest.push(3, t, Some(30.0));
        // Midpoint 10: |12-10| = 2, |-15-10| = 25, |30-10| = 20.
        let markers = frame_markers(&stations, &latest, 10.0, true);
        let order: Vec<Option<f64>> = markers.iter().map(|m| m.value).collect();
        assert_eq!(order, vec![None, Some(12.0), Some(30.0), Some(-15.0)]);
        let tile = draw_markers(
            &markers,
            [0.0, 0.0, 10.0, 10.0],
            10,
            10,
            &OutputCrs::Wgs84,
            6,
        );
        assert_eq!(tile.values.value_at(5 * 10 + 5), Some(-15.0));
        // Midpoint 0: 30 is now the extreme.
        let markers = frame_markers(&stations, &latest, 0.0, true);
        let tile = draw_markers(
            &markers,
            [0.0, 0.0, 10.0, 10.0],
            10,
            10,
            &OutputCrs::Wgs84,
            6,
        );
        assert_eq!(tile.values.value_at(5 * 10 + 5), Some(30.0));
    }

    /// A station without a value in the window is left out; with
    /// `draw_missing` it is a nodata marker holding [`NODATA_MARKER`].
    #[test]
    fn a_station_without_a_value_is_left_out_or_drawn_as_nodata() {
        let stations = [loc("a", 2.5, 2.5), loc("b", 7.5, 7.5)];
        let stations: Vec<&Location> = stations.iter().collect();
        let mut latest = LatestValues::new(2);
        // `b` reported only nulls and a NaN: no value.
        let t = ts("2026-10-18T06:00:00Z");
        latest.push(0, t, Some(1.5));
        latest.push(1, t, None);
        latest.push(1, t, Some(f64::NAN));
        let markers = frame_markers(&stations, &latest, 0.0, false);
        assert_eq!(markers, vec![marker(2.5, 2.5, Some(1.5))]);
        let markers = frame_markers(&stations, &latest, 0.0, true);
        assert_eq!(
            markers,
            vec![marker(7.5, 7.5, None), marker(2.5, 2.5, Some(1.5))]
        );
        let tile = draw_markers(
            &markers,
            [0.0, 0.0, 10.0, 10.0],
            10,
            10,
            &OutputCrs::Wgs84,
            1,
        );
        assert_eq!(tile.values.value_at(2 * 10 + 7), Some(NODATA_MARKER));
        assert_eq!(tile.values.value_at(7 * 10 + 2), Some(1.5));
    }

    /// The latest value in the window wins whatever order rows arrive in,
    /// and a later null does not erase an earlier value. The window is
    /// `[end - max_age, end]`, which is what the statement reads.
    #[test]
    fn the_latest_value_in_the_window_wins() {
        let mut latest = LatestValues::new(1);
        latest.push(0, ts("2026-10-18T05:50:00Z"), Some(3.0));
        latest.push(0, ts("2026-10-18T05:30:00Z"), Some(1.0));
        latest.push(0, ts("2026-10-18T06:00:00Z"), None);
        assert_eq!(latest.value(0), Some(3.0));
        assert_eq!(
            frame_window(ts("2026-10-18T06:00:00Z"), Duration::hours(1)),
            (ts("2026-10-18T05:00:00Z"), ts("2026-10-18T06:00:00Z"))
        );
    }

    /// A marker just past the box still paints its part of the disc, and
    /// one far away is skipped by the pre-filter.
    #[test]
    fn edge_markers_paint_partial_discs_and_far_ones_nothing() {
        let tile = draw_markers(
            &[marker(10.01, 5.0, Some(1.0)), marker(40.0, 5.0, Some(2.0))],
            [0.0, 0.0, 10.0, 10.0],
            100,
            100,
            &OutputCrs::Wgs84,
            6,
        );
        let pixels = painted(&tile);
        assert!(!pixels.is_empty());
        assert!(pixels.iter().all(|&(col, _)| col >= 97));
        assert!(pixels
            .iter()
            .all(|&(c, r)| tile.values.value_at((r * 100 + c) as usize) == Some(1.0)));
    }

    /// #828: a viewport past ±180°, how OGC API Maps passes the seam test box
    /// 170,10,-170,20, shows stations on both sides of the antimeridian.
    #[test]
    fn markers_on_both_sides_of_the_antimeridian_in_a_viewport_past_180() {
        let markers = [
            marker(175.5, 15.5, Some(1.0)),
            marker(-175.5, 15.5, Some(2.0)),
        ];
        for bbox in [[170.0, 10.0, 190.0, 20.0], [-190.0, 10.0, -170.0, 20.0]] {
            for output_crs in [OutputCrs::Wgs84, OutputCrs::WebMercator] {
                let tile = draw_markers(&markers, bbox, 20, 10, &output_crs, 1);
                assert_eq!(
                    painted(&tile),
                    vec![(5, 4), (14, 4)],
                    "{bbox:?} {output_crs:?}"
                );
            }
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

    /// #1052-style weight: every allocated marker slot counts.
    #[test]
    fn frame_weight_counts_allocated_markers() {
        let key: FrameKey = (1, "t2m".into(), ts("2026-10-18T06:00:00Z"), 3, 0);
        let frame = Arc::new(Vec::<Marker>::with_capacity(1000));
        assert!(frame_weight(&key, &frame) >= (1000 * std::mem::size_of::<Marker>()) as u64);
    }
}
