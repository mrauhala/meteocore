use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use api_edr::handlers::EdrState;
use ds_core::config::CollectionConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::model::*;

// ---------------------------------------------------------------------------
// Mock engine
// ---------------------------------------------------------------------------

struct MockEngine;

impl MockEngine {
    fn sample_locations() -> Vec<Location> {
        vec![
            Location {
                id: "helsinki".into(),
                label: "Helsinki".into(),
                latitude: 60.1699,
                longitude: 24.9384,
            },
            Location {
                id: "tampere".into(),
                label: "Tampere".into(),
                latitude: 61.4978,
                longitude: 23.7610,
            },
        ]
    }

    fn sample_query_result() -> QueryResult {
        let times: Vec<DateTime<Utc>> = (0..3)
            .map(|h| {
                format!("2024-01-01T{h:02}:00:00Z")
                    .parse::<DateTime<Utc>>()
                    .unwrap()
            })
            .collect();

        let mut parameters = HashMap::new();
        parameters.insert(
            "temperature".into(),
            ParameterDescription {
                label: "temperature".into(),
                unit: "degC".into(),
                observed_property: "temperature".into(),
            },
        );

        let mut ranges = HashMap::new();
        ranges.insert(
            "temperature".into(),
            NdArray {
                shape: vec![3],
                axis_names: vec!["t".into()],
                values: vec![Some(-2.5), Some(-2.8), None],
            },
        );

        QueryResult {
            domain: DomainDescription::PointSeries {
                x: 24.9384,
                y: 60.1699,
                t: times,
                z: None,
            },
            parameters,
            ranges,
        }
    }
}

impl EdrEngine for MockEngine {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(Self::sample_locations())
    }

    fn query_location(
        &self,
        location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError> {
        if location_id == "helsinki" || location_id == "tampere" {
            Ok(CoverageResponse::Single(Self::sample_query_result()))
        } else {
            Err(DataServerError::LocationNotFound(location_id.into()))
        }
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["temperature".into(), "humidity".into()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((
            "2024-01-01T00:00:00Z".parse().unwrap(),
            "2024-01-01T23:00:00Z".parse().unwrap(),
        ))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([23.7610, 60.1699, 24.9384, 61.4978])
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["locations".to_string(), "area".to_string()]
    }

    fn query_area(
        &self,
        coords: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError> {
        let polygon = ds_core::feature::parse_area_coords(coords)?;
        let mut coverages = Vec::new();
        for loc in Self::sample_locations() {
            if polygon.contains(loc.longitude, loc.latitude) {
                coverages.push(Self::sample_query_result());
            }
        }
        if coverages.is_empty() {
            return Err(DataServerError::LocationNotFound(
                "No locations found within the requested area".into(),
            ));
        }
        Ok(CoverageResponse::Collection(coverages))
    }

    fn query_position(
        &self,
        coords: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError> {
        // Accept any well-formed POINT(lon lat). Parsing is handled here to
        // exercise the handler's MULTIPOINT fan-out (which normalizes each
        // sub-point to POINT before calling the engine).
        let inner = coords
            .trim()
            .strip_prefix("POINT(")
            .or_else(|| coords.trim().strip_prefix("POINT ("))
            .and_then(|s| s.strip_suffix(')'))
            .ok_or_else(|| DataServerError::InvalidParameter(format!("bad point: {coords}")))?;
        let parts: Vec<&str> = inner.split_whitespace().collect();
        if parts.len() != 2 {
            return Err(DataServerError::InvalidParameter(format!(
                "bad point arity: {coords}"
            )));
        }
        let _lon: f64 = parts[0].parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("bad longitude: {}", parts[0]))
        })?;
        let _lat: f64 = parts[1].parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("bad latitude: {}", parts[1]))
        })?;
        Ok(CoverageResponse::Single(Self::sample_query_result()))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

fn build_router() -> axum::Router {
    let engine: Arc<dyn EdrEngine> = Arc::new(MockEngine);
    api_edr::router(make_edr_state(engine))
}

async fn get(uri: &str) -> (StatusCode, Value) {
    let app = build_router();
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap();
    (status, json)
}

// ---------------------------------------------------------------------------
// Landing page tests
// ---------------------------------------------------------------------------

mod landing_page {
    use super::*;

    #[tokio::test]
    async fn returns_200() {
        let (status, _) = get("/").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn has_required_links_field() {
        let (_, json) = get("/").await;
        assert!(
            json.get("links").is_some(),
            "Landing page must contain 'links' per OGC EDR spec"
        );
        assert!(json["links"].is_array());
    }

    #[tokio::test]
    async fn links_have_required_href_and_rel() {
        let (_, json) = get("/").await;
        let links = json["links"].as_array().unwrap();
        assert!(!links.is_empty(), "links array must not be empty");
        for link in links {
            assert!(
                link.get("href").is_some() && link["href"].is_string(),
                "Each link must have 'href' string: {link}"
            );
            assert!(
                link.get("rel").is_some() && link["rel"].is_string(),
                "Each link must have 'rel' string: {link}"
            );
        }
    }

    #[tokio::test]
    async fn has_title() {
        let (_, json) = get("/").await;
        assert!(
            json.get("title").is_some(),
            "Landing page should have a title"
        );
    }

    #[tokio::test]
    async fn has_self_link() {
        let (_, json) = get("/").await;
        let links = json["links"].as_array().unwrap();
        let has_self = links.iter().any(|l| l["rel"] == "self");
        assert!(
            has_self,
            "Landing page should include a 'self' link relation"
        );
    }

    #[tokio::test]
    async fn has_conformance_link() {
        let (_, json) = get("/").await;
        let links = json["links"].as_array().unwrap();
        let has_conformance = links.iter().any(|l| l["rel"] == "conformance");
        assert!(
            has_conformance,
            "Landing page should include a 'conformance' link relation"
        );
    }

    #[tokio::test]
    async fn has_data_link() {
        let (_, json) = get("/").await;
        let links = json["links"].as_array().unwrap();
        let has_data = links.iter().any(|l| l["rel"] == "data");
        assert!(
            has_data,
            "Landing page should include a 'data' link relation for collections"
        );
    }
}

// ---------------------------------------------------------------------------
// Conformance tests
// ---------------------------------------------------------------------------

mod conformance {
    use super::*;

    #[tokio::test]
    async fn returns_200() {
        let (status, _) = get("/conformance").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn has_required_conforms_to_field() {
        let (_, json) = get("/conformance").await;
        assert!(
            json.get("conformsTo").is_some(),
            "Conformance response must contain 'conformsTo'"
        );
        assert!(json["conformsTo"].is_array());
    }

    #[tokio::test]
    async fn conforms_to_contains_strings() {
        let (_, json) = get("/conformance").await;
        let conforms = json["conformsTo"].as_array().unwrap();
        assert!(!conforms.is_empty());
        for item in conforms {
            assert!(item.is_string(), "Each conformsTo entry must be a string");
        }
    }

    #[tokio::test]
    async fn declares_edr_core_conformance() {
        let (_, json) = get("/conformance").await;
        let conforms = json["conformsTo"].as_array().unwrap();
        let has_core = conforms
            .iter()
            .any(|v| v.as_str().unwrap().contains("ogcapi-edr-1"));
        assert!(has_core, "Must declare OGC API - EDR conformance class");
    }

    #[tokio::test]
    async fn declares_covjson_conformance() {
        let (_, json) = get("/conformance").await;
        let conforms = json["conformsTo"].as_array().unwrap();
        let has_covjson = conforms
            .iter()
            .any(|v| v.as_str().unwrap().contains("covjson"));
        assert!(has_covjson, "Must declare CoverageJSON conformance class");
    }
}

// ---------------------------------------------------------------------------
// Collections tests
// ---------------------------------------------------------------------------

mod collections {
    use super::*;

    // -- Collection listing --

    #[tokio::test]
    async fn listing_returns_200() {
        let (status, _) = get("/collections").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn listing_has_required_links() {
        let (_, json) = get("/collections").await;
        assert!(json.get("links").is_some(), "Collections must have 'links'");
        assert!(json["links"].is_array());
    }

    #[tokio::test]
    async fn listing_has_required_collections_array() {
        let (_, json) = get("/collections").await;
        assert!(
            json.get("collections").is_some(),
            "Collections response must have 'collections'"
        );
        assert!(json["collections"].is_array());
    }

    #[tokio::test]
    async fn listing_collections_not_empty() {
        let (_, json) = get("/collections").await;
        let cols = json["collections"].as_array().unwrap();
        assert!(
            !cols.is_empty(),
            "Mock engine should yield at least one collection"
        );
    }

    #[tokio::test]
    async fn listing_each_collection_has_id() {
        let (_, json) = get("/collections").await;
        let cols = json["collections"].as_array().unwrap();
        for col in cols {
            assert!(
                col.get("id").is_some() && col["id"].is_string(),
                "Each collection must have an 'id' string"
            );
        }
    }

    // -- Single collection detail --

    #[tokio::test]
    async fn detail_returns_200() {
        let (status, _) = get("/collections/weather").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn detail_has_required_id() {
        let (_, json) = get("/collections/weather").await;
        assert_eq!(json["id"], "weather");
    }

    #[tokio::test]
    async fn detail_has_required_links() {
        let (_, json) = get("/collections/weather").await;
        assert!(json.get("links").is_some(), "Collection must have 'links'");
        let links = json["links"].as_array().unwrap();
        assert!(!links.is_empty());
        for link in links {
            assert!(link.get("href").is_some());
            assert!(link.get("rel").is_some());
        }
    }

    #[tokio::test]
    async fn detail_has_required_extent() {
        let (_, json) = get("/collections/weather").await;
        assert!(
            json.get("extent").is_some(),
            "Collection must have 'extent' per OGC EDR spec"
        );
        let extent = &json["extent"];
        assert!(extent.is_object());
    }

    #[tokio::test]
    async fn detail_extent_has_spatial() {
        let (_, json) = get("/collections/weather").await;
        let extent = &json["extent"];
        assert!(
            extent.get("spatial").is_some(),
            "extent should include 'spatial'"
        );
        assert!(
            extent["spatial"].get("bbox").is_some(),
            "spatial extent should include 'bbox'"
        );
    }

    #[tokio::test]
    async fn detail_extent_has_temporal() {
        let (_, json) = get("/collections/weather").await;
        let extent = &json["extent"];
        assert!(
            extent.get("temporal").is_some(),
            "extent should include 'temporal'"
        );
        assert!(
            extent["temporal"].get("interval").is_some(),
            "temporal extent should include 'interval'"
        );
    }

    #[tokio::test]
    async fn detail_has_required_data_queries() {
        let (_, json) = get("/collections/weather").await;
        assert!(
            json.get("data_queries").is_some(),
            "Collection must have 'data_queries' per OGC EDR spec"
        );
    }

    #[tokio::test]
    async fn detail_has_required_parameter_names() {
        let (_, json) = get("/collections/weather").await;
        assert!(
            json.get("parameter_names").is_some(),
            "Collection must have 'parameter_names' per OGC EDR spec"
        );
        assert!(json["parameter_names"].is_object());
    }

    #[tokio::test]
    async fn detail_parameter_names_match_engine() {
        let (_, json) = get("/collections/weather").await;
        let params = json["parameter_names"].as_object().unwrap();
        assert!(params.contains_key("temperature"));
        assert!(params.contains_key("humidity"));
    }

    #[tokio::test]
    async fn detail_parameters_have_type_and_observed_property() {
        let (_, json) = get("/collections/weather").await;
        let params = json["parameter_names"].as_object().unwrap();
        for (_name, param) in params {
            assert_eq!(
                param["type"], "Parameter",
                "Each parameter must have type 'Parameter'"
            );
            assert!(
                param.get("observedProperty").is_some(),
                "Each parameter must have 'observedProperty'"
            );
            assert!(
                param["observedProperty"].get("label").is_some(),
                "observedProperty must have 'label'"
            );
        }
    }

    #[tokio::test]
    async fn detail_has_required_output_formats() {
        let (_, json) = get("/collections/weather").await;
        assert!(
            json.get("output_formats").is_some(),
            "Collection must have 'output_formats' per OGC EDR spec"
        );
        assert!(json["output_formats"].is_array());
    }

    #[tokio::test]
    async fn detail_has_required_crs() {
        let (_, json) = get("/collections/weather").await;
        assert!(
            json.get("crs").is_some(),
            "Collection must have 'crs' per OGC EDR spec"
        );
        let crs = json["crs"].as_array().unwrap();
        assert!(!crs.is_empty(), "crs array must not be empty");
    }

    #[tokio::test]
    async fn detail_data_queries_has_locations() {
        let (_, json) = get("/collections/weather").await;
        let dq = &json["data_queries"];
        assert!(
            dq.get("locations").is_some(),
            "data_queries should advertise 'locations' query type"
        );
        assert!(
            dq["locations"].get("link").is_some(),
            "locations data query should have a 'link' object"
        );
        assert!(
            dq["locations"]["link"].get("href").is_some(),
            "locations link should have 'href'"
        );
    }

    #[tokio::test]
    async fn collection_exposes_apis_array() {
        let (_, json) = get("/collections/weather").await;
        let apis = json["apis"].as_array().expect("apis must be present");
        assert!(apis.iter().any(|a| a == "edr"));
    }
}

// ---------------------------------------------------------------------------
// Locations query tests
// ---------------------------------------------------------------------------

mod locations {
    use super::*;

    #[tokio::test]
    async fn returns_200() {
        let (status, _) = get("/collections/weather/locations").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn returns_geojson_feature_collection() {
        let (_, json) = get("/collections/weather/locations").await;
        assert_eq!(json["type"], "FeatureCollection");
    }

    #[tokio::test]
    async fn has_features_array() {
        let (_, json) = get("/collections/weather/locations").await;
        assert!(json.get("features").is_some());
        assert!(json["features"].is_array());
    }

    #[tokio::test]
    async fn features_have_required_geojson_structure() {
        let (_, json) = get("/collections/weather/locations").await;
        let features = json["features"].as_array().unwrap();
        assert!(!features.is_empty());
        for feature in features {
            assert_eq!(
                feature["type"], "Feature",
                "Each feature must have type 'Feature'"
            );
            assert!(
                feature.get("geometry").is_some(),
                "Each feature must have 'geometry'"
            );
            assert!(
                feature.get("properties").is_some(),
                "Each feature must have 'properties'"
            );
            assert!(feature.get("id").is_some(), "Each feature must have 'id'");
        }
    }

    #[tokio::test]
    async fn feature_geometry_is_point() {
        let (_, json) = get("/collections/weather/locations").await;
        let features = json["features"].as_array().unwrap();
        for feature in features {
            let geom = &feature["geometry"];
            assert_eq!(geom["type"], "Point");
            let coords = geom["coordinates"].as_array().unwrap();
            assert_eq!(coords.len(), 2, "Point coordinates should have [lon, lat]");
        }
    }

    #[tokio::test]
    async fn features_match_mock_locations() {
        let (_, json) = get("/collections/weather/locations").await;
        let features = json["features"].as_array().unwrap();
        assert_eq!(features.len(), 2);
        let ids: Vec<&str> = features.iter().map(|f| f["id"].as_str().unwrap()).collect();
        assert!(ids.contains(&"helsinki"));
        assert!(ids.contains(&"tampere"));
    }

    #[tokio::test]
    async fn unknown_collection_returns_404() {
        let (status, json) = get("/collections/nonexistent/locations").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(
            json.get("code").is_some(),
            "Error response must have 'code'"
        );
        assert!(
            json.get("description").is_some(),
            "Error response must have 'description'"
        );
    }

    #[tokio::test]
    async fn features_have_required_edr_properties() {
        let (_, json) = get("/collections/weather/locations").await;
        let features = json["features"].as_array().unwrap();
        for feature in features {
            let props = &feature["properties"];
            assert!(
                props.get("label").is_some() && props["label"].is_string(),
                "Feature properties must have 'label' string"
            );
            assert!(
                props.get("datetime").is_some() && props["datetime"].is_string(),
                "Feature properties must have 'datetime' string"
            );
            assert!(
                props.get("parameter-name").is_some() && props["parameter-name"].is_array(),
                "Feature properties must have 'parameter-name' array"
            );
            assert!(
                props.get("edrqueryendpoint").is_some() && props["edrqueryendpoint"].is_string(),
                "Feature properties must have 'edrqueryendpoint' string"
            );
        }
    }

    #[tokio::test]
    async fn validates_against_edr_locations_schema() {
        let schema_str = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../schemas/edr-locations-geojson.json"
        ))
        .expect("Failed to read EDR locations GeoJSON schema");
        let schema: Value = serde_json::from_str(&schema_str).unwrap();
        let validator = jsonschema::Validator::new(&schema).expect("Failed to compile schema");

        let (_, json) = get("/collections/weather/locations").await;

        let errors: Vec<_> = validator.iter_errors(&json).collect();
        if !errors.is_empty() {
            let msgs: Vec<String> = errors
                .iter()
                .map(|e| format!("  - {e} (at {})", e.instance_path))
                .collect();
            panic!(
                "Locations GeoJSON schema validation failed:\n{}\n\nJSON:\n{}",
                msgs.join("\n"),
                serde_json::to_string_pretty(&json).unwrap()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Location data query tests (CoverageJSON)
// ---------------------------------------------------------------------------

mod location_data {
    use super::*;

    #[tokio::test]
    async fn returns_200() {
        let (status, _) = get("/collections/weather/locations/helsinki").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn returns_coverage_json_type() {
        let (_, json) = get("/collections/weather/locations/helsinki").await;
        assert_eq!(json["type"], "Coverage");
    }

    #[tokio::test]
    async fn has_required_domain() {
        let (_, json) = get("/collections/weather/locations/helsinki").await;
        let domain = &json["domain"];
        assert!(domain.is_object());
        assert_eq!(domain["type"], "Domain");
    }

    #[tokio::test]
    async fn has_required_parameters() {
        let (_, json) = get("/collections/weather/locations/helsinki").await;
        assert!(json.get("parameters").is_some());
        assert!(json["parameters"].is_object());
    }

    #[tokio::test]
    async fn has_required_ranges() {
        let (_, json) = get("/collections/weather/locations/helsinki").await;
        assert!(json.get("ranges").is_some());
        assert!(json["ranges"].is_object());
    }

    #[tokio::test]
    async fn domain_has_axes_and_referencing() {
        let (_, json) = get("/collections/weather/locations/helsinki").await;
        let domain = &json["domain"];
        assert!(domain.get("axes").is_some());
        assert!(domain.get("referencing").is_some());
        assert!(domain["referencing"].is_array());
    }

    #[tokio::test]
    async fn domain_type_is_point_series() {
        let (_, json) = get("/collections/weather/locations/helsinki").await;
        assert_eq!(json["domain"]["domainType"], "PointSeries");
    }

    #[tokio::test]
    async fn axes_have_x_y_t() {
        let (_, json) = get("/collections/weather/locations/helsinki").await;
        let axes = &json["domain"]["axes"];
        assert!(axes.get("x").is_some(), "PointSeries must have x axis");
        assert!(axes.get("y").is_some(), "PointSeries must have y axis");
        assert!(axes.get("t").is_some(), "PointSeries must have t axis");
    }

    #[tokio::test]
    async fn x_and_y_are_single_value() {
        let (_, json) = get("/collections/weather/locations/helsinki").await;
        let axes = &json["domain"]["axes"];
        assert_eq!(axes["x"]["values"].as_array().unwrap().len(), 1);
        assert_eq!(axes["y"]["values"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn referencing_has_spatial_and_temporal() {
        let (_, json) = get("/collections/weather/locations/helsinki").await;
        let refs = json["domain"]["referencing"].as_array().unwrap();
        assert_eq!(refs.len(), 2);

        let spatial = &refs[0];
        assert_eq!(spatial["system"]["type"], "GeographicCRS");
        assert!(spatial["system"].get("id").is_some());

        let temporal = &refs[1];
        assert_eq!(temporal["system"]["type"], "TemporalRS");
        assert_eq!(temporal["system"]["calendar"], "Gregorian");
    }

    #[tokio::test]
    async fn range_ndarray_structure() {
        let (_, json) = get("/collections/weather/locations/helsinki").await;
        let ranges = json["ranges"].as_object().unwrap();
        for (_name, range) in ranges {
            assert_eq!(range["type"], "NdArray");
            assert!(
                range.get("dataType").is_some(),
                "NdArray must have 'dataType'"
            );
            assert!(range.get("values").is_some(), "NdArray must have 'values'");
            assert!(range.get("shape").is_some(), "NdArray must have 'shape'");
            assert!(
                range.get("axisNames").is_some(),
                "NdArray must have 'axisNames'"
            );
        }
    }

    #[tokio::test]
    async fn range_values_length_matches_shape() {
        let (_, json) = get("/collections/weather/locations/helsinki").await;
        let ranges = json["ranges"].as_object().unwrap();
        for (_name, range) in ranges {
            let shape: Vec<u64> = range["shape"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap())
                .collect();
            let expected: u64 = shape.iter().product();
            let actual = range["values"].as_array().unwrap().len() as u64;
            assert_eq!(
                actual, expected,
                "values length must equal product of shape"
            );
        }
    }

    #[tokio::test]
    async fn parameter_structure() {
        let (_, json) = get("/collections/weather/locations/helsinki").await;
        let params = json["parameters"].as_object().unwrap();
        for (_name, param) in params {
            assert_eq!(param["type"], "Parameter");
            assert!(param.get("observedProperty").is_some());
            assert!(param["observedProperty"].get("label").is_some());
            // i18n label must use BCP 47 key
            assert!(
                param["observedProperty"]["label"].get("en").is_some(),
                "observedProperty label must use BCP 47 key like 'en'"
            );
        }
    }

    #[tokio::test]
    async fn with_datetime_parameter() {
        let (status, json) =
            get("/collections/weather/locations/helsinki?datetime=2024-01-01T00:00:00Z/2024-01-01T02:00:00Z")
                .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["type"], "Coverage");
    }

    #[tokio::test]
    async fn with_parameter_name_filter() {
        let (status, json) =
            get("/collections/weather/locations/helsinki?parameter-name=temperature").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["type"], "Coverage");
    }

    #[tokio::test]
    async fn null_values_represented_as_json_null() {
        let (_, json) = get("/collections/weather/locations/helsinki").await;
        let values = json["ranges"]["temperature"]["values"].as_array().unwrap();
        // Our mock has [Some(-2.5), Some(-2.8), None]
        assert!(
            values[2].is_null(),
            "None values must be serialized as JSON null"
        );
    }
}

// ---------------------------------------------------------------------------
// Error response tests
// ---------------------------------------------------------------------------

mod error_responses {
    use super::*;

    #[tokio::test]
    async fn collection_not_found_returns_404() {
        let (status, json) = get("/collections/nonexistent").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(
            json.get("code").is_some(),
            "Error must have 'code' per OGC EDR spec"
        );
        assert!(json["code"].is_string());
        assert!(
            json.get("description").is_some(),
            "Error must have 'description' per OGC EDR spec"
        );
        assert!(json["description"].is_string());
    }

    #[tokio::test]
    async fn location_not_found_returns_404() {
        let (status, json) = get("/collections/weather/locations/nonexistent").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(json.get("code").is_some());
        assert!(json.get("description").is_some());
    }

    #[tokio::test]
    async fn invalid_datetime_returns_400() {
        let (status, json) =
            get("/collections/weather/locations/helsinki?datetime=not-a-date").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(json.get("code").is_some(), "400 error must have 'code'");
        assert!(
            json.get("description").is_some(),
            "400 error must have 'description'"
        );
    }

    #[tokio::test]
    async fn locations_unknown_collection_returns_404() {
        let (status, json) = get("/collections/unknown/locations").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(json.get("code").is_some());
        assert!(json.get("description").is_some());
    }

    #[tokio::test]
    async fn error_code_is_string() {
        let (_, json) = get("/collections/nonexistent").await;
        assert!(
            json["code"].is_string(),
            "OGC EDR error 'code' field must be a string"
        );
    }

    #[tokio::test]
    async fn error_description_is_string() {
        let (_, json) = get("/collections/nonexistent").await;
        assert!(
            json["description"].is_string(),
            "OGC EDR error 'description' field must be a string"
        );
    }

    #[tokio::test]
    async fn nonexistent_route_returns_404() {
        let app = build_router();
        let req = Request::builder()
            .uri("/does/not/exist")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}

// ---------------------------------------------------------------------------
// Unimplemented query type stubs (OGC EDR 1.1 spec)
// ---------------------------------------------------------------------------

mod unimplemented_queries {
    use super::*;

    #[tokio::test]
    async fn position_query_point_returns_single_coverage() {
        // POINT(24.9384 60.1699)
        let (status, json) =
            get("/collections/weather/position?coords=POINT%2824.9384%2060.1699%29").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["type"], "Coverage");
        assert!(json["domain"].is_object());
    }

    #[tokio::test]
    async fn position_query_multipoint_returns_coverage_collection() {
        // MULTIPOINT((24.94 60.17),(23.76 61.5))
        let (status, json) = get(
            "/collections/weather/position?coords=MULTIPOINT%28%2824.94%2060.17%29%2C%2823.76%2061.5%29%29",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["type"], "CoverageCollection");
        assert_eq!(json["domainType"], "PointSeries");
        let coverages = json["coverages"].as_array().unwrap();
        assert_eq!(coverages.len(), 2);
        for cov in coverages {
            assert_eq!(cov["type"], "Coverage");
            assert!(cov["domain"].is_object());
        }
        // Parameters hoisted to collection level.
        assert!(json["parameters"].is_object());
    }

    #[tokio::test]
    async fn position_query_multipoint_flat_form() {
        // MULTIPOINT(24.94 60.17, 23.76 61.5, 27.67 62.9)
        let (status, json) = get(
            "/collections/weather/position?coords=MULTIPOINT%2824.94%2060.17%2C%2023.76%2061.5%2C%2027.67%2062.9%29",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["type"], "CoverageCollection");
        assert_eq!(json["coverages"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn position_query_rejects_polygon() {
        let (status, json) = get(
            "/collections/weather/position?coords=POLYGON%28%280%200%2C1%200%2C1%201%2C0%201%2C0%200%29%29",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["code"], "BadRequest");
    }

    #[tokio::test]
    #[ignore = "radius query not yet implemented"]
    async fn radius_query() {
        // GET /collections/{id}/radius?coords=POINT(24.9384 60.1699)&within=10&within-units=km
        let (status, _) = get(
            "/collections/weather/radius?coords=POINT(24.9384 60.1699)&within=10&within-units=km",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn area_query() {
        // POLYGON covering Helsinki (24.9, 60.1) — should match Helsinki but not Tampere
        let (status, json) = get(
            "/collections/weather/area?coords=POLYGON%28%2824.5%2060.0%2C25.5%2060.0%2C25.5%2060.5%2C24.5%2060.5%2C24.5%2060.0%29%29",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["type"], "CoverageCollection");
        assert_eq!(json["domainType"], "PointSeries");
        let coverages = json["coverages"].as_array().unwrap();
        assert_eq!(coverages.len(), 1, "should match Helsinki only");
        assert_eq!(coverages[0]["type"], "Coverage");
    }

    #[tokio::test]
    async fn area_query_bbox_format() {
        // bbox covering both Helsinki and Tampere
        let (status, json) = get("/collections/weather/area?coords=23.0,59.0,25.5,62.0").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["type"], "CoverageCollection");
        let coverages = json["coverages"].as_array().unwrap();
        assert_eq!(coverages.len(), 2, "should match both locations");
    }

    #[tokio::test]
    async fn area_query_no_match() {
        // POLYGON far from any stations
        let (status, _) = get(
            "/collections/weather/area?coords=POLYGON%28%280%200%2C1%200%2C1%201%2C0%201%2C0%200%29%29",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    #[ignore = "cube query not yet implemented"]
    async fn cube_query() {
        // GET /collections/{id}/cube?bbox=24,60,25,61
        let (status, _) = get("/collections/weather/cube?bbox=24,60,25,61").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    #[ignore = "trajectory query not yet implemented"]
    async fn trajectory_query() {
        // GET /collections/{id}/trajectory?coords=LINESTRING(24 60,25 61)
        let (status, _) =
            get("/collections/weather/trajectory?coords=LINESTRING(24 60,25 61)").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    #[ignore = "corridor query not yet implemented"]
    async fn corridor_query() {
        // GET /collections/{id}/corridor?coords=LINESTRING(...)&corridor-width=10&width-units=km
        let (status, _) = get(
            "/collections/weather/corridor?coords=LINESTRING(24 60,25 61)&corridor-width=10&width-units=km",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    #[ignore = "instances endpoint not yet implemented"]
    async fn instances_listing() {
        // GET /collections/{id}/instances
        let (status, _) = get("/collections/weather/instances").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    #[ignore = "items query not yet implemented"]
    async fn items_query() {
        // GET /collections/{id}/items
        let (status, _) = get("/collections/weather/items").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn collection_has_crs_field() {
        let (_, json) = get("/collections/weather").await;
        assert!(
            json.get("crs").is_some(),
            "OGC EDR spec requires 'crs' in collection metadata"
        );
        let crs = json["crs"].as_array().unwrap();
        assert!(!crs.is_empty());
    }
}
