//! End-to-end over `testdata/bufr-synop/`: eight real SYNOP messages captured
//! from the WIS2 Global Broker on 2026-09-12 08:00Z (templates 307080,
//! 307084 + extras, 307096; master tables 28–34). Expected values were
//! cross-checked with ecCodes `bufr_dump -p`.

use std::path::PathBuf;

use chrono::{DateTime, TimeZone, Utc};
use ds_core::config::{BufrConfig, BufrParameterConfig};
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::{Bbox, DatetimeInterval, FeatureQuery, SortKey};
use ds_core::feature_engine::FeatureEngine;
use ds_core::model::{CoverageResponse, DomainDescription};
use engine_bufr::decode::xy_from_code;
use engine_bufr::{BufrEngine, Decoder};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/bufr-synop")
}

fn config() -> BufrConfig {
    BufrConfig {
        data_path: Some(fixtures().to_string_lossy().into_owned()),
        wis2: None,
        poll_interval_secs: 60,
        // The fixtures are from 2026-09-12; keep them in-window whenever the
        // tests run.
        retention: "P36500D".to_string(),
        max_stations: 50_000,
        stale_after: "PT2H".to_string(),
        position_radius_km: 25.0,
        builtin_parameters: true,
        parameters: Vec::new(),
    }
}

fn engine() -> BufrEngine {
    BufrEngine::new(&config(), "bufr-test").unwrap()
}

fn t0800() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 12, 8, 0, 0).unwrap()
}

const SMHI: &str = "0-20000-0-02598";
const ARG: &str = "0-32-0-0087574CONV";
const ZA: &str = "0-20000-0-68155";

#[test]
fn decoder_reads_every_fixture_with_eccodes_values() {
    let d = Decoder::new();
    let mut n = 0;
    for entry in std::fs::read_dir(fixtures()).unwrap() {
        let p = entry.unwrap().path();
        if p.extension().and_then(|e| e.to_str()) != Some("bufr") {
            continue;
        }
        let out = d.decode(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(out.messages, 1, "{}", p.display());
        assert_eq!(out.reports.len(), 1, "{}", p.display());
        assert!(out.skipped.is_empty(), "{}", p.display());
        n += 1;
    }
    assert_eq!(n, 8);

    // SMHI Östergarnsholm, 307080: WIGOS id from 001125-001128, name, T/Td/RH,
    // wind; pressure missing.
    let smhi = d
        .decode(&std::fs::read(fixtures().join("synop_se-smhi_20260912T0800Z.bufr")).unwrap())
        .unwrap()
        .reports
        .remove(0);
    assert_eq!(smhi.station_id, SMHI);
    assert_eq!(smhi.name.as_deref(), Some("OSTERGARNSHOLM"));
    assert!((smhi.lat - 57.44075).abs() < 1e-4 && (smhi.lon - 18.98391).abs() < 1e-4);
    assert_eq!(smhi.elevation, Some(8.1));
    assert_eq!(smhi.time, t0800());
    let v = |code: &str, period: Option<f64>| smhi.value(xy_from_code(code).unwrap(), period);
    assert!((v("012101", None).unwrap() - 290.12).abs() < 1e-6);
    assert!((v("012103", None).unwrap() - 286.48).abs() < 1e-6);
    assert_eq!(v("013003", None), Some(79.0));
    assert_eq!(v("011001", None), Some(202.0));
    assert!((v("011002", None).unwrap() - 4.1).abs() < 1e-6);
    assert_eq!(v("010051", None), None);
    assert_eq!(v("020003", None), Some(509.0));

    // Argentina Morón, 307096: traditional block/station present but the
    // WIGOS id wins; MSL pressure + 3 h tendency; first (2 m) temperature
    // wins over the later sensor-height replicas.
    let ar = d
        .decode(&std::fs::read(fixtures().join("synop_ar-smn_20260912T0800Z.bufr")).unwrap())
        .unwrap()
        .reports
        .remove(0);
    assert_eq!(ar.station_id, ARG);
    assert_eq!(ar.name.as_deref(), Some("MORON AERO"));
    let v = |code: &str, period: Option<f64>| ar.value(xy_from_code(code).unwrap(), period);
    assert_eq!(v("010051", None), Some(102440.0));
    assert_eq!(v("010061", None), Some(230.0));
    assert!((v("012101", None).unwrap() - 284.55).abs() < 1e-6);
    assert_eq!(v("020001", None), Some(10000.0));
    assert_eq!(v("020010", None), Some(100.0));
    assert_eq!(v("011001", None), Some(180.0));

    // South Africa, 307080 with extremes over 24 h and station pressure
    // (010004 = 926.5 hPa at 839 m; ecCodes labels it `nonCoordinatePressure`
    // — its `pressure` key is the 007004 standard-level coordinate, 850 hPa).
    let za = d
        .decode(&std::fs::read(fixtures().join("synop_za-weathersa_20260912T0800Z.bufr")).unwrap())
        .unwrap()
        .reports
        .remove(0);
    assert_eq!(za.station_id, ZA);
    let v = |code: &str, period: Option<f64>| za.value(xy_from_code(code).unwrap(), period);
    assert_eq!(v("010004", None), Some(92650.0));
    assert!((v("012111", Some(24.0)).unwrap() - 300.55).abs() < 1e-6);
    assert!((v("012112", Some(24.0)).unwrap() - 286.85).abs() < 1e-6);
    assert_eq!(v("012111", Some(12.0)), None);
    assert!((v("011002", None).unwrap() - 2.6).abs() < 1e-6);
}

#[test]
fn edr_locations_parameters_and_extents() {
    let e = engine();
    assert!(e.is_loaded());
    let locs = e.get_locations().unwrap();
    assert_eq!(locs.len(), 8);
    let smhi = locs.iter().find(|l| l.id == SMHI).unwrap();
    assert_eq!(smhi.label, "OSTERGARNSHOLM");
    assert!((smhi.latitude - 57.44075).abs() < 1e-4);
    // Stations without a name label as their id.
    let bz = locs.iter().find(|l| l.id == "0-84-100-9907603").unwrap();
    assert_eq!(bz.label, bz.id);

    let params = e.get_parameters();
    assert_eq!(params[0], "air_temperature");
    assert!(params.contains(&"precipitation_24h".to_string()));
    let d = e.get_parameter_descriptions();
    assert_eq!(d["air_temperature"].unit, "K");
    assert_eq!(
        d["pressure_msl"].observed_property,
        "air_pressure_at_mean_sea_level"
    );

    let (t_min, t_max) = e.get_temporal_extent().unwrap();
    assert_eq!(t_min, t0800());
    assert_eq!(t_max, Utc.with_ymd_and_hms(2026, 9, 12, 8, 20, 0).unwrap()); // rw-rma
    let ext = e.get_spatial_extent().unwrap();
    assert!(
        ext[0] < -88.0 && ext[2] > 122.0 && ext[1] < -34.0 && ext[3] > 57.0,
        "{ext:?}"
    );
    assert_eq!(
        e.supported_query_types(),
        vec!["locations", "position", "area", "radius"]
    );
    assert_eq!(e.gauges(), (8, 8));
}

#[test]
fn edr_location_series_position_and_parameter_filter() {
    let e = engine();
    let CoverageResponse::Single(q) = e.query_location(SMHI, None, None, None, None).unwrap()
    else {
        panic!("expected a single coverage");
    };
    match &q.domain {
        DomainDescription::PointSeries { x, y, t, z } => {
            assert!((x - 18.98391).abs() < 1e-4 && (y - 57.44075).abs() < 1e-4);
            assert_eq!(t, &vec![t0800()]);
            assert!(z.is_none());
        }
        other => panic!("unexpected domain {other:?}"),
    }
    assert_eq!(q.ranges.len(), e.get_parameters().len());
    let temp = &q.ranges["air_temperature"];
    assert_eq!(temp.shape, vec![1]);
    assert!((temp.values[0].unwrap() - 290.12).abs() < 1e-3);
    assert_eq!(q.ranges["pressure_msl"].values[0], None);
    assert_eq!(q.parameters["relative_humidity"].unit, "%");

    // Parameter filter keeps only known names.
    let CoverageResponse::Single(q) = e
        .query_location(
            SMHI,
            None,
            Some(&["wind_speed".to_string(), "nope".to_string()]),
            None,
            None,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(q.ranges.keys().collect::<Vec<_>>(), vec!["wind_speed"]);

    // Position: 5 km from Östergarnsholm hits it; the middle of the Baltic
    // is beyond 25 km → 404.
    let CoverageResponse::Single(q) = e
        .query_position("POINT(19.05 57.40)", None, None, None, None)
        .unwrap()
    else {
        panic!()
    };
    assert!((q.ranges["air_temperature"].values[0].unwrap() - 290.12).abs() < 1e-3);
    assert!(matches!(
        e.query_position("POINT(20.0 58.5)", None, None, None, None),
        Err(DataServerError::LocationNotFound(_))
    ));
    // Unknown station / empty window → 404.
    assert!(matches!(
        e.query_location("0-0-0-nope", None, None, None, None),
        Err(DataServerError::LocationNotFound(_))
    ));
    let later = Utc.with_ymd_and_hms(2026, 9, 12, 9, 0, 0).unwrap();
    assert!(matches!(
        e.query_location(SMHI, Some((later, later)), None, None, None),
        Err(DataServerError::LocationNotFound(_))
    ));
}

#[test]
fn edr_area_and_radius() {
    let e = engine();
    // Baltic box: only SMHI.
    let CoverageResponse::Collection(c) = e
        .query_area(
            "POLYGON((10 55,30 55,30 70,10 70,10 55))",
            None,
            None,
            None,
            None,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(c.len(), 1);
    // Whole world.
    let CoverageResponse::Collection(c) = e
        .query_area("-180,-90,180,90", None, None, None, None)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(c.len(), 8);
    // Empty area → empty collection, not an error.
    let CoverageResponse::Collection(c) =
        e.query_area("0,80,1,81", None, None, None, None).unwrap()
    else {
        panic!()
    };
    assert!(c.is_empty());
    // Radius goes through the default trait impl (point + within → area).
    let CoverageResponse::Collection(c) = e
        .query_radius("POINT(19 57.4)", 50_000.0, None, None, None, None)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(c.len(), 1);
}

#[test]
fn features_station_points_bbox_datetime_and_sortby() {
    let e = engine();
    assert_eq!(e.feature_count(), 8);
    let f = e.get_feature(SMHI).unwrap();
    assert_eq!(f.properties["name"].as_str(), Some("OSTERGARNSHOLM"));
    assert_eq!(f.properties["elevation"].as_f64(), Some(8.1));
    assert_eq!(
        f.properties["last_report"].as_str(),
        Some("2026-09-12T08:00:00Z")
    );
    assert_eq!(f.properties["report_count"].as_f64(), Some(1.0));
    assert!(e.get_feature("nope").is_err());

    let page = e
        .get_features(&FeatureQuery {
            bbox: Some(Bbox::new(10.0, 55.0, 30.0, 70.0).unwrap()),
            ..FeatureQuery::default()
        })
        .unwrap();
    assert_eq!(page.number_matched, 1);
    assert_eq!(page.features[0].id, SMHI);

    // datetime: only rw-rma reported at 08:20.
    let page = e
        .get_features(&FeatureQuery {
            datetime: Some(DatetimeInterval {
                start: Some(Utc.with_ymd_and_hms(2026, 9, 12, 8, 10, 0).unwrap()),
                end: None,
            }),
            ..FeatureQuery::default()
        })
        .unwrap();
    assert_eq!(page.number_matched, 1);
    assert_eq!(page.features[0].id, "0-20000-0-64384");

    // sortby last_report desc puts rw-rma first; paging is stable.
    let page = e
        .get_features(&FeatureQuery {
            sortby: vec![SortKey::descending("last_report")],
            limit: 2,
            ..FeatureQuery::default()
        })
        .unwrap();
    assert_eq!(page.features[0].id, "0-20000-0-64384");
    assert_eq!(page.number_returned, 2);
    assert_eq!(page.next_offset, Some(2));
    assert!(e.sortables().contains(&"last_report"));
    assert!(e.spatial_extent().is_some() && e.temporal_extent().is_some());
    assert!(e.data_version() > 0);
}

#[test]
fn custom_parameter_table_and_builtin_opt_out() {
    let mut cfg = config();
    cfg.builtin_parameters = false;
    cfg.parameters = vec![BufrParameterConfig {
        name: "t2m".into(),
        descriptors: vec!["012101".into()],
        unit: "K".into(),
        label: Some("2 m temperature".into()),
        period_hours: None,
        observed_property: None,
    }];
    let e = BufrEngine::new(&cfg, "bufr-custom").unwrap();
    assert_eq!(e.get_parameters(), vec!["t2m"]);
    let CoverageResponse::Single(q) = e.query_location(SMHI, None, None, None, None).unwrap()
    else {
        panic!()
    };
    assert!((q.ranges["t2m"].values[0].unwrap() - 290.12).abs() < 1e-3);
    assert_eq!(q.parameters["t2m"].label, "2 m temperature");
}

#[test]
fn retention_window_rejects_stale_fixtures() {
    // With a 24 h retention the 2026-09-12 fixtures are out of window
    // (unless the tests run on that very day) — nothing is served, but the
    // scan itself succeeded so the collection is loaded.
    let mut cfg = config();
    cfg.retention = "PT24H".to_string();
    let e = BufrEngine::new(&cfg, "bufr-stale").unwrap();
    assert!(e.is_loaded());
    let now = Utc::now();
    if now - t0800() > chrono::Duration::hours(25) {
        assert_eq!(e.feature_count(), 0);
        assert_eq!(
            e.health
                .reports_out_of_window_total
                .load(std::sync::atomic::Ordering::Relaxed),
            8
        );
    }
}
