//! End-to-end EDR tests for the Zarr engine against the committed
//! `testdata/zarr-era5-t2m` fixture (regenerate with
//! `cargo run -p engine-zarr --example gen_fixture`).
//!
//! The fixture's field is linear in lat/lon — `273.15 + 0.1*lat + 0.01*lon +
//! 0.5*t` — so bilinear interpolation is exact and values are predictable.

use std::path::PathBuf;

use chrono::{TimeZone, Utc};
use ds_core::config::ZarrConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::map_engine::{MapEngine, OutputCrs};
use ds_core::model::CoverageResponse;
use engine_zarr::ZarrEngine;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/zarr-era5-t2m")
}

/// Write a tiny **forecast** Zarr V3 store (dims `init_time, lead_time, lat,
/// lon`, CF `forecast_reference_time` + `forecast_period`) to `dir`. Two runs;
/// the data encodes the run index so a test can prove the *latest* run is used.
/// `temp[init, lead, lat, lon] = init*1000 + lead_idx + 0.1*lat + 0.01*lon`.
fn write_forecast_store(dir: &std::path::Path) {
    use zarrs::array::{codec::GzipCodec, data_type, ArrayBuilder, ArraySubset};
    use zarrs::filesystem::FilesystemStore;
    use zarrs::group::GroupBuilder;

    let obj = |v: serde_json::Value| v.as_object().unwrap().clone();
    let store = std::sync::Arc::new(FilesystemStore::new(dir).unwrap());
    GroupBuilder::new()
        .build(store.clone(), "/")
        .unwrap()
        .store_metadata()
        .unwrap();

    // Two runs: 2026-01-01 00Z and 12Z (12Z is the latest), as "seconds since".
    let run0 = Utc
        .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
        .unwrap()
        .timestamp();
    let run1 = Utc
        .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
        .unwrap()
        .timestamp();
    let coord = |path: &str, vals: Vec<i64>, dim: &str, at: serde_json::Value| {
        let a = ArrayBuilder::new(
            vec![vals.len() as u64],
            vec![vals.len() as u64],
            data_type::int64(),
            0i64,
        )
        .dimension_names(Some([dim]))
        .attributes(obj(at))
        .build(store.clone(), path)
        .unwrap();
        a.store_metadata().unwrap();
        a.store_chunk(&[0], vals).unwrap();
    };
    coord(
        "/init_time",
        vec![run0, run1],
        "init_time",
        serde_json::json!({"units":"seconds since 1970-01-01","standard_name":"forecast_reference_time"}),
    );
    coord(
        "/lead_time",
        vec![0, 3600, 7200],
        "lead_time",
        serde_json::json!({"units":"seconds","standard_name":"forecast_period"}),
    );
    let lats = [60.0_f64, 59.0];
    let lons = [10.0_f64, 11.0];
    let fcoord = |path: &str, vals: &[f64], dim: &str, at: serde_json::Value| {
        let a = ArrayBuilder::new(
            vec![vals.len() as u64],
            vec![vals.len() as u64],
            data_type::float64(),
            f64::NAN,
        )
        .dimension_names(Some([dim]))
        .attributes(obj(at))
        .build(store.clone(), path)
        .unwrap();
        a.store_metadata().unwrap();
        a.store_chunk(&[0], vals.to_vec()).unwrap();
    };
    fcoord(
        "/latitude",
        &lats,
        "latitude",
        serde_json::json!({"units":"degrees_north","standard_name":"latitude"}),
    );
    fcoord(
        "/longitude",
        &lons,
        "longitude",
        serde_json::json!({"units":"degrees_east","standard_name":"longitude"}),
    );

    let mut temp = Vec::new();
    for init in 0..2 {
        for lead in 0..3 {
            for &lat in &lats {
                for &lon in &lons {
                    temp.push((init as f64 * 1000.0 + lead as f64 + 0.1 * lat + 0.01 * lon) as f32);
                }
            }
        }
    }
    let a = ArrayBuilder::new(
        vec![2, 3, 2, 2],
        vec![2, 3, 2, 2],
        data_type::float32(),
        f32::NAN,
    )
    .dimension_names(Some(["init_time", "lead_time", "latitude", "longitude"]))
    .bytes_to_bytes_codecs(vec![std::sync::Arc::new(GzipCodec::new(5).unwrap())])
    .attributes(obj(
        serde_json::json!({"units":"K","long_name":"temperature"}),
    ))
    .build(store.clone(), "/temp")
    .unwrap();
    a.store_metadata().unwrap();
    a.store_chunks(
        &ArraySubset::new_with_shape(a.chunk_grid_shape().to_vec()),
        temp,
    )
    .unwrap();
}

#[test]
fn forecast_uses_latest_run_with_lead_as_time() {
    let dir = tempfile::tempdir().unwrap();
    write_forecast_store(dir.path());
    let cfg = ZarrConfig {
        data_path: Some(dir.path().to_string_lossy().into_owned()),
        endpoint: None,
        bucket: None,
        path: None,
        zarr_version: Some(3),
        parameters: None,
        poll_interval_secs: 300,
        cache_mb: 16,
        icechunk: None,
    };
    let e = ZarrEngine::new("fc", &cfg).expect("open forecast store");
    assert_eq!(e.get_parameters(), vec!["temp".to_string()]);

    // Temporal extent = the LATEST run (12Z) + leads [0h,1h,2h] → 12:00..14:00.
    let (first, last) = e.get_temporal_extent().unwrap();
    assert_eq!(first, Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap());
    assert_eq!(last, Utc.with_ymd_and_hms(2026, 1, 1, 14, 0, 0).unwrap());
    assert_eq!(e.get_available_times().unwrap().len(), 3);

    // A position query at (lon=10, lat=60) must read the LATEST run (init=1):
    // value = 1*1000 + lead_idx + 0.1*60 + 0.01*10 = 1006.1, 1007.1, 1008.1.
    let qr = single(
        e.query_position("POINT(10 60)", None, None, None, None)
            .unwrap(),
    );
    let vals = &qr.ranges.get("temp").unwrap().values;
    assert_eq!(vals.len(), 3);
    assert!(
        (vals[0].unwrap() - 1006.1).abs() < 0.05,
        "lead0 {:?}",
        vals[0]
    );
    assert!(
        (vals[2].unwrap() - 1008.1).abs() < 0.05,
        "lead2 {:?}",
        vals[2]
    );

    // The MAP render path (`read_window`, used by WMS/Maps/Tiles) must pin to
    // the same latest run. Render over the grid extent at the first valid time
    // (latest run, lead 0); every pixel must come from init=1 (value ≥ 1000),
    // never init=0 (~6).
    let tile = e
        .get_raster_tile(
            [9.5, 58.5, 11.5, 60.5],
            8,
            8,
            Some(first),
            &OutputCrs::Wgs84,
            Some("temp"),
            None,
            None,
        )
        .unwrap();
    let rendered: Vec<f64> = tile.values.iter().flatten().copied().collect();
    assert!(!rendered.is_empty(), "render path produced no data");
    assert!(
        rendered.iter().all(|&v| v >= 1000.0),
        "raster render must read the latest run (init=1), got {rendered:?}"
    );
}

fn config(parameters: Option<Vec<String>>) -> ZarrConfig {
    ZarrConfig {
        data_path: Some(fixture_dir().to_string_lossy().into_owned()),
        endpoint: None,
        bucket: None,
        path: None,
        zarr_version: Some(3),
        parameters,
        poll_interval_secs: 300,
        cache_mb: 256,
        icechunk: None,
    }
}

fn engine() -> ZarrEngine {
    assert!(fixture_dir().exists(), "zarr-era5-t2m fixture missing");
    ZarrEngine::new("zarr-test", &config(None)).expect("open fixture")
}

fn single(resp: CoverageResponse) -> ds_core::model::QueryResult {
    match resp {
        CoverageResponse::Single(qr) => qr,
        CoverageResponse::Collection(_) => panic!("expected a Single coverage"),
    }
}

#[test]
fn lists_data_variables_sorted() {
    let params = engine().get_parameters();
    assert_eq!(params, vec!["t2m".to_string(), "t2m_packed".to_string()]);
}

#[test]
fn parameter_descriptions_carry_units_and_label() {
    let descs = engine().get_parameter_descriptions();
    let t2m = descs.get("t2m").unwrap();
    assert_eq!(t2m.unit, "K");
    assert_eq!(t2m.label, "2 metre temperature");
}

#[test]
fn temporal_extent_and_available_times() {
    let e = engine();
    let (first, last) = e.get_temporal_extent().unwrap();
    assert_eq!(first, Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap());
    assert_eq!(last, Utc.with_ymd_and_hms(2026, 1, 1, 18, 0, 0).unwrap());
    assert_eq!(e.get_available_times().unwrap().len(), 4);
}

#[test]
fn spatial_extent_is_half_cell_expanded() {
    let bbox = engine().get_spatial_extent().unwrap();
    assert!((bbox[0] - (-0.5)).abs() < 1e-6, "west {}", bbox[0]);
    assert!((bbox[1] - 48.5).abs() < 1e-6, "south {}", bbox[1]);
    assert!((bbox[2] - 15.5).abs() < 1e-6, "east {}", bbox[2]);
    assert!((bbox[3] - 60.5).abs() < 1e-6, "north {}", bbox[3]);
}

#[test]
fn supported_query_types_is_position() {
    assert_eq!(
        engine().supported_query_types(),
        vec!["position".to_string()]
    );
}

#[test]
fn position_query_bilinear_matches_linear_field() {
    let e = engine();
    let qr = single(
        e.query_position("POINT(5.5 54.5)", None, None, None, None)
            .unwrap(),
    );
    assert_eq!(qr.parameters.len(), 2);

    // Expected field at (lon=5.5, lat=54.5): 273.15 + 0.1*54.5 + 0.01*5.5 = 278.655,
    // rising 0.5 K per timestep.
    let expected = [278.655, 279.155, 279.655, 280.155];
    for (key, tol) in [("t2m", 0.02_f64), ("t2m_packed", 0.02)] {
        let nd = qr.ranges.get(key).unwrap();
        assert_eq!(nd.shape, vec![4]);
        assert_eq!(nd.axis_names, vec!["t".to_string()]);
        for (i, exp) in expected.iter().enumerate() {
            let v = nd.values[i].unwrap_or_else(|| panic!("{key}[{i}] is nodata"));
            assert!((v - exp).abs() < tol, "{key}[{i}] = {v}, expected {exp}");
        }
    }
}

#[test]
fn fill_value_maps_to_nodata() {
    // The NW 2x2 block of the t=0 plane of t2m_packed is `_FillValue`. A query
    // centred in it returns None at t=0 and real values for the later steps
    // (which are not filled).
    let e = engine();
    let qr = single(
        e.query_position(
            "POINT(0.5 59.5)",
            None,
            Some(&["t2m_packed".to_string()]),
            None,
            None,
        )
        .unwrap(),
    );
    let nd = qr.ranges.get("t2m_packed").unwrap();
    assert!(nd.values[0].is_none(), "t=0 should be nodata (fill block)");
    assert!(nd.values[1].is_some(), "t=1 should have data");
    assert!(nd.values[3].is_some(), "t=3 should have data");
}

#[test]
fn parameter_filter_restricts_variables() {
    let e = ZarrEngine::new("zarr-test", &config(Some(vec!["t2m".to_string()]))).unwrap();
    assert_eq!(e.get_parameters(), vec!["t2m".to_string()]);
    let qr = single(
        e.query_position("POINT(5.5 54.5)", None, None, None, None)
            .unwrap(),
    );
    assert_eq!(qr.parameters.len(), 1);
    assert!(qr.parameters.contains_key("t2m"));
}

#[test]
fn datetime_filter_selects_single_step() {
    let e = engine();
    let t = Utc.with_ymd_and_hms(2026, 1, 1, 6, 0, 0).unwrap();
    let qr = single(
        e.query_position(
            "POINT(5.5 54.5)",
            Some((t, t)),
            Some(&["t2m".to_string()]),
            None,
            None,
        )
        .unwrap(),
    );
    let nd = qr.ranges.get("t2m").unwrap();
    assert_eq!(nd.shape, vec![1]);
    let v = nd.values[0].unwrap();
    assert!((v - 279.155).abs() < 0.02, "value {v}");
}

#[test]
fn raster_info_describes_the_grid() {
    let info = engine().raster_info();
    assert_eq!(info.native_crs, "CRS:84");
    assert_eq!(info.times.len(), 4);
    assert!(info.spatial_extent.is_some());
    assert_eq!(info.grid_size, Some([16, 12])); // [nx lon, ny lat]
    let names: Vec<&str> = info.parameters.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"t2m") && names.contains(&"t2m_packed"));
}

#[test]
fn raster_tile_wgs84_matches_linear_field() {
    let e = engine();
    // Full extent, 16x12 — pixel (col=8,row=6) centres on (lon=8.0, lat=54.0).
    let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let tile = e
        .get_raster_tile(
            [-0.5, 48.5, 15.5, 60.5],
            16,
            12,
            Some(t0),
            &OutputCrs::Wgs84,
            Some("t2m"),
            None,
            None,
        )
        .unwrap();
    assert_eq!(tile.values.len(), 16 * 12);
    assert!(tile.values.iter().flatten().all(|v| v.is_finite()));
    let v = tile.values[6 * 16 + 8].expect("pixel (8,6) has data");
    // 273.15 + 0.1*54 + 0.01*8 = 278.63 at t=0.
    assert!((v - 278.63).abs() < 0.05, "pixel value {v}");
}

#[test]
fn raster_tile_projected_via_build_2d_no_nan_leak() {
    // Exercises the OutputCrs::Projected coarse-grid path. TM math is globally
    // valid, so projecting the fixture's region into EPSG:3067 metres and back
    // must place data and never leak NaN.
    let e = engine();
    let crs = ds_core::geo::projected_output_crs("EPSG:3067").unwrap();
    let proj = ds_core::geo::projected_envelope(&crs, [1.0, 50.0, 14.0, 59.0]);
    let read = ds_core::geo::wgs84_envelope(&crs, proj).expect("in-domain envelope");
    let tile = e
        .get_raster_tile(
            read,
            16,
            16,
            None,
            &OutputCrs::Projected { crs, bbox: proj },
            Some("t2m"),
            None,
            None,
        )
        .unwrap();
    assert_eq!(tile.values.len(), 16 * 16);
    assert!(
        tile.values.iter().flatten().all(|v| v.is_finite()),
        "no NaN may leak through the projected path"
    );
    assert!(
        tile.values.iter().filter(|v| v.is_some()).count() > 0,
        "projected tile should have data"
    );
}

#[test]
fn raster_tile_off_grid_is_transparent() {
    let tile = engine()
        .get_raster_tile(
            [100.0, 0.0, 110.0, 5.0],
            8,
            8,
            None,
            &OutputCrs::Wgs84,
            Some("t2m"),
            None,
            None,
        )
        .unwrap();
    assert_eq!(tile.values.len(), 64);
    assert!(
        tile.values.iter().all(|v| v.is_none()),
        "off-grid → transparent"
    );
}

#[test]
fn raster_tile_between_cell_centres_still_renders() {
    // A tile whose bbox falls entirely *between* grid cell centres (no centre
    // inside it) must still interpolate from the bracketing cells, not render
    // transparent. lon centres 0,1,2…; lat centres …55,54…; bbox in the gaps.
    let tile = engine()
        .get_raster_tile(
            [4.3, 54.2, 4.7, 54.8],
            4,
            4,
            None,
            &OutputCrs::Wgs84,
            Some("t2m"),
            None,
            None,
        )
        .unwrap();
    assert!(
        tile.values.iter().any(|v| v.is_some()),
        "between-centres tile must interpolate, not be transparent"
    );
}

#[test]
fn off_grid_position_is_nodata() {
    let e = engine();
    let only_t2m = ["t2m".to_string()];
    // Both axes out of range, only longitude out (lat in 48.5..60.5), and only
    // latitude out — each must yield all-nodata (the sample path requires both
    // axes to locate).
    for coords in ["POINT(100.0 0.0)", "POINT(100.0 54.5)", "POINT(5.5 0.0)"] {
        let qr = single(
            e.query_position(coords, None, Some(&only_t2m), None, None)
                .unwrap(),
        );
        let nd = qr.ranges.get("t2m").unwrap();
        assert!(
            nd.values.iter().all(|v| v.is_none()),
            "{coords}: out-of-grid → nodata"
        );
    }
}
