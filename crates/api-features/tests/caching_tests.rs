//! Cache-Control + ETag / If-None-Match behavior (#499).
//!
//! Items responses carry a per-request `timeStamp`, so their ETag is
//! precomputed over the document with the timestamp excluded — otherwise
//! revalidation could never match. The other endpoints hash the body as-is.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use api_features::handlers::FeaturesState;
use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
use ds_core::feature::*;
use ds_core::feature_engine::FeatureEngine;

struct MockFeatureEngine {
    features: Vec<Feature>,
}

impl MockFeatureEngine {
    fn new() -> Self {
        Self {
            features: vec![Feature {
                id: "helsinki".into(),
                geometry: Geometry::Point {
                    x: 24.9384,
                    y: 60.1699,
                }
                .into(),
                properties: {
                    // Several properties in a fresh-per-request HashMap: with
                    // serde_json's workspace-enabled `preserve_order`,
                    // unsorted map serialization would emit a different key
                    // order per request — the ETag/304 tests below would then
                    // fail, guarding the sorted-serialization fix.
                    let mut m = HashMap::new();
                    for (k, v) in [
                        ("name", "Helsinki"),
                        ("country", "FI"),
                        ("region", "Uusimaa"),
                        ("timezone", "Europe/Helsinki"),
                        ("kind", "capital"),
                        ("status", "active"),
                    ] {
                        m.insert(k.into(), PropertyValue::String(v.into()));
                    }
                    m.into()
                },
            }],
        }
    }
}

impl FeatureEngine for MockFeatureEngine {
    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        let features: Vec<Feature> = self.features.iter().take(query.limit).cloned().collect();
        Ok(FeaturePage {
            number_matched: self.features.len(),
            number_returned: features.len(),
            features,
            next_offset: None,
        })
    }

    fn get_feature(&self, feature_id: &str) -> Result<Feature, DataServerError> {
        self.features
            .iter()
            .find(|f| f.id == feature_id)
            .cloned()
            .ok_or_else(|| DataServerError::FeatureNotFound(feature_id.into()))
    }

    fn feature_count(&self) -> usize {
        self.features.len()
    }

    fn spatial_extent(&self) -> Option<[f64; 4]> {
        Some([23.7610, 60.1699, 24.9384, 61.4978])
    }
}

fn build_router() -> axum::Router {
    let engine: Arc<dyn FeatureEngine> = Arc::new(MockFeatureEngine::new());
    let mut engines: HashMap<String, Arc<dyn FeatureEngine>> = HashMap::new();
    let mut collections = HashMap::new();
    engines.insert("cities".to_string(), engine);
    collections.insert(
        "cities".to_string(),
        CollectionConfig {
            id: "cities".to_string(),
            title: "Cities".to_string(),
            description: "Test collection".to_string(),
            data_path: None,
            apis: vec!["features".to_string()],
            engine_type: "geojson".to_string(),
            keywords: Vec::new(),
            license: None,
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            zarr: None,
            odim: None,
            cap: None,
            postgis: None,
            nowcast: None,
            preview: None,
        },
    );
    api_features::router(Arc::new(ArcSwap::from_pointee(FeaturesState {
        engines,
        collections,
        base_url: String::new(),
        trust_proxy_headers: false,
    })))
}

async fn get_response(uri: &str, if_none_match: Option<&str>) -> axum::response::Response {
    let mut req = Request::builder().uri(uri);
    if let Some(inm) = if_none_match {
        req = req.header(header::IF_NONE_MATCH, inm);
    }
    build_router()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn header_str(resp: &axum::response::Response, name: header::HeaderName) -> Option<&str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

#[tokio::test]
async fn metadata_endpoints_carry_short_cache_control_and_etag() {
    for uri in ["/", "/conformance", "/collections", "/collections/cities"] {
        let resp = get_response(uri, None).await;
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        assert_eq!(
            header_str(&resp, header::CACHE_CONTROL),
            Some("public, max-age=60"),
            "{uri}"
        );
        assert!(header_str(&resp, header::ETAG).is_some(), "{uri}");
    }
}

#[tokio::test]
async fn items_etag_excludes_the_response_timestamp() {
    let resp = get_response("/collections/cities/items", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let etag = header_str(&resp, header::ETAG).unwrap().to_owned();
    let body = resp.into_body().collect().await.unwrap().to_bytes();

    // The advertised ETag must equal the hash of the document with the
    // per-request timeStamp blanked — proving revalidation is immune to the
    // generation time (and that the middleware honoured the precomputed tag).
    let mut doc: Value = serde_json::from_slice(&body).unwrap();
    assert!(doc["timeStamp"].as_str().is_some_and(|t| !t.is_empty()));
    doc["timeStamp"] = json!("");
    let recomputed = ds_core::http_cache::etag_of(serde_json::to_string(&doc).unwrap().as_bytes());
    assert_eq!(etag, recomputed);

    // And it revalidates: a second request (new timeStamp) still 304s.
    let second = get_response("/collections/cities/items", Some(&etag)).await;
    assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
    let body = second.into_body().collect().await.unwrap().to_bytes();
    assert!(body.is_empty());
}

#[tokio::test]
async fn items_cache_control_follows_the_datetime_window() {
    // Settled: closed interval entirely in the past.
    let resp = get_response(
        "/collections/cities/items?datetime=2024-01-01T00:00:00Z/2024-01-01T06:00:00Z",
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        header_str(&resp, header::CACHE_CONTROL),
        Some("public, max-age=86400")
    );

    // Open-ended or absent windows stay short.
    for uri in [
        "/collections/cities/items",
        "/collections/cities/items?datetime=../2024-01-01T06:00:00Z",
        "/collections/cities/items?datetime=2024-01-01T00:00:00Z/..",
    ] {
        let resp = get_response(uri, None).await;
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        assert_eq!(
            header_str(&resp, header::CACHE_CONTROL),
            Some("public, max-age=60"),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn single_feature_revalidates_to_304() {
    let first = get_response("/collections/cities/items/helsinki", None).await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(
        header_str(&first, header::CACHE_CONTROL),
        Some("public, max-age=60")
    );
    let etag = header_str(&first, header::ETAG).unwrap().to_owned();

    let second = get_response("/collections/cities/items/helsinki", Some(&etag)).await;
    assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(header_str(&second, header::ETAG), Some(etag.as_str()));
    let body = second.into_body().collect().await.unwrap().to_bytes();
    assert!(body.is_empty());
}

#[tokio::test]
async fn error_responses_are_left_alone() {
    let resp = get_response("/collections/cities/items/nope", None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(resp.headers().get(header::ETAG).is_none());
    assert!(resp.headers().get(header::CACHE_CONTROL).is_none());
}
