use chrono::{DateTime, Duration, Utc};
use ds_core::config::CapConfig;
use ds_core::feature::{Bbox, FeatureQuery};
use ds_core::feature_engine::FeatureEngine;
use ds_wis2::{parse_notification, Payload, PayloadSource, Resolved};
use engine_cap::CapEngine;
const XML: &str = include_str!("fixtures/helsinki-flood.xml");
fn now() -> DateTime<Utc> {
    "2026-09-13T12:00:00Z".parse().unwrap()
}
fn local() -> (tempfile::TempDir, CapEngine) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.xml"), XML).unwrap();
    let e = CapEngine::new(&config_for(dir.path().to_str().unwrap(), None), "review").unwrap();
    (dir, e)
}
fn cancellation() -> String {
    XML.replace("urn:test:helsinki-flood-1", "cancel-1")
        .replace("<msgType>Alert</msgType>", "<msgType>Cancel</msgType><references>test@meteocore.example,urn:test:helsinki-flood-1,2020-01-01T00:00:00+00:00</references>")
}
#[test]
fn test_cancel_must_not_withdraw_actual() {
    let (dir, e) = local();
    std::fs::write(
        dir.path().join("cancel.xml"),
        cancellation().replace("<status>Actual</status>", "<status>Test</status>"),
    )
    .unwrap();
    e.refresh().unwrap();
    assert_eq!(
        e.feature_count(),
        1,
        "Test cancellation removed the Actual warning"
    );
}
#[test]
fn same_identifier_different_senders_must_survive() {
    let (dir, e) = local();
    std::fs::write(
        dir.path().join("b.xml"),
        XML.replace("test@meteocore.example", "another@provider.example"),
    )
    .unwrap();
    e.refresh().unwrap();
    assert_eq!(e.feature_count(), 2, "distinct senders collapsed");
    assert!(
        e.get_feature("urn:test:helsinki-flood-1.0.0").is_err(),
        "ambiguous legacy id must not select a sender"
    );
    let page = e.get_features(&FeatureQuery::default()).unwrap();
    for f in &page.features {
        assert_eq!(e.get_feature(&f.id).unwrap().id, f.id);
    }
    assert_ne!(page.features[0].id, page.features[1].id);
}
#[test]
fn failed_document_fetch_must_preserve_catalog() {
    let (dir, e) = local();
    // Existing listed object becomes too large: get_many rejects it before download.
    std::fs::write(dir.path().join("a.xml"), vec![b' '; 8 * 1024 * 1024 + 1]).unwrap();
    assert!(e.refresh().is_err());
    assert_eq!(
        e.feature_count(),
        1,
        "failed fetch must preserve the warning"
    );
    assert!(matches!(
        e.live_health(),
        Some(ds_core::health::LiveStatus::Degraded { .. })
    ));
}
#[test]
fn antimeridian_query_must_exclude_helsinki() {
    let (_dir, e) = local();
    let q = FeatureQuery {
        bbox: Some(Bbox::new(170., -90., -170., 90.).unwrap()),
        ..FeatureQuery::default()
    };
    assert_eq!(
        e.get_features(&q).unwrap().number_matched,
        0,
        "Pacific bbox returned Helsinki"
    );
}
fn push(e: &CapEngine, xml: String, data_id: &str, pubtime: DateTime<Utc>) {
    let raw: serde_json::Value = serde_json::from_str(include_str!(
        "wis2-fixtures/meteoalarm-mk-notification.json"
    ))
    .unwrap();
    let mut n = parse_notification(
        raw["topic"].as_str().unwrap(),
        &serde_json::to_vec(&raw["message"]).unwrap(),
    )
    .unwrap();
    n.data_id = data_id.into();
    n.pubtime = pubtime;
    e.wis2_source().unwrap().apply_with_hint(
        Resolved {
            notification: n,
            payload: Some(Payload {
                bytes: xml.into_bytes().into(),
                source: PayloadSource::Downloaded("https://cache/a.xml".into()),
                media_type: None,
                verified: None,
            }),
        },
        None,
        "review",
        now(),
    );
}
#[test]
fn late_cancel_must_not_remove_newer_reissue() {
    let mut c = config_for("unused", None);
    c.data_path = None;
    c.wis2 = Some(ds_core::config::Wis2Config::default());
    let e = CapEngine::new(&c, "review").unwrap();
    push(&e, XML.into(), "newer", now());
    push(
        &e,
        cancellation(),
        "old-cancel",
        now() - Duration::minutes(10),
    );
    assert_eq!(
        e.wis2_source().unwrap().len(),
        1,
        "stale cancellation removed newer content"
    );
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
        wis2: None,
        retention_grace: "PT1H".to_string(),
        max_alerts: 10_000,
        geometry_links: true,
        bbox_fallback: false,
    }
}

#[test]
fn deletion_before_download_completion_must_block_resurrection() {
    let mut c = config_for("unused", None);
    c.data_path = None;
    c.wis2 = Some(ds_core::config::Wis2Config::default());
    let e = CapEngine::new(&c, "review").unwrap();
    let raw: serde_json::Value = serde_json::from_str(include_str!(
        "wis2-fixtures/meteoalarm-mk-notification.json"
    ))
    .unwrap();
    let mut n = parse_notification(
        raw["topic"].as_str().unwrap(),
        &serde_json::to_vec(&raw["message"]).unwrap(),
    )
    .unwrap();
    n.data_id = "inflight-document".into();
    n.pubtime = now();
    e.wis2_source().unwrap().apply_with_hint(
        Resolved {
            notification: n,
            payload: None,
        },
        None,
        "review",
        now(),
    );
    push(
        &e,
        XML.into(),
        "inflight-document",
        now() - Duration::minutes(1),
    );
    e.refresh_with(now).unwrap();
    assert_eq!(
        e.feature_count(),
        0,
        "out-of-order download resurrected deleted warning"
    );
}

#[test]
fn partial_refresh_accepts_new_alerts_and_recovers_failed_documents() {
    let (dir, e) = local();
    std::fs::write(dir.path().join("a.xml"), b"<alert>broken").unwrap();
    std::fs::write(
        dir.path().join("b.xml"),
        XML.replace("urn:test:helsinki-flood-1", "new-warning"),
    )
    .unwrap();
    assert!(e.refresh().is_err());
    assert_eq!(e.feature_count(), 2);
    std::fs::write(
        dir.path().join("a.xml"),
        XML.replace(
            "<severity>Severe</severity>",
            "<severity>Extreme</severity>",
        ),
    )
    .unwrap();
    e.refresh().unwrap();
    assert_eq!(
        e.get_feature("urn:test:helsinki-flood-1.0.0")
            .unwrap()
            .properties["severity"]
            .as_str(),
        Some("Extreme")
    );
    assert!(matches!(
        e.live_health(),
        Some(ds_core::health::LiveStatus::Ready)
    ));
    std::fs::remove_file(dir.path().join("a.xml")).unwrap();
    std::fs::remove_file(dir.path().join("b.xml")).unwrap();
    e.refresh().unwrap();
    assert_eq!(
        e.feature_count(),
        0,
        "successfully empty sources really clear warnings"
    );
}

#[test]
fn wis2_status_and_sender_filtering_precede_withdrawal() {
    let mut c = config_for("unused", None);
    c.data_path = None;
    c.wis2 = Some(ds_core::config::Wis2Config::default());
    let e = CapEngine::new(&c, "cap").unwrap();
    push(&e, XML.into(), "first", now());
    push(
        &e,
        XML.replace("test@meteocore.example", "other@provider.example"),
        "second",
        now(),
    );
    push(
        &e,
        cancellation().replace("<status>Actual</status>", "<status>Test</status>"),
        "test-cancel",
        now() + Duration::seconds(1),
    );
    e.refresh_with(now).unwrap();
    assert_eq!(e.feature_count(), 2);
    push(
        &e,
        cancellation(),
        "actual-cancel",
        now() + Duration::seconds(2),
    );
    e.refresh_with(now).unwrap();
    assert_eq!(e.feature_count(), 1);
    let page = e.get_features(&FeatureQuery::default()).unwrap();
    assert_eq!(
        page.features[0].properties["sender"].as_str(),
        Some("other@provider.example")
    );
}

#[test]
fn wis2_old_reference_does_not_withdraw_new_sent_or_replayed_reissue() {
    let mut c = config_for("unused", None);
    c.data_path = None;
    c.wis2 = Some(ds_core::config::Wis2Config::default());
    let e = CapEngine::new(&c, "cap").unwrap();
    let revised = XML.replace(
        "<sent>2020-01-01T00:00:00+00:00</sent>",
        "<sent>2026-09-13T12:00:00+00:00</sent>",
    );
    push(&e, revised, "reissue", now());
    push(
        &e,
        cancellation(),
        "cancel-old-sent",
        now() + Duration::seconds(1),
    );
    e.refresh_with(now).unwrap();
    assert_eq!(e.feature_count(), 1);

    // Stored Updates must not re-apply old withdrawals at every rebuild.
    push(&e, XML.into(), "original", now() + Duration::seconds(2));
    push(
        &e,
        cancellation().replace("<msgType>Cancel</msgType>", "<msgType>Update</msgType>"),
        "update",
        now() + Duration::seconds(3),
    );
    push(&e, XML.into(), "after-update", now() + Duration::seconds(4));
    for _ in 0..2 {
        e.refresh_with(now).unwrap();
        assert_eq!(e.feature_count(), 2);
    }
}

#[test]
fn dateline_query_keeps_both_sides_and_deduplicates() {
    let dir = tempfile::tempdir().unwrap();
    let xml = include_str!("fixtures/helsinki-flood.xml");
    let parsed =
        engine_cap::CapEngine::new(&config_for(dir.path().to_str().unwrap(), None), "cap").unwrap();
    // Move the entire fixture polygon to each side without crossing a ring.
    for (name, polygon) in [
        ("east", "0,175 0,176 1,176 1,175 0,175"),
        ("west", "0,-176 0,-175 1,-175 1,-176 0,-176"),
        (
            "both",
            "0,175 0,176 1,176 1,175 0,175</polygon><polygon>0,-176 0,-175 1,-175 1,-176 0,-176",
        ),
    ] {
        let a = xml.find("<polygon>").unwrap() + "<polygon>".len();
        let b = xml.find("</polygon>").unwrap();
        let moved = xml
            .replace(&xml[a..b], polygon)
            .replace("urn:test:helsinki-flood-1", name);
        std::fs::write(dir.path().join(name).with_extension("xml"), moved).unwrap();
    }
    parsed.refresh().unwrap();
    let page = parsed
        .get_features(&FeatureQuery {
            bbox: Some(Bbox::new(170., -10., -170., 10.).unwrap()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        page.number_matched, 3,
        "the area spanning both query halves is returned once"
    );
}

#[test]
fn filtered_newer_duplicate_cannot_replace_actual_message() {
    let (dir, e) = local();
    let filtered = XML
        .replace("<status>Actual</status>", "<status>Test</status>")
        .replace(
            "<sent>2020-01-01T00:00:00+00:00</sent>",
            "<sent>2027-01-01T00:00:00+00:00</sent>",
        );
    std::fs::write(dir.path().join("test.xml"), filtered).unwrap();
    e.refresh().unwrap();
    assert_eq!(e.feature_count(), 1);
    assert_eq!(
        e.get_features(&FeatureQuery::default()).unwrap().features[0].properties["status"].as_str(),
        Some("Actual")
    );
}

#[test]
fn deletion_tombstone_survives_backlog_and_older_deletion_keeps_reissue() {
    let mut c = config_for("unused", None);
    c.data_path = None;
    c.wis2 = Some(ds_core::config::Wis2Config::default());
    c.retention_grace = "PT1S".into();
    let e = CapEngine::new(&c, "cap").unwrap();
    let raw: serde_json::Value = serde_json::from_str(include_str!(
        "wis2-fixtures/meteoalarm-mk-notification.json"
    ))
    .unwrap();
    let mut n = parse_notification(
        raw["topic"].as_str().unwrap(),
        &serde_json::to_vec(&raw["message"]).unwrap(),
    )
    .unwrap();
    n.data_id = "document".into();
    n.pubtime = now() - Duration::hours(2);
    let src = e.wis2_source().unwrap();
    src.apply_with_hint(
        Resolved {
            notification: n.clone(),
            payload: None,
        },
        None,
        "cap",
        now(),
    );
    e.refresh_with(|| now() + Duration::seconds(5)).unwrap();
    push(&e, XML.into(), "document", now() - Duration::hours(3));
    assert_eq!(
        src.len(),
        0,
        "pruning is anchored to receipt and must cover in-flight downloads"
    );
    push(&e, XML.into(), "document", now());
    src.apply_with_hint(
        Resolved {
            notification: n,
            payload: None,
        },
        None,
        "cap",
        now(),
    );
    e.refresh_with(now).unwrap();
    assert_eq!(
        e.feature_count(),
        1,
        "an older deletion cannot remove the newer publication"
    );
}
