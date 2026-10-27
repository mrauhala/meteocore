//! Station-symbol map layers (#1063, #1204): the drawing half every station
//! engine shares (CSV, BUFR, PostGIS). The domain half (settings, the frame
//! time axis and window, the latest-value reduction) is
//! `ds_core::station_map`.
//!
//! - **Overlap order.** The most extreme value is on top: markers are drawn
//!   in ascending `|value - midpoint|` ([`order_markers`]), the midpoint
//!   being the middle of the layer's default style range (style `min`/`max`,
//!   else the palette's first and last stops), which the server hands the
//!   engine as [`StationSymbols`]. Nodata markers go under every value.
//! - **Drawing.** Discs of `marker_size` output pixels on a transparent
//!   ground, projected per station through `OutputCrs::world_to_fraction`,
//!   never per output pixel (Critical Rule 5) ([`draw_markers`]).
//! - **Nodata.** A nodata marker's pixels hold [`NODATA_MARKER`]; the server
//!   wraps the layer's colormaps in [`crate::SymbolColorMap`], which draws it
//!   in the palette's `nodata_color` and keeps the ground transparent.
//! - **Frames are cached.** The meta-tile loop calls `get_raster_tile` up to
//!   ~200 times a viewport, so one process-global, byte-bounded,
//!   single-flight cache ([`frame_cache`], `MC_STATION_FRAME_CACHE_MB`)
//!   holds every engine's frames, keyed by a process-unique source id
//!   ([`next_frame_source`]), the parameter, the frame instant, the content
//!   version and the midpoint. [`StationLayers`] wraps it for the in-memory
//!   engines.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use chrono::{DateTime, Utc};
use ds_cache::ByteBoundedCache;
use ds_core::error::DataServerError;
use ds_core::map_engine::{OutputCrs, RasterTile};
use ds_core::model::Location;
use ds_core::station_map::{
    advertised_times, frame_window, resolve_frame_end, LatestValues, StationMapSettings,
};

/// The value a nodata marker's pixels hold: a station drawn without a value
/// (`draw_missing`). No measurement is `f64::MIN`; [`crate::SymbolColorMap`]
/// matches it exactly and draws it in the palette's `nodata_color`.
pub const NODATA_MARKER: f64 = f64::MIN;

/// The style facts of a station collection's map layers, from the server's
/// style resolution: each parameter layer's midpoint, the middle of its
/// default style's value range.
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

/// One station to draw: its position and its value in the frame, `None` for
/// a nodata marker.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Marker {
    pub lon: f64,
    pub lat: f64,
    pub value: Option<f64>,
}

/// `markers` in draw order: those without a value first (only with
/// `draw_missing`, else left out), in input order, then those with a value
/// in ascending `|value - midpoint|`, ties in input order, so the most
/// extreme value is drawn last and stays on top.
pub fn order_markers(
    markers: impl IntoIterator<Item = Marker>,
    midpoint: f64,
    draw_missing: bool,
) -> Vec<Marker> {
    let mut missing = Vec::new();
    let mut valued = Vec::new();
    for marker in markers {
        match marker.value {
            Some(_) => valued.push(marker),
            None if draw_missing => missing.push(marker),
            None => {}
        }
    }
    let distance = |m: &Marker| m.value.map_or(0.0, |v| (v - midpoint).abs());
    // Stable: equal distances keep input order.
    valued.sort_by(|a, b| distance(a).total_cmp(&distance(b)));
    missing.extend(valued);
    missing.shrink_to_fit();
    missing
}

/// A frame's markers in draw order ([`order_markers`]): one per station of
/// `stations`, at its position, with its value in `latest` (by place).
pub fn frame_markers(
    stations: &[&Location],
    latest: &LatestValues,
    midpoint: f64,
    draw_missing: bool,
) -> Vec<Marker> {
    order_markers(
        stations.iter().enumerate().map(|(at, station)| Marker {
            lon: station.longitude,
            lat: station.latitude,
            value: latest.value(at),
        }),
        midpoint,
        draw_missing,
    )
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
pub fn draw_markers(
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

/// Frame cache key: the frame source ([`next_frame_source`]: one engine
/// instance, so a rebuilt engine never reads its predecessor's frames), the
/// parameter, the frame instant, the content version the frame was read
/// under, and the midpoint's bits (the draw order is baked into the frame).
pub type FrameKey = (u64, String, DateTime<Utc>, u64, u64);

/// Cache weight of one frame: the key and one `Marker` per slot the `Vec`
/// allocated.
fn frame_weight(k: &FrameKey, v: &Arc<Vec<Marker>>) -> u64 {
    (k.1.len() + 64 + v.capacity() * std::mem::size_of::<Marker>()) as u64
}

/// Process-global station-frame cache (`MC_STATION_FRAME_CACHE_MB`, default
/// 32 MiB), shared by every station engine. An entry is about 32 bytes a
/// station, so 20 000 stations is 640 KB a frame; the 1 MiB entry estimate
/// keeps the default cache one shard, so a frame of the 200 000-station
/// ceiling (6.4 MB) still fits.
pub fn frame_cache() -> &'static ByteBoundedCache<FrameKey, Arc<Vec<Marker>>> {
    static CACHE: OnceLock<ByteBoundedCache<FrameKey, Arc<Vec<Marker>>>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let (bytes, source) = ds_cache::plan::env_bytes("MC_STATION_FRAME_CACHE_MB", 32);
        ByteBoundedCache::new(bytes, 1024 * 1024, frame_weight).registered(
            "station_frame_cache",
            ds_cache::plan::Scope::Process,
            source,
        )
    })
}

/// Snapshot of the station-frame cache counters for the `/metrics` scrape.
pub fn station_frame_cache_metrics() -> ds_cache::CacheMetrics {
    frame_cache().metrics()
}

/// A process-unique frame source id: each engine instance takes one, and
/// [`FrameKey`] leads with it, so two engines (of any type) never share a
/// frame.
pub fn next_frame_source() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The station layers of one in-memory station engine (CSV, BUFR): its
/// settings, its frame source id and the style facts the server hands it.
/// The engine supplies the advertised times, the content version and a
/// reader for one frame's window; this does the rest.
#[derive(Debug)]
pub struct StationLayers {
    source: u64,
    settings: StationMapSettings,
    symbols: RwLock<Arc<StationSymbols>>,
}

impl Default for StationLayers {
    fn default() -> Self {
        Self::new(StationMapSettings::default())
    }
}

impl StationLayers {
    pub fn new(settings: StationMapSettings) -> Self {
        Self {
            source: next_frame_source(),
            settings,
            symbols: RwLock::new(Arc::new(StationSymbols::default())),
        }
    }

    pub fn settings(&self) -> &StationMapSettings {
        &self.settings
    }

    /// Hand the layers their style facts. The server calls this on every
    /// load that wires the collection to a map API, built or reused, since a
    /// reload can change the styles without changing the collection.
    pub fn set_symbols(&self, symbols: StationSymbols) {
        *self.symbols.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(symbols);
    }

    /// The midpoint `parameter`'s markers are ordered from.
    pub fn midpoint(&self, parameter: &str) -> f64 {
        self.symbols
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .midpoint(parameter)
    }

    /// The TIME steps to advertise for data spanning `temporal_extent`.
    pub fn advertised_times(
        &self,
        temporal_extent: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> Vec<DateTime<Utc>> {
        advertised_times(temporal_extent, self.settings.time_step)
    }

    /// The frame instant a request for `time` draws, given the advertised
    /// steps: the one selection `get_raster_tile` and `resolve_time` share
    /// (#507).
    pub fn frame_end(
        &self,
        advertised: &[DateTime<Utc>],
        time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        resolve_frame_end(advertised, time, self.settings.time_step)
    }

    /// The markers of `parameter`'s frame at `end` in draw order, read once
    /// per `(parameter, end, version, midpoint)` and cached. `read` gets the
    /// frame's window and returns one marker per station, `value: None` for
    /// a station without a value in it, in a stable station order.
    pub fn frame(
        &self,
        parameter: &str,
        end: DateTime<Utc>,
        version: u64,
        read: impl FnOnce((DateTime<Utc>, DateTime<Utc>)) -> Result<Vec<Marker>, DataServerError>,
    ) -> Result<Arc<Vec<Marker>>, DataServerError> {
        let midpoint = self.midpoint(parameter);
        let key: FrameKey = (
            self.source,
            parameter.to_string(),
            end,
            version,
            midpoint.to_bits(),
        );
        frame_cache().get_or_insert_with(&key, || {
            let markers = read(frame_window(end, self.settings.max_age))?;
            Ok(Arc::new(order_markers(
                markers,
                midpoint,
                self.settings.draw_missing,
            )))
        })
    }

    /// Draw a frame's markers over `bbox` ([`draw_markers`]).
    pub fn draw(
        &self,
        markers: &[Marker],
        bbox: [f64; 4],
        width: u32,
        height: u32,
        output_crs: &OutputCrs,
    ) -> RasterTile {
        draw_markers(
            markers,
            bbox,
            width,
            height,
            output_crs,
            self.settings.marker_size,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

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

    /// #1052-style weight: every allocated marker slot counts.
    #[test]
    fn frame_weight_counts_allocated_markers() {
        let key: FrameKey = (1, "t2m".into(), ts("2026-10-18T06:00:00Z"), 3, 0);
        let frame = Arc::new(Vec::<Marker>::with_capacity(1000));
        assert!(frame_weight(&key, &frame) >= (1000 * std::mem::size_of::<Marker>()) as u64);
    }

    /// `StationLayers` reads a frame once per key, in draw order around the
    /// midpoint the server handed it, with the window `[end - max_age,
    /// end]`; a new content version or midpoint reads it again, and two
    /// layers never share a frame.
    #[test]
    fn station_layers_cache_frames_per_version_and_midpoint() {
        let layers = StationLayers::new(StationMapSettings {
            max_age: Duration::minutes(30),
            ..Default::default()
        });
        let end = ts("2026-10-18T06:00:00Z");
        let reads = std::cell::Cell::new(0);
        let read = |window: (DateTime<Utc>, DateTime<Utc>)| {
            reads.set(reads.get() + 1);
            assert_eq!(window, (ts("2026-10-18T05:30:00Z"), end));
            Ok(vec![
                marker(1.0, 1.0, Some(30.0)),
                marker(2.0, 2.0, None),
                marker(3.0, 3.0, Some(12.0)),
            ])
        };
        let frame = layers.frame("t", end, 7, read).unwrap();
        let values: Vec<Option<f64>> = frame.iter().map(|m| m.value).collect();
        assert_eq!(values, vec![Some(12.0), Some(30.0)], "midpoint 0");
        layers.frame("t", end, 7, read).unwrap();
        assert_eq!(reads.get(), 1, "cached");
        layers.frame("t", end, 8, read).unwrap();
        assert_eq!(reads.get(), 2, "a new version reads again");
        layers.set_symbols(StationSymbols::new(HashMap::from([("t".into(), 25.0)])));
        let frame = layers.frame("t", end, 8, read).unwrap();
        assert_eq!(reads.get(), 3, "a new midpoint reads again");
        let values: Vec<Option<f64>> = frame.iter().map(|m| m.value).collect();
        assert_eq!(values, vec![Some(30.0), Some(12.0)], "midpoint 25");
        StationLayers::new(layers.settings)
            .frame("t", end, 8, read)
            .unwrap();
        assert_eq!(reads.get(), 4, "another engine's frame");
        assert_eq!(
            layers.frame_end(&[end], Some(ts("2026-10-18T06:09:59Z"))),
            Some(end)
        );
        assert_eq!(layers.frame_end(&[end], None), Some(end));
    }
}
