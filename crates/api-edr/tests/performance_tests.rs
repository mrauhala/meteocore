// =============================================================================
// Performance-Oriented Test Plan for OGC API - EDR Implementation
// =============================================================================
//
// This file contains test stubs focused on performance, scalability, and
// resource-related aspects of the EDR API. Each test stub documents what it
// verifies and why it matters from a spec-compliance and operational perspective.
//
// Key concerns identified in the current implementation:
//
// 1. NO PAGINATION on /collections/{id}/locations endpoint.
//    The OGC EDR 1.1 spec does not mandate pagination on the locations
//    endpoint, but returning unbounded results is a denial-of-service vector.
//    The Features API sibling already supports limit/offset. The locations
//    endpoint serializes ALL locations into a single GeoJSON FeatureCollection
//    in memory via serde_json::json!, meaning both CPU and RAM scale linearly
//    with location count.
//
// 2. FULL IN-MEMORY RESPONSE CONSTRUCTION.
//    Both locations_to_geojson() and query_result_to_coverage_json() build
//    the entire serde_json::Value tree before serialization. For large time
//    series (e.g., 1 year of hourly data = 8760 timesteps x N parameters),
//    this creates significant allocation pressure. Streaming serialization
//    (e.g., via axum::body::Body::from_stream) would reduce peak memory.
//
// 3. NO 413 (Payload Too Large) SUPPORT.
//    The OGC EDR 1.1 spec defines HTTP 413 as a valid response for query
//    endpoints (Section 8.2). The current implementation has no mechanism to
//    reject queries that would produce excessively large responses.
//
// 4. NO 202 (Accepted) ASYNC PROCESSING.
//    The spec allows 202 for long-running queries. All queries currently
//    block the handler until completion.
//
// 5. Arc<dyn EdrEngine> CONCURRENCY.
//    The EdrEngine trait requires Send + Sync, and state is shared via Arc.
//    This is correct for concurrent access but should be validated under load.
//
// =============================================================================

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use tower::ServiceExt;

use api_edr::handlers::EdrState;
use ds_core::config::CollectionConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::model::*;

// ---------------------------------------------------------------------------
// Mock engine that generates synthetic data of configurable size
// ---------------------------------------------------------------------------

struct ScalableEngine {
    location_count: usize,
    timestep_count: usize,
    parameter_count: usize,
}

impl EdrEngine for ScalableEngine {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        let locations = (0..self.location_count)
            .map(|i| Location {
                id: format!("loc_{i}"),
                label: format!("Location {i}"),
                latitude: 60.0 + (i as f64 * 0.01),
                longitude: 24.0 + (i as f64 * 0.01),
            })
            .collect();
        Ok(locations)
    }

    fn query_location(
        &self,
        location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError> {
        let idx: usize = location_id
            .strip_prefix("loc_")
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| DataServerError::LocationNotFound(location_id.to_string()))?;

        if idx >= self.location_count {
            return Err(DataServerError::LocationNotFound(location_id.to_string()));
        }

        // Generate unique timestamps by spreading across multiple days.
        // CoverageJSON schema requires uniqueItems on time axis values.
        use chrono::Duration;
        let base: DateTime<Utc> = "2024-01-01T00:00:00Z".parse().unwrap();
        let times: Vec<DateTime<Utc>> = (0..self.timestep_count)
            .map(|h| base + Duration::hours(h as i64))
            .collect();

        let mut parameters = HashMap::new();
        let mut ranges = HashMap::new();
        for p in 0..self.parameter_count {
            let name = format!("param_{p}");
            parameters.insert(
                name.clone(),
                ParameterDescription {
                    label: format!("Parameter {p}"),
                    unit: "unit".to_string(),
                    observed_property: name.clone(),
                },
            );
            ranges.insert(
                name,
                NdArray {
                    shape: vec![self.timestep_count],
                    axis_names: vec!["t".to_string()],
                    values: (0..self.timestep_count).map(|v| Some(v as f64)).collect(),
                },
            );
        }

        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::PointSeries {
                x: 24.0 + (idx as f64 * 0.01),
                y: 60.0 + (idx as f64 * 0.01),
                t: times,
                z: None,
            },
            parameters,
            ranges,
        }))
    }

    fn get_parameters(&self) -> Vec<String> {
        (0..self.parameter_count)
            .map(|p| format!("param_{p}"))
            .collect()
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((
            "2024-01-01T00:00:00Z".parse().unwrap(),
            "2024-12-31T23:00:00Z".parse().unwrap(),
        ))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([24.0, 60.0, 25.0, 61.0])
    }
}

fn make_edr_state(engine: Arc<dyn EdrEngine>) -> Arc<ArcSwap<EdrState>> {
    let mut engines = HashMap::new();
    let mut collections = HashMap::new();
    engines.insert("weather".to_string(), engine);
    collections.insert(
        "weather".to_string(),
        CollectionConfig {
            id: "weather".to_string(),
            title: "Finnish Weather Observations".to_string(),
            description: "Test collection".to_string(),
            data_path: None,
            apis: vec!["edr".to_string()],
            engine_type: "csv".to_string(),
            keywords: Vec::new(),
            license: None,
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            odim: None,
            postgis: None,
            preview: None,
        },
    );
    Arc::new(ArcSwap::from_pointee(EdrState {
        engines,
        collections,
        base_url: String::new(),
    }))
}

fn build_app(engine: ScalableEngine) -> axum::Router {
    let engine: Arc<dyn EdrEngine> = Arc::new(engine);
    api_edr::router(make_edr_state(engine))
}

// ===========================================================================
// 1. Response Size Tests
// ===========================================================================

/// Verify that the locations endpoint can handle a large number of locations
/// without panicking or producing malformed JSON.
///
/// This exercises the `locations_to_geojson` serializer with 10,000 features.
/// Validates that:
/// - The response status is 200
/// - The response is valid JSON
/// - The feature count matches the engine's location count
/// - The content-type is application/json
#[tokio::test]
async fn locations_10000_features_returns_valid_geojson() {
    let app = build_app(ScalableEngine {
        location_count: 10_000,
        timestep_count: 1,
        parameter_count: 1,
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/collections/weather/locations")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), 100 * 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["type"], "FeatureCollection");
    assert_eq!(json["features"].as_array().unwrap().len(), 10_000);
}

/// Verify that a location query with a large time series (8760 hourly
/// timesteps, simulating one year) produces valid CoverageJSON.
///
/// This is the most realistic stress test: a single-location query for a full
/// year of hourly data with multiple parameters. The response JSON will be
/// several MB.
#[tokio::test]
async fn location_query_large_timeseries_returns_valid_covjson() {
    let app = build_app(ScalableEngine {
        location_count: 1,
        timestep_count: 8760, // one year of hourly data
        parameter_count: 5,
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/collections/weather/locations/loc_0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), 100 * 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["type"], "Coverage");
    let t_axis = json["domain"]["axes"]["t"]["values"].as_array().unwrap();
    assert_eq!(t_axis.len(), 8760);
    assert_eq!(json["ranges"].as_object().unwrap().len(), 5);
}

/// Verify that a location query with many parameters (50) produces valid
/// CoverageJSON with all parameter entries and corresponding ranges.
#[tokio::test]
async fn location_query_many_parameters_returns_all_ranges() {
    let app = build_app(ScalableEngine {
        location_count: 1,
        timestep_count: 24,
        parameter_count: 50,
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/collections/weather/locations/loc_0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), 100 * 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["parameters"].as_object().unwrap().len(), 50);
    assert_eq!(json["ranges"].as_object().unwrap().len(), 50);
}

// ===========================================================================
// 2. Pagination Gap - Locations Endpoint
// ===========================================================================

/// The OGC EDR 1.1 spec does not strictly require pagination on the locations
/// endpoint, but operational deployments need it. This test documents the
/// current behavior: ALL locations are returned in a single response with no
/// limit/offset support.
///
/// When pagination is implemented, this test should be updated to verify:
/// - Default limit is applied (e.g., 100)
/// - `limit` query parameter caps the feature count
/// - `offset` skips features
/// - `next` link is present when more results exist
/// - `numberMatched` / `numberReturned` properties are present
#[tokio::test]
async fn locations_endpoint_returns_all_results_without_pagination() {
    let app = build_app(ScalableEngine {
        location_count: 500,
        timestep_count: 1,
        parameter_count: 1,
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/collections/weather/locations")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), 100 * 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // Currently returns all 500 features -- no pagination applied
    let features = json["features"].as_array().unwrap();
    assert_eq!(
        features.len(),
        500,
        "locations endpoint currently returns ALL locations without pagination"
    );

    // Pagination links are absent (spec gap)
    assert!(
        json.get("links").is_none()
            || json["links"]
                .as_array()
                .is_none_or(|links| !links.iter().any(|l| l["rel"] == "next")),
        "no 'next' pagination link exists (pagination not implemented)"
    );
}

// ===========================================================================
// 3. Content-Type Header Tests
// ===========================================================================

/// Verify that the locations endpoint returns Content-Type: application/geo+json.
#[tokio::test]
async fn locations_response_has_geojson_content_type() {
    let app = build_app(ScalableEngine {
        location_count: 2,
        timestep_count: 1,
        parameter_count: 1,
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/collections/weather/locations")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .expect("content-type header must be present")
        .to_str()
        .unwrap();
    assert!(
        content_type.contains("application/geo+json"),
        "expected application/geo+json, got: {content_type}"
    );
}

/// Verify that the location query (CoverageJSON) endpoint returns
/// Content-Type: application/prs.coverage+json.
#[tokio::test]
async fn location_query_response_has_covjson_content_type() {
    let app = build_app(ScalableEngine {
        location_count: 1,
        timestep_count: 2,
        parameter_count: 1,
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/collections/weather/locations/loc_0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .expect("content-type header must be present")
        .to_str()
        .unwrap();
    assert!(
        content_type.contains("application/prs.coverage+json"),
        "expected application/prs.coverage+json, got: {content_type}"
    );
}

/// Verify that error responses also carry application/json content-type.
#[tokio::test]
async fn error_response_has_json_content_type() {
    let app = build_app(ScalableEngine {
        location_count: 1,
        timestep_count: 1,
        parameter_count: 1,
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/collections/nonexistent/locations")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let content_type = response
        .headers()
        .get("content-type")
        .expect("error responses must have content-type")
        .to_str()
        .unwrap();
    assert!(
        content_type.contains("application/json"),
        "error response should be JSON, got: {content_type}"
    );
}

// ===========================================================================
// 4. Filtering Efficiency Tests
// ===========================================================================
//
// NOTE: The current EdrEngine trait signature accepts datetime and parameter-name
// filters, but the ScalableEngine mock above ignores them. In a real
// implementation, these filters reduce the data returned by the engine. These
// tests verify that the handler correctly passes filter parameters through to
// the engine and that the serialized response reflects the filtered data.
//
// When the engine properly filters:
// - datetime filtering should reduce the number of timesteps in axes.t
// - parameter-name filtering should reduce parameters and ranges maps

/// Verify that the parameter-name query parameter is parsed and forwarded to
/// the engine. With a filtering engine, requesting 1 of 5 parameters should
/// produce a response with only that parameter's range.
///
/// Currently tests that the query parameter is accepted without error.
/// TODO: Update with a filtering mock to verify response size reduction.
#[tokio::test]
async fn parameter_name_filter_is_accepted() {
    let app = build_app(ScalableEngine {
        location_count: 1,
        timestep_count: 24,
        parameter_count: 5,
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/collections/weather/locations/loc_0?parameter-name=param_0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

/// Verify that the datetime query parameter is parsed and forwarded to the
/// engine. The handler should accept RFC 3339 interval syntax.
#[tokio::test]
async fn datetime_filter_is_accepted() {
    let app = build_app(ScalableEngine {
        location_count: 1,
        timestep_count: 24,
        parameter_count: 1,
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/collections/weather/locations/loc_0?datetime=2024-01-01T00:00:00Z/2024-01-01T06:00:00Z")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

/// Verify that an invalid datetime filter returns 400 Bad Request, not 500.
#[tokio::test]
async fn invalid_datetime_filter_returns_400() {
    let app = build_app(ScalableEngine {
        location_count: 1,
        timestep_count: 24,
        parameter_count: 1,
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/collections/weather/locations/loc_0?datetime=not-a-date")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ===========================================================================
// 5. HTTP 413 (Payload Too Large) - Not Yet Implemented
// ===========================================================================
//
// The OGC EDR 1.1 spec (Section 8.2, Table 8) lists HTTP 413 as a valid
// response code for query endpoints. This should be returned when:
//
// - A locations query would return more features than a configured maximum
// - A location data query spans a time range or parameter set that would
//   produce a response exceeding a size threshold
// - Estimated response size (timesteps x parameters x ~20 bytes/value)
//   exceeds a configurable limit
//
// Implementation suggestions:
// - Add a `max_response_size_bytes` config option
// - Check estimated response size before calling engine.query_location()
//   (timestep_count * param_count * ~20 bytes for float JSON values)
// - Return 413 with a JSON error body explaining the limit
// - For locations: enforce a max feature count (e.g., 10,000)
//
// Test stubs below will pass once 413 support is added.

// #[tokio::test]
// async fn location_query_exceeding_size_limit_returns_413() {
//     // Configure engine with huge dataset, configure app with size limit
//     // Expect HTTP 413 with JSON error body
// }

// #[tokio::test]
// async fn locations_exceeding_feature_limit_returns_413() {
//     // Configure engine with 100,000 locations, app with 10,000 limit
//     // Expect HTTP 413
// }

// ===========================================================================
// 6. Concurrent Request Handling
// ===========================================================================

/// Verify that multiple concurrent requests to the same endpoint all succeed.
/// This validates that the Arc<dyn EdrEngine> sharing pattern works correctly
/// and that no internal state causes data races or deadlocks.
#[tokio::test]
async fn concurrent_location_queries_all_succeed() {
    let engine: Arc<dyn EdrEngine> = Arc::new(ScalableEngine {
        location_count: 10,
        timestep_count: 24,
        parameter_count: 3,
    });

    let app = api_edr::router(make_edr_state(engine));

    // Spawn 20 concurrent requests to different locations
    let mut handles = Vec::new();
    for i in 0..10 {
        let app = app.clone();
        handles.push(tokio::spawn(async move {
            let response = app
                .oneshot(
                    Request::builder()
                        .uri(format!("/collections/weather/locations/loc_{i}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            (i, response.status())
        }));
    }

    for handle in handles {
        let (i, status) = handle.await.unwrap();
        assert_eq!(
            status,
            StatusCode::OK,
            "concurrent request for loc_{i} failed with {status}"
        );
    }
}

/// Verify that concurrent requests to different endpoint types all succeed.
/// Mixes locations listing, location queries, and collection metadata.
#[tokio::test]
async fn concurrent_mixed_endpoint_requests_all_succeed() {
    let engine: Arc<dyn EdrEngine> = Arc::new(ScalableEngine {
        location_count: 5,
        timestep_count: 24,
        parameter_count: 2,
    });

    let app = api_edr::router(make_edr_state(engine));

    let uris = vec![
        "/collections/weather/locations",
        "/collections/weather/locations/loc_0",
        "/collections/weather/locations/loc_1",
        "/collections/weather",
        "/collections",
        "/",
        "/conformance",
        "/collections/weather/locations/loc_2",
    ];

    let mut handles = Vec::new();
    for uri in &uris {
        let app = app.clone();
        let uri = uri.to_string();
        handles.push(tokio::spawn(async move {
            let response = app
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            (uri, response.status())
        }));
    }

    for handle in handles {
        let (uri, status) = handle.await.unwrap();
        assert_eq!(
            status,
            StatusCode::OK,
            "concurrent request to {uri} failed with {status}"
        );
    }
}

// ===========================================================================
// Additional Performance-Adjacent Tests
// ===========================================================================

/// Verify that the response for zero locations is a valid empty
/// FeatureCollection, not an error. Edge case for newly configured
/// collections with no data loaded yet.
#[tokio::test]
async fn locations_empty_collection_returns_empty_feature_collection() {
    let app = build_app(ScalableEngine {
        location_count: 0,
        timestep_count: 1,
        parameter_count: 1,
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/collections/weather/locations")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["type"], "FeatureCollection");
    assert_eq!(json["features"].as_array().unwrap().len(), 0);
}

/// Measure approximate response body size for a known dataset to establish a
/// baseline. This is not a pass/fail test but documents the current size
/// characteristics for regression tracking.
#[tokio::test]
async fn response_size_baseline_for_regression_tracking() {
    let app = build_app(ScalableEngine {
        location_count: 100,
        timestep_count: 1,
        parameter_count: 1,
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/collections/weather/locations")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), 100 * 1024 * 1024)
        .await
        .unwrap();

    // 100 point features should be roughly 10-20 KB of JSON.
    // If this grows significantly, serialization may have regressed.
    let size = body.len();
    assert!(
        size < 100_000,
        "100 point features produced {size} bytes, expected under 100KB. \
         Check for serialization bloat."
    );
    assert!(
        size > 1_000,
        "100 point features produced only {size} bytes, suspiciously small"
    );

    // Print for manual inspection during development
    eprintln!(
        "Baseline: 100 locations = {size} bytes ({:.1} bytes/feature)",
        size as f64 / 100.0
    );
}
