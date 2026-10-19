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
        map: None,
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
    // Display units (BUFR K / Pa → °C / hPa, mechanically from the unit).
    assert_eq!(d["air_temperature"].unit, "°C");
    assert_eq!(d["pressure_msl"].unit, "hPa");
    assert_eq!(d["precipitation_24h"].unit, "mm");
    assert_eq!(d["wind_speed"].unit, "m s-1");
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
    // Station series at the snapshot's locations: EDR GeoJSON (#929).
    assert!(e.serves_station_series());
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
    // 290.12 K, served as 16.97 °C.
    assert!((temp.values[0].unwrap() - 16.97).abs() < 1e-3);
    assert_eq!(q.ranges["pressure_msl"].values[0], None);
    assert_eq!(q.parameters["relative_humidity"].unit, "%");
    assert_eq!(q.parameters["air_temperature"].unit, "°C");

    // `parameter-name` is the shared rule (#966): case-insensitive, and an
    // unknown name or an empty list is a 400, never silently narrowed.
    let CoverageResponse::Single(q) = e
        .query_location(SMHI, None, Some(&["WIND_SPEED".to_string()]), None, None)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(q.ranges.keys().collect::<Vec<_>>(), vec!["wind_speed"]);
    for names in [vec!["wind_speed".to_string(), "nope".to_string()], vec![]] {
        assert!(matches!(
            e.query_location(SMHI, None, Some(&names), None, None),
            Err(DataServerError::InvalidParameter(m)) if m.contains("wind_speed")
        ));
        assert!(matches!(
            e.query_area("18,57,20,58", None, Some(&names), None, None),
            Err(DataServerError::InvalidParameter(_))
        ));
    }

    // Position: 5 km from Östergarnsholm hits it; the middle of the Baltic
    // is beyond 25 km → 404.
    let CoverageResponse::Single(q) = e
        .query_position("POINT(19.05 57.40)", None, None, None, None)
        .unwrap()
    else {
        panic!()
    };
    assert!((q.ranges["air_temperature"].values[0].unwrap() - 16.97).abs() < 1e-3);
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
    // Pressure: 010004 = 92650 Pa in the ZA report, served as 926.5 hPa.
    let CoverageResponse::Single(q) = e.query_location(ZA, None, None, None, None).unwrap() else {
        panic!()
    };
    assert!((q.ranges["pressure"].values[0].unwrap() - 926.5).abs() < 1e-3);
    assert_eq!(q.parameters["pressure"].unit, "hPa");
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

/// The `serves_station_series` contract EDR GeoJSON relies on (#929): every
/// coverage is a `PointSeries` without `z`, at the coordinates of exactly one
/// `get_locations` entry, and a position answer sits at its station.
#[test]
fn station_series_sit_at_listed_locations() {
    let e = engine();
    assert!(e.serves_station_series());
    let locations = e.get_locations().unwrap();
    let at_one_location = |domain: &DomainDescription| {
        let DomainDescription::PointSeries { x, y, z: None, .. } = domain else {
            panic!("expected a PointSeries without z: {domain:?}");
        };
        let found: Vec<_> = locations
            .iter()
            .filter(|l| l.longitude == *x && l.latitude == *y)
            .collect();
        assert_eq!(found.len(), 1, "({x}, {y})");
        found[0].id.clone()
    };
    let CoverageResponse::Collection(c) = e
        .query_area("-180,-90,180,90", None, None, None, None)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(c.len(), locations.len());
    for coverage in &c {
        at_one_location(&coverage.domain);
    }
    let CoverageResponse::Single(q) = e
        .query_position("POINT(18.98 57.44)", None, None, None, None)
        .unwrap()
    else {
        panic!("a position answer is one station's series");
    };
    assert_eq!(at_one_location(&q.domain), SMHI);
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
    // The override names the BUFR unit (K); the display rule still applies.
    assert!((q.ranges["t2m"].values[0].unwrap() - 16.97).abs() < 1e-3);
    assert_eq!(q.parameters["t2m"].label, "2 m temperature");
    assert_eq!(q.parameters["t2m"].unit, "°C");
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

/// A copy of `msg` whose first section-3 descriptor is replaced with the
/// unassigned `3-63-255` (edition 4, no optional section 2 — true for every
/// fixture), so tinybufr fails on that message and no other.
fn corrupt_descriptor(msg: &[u8]) -> Vec<u8> {
    let be24 = |o: usize| u32::from_be_bytes([0, msg[o], msg[o + 1], msg[o + 2]]) as usize;
    assert_eq!(msg[7], 4, "edition 4 fixture");
    assert_eq!(msg[8 + 9] & 0x80, 0, "no optional section 2");
    let s3 = 8 + be24(8);
    let mut out = msg.to_vec();
    out[s3 + 7] = 0xFF;
    out[s3 + 8] = 0xFF;
    out
}

#[test]
fn concatenated_file_keeps_the_good_messages_around_a_bad_one() {
    // A bulletin of three messages; the middle one cannot be decoded. The
    // reports before AND after it must survive (a per-message failure is
    // not fatal to the file), and the failure is reported once.
    let smhi = std::fs::read(fixtures().join("synop_se-smhi_20260912T0800Z.bufr")).unwrap();
    let arg = std::fs::read(fixtures().join("synop_ar-smn_20260912T0800Z.bufr")).unwrap();
    let mut file = smhi.clone();
    file.extend_from_slice(&corrupt_descriptor(&smhi));
    file.extend_from_slice(&arg);

    let d = Decoder::new();
    let out = d.decode(&file).unwrap();
    assert_eq!(out.messages, 3);
    assert_eq!(out.failed.len(), 1, "{:?}", out.failed);
    let ids: Vec<&str> = out.reports.iter().map(|r| r.station_id.as_str()).collect();
    assert_eq!(ids, vec![SMHI, ARG]);
    // The bad message alone is still a decodable stream with one failure,
    // never an `Err` (only a stream with no BUFR magic at all is).
    let alone = d.decode(&corrupt_descriptor(&smhi)).unwrap();
    assert_eq!(
        (alone.messages, alone.failed.len(), alone.reports.len()),
        (1, 1, 0)
    );

    // Through the engine: both good reports ingest, one failure counted.
    let e = engine();
    let before = e.health.decode_failures_total();
    assert_eq!(e.ingest_bytes(&file, "bulletin", t0800()), 2);
    let after = e.health.decode_failures_total();
    assert_eq!(after - before, 1);
}

fn decoder_fixture(name: &str) -> Vec<u8> {
    std::fs::read(fixtures().join("../bufr-decoder").join(name)).unwrap()
}

#[test]
fn eccodes_compressed_strings_wide_numbers_and_missing_increments() {
    let decoder = Decoder::new();
    for constant in [false, true] {
        let name = if constant {
            "constant-string-missing-increment.bufr"
        } else {
            "compressed-strings-wide.bufr"
        };
        let decoded = decoder.decode(&decoder_fixture(name)).unwrap();
        assert!(decoded.failed.is_empty(), "{:?}", decoded.failed);
        assert!(decoded.skipped.is_empty());
        assert_eq!(decoded.reports.len(), 3);
        for (i, report) in decoded.reports.iter().enumerate() {
            assert_eq!(report.station_id, format!("0-20000-0-01{}", 101 + i));
            let expected_name = if constant {
                "SAME STATION"
            } else {
                ["NORTH STATION", "SOUTH STATION", "EAST STATION"][i]
            };
            assert_eq!(report.name.as_deref(), Some(expected_name));
            assert_eq!(
                report.time,
                Utc.with_ymd_and_hms(2026, 9, 16, 10, 0, 0).unwrap()
            );
            assert!((report.lat - (60.0 + i as f64)).abs() < 1e-8);
            assert!((report.lon - (24.0 + i as f64)).abs() < 1e-8);
            assert_eq!(
                report.value(xy_from_code("010004").unwrap(), None),
                Some(101300.0 + 10.0 * i as f64)
            );
            let temperature = report.value(xy_from_code("012101").unwrap(), None);
            if constant && i == 1 {
                assert_eq!(temperature, None);
            } else {
                assert!((temperature.unwrap() - (280.15 + i as f64)).abs() < 1e-8);
            }
        }
    }
}

#[test]
fn eccodes_short_character_field_and_value_above_u32() {
    let decoded = Decoder::new()
        .decode(&decoder_fixture("short-string-wide-number.bufr"))
        .unwrap();
    assert!(decoded.failed.is_empty(), "{:?}", decoded.failed);
    assert_eq!(decoded.reports.len(), 1);
    let report = &decoded.reports[0];
    assert_eq!(report.station_id, "0-20000-0-01104");
    assert_eq!(report.name.as_deref(), Some("ABC"));
    assert!((report.lat - 63.0).abs() < 1e-8 && (report.lon - 27.0).abs() < 1e-8);
    assert_eq!(
        report.value(xy_from_code("010004").unwrap(), None),
        Some(42949672960.0)
    );
    assert!((report.value(xy_from_code("012101").unwrap(), None).unwrap() - 283.15).abs() < 1e-8);
}

#[test]
fn dwd_local_descriptors_are_scoped_to_centre_and_version() {
    let decoder = Decoder::new();
    let mut bytes = decoder_fixture("dwd-local-hour.bufr");
    // Edition 4 section 1: centre at bytes 12–13, local-table version at 22.
    assert_eq!(&bytes[12..14], &78u16.to_be_bytes());
    assert_eq!(bytes[22], 8);
    for version in 2..=8 {
        bytes[22] = version;
        let decoded = decoder.decode(&bytes).unwrap();
        assert!(decoded.failed.is_empty(), "{:?}", decoded.failed);
        assert_eq!(decoded.reports.len(), 1);
        assert_eq!(
            decoded.reports[0].value(xy_from_code("004214").unwrap(), None),
            Some(10.0)
        );
        assert!(
            (decoded.reports[0].lat - 63.0).abs() < 1e-8
                && (decoded.reports[0].lon - 27.0).abs() < 1e-8
        );
    }
    bytes[12..14].copy_from_slice(&98u16.to_be_bytes());
    let decoded = decoder.decode(&bytes).unwrap();
    assert_eq!(decoded.failed.len(), 1);
    assert!(decoded.reports.is_empty());
    bytes[12..14].copy_from_slice(&78u16.to_be_bytes());
    bytes[22] = 1;
    let decoded = decoder.decode(&bytes).unwrap();
    assert_eq!(decoded.failed.len(), 1);
    assert!(decoded.reports.is_empty());
}

// ---- #1008: live WIS2 SYNOP messages that used to fail ---------------------
//
// `testdata/bufr-regressions/`: real messages from the WIS2 Global Cache
// (2026-10-09/10). Expected values are ecCodes 2.47.0 `bufr_dump -p` of the
// same file; its README lists provenance and the cause of each failure.

fn regression(name: &str) -> Vec<u8> {
    std::fs::read(fixtures().join("../bufr-regressions").join(name)).unwrap()
}

/// Decode a regression fixture that must decode cleanly.
fn decode_clean(name: &str) -> engine_bufr::decode::Decoded {
    let decoded = Decoder::new().decode(&regression(name)).unwrap();
    assert!(decoded.failed.is_empty(), "{name}: {:?}", decoded.failed);
    decoded
}

fn value(report: &engine_bufr::ObsReport, code: &str) -> Option<f64> {
    report.value(xy_from_code(code).unwrap(), None)
}

fn close(a: Option<f64>, b: f64) -> bool {
    a.is_some_and(|a| (a - b).abs() < 1e-6)
}

#[test]
fn compressed_synop_with_delayed_replication() {
    // ca-eccc-msc 307091, three compressed subsets behind a GTS heading.
    let d = decode_clean("ca-eccc-msc_307091_compressed.bufr");
    let ids: Vec<&str> = d.reports.iter().map(|r| r.station_id.as_str()).collect();
    assert_eq!(
        ids,
        ["0-20000-0-71522", "0-20000-0-71615", "0-20000-0-71641"]
    );
    let names: Vec<_> = d.reports.iter().map(|r| r.name.as_deref()).collect();
    assert_eq!(
        names,
        [
            Some("CHUTE-DES-PASSES"),
            Some("KUUJJUARAPIK"),
            Some("SALLUIT")
        ]
    );
    assert!((d.reports[2].lat - 62.1797).abs() < 1e-4);
    assert!((d.reports[2].lon + 75.6701).abs() < 1e-4);
    assert_eq!(value(&d.reports[0], "012101"), None);
    assert!(close(value(&d.reports[1], "012101"), 276.59));
    assert!(close(value(&d.reports[2], "011002"), 1.5));

    // kz-kazhydromet 307080, one compressed subset whose name is all-ones up
    // to NUL padding (missing, as ecCodes reads it) and whose position is
    // missing: decoded, then skipped for want of a position.
    let d = decode_clean("kz-kazhydromet_307080_compressed_nul-padded-name.bufr");
    assert!(d.reports.is_empty());
    assert_eq!(d.skipped, [engine_bufr::decode::SkipReason::NoPosition]);
}

#[test]
fn older_master_table_version_uses_its_own_widths() {
    // jp-jma SYOWA, master table 13: the 302045 radiation elements were
    // narrower before version 14, so the current widths overran the data.
    let d = decode_clean("jp-jma_307080_master-v13.bufr");
    let r = &d.reports[0];
    assert_eq!(r.station_id, "0-20000-0-89532");
    assert_eq!(r.name.as_deref(), Some("SYOWA"));
    assert_eq!(r.time, Utc.with_ymd_and_hms(2026, 10, 9, 21, 0, 0).unwrap());
    assert!(close(value(r, "010051"), 97790.0));
    assert!(close(value(r, "012101"), 266.45));
    assert!(close(value(r, "011002"), 11.8));
}

#[test]
fn ra_iii_sequence_307083() {
    // bb-barbadosmetservices: the generated Table D lacked 307083's
    // leading 301090 302031.
    let d = decode_clean("bb-barbadosmetservices_307083.bufr");
    let r = &d.reports[0];
    assert_eq!(r.station_id, "0-52-130-78954");
    assert!((r.lat - 13.0733).abs() < 1e-4 && (r.lon + 59.5).abs() < 1e-4);
    assert_eq!(r.elevation, Some(56.6));
    assert!(close(value(r, "010004"), 100470.0));
    assert!(close(value(r, "010051"), 101240.0));
    assert!(close(value(r, "012101"), 301.6));
    assert!(close(value(r, "012103"), 297.6));
    assert!(close(value(r, "020001"), 30000.0));
    assert!(close(value(r, "011001"), 80.0));
}

#[test]
fn associated_fields_and_changed_reference_values() {
    // cy-dom 307092 (204018 … 204000), one subset extracted by ecCodes.
    let d = decode_clean("cy-dom_307092_associated-fields.bufr");
    let r = &d.reports[0];
    assert_eq!(r.station_id, "0-196-0-01727");
    assert_eq!(
        r.time,
        Utc.with_ymd_and_hms(2026, 10, 9, 20, 40, 0).unwrap()
    );
    assert!((r.lat - 34.9116).abs() < 1e-4 && (r.lon - 33.6301).abs() < 1e-4);
    assert!(close(value(r, "012101"), 295.95));
    assert_eq!(r.name.as_deref(), Some("FANEROMENI"));
    assert!(close(value(r, "013011"), 0.0));

    // il-ims: 203014 redefines the 007030/007031 reference to -5000, so
    // this station's raw height reads -200 m (the table reference gives
    // -100 m).
    let d = decode_clean("il-ims_203-changed-reference.bufr");
    let r = &d.reports[0];
    assert_eq!(r.station_id, "0-376-0-621");
    assert_eq!(r.name.as_deref(), Some("Zemah"));
    assert_eq!(r.elevation, Some(-200.0));
    assert!(close(value(r, "012101"), 298.42));
    assert!(close(value(r, "011002"), 1.2));
}

#[test]
fn non_ascii_station_name_does_not_fail_the_message() {
    let d = decode_clean("cl-meteochile_non-ascii-name.bufr");
    let r = &d.reports[0];
    assert_eq!(r.station_id, "0-152-0-320049");
    assert_eq!(r.name.as_deref(), Some("Chincolco Liceo agr\u{fffd}"));
    assert!(close(value(r, "012101"), 292.03));
}

#[test]
fn dwd_local_template_and_supplement_merge() {
    // A local-version-8 message using 004214/004215 and further DWD
    // elements (020193, 020199, 052210, …).
    let d = decode_clean("de-dwd_local-v8.bufr");
    let r = &d.reports[0];
    assert_eq!(r.station_id, "0-20000-0-10818");
    assert_eq!(r.name.as_deref(), Some("Klippeneck"));
    assert!(close(value(r, "012101"), 281.25));
    assert!(close(value(r, "020237"), 39502.0));
    assert!(close(value(r, "011002"), 4.9));

    // A bulletin's SYNOP message (local version 0) followed by its national
    // supplement (local version 8, 020193 first) for the same station and
    // time: one report carrying both. Before #1008 the supplement failed;
    // decoded alone it would replace the SYNOP row in the store.
    let d = decode_clean("de-dwd_synop-and-supplement.bufr");
    assert_eq!(d.messages, 2);
    assert_eq!(d.reports.len(), 1);
    let r = &d.reports[0];
    assert_eq!(r.station_id, "0-20000-0-10022");
    assert_eq!(
        r.time,
        Utc.with_ymd_and_hms(2026, 10, 10, 3, 30, 0).unwrap()
    );
    assert!(close(value(r, "012101"), 283.65)); // SYNOP
    assert!(close(value(r, "010051"), 100130.0)); // SYNOP
    assert!(close(value(r, "020237"), 56613.0)); // supplement (local)
    assert!(close(value(r, "012130"), 283.95)); // supplement soil temperature

    // Through the engine: one row with the SYNOP's parameters intact.
    let e = engine();
    let t = Utc.with_ymd_and_hms(2026, 10, 10, 3, 30, 0).unwrap();
    assert_eq!(
        e.ingest_bytes(&regression("de-dwd_synop-and-supplement.bufr"), "dwd", t),
        1
    );
    assert_eq!(e.health.decode_failures_total(), 0);
}

#[test]
fn failures_are_counted_by_kind() {
    use engine_bufr::decode::FailureKind;
    let e = engine();
    // A NIL bulletin (cy-dom) is not BUFR at all.
    assert_eq!(e.ingest_bytes(b"NIL", "nil", t0800()), 0);
    // A message cut short inside its data section.
    let jma = regression("jp-jma_307080_master-v13.bufr");
    let start = jma.windows(4).position(|w| w == b"BUFR").unwrap();
    let mut cut = jma[start..jma.len() - 40].to_vec();
    let len = (cut.len() as u32).to_be_bytes();
    cut[4..7].copy_from_slice(&len[1..]);
    assert_eq!(e.ingest_bytes(&cut, "cut", t0800()), 0);
    // An unregistered local element: DWD's 020193 under local version 1.
    let mut dwd = regression("de-dwd_local-v8.bufr");
    assert_eq!(dwd[22], 8);
    dwd[22] = 1;
    assert_eq!(e.ingest_bytes(&dwd, "dwd-v1", t0800()), 0);
    assert_eq!(e.health.decode_failures(FailureKind::NotBufr), 1);
    assert_eq!(e.health.decode_failures(FailureKind::Truncated), 1);
    assert_eq!(e.health.decode_failures(FailureKind::UnknownDescriptor), 1);
    assert_eq!(e.health.decode_failures_total(), 3);
    let counters = e.health.counters();
    assert_eq!(counters[7..], [1, 1, 1, 0, 0]);
}

// ---------------------------------------------------------------------------
// Station-symbol map layers (#1204). Positions as decoded (ecCodes agrees):
// SMHI 18.98391°E 57.44075°N, 16.97 °C, no MSL pressure; Hong Kong
// 113.92194°E 22.30944°N, 34.2 °C; Indonesia 122.85125°E 0.63907°N, 30.5 °C;
// Rwanda 29.73202°E 2.59444°S reports at 08:20Z, every other station at
// 08:00Z. Reference projections from PROJ (`cs2cs EPSG:4326 EPSG:3857` /
// `EPSG:3035`).
// ---------------------------------------------------------------------------

mod station_map {
    use super::*;
    use ds_core::config::StationMapConfig;
    use ds_core::map_engine::{MapEngine, OutputCrs, RasterTile};
    use ds_render::station_symbols::{StationSymbols, NODATA_MARKER};
    use std::collections::HashMap;

    fn map_engine(map: StationMapConfig) -> BufrEngine {
        let cfg = BufrConfig {
            map: Some(map),
            ..config()
        };
        BufrEngine::new(&cfg, "bufr-map-test").unwrap()
    }

    fn one_pixel() -> StationMapConfig {
        StationMapConfig {
            marker_size: Some(1),
            ..Default::default()
        }
    }

    fn render(
        e: &BufrEngine,
        bbox: [f64; 4],
        size: (u32, u32),
        time: DateTime<Utc>,
        crs: &OutputCrs,
        parameter: &str,
    ) -> RasterTile {
        e.get_raster_tile(
            bbox,
            size.0,
            size.1,
            Some(time),
            crs,
            Some(parameter),
            None,
            None,
        )
        .unwrap()
    }

    fn painted(tile: &RasterTile) -> HashMap<(u32, u32), f64> {
        (0..tile.height)
            .flat_map(|row| (0..tile.width).map(move |col| (col, row)))
            .filter_map(|(col, row)| {
                tile.values
                    .value_at((row * tile.width + col) as usize)
                    .map(|v| ((col, row), v))
            })
            .collect()
    }

    /// One layer per parameter in table order with its display unit, frames
    /// every 10 minutes from the first report (08:00Z) to the newest
    /// (08:20Z), the render's selection for `resolve_time` and
    /// `resolve_parameter_time`, a non-zero content version (the store
    /// revises in place) and no GetFeatureInfo.
    #[test]
    fn layers_advertise_each_parameter_on_ten_minute_frames() {
        let e = map_engine(StationMapConfig::default());
        let info = e.raster_info_shared();
        assert_eq!(info.parameter, "air_temperature");
        assert_eq!(info.unit, "°C");
        assert_eq!(
            info.parameters
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>(),
            e.get_parameters()
        );
        let msl = info
            .parameters
            .iter()
            .find(|p| p.name == "pressure_msl")
            .unwrap();
        assert_eq!(msl.unit, "hPa");
        let at = |hh: u32, mm: u32| Utc.with_ymd_and_hms(2026, 9, 12, hh, mm, 0).unwrap();
        assert_eq!(info.times, vec![at(8, 0), at(8, 10), at(8, 20)]);
        assert_eq!(e.resolve_time(None, None), Some(at(8, 20)));
        assert_eq!(
            e.resolve_parameter_time(Some("pressure_msl"), Some(at(8, 19)), None),
            Some(at(8, 10))
        );
        assert_ne!(e.content_version(), 0);
        assert!(!e.layer_shows_values("air_temperature"));
    }

    /// CRS:84, 1° a pixel over the world: each station at its pixel with
    /// its 08:00Z value. Rwanda's 08:20Z report is after the frame; at
    /// 08:20Z it is drawn too.
    #[test]
    fn markers_land_on_their_pixels_in_crs84() {
        let e = map_engine(one_pixel());
        let world = [-180.0, -90.0, 180.0, 90.0];
        let tile = render(
            &e,
            world,
            (360, 180),
            t0800(),
            &OutputCrs::Wgs84,
            "air_temperature",
        );
        let pixels = painted(&tile);
        assert_eq!(pixels.len(), 7, "{pixels:?}");
        assert_eq!(pixels[&(198, 32)], 16.97, "SMHI");
        assert_eq!(pixels[&(293, 67)], 34.2, "Hong Kong");
        assert_eq!(pixels[&(121, 124)], 11.4, "Morón");
        assert!(!pixels.contains_key(&(209, 92)), "Rwanda reports at 08:20Z");
        let later = t0800() + chrono::Duration::minutes(20);
        let tile = render(
            &e,
            world,
            (360, 180),
            later,
            &OutputCrs::Wgs84,
            "air_temperature",
        );
        assert_eq!(painted(&tile)[&(209, 92)], 24.1, "Rwanda");
    }

    /// EPSG:3857 over 10°–30°E, 50°–60°N in 200 × 100 pixels: SMHI is row 28
    /// in Web Mercator but row 25 in CRS:84. PROJ northings: 60°N
    /// 8 399 737.8898 m, 57.44075°N 7 850 742.8523 m, 50°N 6 446 275.8410 m.
    #[test]
    fn markers_land_on_their_pixels_in_epsg3857() {
        let (north, station, south): (f64, f64, f64) =
            (8_399_737.889_818, 7_850_742.852_275, 6_446_275.841_017);
        let row = ((north - station) / (north - south) * 100.0).floor() as u32;
        assert_eq!(row, 28, "the reference itself");
        let e = map_engine(one_pixel());
        let bbox = [10.0, 50.0, 30.0, 60.0];
        let mercator = render(
            &e,
            bbox,
            (200, 100),
            t0800(),
            &OutputCrs::WebMercator,
            "air_temperature",
        );
        assert_eq!(painted(&mercator), HashMap::from([((89, row), 16.97)]));
        let linear = render(
            &e,
            bbox,
            (200, 100),
            t0800(),
            &OutputCrs::Wgs84,
            "air_temperature",
        );
        assert_eq!(painted(&linear), HashMap::from([((89, 25), 16.97)]));
    }

    /// EPSG:3035: SMHI is (4 859 507.0774 E, 3 849 268.3648 N). 100 m pixels
    /// with the station at the centre of pixel (30, 70).
    #[test]
    fn markers_land_on_their_pixels_in_epsg3035() {
        let crs = ds_core::geo::projected_output_crs("EPSG:3035").unwrap();
        let (east, north) = (4_859_507.077_4, 3_849_268.364_8);
        let west = east - 3_050.0;
        let top = north + 7_050.0;
        let projected = [west, top - 10_000.0, west + 10_000.0, top];
        let output = OutputCrs::Projected {
            crs: crs.clone(),
            bbox: projected,
        };
        let envelope = ds_core::geo::wgs84_envelope(&crs, projected).unwrap();
        let e = map_engine(one_pixel());
        let tile = render(
            &e,
            envelope,
            (100, 100),
            t0800(),
            &output,
            "air_temperature",
        );
        assert_eq!(painted(&tile), HashMap::from([((30, 70), 16.97)]));
    }

    /// The overlap rule: at 10° a pixel, the 6-pixel markers of Hong Kong
    /// (34.2 °C) and Indonesia (30.5 °C) each cover the other's pixel; the
    /// one farther from the layer's midpoint is on top of both.
    #[test]
    fn the_most_extreme_value_is_on_top() {
        let e = map_engine(StationMapConfig::default());
        let frame = |midpoint: f64| {
            e.set_station_symbols(StationSymbols::new(HashMap::from([(
                "air_temperature".to_string(),
                midpoint,
            )])));
            let tile = render(
                &e,
                [-180.0, -90.0, 180.0, 90.0],
                (36, 18),
                t0800(),
                &OutputCrs::Wgs84,
                "air_temperature",
            );
            let pixels = painted(&tile);
            (pixels[&(29, 6)], pixels[&(30, 8)])
        };
        // |34.2 - 32| = 2.2 > |30.5 - 32| = 1.5: Hong Kong on top.
        assert_eq!(frame(32.0), (34.2, 34.2));
        // |34.2 - 33| = 1.2 < |30.5 - 33| = 2.5: Indonesia on top.
        assert_eq!(frame(33.0), (30.5, 30.5));
    }

    /// No marker without a value: SMHI reports no MSL pressure, so the
    /// pressure layer leaves it out, or draws a nodata marker with
    /// `draw_missing`.
    #[test]
    fn a_station_without_a_value_has_no_marker() {
        let world = [-180.0, -90.0, 180.0, 90.0];
        let e = map_engine(one_pixel());
        let tile = render(
            &e,
            world,
            (360, 180),
            t0800(),
            &OutputCrs::Wgs84,
            "pressure_msl",
        );
        let pixels = painted(&tile);
        assert!(!pixels.contains_key(&(198, 32)), "{pixels:?}");
        // The stored f32 widened as EDR serves it.
        assert_eq!(pixels[&(293, 67)], 1011.59998, "Hong Kong");
        let e = map_engine(StationMapConfig {
            draw_missing: Some(true),
            ..one_pixel()
        });
        let tile = render(
            &e,
            world,
            (360, 180),
            t0800(),
            &OutputCrs::Wgs84,
            "pressure_msl",
        );
        assert_eq!(painted(&tile)[&(198, 32)], NODATA_MARKER);
    }
}
