//! End-to-end tests for `engine-cap`, exercising both the `FeatureEngine` and
//! `MapEngine` trait surfaces against committed CAP v1.2 fixtures.

use std::path::PathBuf;

use chrono::{DateTime, TimeZone, Utc};

use ds_core::config::CapConfig;
use ds_core::feature::{Bbox, DatetimeInterval, FeatureQuery, Geometry};
use ds_core::feature_engine::FeatureEngine;
use ds_core::map_engine::{MapEngine, OutputCrs};
use engine_cap::CapEngine;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn config_for(dir: &str, language: Option<&str>) -> CapConfig {
    CapConfig {
        data_path: Some(dir.to_string()),
        feed_url: None,
        poll_interval_secs: 300,
        language: language.map(String::from),
        status_filter: vec!["Actual".to_string()],
        default_ttl: None,
        circle_segments: 64,
        geocode_geometry: None,
        geocode_property: "code".to_string(),
        geocode_value_name: None,
        feed_allowlist: Vec::new(),
    }
}

fn engine(language: Option<&str>) -> CapEngine {
    let dir = fixtures_dir();
    CapEngine::new(&config_for(dir.to_str().unwrap(), language), "cap-test").unwrap()
}

fn at(y: i32, mo: u32, d: u32, h: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, 0, 0).unwrap()
}

fn ids(page: &ds_core::feature::FeaturePage) -> Vec<String> {
    page.features.iter().map(|f| f.id.clone()).collect()
}

// ---------------------------------------------------------------------------
// Features
// ---------------------------------------------------------------------------

#[test]
fn loads_all_actual_alerts_and_drops_test_status() {
    let eng = engine(None);
    // flood (1) + heat en+fi (2) + storm (1) + geocode-only (1) = 5 records.
    // The <status>Test</status> fixture is dropped by the default filter.
    assert_eq!(eng.feature_count(), 5);
    let page = eng.get_features(&FeatureQuery::default()).unwrap();
    let id_list = ids(&page);
    assert!(id_list.iter().any(|i| i.contains("helsinki-flood")));
    assert!(!id_list.iter().any(|i| i.contains("exercise")));
}

#[test]
fn feature_geometry_is_lat_lon_swapped_to_finland() {
    let eng = engine(None);
    let f = eng.get_feature("urn:test:helsinki-flood-1.0.0").unwrap();
    // CAP polygon "60.10,24.90 …" must land in Finland: lon ≈ 24.9, lat ≈ 60.1.
    // A missing lat/lon swap would put the first vertex at lon=60.1 (off-globe
    // for a Finnish alert), so this pins the swap.
    match &*f.geometry {
        Geometry::Polygon { exterior, .. } => {
            assert!(
                (exterior[0][0] - 24.90).abs() < 1e-6,
                "lon {}",
                exterior[0][0]
            );
            assert!(
                (exterior[0][1] - 60.10).abs() < 1e-6,
                "lat {}",
                exterior[0][1]
            );
        }
        other => panic!("expected Polygon, got {other:?}"),
    }
    let b = f.geometry.bbox().unwrap();
    assert!((24.0..26.0).contains(&b[0]) && (59.0..62.0).contains(&b[1]));
}

#[test]
fn feature_properties_carry_cap_metadata() {
    let eng = engine(None);
    let f = eng.get_feature("urn:test:helsinki-flood-1.0.0").unwrap();
    use ds_core::feature::PropertyValue::*;
    assert_eq!(
        f.properties.get("event"),
        Some(&String("Flood Warning".into()))
    );
    assert_eq!(f.properties.get("severity"), Some(&String("Severe".into())));
    assert_eq!(f.properties.get("status"), Some(&String("Actual".into())));
    assert_eq!(
        f.properties.get("areaDesc"),
        Some(&String("Greater Helsinki".into()))
    );
    // category is a flat List.
    assert!(matches!(f.properties.get("category"), Some(List(_))));
    // Unknown feature → 404 mapping.
    assert!(eng.get_feature("does.not.exist").is_err());
}

#[test]
fn circle_feature_carries_radius_property() {
    let eng = engine(Some("en"));
    let f = eng.get_feature("urn:test:helsinki-heat-1.0.0").unwrap();
    assert!(matches!(&*f.geometry, Geometry::Polygon { .. }));
    assert_eq!(
        f.properties.get("radius_km"),
        Some(&ds_core::feature::PropertyValue::Float(8.0))
    );
}

#[test]
fn language_filter_keeps_one_info_per_alert() {
    let all = engine(None);
    let en = engine(Some("en"));
    // Without a language filter both heat infos (en + fi) appear; with "en"
    // only the English one survives, so the total drops by one.
    assert_eq!(all.feature_count(), 5);
    assert_eq!(en.feature_count(), 4);
    assert!(en.get_feature("urn:test:helsinki-heat-1.1.0").is_err()); // fi info gone
}

#[test]
fn bbox_filter_selects_by_location() {
    let eng = engine(Some("en"));
    // Over Helsinki: flood + heat (geocode-only has no geometry → excluded).
    let helsinki = FeatureQuery {
        bbox: Some(Bbox::new(24.8, 60.0, 25.2, 60.4).unwrap()),
        ..Default::default()
    };
    let got = ids(&eng.get_features(&helsinki).unwrap());
    assert!(got.iter().any(|i| i.contains("helsinki-flood")));
    assert!(got.iter().any(|i| i.contains("helsinki-heat")));
    assert!(!got.iter().any(|i| i.contains("storm")));

    // Over the Lahti storm region: only the storm.
    let lahti = FeatureQuery {
        bbox: Some(Bbox::new(25.4, 60.9, 26.1, 61.6).unwrap()),
        ..Default::default()
    };
    let got = ids(&eng.get_features(&lahti).unwrap());
    assert_eq!(got, vec!["urn:test:storm-window-1.0.0".to_string()]);
}

#[test]
fn datetime_filter_selects_active_window() {
    let eng = engine(Some("en"));
    // During the storm window (10:00–16:00Z) the storm is active.
    let during = FeatureQuery {
        datetime: Some(instant(at(2026, 6, 15, 12))),
        ..Default::default()
    };
    let got = ids(&eng.get_features(&during).unwrap());
    assert!(got.iter().any(|i| i.contains("storm")));

    // The next day the storm has expired; the open-ended alerts remain.
    let after = FeatureQuery {
        datetime: Some(instant(at(2026, 6, 16, 0))),
        ..Default::default()
    };
    let got = ids(&eng.get_features(&after).unwrap());
    assert!(!got.iter().any(|i| i.contains("storm")));
    assert!(got.iter().any(|i| i.contains("helsinki-flood")));
}

#[test]
fn geocode_only_area_is_a_null_geometry_feature() {
    let eng = engine(None);
    let f = eng.get_feature("urn:test:geocode-only-1.0.0").unwrap();
    assert!(matches!(&*f.geometry, Geometry::Null));
    // It is still listed (no bbox query), but never matches a bbox.
    let none = eng
        .get_features(&FeatureQuery {
            bbox: Some(Bbox::new(-10.0, -10.0, 10.0, 10.0).unwrap()),
            ..Default::default()
        })
        .unwrap();
    assert!(!ids(&none).iter().any(|i| i.contains("geocode-only")));
}

#[test]
fn pagination_is_stable() {
    let eng = engine(None);
    let p1 = eng
        .get_features(&FeatureQuery {
            limit: 2,
            offset: 0,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(p1.number_matched, 5);
    assert_eq!(p1.number_returned, 2);
    assert_eq!(p1.next_offset, Some(2));
    // Same query is deterministic (records sorted by id).
    let again = eng
        .get_features(&FeatureQuery {
            limit: 2,
            offset: 0,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(ids(&p1), ids(&again));
}

#[test]
fn data_version_is_stable_across_reloads_of_same_data() {
    let eng = engine(None);
    let v1 = eng.data_version();
    eng.refresh().unwrap(); // re-read identical files
    assert_eq!(eng.data_version(), v1, "unchanged data → unchanged version");
    assert_ne!(v1, 0);
}

// ---------------------------------------------------------------------------
// MapEngine
// ---------------------------------------------------------------------------

const HELSINKI_BBOX: [f64; 4] = [24.8, 60.0, 25.2, 60.4];

#[test]
fn renders_severity_with_max_overlap_wgs84() {
    let eng = engine(Some("en"));
    let tile = eng
        .get_raster_tile(
            HELSINKI_BBOX,
            100,
            100,
            None,
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
        .unwrap();
    let px = |x: usize, y: usize| tile.values.value_at(y * 100 + x);
    // Centre (25.0, 60.2) is inside both the flood polygon (Severe=3) and the
    // 8 km heat circle (Extreme=4) → Combine::Max resolves to 4.
    assert_eq!(px(50, 50), Some(4.0));
    // (24.92, 60.12) is inside the flood polygon but outside the heat circle → 3.
    assert_eq!(px(30, 70), Some(3.0));
    // The NW corner (≈24.8, 60.4) is outside every alert → nodata.
    assert_eq!(px(0, 0), None);
    assert!(!tile.is_empty());
}

#[test]
fn renders_in_web_mercator() {
    // The goal explicitly requires Web Mercator support: the flood/heat alerts
    // must still rasterize under EPSG:3857 pixel spacing.
    let eng = engine(Some("en"));
    let tile = eng
        .get_raster_tile(
            HELSINKI_BBOX,
            100,
            100,
            None,
            &OutputCrs::WebMercator,
            None,
            None,
            None,
        )
        .unwrap();
    let filled = tile.values.iter_values().filter(|v| v.is_some()).count();
    assert!(filled > 100, "expected a substantial fill, got {filled}");
    // Highest severity present is Extreme (4) where the alerts overlap.
    let max = tile.values.iter_values().flatten().fold(f64::MIN, f64::max);
    assert_eq!(max, 4.0);
}

#[test]
fn time_selection_renders_only_active_alerts() {
    let eng = engine(Some("en"));
    let storm_bbox = [25.4, 60.9, 26.1, 61.6];
    // During the storm window → the Lahti polygon fills (Severe=3).
    let during = eng
        .get_raster_tile(
            storm_bbox,
            64,
            64,
            Some(at(2026, 6, 15, 12)),
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
        .unwrap();
    assert!(!during.is_empty());
    assert_eq!(
        during.values.iter_values().flatten().fold(0.0, f64::max),
        3.0
    );

    // After it expires → nothing active in that bbox.
    let after = eng
        .get_raster_tile(
            storm_bbox,
            64,
            64,
            Some(at(2026, 6, 16, 0)),
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
        .unwrap();
    assert!(after.is_empty());
}

#[test]
fn degenerate_request_bbox_renders_empty_tile() {
    // A zero-area / non-finite request bbox asks for no region → empty tile,
    // never a full-catalog render.
    let eng = engine(Some("en"));
    let degenerate = [f64::NAN, 60.0, 25.0, 61.0];
    let tile = eng
        .get_raster_tile(
            degenerate,
            32,
            32,
            None,
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
        .unwrap();
    assert!(tile.is_empty());
    assert_eq!(tile.values.len(), 32 * 32);
}

#[test]
fn raster_info_advertises_layer_time_and_extent() {
    let eng = engine(None);
    let info = eng.raster_info();
    assert_eq!(info.native_crs, "CRS:84");
    assert_eq!(info.parameter, "severity");
    // Spatial extent covers Finland.
    let ext = info.spatial_extent.unwrap();
    assert!((24.0..26.5).contains(&ext[0]) && (59.0..62.0).contains(&ext[1]));
    // A TIME dimension is advertised (non-empty, ascending, ending at "now").
    assert!(!info.times.is_empty());
    assert!(info.times.windows(2).all(|w| w[0] <= w[1]));
}

// ---------------------------------------------------------------------------
// Poll / refresh
// ---------------------------------------------------------------------------

#[test]
fn refresh_picks_up_added_and_removed_files() {
    let dir = tempfile::tempdir().unwrap();
    let src = fixtures_dir();
    let copy = |name: &str| {
        std::fs::copy(src.join(name), dir.path().join(name)).unwrap();
    };
    let rm = |name: &str| {
        std::fs::remove_file(dir.path().join(name)).unwrap();
    };

    copy("helsinki-flood.xml");
    let eng = CapEngine::new(
        &config_for(dir.path().to_str().unwrap(), Some("en")),
        "cap-poll",
    )
    .unwrap();
    assert_eq!(eng.feature_count(), 1);

    // Add the storm file → next refresh sees it.
    copy("storm-window.xml");
    eng.refresh().unwrap();
    assert_eq!(eng.feature_count(), 2);

    // Remove the flood file → next refresh drops it.
    rm("helsinki-flood.xml");
    eng.refresh().unwrap();
    let got = ids(&eng.get_features(&FeatureQuery::default()).unwrap());
    assert_eq!(got, vec!["urn:test:storm-window-1.0.0".to_string()]);
}

/// A minimal one-area CAP document with the given identifier (XML-escaped, since
/// a CAP `<identifier>` may legitimately contain `<`/`>`/`&`).
fn cap_xml(identifier: &str) -> String {
    let identifier = identifier
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        r#"<alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
          <identifier>{identifier}</identifier><status>Actual</status>
          <sent>2020-01-01T00:00:00Z</sent>
          <info><event>Flood</event><severity>Severe</severity>
            <onset>2020-01-01T00:00:00Z</onset>
            <area><areaDesc>A</areaDesc><polygon>60,24 60,25 61,25 61,24 60,24</polygon></area>
          </info></alert>"#
    )
}

#[test]
fn identifier_with_slash_is_url_safe_and_reachable() {
    // A URI-style identifier containing '/' must not break the Features
    // self-link / route: the emitted Feature id is percent-encoded (no raw '/'),
    // while the catalog key stays the decoded id that the route resolves to.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.xml"),
        cap_xml("urn:oid:2.49.0.1.840/abc"),
    )
    .unwrap();
    let eng = CapEngine::new(&config_for(dir.path().to_str().unwrap(), None), "cap-slash").unwrap();

    let page = eng.get_features(&FeatureQuery::default()).unwrap();
    let fid = &page.features[0].id;
    assert!(
        !fid.contains('/'),
        "emitted id must not contain a raw '/': {fid}"
    );
    assert!(
        fid.contains("%2F"),
        "the '/' must be percent-encoded: {fid}"
    );
    // The route decodes %2F back to '/', so get_feature is keyed by the decoded
    // id (what the api-features Path extractor yields).
    assert!(eng.get_feature("urn:oid:2.49.0.1.840/abc.0.0").is_ok());
}

#[test]
fn identifier_with_brackets_is_url_safe_and_reachable() {
    // Real US-NWS-style identifiers can contain '[' / ']', which RFC 3986
    // forbids unencoded in a path segment.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.xml"), cap_xml("NWS-IDP[KOUN][2026]")).unwrap();
    let eng = CapEngine::new(&config_for(dir.path().to_str().unwrap(), None), "cap-brk").unwrap();
    let fid = eng.get_features(&FeatureQuery::default()).unwrap().features[0]
        .id
        .clone();
    assert!(
        !fid.contains('[') && !fid.contains(']'),
        "brackets must be encoded: {fid}"
    );
    assert!(fid.contains("%5B") && fid.contains("%5D"));
    // The decoded form (what the route yields) resolves.
    assert!(eng.get_feature("NWS-IDP[KOUN][2026].0.0").is_ok());
}

#[test]
fn duplicate_identifiers_are_deduped_and_reachable() {
    // Two files (or a feed serving the same alert twice) with the same
    // identifier must not split-brain: one record, reachable via get_feature.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.xml"), cap_xml("urn:dup:1")).unwrap();
    std::fs::write(dir.path().join("b.xml"), cap_xml("urn:dup:1")).unwrap();
    let eng = CapEngine::new(&config_for(dir.path().to_str().unwrap(), None), "cap-dup").unwrap();
    assert_eq!(
        eng.feature_count(),
        1,
        "duplicate ids collapse to one record"
    );
    assert!(eng.get_feature("urn:dup:1.0.0").is_ok());
}

#[test]
fn geocode_only_areas_resolve_via_lookup() {
    // MeteoAlarm-style geocode-only area (EMMA_ID, no inline polygon) gains
    // geometry from the geocode_geometry lookup, so it positions on the map and
    // contributes a spatial + temporal extent.
    let dir = tempfile::tempdir().unwrap();
    let xml = r#"<alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
      <identifier>urn:test:emma-1</identifier><status>Actual</status>
      <sent>2020-01-01T00:00:00Z</sent>
      <info><event>Strong wind advisory</event><severity>Moderate</severity>
        <onset>2020-01-01T00:00:00Z</onset><expires>2020-01-02T00:00:00Z</expires>
        <area><areaDesc>Selkameri, Merenkurkku</areaDesc>
          <geocode><valueName>EMMA_ID</valueName><value>FI801</value></geocode>
          <geocode><valueName>EMMA_ID</valueName><value>FI802</value></geocode>
        </area></info></alert>"#;
    std::fs::write(dir.path().join("a.xml"), xml).unwrap();

    let zones = fixtures_dir().join("zones.geojson");
    let mut cfg = config_for(dir.path().to_str().unwrap(), None);
    cfg.geocode_geometry = Some(zones.to_str().unwrap().to_string());
    cfg.geocode_value_name = Some("EMMA_ID".to_string());
    let eng = CapEngine::new(&cfg, "cap-geocode").unwrap();

    // The two EMMA zones merge into one MultiPolygon feature.
    let f = eng.get_feature("urn:test:emma-1.0.0").unwrap();
    assert!(matches!(&*f.geometry, Geometry::MultiPolygon { polygons } if polygons.len() == 2));
    // Resolved geometry yields a spatial extent (zones span ~20..25E, 60..63N).
    let ext = eng.spatial_extent().unwrap();
    assert!((19.0..26.0).contains(&ext[0]) && (59.0..64.0).contains(&ext[3]));

    // Without the lookup the same alert is null-geometry (no extent).
    let bare = CapEngine::new(&config_for(dir.path().to_str().unwrap(), None), "cap-bare").unwrap();
    assert!(matches!(
        &*bare.get_feature("urn:test:emma-1.0.0").unwrap().geometry,
        Geometry::Null
    ));
    assert!(bare.spatial_extent().is_none());
}

#[test]
fn temporal_extent_spans_alert_windows() {
    let eng = engine(Some("en"));
    let (start, end) = eng
        .temporal_extent()
        .expect("alerts present → temporal extent");
    assert!(start <= end);
    // The storm window (10:00–16:00Z on 2026-06-15) sits inside the span.
    assert!(start <= at(2026, 6, 15, 10) && end >= at(2026, 6, 15, 16));
}

// Multi-thread runtime: CapEngine::new()'s initial DataStore read uses
// `block_in_place`, which is only valid on a multi-thread-runtime worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_before_poll_loop_is_not_lost() {
    // A shutdown() that fires before poll_loop subscribes (rapid reload) must
    // still stop the loop — else it runs forever. poll_loop should return
    // promptly here rather than time out.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.xml"), cap_xml("urn:test:shut-1")).unwrap();
    let eng = std::sync::Arc::new(
        CapEngine::new(&config_for(dir.path().to_str().unwrap(), None), "cap-shut").unwrap(),
    );
    eng.shutdown(); // signal BEFORE poll_loop runs its subscribe
    let poller = eng.clone();
    tokio::time::timeout(std::time::Duration::from_secs(5), async move {
        poller.poll_loop().await
    })
    .await
    .expect("poll_loop must exit after a pre-subscribe shutdown");
}

#[test]
fn empty_source_is_loaded_but_has_no_records() {
    // A reachable source with zero CAP files is a healthy empty collection:
    // is_loaded() is true (so admin marks it Ready, not Degraded), even though
    // feature_count is 0.
    let dir = tempfile::tempdir().unwrap();
    let eng = CapEngine::new(&config_for(dir.path().to_str().unwrap(), None), "cap-empty").unwrap();
    assert!(
        eng.is_loaded(),
        "a successful zero-record load is still 'loaded'"
    );
    assert_eq!(eng.feature_count(), 0);
}

fn instant(t: DateTime<Utc>) -> DatetimeInterval {
    DatetimeInterval {
        start: Some(t),
        end: Some(t),
    }
}
