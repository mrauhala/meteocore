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
use ds_core::model::CoverageResponse;
use engine_zarr::ZarrEngine;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/zarr-era5-t2m")
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
        e.query_position("POINT(5.5 54.5)", None, None, None)
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
        e.query_position("POINT(5.5 54.5)", None, None, None)
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
        )
        .unwrap(),
    );
    let nd = qr.ranges.get("t2m").unwrap();
    assert_eq!(nd.shape, vec![1]);
    let v = nd.values[0].unwrap();
    assert!((v - 279.155).abs() < 0.02, "value {v}");
}

#[test]
fn off_grid_position_is_nodata() {
    let e = engine();
    let qr = single(
        e.query_position("POINT(100.0 0.0)", None, Some(&["t2m".to_string()]), None)
            .unwrap(),
    );
    let nd = qr.ranges.get("t2m").unwrap();
    assert!(
        nd.values.iter().all(|v| v.is_none()),
        "out-of-grid → nodata"
    );
}
