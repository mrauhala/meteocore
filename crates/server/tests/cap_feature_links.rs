//! CAP through the real engine and Features router: links followed from source
//! CAP XML, and the standard property filters of an empty catalog (#1001).
use std::{collections::HashMap, sync::Arc};

use arc_swap::ArcSwap;
use axum::{body::Body, http::Request, Router};
use ds_core::{
    config::{CapConfig, CollectionConfig},
    feature_engine::FeatureEngine,
};
use http_body_util::BodyExt;
use quick_xml::{
    events::{BytesEnd, BytesStart, BytesText, Event},
    Writer,
};
use serde_json::{json, Value};
use tower::ServiceExt;

const BASE: &str = "https://example.test";
const SENDER: &str = "https://meteo.hr";
const IDS: &[&str] = &[
    "2.49.0.0.191.0.HR.260918141920_3_yellow_HR008",
    "zone/1",
    "zone%2F1",
    "zone[2] %?# &+ Ž",
];

fn cap_xml(identifier: &str) -> Vec<u8> {
    let mut writer = Writer::new(Vec::new());
    writer
        .write_event(Event::Start(BytesStart::new("alert")))
        .unwrap();
    for (name, value) in [
        ("sender", SENDER),
        ("identifier", identifier),
        ("status", "Actual"),
    ] {
        writer
            .write_event(Event::Start(BytesStart::new(name)))
            .unwrap();
        writer
            .write_event(Event::Text(BytesText::new(value)))
            .unwrap();
        writer.write_event(Event::End(BytesEnd::new(name))).unwrap();
    }
    writer
        .write_event(Event::Start(BytesStart::new("info")))
        .unwrap();
    writer
        .write_event(Event::Start(BytesStart::new("area")))
        .unwrap();
    writer
        .write_event(Event::End(BytesEnd::new("area")))
        .unwrap();
    writer
        .write_event(Event::End(BytesEnd::new("info")))
        .unwrap();
    writer
        .write_event(Event::End(BytesEnd::new("alert")))
        .unwrap();
    writer.into_inner()
}

async fn get(app: &Router, href: &str) -> String {
    let path = href.strip_prefix(BASE).unwrap_or(href);
    let response = app
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "following {href}");
    String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}

/// The real CAP engine over `dir`, served as collection `cap` under `/features`.
fn features_app(dir: &std::path::Path) -> (Router, Arc<dyn FeatureEngine>) {
    let cfg: CapConfig = serde_json::from_value(json!({"data_path": dir})).unwrap();
    let engine: Arc<dyn FeatureEngine> = Arc::new(engine_cap::CapEngine::new(&cfg, "cap").unwrap());
    let config: CollectionConfig = serde_json::from_value(json!({
        "id": "cap", "title": "Warnings", "description": "CAP link contract", "apis": ["features"]
    }))
    .unwrap();
    let app = Router::new().nest(
        "/features",
        api_features::router(Arc::new(ArcSwap::from_pointee(
            api_features::handlers::FeaturesState {
                engines: HashMap::from([("cap".into(), engine.clone())]),
                collections: HashMap::from([("cap".into(), config)]),
                base_url: BASE.into(),
                trust_proxy_headers: false,
                vector_tileset_ids: Default::default(),
            },
        ))),
    );
    (app, engine)
}

/// #1001: before the first alert arrives, the standard CAP fields are still
/// accepted as `/items` filters (an empty page, not a 400) and advertised as
/// `/items` parameters in the OpenAPI document.
#[tokio::test(flavor = "multi_thread")]
async fn empty_catalog_accepts_and_advertises_standard_cap_filters() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _) = features_app(dir.path());
    let page: Value = serde_json::from_str(
        &get(
            &app,
            "/features/collections/cap/items?status=Actual&scope=Public&f=json",
        )
        .await,
    )
    .unwrap();
    assert_eq!(page["numberMatched"], 0);
    assert_eq!(page["features"].as_array().unwrap().len(), 0);

    let api: Value = serde_json::from_str(&get(&app, "/features/api").await).unwrap();
    let (_, items) = api["paths"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(path, _)| path.ends_with("/collections/cap/items"))
        .expect("cap /items operation");
    let names: Vec<&str> = items["get"]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    for name in ["status", "scope", "msgType"] {
        assert!(names.contains(&name), "{name} not in {names:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cap_identifiers_round_trip_through_json_and_html_links() {
    let dir = tempfile::tempdir().unwrap();
    for (i, id) in IDS.iter().enumerate() {
        std::fs::write(dir.path().join(format!("{i}.xml")), cap_xml(id)).unwrap();
    }
    let (app, engine) = features_app(dir.path());
    let listing: Value =
        serde_json::from_str(&get(&app, "/features/collections/cap/items?f=json").await).unwrap();
    let features = listing["features"].as_array().unwrap();
    assert_eq!(features.len(), IDS.len());
    let listing_html = get(&app, "/features/collections/cap/items?f=html").await;
    for feature in features {
        let raw_id = feature["properties"]["identifier"].as_str().unwrap();
        assert!(IDS.contains(&raw_id));
        assert_eq!(feature["properties"]["sender"], SENDER);
        for link in feature["links"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|l| l["rel"] == "self" || l["rel"] == "alternate")
        {
            let href = link["href"].as_str().unwrap();
            let body = get(&app, href).await;
            if link["type"] == "application/geo+json" {
                let item: Value = serde_json::from_str(&body).unwrap();
                assert_eq!(item["id"], feature["id"]);
                assert_eq!(item["properties"], feature["properties"]);
            } else {
                assert!(listing_html.contains(&format!("href=\"{href}\"")));
                assert!(body.contains("FEATURE DETAIL"));
                let json_href = feature["links"][0]["href"].as_str().unwrap();
                assert!(
                    body.contains(&format!("href=\"{json_href}\"")),
                    "JSON toggle must preserve identity"
                );
            }
        }
        // Domain IDs must also be directly usable by the engine's lookup API.
        let id = feature["id"].as_str().unwrap();
        assert_eq!(engine.get_feature(id).unwrap().id, id);
    }
}
