//! `MapEngine` for the events shape (#504): the age-colored lightning layer.
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
//! never go stale, and the live window's staleness is bounded by the TIME
//! quantum (a new minute ⇒ a new key).

use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Duration, Utc};
use ds_cache::ByteBoundedCache;
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use tokio_postgres::Row;

use crate::engine::PostgisEngine;
use crate::metadata::CollectionMeta;
use crate::query::{build_events_window, params_as_refs, BuiltQuery};
use crate::schema::EventsShape;

/// One strike ready to splat.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Strike {
    pub time: DateTime<Utc>,
    pub lon: f64,
    pub lat: f64,
}

/// WMS TIME quantum: requested instants floor to the minute, so cache keys
/// stay bounded and animation frames are deterministic. The live frame's
/// staleness is bounded by this quantum plus ingest lag.
pub(crate) const WMS_TIME_STEP_SECS: i64 = 60;

/// Advertised TIME dimension depth: 1-minute steps over the most recent 6 h
/// (≤360 entries in GetCapabilities). Older frames still render via an
/// explicit TIME — membership in the advertised list is not required.
pub(crate) const WMS_TIME_DEPTH: Duration = Duration::hours(6);

/// Fixed splat radius in output pixels. Constant across zoom — classic
/// lightning markers stay marker-sized rather than scaling with the map
/// (the #504 symbol-size question, resolved to the cheap option).
const SYMBOL_RADIUS_PX: i64 = 2;

/// Hard cap on strikes fetched per window — bounds the cache entry and the
/// splat loop. `ORDER BY time DESC` in the fetch means a truncated window
/// keeps the NEWEST strikes. 200k ≈ 5× the heaviest observed storm-day.
const MAX_WINDOW_STRIKES: usize = 200_000;

/// The advertised parameter id of the derived age layer.
pub const LIGHTNING_AGE_PARAMETER: &str = "lightning_age";

/// Floor a requested instant to the TIME quantum.
pub(crate) fn quantize_wms_time(t: DateTime<Utc>) -> DateTime<Utc> {
    let secs = t.timestamp();
    DateTime::from_timestamp(secs - secs.rem_euclid(WMS_TIME_STEP_SECS), 0).unwrap_or(t)
}

/// The window end `get_raster_tile` renders for a requested time — the
/// #507 cache-key authority for this engine: explicit TIME floors to the
/// quantum; no TIME ⇒ the latest advertised step. `None` only when the
/// collection has no data yet.
pub(crate) fn resolve_window_end(
    meta: &CollectionMeta,
    time: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    match time {
        Some(t) => Some(quantize_wms_time(t)),
        None => meta.wms_times.last().copied(),
    }
}

/// The advertised TIME steps for the collection metadata: quantized,
/// ascending, newest last, clamped to [`WMS_TIME_DEPTH`] below the newest
/// event. Rebuilt per metadata refresh (never per request).
pub(crate) fn build_wms_times(
    temporal_extent: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Vec<DateTime<Utc>> {
    let Some((first, last)) = temporal_extent else {
        return Vec::new();
    };
    let newest = quantize_wms_time(last);
    let floor = quantize_wms_time((last - WMS_TIME_DEPTH).max(first));
    let mut steps = Vec::new();
    let mut t = floor;
    while t <= newest {
        steps.push(t);
        t += Duration::seconds(WMS_TIME_STEP_SECS);
    }
    steps
}

/// Process-global strike-window cache. Global (not per-engine) so a reload
/// keeps warm windows; the collection id in the key isolates collections
/// sharing the process. Weight = key + one `Strike` per row.
type WindowKey = (String, DateTime<Utc>, DateTime<Utc>);

pub fn strike_window_cache() -> &'static ByteBoundedCache<WindowKey, Arc<Vec<Strike>>> {
    static CACHE: OnceLock<ByteBoundedCache<WindowKey, Arc<Vec<Strike>>>> = OnceLock::new();
    CACHE.get_or_init(|| {
        ByteBoundedCache::from_env("MC_LIGHTNING_STRIKE_CACHE_MB", 32, 64 * 1024, |k, v| {
            (k.0.len() + 32 + v.len() * std::mem::size_of::<Strike>()) as u64
        })
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
    let fut = async {
        let client = pool
            .get()
            .await
            .map_err(|e| DataServerError::Engine(format!("pool acquire failed: {e}")))?;
        let refs = params_as_refs(&built.params);
        client
            .query(&built.sql, &refs)
            .await
            .map_err(|e| crate::engine::map_pg_error(e, built))
    };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.block_on(fut),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| DataServerError::Engine(format!("runtime build failed: {e}")))?
            .block_on(fut),
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
    let key: WindowKey = (engine.collection_id().to_string(), start, end);
    strike_window_cache().get_or_insert_with(&key, || {
        let built = build_events_window(shape, (start, end), MAX_WINDOW_STRIKES + 1)
            .map_err(|e| DataServerError::Engine(format!("build_events_window: {e}")))?;
        let rows = run_query_from_blocking(engine.pool(), &built)?;
        let truncated = rows.len() > MAX_WINDOW_STRIKES;
        let mut strikes = Vec::with_capacity(rows.len().min(MAX_WINDOW_STRIKES));
        // Rows arrive newest-first (truncation keeps the newest); take the
        // cap then reverse to ascending so the splat paints newest LAST.
        for row in rows.iter().take(MAX_WINDOW_STRIKES) {
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
    for s in strikes {
        let (fx, fy) = output_crs.world_to_fraction(bbox, s.lon, s.lat);
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
                if dx * dx + dy * dy > SYMBOL_RADIUS_PX * SYMBOL_RADIUS_PX {
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
        _parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        let Some(shape) = self.config().events() else {
            // Station shapes have no raster surface; config validation and
            // the admin allowlist keep them out of the map registries.
            return Err(DataServerError::InvalidParameter(
                "map rendering requires the events shape".into(),
            ));
        };
        let shape = shape.clone();
        let meta = self.cache().load();
        let end = resolve_window_end(&meta, time).ok_or_else(|| {
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
        // O(1): every field is a snapshot clone (wms_times is rebuilt per
        // metadata refresh, #211).
        let meta = self.cache().load();
        RasterInfo {
            native_crs: "CRS:84".to_string(),
            spatial_extent: self.config().events_extent_bbox,
            times: (*meta.wms_times).clone(),
            parameter: LIGHTNING_AGE_PARAMETER.to_string(),
            unit: "min".to_string(),
            parameters: vec![],
            vertical: None,
            grid_size: None,
            layer_subtitle: None,
            reference_times: Vec::new(),
        }
    }

    fn resolve_time(
        &self,
        time: Option<DateTime<Utc>>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        // The #507 cache-key authority: the SAME `resolve_window_end` the
        // render path uses (quantized TIME / latest advertised step). Falls
        // back to the requested time when there is no data yet — the render
        // errors and caches nothing.
        resolve_window_end(&self.cache().load(), time).or(time)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn quantize_floors_to_the_minute() {
        assert_eq!(
            quantize_wms_time(ts("2026-07-11T20:20:37Z")),
            ts("2026-07-11T20:20:00Z")
        );
        assert_eq!(
            quantize_wms_time(ts("2026-07-11T20:20:00Z")),
            ts("2026-07-11T20:20:00Z")
        );
    }

    #[test]
    fn wms_times_are_bounded_quantized_ascending() {
        let times = build_wms_times(Some((
            ts("2026-07-01T00:00:00Z"),
            ts("2026-07-11T20:20:37Z"),
        )));
        assert_eq!(*times.last().unwrap(), ts("2026-07-11T20:20:00Z"));
        // 6 h of 1-min steps + the endpoint.
        assert_eq!(times.len(), 361);
        assert!(times.windows(2).all(|w| w[0] < w[1]));

        // A short extent clamps to its start.
        let times = build_wms_times(Some((
            ts("2026-07-11T20:00:30Z"),
            ts("2026-07-11T20:10:00Z"),
        )));
        assert_eq!(*times.first().unwrap(), ts("2026-07-11T20:00:00Z"));
        assert_eq!(*times.last().unwrap(), ts("2026-07-11T20:10:00Z"));
        assert!(build_wms_times(None).is_empty());
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
