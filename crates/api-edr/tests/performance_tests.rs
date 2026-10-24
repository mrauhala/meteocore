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
// 1. COMPLETE INVENTORY on /collections/{id}/locations.
//    Without `limit` the list is the whole inventory. Direct serialization
//    avoids a second JSON tree and stays within bounded query execution
//    (#533), but the final response bytes are capped per response and
//    admitted process-wide. EDR 1.2 `limit` pages it under the same caps (#922).
//
// 2. FULL IN-MEMORY RESPONSE CONSTRUCTION.
//    Every response body is built in memory, then sent. CoverageJSON range
//    values and locations are serialised directly, without a JSON tree,
//    and every body is admitted process-wide by the EDR response memory
//    budget until it is delivered or dropped (#1041).
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
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        let locations = (0..self.location_count)
            .map(|i| Location {
                id: format!("loc_{i}"),
                label: format!("Location {i}"),
                latitude: 60.0 + (i as f64 * 0.01),
                longitude: 24.0 + (i as f64 * 0.01),
            })
            .collect();
        Ok(Arc::new(LocationSnapshot::new(locations)))
    }

    fn query_location(
        &self,
        location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
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
                    standard_name: None,
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
            zarr: None,
            odim: None,
            cap: None,
            postgis: None,
            nowcast: None,
            station_grid: None,
            bufr: None,
            satellite: None,
            netcdf: None,
            csv: None,
            preview: None,
            derive_wind: None,
            derive_geopotential_height: None,
        },
    );
    Arc::new(ArcSwap::from_pointee(EdrState {
        engines,
        feature_engines: HashMap::new(),
        collections,
        styles: HashMap::new(),
        base_url: String::new(),
        trust_proxy_headers: false,
    }))
}

fn build_app(engine: ScalableEngine) -> axum::Router {
    let engine: Arc<dyn EdrEngine> = Arc::new(engine);
    api_edr::router(make_edr_state(engine))
}

struct WorkerMetadataEngine {
    request_thread: std::thread::ThreadId,
}

impl EdrEngine for WorkerMetadataEngine {
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        Ok(Arc::new(LocationSnapshot::new(vec![Location {
            id: "one".into(),
            label: "Station one".into(),
            latitude: 60.0,
            longitude: 24.0,
        }])))
    }

    fn query_location(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        unreachable!("inventory requests do not query observations")
    }

    fn get_parameters(&self) -> Vec<String> {
        assert_ne!(std::thread::current().id(), self.request_thread);
        vec!["temperature".into()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        assert_ne!(std::thread::current().id(), self.request_thread);
        None
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        None
    }
}

#[tokio::test(flavor = "current_thread")]
async fn locations_metadata_runs_off_http_worker_and_etag_matches_response() {
    let app = api_edr::router(make_edr_state(Arc::new(WorkerMetadataEngine {
        request_thread: std::thread::current().id(),
    })));
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/collections/weather/locations")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let etag = response.headers()["etag"].to_str().unwrap().to_owned();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    assert_eq!(etag, ds_core::http_cache::etag_of(&bytes));
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        body["features"][0]["properties"]["parameter-name"][0],
        "temperature"
    );
    assert_eq!(body["features"][0]["properties"]["datetime"], "");
    let cached = app
        .oneshot(
            Request::builder()
                .uri("/collections/weather/locations")
                .header("if-none-match", etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
    assert!(axum::body::to_bytes(cached.into_body(), 1_000_000)
        .await
        .unwrap()
        .is_empty());
}

// Run with isolated environment settings: the production budget is process-wide.
// `/locations` shares the EDR response budget (#1041);
// `MC_EDR_LOCATIONS_MEMORY_MB`, its own budget before, is ignored.
#[test]
fn locations_budget_errors_are_complete_json_and_memory_recovers() {
    const CHILD: &str = "MC_TEST_LOCATIONS_BUDGET";
    let Ok(mode) = std::env::var(CHILD) else {
        for mode in ["size", "memory", "retired"] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "locations_budget_errors_are_complete_json_and_memory_recovers",
                    "--nocapture",
                ])
                .env(CHILD, mode)
                .env(
                    "MC_EDR_LOCATIONS_MAX_BYTES",
                    if mode == "size" { "128" } else { "1048576" },
                );
            if mode == "retired" {
                child
                    .env("MC_EDR_LOCATIONS_MEMORY_MB", "1")
                    .env_remove("MC_EDR_RESPONSE_MEMORY_MB");
            } else {
                child.env("MC_EDR_RESPONSE_MEMORY_MB", "1");
            }
            assert!(child.status().unwrap().success(), "{mode} child failed");
        }
        return;
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let app = build_app(ScalableEngine {
                location_count: 100,
                timestep_count: 1,
                parameter_count: 1,
            });
            let request = || {
                Request::builder()
                    .uri("/collections/weather/locations")
                    .body(Body::empty())
                    .unwrap()
            };
            let mut held = Vec::new();
            // Each body occupies at least 64 KiB; holding 16 exhausts a 1 MiB pool.
            let expected = if mode == "size" {
                "ResponseLimit"
            } else {
                "ServerBusy"
            };
            for _ in 0..=16 {
                let response = app.clone().oneshot(request()).await.unwrap();
                if response.status() == StatusCode::OK {
                    assert_ne!(mode, "size");
                    held.push(response);
                } else {
                    assert_ne!(mode, "retired", "the retired variable limits nothing");
                    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
                    // Busy is worth retrying; over the per-response cap is not.
                    assert_eq!(
                        response.headers().get("retry-after").is_some(),
                        mode == "memory"
                    );
                    let bytes = axum::body::to_bytes(response.into_body(), 4096)
                        .await
                        .unwrap();
                    let error: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(error["code"], expected);
                    assert!(
                        error.get("features").is_none(),
                        "never send a truncated inventory"
                    );
                    if mode == "memory" {
                        assert_eq!(held.len(), 16);
                        drop(held);
                        let recovered = app.oneshot(request()).await.unwrap();
                        assert_eq!(recovered.status(), StatusCode::OK);
                        let bytes = axum::body::to_bytes(recovered.into_body(), 1048576)
                            .await
                            .unwrap();
                        let inventory: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                        assert_eq!(inventory["features"].as_array().unwrap().len(), 100);
                    }
                    return;
                }
            }
            assert_eq!(mode, "retired", "budget never rejected a response");
            assert_eq!(held.len(), 17);
            let (used, capacity, _) = api_edr::response_budget_metrics();
            assert_eq!(capacity, 512 << 20, "MC_EDR_RESPONSE_MEMORY_MB's default");
            assert!(used >= 17 * 64 * 1024, "{used}");
            drop(held);
            assert_eq!(api_edr::response_budget_metrics().0, 0);
        });
}

// A page is admitted by the same byte budget as the complete inventory: one
// within it is served when the whole inventory is not (#922).
#[test]
fn locations_pages_share_the_inventory_byte_budget() {
    const CHILD: &str = "MC_TEST_LOCATIONS_PAGE_BUDGET";
    if std::env::var(CHILD).is_err() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "locations_pages_share_the_inventory_byte_budget",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("MC_EDR_LOCATIONS_MAX_BYTES", "4096")
            .status()
            .unwrap();
        assert!(status.success(), "child failed");
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let app = build_app(ScalableEngine {
                location_count: 100,
                timestep_count: 1,
                parameter_count: 1,
            });
            let get = |uri: &'static str| {
                let app = app.clone();
                async move {
                    let response = app
                        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                        .await
                        .unwrap();
                    let status = response.status();
                    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                        .await
                        .unwrap();
                    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    (status, json)
                }
            };
            for (uri, hint) in [
                (
                    "/collections/weather/locations",
                    "Complete location inventory exceeds the configured response limit; page it with limit",
                ),
                // Filtered, not the complete inventory (#961).
                (
                    "/collections/weather/locations?bbox=-180,-90,180,90",
                    "Filtered location list exceeds the configured response limit; page it with limit",
                ),
                (
                    "/collections/weather/locations?limit=100",
                    "request a smaller limit",
                ),
            ] {
                let (status, error) = get(uri).await;
                assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
                assert_eq!(error["code"], "ResponseLimit", "{uri}");
                let description = error["description"].as_str().unwrap();
                assert!(description.contains(hint), "{uri}: {description}");
            }
            let (status, page) = get("/collections/weather/locations?limit=2&offset=98").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(page["numberMatched"], 100);
            assert_eq!(page["numberReturned"], 2);
            assert_eq!(page["features"][1]["id"], "loc_99");
        });
}

/// Serves `/locations/big`, one million values as long decimals (about
/// 18 MB of CoverageJSON), and `/locations/small`, ten.
struct MillionValuesEngine;

impl EdrEngine for MillionValuesEngine {
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        Ok(Arc::new(LocationSnapshot::new(
            ["big", "small"]
                .map(|id| Location {
                    id: id.into(),
                    label: id.into(),
                    latitude: 60.0,
                    longitude: 24.0,
                })
                .into(),
        )))
    }

    fn query_location(
        &self,
        location_id: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let (steps, parameters) = match location_id {
            "big" => (10_000, 100),
            "small" => (10, 1),
            _ => return Err(DataServerError::LocationNotFound(location_id.into())),
        };
        let base: DateTime<Utc> = "2024-01-01T00:00:00Z".parse().unwrap();
        let mut result = QueryResult {
            domain: DomainDescription::PointSeries {
                x: 24.0,
                y: 60.0,
                t: (0..steps)
                    .map(|h| base + chrono::Duration::hours(h))
                    .collect(),
                z: None,
            },
            parameters: HashMap::new(),
            ranges: HashMap::new(),
        };
        for p in 0..parameters {
            let name = format!("param_{p}");
            result.parameters.insert(
                name.clone(),
                ParameterDescription {
                    label: name.clone(),
                    unit: "K".into(),
                    observed_property: name.clone(),
                    standard_name: None,
                },
            );
            result.ranges.insert(
                name,
                NdArray {
                    shape: vec![steps as usize],
                    axis_names: vec!["t".into()],
                    values: (0..steps)
                        .map(|i| Some((p * steps + i) as f64 / 7.0))
                        .collect(),
                },
            );
        }
        Ok(CoverageResponse::Single(result))
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["param_0".into()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        None
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([24.0, 60.0, 24.0, 60.0])
    }
}

/// Runs `test` in a child process with `env`: the response budget is
/// process-wide and read once.
fn in_child(test: &str, env: &[(&str, &str)], body: impl FnOnce()) {
    const CHILD: &str = "MC_TEST_RESPONSE_BUDGET_CHILD";
    if std::env::var(CHILD).is_ok() {
        return body();
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(CHILD, "1")
        .envs(env.iter().copied())
        .status()
        .unwrap();
    assert!(status.success(), "{test} child failed");
}

/// With an EDR response budget smaller than one million-value response,
/// its CoverageJSON and its HTML page are both a 503 to retry, the HTML
/// before it renders; a small response is still served, and every
/// reservation is released once its body drops (#1041).
#[test]
fn large_bodies_over_the_response_budget_are_503() {
    in_child(
        "large_bodies_over_the_response_budget_are_503",
        &[("MC_EDR_RESPONSE_MEMORY_MB", "4")],
        || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let app = api_edr::router(make_edr_state(Arc::new(MillionValuesEngine)));
                    let get = |uri: &'static str| {
                        let app = app.clone();
                        async move {
                            app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                                .await
                                .unwrap()
                        }
                    };
                    for uri in [
                        "/collections/weather/locations/big",
                        "/collections/weather/locations/big?f=html",
                    ] {
                        let response = get(uri).await;
                        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{uri}");
                        assert_eq!(response.headers()["retry-after"], "1", "{uri}");
                        if !uri.ends_with("html") {
                            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                                .await
                                .unwrap();
                            let error: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                            assert_eq!(error["code"], "ServerBusy");
                        }
                        assert_eq!(api_edr::response_budget_metrics().0, 0, "{uri}");
                    }
                    let (_, capacity, rejected) = api_edr::response_budget_metrics();
                    assert_eq!(capacity, 4 << 20);
                    assert_eq!(rejected, 2);
                    for uri in [
                        "/collections/weather/locations/small",
                        "/collections/weather/locations/small?f=html",
                    ] {
                        let response = get(uri).await;
                        assert_eq!(response.status(), StatusCode::OK, "{uri}");
                        assert!(api_edr::response_budget_metrics().0 > 0, "{uri}");
                        drop(response);
                        assert_eq!(api_edr::response_budget_metrics().0, 0, "{uri}");
                    }
                });
        },
    );
}

/// A body being sent stays charged to the response budget, and a client
/// that disconnects mid-body releases it (#1041).
#[test]
fn a_disconnected_client_releases_its_response_bytes() {
    in_child(
        "a_disconnected_client_releases_its_response_bytes",
        &[],
        || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            let app = api_edr::router(make_edr_state(Arc::new(MillionValuesEngine)));
            let listener = runtime
                .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
                .unwrap();
            let address = listener.local_addr().unwrap();
            runtime.spawn(async move { axum::serve(listener, app).await.unwrap() });

            use std::io::{Read, Write};
            let mut client = std::net::TcpStream::connect(address).unwrap();
            client
                .set_read_timeout(Some(std::time::Duration::from_secs(60)))
                .unwrap();
            client
                .write_all(
                    b"GET /collections/weather/locations/big HTTP/1.1\r\nHost: localhost\r\n\r\n",
                )
                .unwrap();
            // Read the head and the first bytes of the body, then stop.
            let mut received = Vec::new();
            let mut buffer = [0u8; 16 * 1024];
            let head = loop {
                let n = client.read(&mut buffer).unwrap();
                assert!(n > 0, "connection closed before the head");
                received.extend_from_slice(&buffer[..n]);
                if let Some(end) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                    break String::from_utf8_lossy(&received[..end]).into_owned();
                }
            };
            assert!(head.starts_with("HTTP/1.1 200"), "{head}");
            let length: u64 = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(str::to_owned)
                })
                .expect("a buffered body has a length")
                .trim()
                .parse()
                .unwrap();
            assert!(
                length > 16 << 20,
                "{length}: larger than the socket buffers"
            );
            let (used, _, _) = api_edr::response_budget_metrics();
            assert!(
                used >= length,
                "the body in flight is charged: {used} < {length}"
            );

            drop(client);
            let released = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while api_edr::response_budget_metrics().0 != 0 {
                assert!(
                    std::time::Instant::now() < released,
                    "still charged after the client left: {:?}",
                    api_edr::response_budget_metrics()
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        },
    );
}

// ===========================================================================
// 1. Response Size Tests
// ===========================================================================

/// Verify that the locations endpoint can handle a large number of locations
/// without panicking or producing malformed JSON.
///
/// This exercises the direct locations serializer with 10,000 features.
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
// 2. EDR 1.1 Complete Locations Inventory
// ===========================================================================

/// Preserve the EDR 1.1 complete-inventory contract while optimizing response
/// construction. Optional EDR 1.2 paging is a separate API contract change.
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

    // No continuation is needed: every location was returned.
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
/// Content-Type: application/vnd.cov+json (EDR 1.2, #920).
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
        content_type == "application/vnd.cov+json",
        "expected application/vnd.cov+json, got: {content_type}"
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
