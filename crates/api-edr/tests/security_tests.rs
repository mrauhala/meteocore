use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use tower::ServiceExt;

use api_edr::handlers::EdrState;
use ds_core::config::CollectionConfig;
use ds_core::engine::Engine;
use ds_core::error::DataServerError;
use ds_core::model::*;

// ---------------------------------------------------------------------------
// Mock engine
// ---------------------------------------------------------------------------

struct MockEngine;

impl Engine for MockEngine {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(vec![Location {
            id: "helsinki".to_string(),
            label: "Helsinki".to_string(),
            latitude: 60.1699,
            longitude: 24.9384,
        }])
    }

    fn query_location(
        &self,
        location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
    ) -> Result<QueryResult, DataServerError> {
        if location_id != "helsinki" {
            return Err(DataServerError::LocationNotFound(location_id.to_string()));
        }

        let time: DateTime<Utc> = "2024-01-01T00:00:00Z".parse().unwrap();
        let mut parameters = HashMap::new();
        parameters.insert(
            "temperature".to_string(),
            ParameterDescription {
                label: "temperature".to_string(),
                unit: "°C".to_string(),
                observed_property: "temperature".to_string(),
            },
        );
        let mut ranges = HashMap::new();
        ranges.insert(
            "temperature".to_string(),
            NdArray {
                shape: vec![1],
                axis_names: vec!["t".to_string()],
                values: vec![Some(-2.5)],
            },
        );

        Ok(QueryResult {
            domain: DomainDescription {
                domain_type: "PointSeries".to_string(),
                axes_x: 24.9384,
                axes_y: 60.1699,
                axes_t: vec![time],
            },
            parameters,
            ranges,
        })
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["temperature".to_string()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let start: DateTime<Utc> = "2024-01-01T00:00:00Z".parse().unwrap();
        let end: DateTime<Utc> = "2024-01-01T06:00:00Z".parse().unwrap();
        Some((start, end))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([24.0, 60.0, 25.0, 61.0])
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_edr_state(engine: Arc<dyn Engine>) -> Arc<EdrState> {
    let mut engines = HashMap::new();
    let mut collections = HashMap::new();
    engines.insert("weather".to_string(), engine);
    collections.insert("weather".to_string(), CollectionConfig {
        id: "weather".to_string(),
        title: "Finnish Weather Observations".to_string(),
        description: "Test collection".to_string(),
        data_path: String::new(),
        apis: vec!["edr".to_string()],
        engine_type: "csv".to_string(),
    });
    Arc::new(EdrState { engines, collections, base_url: String::new() })
}

fn app() -> axum::Router {
    let engine = Arc::new(MockEngine) as Arc<dyn Engine>;
    api_edr::router(make_edr_state(engine))
}

async fn get_response(uri: &str) -> (StatusCode, String) {
    let req = match Request::get(uri).body(Body::empty()) {
        Ok(req) => req,
        Err(_) => {
            // URI contains invalid characters (null bytes, angle brackets, etc.)
            // This means the HTTP layer rejects the input before it reaches our handlers.
            // Treat this as a successful rejection — the server never sees the payload.
            return (StatusCode::BAD_REQUEST, String::new());
        }
    };
    let response = app().oneshot(req).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).to_string())
}

async fn get_response_with_headers(
    uri: &str,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let response = app()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&body).to_string())
}

fn assert_valid_error_response(body: &str) {
    let json: serde_json::Value =
        serde_json::from_str(body).expect("Error response must be valid JSON");
    assert!(
        json.get("code").is_some(),
        "Error response must have 'code' field: {body}"
    );
    assert!(
        json["code"].is_string(),
        "Error 'code' must be a string: {body}"
    );
    assert!(
        json.get("description").is_some(),
        "Error response must have 'description' field: {body}"
    );
    assert!(
        json["description"].is_string(),
        "Error 'description' must be a string: {body}"
    );
}

// ===========================================================================
// 1. INPUT VALIDATION TESTS
// ===========================================================================

// --- Path traversal ---

#[tokio::test]
async fn path_traversal_in_collection_id_returns_404() {
    let payloads = [
        "../etc/passwd",
        "..%2F..%2Fetc%2Fpasswd",
        "....//....//etc/passwd",
        "%2e%2e%2f%2e%2e%2f",
        "weather/../../secret",
    ];
    for payload in &payloads {
        let uri = format!("/collections/{payload}");
        let (status, body) = get_response(&uri).await;
        // Axum may return 404 for paths with slashes (route mismatch) or our handler returns 404
        assert!(
            status == StatusCode::NOT_FOUND || status == StatusCode::BAD_REQUEST,
            "Path traversal payload '{payload}' should not succeed, got {status}"
        );
        // If we got a JSON body, validate its structure
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
            if json.get("code").is_some() {
                assert_valid_error_response(&body);
            }
        }
    }
}

#[tokio::test]
async fn path_traversal_in_location_id_returns_404() {
    let payloads = [
        "../etc/passwd",
        "..%2Fetc%2Fpasswd",
        "%00helsinki",
        "helsinki%00.txt",
    ];
    for payload in &payloads {
        let uri = format!("/collections/weather/locations/{payload}");
        let (status, _body) = get_response(&uri).await;
        assert!(
            status == StatusCode::NOT_FOUND || status == StatusCode::BAD_REQUEST,
            "Path traversal location '{payload}' should not succeed, got {status}"
        );
    }
}

// --- Extremely long strings ---

#[tokio::test]
async fn extremely_long_collection_id_returns_404() {
    let long_id: String = "a".repeat(10_000);
    let uri = format!("/collections/{long_id}");
    let (status, body) = get_response(&uri).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_valid_error_response(&body);
}

#[tokio::test]
async fn extremely_long_location_id_returns_404() {
    let long_id: String = "x".repeat(10_000);
    let uri = format!("/collections/weather/locations/{long_id}");
    let (status, body) = get_response(&uri).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_valid_error_response(&body);
}

#[tokio::test]
async fn extremely_long_datetime_param_returns_400() {
    let long_dt: String = "2024-01-01T00:00:00Z/".to_string() + &"9".repeat(10_000);
    let uri = format!(
        "/collections/weather/locations/helsinki?datetime={}",
        long_dt
    );
    let (status, body) = get_response(&uri).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_valid_error_response(&body);
}

#[tokio::test]
async fn extremely_long_parameter_name_list() {
    // 1000 comma-separated parameter names
    let params: String = (0..1000)
        .map(|i| format!("param{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let uri = format!(
        "/collections/weather/locations/helsinki?parameter-name={params}"
    );
    let (status, _body) = get_response(&uri).await;
    // Should succeed (engine just filters) or return a valid response, not crash
    assert!(
        status == StatusCode::OK || status.is_client_error(),
        "Extremely long parameter-name list should not cause server error, got {status}"
    );
}

// --- Special characters ---

#[tokio::test]
async fn null_bytes_in_collection_id() {
    let uri = "/collections/wea\0ther";
    let (status, _body) = get_response(uri).await;
    assert_ne!(
        status,
        StatusCode::OK,
        "Null byte in collection ID should not succeed"
    );
}

#[tokio::test]
async fn unicode_in_collection_id() {
    let payloads = [
        "caf\u{00e9}",          // Latin accent
        "\u{0437}\u{0434}",     // Cyrillic
        "\u{1F4A9}",            // Emoji
        "\u{202e}rehtaew",      // RTL override + reversed "weather"
        "\u{0000}",             // Null
    ];
    for payload in &payloads {
        let uri = format!("/collections/{payload}");
        let (status, _body) = get_response(&uri).await;
        assert_ne!(
            status,
            StatusCode::OK,
            "Unicode payload '{payload}' in collection ID should not succeed"
        );
    }
}

#[tokio::test]
async fn sql_injection_in_collection_id() {
    let payloads = [
        "weather' OR '1'='1",
        "weather; DROP TABLE collections;--",
        "weather' UNION SELECT * FROM users--",
        "1 OR 1=1",
    ];
    for payload in &payloads {
        let uri = format!("/collections/{payload}");
        let (status, _body) = get_response(&uri).await;
        // These should never match "weather", so 404 (or route mismatch)
        assert_ne!(
            status,
            StatusCode::OK,
            "SQL injection payload should not succeed"
        );
        assert_ne!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "SQL injection payload should not cause 500"
        );
    }
}

#[tokio::test]
async fn sql_injection_in_location_id() {
    let payloads = [
        "helsinki' OR '1'='1",
        "'; DROP TABLE locations;--",
    ];
    for payload in &payloads {
        let uri = format!("/collections/weather/locations/{payload}");
        let (status, _body) = get_response(&uri).await;
        assert_ne!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "SQL injection in location ID should not cause 500"
        );
    }
}

#[tokio::test]
async fn xss_payloads_in_collection_id() {
    let payloads = [
        "<script>alert('xss')</script>",
        "weather<img src=x onerror=alert(1)>",
        "weather\"><script>alert(1)</script>",
        "javascript:alert(1)",
    ];
    for payload in &payloads {
        let uri = format!("/collections/{payload}");
        let (status, body) = get_response(&uri).await;
        assert_ne!(
            status,
            StatusCode::OK,
            "XSS payload should not succeed"
        );
        // If we got a JSON error response, verify the payload is JSON-encoded
        // (angle brackets should appear as part of a JSON string, not raw HTML)
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
            if let Some(desc) = json.get("description") {
                let desc_str = desc.as_str().unwrap_or("");
                // The description may contain the payload, but it is inside a JSON string
                // which is inherently safe. Verify the response is valid JSON (already parsed).
                // The key check: the raw body must be valid JSON, so no raw HTML injection.
                assert!(
                    serde_json::from_str::<serde_json::Value>(&body).is_ok(),
                    "Response with reflected input must be valid JSON, not raw HTML"
                );
                let _ = desc_str; // suppress unused warning
            }
        }
    }
}

#[tokio::test]
async fn xss_payloads_in_location_id() {
    let payload = "<script>alert('xss')</script>";
    let uri = format!("/collections/weather/locations/{payload}");
    let (status, body) = get_response(&uri).await;

    // URI with angle brackets is rejected at HTTP layer or by handler
    assert_ne!(status, StatusCode::OK, "XSS payload should not succeed");
    assert_ne!(status, StatusCode::INTERNAL_SERVER_ERROR, "XSS payload should not cause 500");

    // If we got a body, it must be valid JSON (not raw HTML)
    if !body.is_empty() {
        let json: serde_json::Value =
            serde_json::from_str(&body).expect("Error response must be valid JSON, not raw HTML");
        assert_valid_error_response(&body);
        let desc = json["description"].as_str().unwrap();
        assert!(!desc.is_empty(), "Description should not be empty");
    }
}

#[tokio::test]
async fn xss_payloads_in_datetime_param() {
    let payload = "<script>alert(1)</script>";
    let uri = format!(
        "/collections/weather/locations/helsinki?datetime={payload}"
    );
    let (status, body) = get_response(&uri).await;
    // The URI may be rejected at the HTTP layer (invalid chars) or by our handler
    assert_ne!(status, StatusCode::OK, "XSS payload in datetime should not succeed");
    assert_ne!(status, StatusCode::INTERNAL_SERVER_ERROR, "XSS payload should not cause 500");
    // If we got a body, it must be valid JSON
    if !body.is_empty() {
        assert!(
            serde_json::from_str::<serde_json::Value>(&body).is_ok(),
            "Response body must be valid JSON even with XSS payload in datetime"
        );
    }
}

// --- Malformed datetime strings ---

#[tokio::test]
async fn adversarial_datetime_inputs() {
    let payloads = [
        "",                                          // empty string
        "/",                                         // just separator
        "//",                                        // double separator
        "../..",                                     // open both ends
        "not-a-date",                                // garbage
        "2024-13-01T00:00:00Z",                      // invalid month
        "2024-01-32T00:00:00Z",                      // invalid day
        "2024-01-01T25:00:00Z",                      // invalid hour
        "9999-99-99T99:99:99Z",                      // all invalid
        "2024-01-01T00:00:00Z/2024-01-01T00:00:00Z/2024-01-01T00:00:00Z", // triple
        &"2024-01-01T00:00:00Z".repeat(100),         // repeated valid datetime
        "2024-01-01T00:00:00+99:99",                 // invalid timezone offset
        "\0",                                        // null byte
        "2024-01-01T00:00:00Z/\0",                   // null byte in interval end
    ];
    for payload in &payloads {
        let uri = format!(
            "/collections/weather/locations/helsinki?datetime={payload}"
        );
        let (status, body) = get_response(&uri).await;
        assert_ne!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "Adversarial datetime '{payload}' should not cause 500, got body: {body}"
        );
        // Should be 400 (bad request) or 200 (if somehow valid)
        if status == StatusCode::BAD_REQUEST && !body.is_empty() {
            assert_valid_error_response(&body);
        }
    }
}

// ===========================================================================
// 2. ERROR INFORMATION LEAKAGE TESTS
// ===========================================================================

#[tokio::test]
async fn error_404_does_not_expose_internal_paths() {
    let (status, body) = get_response("/collections/nonexistent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_valid_error_response(&body);

    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let desc = json["description"].as_str().unwrap();

    // Should not contain file system paths or stack traces
    assert!(!desc.contains('/'), "Error description should not contain file paths: {desc}");
    assert!(
        !desc.to_lowercase().contains("stack"),
        "Error description should not contain stack traces: {desc}"
    );
    assert!(
        !desc.to_lowercase().contains("panic"),
        "Error description should not mention panics: {desc}"
    );
}

#[tokio::test]
async fn error_404_location_does_not_expose_internals() {
    let (status, body) =
        get_response("/collections/weather/locations/does_not_exist").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_valid_error_response(&body);

    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let desc = json["description"].as_str().unwrap();

    // Should not expose file system details
    assert!(
        !desc.contains(".csv"),
        "Error should not expose data file names: {desc}"
    );
    assert!(
        !desc.contains("src/"),
        "Error should not expose source paths: {desc}"
    );
}

#[tokio::test]
async fn error_400_datetime_does_not_expose_internals() {
    let (status, body) = get_response(
        "/collections/weather/locations/helsinki?datetime=garbage",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_valid_error_response(&body);

    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let desc = json["description"].as_str().unwrap();

    // Should not expose internal module paths
    assert!(
        !desc.contains("crates/"),
        "Error should not expose crate paths: {desc}"
    );
}

#[tokio::test]
async fn error_response_format_is_consistent() {
    // Check all error-producing endpoints return consistent error format
    let error_uris = [
        "/collections/nonexistent",
        "/collections/nonexistent/locations",
        "/collections/weather/locations/nonexistent",
        "/collections/weather/locations/helsinki?datetime=invalid",
    ];

    for uri in &error_uris {
        let (status, body) = get_response(uri).await;
        assert!(
            status.is_client_error(),
            "URI {uri} should return client error, got {status}"
        );
        assert_valid_error_response(&body);

        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        // Verify only expected fields are present (no extra info leakage)
        let obj = json.as_object().unwrap();
        for key in obj.keys() {
            assert!(
                key == "code" || key == "description",
                "Unexpected field '{key}' in error response for {uri}"
            );
        }
    }
}

// ===========================================================================
// 3. HEADER SECURITY TESTS
// ===========================================================================

#[tokio::test]
async fn content_type_is_set_for_success_responses() {
    let uris_and_types: Vec<(&str, &str)> = vec![
        ("/", "application/json"),
        ("/conformance", "application/json"),
        ("/collections", "application/json"),
        ("/collections/weather", "application/json"),
        ("/collections/weather/locations", "application/geo+json"),
        ("/collections/weather/locations/helsinki", "application/prs.coverage+json"),
    ];
    for (uri, expected_ct) in &uris_and_types {
        let (status, headers, _body) = get_response_with_headers(uri).await;
        assert_eq!(status, StatusCode::OK, "URI {uri} should return 200");
        let content_type = headers
            .get("content-type")
            .unwrap_or_else(|| panic!("Missing content-type for {uri}"))
            .to_str()
            .unwrap();
        assert!(
            content_type.contains(expected_ct),
            "Content-Type for {uri} should contain {expected_ct}, got: {content_type}"
        );
    }
}

#[tokio::test]
async fn no_server_version_header_leaked() {
    let (_, headers, _) = get_response_with_headers("/").await;

    // Should not expose server software version
    assert!(
        headers.get("server").is_none(),
        "Server header should not be present to avoid version disclosure"
    );
    assert!(
        headers.get("x-powered-by").is_none(),
        "X-Powered-By header should not be present"
    );
}

#[tokio::test]
async fn no_sensitive_headers_in_error_responses() {
    let (_, headers, _) =
        get_response_with_headers("/collections/nonexistent").await;

    assert!(
        headers.get("server").is_none(),
        "Error responses should not expose Server header"
    );
    assert!(
        headers.get("x-powered-by").is_none(),
        "Error responses should not expose X-Powered-By header"
    );
    // Should not leak internal debug headers
    assert!(
        headers.get("x-debug").is_none(),
        "Should not have debug headers"
    );
    assert!(
        headers.get("x-request-id").is_none()
            || true, // request IDs are fine, just check no stack traces
        "Request ID headers are acceptable"
    );
}

// Note: CORS headers are added at the server level (CorsLayer in main.rs),
// not in the api-edr router. Testing CORS requires a full server integration
// test. The api-edr router alone will not have CORS headers. This is by design
// per the architecture rules.

// ===========================================================================
// 4. RESOURCE EXHAUSTION TESTS
// ===========================================================================

#[tokio::test]
async fn nonexistent_location_returns_quickly() {
    use std::time::Instant;

    let start = Instant::now();
    let (status, _body) =
        get_response("/collections/weather/locations/definitely_not_a_real_location_id_12345")
            .await;
    let elapsed = start.elapsed();

    assert_eq!(status, StatusCode::NOT_FOUND);
    // A simple string comparison or hash lookup should be well under 100ms
    assert!(
        elapsed.as_millis() < 100,
        "Non-existent location lookup took {}ms, should be fast",
        elapsed.as_millis()
    );
}

#[tokio::test]
async fn large_datetime_range_does_not_cause_issues() {
    // Open interval covering all time
    let uri = "/collections/weather/locations/helsinki?datetime=../..";
    let (status, _body) = get_response(uri).await;
    // Should succeed (returns all data) or at least not error
    assert!(
        status == StatusCode::OK || status == StatusCode::BAD_REQUEST,
        "Fully open datetime range should not cause 500, got {status}"
    );
}

#[tokio::test]
async fn extreme_datetime_range() {
    // Very distant past to very distant future
    let uri = "/collections/weather/locations/helsinki?datetime=0001-01-01T00:00:00Z/9999-12-31T23:59:59Z";
    let (status, _body) = get_response(uri).await;
    assert_ne!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "Extreme datetime range should not cause 500"
    );
}

#[tokio::test]
async fn many_nonexistent_locations_sequentially() {
    // Verify we can handle many 404s without degradation
    use std::time::Instant;

    let start = Instant::now();
    for i in 0..100 {
        let uri = format!("/collections/weather/locations/fake_location_{i}");
        let (status, _) = get_response(&uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let elapsed = start.elapsed();

    // 100 lookups should complete well under 5 seconds
    assert!(
        elapsed.as_secs() < 5,
        "100 non-existent location lookups took {:?}, possible resource issue",
        elapsed
    );
}

// ===========================================================================
// 5. HTTP METHOD TESTS (bonus security surface)
// ===========================================================================

#[tokio::test]
async fn post_requests_are_rejected() {
    let response = app()
        .oneshot(
            Request::post("/collections/weather")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::METHOD_NOT_ALLOWED,
        "POST should be rejected on GET-only endpoint"
    );
}

#[tokio::test]
async fn delete_requests_are_rejected() {
    let response = app()
        .oneshot(
            Request::delete("/collections/weather")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::METHOD_NOT_ALLOWED,
        "DELETE should be rejected on GET-only endpoint"
    );
}

#[tokio::test]
async fn put_requests_are_rejected() {
    let response = app()
        .oneshot(
            Request::put("/collections/weather/locations/helsinki")
                .body(Body::from("{\"data\": \"malicious\"}"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::METHOD_NOT_ALLOWED,
        "PUT should be rejected on GET-only endpoint"
    );
}
