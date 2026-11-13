//! End to end over the repository's committed radar frames
//! (`testdata/radar`: five MET Norway reflectivity mosaics, 23:15–23:35
//! UTC, 5 minutes apart, WGS84 `float32`) through the public GeoTIFF
//! engine, the `Arc<dyn MapEngine>` the server hands the period engine.

use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use ds_core::config::{GeoTiffConfig, PeriodConfig, RainRateConfig};
use ds_core::edr_engine::EdrEngine;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterValues};
use ds_core::model::CoverageResponse;
use engine_geotiff::GeoTiffEngine;
use engine_period::geometry::GridGeom;
use engine_period::PeriodEngine;

/// The default `max_pixels`: the fixture's 3249 × 1750 cells stay native.
const DEFAULT_MAX_PIXELS: usize = 40_000_000;
/// A grid coarser than the source: 406 × 437 cells.
const COARSE_MAX_PIXELS: usize = 250_000;
/// The default `read_strip_cells`: the fixture's frames in two strips.
const DEFAULT_STRIP: usize = 4 * 1024 * 1024;

fn radar() -> Arc<GeoTiffEngine> {
    let config = GeoTiffConfig {
        filename_template: Some("radar_%Y%m%dT%H%MZ.tif".into()),
        filename_pattern: None,
        timestamp_format: None,
        parameter: "reflectivity".into(),
        unit: "dBZ".into(),
        poll_interval_secs: 3600,
        tile_cache_mb: Some(ds_core::config::CacheSize::Mib(64)),
        band: 1,
        max_files: None,
        nodata: None,
        scale: None,
        offset: None,
        exclude_patterns: vec![],
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        time_window: None,
        scan_days: None,
        stac_url: None,
        stac_asset_key: "data".into(),
        stac_asset_allowlist: None,
    };
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/radar");
    Arc::new(GeoTiffEngine::new("radar", Some(dir), &config).expect("the committed frames load"))
}

fn at(minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 24, 23, minute, 0).unwrap()
}

fn marshall_palmer(dbz: f64) -> f64 {
    if dbz < 7.0 {
        0.0
    } else {
        (10f64.powf(dbz / 10.0) / 200.0).powf(1.0 / 1.6)
    }
}

/// At the default `max_pixels` the grid is the source's own: the
/// accumulation at the wettest cell is the Marshall–Palmer sum of the four
/// frames' reflectivity at that point, as the source's own EDR position
/// query reads it.
#[test]
fn the_default_grid_is_native_and_matches_the_source_pixels() {
    let radar = radar();
    let (engine, geom, got) = accumulate(&radar, DEFAULT_MAX_PIXELS, DEFAULT_STRIP);
    assert_eq!((geom.nx, geom.ny), (3249, 1750), "native");
    assert_eq!(engine.grid_bytes(), 6 * 3249 * 1750);
    let (cell, _) = got
        .iter()
        .enumerate()
        .filter(|(_, v)| v.is_finite())
        .max_by(|a, b| a.1.total_cmp(b.1))
        .unwrap();
    let (col, row) = (
        (cell % geom.nx as usize) as u32,
        (cell / geom.nx as usize) as u32,
    );
    let coords = format!("POINT({} {})", geom.lon(col), geom.lat(row));
    let CoverageResponse::Single(series) = radar
        .query_position(&coords, Some((at(15), at(30))), None, None, None)
        .unwrap()
    else {
        panic!("one coverage")
    };
    let dbz = &series.ranges.values().next().unwrap().values;
    assert_eq!(dbz.len(), 4);
    let want: f64 = dbz
        .iter()
        .flatten()
        .map(|&z| marshall_palmer(z) * 5.0 / 60.0)
        .sum();
    let short = "precipitation_amount_PT20M";
    let CoverageResponse::Single(point) = engine
        .query_position(&coords, None, Some(&[short.to_string()]), None, None)
        .unwrap()
    else {
        panic!("one coverage")
    };
    let served = point.ranges[short].values[0].unwrap();
    assert!((served - want).abs() <= 1e-6 * want, "{served} vs {want}");
    println!(
        "native {}x{}: {served} mm at {coords}, {dbz:?} dBZ",
        geom.nx, geom.ny
    );
}

/// Rain accumulation over the committed frames, on the native grid and on
/// one coarser than the source: the 20-minute window is served at the last
/// frame with full coverage and matches the sum of the four frames'
/// Marshall–Palmer rates times 5 minutes, read from the GeoTIFF engine at
/// the same grid (an overview on the coarse one); the hour is not served,
/// and health says why.
#[test]
fn rain_accumulation_over_the_committed_radar_frames() {
    let radar = radar();
    for max_pixels in [DEFAULT_MAX_PIXELS, COARSE_MAX_PIXELS] {
        let (engine, geom, got) = accumulate(&radar, max_pixels, DEFAULT_STRIP);
        check(&radar, &engine, &geom, &got);
    }
}

/// The real source read in strips of 333 rows (5 × 333 + 85 over the
/// native grid's 1750 rows; 2 × 333 + 103 over a coarse grid's 437) gives
/// bit for bit the grid read in one piece: each strip's box has the grid's
/// own row edges, so the source samples every cell where the whole read
/// does.
#[test]
fn strips_of_the_real_source_equal_the_whole_frame_read() {
    let radar = radar();
    for max_pixels in [DEFAULT_MAX_PIXELS, COARSE_MAX_PIXELS] {
        let (whole, geom, a) = accumulate(&radar, max_pixels, 64_000_000);
        let (strips, _, b) = accumulate(&radar, max_pixels, 333 * geom.nx as usize);
        let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
        assert!(bits(&a) == bits(&b), "{}x{}", geom.nx, geom.ny);
        assert!(strips.peak_frame_bytes() <= 333 * geom.nx as u64 * 4);
        assert!(whole.peak_frame_bytes() >= geom.cells() as u64 * 4);
    }
}

/// A period engine over the fixture with `max_pixels`, polled once, its
/// working grid and the 20-minute sum on it.
fn accumulate(
    radar: &Arc<GeoTiffEngine>,
    max_pixels: usize,
    read_strip_cells: usize,
) -> (PeriodEngine, GridGeom, Vec<f32>) {
    let info = radar.raster_info();
    assert_eq!(
        info.times,
        vec![at(15), at(20), at(25), at(30), at(35)],
        "the fixture's frames"
    );
    let config = PeriodConfig {
        source: "radar".into(),
        parameter: Some("reflectivity".into()),
        statistic: "sum".into(),
        windows: vec!["PT20M".into(), "PT1H".into()],
        rain_rate: Some(RainRateConfig {
            a: 200.0,
            b: 1.6,
            min_dbz: 7.0,
        }),
        threshold: None,
        min_coverage: 0.9,
        cadence: None,
        poll_interval_secs: 30,
        max_pixels,
        max_reads_per_cycle: 16,
        read_strip_cells,
    };
    let engine = PeriodEngine::new("rain", "radar", radar.clone(), &config).unwrap();
    engine.poll_once();
    let geom = GridGeom::over(
        info.spatial_extent.unwrap(),
        info.grid_size.unwrap(),
        max_pixels,
    )
    .unwrap();
    let tile = engine
        .get_raster_tile(
            geom.bbox(),
            geom.nx,
            geom.ny,
            None,
            &OutputCrs::Wgs84,
            Some("precipitation_amount_PT20M"),
            None,
            None,
        )
        .unwrap();
    let RasterValues::F32 { data: got, .. } = tile.values else {
        panic!("renders f32")
    };
    (engine, geom, got)
}

fn check(radar: &Arc<GeoTiffEngine>, engine: &PeriodEngine, geom: &GridGeom, got: &[f32]) {
    let short = "precipitation_amount_PT20M";
    assert_eq!(engine.parameter_times(short).unwrap()[..], [at(35)]);
    assert!(engine
        .parameter_times("precipitation_amount_PT1H")
        .unwrap()
        .is_empty());
    let why = engine.waiting_for().unwrap();
    assert!(
        why.contains("PT1H: the source holds 20 min of frames; this window needs 1 h"),
        "{why}"
    );
    let rinfo = engine.raster_info();
    assert_eq!(rinfo.parameter, short);
    assert_eq!(rinfo.unit, "mm");
    assert_eq!(rinfo.times, vec![at(35)]);

    // The same working grid, read straight from the source.
    assert_eq!(rinfo.grid_size, Some([geom.nx, geom.ny]));
    let cells = geom.cells();
    let mut want: Vec<Option<f64>> = vec![None; cells];
    for minute in [15, 20, 25, 30] {
        let tile = radar
            .get_raster_tile(
                geom.bbox(),
                geom.nx,
                geom.ny,
                Some(at(minute)),
                &OutputCrs::Wgs84,
                None,
                None,
                None,
            )
            .unwrap();
        for (cell, w) in want.iter_mut().enumerate() {
            if let Some(dbz) = tile.values.value_at(cell) {
                *w = Some(w.unwrap_or(0.0) + marshall_palmer(dbz) * 5.0 / 60.0);
            }
        }
    }
    let peak = want.iter().flatten().fold(0f64, |m, &v| m.max(v));
    assert!(peak > 1.0, "the fixture has rain: {peak} mm");
    let (mut wet, mut nodata) = (0, 0);
    for (cell, (g, w)) in got.iter().zip(&want).enumerate() {
        match w {
            Some(w) => {
                assert!(
                    (f64::from(*g) - w).abs() <= 1e-6 * peak,
                    "cell {cell}: {g} vs {w}"
                );
                wet += usize::from(*w > 0.0);
            }
            None => {
                assert!(g.is_nan(), "cell {cell}: nodata stays nodata");
                nodata += 1;
            }
        }
    }
    assert!(wet > 0 && nodata > 0);

    // EDR at the wettest cell answers the map's value.
    let (cell, &value) = got
        .iter()
        .enumerate()
        .filter(|(_, v)| v.is_finite())
        .max_by(|a, b| a.1.total_cmp(b.1))
        .unwrap();
    let (col, row) = (
        (cell % geom.nx as usize) as u32,
        (cell / geom.nx as usize) as u32,
    );
    let coords = format!("POINT({} {})", geom.lon(col), geom.lat(row));
    let CoverageResponse::Single(point) = engine
        .query_position(&coords, None, Some(&[short.to_string()]), None, None)
        .unwrap()
    else {
        panic!("one coverage")
    };
    let edr = point.ranges[short].values[0].unwrap();
    assert!((edr - f64::from(value)).abs() < 1e-6 * f64::from(value));

    // The coverage parameter shows the whole window wherever every frame
    // has a value.
    let coverage = engine
        .get_raster_tile(
            geom.bbox(),
            geom.nx,
            geom.ny,
            None,
            &OutputCrs::Wgs84,
            Some("coverage_PT20M"),
            None,
            None,
        )
        .unwrap();
    assert_eq!(coverage.values.value_at(cell), Some(1.0));

    let (polls, reads, _) = engine.metrics();
    assert_eq!((polls, reads), (1, 4), "four frames read once each");
    assert_eq!(engine.grid_bytes(), 6 * cells as u64, "one window's grids");
    println!(
        "{} x {} cells, {} bytes of grids, peak {peak:.2} mm in 20 min, {wet} wet cells",
        geom.nx,
        geom.ny,
        engine.grid_bytes()
    );
}
