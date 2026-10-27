//! `MapEngine` for the events shape (#504): the age-colored lightning layer.
//! The station shapes' symbol layers (#1063) live in `station_map.rs`; they
//! share this module's advertised steps and TIME resolution, on their own
//! `[postgis.map] time_step` ([`time_step`]).
//!
//! Each WMS/Maps/Tiles frame renders the strikes of a sliding window ending
//! at the (quantized) requested TIME, splatted as fixed-size discs whose
//! pixel VALUE is the strike age in minutes — the `lightning_age` colormap
//! ramps fresh strikes near-white through orange/red to dark violet at the
//! window edge. Vector→raster follows the CAP-engine precedent: strikes are
//! projected per-VERTEX through [`OutputCrs::world_to_fraction`], never per
//! output pixel (root CLAUDE.md Critical Rule 5).
//!
//! ## DB access contract
//!
//! `get_raster_tile` is dispatched inside `spawn_blocking` by every raster
//! API, where the engine's usual `block_in_place` bridge PANICS (root
//! CLAUDE.md Critical Rule 7). The window fetch therefore drives the pool
//! via `Handle::block_on` (valid on a `spawn_blocking` pool thread — the
//! ODIM PVOL pixel-fetch pattern), falling back to a throwaway
//! current-thread runtime outside any runtime (unit tests).
//!
//! One frame is ONE whole-extent DB fetch, cached: the meta-tile loop calls
//! `get_raster_tile` up to ~200 times per viewport, so per-tile queries
//! would hammer the pool. [`strike_window_cache`] (byte-bounded, #480,
//! single-flight) holds decoded strike windows keyed
//! `(collection, start, end)`; strikes are immutable so settled windows
//! never go stale. Staleness bounds for the live edge: an EXPLICIT TIME
//! frame is bounded by the 1-minute quantum (a new minute ⇒ a new key),
//! while a TIME-omitted request resolves to the latest ADVERTISED step —
//! rebuilt on the metadata refresh cadence (`metadata_refresh_secs`,
//! default 300; set 60 for a live lightning collection). Real-time
//! clients should pass explicit TIME, like the animation clients do.

use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Duration, Utc};
use ds_cache::ByteBoundedCache;
use ds_core::error::DataServerError;
use ds_core::feature_pick::{FeaturePick, PickedFeatures};
use ds_core::map_engine::{MapEngine, OutputCrs, ParameterInfo, RasterInfo, RasterTile};
use tokio_postgres::Row;

use crate::config::PostgisEngineConfig;
use crate::engine::PostgisEngine;
use crate::metadata::CollectionMeta;
use crate::query::{build_events_window, params_as_refs, BuiltQuery, WindowAttrs};
use crate::schema::EventsShape;

/// One strike ready to splat.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Strike {
    pub time: DateTime<Utc>,
    pub lon: f64,
    pub lat: f64,
}

/// The events layer's TIME quantum: requested instants floor to the minute,
/// so cache keys stay bounded and animation frames are deterministic. The
/// live frame's staleness is bounded by this quantum plus ingest lag. The
/// station layers step on `[postgis.map] time_step` instead (#1063).
pub(crate) const EVENTS_TIME_STEP: Duration = Duration::minutes(1);

// The advertised TIME dimension spans the most recent 6 h
// (`ds_core::config::STATION_MAP_TIME_WINDOW`: 361 one-minute entries for a
// full events window, both endpoints inclusive). Older frames still render
// via an explicit TIME — membership in the advertised list is not required.
// The axis helpers are shared with the CSV and BUFR station layers (#1204).
pub(crate) use ds_core::station_map::advertised_times as build_wms_times;
#[cfg(test)]
use ds_core::station_map::quantize_time;

/// Fixed splat radius in output pixels. Constant across zoom — classic
/// lightning markers stay marker-sized rather than scaling with the map
/// (the #504 symbol-size question, resolved to the cheap option).
const SYMBOL_RADIUS_PX: i64 = 2;

/// Whether a strike's disc, drawn around the strike's own pixel, covers the
/// pixel `(dx, dy)` away from it: the one home of the disc's shape, which
/// [`splat_strikes`] paints and a WMS GetFeatureInfo pick grows the picked
/// pixel by ([`pick_region`], #1100).
fn disc_covers(dx: i64, dy: i64) -> bool {
    dx * dx + dy * dy <= SYMBOL_RADIUS_PX * SYMBOL_RADIUS_PX
}

/// The ground area holding every strike whose drawn disc covers the picked
/// pixel, as WMS GetFeatureInfo picks them (#1100): the pixels the disc
/// reaches the picked pixel from, on the map's own grid
/// ([`FeaturePick::grown_ring`], row half-widths from [`disc_covers`]),
/// then, across the antimeridian, its copy a turn back. A click anywhere on
/// a drawn disc finds its strike, and a click past it does not.
pub(crate) fn pick_region(pick: &FeaturePick<'_>) -> Vec<Vec<[f64; 2]>> {
    let half_width = |dy: i64| {
        (0..=SYMBOL_RADIUS_PX)
            .rev()
            .find(|&dx| disc_covers(dx, dy))
            .unwrap_or(0)
    };
    FeaturePick::with_turn_copies(pick.grown_ring(SYMBOL_RADIUS_PX as u32, half_width))
}

/// Hard cap on strikes fetched per window — bounds the cache entry and the
/// splat loop. `ORDER BY time DESC` in the fetch means a truncated window
/// keeps the NEWEST strikes. 200k ≈ 5× the heaviest observed storm-day.
const MAX_WINDOW_STRIKES: usize = 200_000;

/// The advertised parameter id of the derived age layer.
pub const LIGHTNING_AGE_PARAMETER: &str = "lightning_age";

/// The map metadata of a refreshed snapshot (#1063): the events shape's one
/// age layer, or a station collection's layer per configured parameter, the
/// first the default. Built per metadata refresh so `raster_info_shared` is
/// an `Arc` clone.
pub(crate) fn build_raster_info(
    cfg: &PostgisEngineConfig,
    spatial_extent: Option<[f64; 4]>,
    wms_times: &[DateTime<Utc>],
) -> RasterInfo {
    let (parameter, unit, parameters) = if cfg.events().is_some() {
        (
            LIGHTNING_AGE_PARAMETER.to_string(),
            "min".to_string(),
            vec![],
        )
    } else {
        let first = cfg.parameters.first();
        (
            first.map(|p| p.name.clone()).unwrap_or_default(),
            first.map(|p| p.unit.clone()).unwrap_or_default(),
            cfg.parameters
                .iter()
                .map(|p| ParameterInfo {
                    name: p.name.clone(),
                    title: if p.label.is_empty() {
                        p.name.clone()
                    } else {
                        p.label.clone()
                    },
                    unit: p.unit.clone(),
                })
                .collect(),
        )
    };
    RasterInfo {
        native_crs: "CRS:84".to_string(),
        spatial_extent,
        times: wms_times.to_vec(),
        parameter,
        unit,
        parameters,
        vertical: None,
        grid_size: None,
        layer_subtitle: None,
        reference_times: Vec::new(),
        native_grid: None,
    }
}

/// The collection's TIME step: one minute for the events layer, the
/// configured `[postgis.map] time_step` (default 10 minutes) for the station
/// layers (#1063). Whole seconds, at least one minute (validated at load).
pub(crate) fn time_step(cfg: &PostgisEngineConfig) -> Duration {
    if cfg.events().is_some() {
        EVENTS_TIME_STEP
    } else {
        cfg.station_map.time_step
    }
}

/// The window end `get_raster_tile` renders for a requested time — the
/// #507 cache-key authority for this engine: explicit TIME floors to the
/// step ([`time_step`]); no TIME ⇒ the latest advertised step. `None` only
/// when the collection has no data yet. The selection is the shared
/// `ds_core::station_map::resolve_frame_end` (#1204).
pub(crate) fn resolve_window_end(
    meta: &CollectionMeta,
    time: Option<DateTime<Utc>>,
    step: Duration,
) -> Option<DateTime<Utc>> {
    ds_core::station_map::resolve_frame_end(&meta.wms_times, time, step)
}

/// Process-global strike-window cache. Global (not per-engine) so a reload
/// keeps warm windows; the source component of the key isolates collections
/// sharing the process AND invalidates naturally when a reload repoints a
/// collection id at a different table or database — the collection id alone
/// would keep serving the old table's cached windows until eviction.
/// Weight: see [`strike_window_weight`].
type WindowKey = (String, DateTime<Utc>, DateTime<Utc>);

/// The cache-key source component: collection id + qualified table + pool
/// identity (`<user>@<host>:<port>/<db>` or the configured `pool_label`).
fn window_key_source(engine: &PostgisEngine, shape: &EventsShape) -> String {
    format!(
        "{}|{}.{}|{}",
        engine.collection_id(),
        shape.table.schema,
        shape.table.table,
        engine.pool_key_label()
    )
}

/// Cache weight of one strike window: the key, the `Arc` header, and one
/// `Strike` per slot the `Vec` **allocated** (#1052). The window reserves
/// one slot per returned row and skips rows with an empty geometry, so the
/// length can fall short of what the entry holds.
fn strike_window_weight(k: &WindowKey, v: &Arc<Vec<Strike>>) -> u64 {
    (k.0.len() + 32 + v.capacity() * std::mem::size_of::<Strike>()) as u64
}

pub fn strike_window_cache() -> &'static ByteBoundedCache<WindowKey, Arc<Vec<Strike>>> {
    static CACHE: OnceLock<ByteBoundedCache<WindowKey, Arc<Vec<Strike>>>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let (bytes, source) = ds_cache::plan::env_bytes("MC_LIGHTNING_STRIKE_CACHE_MB", 32);
        ByteBoundedCache::new(bytes, 64 * 1024, strike_window_weight).registered(
            "lightning_strike_cache",
            ds_cache::plan::Scope::Engine("postgis"),
            source,
        )
    })
}

/// Snapshot of the strike-window cache counters for the `/metrics` scrape.
pub fn strike_window_cache_metrics() -> ds_cache::CacheMetrics {
    strike_window_cache().metrics()
}

/// Run one built query on a pooled connection from a `spawn_blocking`
/// thread. `Handle::block_on` is the ONLY valid bridge there — the engine's
/// `block_in_place` helper panics off a runtime worker. Outside any runtime
/// (unit tests) a throwaway current-thread runtime serves.
fn run_query_from_blocking(
    pool: &deadpool_postgres::Pool,
    built: &BuiltQuery,
) -> Result<Vec<Row>, DataServerError> {
    block_on_from_blocking(async {
        let client = pool
            .get()
            .await
            .map_err(|e| DataServerError::Engine(format!("pool acquire failed: {e}")))?;
        let refs = params_as_refs(&built.params);
        client
            .query(&built.sql, &refs)
            .await
            .map_err(|e| crate::engine::map_pg_error(e, built))
    })?
}

/// Drive `fut` to completion from a `spawn_blocking` thread, the bridge
/// [`run_query_from_blocking`] and the station-map frame fetch (#1063)
/// share. `Err` only when no runtime exists and none can be built.
pub(crate) fn block_on_from_blocking<F: std::future::Future>(
    fut: F,
) -> Result<F::Output, DataServerError> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => Ok(handle.block_on(fut)),
        Err(_) => Ok(tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| DataServerError::Engine(format!("runtime build failed: {e}")))?
            .block_on(fut)),
    }
}

/// Fetch (or reuse) the strike window `(start, end]` for this collection.
/// Single-flight per key: concurrent meta-tile renders of one frame share
/// one DB query.
fn fetch_window(
    engine: &PostgisEngine,
    shape: &EventsShape,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Arc<Vec<Strike>>, DataServerError> {
    engine.require_validated()?;
    let key: WindowKey = (window_key_source(engine, shape), start, end);
    strike_window_cache().get_or_insert_with(&key, || {
        let built = build_events_window(
            shape,
            (start, end),
            MAX_WINDOW_STRIKES + 1,
            WindowAttrs::Omit,
        )
        .map_err(|e| DataServerError::Engine(format!("build_events_window: {e}")))?;
        let rows = run_query_from_blocking(engine.pool(), &built)?;
        let truncated = rows.len() > MAX_WINDOW_STRIKES;
        let mut strikes = Vec::with_capacity(rows.len().min(MAX_WINDOW_STRIKES));
        // Rows arrive newest-first (truncation keeps the newest); take the
        // cap then reverse to ascending so the splat paints newest LAST.
        for row in rows.iter().take(MAX_WINDOW_STRIKES) {
            engine.check_geometry_row(row)?;
            let time: DateTime<Utc> = row
                .try_get("time")
                .map_err(|e| DataServerError::Engine(format!("decode strike time: {e}")))?;
            let lon: Option<f64> = row
                .try_get("lon")
                .map_err(|e| DataServerError::Engine(format!("decode strike lon: {e}")))?;
            let lat: Option<f64> = row
                .try_get("lat")
                .map_err(|e| DataServerError::Engine(format!("decode strike lat: {e}")))?;
            // Degenerate geometries (ST_X of POINT EMPTY) skip the strike,
            // mirroring the EDR decode path.
            let (Some(lon), Some(lat)) = (lon, lat) else {
                continue;
            };
            strikes.push(Strike { time, lon, lat });
        }
        strikes.reverse();
        if truncated {
            tracing::warn!(
                collection = %engine.collection_id(),
                window_start = %start,
                window_end = %end,
                cap = MAX_WINDOW_STRIKES,
                "lightning map window truncated to the newest strikes — widen \
                 MC_LIGHTNING_STRIKE_CACHE_MB has no effect on this cap; the \
                 window simply holds more events than the splat cap"
            );
        }
        Ok(Arc::new(strikes))
    })
}

/// Pure splat: paint each strike as a [`SYMBOL_RADIUS_PX`] disc whose value
/// is its age in minutes at `window_end`. Strikes must be ascending by time
/// — the newest paints last and wins overlaps. Strikes projecting outside
/// the canvas (plus symbol margin) are skipped; the per-strike projection is
/// one `world_to_fraction` call (per-vertex, never per output pixel).
///
/// The lon/lat pre-filter is the tile-loop cost guard: the window slice is
/// whole-extent while a meta-tile covers a sliver of it, and this function
/// runs once per covering tile (~200/viewport) — without the cheap bbox
/// reject first, a busy storm frame would be tiles × strikes projections.
/// The margin converts the symbol radius to degrees generously (4× the
/// linear per-pixel estimate) so partial discs at tile edges still paint —
/// Mercator's y-stretch stays well under 4× at the ±85° tile-grid clamp.
/// Projected outputs (LAEA/LCC/stereographic) SKIP the pre-filter: their
/// local degrees-per-pixel can exceed any fixed multiple of the bbox
/// average near high-distortion regions, and they never take the
/// meta-tiled path — one direct render per request makes the linear scan
/// over the window affordable.
pub(crate) fn splat_strikes(
    strikes: &[Strike],
    window_end: DateTime<Utc>,
    bbox: [f64; 4],
    width: u32,
    height: u32,
    output_crs: &OutputCrs,
) -> RasterTile {
    let (w, h) = (width as i64, height as i64);
    let mut values: Vec<Option<f64>> = vec![None; (width as usize) * (height as usize)];
    let [west, south, east, north] = bbox;
    let prefilter = !matches!(output_crs, OutputCrs::Projected { .. });
    let margin_lon = ((east - west) / width.max(1) as f64) * SYMBOL_RADIUS_PX as f64 * 4.0;
    let margin_lat = ((north - south) / height.max(1) as f64) * SYMBOL_RADIUS_PX as f64 * 4.0;
    for s in strikes {
        // A viewport reaching past ±180° (a Maps bbox crossing the
        // antimeridian arrives unwrapped, #828) shows a strike at −175° at
        // 185°: take the world copy of the strike that falls in view. A
        // projected output wraps longitude in its own forward transform.
        let lon = if !prefilter {
            s.lon
        } else if s.lon < west - margin_lon {
            s.lon + 360.0
        } else if s.lon > east + margin_lon {
            s.lon - 360.0
        } else {
            s.lon
        };
        if prefilter
            && (lon < west - margin_lon
                || lon > east + margin_lon
                || s.lat < south - margin_lat
                || s.lat > north + margin_lat)
        {
            continue;
        }
        let (fx, fy) = output_crs.world_to_fraction(bbox, lon, s.lat);
        if !fx.is_finite() || !fy.is_finite() {
            continue;
        }
        let cx = (fx * w as f64).floor() as i64;
        let cy = (fy * h as f64).floor() as i64;
        if cx < -SYMBOL_RADIUS_PX
            || cy < -SYMBOL_RADIUS_PX
            || cx >= w + SYMBOL_RADIUS_PX
            || cy >= h + SYMBOL_RADIUS_PX
        {
            continue;
        }
        let age_min = (window_end - s.time).num_seconds().max(0) as f64 / 60.0;
        for dy in -SYMBOL_RADIUS_PX..=SYMBOL_RADIUS_PX {
            for dx in -SYMBOL_RADIUS_PX..=SYMBOL_RADIUS_PX {
                if !disc_covers(dx, dy) {
                    continue; // disc, not square
                }
                let (px, py) = (cx + dx, cy + dy);
                if px < 0 || py < 0 || px >= w || py >= h {
                    continue;
                }
                values[(py * w + px) as usize] = Some(age_min);
            }
        }
    }
    RasterTile {
        width,
        height,
        values: values.into(),
    }
}

impl MapEngine for PostgisEngine {
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        self.require_validated()?;
        let Some(shape) = self.config().events() else {
            // Station shapes: one symbol layer per parameter (#1063).
            return crate::station_map::render(
                self, bbox, width, height, time, output_crs, parameter,
            );
        };
        let shape = shape.clone();
        let meta = self.cache().load();
        let end = resolve_window_end(&meta, time, EVENTS_TIME_STEP).ok_or_else(|| {
            DataServerError::Engine("no event data available for the requested time".into())
        })?;
        let window = self
            .config()
            .events_default_window
            .unwrap_or_else(|| Duration::hours(crate::config::DEFAULT_EVENTS_WINDOW_HOURS));
        let strikes = fetch_window(self, &shape, end - window, end)?;
        Ok(splat_strikes(
            &strikes, end, bbox, width, height, output_crs,
        ))
    }

    fn raster_info(&self) -> RasterInfo {
        (*self.cache().load().raster_info).clone()
    }

    /// O(1): the snapshot's `Arc`, rebuilt per metadata refresh (#211).
    fn raster_info_shared(&self) -> Arc<RasterInfo> {
        Arc::clone(&self.cache().load().raster_info)
    }

    fn resolve_time(
        &self,
        time: Option<DateTime<Utc>>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        // The #507 cache-key authority: the SAME `resolve_window_end` the
        // render path uses (TIME floored to the collection's step / latest
        // advertised step), for the events layer and the station layers
        // alike. Falls back to the requested time when there is no data
        // yet — the render errors and caches nothing.
        resolve_window_end(&self.cache().load(), time, time_step(self.config())).or(time)
    }

    /// Every station layer shares one time axis and frame selection
    /// (#1063): [`Self::resolve_time`] for any parameter.
    fn resolve_parameter_time(
        &self,
        _parameter: Option<&str>,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        self.resolve_time(time, reference_time)
    }

    /// Strikes are immutable (`0`); a station frame can change under its
    /// instant when rows arrive late or are corrected, so the station layers
    /// report the fingerprint the metadata refresh keeps (#1063).
    fn content_version(&self) -> u64 {
        if self.config().events().is_some() {
            0
        } else {
            self.frame_version()
        }
    }

    /// A station marker shows the latest value within `max_age`, which the
    /// EDR position query (an exact instant at the nearest station) does
    /// not answer, so GetFeatureInfo must not read it there (#1063).
    fn layer_shows_values(&self, _parameter: &str) -> bool {
        self.config().events().is_some()
    }

    /// The events layer draws strikes, which WMS GetFeatureInfo picks
    /// (#1100); a station layer's markers are not picked.
    fn layer_picks_features(&self, _parameter: &str) -> bool {
        self.config().events().is_some()
    }

    /// The strikes a frame at `pick.time` draws inside the picked pixel's
    /// footprint ([`PostgisEngine::pick_events`]). Runs on the query runtime,
    /// through the engine's `block_in_place` bridge, not this module's
    /// `spawn_blocking` one.
    fn pick_features(&self, pick: &FeaturePick<'_>) -> Result<PickedFeatures, DataServerError> {
        match self.config().events() {
            Some(shape) => self.pick_events(shape, pick),
            None => Err(DataServerError::InvalidParameter(
                "a station layer draws no features to pick".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// #1052: the weight counts the slots the window reserved, not only
    /// the strikes left after empty-geometry rows were skipped.
    #[test]
    fn strike_window_weight_counts_reserved_slots() {
        let key: WindowKey = (
            "lightning|public.strikes|pool".into(),
            ts("2026-07-11T20:00:00Z"),
            ts("2026-07-11T20:20:00Z"),
        );
        // Reserved for 100 rows; 40 had an empty geometry and were skipped.
        let mut strikes = Vec::with_capacity(100);
        for _ in 0..60 {
            strikes.push(Strike {
                time: ts("2026-07-11T20:10:00Z"),
                lon: 25.0,
                lat: 60.0,
            });
        }
        let window = Arc::new(strikes);
        let allocated = (window.capacity() * std::mem::size_of::<Strike>()) as u64;
        let weight = strike_window_weight(&key, &window);
        assert!(
            weight >= allocated,
            "weight {weight} B under-counts a {allocated} B allocation"
        );
    }

    /// #1100: a WMS GetFeatureInfo pick finds a strike exactly when the
    /// render paints the picked pixel with its disc: a click on the disc's
    /// edge pixel, not its own, finds it, and a click just past the disc
    /// does not. Checked against `splat_strikes` itself, every strike pixel
    /// within four of the picked one, in CRS:84 and EPSG:3857.
    #[test]
    fn a_pick_holds_the_strikes_whose_disc_covers_the_pixel() {
        use ds_core::feature::Geometry;
        use ds_core::feature_pick::{FeaturePick, PickMap};
        let end = ts("2026-07-11T20:00:00Z");
        let bbox = [24.0, 59.0, 26.0, 61.0];
        for output_crs in [OutputCrs::Wgs84, OutputCrs::WebMercator] {
            // Pixel (10, 10) of a 20 × 20 map, centred as api-wms centres it.
            let (lon, lat) = output_crs.project_node(bbox, 10.5 / 20.0, 10.5 / 20.0);
            let pick = FeaturePick {
                parameter: Some(LIGHTNING_AGE_PARAMETER),
                map: PickMap {
                    output_crs: &output_crs,
                    wgs84_bbox: bbox,
                    width: 20,
                    height: 20,
                },
                pixel: (10, 10),
                point: (lon, lat),
                time: Some(end),
                reference_time: None,
                z: None,
                limit: 10,
            };
            let rings = pick_region(&pick);
            assert_eq!(rings.len(), 1);
            let region = Geometry::Polygon {
                exterior: rings[0].clone(),
                holes: Vec::new(),
            };
            let mut found = Vec::new();
            for dy in -4..=4i64 {
                for dx in -4..=4i64 {
                    // A strike at the centre of the pixel (dx, dy) away.
                    let (x, y) = ((10 + dx) as f64 + 0.5, (10 + dy) as f64 + 0.5);
                    let (s_lon, s_lat) = output_crs.project_node(bbox, x / 20.0, y / 20.0);
                    let strike = Strike {
                        time: end,
                        lon: s_lon,
                        lat: s_lat,
                    };
                    let tile = splat_strikes(&[strike], end, bbox, 20, 20, &output_crs);
                    let painted = tile.values.value_at(10 * 20 + 10).is_some();
                    assert_eq!(
                        region.contains(s_lon, s_lat),
                        painted,
                        "{output_crs:?} ({dx}, {dy})"
                    );
                    if painted {
                        found.push((dx, dy));
                    }
                }
            }
            assert_eq!(found.len(), 13, "{output_crs:?}");
            // The disc's edge pixels pick it; the pixels just past do not.
            for edge in [(2, 0), (-2, 0), (0, 2), (0, -2), (1, 1)] {
                assert!(found.contains(&edge), "{edge:?}");
            }
            for outside in [(3, 0), (2, 1), (1, 2), (2, 2)] {
                assert!(!found.contains(&outside), "{outside:?}");
            }
        }
    }

    #[test]
    fn quantize_floors_to_the_minute() {
        assert_eq!(
            quantize_time(ts("2026-07-11T20:20:37Z"), EVENTS_TIME_STEP),
            ts("2026-07-11T20:20:00Z")
        );
        assert_eq!(
            quantize_time(ts("2026-07-11T20:20:00Z"), EVENTS_TIME_STEP),
            ts("2026-07-11T20:20:00Z")
        );
    }

    #[test]
    fn wms_times_are_bounded_quantized_ascending() {
        let times = build_wms_times(
            Some((ts("2026-07-01T00:00:00Z"), ts("2026-07-11T20:20:37Z"))),
            EVENTS_TIME_STEP,
        );
        assert_eq!(*times.last().unwrap(), ts("2026-07-11T20:20:00Z"));
        // 6 h of 1-min steps + the endpoint.
        assert_eq!(times.len(), 361);
        assert!(times.windows(2).all(|w| w[0] < w[1]));

        // A short extent clamps to its start.
        let times = build_wms_times(
            Some((ts("2026-07-11T20:00:30Z"), ts("2026-07-11T20:10:00Z"))),
            EVENTS_TIME_STEP,
        );
        assert_eq!(*times.first().unwrap(), ts("2026-07-11T20:00:00Z"));
        assert_eq!(*times.last().unwrap(), ts("2026-07-11T20:10:00Z"));
        assert!(build_wms_times(None, EVENTS_TIME_STEP).is_empty());
    }

    /// #1063: the station layers step on `[postgis.map] time_step`: the
    /// default 10 minutes advertises 37 steps over the 6 h window, and a TIME
    /// floors to its step, the same instant `resolve_window_end` resolves.
    #[test]
    fn station_times_step_on_the_configured_time_step() {
        let step = Duration::minutes(10);
        let times = build_wms_times(
            Some((ts("2026-07-01T00:00:00Z"), ts("2026-07-11T20:27:37Z"))),
            step,
        );
        assert_eq!(times.first(), Some(&ts("2026-07-11T14:20:00Z")));
        assert_eq!(times.last(), Some(&ts("2026-07-11T20:20:00Z")));
        assert_eq!(times.len(), 37);
        assert!(times.windows(2).all(|w| w[1] - w[0] == step));
        assert_eq!(
            quantize_time(ts("2026-07-11T20:29:59Z"), step),
            ts("2026-07-11T20:20:00Z")
        );
        assert_eq!(
            quantize_time(ts("2026-07-11T20:29:59Z"), Duration::minutes(15)),
            ts("2026-07-11T20:15:00Z")
        );
        let mut cfg = crate::station_map::tests::station_config();
        assert_eq!(time_step(&cfg), step, "the default");
        cfg.station_map.time_step = Duration::minutes(30);
        assert_eq!(time_step(&cfg), Duration::minutes(30));
    }

    #[test]
    fn splat_paints_disc_with_age_minutes_newest_wins() {
        let end = ts("2026-07-11T20:20:00Z");
        // Two strikes on the same spot: older first (ascending input),
        // newest must win the overlap.
        let strikes = [
            Strike {
                time: ts("2026-07-11T19:50:00Z"), // 30 min old
                lon: 25.0,
                lat: 60.0,
            },
            Strike {
                time: ts("2026-07-11T20:15:00Z"), // 5 min old
                lon: 25.0,
                lat: 60.0,
            },
        ];
        // 10×10 canvas over a 1°×1° box centred on the strike.
        let tile = splat_strikes(
            &strikes,
            end,
            [24.5, 59.5, 25.5, 60.5],
            10,
            10,
            &OutputCrs::Wgs84,
        );
        // Centre pixel: fraction (0.5, 0.5) → pixel (5, 5).
        let v = tile.values.value_at(5 * 10 + 5).expect("centre painted");
        assert!((v - 5.0).abs() < 1e-9, "newest strike wins: got {v}");
        // The disc has some radius but corners stay empty.
        assert!(tile.values.value_at(0).is_none());
        // A neighbour within the disc radius is painted too.
        assert!(tile.values.value_at(5 * 10 + 6).is_some());
    }

    #[test]
    fn splat_edge_strike_still_paints_partial_disc() {
        // A strike just outside the bbox but within the symbol margin must
        // survive the lon/lat pre-filter and paint its in-canvas pixels.
        let end = ts("2026-07-11T20:20:00Z");
        let strikes = [Strike {
            time: ts("2026-07-11T20:15:00Z"),
            lon: 25.501, // ~0.001° east of the bbox edge; 0.1°/px canvas
            lat: 60.0,
        }];
        let tile = splat_strikes(
            &strikes,
            end,
            [24.5, 59.5, 25.5, 60.5],
            10,
            10,
            &OutputCrs::Wgs84,
        );
        assert!(
            !tile.is_empty(),
            "edge strike's partial disc must paint into the canvas"
        );
    }

    /// #828: a viewport past ±180°, which is how OGC API Maps passes the
    /// seam test box 170,10,-170,20, paints strikes on both sides of the
    /// antimeridian at their positions in view.
    #[test]
    fn splat_paints_both_sides_of_the_antimeridian_in_a_viewport_past_180() {
        let end = ts("2026-07-11T20:20:00Z");
        let strike = |lon: f64| Strike {
            time: ts("2026-07-11T20:15:00Z"),
            lon,
            lat: 15.5,
        };
        let strikes = [strike(175.5), strike(-175.5)];
        // 1° per pixel: 175.5°E is column 5, at 175.5° or −184.5°; 175.5°W
        // is column 14, at 184.5° or −175.5°; 15.5°N is row 4.
        for bbox in [[170.0, 10.0, 190.0, 20.0], [-190.0, 10.0, -170.0, 20.0]] {
            for output_crs in [OutputCrs::Wgs84, OutputCrs::WebMercator] {
                let tile = splat_strikes(&strikes, end, bbox, 20, 10, &output_crs);
                for col in [5, 14] {
                    assert_eq!(
                        tile.values.value_at(4 * 20 + col),
                        Some(5.0),
                        "{bbox:?} {output_crs:?} column {col}"
                    );
                }
                // Nothing at the seam itself, between the two discs.
                assert!(tile.values.value_at(4 * 20 + 10).is_none());
            }
        }
    }

    #[test]
    fn splat_skips_out_of_view_strikes() {
        let end = ts("2026-07-11T20:20:00Z");
        let strikes = [Strike {
            time: ts("2026-07-11T20:00:00Z"),
            lon: 40.0, // far outside the bbox
            lat: 60.0,
        }];
        let tile = splat_strikes(
            &strikes,
            end,
            [24.5, 59.5, 25.5, 60.5],
            8,
            8,
            &OutputCrs::Wgs84,
        );
        assert!(tile.is_empty());
    }
}
