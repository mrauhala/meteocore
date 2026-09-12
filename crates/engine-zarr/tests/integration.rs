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

/// A geographic store whose longitude axis runs every 2° from −178 to 178
/// (so an area query across the seam lands on native-resolution cells on
/// both sides), three latitudes, two timesteps, field
/// `300 + 0.1·lat + 0.01·lon + t`.
fn write_seam_store(dir: &std::path::Path) {
    use zarrs::array::{data_type, ArrayBuilder, ArraySubset};
    use zarrs::filesystem::FilesystemStore;
    use zarrs::group::GroupBuilder;

    let obj = |v: serde_json::Value| v.as_object().unwrap().clone();
    let store = std::sync::Arc::new(FilesystemStore::new(dir).unwrap());
    GroupBuilder::new()
        .build(store.clone(), "/")
        .unwrap()
        .store_metadata()
        .unwrap();
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
    let t0 = Utc
        .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
        .unwrap()
        .timestamp() as f64;
    fcoord(
        "/time",
        &[t0, t0 + 3600.0],
        "time",
        serde_json::json!({"units":"seconds since 1970-01-01","standard_name":"time"}),
    );
    let lats = [20.0_f64, 15.0, 10.0];
    let lons: Vec<f64> = (0..179).map(|i| -178.0 + 2.0 * i as f64).collect();
    fcoord(
        "/lat",
        &lats,
        "lat",
        serde_json::json!({"units":"degrees_north","standard_name":"latitude"}),
    );
    fcoord(
        "/lon",
        &lons,
        "lon",
        serde_json::json!({"units":"degrees_east","standard_name":"longitude"}),
    );
    let mut temp = Vec::new();
    for t in 0..2 {
        for &lat in &lats {
            for &lon in lons.iter() {
                temp.push((300.0 + 0.1 * lat + 0.01 * lon + t as f64) as f32);
            }
        }
    }
    let a = ArrayBuilder::new(
        vec![2, 3, 179],
        vec![2, 3, 179],
        data_type::float32(),
        f32::NAN,
    )
    .dimension_names(Some(["time", "lat", "lon"]))
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

/// A plain geographic store with one timestep, the given axes and one
/// constant-valued float32 variable per name — for the request-budget tests.
fn write_grid_store(dir: &std::path::Path, lats: &[f64], lons: &[f64], names: &[&str]) {
    use zarrs::array::{data_type, ArrayBuilder, ArraySubset};
    use zarrs::filesystem::FilesystemStore;
    use zarrs::group::GroupBuilder;

    let obj = |v: serde_json::Value| v.as_object().unwrap().clone();
    let store = std::sync::Arc::new(FilesystemStore::new(dir).unwrap());
    GroupBuilder::new()
        .build(store.clone(), "/")
        .unwrap()
        .store_metadata()
        .unwrap();
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
    let t0 = Utc
        .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
        .unwrap()
        .timestamp() as f64;
    fcoord(
        "/time",
        &[t0],
        "time",
        serde_json::json!({"units":"seconds since 1970-01-01","standard_name":"time"}),
    );
    fcoord(
        "/lat",
        lats,
        "lat",
        serde_json::json!({"units":"degrees_north","standard_name":"latitude"}),
    );
    fcoord(
        "/lon",
        lons,
        "lon",
        serde_json::json!({"units":"degrees_east","standard_name":"longitude"}),
    );
    let n = lats.len() as u64 * lons.len() as u64;
    for (i, name) in names.iter().enumerate() {
        let a = ArrayBuilder::new(
            vec![1, lats.len() as u64, lons.len() as u64],
            vec![1, lats.len() as u64, lons.len() as u64],
            data_type::float32(),
            f32::NAN,
        )
        .dimension_names(Some(["time", "lat", "lon"]))
        .attributes(obj(serde_json::json!({"units":"K","long_name":name})))
        .build(store.clone(), &format!("/{name}"))
        .unwrap();
        a.store_metadata().unwrap();
        a.store_chunks(
            &ArraySubset::new_with_shape(a.chunk_grid_shape().to_vec()),
            vec![i as f32; n as usize],
        )
        .unwrap();
    }
}

/// Review on #674: the two hard caps on the blocking store read must reject,
/// not just exist.
#[test]
fn area_query_caps_variables_per_request() {
    let dir = tempfile::tempdir().unwrap();
    let names: Vec<String> = (0..9).map(|i| format!("v{i}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    write_grid_store(dir.path(), &[52.0, 51.0], &[10.0, 11.0], &refs);
    let cfg = ZarrConfig {
        data_path: Some(dir.path().to_string_lossy().into_owned()),
        ..config(None)
    };
    let e = ZarrEngine::new("many", &cfg).unwrap();
    assert_eq!(e.get_parameters().len(), 9);
    let err = e
        .query_area("10,51,11,52", None, None, None, None)
        .unwrap_err();
    match err {
        ds_core::error::DataServerError::QueryTooLarge(m) => {
            assert!(m.contains("parameter-name"), "{m}")
        }
        other => panic!("expected QueryTooLarge, got {other}"),
    }
    // Selecting a subset is fine.
    let qr = single(
        e.query_area("10,51,11,52", None, Some(&names[..2]), None, None)
            .unwrap(),
    );
    assert_eq!(qr.ranges.len(), 2);
}

#[test]
fn area_query_budgets_the_native_read() {
    // 1100 × 1000 native cells > the 1M-value budget even though the output
    // grid is coarsened to 256 × 256.
    let dir = tempfile::tempdir().unwrap();
    let lats: Vec<f64> = (0..1000).map(|i| 80.0 - i as f64 * 0.1).collect();
    let lons: Vec<f64> = (0..1100).map(|i| -100.0 + i as f64 * 0.1).collect();
    write_grid_store(dir.path(), &lats, &lons, &["t"]);
    let cfg = ZarrConfig {
        data_path: Some(dir.path().to_string_lossy().into_owned()),
        ..config(None)
    };
    let e = ZarrEngine::new("fine", &cfg).unwrap();
    let err = e
        .query_area("-100,-20,10,80", None, None, None, None)
        .unwrap_err();
    match err {
        ds_core::error::DataServerError::QueryTooLarge(m) => {
            assert!(m.contains("native-resolution"), "{m}")
        }
        other => panic!("expected QueryTooLarge, got {other}"),
    }
    // A sub-budget window of the same store succeeds.
    let qr = single(
        e.query_area("-50,20,-40,30", None, None, None, None)
            .unwrap(),
    );
    assert_eq!(qr.ranges["t"].shape, vec![100, 100]);
}

/// Review on #674: an antimeridian-crossing area bbox used to produce an
/// all-null 200 because the store window needs min ≤ max. It is now read
/// as one window per side of the seam.
#[test]
fn area_query_across_the_antimeridian_reads_both_sides() {
    let dir = tempfile::tempdir().unwrap();
    write_seam_store(dir.path());
    let cfg = ZarrConfig {
        data_path: Some(dir.path().to_string_lossy().into_owned()),
        ..config(None)
    };
    let e = ZarrEngine::new("seam", &cfg).unwrap();
    let qr = single(
        e.query_area("170,8,-170,22", None, None, None, None)
            .unwrap(),
    );
    let ds_core::model::DomainDescription::Grid { x, y, t, .. } = &qr.domain else {
        panic!("expected a Grid domain");
    };
    assert_eq!(t.as_ref().map(Vec::len), Some(2));
    assert!(x.iter().any(|&lon| lon > 170.0) && x.iter().any(|&lon| lon < -170.0));
    let nd = &qr.ranges["temp"];
    let per_t = x.len() * y.len();
    let (mut east_side, mut west_side) = (0, 0);
    for (iy, &lat) in y.iter().enumerate() {
        for (ix, &lon) in x.iter().enumerate() {
            let v = nd.values[iy * x.len() + ix];
            let v1 = nd.values[per_t + iy * x.len() + ix];
            // Cells beyond the outermost native columns (|lon| > 178) are
            // off-grid and null; every other cell carries the field.
            if lon.abs() <= 178.0 && (10.0..=20.0).contains(&lat) {
                let v = v.unwrap_or_else(|| panic!("({lon}, {lat}) is null"));
                let exp = 300.0 + 0.1 * lat + 0.01 * lon;
                assert!(
                    (v - exp).abs() < 1e-3,
                    "({lon}, {lat}) = {v}, expected {exp}"
                );
                assert!(
                    (v1.unwrap() - v - 1.0).abs() < 1e-3,
                    "second step rises by 1"
                );
                if lon > 0.0 {
                    east_side += 1
                } else {
                    west_side += 1
                }
            }
        }
    }
    assert!(
        east_side > 0 && west_side > 0,
        "both sides of the seam: {east_side}/{west_side}"
    );
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
    let rendered: Vec<f64> = tile.values.iter_values().flatten().collect();
    assert!(!rendered.is_empty(), "render path produced no data");
    assert!(
        rendered.iter().all(|&v| v >= 1000.0),
        "raster render must read the latest run (init=1), got {rendered:?}"
    );
}

/// Model runs as EDR instances (#337): every run on the reference axis is an
/// instance with its own valid times, selectable on EDR queries, the render
/// path and the two cache-key resolvers.
#[test]
fn forecast_runs_are_instances_and_selectable() {
    let dir = tempfile::tempdir().unwrap();
    write_forecast_store(dir.path());
    let cfg = ZarrConfig {
        data_path: Some(dir.path().to_string_lossy().into_owned()),
        ..config(None)
    };
    let e = ZarrEngine::new("fc", &cfg).unwrap();
    let run0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let run1 = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();

    assert!(e.has_instances());
    let inst = e.get_instances();
    assert_eq!(
        inst.iter().map(|r| r.reference_time).collect::<Vec<_>>(),
        vec![run0, run1],
        "ascending by reference time"
    );
    assert_eq!(inst[0].instance_id(), "20260101T0000Z");
    assert_eq!(
        inst[0].valid_times,
        vec![
            run0,
            run0 + chrono::Duration::hours(1),
            run0 + chrono::Duration::hours(2)
        ]
    );
    assert_eq!(e.find_instance(run0).unwrap().valid_times.len(), 3);
    assert!(e.find_instance(run0 + chrono::Duration::hours(6)).is_none());
    assert_eq!(e.raster_info().reference_times, vec![run0, run1]);

    // EDR position against run 0: value = 0*1000 + lead + 0.1*60 + 0.01*10.
    let qr = single(
        e.query_position("POINT(10 60)", None, None, None, Some(run0))
            .unwrap(),
    );
    let ds_core::model::DomainDescription::PointSeries { t, .. } = &qr.domain else {
        panic!("expected PointSeries");
    };
    assert_eq!(t[0], run0, "valid times belong to the selected run");
    let vals = &qr.ranges["temp"].values;
    assert!((vals[0].unwrap() - 6.1).abs() < 0.05, "{vals:?}");
    assert!((vals[2].unwrap() - 8.1).abs() < 0.05, "{vals:?}");
    // A datetime window inside run 0's valid times but outside run 1's.
    let qr = single(
        e.query_position(
            "POINT(10 60)",
            Some((run0, run0 + chrono::Duration::hours(1))),
            None,
            None,
            Some(run0),
        )
        .unwrap(),
    );
    assert_eq!(qr.ranges["temp"].values.len(), 2);
    // Area against run 0 reads run 0 too.
    let qr = single(
        e.query_area("9.5,58.5,11.5,60.5", None, None, None, Some(run0))
            .unwrap(),
    );
    assert!(qr.ranges["temp"]
        .values
        .iter()
        .flatten()
        .all(|&v| v < 1000.0));
    // Unknown run → ReferenceTimeNotFound; None → latest.
    let err = e
        .query_position(
            "POINT(10 60)",
            None,
            None,
            None,
            Some(run0 + chrono::Duration::hours(3)),
        )
        .unwrap_err();
    assert!(
        matches!(
            err,
            ds_core::error::DataServerError::ReferenceTimeNotFound(_)
        ),
        "{err}"
    );

    // Render path honours the run, and the cache-key resolvers agree with it.
    let tile = e
        .get_raster_tile(
            [9.5, 58.5, 11.5, 60.5],
            4,
            4,
            Some(run0),
            &OutputCrs::Wgs84,
            Some("temp"),
            None,
            Some(run0),
        )
        .unwrap();
    let rendered: Vec<f64> = tile.values.iter_values().flatten().collect();
    assert!(
        !rendered.is_empty() && rendered.iter().all(|&v| v < 1000.0),
        "{rendered:?}"
    );
    assert_eq!(e.resolve_reference_time(None, None), Some(run1));
    assert_eq!(e.resolve_reference_time(None, Some(run0)), Some(run0));
    assert_eq!(
        e.resolve_time(Some(run0 + chrono::Duration::minutes(40)), Some(run0)),
        Some(run0 + chrono::Duration::hours(1))
    );
    assert_eq!(
        e.resolve_time(None, None),
        Some(run1 + chrono::Duration::hours(2))
    );

    // A non-forecast store has no instances and ignores reference_time.
    let plain = engine();
    assert!(!plain.has_instances() && plain.get_instances().is_empty());
    assert!(plain.raster_info().reference_times.is_empty());
    assert_eq!(plain.resolve_reference_time(None, None), None);
    assert!(plain
        .query_position("POINT(5.5 54.5)", None, None, None, Some(run0))
        .is_ok());
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
fn supported_query_types_are_position_area_radius() {
    assert_eq!(
        engine().supported_query_types(),
        vec![
            "position".to_string(),
            "area".to_string(),
            "radius".to_string()
        ]
    );
}

#[test]
fn area_query_grid_matches_linear_field_and_masks_the_polygon() {
    let e = engine();
    // A right triangle with its right angle at the south-west corner: the
    // bbox's north-east cells lie outside the shape.
    let resp = e
        .query_area(
            "POLYGON((3 52, 8 52, 3 57, 3 52))",
            Some((
                Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
                Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            )),
            Some(&["t2m".to_string()]),
            None,
            None,
        )
        .unwrap();
    let qr = single(resp);
    let ds_core::model::DomainDescription::Grid { x, y, t, .. } = &qr.domain else {
        panic!("expected a Grid domain");
    };
    assert!(t.is_none(), "single timestep → no t axis");
    // Native 1° cells over a 5°×5° bbox.
    assert_eq!(x.len(), 5);
    assert_eq!(y.len(), 5);
    assert!(x.windows(2).all(|p| p[0] < p[1]));
    assert!(y.windows(2).all(|p| p[0] > p[1]));
    let nd = &qr.ranges["t2m"];
    assert_eq!(nd.shape, vec![5, 5]);
    assert_eq!(nd.axis_names, vec!["y", "x"]);
    let mut inside = 0;
    for (r, &lat) in y.iter().enumerate() {
        for (c, &lon) in x.iter().enumerate() {
            let v = nd.values[r * 5 + c];
            // Inside the triangle ⇔ lon - 3 + lat - 52 ≤ 5 at the cell centre
            // (the boundary is inclusive, so centres on the hypotenuse count).
            let expect_inside = (lon - 3.0) + (lat - 52.0) <= 5.0;
            match v {
                Some(v) => {
                    assert!(
                        expect_inside,
                        "cell ({lon}, {lat}) outside the triangle has a value"
                    );
                    let exp = 273.15 + 0.1 * lat + 0.01 * lon; // f32 store: 1e-4 tolerance
                    assert!(
                        (v - exp).abs() < 1e-4,
                        "({lon}, {lat}) = {v}, expected {exp}"
                    );
                    inside += 1;
                }
                None => assert!(
                    !expect_inside,
                    "cell ({lon}, {lat}) inside the triangle is null"
                ),
            }
        }
    }
    // Centres on the hypotenuse (offset sum == 5) are boundary points and the
    // boundary is inclusive: 10 strictly inside + 5 on the edge.
    assert_eq!(inside, 15, "10 interior + 5 hypotenuse cell centres");
}

#[test]
fn area_query_all_timesteps_has_t_axis() {
    let e = engine();
    let qr = single(e.query_area("4,53,6,55", None, None, None, None).unwrap());
    let ds_core::model::DomainDescription::Grid { x, y, t, .. } = &qr.domain else {
        panic!("expected a Grid domain");
    };
    let t = t.as_ref().expect("all timesteps → t axis");
    assert_eq!(t.len(), 4);
    for key in ["t2m", "t2m_packed"] {
        let nd = &qr.ranges[key];
        assert_eq!(nd.shape, vec![4, y.len(), x.len()]);
        assert_eq!(nd.axis_names, vec!["t", "y", "x"]);
        // The field rises 0.5 K per timestep at every cell.
        let per_t = y.len() * x.len();
        for cell in 0..per_t {
            let a = nd.values[cell].unwrap();
            let b = nd.values[3 * per_t + cell].unwrap();
            assert!((b - a - 1.5).abs() < 0.03, "{key} cell {cell}: {a} → {b}");
        }
    }
}

#[test]
fn area_query_outside_extent_is_not_found() {
    let err = engine()
        .query_area("100,10,101,11", None, None, None, None)
        .unwrap_err();
    assert!(
        matches!(err, ds_core::error::DataServerError::LocationNotFound(_)),
        "{err}"
    );
}

#[test]
fn window_dims_budget_the_native_read() {
    // Reach the catalog through the engine's public surface: an area over the
    // whole fixture reads at most the native 16 × 12 grid per step, and the
    // response is well under the budget — while an off-grid bbox is None.
    let e = engine();
    let qr = single(
        e.query_area(
            "-0.5,48.5,15.5,60.5",
            None,
            Some(&["t2m".to_string()]),
            None,
            None,
        )
        .unwrap(),
    );
    let nd = &qr.ranges["t2m"];
    assert_eq!(nd.shape[0], 4);
    assert!(nd.shape[1] <= 12 && nd.shape[2] <= 16, "{:?}", nd.shape);
}

#[test]
fn radius_query_delegates_to_area() {
    let e = engine();
    let qr = single(
        e.query_radius(
            "POINT(5.5 54.5)",
            250_000.0,
            None,
            Some(&["t2m".to_string()]),
            None,
            None,
        )
        .unwrap(),
    );
    let ds_core::model::DomainDescription::Grid { x, y, .. } = &qr.domain else {
        panic!("expected a Grid domain");
    };
    let nd = &qr.ranges["t2m"];
    // A 250 km disc spans ~4.5° of latitude → ≥ 4 native 1° cells per axis, so
    // the bounding square's corner cell centres (~1.1 r out) are masked.
    assert!(x.len() >= 4 && y.len() >= 4, "grid {}×{}", x.len(), y.len());
    assert!(nd.values[0].is_none(), "NW corner must be masked");
    let centre = ((y.len() / 2) * x.len()) + x.len() / 2;
    assert!(nd.values[centre].is_some(), "centre must have data");
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
    assert!(tile.values.iter_values().flatten().all(|v| v.is_finite()));
    let v = tile
        .values
        .value_at(6 * 16 + 8)
        .expect("pixel (8,6) has data");
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
        tile.values.iter_values().flatten().all(|v| v.is_finite()),
        "no NaN may leak through the projected path"
    );
    assert!(
        tile.values.iter_values().filter(|v| v.is_some()).count() > 0,
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
        tile.values.iter_values().all(|v| v.is_none()),
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
        tile.values.iter_values().any(|v| v.is_some()),
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
