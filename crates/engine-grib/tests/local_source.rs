//! Local `data_path` source for the GRIB engine (#327).
//!
//! Constructs a `GribEngine` over a committed local directory (a single ECMWF
//! message `q` @ 150 hPa plus a 1-entry ecmwf-json index) and verifies the full
//! local path: list the directory → parse the index → byte-range read + decode
//! the message → serve it via EDR + Maps. Before #327 the engine was S3/HTTP
//! only, so this exercised path is new.

use ds_core::config::GribConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::map_engine::{MapEngine, OutputCrs};
use engine_grib::GribEngine;

fn local_config() -> GribConfig {
    GribConfig {
        level_types: None,
        data_path: Some("../../testdata/grib-local".to_string()),
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        index_suffix: None,
        data_suffix: None,
        poll_interval_secs: 600,
        max_runs: None,
        time_window: None,
        parameters: None,
        grid_cache_mb: 256,
        message_cache_mb: 0,
        run_hours: None,
        index_format: Some("ecmwf-json".to_string()),
        filename_contains: None,
    }
}

#[test]
fn grib_engine_serves_local_directory() {
    // Constructing the engine runs the initial scan: it lists the local dir,
    // parses sample-message.index, and eager-probes the message via a *local*
    // byte-range read — so a populated parameter list already proves the path.
    let engine =
        GribEngine::new("grib-local-test", &local_config()).expect("engine builds from data_path");

    let params = engine.get_parameters();
    assert!(
        !params.is_empty(),
        "local grib must expose its parameter(s); got {params:?}"
    );

    let (start, end) = engine
        .get_temporal_extent()
        .expect("local grib must have a temporal extent");
    assert_eq!(start, end, "single-step fixture has one timestamp");
    assert_eq!(
        start.format("%Y-%m-%dT%H:%M").to_string(),
        "2026-04-05T00:00",
        "run/valid time from the index (date 20260405, time 0000, step 0)"
    );

    // On-demand render exercises the local byte-range read + decode + resample
    // end-to-end (not just the eager probe).
    let info = engine.raster_info();
    let descriptions = engine.get_parameter_descriptions();
    for p in &info.parameters {
        assert_eq!(p.unit, descriptions[&p.name].unit);
        assert!(!p.unit.is_empty());
    }
    let param = info.parameter.clone();
    let bbox = info.spatial_extent.unwrap_or([-180.0, -90.0, 180.0, 90.0]);
    let tile = engine
        .get_raster_tile(
            bbox,
            32,
            16,
            Some(start),
            &OutputCrs::Wgs84,
            Some(&param),
            None,
            None,
        )
        .expect("render a tile from the local grib");
    assert_eq!(tile.values.len(), 32 * 16);
    assert!(
        tile.values.iter_values().any(|v| v.is_some()),
        "rendered tile should contain data values"
    );
}

/// #671: an area query masks cells whose centre lies outside the polygon
/// instead of returning the polygon's whole bounding box.
#[test]
fn grib_area_query_masks_outside_the_polygon() {
    let engine = GribEngine::new("grib-local-test", &local_config()).expect("engine builds");
    let [w, s, e, n] = engine.get_spatial_extent().expect("extent");
    let (x0, x1) = (w + 0.25 * (e - w), w + 0.75 * (e - w));
    let (y0, y1) = (s + 0.25 * (n - s), s + 0.75 * (n - s));
    // Right triangle with the right angle at the south-west corner: the
    // bbox's north-east cell is outside, its south-west cell inside.
    let coords = format!("POLYGON(({x0} {y0}, {x1} {y0}, {x0} {y1}, {x0} {y0}))");
    let param = engine.get_parameters()[0].clone();
    let resp = engine
        .query_area(
            &coords,
            None,
            Some(std::slice::from_ref(&param)),
            None,
            None,
        )
        .expect("area query");
    let ds_core::model::CoverageResponse::Single(res) = resp else {
        panic!("expected a single Grid coverage");
    };
    let ds_core::model::DomainDescription::Grid { x, y, .. } = &res.domain else {
        panic!("expected a Grid domain");
    };
    let arr = &res.ranges[&param];
    assert_eq!(arr.shape, vec![y.len(), x.len()]);
    assert!(x.len() >= 2 && y.len() >= 2, "grid {}×{}", x.len(), y.len());
    // y ascends (south first): the last row / last column is the NE corner.
    assert!(
        arr.values[arr.values.len() - 1].is_none(),
        "NE corner must be masked"
    );
    assert!(arr.values[0].is_some(), "SW corner must carry data");
    let inside = arr.values.iter().filter(|v| v.is_some()).count();
    assert!(
        inside * 3 < arr.values.len() * 2,
        "about half the bbox is masked: {inside}/{}",
        arr.values.len()
    );
}

/// Discovery settings the poll would reject are load errors: an invalid
/// `time_window` (the poll used to skip its filter silently) and a
/// `prefix_pattern` with an unknown or hour specifier (the run hour is `{run}`).
#[test]
fn invalid_discovery_settings_fail_at_load() {
    let bad_window = GribConfig {
        time_window: Some("PT2H5".to_string()),
        ..local_config()
    };
    assert!(GribEngine::new("grib-local-test", &bad_window).is_err());

    let s3 = |prefix: &str| GribConfig {
        data_path: None,
        endpoint: Some("https://s3.example.com".to_string()),
        bucket: Some("models".to_string()),
        prefix_pattern: Some(prefix.to_string()),
        ..local_config()
    };
    assert!(GribEngine::new("grib-s3-test", &s3("%Y%m%d/%H/")).is_err());
    assert!(GribEngine::new("grib-s3-test", &s3("%Y%m%d/%!/")).is_err());
}

/// #475: the map path emits the compact `F32` form, and every sample is
/// exactly the pre-#475 boxed value — f64 bilinear resample plus display
/// conversion — narrowed to f32, with nodata in the same pixels. Storage
/// width is the only change; checked for all three output-CRS paths.
#[test]
fn map_tiles_are_f32_narrowings_of_the_f64_resample() {
    use ds_core::map_engine::RasterValues;
    use engine_grib::{reader::decode_message, units};

    let engine = GribEngine::new("grib-local-test", &local_config()).expect("engine builds");
    let bytes = std::fs::read("../../testdata/grib-local/sample-message.grib2").expect("fixture");
    let grid = decode_message(&bytes, "fixture").expect("decode fixture");
    let (discipline, category, number) = grid.triple;
    let display = units::lookup(grid.centre, discipline, category, number)
        .map(|info| units::default_display(info.source_unit))
        .filter(units::DisplayConversion::has_conversion);
    let time = engine.get_temporal_extent().expect("extent").0;
    let param = engine.raster_info().parameter;

    // Europe, straddling Greenwich; the projected view is the same area in
    // EPSG:3035 (the `ProjectionGrid` path).
    let bbox = [-20.0, 35.0, 40.0, 72.0];
    let crs = ds_core::geo::projected_output_crs("EPSG:3035").unwrap();
    let projected = ds_core::geo::projected_envelope(&crs, bbox);
    let read = ds_core::geo::wgs84_envelope(&crs, projected).unwrap();
    for (bbox, output) in [
        (bbox, OutputCrs::Wgs84),
        (bbox, OutputCrs::WebMercator),
        (
            read,
            OutputCrs::Projected {
                crs,
                bbox: projected,
            },
        ),
    ] {
        let (w, h) = (128, 96);
        let tile = engine
            .get_raster_tile(bbox, w, h, Some(time), &output, Some(&param), None, None)
            .expect("render");
        assert!(
            matches!(tile.values, RasterValues::F32 { nodata: None, .. }),
            "GRIB map tiles must use the compact F32 form"
        );
        let reference = grid.resample(bbox, w, h, &output);
        assert_eq!(tile.values.len(), reference.len());
        assert!(reference.iter().any(Option::is_some), "view must hit data");
        for (i, value) in reference.into_iter().enumerate() {
            let converted = value.map(|v| display.map_or(v, |d| d.convert(v)));
            assert_eq!(
                tile.values.value_at(i),
                converted.map(|v| f64::from(v as f32)),
                "pixel {i}"
            );
        }
    }
}
