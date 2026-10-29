//! An engine's error on `/items` and `/items/{featureId}` maps to its HTTP
//! status: a request the engine turns down (a station grid's analysis cap)
//! is a 400 with its message, an engine that is not ready a 503 with
//! `Retry-After`, a missing feature a 404, and anything else a 500 that
//! names nothing internal.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use api_features::handlers::FeaturesState;
use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
use ds_core::feature::{Feature, FeaturePage, FeatureQuery};
use ds_core::feature_engine::FeatureEngine;

/// Fails every request with the error `fail` makes.
struct Failing(fn() -> DataServerError);

impl FeatureEngine for Failing {
    fn get_features(&self, _: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        Err((self.0)())
    }

    fn get_feature(&self, _: &str) -> Result<Feature, DataServerError> {
        Err((self.0)())
    }

    fn feature_count(&self) -> usize {
        0
    }
}

fn collection() -> CollectionConfig {
    serde_json::from_value(serde_json::json!({
        "id": "grid", "title": "Grid", "description": "",
        "engine_type": "station-grid", "apis": ["features"],
    }))
    .expect("a collection config")
}

async fn get(fail: fn() -> DataServerError, uri: &str) -> (StatusCode, Option<String>, Value) {
    let state = FeaturesState {
        engines: HashMap::from([(
            "grid".to_string(),
            Arc::new(Failing(fail)) as Arc<dyn FeatureEngine>,
        )]),
        collections: HashMap::from([("grid".to_string(), collection())]),
        discovery: Default::default(),
        base_url: String::new(),
        trust_proxy_headers: false,
        vector_tileset_ids: Default::default(),
        collection_pages: Default::default(),
    };
    let app = api_features::router(Arc::new(ArcSwap::from_pointee(state)));
    let response = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let retry_after = response
        .headers()
        .get(header::RETRY_AFTER)
        .map(|v| v.to_str().unwrap().to_string());
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, retry_after, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn engine_errors_map_to_their_status() {
    for uri in ["/collections/grid/items", "/collections/grid/items/x"] {
        let (status, retry, body) = get(
            || DataServerError::QueryTooLarge("the limit is 96".into()),
            uri,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(retry, None);
        assert_eq!(body["code"], "BadRequest");
        assert_eq!(body["description"], "Query too large: the limit is 96");

        let (status, retry, body) = get(|| DataServerError::ResourceExhausted, uri).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(retry.as_deref(), Some("1"));
        assert_eq!(body["code"], "ServerBusy");

        let (status, retry, _) = get(|| DataServerError::DeadlineExceeded, uri).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(retry.as_deref(), Some("1"));

        let (status, _, body) = get(
            || DataServerError::Engine("secret detail at /var/lib".into()),
            uri,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{uri}");
        assert_eq!(body["description"], "Internal server error");
    }
    let (status, _, body) = get(
        || DataServerError::FeatureNotFound("x".into()),
        "/collections/grid/items/x",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["description"], "Feature not found");
}
