//! OGC API - EDR 1.1 Spec Compliance Tests
//!
//! This file documents every deviation from the OGC API - EDR 1.1 specification
//! found in the current implementation. Each test either:
//! - PASSES and validates current compliance, or
//! - FAILS (marked with `#[should_panic]` or commented out) documenting a gap
//!
//! Reference: OGC API - Environmental Data Retrieval Standard 1.1
//! https://docs.ogc.org/is/19-086r6/19-086r6.html

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::util::ServiceExt;

use api_edr::handlers::EdrState;
use ds_core::config::CollectionConfig;
use ds_core::engine::Engine;
use ds_core::error::DataServerError;
use ds_core::model::*;

// ---------------------------------------------------------------------------
// Mock engine for integration tests
// ---------------------------------------------------------------------------
struct MockEngine;

impl Engine for MockEngine {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(vec![
            Location {
                id: "station1".to_string(),
                label: "Helsinki".to_string(),
                latitude: 60.1699,
                longitude: 24.9384,
            },
            Location {
                id: "station2".to_string(),
                label: "Tampere".to_string(),
                latitude: 61.4978,
                longitude: 23.7610,
            },
        ])
    }

    fn query_location(
        &self,
        location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
    ) -> Result<QueryResult, DataServerError> {
        if location_id != "station1" && location_id != "station2" {
            return Err(DataServerError::LocationNotFound(location_id.to_string()));
        }
        let times: Vec<DateTime<Utc>> = (0..3)
            .map(|h| {
                format!("2024-01-01T{h:02}:00:00Z")
                    .parse()
                    .unwrap()
            })
            .collect();
        let mut parameters = HashMap::new();
        parameters.insert(
            "temperature".to_string(),
            ParameterDescription {
                label: "temperature".to_string(),
                unit: "degC".to_string(),
                observed_property: "temperature".to_string(),
            },
        );
        let mut ranges = HashMap::new();
        ranges.insert(
            "temperature".to_string(),
            NdArray {
                shape: vec![3],
                axis_names: vec!["t".to_string()],
                values: vec![Some(-2.5), Some(-2.8), Some(-3.1)],
            },
        );
        Ok(QueryResult {
            domain: DomainDescription::PointSeries {
                x: 24.9384,
                y: 60.1699,
                t: times,
            },
            parameters,
            ranges,
        })
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["temperature".to_string(), "humidity".to_string()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((
            "2024-01-01T00:00:00Z".parse().unwrap(),
            "2024-01-31T23:00:00Z".parse().unwrap(),
        ))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([19.0, 59.0, 32.0, 71.0])
    }
}

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
        geotiff: None,
    });
    Arc::new(EdrState { engines, collections, base_url: String::new() })
}

fn app() -> axum::Router {
    let engine: Arc<dyn Engine> = Arc::new(MockEngine);
    api_edr::router(make_edr_state(engine))
}

async fn get_json(uri: &str) -> (StatusCode, Value) {
    let app = app();
    let req = match Request::builder().uri(uri).body(Body::empty()) {
        Ok(req) => req,
        Err(_) => {
            // URI contains invalid characters — HTTP layer rejects before reaching handler
            return (StatusCode::BAD_REQUEST, Value::Null);
        }
    };
    let response = app.oneshot(req).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    if body.is_empty() {
        return (status, Value::Null);
    }
    let json: Value = serde_json::from_slice(&body).unwrap();
    (status, json)
}

// ===========================================================================
// FINDING 1: Collection metadata missing required "crs" field
// ===========================================================================
// Spec: OGC EDR 1.1 Section 8.2.1 / collection response schema requires
//   "crs" as a list of supported CRS identifiers.
// Implementation: build_collection_metadata() does not include "crs" field.
// Impact: Clients cannot discover which CRS the server supports.
// Fix: Add `"crs": ["CRS84"]` (or the full URI
//   "http://www.opengis.net/def/crs/OGC/1.3/CRS84") to the collection JSON.

#[tokio::test]
async fn finding_01_collection_has_crs_field() {
    let (_status, json) = get_json("/collections/weather").await;
    assert!(
        json.get("crs").is_some(),
        "collection must have 'crs' field (required by EDR 1.1 spec)"
    );
    let crs = json["crs"].as_array().unwrap();
    assert!(!crs.is_empty());
    assert!(crs[0].as_str().unwrap().starts_with("http"));
}

// ===========================================================================
// FINDING 2: extent.spatial.crs should be a full URI, not bare "CRS84"
// ===========================================================================
// Spec: The CRS identifier in extent.spatial should be a full OGC URI:
//   "http://www.opengis.net/def/crs/OGC/1.3/CRS84"
// Implementation: uses bare string "CRS84"
// Impact: Interoperability - clients may not recognize the abbreviated form.
// Fix: Change "crs": "CRS84" to
//   "crs": "http://www.opengis.net/def/crs/OGC/1.3/CRS84"

#[tokio::test]
async fn finding_02_spatial_extent_crs_is_uri() {
    let (_status, json) = get_json("/collections/weather").await;
    let crs = json["extent"]["spatial"]["crs"].as_str().unwrap();
    assert!(
        crs.starts_with("http"),
        "spatial CRS should be a full OGC URI, got: {crs}"
    );
}

// ===========================================================================
// FINDING 3: Landing page links use relative URLs
// ===========================================================================
// Spec: OGC API Common Section 7.2 - link href values SHOULD be absolute URIs
//   per RFC 8288. While relative URIs are technically allowed, spec examples
//   and conformance tests generally expect absolute URIs.
// Implementation: All hrefs are relative paths like "/edr/", "/edr/conformance"
// Impact: Clients that don't resolve relative URIs will break.
// Fix: Either construct absolute URIs from the Host header / config, or
//   accept this as a known limitation. (Many OGC servers use relative URIs.)

#[tokio::test]
async fn finding_03_landing_page_links_are_relative() {
    let (_status, json) = get_json("/").await;
    let links = json["links"].as_array().unwrap();
    let mut all_absolute = true;
    for link in links {
        let href = link["href"].as_str().unwrap();
        if !href.starts_with("http") {
            all_absolute = false;
        }
    }
    // This documents the gap - links are relative, not absolute.
    // Not a hard failure, but a compliance note.
    assert!(
        !all_absolute,
        "Expected relative links (documenting current behavior)"
    );
}

// ===========================================================================
// FINDING 4: Landing page missing required "api" link
// ===========================================================================
// Spec: EDR 1.1 landing page SHOULD include a link with rel="service-desc"
//   pointing to the API definition (e.g., OpenAPI document).
// Implementation: No rel="service-desc" or rel="service-doc" link present.
// Impact: Clients cannot discover the API description document.
// Fix: Add link { "href": "/edr/api", "rel": "service-desc",
//        "type": "application/vnd.oai.openapi+json;version=3.0" }

#[tokio::test]
async fn finding_04_landing_page_missing_service_desc_link() {
    let (_status, json) = get_json("/").await;
    let links = json["links"].as_array().unwrap();
    let has_service_desc = links
        .iter()
        .any(|l| l["rel"].as_str() == Some("service-desc"));
    // SPEC GAP: No service-desc link
    assert!(
        !has_service_desc,
        "Documenting: no service-desc link exists (should be added)"
    );
}

// ===========================================================================
// FINDING 5: Collections endpoint ignores bbox query parameter
// ===========================================================================
// Spec: GET /collections supports optional query parameters: bbox, datetime, f
// Implementation: collections() handler takes no query parameters at all.
// Impact: Clients filtering collections by spatial/temporal extent get no filtering.
// Fix: Add optional bbox/datetime/f query params to collections handler,
//   filter the collection list accordingly.

#[tokio::test]
async fn finding_05_collections_ignores_bbox_param() {
    // Passing bbox should not cause an error (it should be accepted gracefully).
    // Currently the param is simply ignored.
    let (status, _json) = get_json("/collections?bbox=20,60,30,70").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "bbox param should be accepted (even if not yet filtering)"
    );
}

// ===========================================================================
// FINDING 6: No "f" (format) query parameter support on any endpoint
// ===========================================================================
// Spec: All endpoints should support `f` query parameter for content
//   negotiation (e.g., f=json, f=html).
// Implementation: No endpoint accepts or processes the `f` parameter.
// Impact: Clients requesting specific output format via query param get
//   no format negotiation.
// Fix: Add `f` parameter support to all handlers, or at minimum accept
//   and ignore it without returning an error.

#[tokio::test]
async fn finding_06_landing_page_accepts_f_param() {
    let (status, _json) = get_json("/?f=json").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "f=json param should be accepted on landing page"
    );
}

#[tokio::test]
async fn finding_06b_conformance_accepts_f_param() {
    let (status, _json) = get_json("/conformance?f=json").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "f=json param should be accepted on conformance"
    );
}

// ===========================================================================
// FINDING 7: Content-Type header not explicitly set
// ===========================================================================
// Spec: Responses should include appropriate Content-Type headers:
//   - application/json for JSON
//   - application/geo+json for GeoJSON (locations)
//   - application/prs.coverage+json for CoverageJSON
// Implementation: axum's Json() extractor sets application/json for ALL
//   responses, even GeoJSON and CoverageJSON.
// Impact: Clients expecting proper media type detection fail.
// Fix: Use custom response types with explicit Content-Type headers for
//   GeoJSON and CoverageJSON responses.

#[tokio::test]
async fn finding_07_locations_returns_geojson_content_type() {
    let app = app();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/collections/weather/locations")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    assert!(
        content_type.contains("application/geo+json"),
        "Locations response should have Content-Type: application/geo+json, got: {content_type}"
    );
}

#[tokio::test]
async fn finding_07b_coverage_returns_covjson_content_type() {
    let app = app();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/collections/weather/locations/station1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    assert!(
        content_type.contains("application/prs.coverage+json"),
        "CoverageJSON response should have Content-Type: application/prs.coverage+json, got: {content_type}"
    );
}

// ===========================================================================
// FINDING 8: Locations response (GeoJSON) missing required links
// ===========================================================================
// Spec: GeoJSON FeatureCollection responses from EDR should include "links"
//   array with at least a "self" link and links to individual location queries.
// Implementation: locations_to_geojson() produces bare FeatureCollection
//   with no "links" array.
// Impact: Clients cannot navigate to individual location data queries.
// Fix: Add "links" to FeatureCollection and to each Feature.

#[tokio::test]
async fn finding_08_locations_geojson_has_links() {
    let (_status, json) = get_json("/collections/weather/locations").await;
    assert!(
        json.get("links").is_some(),
        "GeoJSON FeatureCollection must have 'links' for EDR compliance"
    );
    let links = json["links"].as_array().unwrap();
    assert!(!links.is_empty());
}

// ===========================================================================
// FINDING 9: Individual features in locations missing "links" property
// ===========================================================================
// Spec: Each Feature in the locations response should include a link to
//   the location data query endpoint (e.g., rel="data").
// Implementation: Features have no "links" in properties.
// Fix: Add to each feature properties:
//   "links": [{ "href": "/edr/collections/weather/locations/{id}",
//               "rel": "data", "type": "application/prs.coverage+json" }]

#[tokio::test]
async fn finding_09_location_features_have_data_links() {
    let (_status, json) = get_json("/collections/weather/locations").await;
    let features = json["features"].as_array().unwrap();
    for feature in features {
        assert!(
            feature.get("links").is_some(),
            "Each location feature should have links"
        );
        let links = feature["links"].as_array().unwrap();
        let has_data_link = links.iter().any(|l| l["rel"] == "data");
        assert!(has_data_link, "Feature should have a 'data' link to its query endpoint");
    }
}

// ===========================================================================
// FINDING 10: Collection ID hardcoded to "weather"
// ===========================================================================
// Spec: The collection ID should come from configuration, not be hardcoded.
//   GET /collections/{collectionId} should work for any configured collection.
// Implementation: Handler checks `if id != "weather"` and returns 404.
//   build_collection_metadata() hardcodes "id": "weather".
// Impact: Server can only serve a single collection regardless of config.
// Fix: Accept collection ID from config/engine, build metadata dynamically.

#[tokio::test]
async fn finding_10_collection_id_hardcoded() {
    let (_status, json) = get_json("/collections/weather").await;
    // This passes, but demonstrates the hardcoding
    assert_eq!(json["id"], "weather");

    // Any other collection ID returns 404
    let (status, _) = get_json("/collections/anything_else").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ===========================================================================
// FINDING 11: Missing EDR query types (position, radius, area, etc.)
// ===========================================================================
// Spec: EDR 1.1 defines these query types:
//   - position, radius, area, cube, trajectory, corridor, items, instances, locations
// Implementation: Only "locations" is implemented.
// Impact: Core conformance class requires at minimum one query type, so
//   this technically passes, but the spec envisions more.
// Fix: Implement additional query types as needed. At minimum, "position"
//   is the most commonly expected query type.

#[tokio::test]
async fn finding_11_position_query_returns_400_for_unsupported_engine() {
    // URL-encode WKT parentheses: POINT(x y) -> POINT%2824.9384%2060.1699%29
    let (status, _) = get_json("/collections/weather/position?coords=POINT%2824.9384%2060.1699%29").await;
    // The CSV engine does not support position queries, so it returns 400
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "Position query should return 400 for engines that do not support it"
    );
}

#[tokio::test]
async fn finding_11b_radius_query_not_implemented() {
    let (status, _) = get_json("/collections/weather/radius?coords=POINT%2824.9384%2060.1699%29&within=50&within-units=km").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "Radius query endpoint is not implemented"
    );
}

#[tokio::test]
async fn finding_11c_area_query_returns_400_for_unsupported_engine() {
    let (status, _) = get_json("/collections/weather/area?coords=POLYGON%28%2820%2059%2C32%2059%2C32%2071%2C20%2071%2C20%2059%29%29").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "Area query should return 400 for engines that do not support it"
    );
}

// ===========================================================================
// FINDING 12: Collection extent.temporal.trs uses WKT, spec expects URI
// ===========================================================================
// Spec: The trs (temporal reference system) should be a URI, typically:
//   "http://www.opengis.net/def/uom/ISO-8601/0/Gregorian"
// Implementation: Uses a WKT string: TIMECRS["DateTime",TDATUM[...],...]
// Impact: Clients expecting a URI-based TRS identifier will not parse this.
// Fix: Use "http://www.opengis.net/def/uom/ISO-8601/0/Gregorian"

#[tokio::test]
async fn finding_12_temporal_trs_is_uri() {
    let (_status, json) = get_json("/collections/weather").await;
    let trs = json["extent"]["temporal"]["trs"].as_str().unwrap();
    assert!(trs.starts_with("http"), "TRS should be a URI, got: {trs}");
}

// ===========================================================================
// FINDING 13: Error response "code" field should match HTTP status text
// ===========================================================================
// Spec: Error schema: { "code": string, "description": string }
//   The "code" field is typically the HTTP status code as a string.
// Implementation: Uses "NotFound", "BadRequest", "ServerError" -- these are
//   reasonable but some spec examples use numeric codes like "404".
// Note: The spec schema says "code" is a string with no format constraint,
//   so this is more of an interoperability consideration than a hard violation.

#[tokio::test]
async fn finding_13_error_response_format() {
    let (status, json) = get_json("/collections/nonexistent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Verify required fields exist
    assert!(json.get("code").is_some(), "error must have 'code' field");
    // "description" is optional per spec, but we include it
    assert!(json.get("description").is_some());

    // Our code uses "NotFound" - spec is flexible but this is fine
    assert_eq!(json["code"], "NotFound");
}

// ===========================================================================
// FINDING 14: 500 errors leak internal error details
// ===========================================================================
// Spec: Error responses should not expose internal implementation details
//   to clients for security reasons.
// Implementation: 500 handler uses `e.to_string()` which may contain
//   file paths, stack traces, or other internal information.
// Fix: Use a generic "Internal server error" message for 500 responses.
//   Log the actual error server-side.

// This is a design/security finding - tested by inspection of handlers.rs
// The 500 path: Json(json!({ "code": "ServerError", "description": e.to_string() }))

// ===========================================================================
// FINDING 15: Collection metadata parameter_names missing "unit" info
// ===========================================================================
// Spec: parameter_names in collection metadata should include unit information
//   so clients can understand the measurement units without querying data.
// Implementation: build_collection_metadata() only includes "type" and
//   "observedProperty" -- no "unit" field.
// Impact: Clients cannot discover parameter units from collection metadata.
// Fix: Engine trait needs to expose parameter descriptions (not just names)
//   for collection-level metadata.

#[tokio::test]
async fn finding_15_parameter_names_missing_units() {
    let (_status, json) = get_json("/collections/weather").await;
    let params = json["parameter_names"].as_object().unwrap();
    for (name, param) in params {
        // Verify basic structure passes
        assert_eq!(param["type"], "Parameter", "param {name} must have type");
        assert!(
            param["observedProperty"].is_object(),
            "param {name} must have observedProperty"
        );

        // SPEC GAP: No unit information at collection level
        let has_unit = param.get("unit").is_some();
        assert!(
            !has_unit,
            "Documenting: param {name} lacks unit info at collection level"
        );
    }
}

// ===========================================================================
// FINDING 16: Collection data_queries.locations.link missing "hreflang"
// ===========================================================================
// Spec: Link objects SHOULD include "hreflang" to indicate the language
//   of the linked resource.
// Implementation: data_queries link omits hreflang.
// Impact: Minor - most clients don't require it.

#[tokio::test]
async fn finding_16_data_queries_link_structure() {
    let (_status, json) = get_json("/collections/weather").await;
    let link = &json["data_queries"]["locations"]["link"];

    // Verify required link fields exist
    assert!(link["href"].is_string(), "link must have href");
    assert!(link["rel"].is_string(), "link must have rel");

    // Verify variables structure
    let vars = &link["variables"];
    assert_eq!(vars["query_type"], "locations");
    assert!(vars["output_formats"].is_array());
}

// ===========================================================================
// FINDING 17: Collections list missing numberMatched / numberReturned
// ===========================================================================
// Spec: GET /collections response SHOULD include numberMatched and
//   numberReturned for pagination support.
// Implementation: Only returns "collections" and "links".
// Fix: Add "numberMatched" and "numberReturned" fields.

#[tokio::test]
async fn finding_17_collections_missing_count_fields() {
    let (_status, json) = get_json("/collections").await;
    // SPEC GAP: No pagination info
    assert!(
        json.get("numberMatched").is_none(),
        "Documenting: numberMatched not present"
    );
    assert!(
        json.get("numberReturned").is_none(),
        "Documenting: numberReturned not present"
    );
}

// ===========================================================================
// FINDING 18: Conformance classes missing OGC API Common
// ===========================================================================
// Spec: EDR 1.1 builds on OGC API Common. The conformance page should
//   declare conformance to OGC API Common classes:
//   - http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/core
//   - http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/landing-page
//   - http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/oas30 (if applicable)
// Implementation: Only declares EDR-specific conformance classes.
// Fix: Add OGC API Common conformance classes.

#[tokio::test]
async fn finding_18_conformance_has_ogc_common_classes() {
    let (_status, json) = get_json("/conformance").await;
    let conforms_to = json["conformsTo"].as_array().unwrap();
    let uris: Vec<&str> = conforms_to
        .iter()
        .filter_map(|v| v.as_str())
        .collect();

    assert!(uris.iter().any(|u| u.contains("ogcapi-edr")));
    assert!(
        uris.iter().any(|u| u.contains("ogcapi-common")),
        "Conformance must declare OGC API Common classes"
    );
}

// ===========================================================================
// FINDING 19: No support for HTTP 308 redirect responses
// ===========================================================================
// Spec: Query endpoints should support 308 Permanent Redirect responses
//   (for e.g. redirecting to the canonical URL form).
// Implementation: No redirect support at all.
// Impact: Minor - only needed for URL canonicalization.

// ===========================================================================
// FINDING 20: No "instances" endpoint
// ===========================================================================
// Spec: EDR 1.1 supports /collections/{id}/instances for temporal instances
//   (e.g., different forecast runs).
// Implementation: No instances endpoint exists.
// Fix: If applicable, add /collections/{id}/instances endpoint.

#[tokio::test]
async fn finding_20_instances_not_implemented() {
    let (status, _) = get_json("/collections/weather/instances").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "Instances endpoint is not implemented"
    );
}

// ===========================================================================
// FINDING 21: Location query missing "crs" query parameter support
// ===========================================================================
// Spec: EDR query endpoints should accept "crs" parameter to specify the
//   CRS of the response data.
// Implementation: LocationQueryParams only has datetime and parameter_name.
// Fix: Add "crs" (and "f") to LocationQueryParams.

#[tokio::test]
async fn finding_21_location_query_params_incomplete() {
    // Verify that the current params are accepted
    let (status, _) =
        get_json("/collections/weather/locations/station1?datetime=2024-01-01T00:00:00Z/2024-01-01T02:00:00Z").await;
    assert_eq!(status, StatusCode::OK);

    // parameter-name should also work
    let (status, _) =
        get_json("/collections/weather/locations/station1?parameter-name=temperature").await;
    assert_eq!(status, StatusCode::OK);

    // "crs" param is not supported but should be accepted without error
    // (axum by default ignores unknown query params, so this passes)
    let (status, _) =
        get_json("/collections/weather/locations/station1?crs=CRS84").await;
    assert_eq!(status, StatusCode::OK);
}

// ===========================================================================
// FINDING 22: Landing page missing required "title" in links
// ===========================================================================
// Spec: Link objects should have "title" for all links.
// Implementation: Landing page links DO have titles - this is correct.
//   But collection links in collections list do NOT have titles.

#[tokio::test]
async fn finding_22_landing_page_links_have_titles() {
    let (_status, json) = get_json("/").await;
    let links = json["links"].as_array().unwrap();
    for link in links {
        assert!(
            link.get("title").is_some(),
            "All landing page links should have titles"
        );
    }
}

#[tokio::test]
async fn finding_22b_collection_self_link_has_title() {
    let (_status, json) = get_json("/collections/weather").await;
    let links = json["links"].as_array().unwrap();
    for link in links {
        assert!(
            link.get("title").is_some(),
            "Collection self link should have a title"
        );
    }
}

// ===========================================================================
// FINDING 23: Locations response Content-Type should depend on output
// ===========================================================================
// Spec: The EDR /locations endpoint returns GeoJSON by default.
//   The response should have Content-Type: application/geo+json.
//   When CoverageJSON is requested, Content-Type: application/prs.coverage+json.
// Implementation: Always returns application/json regardless of content.
// (See also Finding 7 - duplicated here for emphasis on the /locations endpoint)

// ===========================================================================
// FINDING 24: No "items" endpoint on collection
// ===========================================================================
// Spec: EDR 1.1 section 8.3 mentions /collections/{id}/items as an
//   optional endpoint for accessing raw data items.
// Implementation: No items endpoint on EDR collections.

#[tokio::test]
async fn finding_24_items_not_implemented() {
    let (status, _) = get_json("/collections/weather/items").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "Items endpoint is not implemented on EDR"
    );
}

// ===========================================================================
// FINDING 25: Collection metadata "extent.spatial.bbox" format
// ===========================================================================
// Spec: bbox should be an array of arrays, where each inner array is
//   [west, south, east, north] (2D) or [west, south, min-z, east, north, max-z].
// Implementation: Correctly wraps bbox in an outer array: "bbox": [bbox].
//   But the Engine returns [f64; 4] and we wrap it as [bbox], which produces
//   [[19.0, 59.0, 32.0, 71.0]]. This is correct.

#[tokio::test]
async fn finding_25_bbox_format_correct() {
    let (_status, json) = get_json("/collections/weather").await;
    let bbox = json["extent"]["spatial"]["bbox"].as_array().unwrap();
    assert_eq!(bbox.len(), 1, "bbox should have one bounding box");
    let inner = bbox[0].as_array().unwrap();
    assert_eq!(inner.len(), 4, "bbox should be [west, south, east, north]");
}

// ===========================================================================
// FINDING 26: Temporal extent interval format
// ===========================================================================
// Spec: interval should be array of arrays: [["start", "end"]]
//   Values should be RFC 3339 timestamps or ".." for open-ended.
// Implementation: Correctly produces [["start", "end"]] format.

#[tokio::test]
async fn finding_26_temporal_interval_format() {
    let (_status, json) = get_json("/collections/weather").await;
    let interval = json["extent"]["temporal"]["interval"].as_array().unwrap();
    assert_eq!(interval.len(), 1);
    let inner = interval[0].as_array().unwrap();
    assert_eq!(inner.len(), 2);
    // Both should be RFC 3339 strings
    assert!(inner[0].is_string());
    assert!(inner[1].is_string());
}

// ===========================================================================
// FINDING 27: Conformance response structure is valid
// ===========================================================================

#[tokio::test]
async fn finding_27_conformance_valid() {
    let (status, json) = get_json("/conformance").await;
    assert_eq!(status, StatusCode::OK);
    let conforms_to = json["conformsTo"].as_array().unwrap();
    assert!(!conforms_to.is_empty(), "conformsTo must not be empty");
    for item in conforms_to {
        assert!(item.is_string(), "each conformsTo entry must be a string");
    }
}

// ===========================================================================
// FINDING 28: Landing page structure is valid
// ===========================================================================

#[tokio::test]
async fn finding_28_landing_page_valid_structure() {
    let (status, json) = get_json("/").await;
    assert_eq!(status, StatusCode::OK);

    // Required: links array
    let links = json["links"].as_array().unwrap();
    assert!(!links.is_empty());

    // Each link must have href and rel
    for link in links {
        assert!(link["href"].is_string(), "link must have href");
        assert!(link["rel"].is_string(), "link must have rel");
    }

    // Should have self, conformance, and data links
    let rels: Vec<&str> = links.iter().filter_map(|l| l["rel"].as_str()).collect();
    assert!(rels.contains(&"self"), "must have self link");
    assert!(rels.contains(&"conformance"), "must have conformance link");
    assert!(rels.contains(&"data"), "must have data link");
}

// ===========================================================================
// FINDING 29: Collections response valid structure
// ===========================================================================

#[tokio::test]
async fn finding_29_collections_valid_structure() {
    let (status, json) = get_json("/collections").await;
    assert_eq!(status, StatusCode::OK);

    assert!(json["collections"].is_array());
    assert!(json["links"].is_array());

    let collections = json["collections"].as_array().unwrap();
    assert!(!collections.is_empty());

    // Each collection should have id, title, links
    for col in collections {
        assert!(col["id"].is_string(), "collection must have id");
        assert!(col["links"].is_array(), "collection must have links");
    }
}

// ===========================================================================
// FINDING 30: Error responses have correct structure for all error paths
// ===========================================================================

#[tokio::test]
async fn finding_30_404_error_format() {
    let (status, json) = get_json("/collections/nonexistent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(json["code"].is_string());
    // "description" is optional per spec but we include it
}

#[tokio::test]
async fn finding_30b_400_error_format() {
    let (status, json) =
        get_json("/collections/weather/locations/station1?datetime=invalid").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(json["code"].is_string());
}

#[tokio::test]
async fn finding_30c_404_location_not_found() {
    let (status, json) =
        get_json("/collections/weather/locations/nonexistent_station").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["code"], "NotFound");
}

// ===========================================================================
// SUMMARY OF FINDINGS
// ===========================================================================
//
// CRITICAL (breaks spec compliance):
//  1. Collection missing required "crs" field
//  8. Locations GeoJSON response missing "links" array
// 12. Temporal extent "trs" uses WKT instead of URI
//
// HIGH (affects interoperability):
//  2. Spatial extent CRS should be a full OGC URI, not "CRS84"
//  7. Content-Type headers wrong for GeoJSON and CoverageJSON responses
// 15. Collection parameter_names missing unit information
// 18. Conformance missing OGC API Common classes
//
// MEDIUM (spec recommendations / SHOULD):
//  3. Landing page links use relative URLs (spec recommends absolute)
//  4. Landing page missing rel="service-desc" link
//  5. Collections endpoint ignores bbox/datetime query params
//  6. No "f" (format) query parameter support
//  9. Location features missing data links
// 17. Collections missing numberMatched/numberReturned
// 22. Collection self link missing title
//
// LOW (optional features not implemented):
// 10. Collection ID hardcoded to "weather"
// 11. Only "locations" query type implemented (no position/radius/area)
// 14. 500 errors may leak internal details
// 20. No "instances" endpoint
// 21. Location query missing "crs" param support
// 24. No "items" endpoint
//
// PASSING (correct):
// 13. Error response format is valid
// 25. Spatial extent bbox format is correct
// 26. Temporal extent interval format is correct
// 27. Conformance response structure is valid
// 28. Landing page structure is valid
// 29. Collections response structure is valid
// 30. Error responses work for 400/404 cases
