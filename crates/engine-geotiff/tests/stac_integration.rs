//! Integration tests for STAC support in engine-geotiff.

use ds_core::config::GeoTiffConfig;
use ds_core::error::DataServerError;
use engine_geotiff::stac::StacClient;
use engine_geotiff::GeoTiffEngine;

/// Helper to build a GeoTiffConfig with required fields and sensible defaults.
fn base_config() -> GeoTiffConfig {
    GeoTiffConfig {
        filename_template: None,
        filename_pattern: None,
        timestamp_format: None,
        parameter: "reflectivity".to_string(),
        unit: "dBZ".to_string(),
        poll_interval_secs: 30,
        exclude_patterns: vec![],
        max_files: None,
        tile_cache_mb: 0,
        band: 1,
        nodata: None,
        scale: None,
        offset: None,
        stac_url: None,
        stac_asset_key: "data".to_string(),
        stac_asset_allowlist: None,
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        time_window: None,
        scan_days: None,
    }
}

/// Extract the error from a GeoTiffEngine::new result.
/// Panics if the result is Ok (GeoTiffEngine doesn't impl Debug, so unwrap_err won't work).
fn expect_err(result: Result<GeoTiffEngine, DataServerError>) -> String {
    match result {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected error, got Ok"),
    }
}

// =========================================================================
// MET Norway live tests (require network — run with `cargo test -- --ignored`)
// =========================================================================

#[test]
#[ignore]
fn met_norway_stac_fetch_items() {
    let client = StacClient::new(
        "https://radar-stacapi.met.no/v1/collections/Mosaic-Norway-v1/items",
        "data",
        vec!["https://thredds.met.no/".to_string()],
    )
    .expect("StacClient creation should succeed");

    let items = client
        .fetch_items(None, Some(3))
        .expect("fetch_items should succeed");

    assert!(!items.is_empty(), "should return at least one item");
    assert!(items.len() <= 3, "should respect max_items limit");

    for item in &items {
        assert!(item.datetime.timestamp() > 0, "datetime should be positive");

        assert!(
            item.asset_url.starts_with("https://thredds.met.no/"),
            "asset URL '{}' should start with https://thredds.met.no/",
            item.asset_url
        );

        assert!(!item.item_id.is_empty(), "item_id should not be empty");
    }
}

#[test]
#[ignore]
fn met_norway_asset_urls_accessible() {
    let client = StacClient::new(
        "https://radar-stacapi.met.no/v1/collections/Mosaic-Norway-v1/items",
        "data",
        vec!["https://thredds.met.no/".to_string()],
    )
    .expect("StacClient creation should succeed");

    let items = client
        .fetch_items(None, Some(1))
        .expect("fetch_items should succeed");

    assert!(!items.is_empty(), "need at least one item to test");

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();

    let resp = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(http.head(&items[0].asset_url).send())
    })
    .expect("HEAD request should succeed");

    assert_eq!(
        resp.status().as_u16(),
        200,
        "asset URL should be accessible: {}",
        items[0].asset_url
    );
}

// =========================================================================
// Config validation tests
// =========================================================================

#[test]
fn stac_url_with_data_path_is_error() {
    let mut config = base_config();
    config.stac_url = Some("https://api.example.com/collections/radar/items".to_string());
    config.stac_asset_allowlist = Some(vec!["https://example.com/".to_string()]);

    let err = expect_err(GeoTiffEngine::new("test", Some("/some/path"), &config));
    assert!(
        err.contains("mutually exclusive"),
        "expected mutually exclusive error, got: {}",
        err
    );
}

#[test]
fn stac_url_with_endpoint_is_error() {
    let mut config = base_config();
    config.stac_url = Some("https://api.example.com/collections/radar/items".to_string());
    config.stac_asset_allowlist = Some(vec!["https://example.com/".to_string()]);
    config.endpoint = Some("https://s3.example.com".to_string());
    config.bucket = Some("my-bucket".to_string());

    let err = expect_err(GeoTiffEngine::new("test", None, &config));
    assert!(
        err.contains("mutually exclusive"),
        "expected mutually exclusive error, got: {}",
        err
    );
}

#[test]
fn stac_url_without_allowlist_is_error() {
    let mut config = base_config();
    config.stac_url = Some("https://api.example.com/collections/radar/items".to_string());
    config.stac_asset_allowlist = None;

    let err = expect_err(GeoTiffEngine::new("test", None, &config));
    assert!(
        err.contains("stac_asset_allowlist"),
        "expected allowlist error, got: {}",
        err
    );
}

#[test]
fn stac_url_with_empty_allowlist_is_error() {
    let mut config = base_config();
    config.stac_url = Some("https://api.example.com/collections/radar/items".to_string());
    config.stac_asset_allowlist = Some(vec![]);

    let err = expect_err(GeoTiffEngine::new("test", None, &config));
    assert!(
        err.contains("must not be empty"),
        "expected empty allowlist error, got: {}",
        err
    );
}

#[test]
fn valid_stac_config_no_filename_template() {
    // Valid STAC config should not require filename_template.
    // It will fail on the actual STAC fetch (network error), not on config validation.
    let mut config = base_config();
    config.stac_url = Some("https://api.example.com/collections/radar/items".to_string());
    config.stac_asset_allowlist = Some(vec!["https://example.com/".to_string()]);

    let err = expect_err(GeoTiffEngine::new("test", None, &config));
    // Should NOT be a config validation error — should be a network/fetch error
    assert!(
        !err.contains("mutually exclusive")
            && !err.contains("stac_asset_allowlist")
            && !err.contains("filename_template"),
        "config validation should pass; got config error: {}",
        err
    );
}

// =========================================================================
// StacClient error handling tests
// =========================================================================

#[test]
fn stac_client_invalid_url_scheme() {
    let result = StacClient::new(
        "ftp://example.com/items",
        "data",
        vec!["https://example.com/".to_string()],
    );
    assert!(result.is_err(), "ftp:// scheme should be rejected");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("http or https"),
        "error should mention http/https, got: {}",
        err
    );
}

#[test]
fn stac_client_invalid_url_format() {
    let result = StacClient::new(
        "not-a-url",
        "data",
        vec!["https://example.com/".to_string()],
    );
    assert!(result.is_err(), "invalid URL should be rejected");
}

#[test]
fn stac_client_fetch_from_nonexistent_endpoint() {
    let client = StacClient::new(
        "http://127.0.0.1:1/nonexistent",
        "data",
        vec!["http://127.0.0.1/".to_string()],
    )
    .expect("client creation should succeed even with unreachable URL");

    let result = client.fetch_items(None, Some(1));
    assert!(
        result.is_err(),
        "fetch from unreachable endpoint should fail"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("STAC request failed"),
        "error should mention request failure, got: {}",
        err
    );
}
