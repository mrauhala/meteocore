//! Contour lines over a real GRIB field (#1378) match an independent trace
//! of the source grid to within half a source cell.
//!
//! The field is the committed GFS 2 m temperature at 1° (complex packing,
//! `testdata/grib-gfs-complex`), served by the engine in °C. The contours
//! are the built-in isotherms, every 2 °C, drawn as every map API draws
//! them: the engine's `get_raster_tile` over a `ContourPlan`'s grown view,
//! traced and drawn by `ds_render`. The independent trace decodes the
//! message with the `grib` crate and walks the source cells' edges, linear
//! along each, never touching the renderer.

use std::io::Cursor;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use ds_core::config::GribConfig;
use ds_core::map_engine::{MapEngine, OutputCrs};
use ds_render::{source_spacing_deg, ContourPlan, ContourStyle};
use engine_grib::GribEngine;

const FIXTURE: &str = "gfs-tmp2m-1deg.grib2";
/// The view: Europe, at four output pixels per source cell.
const BBOX: [f64; 4] = [-10.0, 35.0, 30.0, 70.0];
const PX_PER_DEG: f64 = 4.0;
const INTERVAL: f64 = 2.0;

fn source_bytes() -> Vec<u8> {
    std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/grib-gfs-complex")
            .join(FIXTURE),
    )
    .expect("committed GFS fixture")
}

/// A local GRIB directory holding the fixture under an ECMWF JSON index.
fn engine() -> GribEngine {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("grib-contours");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bytes = source_bytes();
    std::fs::write(dir.join("gfs-2t.grib2"), &bytes).unwrap();
    std::fs::write(
        dir.join("gfs-2t.index"),
        format!(
            "{{\"domain\": \"g\", \"date\": \"20261026\", \"time\": \"0000\", \"step\": \"6\", \
             \"levtype\": \"sfc\", \"param\": \"2t\", \"_offset\": 0, \"_length\": {}}}\n",
            bytes.len()
        ),
    )
    .unwrap();
    GribEngine::new(
        "gfs-contours",
        &GribConfig {
            level_types: None,
            data_path: Some(dir.to_string_lossy().into_owned()),
            endpoint: None,
            bucket: None,
            prefix_pattern: None,
            index_suffix: None,
            data_suffix: None,
            poll_interval_secs: 600,
            max_runs: None,
            time_window: None,
            parameters: None,
            grid_cache_mb: 64,
            message_cache_mb: 0,
            point_index_cache_mb: None,
            prewarm: false,
            read_ahead: ds_core::config::DEFAULT_GRIB_READ_AHEAD,
            run_hours: None,
            index_format: Some("ecmwf-json".to_string()),
            filename_contains: None,
        },
    )
    .expect("engine over the fixture")
}

/// The source grid, decoded straight from the message: 360 × 181 nodes,
/// north row first, from 0°E, in °C when the engine serves °C.
struct Source {
    values: Vec<f64>,
}

impl Source {
    fn decode(offset: f64) -> Self {
        let bytes = source_bytes();
        let grib2 = grib::from_reader(Cursor::new(&bytes[..])).unwrap();
        let (_, submessage) = grib2.iter().next().unwrap();
        let values: Vec<f64> = grib::Grib2SubmessageDecoder::from(submessage)
            .unwrap()
            .dispatch()
            .unwrap()
            .map(|v| f64::from(v) + offset)
            .collect();
        assert_eq!(values.len(), 360 * 181);
        Self { values }
    }

    /// Node `(i, j)`: longitude `i`° east (wrapping), latitude `90 − j`.
    fn node(&self, i: i64, j: i64) -> f64 {
        self.values[j as usize * 360 + i.rem_euclid(360) as usize]
    }

    /// The bilinear surface of the nodes at `(lon, lat)`.
    fn at(&self, lon: f64, lat: f64) -> f64 {
        let (x, y) = (lon, 90.0 - lat);
        let (i, j) = (x.floor(), y.floor());
        let (fx, fy) = (x - i, y - j);
        let (i, j) = (i as i64, j as i64);
        let top = self.node(i, j) * (1.0 - fx) + self.node(i + 1, j) * fx;
        let bottom = self.node(i, j + 1) * (1.0 - fx) + self.node(i + 1, j + 1) * fx;
        top * (1.0 - fy) + bottom * fy
    }
}

#[test]
fn isotherms_match_an_independent_trace_within_half_a_cell() {
    let engine = engine();
    let info = engine.raster_info();
    let unit = info
        .parameter_unit(Some("2t"))
        .expect("2t served")
        .to_string();
    // The engine serves kelvin in °C; the trace follows the served unit.
    let offset = if unit.contains('C') { -273.15 } else { 0.0 };
    let base = if unit.contains('C') { 0.0 } else { 273.15 };
    let source = Source::decode(offset);
    let time: DateTime<Utc> = "2026-10-26T06:00:00Z".parse().unwrap();

    let width = ((BBOX[2] - BBOX[0]) * PX_PER_DEG) as u32;
    let height = ((BBOX[3] - BBOX[1]) * PX_PER_DEG) as u32;
    let style = ContourStyle::new(INTERVAL, base, None, Some(1.0), false).unwrap();
    let plan = ContourPlan::new(
        &style,
        BBOX,
        width,
        height,
        &OutputCrs::Wgs84,
        source_spacing_deg(&info),
    );
    assert_eq!(
        plan.smoothing(),
        0,
        "a view finer than its source is traced as read"
    );
    let tile = engine
        .get_raster_tile(
            plan.fetch_bbox(),
            plan.fetch_width(),
            plan.fetch_height(),
            Some(time),
            plan.fetch_crs(),
            Some("2t"),
            None,
            None,
        )
        .unwrap();
    let rgba = plan.draw(&tile, &style, None).unwrap().expect("isotherms");
    let alpha = |x: usize, y: usize| rgba[(y * width as usize + x) * 4 + 3];
    let centre = |x: usize, y: usize| {
        (
            BBOX[0] + (x as f64 + 0.5) / PX_PER_DEG,
            BBOX[3] - (y as f64 + 0.5) / PX_PER_DEG,
        )
    };
    let level = |k: f64| base + k * INTERVAL;

    // Every pixel the lines cover at least half lies within half a source
    // cell of an isotherm of the source surface: over a disc of radius
    // 0.5° around its centre, the surface takes that isotherm's value. A
    // bilinear surface takes its extremes over a disc at the nodes inside
    // it or on its rim, so those are what is sampled.
    let mut drawn = 0;
    for y in 0..height as usize {
        for x in 0..width as usize {
            if alpha(x, y) < 128 {
                continue;
            }
            drawn += 1;
            let (lon, lat) = centre(x, y);
            let rim = (0..64).map(|k| {
                let a = f64::from(k) * std::f64::consts::PI / 32.0;
                source.at(lon + 0.5 * a.cos(), lat + 0.5 * a.sin())
            });
            let nodes = (lon.floor() as i64..=lon.ceil() as i64)
                .flat_map(|i| (lat.floor() as i64..=lat.ceil() as i64).map(move |j| (i, j)))
                .filter(|&(i, j)| (i as f64 - lon).hypot(j as f64 - lat) <= 0.5)
                .map(|(i, j)| source.node(i, 90 - j));
            let samples: Vec<f64> = rim.chain(nodes).collect();
            let (lo, hi) = samples
                .iter()
                .fold((f64::MAX, f64::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
            let k = ((lo - base) / INTERVAL).ceil();
            assert!(
                level(k) <= hi,
                "line pixel ({x}, {y}) at {lon:.2}°E {lat:.2}°N: no isotherm within \
                 half a cell ({lo:.2}..{hi:.2})"
            );
        }
    }
    assert!(drawn > 1_000, "{drawn} line pixels");

    // Every isotherm crossing of a source cell edge inside the view has a
    // drawn pixel within half a cell (two output pixels) of it, unless no
    // output pixel centre within half a cell lies on its far side: a loop
    // around one node, smaller than an output pixel, which the output grid
    // cannot hold and no trace at that resolution draws (64 of 1866 here).
    let pixels_near = |lon: f64, lat: f64| {
        let px = (lon - BBOX[0]) * PX_PER_DEG;
        let py = (BBOX[3] - lat) * PX_PER_DEG;
        (-2i64..=2)
            .flat_map(move |dy| {
                (-2i64..=2).map(move |dx| (px.floor() as i64 + dx, py.floor() as i64 + dy))
            })
            .filter(move |&(x, y)| {
                (0..width as i64).contains(&x)
                    && (0..height as i64).contains(&y)
                    && (x as f64 + 0.5 - px).hypot(y as f64 + 0.5 - py) <= 0.5 * PX_PER_DEG
            })
            .map(|(x, y)| (x as usize, y as usize))
    };
    let (mut crossings, mut sub_pixel) = (0, 0);
    let (lon0, lon1) = (BBOX[0] as i64 + 1, BBOX[2] as i64 - 1);
    let (lat0, lat1) = (BBOX[1] as i64 + 1, BBOX[3] as i64 - 1);
    for lat in lat0..=lat1 {
        for lon in lon0..=lon1 {
            let (i, j) = (lon, 90 - lat);
            let here = source.node(i, j);
            for (east, south, other) in [
                (1.0, 0.0, source.node(i + 1, j)),
                (0.0, 1.0, source.node(i, j + 1)),
            ] {
                if (east > 0.0 && lon + 1 > lon1) || (south > 0.0 && lat - 1 < lat0) {
                    continue;
                }
                let (lo, hi) = (here.min(other), here.max(other));
                let first = ((lo - base) / INTERVAL).ceil() as i64;
                let last = ((hi - base) / INTERVAL).floor() as i64;
                for k in first..=last {
                    let l = level(k as f64);
                    if l > lo && l < hi {
                        crossings += 1;
                        let t = (l - here) / (other - here);
                        let (clon, clat) = (lon as f64 + east * t, lat as f64 - south * t);
                        let below = |(x, y): (usize, usize)| {
                            let (lon, lat) = centre(x, y);
                            source.at(lon, lat) < l
                        };
                        let resolved = pixels_near(clon, clat).any(below)
                            && pixels_near(clon, clat).any(|p| !below(p));
                        if !resolved {
                            sub_pixel += 1;
                            continue;
                        }
                        assert!(
                            pixels_near(clon, clat).any(|(x, y)| alpha(x, y) > 0),
                            "isotherm {l} crosses the source at {clon:.2}°E {clat:.2}°N \
                             with no line within half a cell"
                        );
                    }
                }
            }
        }
    }
    assert!(crossings > 200, "{crossings} source crossings");
    assert!(
        sub_pixel * 20 < crossings,
        "{sub_pixel} of {crossings} crossings in loops below an output pixel"
    );
}
