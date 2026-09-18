//! Source-file → real engine → HTTP representation contract. Renderer-only
//! fixtures cannot catch property types lost during ingestion.
use std::{collections::HashMap, sync::Arc};

use arc_swap::ArcSwap;
use axum::{body::Body, http::Request, Router};
use ds_core::{config::CollectionConfig, feature_engine::FeatureEngine};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn get(app: &Router, path: &str) -> String {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
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

#[tokio::test]
async fn geojson_source_arrays_survive_item_and_listing_responses() {
    let properties = json!({
        "name": "Sample",
        "angles": [0.3, 0.8],
        "labels": ["first", "second"],
        "mixed": [0, false, null, ""],
        "empty": [],
        "literal": "[0.3,0.8]"
    });
    let source = json!({
        "type": "FeatureCollection",
        "features": [{
            "type": "Feature", "id": "one",
            "geometry": {"type": "Point", "coordinates": [24,60]},
            "properties": properties
        }]
    });
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), source.to_string()).unwrap();
    let engine: Arc<dyn FeatureEngine> =
        Arc::new(engine_geojson::GeoJsonEngine::load(file.path().to_str().unwrap()).unwrap());
    let config: CollectionConfig = serde_json::from_value(json!({
        "id": "sample", "title": "Sample", "description": "Array contract fixture",
        "apis": ["features"]
    }))
    .unwrap();
    let app = api_features::router(Arc::new(ArcSwap::from_pointee(
        api_features::handlers::FeaturesState {
            engines: HashMap::from([("sample".into(), engine)]),
            collections: HashMap::from([("sample".into(), config)]),
            base_url: "https://example.test".into(),
            trust_proxy_headers: false,
        },
    )));

    let item: Value =
        serde_json::from_str(&get(&app, "/collections/sample/items/one?f=json").await).unwrap();
    assert_eq!(item["properties"], properties);
    let listing: Value =
        serde_json::from_str(&get(&app, "/collections/sample/items?f=json").await).unwrap();
    assert_eq!(listing["features"][0]["properties"], properties);

    let html = get(&app, "/collections/sample/items/one?f=html").await;
    for (name, count) in [("angles", 2), ("labels", 2), ("mixed", 4)] {
        let row = html
            .split(&format!("<tr data-property=\"{name}\">"))
            .nth(1)
            .unwrap()
            .split("</tr>")
            .next()
            .unwrap();
        assert!(row.contains(">array</td>"), "{name}: {row}");
        assert_eq!(row.matches("class=\"chip\"").count(), count, "{name}");
        assert!(!row.contains("<details"), "{name}: {row}");
    }

    let filtered: Value = serde_json::from_str(
        &get(
            &app,
            "/collections/sample/items?f=json&labels=second&angles=0.3",
        )
        .await,
    )
    .unwrap();
    assert_eq!(filtered["numberMatched"], 1);
    assert_eq!(filtered["features"][0]["properties"], properties);
}
