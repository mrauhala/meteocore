//! An engine's error on `/items` and `/items/{featureId}` maps to its HTTP
//! status, its class's (#17): a request the engine turns down (a station
//! grid's analysis cap) is a 400 with its message, an engine that is not
//! ready a 503 with `Retry-After`, a data source that keeps refusing reads a
//! 503 with a longer one, a missing feature or another absent resource a
//! 404, and anything else a 500 that names nothing internal.

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

#[path = "../../api-common/tests/support/engine_errors.rs"]
mod engine_errors;

/// Fails every request with the error `fail` makes.
struct Failing(Box<dyn Fn() -> DataServerError + Send + Sync>);

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

async fn get(
    fail: impl Fn() -> DataServerError + Send + Sync + 'static,
    uri: &str,
) -> (StatusCode, Option<String>, Value) {
    let state = FeaturesState {
        engines: HashMap::from([(
            "grid".to_string(),
            Arc::new(Failing(Box::new(fail))) as Arc<dyn FeatureEngine>,
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

        // A data source that keeps refusing reads (#1355): a longer wait,
        // and nothing of the upstream in the body.
        let (status, retry, body) = get(
            || DataServerError::Unavailable("GET https://bucket.example/x: 503 Slow Down".into()),
            uri,
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(retry.as_deref(), Some("10"));
        assert_eq!(body["code"], "SourceUnavailable");
        assert_eq!(
            body["description"],
            "Data source temporarily unavailable, try again later"
        );

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

/// Every engine error answers its class's status on both routes (#17): an
/// absent collection, location or model run is the 404 every API answers,
/// no longer a 500, and no 5xx names the engine's detail.
#[tokio::test]
async fn every_engine_error_answers_its_class_status() {
    for uri in ["/collections/grid/items", "/collections/grid/items/x"] {
        for (i, error) in engine_errors::every_error().into_iter().enumerate() {
            let (status, retry, body) =
                get(move || engine_errors::every_error().swap_remove(i), uri).await;
            assert_eq!(
                status.as_u16(),
                error.class().status_code(),
                "{uri} {error:?}"
            );
            assert_eq!(
                retry.as_deref(),
                engine_errors::retry_after(&error),
                "{uri} {error:?}"
            );
            let description = body["description"].as_str().unwrap();
            if status.is_server_error() {
                assert!(
                    !description.contains(engine_errors::SECRET),
                    "{uri} {error:?}: {description}"
                );
            }
        }
    }
    // An absent resource names itself, as in every API's 404.
    let (status, _, body) = get(
        || DataServerError::ReferenceTimeNotFound("2026-06-07T00:00:00Z".into()),
        "/collections/grid/items",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        serde_json::json!({
            "code": "NotFound",
            "description": "Reference time not found: 2026-06-07T00:00:00Z",
        })
    );
}
