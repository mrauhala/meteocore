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
