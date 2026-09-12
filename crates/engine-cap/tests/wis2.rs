//! WIS2 source mode, offline: real MeteoAlarm hub artefacts captured from the
//! Global Broker on 2026-09-12 (notification, the canonical CAP XML, the
//! `rel=geometry` zone polygon) are fed straight into the accumulator — no
//! broker, no HTTP — and the resulting catalog is checked through the public
//! engine API.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::config::{CapConfig, Wis2Config};
use ds_core::feature::{FeatureQuery, Geometry};
use ds_core::feature_engine::FeatureEngine;
use ds_core::health::LiveStatus;
use ds_wis2::{parse_notification, Notification, Payload, PayloadSource, Resolved};
use engine_cap::wis2::{hint_from_bytes, hint_key};
use engine_cap::CapEngine;

fn fixture(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/wis2-fixtures")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn notification(name: &str) -> Notification {
    let v: serde_json::Value = serde_json::from_slice(&fixture(name)).unwrap();
    let topic = v["topic"].as_str().unwrap();
    parse_notification(topic, &serde_json::to_vec(&v["message"]).unwrap()).unwrap()
}

fn config(language: Option<&str>, bbox_fallback: bool) -> CapConfig {
    CapConfig {
        data_path: None,
        feed_url: None,
        poll_interval_secs: 60,
        language: language.map(String::from),
        status_filter: vec!["Actual".to_string()],
        default_ttl: None,
        circle_segments: 64,
        geocode_geometry: None,
        geocode_property: "code".to_string(),
        geocode_value_name: None,
        feed_allowlist: Vec::new(),
        wis2: Some(Wis2Config {
            topics: vec![
                "cache/a/wis2/eu-eumetnet-warnings/data/core/weather/advisories-warnings".into(),
            ],
            ..Wis2Config::default()
        }),
        retention_grace: "PT1H".to_string(),
        max_alerts: 1000,
        geometry_links: true,
        bbox_fallback,
    }
}

fn resolved(n: Notification, bytes: Vec<u8>) -> Resolved {
    Resolved {
        notification: n,
        payload: Some(Payload {
            bytes: bytes.into(),
            source: PayloadSource::Downloaded("https://cache/x.xml".into()),
            media_type: Some("application/xml".into()),
            verified: None,
        }),
    }
}

const MK_IDENTIFIER: &str = "2.49.0.0.807.0.MK.120926101959.8054";

#[test]
fn constructor_does_no_network_and_starts_degraded() {
    let engine = CapEngine::new(&config(None, false), "cap-wis2").unwrap();
    assert!(engine.is_wis2());
    assert!(!engine.is_loaded());
    assert_eq!(
        engine.live_health(),
        Some(LiveStatus::Degraded {
            reason: "connecting to WIS2 broker"
        })
    );
    assert!(engine.wis2_status().is_none());
    assert_eq!(engine.feature_count(), 0);
}

#[test]
fn meteoalarm_alert_gets_exact_zone_polygon_from_the_geometry_hint() {
    let engine = CapEngine::new(&config(None, false), "cap-wis2").unwrap();
    let src = engine.wis2_source().unwrap();

    let n = notification("meteoalarm-mk-notification.json");
    // The hub keys the hint to (info 1 = mk-MKD, area 0) — 0-based, document order.
    let (info_idx, area_idx, href) = hint_key(&n).unwrap();
    assert_eq!((info_idx, area_idx), (1, 0));
    assert!(href.contains("/features/4f7e90d1-925f-4b3b-b288-afb475bbe056.geojson"));
    let hint = hint_from_bytes(&n, &fixture("meteoalarm-mk-area.geojson")).unwrap();
    assert_eq!(hint.source, "notification");
    let hb = hint.geometry.bbox().unwrap();
    // 82-vertex NUTS3 polygon inside the notification's bbox.
    assert!(
        hb[0] >= 20.5 && hb[2] <= 21.25 && hb[1] >= 41.49 && hb[3] <= 42.21,
        "{hb:?}"
    );

    let now: DateTime<Utc> = "2026-09-12T08:20:10Z".parse().unwrap();
    src.apply_with_hint(
        resolved(n, fixture("meteoalarm-mk-alert.xml")),
        Some((info_idx, area_idx, hint)),
        "cap-wis2",
        now,
    );
    assert!(src.take_dirty());
    engine.refresh().unwrap();
    assert!(engine.is_loaded());

    // Two infos (en-GB, mk-MKD) × one area each → two features; only the
    // hinted one has geometry, the other stays geocode-only (null).
    assert_eq!(engine.feature_count(), 2);
    let hinted = engine.get_feature(&format!("{MK_IDENTIFIER}.1.0")).unwrap();
    assert!(
        matches!(&*hinted.geometry, Geometry::Polygon { exterior, .. } if exterior.len() == 82)
    );
    assert_eq!(
        hinted
            .properties
            .get("geometry_source")
            .and_then(|v| v.as_str()),
        Some("notification")
    );
    assert_eq!(
        hinted.properties.get("language").and_then(|v| v.as_str()),
        Some("mk-MKD")
    );
    let plain = engine.get_feature(&format!("{MK_IDENTIFIER}.0.0")).unwrap();
    assert!(matches!(&*plain.geometry, Geometry::Null));
    assert!(plain.properties.get("geometry_source").is_none());
    // The spatial extent comes from the hinted polygon alone.
    let ext = engine.spatial_extent().unwrap();
    assert!(
        ext[0] > 20.0 && ext[2] < 21.5 && ext[1] > 41.0 && ext[3] < 42.5,
        "{ext:?}"
    );
}

#[test]
fn per_area_notifications_merge_hints_and_bbox_fallback_fills_the_rest() {
    // `bbox_fallback = true`: the en-GB area (no hint) gets the notification
    // bbox; the mk-MKD area keeps its exact polygon even though its hint
    // arrived on an EARLIER notification than the last one for the same XML.
    let engine = CapEngine::new(&config(None, true), "cap-wis2").unwrap();
    let src = engine.wis2_source().unwrap();
    let xml = fixture("meteoalarm-mk-alert.xml");
    let n1 = notification("meteoalarm-mk-notification.json");
    let hint = hint_from_bytes(&n1, &fixture("meteoalarm-mk-area.geojson")).unwrap();
    let t0: DateTime<Utc> = "2026-09-12T08:20:10Z".parse().unwrap();
    src.apply_with_hint(
        resolved(n1.clone(), xml.clone()),
        Some((1, 0, hint)),
        "t",
        t0,
    );

    // Second notification for info 0 — same XML, later pubtime, no usable hint.
    let mut n2 = n1.clone();
    n2.id = "second".into();
    n2.data_id = "eu-eumetnet-warnings/second".into();
    n2.pubtime = t0 + chrono::Duration::seconds(30);
    n2.extra
        .insert("indexInfo".into(), serde_json::Value::from(0u64));
    n2.links.retain(|l| l.rel != "geometry");
    assert!(hint_key(&n2).is_none());
    src.apply_with_hint(
        resolved(n2, xml),
        None,
        "t",
        t0 + chrono::Duration::seconds(30),
    );
    engine.refresh().unwrap();

    let a = engine.get_feature(&format!("{MK_IDENTIFIER}.1.0")).unwrap();
    assert_eq!(
        a.properties.get("geometry_source").and_then(|v| v.as_str()),
        Some("notification")
    );
    let b = engine.get_feature(&format!("{MK_IDENTIFIER}.0.0")).unwrap();
    assert_eq!(
        b.properties.get("geometry_source").and_then(|v| v.as_str()),
        Some("bbox")
    );
    assert!(matches!(&*b.geometry, Geometry::Polygon { exterior, .. } if exterior.len() == 5));
    // Both areas now have geometry → both hit a bbox query over Macedonia.
    let page = engine
        .get_features(&FeatureQuery {
            bbox: Some(ds_core::feature::Bbox::new(20.0, 41.0, 22.0, 43.0).unwrap()),
            ..FeatureQuery::default()
        })
        .unwrap();
    assert_eq!(page.number_matched, 2);
}

#[test]
fn hint_outside_the_notification_bbox_is_rejected() {
    let mut n = notification("meteoalarm-mk-notification.json");
    // Move the notification bbox to Finland; the Macedonian polygon must fail.
    n.geometry = Some(ds_wis2::Geometry::Polygon(vec![
        [24.0, 60.0],
        [24.0, 61.0],
        [25.0, 61.0],
        [25.0, 60.0],
        [24.0, 60.0],
    ]));
    assert_eq!(
        hint_from_bytes(&n, &fixture("meteoalarm-mk-area.geojson")).unwrap_err(),
        "polygon lies outside the notification bbox"
    );
    assert_eq!(
        hint_from_bytes(&n, b"{\"type\":\"Feature\",\"geometry\":null}").unwrap_err(),
        "not a GeoJSON polygon"
    );
}

#[test]
fn language_filter_keeps_original_info_index_for_hint_keys() {
    // With `language = "mk"` only info 1 is emitted, and its feature id still
    // carries the original index the hub keyed the hint to.
    let engine = CapEngine::new(&config(Some("mk"), false), "cap-wis2").unwrap();
    let src = engine.wis2_source().unwrap();
    let n = notification("meteoalarm-mk-notification.json");
    let hint = hint_from_bytes(&n, &fixture("meteoalarm-mk-area.geojson")).unwrap();
    src.apply_with_hint(
        resolved(n, fixture("meteoalarm-mk-alert.xml")),
        Some((1, 0, hint)),
        "t",
        "2026-09-12T08:20:10Z".parse().unwrap(),
    );
    engine.refresh().unwrap();
    assert_eq!(engine.feature_count(), 1);
    let f = engine.get_feature(&format!("{MK_IDENTIFIER}.1.0")).unwrap();
    assert!(matches!(&*f.geometry, Geometry::Polygon { .. }));
    assert!(engine.get_feature(&format!("{MK_IDENTIFIER}.0.0")).is_err());
}

#[test]
fn expiry_and_deletion_flow_through_to_the_catalog() {
    let engine = CapEngine::new(&config(None, false), "cap-wis2").unwrap();
    let src = engine.wis2_source().unwrap();
    let n = notification("meteoalarm-mk-notification.json");
    let data_id = n.data_id.clone();
    src.apply_with_hint(
        resolved(n.clone(), fixture("meteoalarm-mk-alert.xml")),
        None,
        "t",
        "2026-09-12T08:20:10Z".parse().unwrap(),
    );
    engine.refresh().unwrap();
    assert_eq!(engine.feature_count(), 2);

    // A deletion notification for the same data_id withdraws the alert.
    let mut del = n.clone();
    del.id = "del".into();
    del.links = vec![ds_wis2::Link {
        rel: "deletion".into(),
        href: "https://cache/x.xml".into(),
        media_type: None,
        length: None,
    }];
    assert_eq!(del.data_id, data_id);
    src.apply_with_hint(
        Resolved {
            notification: del,
            payload: None,
        },
        None,
        "t",
        "2026-09-12T08:21:00Z".parse().unwrap(),
    );
    assert!(src.take_dirty());
    engine.refresh().unwrap();
    assert_eq!(engine.feature_count(), 0);
    assert_eq!(engine.wis2_source_stats().unwrap()[2], 1, "deletions");

    // Re-ingest, then confirm the snapshot at 2026-09-12T18:16Z+grace drops it
    // (the alert expires 18:16+02:00 = 16:16Z; grace 1 h ⇒ gone after 17:16Z).
    let mut again = n;
    again.id = "again".into();
    again.data_id = "eu-eumetnet-warnings/again".into();
    again.pubtime = "2026-09-12T09:00:00Z".parse().unwrap();
    src.apply_with_hint(
        resolved(again, fixture("meteoalarm-mk-alert.xml")),
        None,
        "t",
        "2026-09-12T09:00:01Z".parse().unwrap(),
    );
    let live = src.snapshot("2026-09-12T17:00:00Z".parse().unwrap());
    assert_eq!(live.len(), 1);
    let gone = src.snapshot("2026-09-12T17:30:00Z".parse().unwrap());
    assert!(gone.is_empty());
    let _ = Arc::clone(src);
}
