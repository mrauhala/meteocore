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

#[path = "support/edr_schema.rs"]
mod edr_schema;
use edr_schema::{GEOJSON, JSON};

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
                standard_name: None,
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
        _reference_time: Option<DateTime<Utc>>,
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
        vec![
            "locations".to_string(),
            "position".to_string(),
            "area".to_string(),
            "radius".to_string(),
        ]
    }

    fn query_area(
        &self,
        coords: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
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
        _reference_time: Option<DateTime<Utc>>,
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
            bufr: None,
            satellite: None,
            preview: None,
            derive_wind: None,
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

/// Like [`get`] but tolerates a non-JSON body (axum's own query-extractor
/// rejection for a missing required parameter is plain text).
async fn get_status(uri: &str) -> (StatusCode, Option<Value>) {
    let app = build_router();
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).ok())
}

// ---------------------------------------------------------------------------
// Reverse-proxy base-URL resolution (#12)
// ---------------------------------------------------------------------------

mod proxy_headers {
    use super::*;

    /// Build a router whose state has an explicit fallback base URL and a
    /// configurable `trust_proxy_headers` flag.
    fn router_with(base_url: &str, trust_proxy_headers: bool) -> axum::Router {
        let engine: Arc<dyn EdrEngine> = Arc::new(MockEngine);
        let state = make_edr_state(engine);
        let cur = state.load();
        state.store(Arc::new(EdrState {
            engines: cur.engines.clone(),
            feature_engines: cur.feature_engines.clone(),
            collections: cur.collections.clone(),
            styles: cur.styles.clone(),
            base_url: base_url.to_string(),
            trust_proxy_headers,
        }));
        api_edr::router(state)
    }

    /// Fetch the landing page's `self` link href with the given forwarding headers.
    async fn self_href(app: axum::Router, headers: &[(&str, &str)]) -> String {
        let mut req = Request::builder().uri("/");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&body).unwrap();
        json["links"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["rel"] == "self")
            .expect("landing page has a self link")["href"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn x_forwarded_headers_rewrite_links_when_trusted() {
        let href = self_href(
            router_with("http://127.0.0.1:8000", true),
            &[
                ("x-forwarded-host", "api.example.com"),
                ("x-forwarded-proto", "https"),
            ],
        )
        .await;
        assert_eq!(href, "https://api.example.com/edr/");
    }

    #[tokio::test]
    async fn forwarded_header_rewrites_links_when_trusted() {
        let href = self_href(
            router_with("http://127.0.0.1:8000", true),
            &[(
                "forwarded",
                "for=192.0.2.1;host=proxy.example.com;proto=https",
            )],
        )
        .await;
        assert_eq!(href, "https://proxy.example.com/edr/");
    }

    #[tokio::test]
    async fn headers_ignored_when_not_trusted() {
        // The default: a client cannot spoof the base URL.
        let href = self_href(
            router_with("http://127.0.0.1:8000", false),
            &[
                ("x-forwarded-host", "evil.example.com"),
                ("x-forwarded-proto", "https"),
            ],
        )
        .await;
        assert_eq!(href, "http://127.0.0.1:8000/edr/");
    }

    #[tokio::test]
    async fn falls_back_without_headers() {
        let href = self_href(router_with("http://127.0.0.1:8000", true), &[]).await;
        assert_eq!(href, "http://127.0.0.1:8000/edr/");
    }
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

    #[tokio::test]
    async fn validates_against_edr_bundles() {
        let (_, json) = get("/").await;
        edr_schema::assert_valid("/", JSON, &json, "landing page");
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

    #[tokio::test]
    async fn validates_against_edr_bundles() {
        let (_, json) = get("/conformance").await;
        edr_schema::assert_valid("/conformance", JSON, &json, "conformance");
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

    /// `/collections` validates against the OGC EDR 1.1 and 1.2 bundled
    /// OpenAPI schemas for `GET /collections` 200 `application/json`. The
    /// `extent.vertical` block in particular requires `vrs` and typed
    /// strings for `interval` / `values` — this test guards against the
    /// regressions that broke a real radar collection. A dedicated
    /// vertical-aware mock fires that branch without disturbing the
    /// `no-vertical` invariant the rest of the suite relies on.
    #[tokio::test]
    async fn listing_validates_against_ogc_edr_schema() {
        // Reuse MockEngine for everything except `get_vertical_extent`
        // by wrapping it — that way every other handler invariant
        // (locations, parameters, etc.) is exercised identically.
        struct VerticalMockEngine(MockEngine);
        impl EdrEngine for VerticalMockEngine {
            fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
                self.0.get_locations()
            }
            fn query_location(
                &self,
                location_id: &str,
                dt: Option<(DateTime<Utc>, DateTime<Utc>)>,
                params: Option<&[String]>,
                z: Option<&[f64]>,
                rt: Option<DateTime<Utc>>,
            ) -> Result<CoverageResponse, DataServerError> {
                self.0.query_location(location_id, dt, params, z, rt)
            }
            fn get_parameters(&self) -> Vec<String> {
                self.0.get_parameters()
            }
            fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
                self.0.get_temporal_extent()
            }
            fn get_spatial_extent(&self) -> Option<[f64; 4]> {
                self.0.get_spatial_extent()
            }
            fn get_vertical_extent(&self) -> Option<ds_core::vertical::VerticalDimension> {
                Some(ds_core::vertical::VerticalDimension::new(
                    ds_core::vertical::VerticalKind::Pressure,
                    vec![1000.0, 850.0, 700.0, 500.0, 250.0],
                ))
            }
            fn supported_query_types(&self) -> Vec<String> {
                self.0.supported_query_types()
            }
        }

        let engine: Arc<dyn EdrEngine> = Arc::new(VerticalMockEngine(MockEngine));
        let router = api_edr::router(make_edr_state(engine));
        let req = Request::builder()
            .uri("/collections")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&body).unwrap();

        edr_schema::assert_valid("/collections", JSON, &json, "vertical collection listing");
    }

    /// A parameter on its own time axis (a satellite product, #819) carries
    /// its own `extent.temporal` in `parameter_names`; the collection keeps
    /// the union. Both documents still validate against EDR 1.1 and 1.2,
    /// whose parameter schemas allow an `extent`.
    #[tokio::test]
    async fn per_parameter_temporal_extent_validates() {
        struct OwnTimesEngine(MockEngine);
        impl EdrEngine for OwnTimesEngine {
            fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
                self.0.get_locations()
            }
            fn query_location(
                &self,
                location_id: &str,
                dt: Option<(DateTime<Utc>, DateTime<Utc>)>,
                params: Option<&[String]>,
                z: Option<&[f64]>,
                rt: Option<DateTime<Utc>>,
            ) -> Result<CoverageResponse, DataServerError> {
                self.0.query_location(location_id, dt, params, z, rt)
            }
            fn get_parameters(&self) -> Vec<String> {
                self.0.get_parameters()
            }
            fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
                self.0.get_temporal_extent()
            }
            fn get_available_times(&self) -> Option<Vec<DateTime<Utc>>> {
                Some(vec![
                    "2024-01-01T00:00:00Z".parse().unwrap(),
                    "2024-01-01T23:00:00Z".parse().unwrap(),
                ])
            }
            fn get_parameter_available_times(&self, parameter: &str) -> Option<Vec<DateTime<Utc>>> {
                (parameter == "humidity").then(|| vec!["2024-01-01T00:00:00Z".parse().unwrap()])
            }
            fn get_spatial_extent(&self) -> Option<[f64; 4]> {
                self.0.get_spatial_extent()
            }
            fn supported_query_types(&self) -> Vec<String> {
                self.0.supported_query_types()
            }
        }

        let engine: Arc<dyn EdrEngine> = Arc::new(OwnTimesEngine(MockEngine));
        for (uri, path) in [
            ("/collections", "/collections"),
            ("/collections/weather", "/collections/{collectionId}"),
        ] {
            let router = api_edr::router(make_edr_state(engine.clone()));
            let resp = router
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let json: Value =
                serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            let collection = match json.get("collections") {
                Some(list) => &list[0],
                None => &json,
            };
            let names = &collection["parameter_names"];
            assert_eq!(
                names["humidity"]["extent"]["temporal"]["values"],
                serde_json::json!(["2024-01-01T00:00:00+00:00"]),
                "{uri}"
            );
            assert!(names["temperature"].get("extent").is_none(), "{uri}");
            assert_eq!(
                collection["extent"]["temporal"]["values"]
                    .as_array()
                    .map(Vec::len),
                Some(2)
            );
            edr_schema::assert_valid(path, JSON, &json, uri);
        }
    }

    /// `parameter_names` follows OGC API - EDR Metocean Profile Requirement 7
    /// (#273) as far as the engine's metadata goes: a `label` of at most 50
    /// characters, a `description` that is not just the label, a QUDT
    /// `unit.symbol` for units QUDT has, and the CF standard name URI as
    /// `observedProperty.id` when the engine knows one. Still EDR 1.1- and
    /// 1.2-valid.
    #[tokio::test]
    async fn parameter_names_follow_metocean_profile() {
        struct DescribedEngine(MockEngine);
        impl EdrEngine for DescribedEngine {
            fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
                self.0.get_locations()
            }
            fn query_location(
                &self,
                location_id: &str,
                dt: Option<(DateTime<Utc>, DateTime<Utc>)>,
                params: Option<&[String]>,
                z: Option<&[f64]>,
                rt: Option<DateTime<Utc>>,
            ) -> Result<CoverageResponse, DataServerError> {
                self.0.query_location(location_id, dt, params, z, rt)
            }
            fn get_parameters(&self) -> Vec<String> {
                self.get_parameter_descriptions().into_keys().collect()
            }
            fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
                let desc = |label: &str, unit: &str, id: &str, cf: Option<&str>| {
                    let desc = ParameterDescription {
                        label: label.into(),
                        unit: unit.into(),
                        observed_property: id.into(),
                        standard_name: cf.map(Into::into),
                    };
                    (id.to_string(), desc)
                };
                HashMap::from([
                    desc("2 metre temperature", "K", "t2m", Some("air_temperature")),
                    desc("DBZH — Reflectivity (horizontal)", "dBZ", "DBZH", None),
                    desc(
                        "Water equivalent of accumulated snow depth (2 m above ground)",
                        "kg m-2",
                        "sd",
                        None,
                    ),
                    desc("Correlation coefficient", "", "RHOHV", None),
                    // A CF modifier is not a bare standard name: no URI.
                    desc(
                        "Temperature error",
                        "K",
                        "t_err",
                        Some("air_temperature standard_error"),
                    ),
                ])
            }
            fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
                self.0.get_temporal_extent()
            }
            fn get_spatial_extent(&self) -> Option<[f64; 4]> {
                self.0.get_spatial_extent()
            }
            fn supported_query_types(&self) -> Vec<String> {
                self.0.supported_query_types()
            }
        }

        let engine: Arc<dyn EdrEngine> = Arc::new(DescribedEngine(MockEngine));
        for (uri, path) in [
            ("/collections", "/collections"),
            ("/collections/weather", "/collections/{collectionId}"),
        ] {
            let router = api_edr::router(make_edr_state(engine.clone()));
            let resp = router
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let json: Value =
                serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            let collection = match json.get("collections") {
                Some(list) => &list[0],
                None => &json,
            };
            let names = collection["parameter_names"].as_object().unwrap();
            assert_eq!(names.len(), 5, "{uri}");
            for (name, param) in names {
                let label = param["label"].as_str().unwrap();
                let description = param["description"].as_str().unwrap();
                assert!(label.chars().count() <= 50, "{uri} {name}: {label}");
                assert_ne!(label, description, "{uri} {name}");
            }

            // A CF standard name identifies the property; QUDT names the unit.
            let t2m = &names["t2m"];
            assert_eq!(t2m["label"], "2 metre temperature");
            assert_eq!(t2m["description"], "2 metre temperature, in K");
            assert_eq!(
                t2m["unit"]["symbol"],
                serde_json::json!({"value": "K", "type": "https://qudt.org/vocab/unit/K"})
            );
            assert_eq!(
                t2m["observedProperty"]["id"],
                "https://vocab.nerc.ac.uk/standard_name/air_temperature"
            );
            assert!(t2m["observedProperty"].get("description").is_none());

            // No QUDT unit for radar reflectivity: the UCUM form stays. No CF
            // name: the property is described instead of identified.
            let dbzh = &names["DBZH"];
            assert_eq!(
                dbzh["unit"]["symbol"],
                serde_json::json!({"value": "dBZ", "type": "http://www.opengis.net/def/uom/UCUM/"})
            );
            assert!(dbzh["observedProperty"].get("id").is_none());
            assert_eq!(
                dbzh["observedProperty"]["description"],
                "DBZH — Reflectivity (horizontal), in dBZ"
            );

            // An over-long label is shortened; the description keeps it whole.
            let sd = &names["sd"];
            assert_eq!(sd["label"].as_str().unwrap().chars().count(), 50);
            assert!(sd["label"].as_str().unwrap().ends_with('…'));
            assert_eq!(
                sd["description"],
                "Water equivalent of accumulated snow depth (2 m above ground), in kg/m²"
            );
            assert_eq!(
                sd["observedProperty"]["label"]["en"],
                "Water equivalent of accumulated snow depth (2 m above ground)"
            );
            assert_eq!(
                sd["unit"]["symbol"]["type"],
                "https://qudt.org/vocab/unit/KiloGM-PER-M2"
            );

            // An engine that knows no unit advertises none.
            assert!(names["RHOHV"].get("unit").is_none());
            assert_eq!(
                names["RHOHV"]["description"],
                "Correlation coefficient (unit not specified)"
            );
            assert!(names["t_err"]["observedProperty"].get("id").is_none());

            edr_schema::assert_valid(path, JSON, &json, uri);
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

    /// The collection document, with locations, position, area and radius
    /// queries, validates against EDR 1.1 and 1.2. 1.2 requires each data
    /// query link's `variables` to carry a title, description, output
    /// formats and CRS details (#918).
    #[tokio::test]
    async fn detail_validates_against_edr_bundles() {
        let (_, json) = get("/collections/weather").await;
        let mut query_types: Vec<&str> = json["data_queries"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        query_types.sort_unstable();
        assert_eq!(query_types, ["area", "locations", "position", "radius"]);
        edr_schema::assert_valid("/collections/{collectionId}", JSON, &json, "collection");
    }

    /// Negative control, so the 1.2 check is not vacuous: a data query link
    /// whose `variables` lack the `title` passes EDR 1.1, which does not
    /// require it, and fails EDR 1.2.
    #[tokio::test]
    async fn detail_without_a_link_variable_title_fails_edr_1_2() {
        use edr_schema::Edr;

        let path = "/collections/{collectionId}";
        let (_, mut json) = get("/collections/weather").await;
        assert!(edr_schema::errors(Edr::V1_2, path, JSON, &json).is_empty());
        let variables = json["data_queries"]["radius"]["link"]["variables"]
            .as_object_mut()
            .unwrap();
        assert!(variables.remove("title").is_some());

        assert!(edr_schema::errors(Edr::V1_1, path, JSON, &json).is_empty());
        let errors = edr_schema::errors(Edr::V1_2, path, JSON, &json);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("\"title\" is a required property")),
            "{errors:#?}"
        );
    }

    /// Negative control for the `parameter_names` entry check: the bundles'
    /// own `parameter_names` schema constrains no entry, so the helper checks
    /// each entry against the parameter schema the bundles intend. An entry
    /// without its required `observedProperty` fails both versions.
    #[tokio::test]
    async fn detail_with_a_parameter_without_observed_property_fails() {
        let path = "/collections/{collectionId}";
        let (_, mut json) = get("/collections/weather").await;
        let parameter = json["parameter_names"]["temperature"]
            .as_object_mut()
            .unwrap();
        assert!(parameter.remove("observedProperty").is_some());

        for version in edr_schema::VERSIONS {
            let errors = edr_schema::errors(version, path, JSON, &json);
            assert!(
                errors
                    .iter()
                    .any(|e| e.starts_with("- parameter_names.temperature:")
                        && e.contains("\"observedProperty\" is a required property")),
                "{version:?}: {errors:#?}"
            );
        }
    }

    #[tokio::test]
    async fn collection_omits_nonstandard_apis_field() {
        // `apis` is a vendor extension with no OGC schema; it must not leak
        // into the standard collection JSON.
        let (_, json) = get("/collections/weather").await;
        assert!(
            json.get("apis").is_none(),
            "apis must not be present in the standard collection JSON"
        );
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
                .map(|e| format!("  - {e} (at {})", e.instance_path()))
                .collect();
            panic!(
                "Locations GeoJSON schema validation failed:\n{}\n\nJSON:\n{}",
                msgs.join("\n"),
                serde_json::to_string_pretty(&json).unwrap()
            );
        }
    }

    #[tokio::test]
    async fn validates_against_edr_bundles() {
        let (_, json) = get("/collections/weather/locations").await;
        edr_schema::assert_valid(
            "/collections/{collectionId}/locations",
            GEOJSON,
            &json,
            "locations",
        );
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

    /// A `z` selector against a collection with no vertical extent
    /// (`MockEngine` does not override `get_vertical_extent`) is ignored:
    /// EDR 1.2 `/req/edr/z-response` A says it SHALL be. A malformed `z` is
    /// still a 400.
    #[tokio::test]
    async fn z_against_non_vertical_collection_is_ignored() {
        let (status, json) = get("/collections/weather/locations/helsinki?z=0.5").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["type"], "Coverage");

        let (status, json) = get("/collections/weather/locations/helsinki?z=low").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(json.get("code").is_some(), "400 error must have 'code'");
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
    async fn radius_query() {
        // 10 km around Helsinki: matches Helsinki, not Tampere (~160 km away).
        let (status, json) = get(
            "/collections/weather/radius?coords=POINT%2824.9384%2060.1699%29&within=10&within-units=km",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["type"], "CoverageCollection");
        let coverages = json["coverages"].as_array().unwrap();
        assert_eq!(coverages.len(), 1, "should match Helsinki only");
    }

    #[tokio::test]
    async fn radius_query_units_and_large_radius() {
        // 200 km (in miles) around Helsinki reaches Tampere as well.
        let (status, json) = get(
            "/collections/weather/radius?coords=POINT%2824.9384%2060.1699%29&within=125&within-units=mi",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["coverages"].as_array().unwrap().len(), 2);
        // Bare lon,lat shorthand and metres are accepted too.
        let (status, json) =
            get("/collections/weather/radius?coords=24.9384,60.1699&within=5000&within-units=m")
                .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["coverages"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn radius_query_rejects_bad_input() {
        for (uri, why) in [
            (
                "/collections/weather/radius?coords=POINT%2824.9%2060.2%29&within=10",
                "within-units is required",
            ),
            (
                "/collections/weather/radius?coords=POINT%2824.9%2060.2%29&within-units=km",
                "within is required",
            ),
            (
                "/collections/weather/radius?coords=POINT%2824.9%2060.2%29&within=10&within-units=furlong",
                "unknown unit",
            ),
            (
                "/collections/weather/radius?coords=POINT%2824.9%2060.2%29&within=0&within-units=km",
                "zero radius",
            ),
            (
                "/collections/weather/radius?coords=POINT%2824.9%2060.2%29&within=-1&within-units=km",
                "negative radius",
            ),
            (
                "/collections/weather/radius?coords=POINT%2824.9%2060.2%29&within=1001&within-units=km",
                "radius over the cap",
            ),
            (
                "/collections/weather/radius?coords=POINT%2824.9%2089.9%29&within=100&within-units=km",
                "circle containing the pole",
            ),
            (
                "/collections/weather/radius?coords=MULTIPOINT%28%2824.9%2060.2%29%29&within=10&within-units=km",
                "MULTIPOINT centre",
            ),
            (
                "/collections/weather/radius?coords=POINT%2824.9%2060.2%29&within=10&within-units=km&f=png",
                "PNG output",
            ),
        ] {
            let (status, json) = get_status(uri).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{why}: {uri}");
            if let Some(json) = json {
                assert_eq!(json["code"], "BadRequest", "{why}");
            }
        }
    }

    #[tokio::test]
    async fn radius_query_no_match_is_404() {
        let (status, _) =
            get("/collections/weather/radius?coords=POINT%280%200%29&within=10&within-units=km")
                .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn radius_advertised_in_data_queries_with_units() {
        let (status, json) = get("/collections/weather").await;
        assert_eq!(status, StatusCode::OK);
        let radius = &json["data_queries"]["radius"]["link"];
        assert!(
            radius["href"]
                .as_str()
                .unwrap()
                .ends_with("/collections/weather/radius"),
            "{radius}"
        );
        assert_eq!(radius["variables"]["query_type"], "radius");
        assert_eq!(
            radius["variables"]["within_units"],
            serde_json::json!(["km", "m", "mi"])
        );
        let (_, api) = get("/api").await;
        assert!(api["paths"]["/edr/collections/weather/radius"].is_object());
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
    async fn trajectory_query_unsupported_engine_returns_404() {
        // `MockEngine` does not advertise `trajectory` in
        // `supported_query_types`, so the route must answer 404 (the
        // capability is absent for this collection) rather than 400.
        let (status, json) =
            get("/collections/weather/trajectory?coords=LINESTRING(24%2060,25%2061)").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json["code"], "NotFound");
    }

    /// A trajectory-capable engine routes a real `Section` through the
    /// Axum handler: 200 + CoverageJSON by default, 200 + `image/png` for
    /// `f=PNG`. Guards the handler's content-type and PNG dispatch, which
    /// the 404-only test above can't reach.
    #[tokio::test]
    async fn trajectory_query_returns_section_and_png() {
        use ds_core::model::{NdArray, VerticalCoord};
        use ds_core::vertical::VerticalKind;

        struct TrajectoryMock;
        impl EdrEngine for TrajectoryMock {
            fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
                Ok(vec![])
            }
            fn query_location(
                &self,
                _id: &str,
                _dt: Option<(DateTime<Utc>, DateTime<Utc>)>,
                _p: Option<&[String]>,
                _z: Option<&[f64]>,
                _rt: Option<DateTime<Utc>>,
            ) -> Result<CoverageResponse, DataServerError> {
                Err(DataServerError::LocationNotFound("n/a".into()))
            }
            fn get_parameters(&self) -> Vec<String> {
                vec!["DBZH".into()]
            }
            fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
                None
            }
            fn get_spatial_extent(&self) -> Option<[f64; 4]> {
                Some([24.0, 60.0, 25.0, 61.0])
            }
            fn supported_query_types(&self) -> Vec<String> {
                vec!["trajectory".into()]
            }
            fn trajectory_shape(&self) -> ds_core::edr_engine::TrajectoryShape {
                ds_core::edr_engine::TrajectoryShape::CrossSection
            }
            fn query_trajectory(
                &self,
                _coords: &str,
                _dt: Option<(DateTime<Utc>, DateTime<Utc>)>,
                _p: Option<&[String]>,
                _z: Option<&[f64]>,
                _rt: Option<DateTime<Utc>>,
            ) -> Result<CoverageResponse, DataServerError> {
                let t = "2024-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
                let nodes = vec![(t, 24.0, 60.0), (t, 24.5, 60.5), (t, 25.0, 61.0)];
                let mut parameters = HashMap::new();
                parameters.insert(
                    "DBZH".to_string(),
                    ParameterDescription {
                        label: "Reflectivity".into(),
                        unit: "dBZ".into(),
                        observed_property: "DBZH".into(),
                        standard_name: None,
                    },
                );
                let mut ranges = HashMap::new();
                ranges.insert(
                    "DBZH".to_string(),
                    NdArray {
                        shape: vec![3, 2],
                        axis_names: vec!["composite".into(), "z".into()],
                        values: vec![
                            Some(10.0),
                            Some(20.0),
                            None,
                            Some(15.0),
                            Some(5.0),
                            Some(25.0),
                        ],
                    },
                );
                Ok(CoverageResponse::Single(QueryResult {
                    domain: DomainDescription::Section {
                        nodes,
                        z: VerticalCoord {
                            kind: VerticalKind::HeightAboveAntenna,
                            values: vec![0.0, 1000.0],
                        },
                        coverage_floor: Some(vec![50.0, 420.0, 1150.0]),
                    },
                    parameters,
                    ranges,
                }))
            }
        }

        let engine: Arc<dyn EdrEngine> = Arc::new(TrajectoryMock);
        let router = api_edr::router(make_edr_state(engine));

        // Default → CoverageJSON Section.
        let req = Request::builder()
            .uri("/collections/weather/trajectory?coords=LINESTRING(24%2060,25%2061)")
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/vnd.cov+json")
        );
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["domain"]["domainType"], "Section");

        // f=PNG → image/png with a valid PNG signature.
        let req = Request::builder()
            .uri("/collections/weather/trajectory?coords=LINESTRING(24%2060,25%2061)&f=PNG")
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("image/png")
        );
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[0..8], b"\x89PNG\r\n\x1a\n", "PNG signature");

        // `Accept: image/png` chooses the heatmap among the two offered
        // formats, so the response varies on Accept.
        let req = Request::builder()
            .uri("/collections/weather/trajectory?coords=LINESTRING(24%2060,25%2061)")
            .header("accept", "image/png")
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["content-type"], "image/png");
        assert!(resp
            .headers()
            .get_all("vary")
            .iter()
            .any(|v| v.to_str().unwrap().eq_ignore_ascii_case("accept")));

        // A trajectory is never GeoJSON (#929).
        let req = Request::builder()
            .uri("/collections/weather/trajectory?coords=LINESTRING(24%2060,25%2061)&f=GeoJSON")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// An along-path (gridded) engine, #926: the handler validates the
    /// whole path and the EDR 1.2 Z/`z` and M/`datetime` exclusions before
    /// dispatch, refuses PNG, and serves the `Trajectory` domain; metadata
    /// and `/api` advertise CoverageJSON only and the Z/M/ZM `coords`.
    #[tokio::test]
    async fn along_path_trajectory_validates_before_dispatch() {
        use ds_core::trajectory::{GridSpacing, TrajectoryAxes, TrajectoryPath, TrajectoryPlan};
        use ds_core::vertical::{VerticalDimension, VerticalKind};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct AlongPathMock(Arc<AtomicUsize>);
        fn levels() -> VerticalDimension {
            VerticalDimension::new(VerticalKind::Pressure, vec![1000.0, 850.0, 500.0])
        }
        impl EdrEngine for AlongPathMock {
            fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
                Ok(vec![])
            }
            fn query_location(
                &self,
                _id: &str,
                _dt: Option<(DateTime<Utc>, DateTime<Utc>)>,
                _p: Option<&[String]>,
                _z: Option<&[f64]>,
                _rt: Option<DateTime<Utc>>,
            ) -> Result<CoverageResponse, DataServerError> {
                Err(DataServerError::LocationNotFound("n/a".into()))
            }
            fn get_parameters(&self) -> Vec<String> {
                vec!["TMP".into()]
            }
            fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
                Some((
                    "2024-01-01T00:00:00Z".parse().unwrap(),
                    "2024-01-01T06:00:00Z".parse().unwrap(),
                ))
            }
            fn get_spatial_extent(&self) -> Option<[f64; 4]> {
                Some([20.0, 55.0, 30.0, 65.0])
            }
            fn get_vertical_extent(&self) -> Option<VerticalDimension> {
                Some(levels())
            }
            fn supported_query_types(&self) -> Vec<String> {
                vec!["trajectory".into()]
            }
            fn query_trajectory(
                &self,
                coords: &str,
                datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
                _p: Option<&[String]>,
                z: Option<&[f64]>,
                _rt: Option<DateTime<Utc>>,
            ) -> Result<CoverageResponse, DataServerError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                let path = TrajectoryPath::parse(coords)?;
                // The position-style step selection: every step, or those
                // inside the window (an instant matches exactly).
                let times: Vec<DateTime<Utc>> = [
                    "2024-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap(),
                    "2024-01-01T06:00:00Z".parse().unwrap(),
                ]
                .into_iter()
                .filter(|t| datetime.is_none_or(|(start, end)| *t >= start && *t <= end))
                .collect();
                let vertical = levels();
                let plan = TrajectoryPlan::new(
                    &path,
                    GridSpacing::new(0.5, 0.5).unwrap(),
                    TrajectoryAxes {
                        times: &times,
                        vertical: Some(&vertical),
                        z,
                    },
                    1,
                )?;
                let values = vec![plan
                    .fields()
                    .iter()
                    .map(|f| vec![Some(1.0); f.points.len()])
                    .collect()];
                let parameters = [(
                    "TMP".to_string(),
                    ParameterDescription {
                        label: "Temperature".into(),
                        unit: "°C".into(),
                        observed_property: "TMP".into(),
                        standard_name: None,
                    },
                )];
                plan.into_response(&parameters, &values)
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let engine: Arc<dyn EdrEngine> = Arc::new(AlongPathMock(calls.clone()));
        let router = api_edr::router(make_edr_state(engine));
        let fetch = |uri: String| {
            let router = router.clone();
            async move {
                let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
                let resp = router.oneshot(req).await.unwrap();
                let status = resp.status();
                let body = resp.into_body().collect().await.unwrap().to_bytes();
                (status, serde_json::from_slice::<Value>(&body).unwrap())
            }
        };
        let base = "/collections/weather/trajectory?coords=";

        for query in [
            // A Z path carries its levels: `z` too is an error (EDR 1.2).
            "LINESTRING%20Z(24%2060%20850,25%2061%20500)&z=850",
            "LINESTRING%20ZM(24%2060%20850%201704067200,25%2061%20500%201704088800)&z=850",
            // An M path carries its times: `datetime` too is an error.
            "LINESTRINGM(24%2060%201704067200,25%2061%201704088800)&datetime=2024-01-01T00:00:00Z",
            "LINESTRING%20ZM(24%2060%20850%201704067200,25%2061%20500%201704088800)&datetime=2024-01-01T00:00:00Z",
            // A `datetime` list is a `datetime` too.
            "LINESTRINGM(24%2060%201704067200,25%2061%201704088800)&datetime=2024-01-01T00:00:00Z,2024-01-01T06:00:00Z",
            // Malformed paths never reach the engine.
            "LINESTRING(24%2060)",
            "LINESTRING%20Z(24%2060,25%2061)",
            "MULTILINESTRING((24%2060,25%2061))",
            // Only a radar cross-section renders as a PNG; no trajectory is
            // GeoJSON.
            "LINESTRING(24%2060,25%2061)&f=PNG",
            "LINESTRING(24%2060,25%2061)&f=GeoJSON",
        ] {
            let (status, json) = fetch(format!("{base}{query}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {json}");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0, "rejected before dispatch");

        let (status, json) = fetch(format!(
            "{base}LINESTRING%20Z(24%2060%201000,25%2061%20500)"
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["domainType"], "Trajectory");
        let domain = &json["coverages"][0]["domain"];
        assert_eq!(
            domain["axes"]["composite"]["coordinates"],
            serde_json::json!(["t", "x", "y", "z"])
        );
        // An M path on one level: a single coverage with a `z` axis.
        let (status, json) = fetch(format!(
            "{base}LINESTRING%20M(24%2060%201704067200,25%2061%201704088800)&z=850"
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["type"], "Coverage");
        assert_eq!(
            json["domain"]["axes"]["z"]["values"],
            serde_json::json!([850.0])
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // A 2-D path with a `datetime` list: one engine query per instant,
        // one coverage per instant (EDR 1.2 /req/core/datetime-response D).
        let (status, json) = fetch(format!(
            "{base}LINESTRING(24%2060,25%2061)&z=850\
             &datetime=2024-01-01T00:00:00Z,2024-01-01T06:00:00Z"
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["type"], "CoverageCollection");
        let steps: Vec<&str> = json["coverages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                c["domain"]["axes"]["composite"]["values"][0][0]
                    .as_str()
                    .unwrap()
            })
            .collect();
        assert_eq!(
            steps,
            ["2024-01-01T00:00:00+00:00", "2024-01-01T06:00:00+00:00"]
        );
        assert_eq!(calls.load(Ordering::SeqCst), 4);

        // `Accept: image/png` is not a failure: CoverageJSON, the only
        // format offered along a path, so nothing varies on Accept.
        let req = Request::builder()
            .uri(format!("{base}LINESTRING(24%2060,25%2061)&z=850"))
            .header("accept", "image/png")
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["content-type"], "application/vnd.cov+json");
        assert!(resp.headers().get("vary").is_none_or(|v| !v
            .to_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("accept")));

        let (_, meta) = fetch("/collections/weather".into()).await;
        assert_eq!(
            meta["data_queries"]["trajectory"]["link"]["variables"]["output_formats"],
            serde_json::json!(["CoverageJSON"])
        );
        let (_, api) = fetch("/api".into()).await;
        let op = &api["paths"]["/edr/collections/weather/trajectory"]["get"];
        assert_eq!(
            op["parameters"][0]["$ref"],
            "#/components/parameters/coords-trajectory"
        );
        let coords_doc = api["components"]["parameters"]["coords-trajectory"]["description"]
            .as_str()
            .unwrap();
        assert!(coords_doc.contains("Unix epoch"), "{coords_doc}");
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

/// Keywords + license surface in the EDR collection JSON (the code path is
/// symmetric with Maps/Tiles/Features, but tested here so a regression in this
/// crate's builder is caught — review on PR #324).
mod metadata_extras {
    use super::*;

    fn state_with(
        keywords: Vec<String>,
        license: Option<ds_core::config::LicenseConfig>,
    ) -> Arc<ArcSwap<EdrState>> {
        let mut engines = HashMap::new();
        let mut collections = HashMap::new();
        engines.insert(
            "weather".to_string(),
            Arc::new(MockEngine) as Arc<dyn EdrEngine>,
        );
        collections.insert(
            "weather".to_string(),
            CollectionConfig {
                id: "weather".to_string(),
                title: "Finnish Weather Observations".to_string(),
                description: "Test collection".to_string(),
                data_path: None,
                apis: vec!["edr".to_string()],
                engine_type: "csv".to_string(),
                keywords,
                license,
                geotiff: None,
                querydata: None,
                wms: None,
                grib: None,
                zarr: None,
                odim: None,
                cap: None,
                postgis: None,
                nowcast: None,
                bufr: None,
                satellite: None,
                preview: None,
                derive_wind: None,
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

    async fn collection_json(
        keywords: Vec<String>,
        license: Option<ds_core::config::LicenseConfig>,
    ) -> Value {
        let app = api_edr::router(state_with(keywords, license));
        let req = Request::builder()
            .uri("/collections/weather")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&body).unwrap()
    }

    async fn q_count(q: &str) -> u64 {
        let app = api_edr::router(state_with(vec!["thunderstorm".into()], None));
        let req = Request::builder().uri(q).body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&body).unwrap();
        json["numberMatched"].as_u64().unwrap()
    }

    #[tokio::test]
    async fn keyword_is_matched_by_q_search() {
        // End-to-end guard for config.keywords -> tuple -> CollectionMatch
        // ("thunderstorm" is in neither title nor description).
        assert_eq!(q_count("/collections?q=thunderstorm").await, 1);
        assert_eq!(q_count("/collections?q=zzznotaword").await, 0);
    }

    #[tokio::test]
    async fn keywords_and_license_surface_in_json() {
        let lic = ds_core::config::LicenseConfig {
            title: "CC-BY-4.0".into(),
            url: None,
        };
        let json = collection_json(vec!["radar".into(), "weather".into()], Some(lic)).await;
        assert_eq!(json["keywords"], serde_json::json!(["radar", "weather"]));
        let link = json["links"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["rel"] == "license")
            .expect("a rel=license link");
        assert_eq!(link["href"], "https://spdx.org/licenses/CC-BY-4.0.html");
        assert_eq!(link["title"], "CC-BY-4.0");
    }

    #[tokio::test]
    async fn no_keywords_or_license_when_unset() {
        let json = collection_json(Vec::new(), None).await;
        assert!(json.get("keywords").is_none());
        assert!(json["links"]
            .as_array()
            .unwrap()
            .iter()
            .all(|l| l["rel"] != "license"));
    }
}

mod request_budget {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingEngine {
        calls: Arc<AtomicUsize>,
        values_per_point: usize,
    }
    impl EdrEngine for CountingEngine {
        fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
            MockEngine.get_locations()
        }
        fn get_parameters(&self) -> Vec<String> {
            MockEngine.get_parameters()
        }
        fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
            MockEngine.get_temporal_extent()
        }
        fn get_spatial_extent(&self) -> Option<[f64; 4]> {
            MockEngine.get_spatial_extent()
        }
        fn supported_query_types(&self) -> Vec<String> {
            vec!["position".into()]
        }
        fn query_location(
            &self,
            id: &str,
            dt: Option<(DateTime<Utc>, DateTime<Utc>)>,
            p: Option<&[String]>,
            z: Option<&[f64]>,
            rt: Option<DateTime<Utc>>,
        ) -> Result<CoverageResponse, DataServerError> {
            MockEngine.query_location(id, dt, p, z, rt)
        }
        fn query_position(
            &self,
            _coords: &str,
            _dt: Option<(DateTime<Utc>, DateTime<Utc>)>,
            _p: Option<&[String]>,
            _z: Option<&[f64]>,
            _rt: Option<DateTime<Utc>>,
        ) -> Result<CoverageResponse, DataServerError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let mut result = MockEngine::sample_query_result();
            if self.values_per_point > 0 {
                let range = result.ranges.get_mut("temperature").unwrap();
                range.values = vec![None; self.values_per_point];
                range.shape = vec![self.values_per_point];
            }
            Ok(CoverageResponse::Single(result))
        }
    }

    struct BatchEngine {
        calls: Arc<AtomicUsize>,
        deadline: bool,
    }

    impl EdrEngine for BatchEngine {
        fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
            MockEngine.get_locations()
        }
        fn get_parameters(&self) -> Vec<String> {
            MockEngine.get_parameters()
        }
        fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
            MockEngine.get_temporal_extent()
        }
        fn get_spatial_extent(&self) -> Option<[f64; 4]> {
            MockEngine.get_spatial_extent()
        }
        fn supported_query_types(&self) -> Vec<String> {
            vec!["position".into()]
        }
        fn query_location(
            &self,
            id: &str,
            dt: Option<(DateTime<Utc>, DateTime<Utc>)>,
            p: Option<&[String]>,
            z: Option<&[f64]>,
            rt: Option<DateTime<Utc>>,
        ) -> Result<CoverageResponse, DataServerError> {
            MockEngine.query_location(id, dt, p, z, rt)
        }
        fn query_position(
            &self,
            _: &str,
            _: Option<(DateTime<Utc>, DateTime<Utc>)>,
            _: Option<&[String]>,
            _: Option<&[f64]>,
            _: Option<DateTime<Utc>>,
        ) -> Result<CoverageResponse, DataServerError> {
            panic!("handler bypassed the batch override")
        }
        fn query_positions(
            &self,
            points: &[String],
            _: Option<(DateTime<Utc>, DateTime<Utc>)>,
            _: Option<&[String]>,
            _: Option<&[f64]>,
            _: Option<DateTime<Utc>>,
            emit: &mut dyn FnMut(CoverageResponse) -> Result<(), DataServerError>,
        ) -> Result<(), DataServerError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.deadline {
                return Err(DataServerError::DeadlineExceeded);
            }
            for point in points {
                let (lat, lon) = ds_core::feature::parse_point_coords(point)?;
                let mut result = MockEngine::sample_query_result();
                let DomainDescription::PointSeries { x, y, .. } = &mut result.domain else {
                    panic!()
                };
                *x = lon;
                *y = lat;
                emit(CoverageResponse::Single(result))?;
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn position_handler_uses_one_batch_and_preserves_point_order() {
        for (coords, count) in [("POINT(1%202)", 1), ("MULTIPOINT(1%202,3%204,5%206)", 3)] {
            let calls = Arc::new(AtomicUsize::new(0));
            let app = api_edr::router(make_edr_state(Arc::new(BatchEngine {
                calls: calls.clone(),
                deadline: false,
            })));
            let response = app
                .oneshot(
                    Request::builder()
                        .uri(format!("/collections/weather/position?coords={coords}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: Value = serde_json::from_slice(&body).unwrap();
            if count == 1 {
                assert_eq!(json["type"], "Coverage");
                assert_eq!(json["domain"]["axes"]["x"]["values"][0], 1.0);
            } else {
                assert_eq!(json["type"], "CoverageCollection");
                let coverages = json["coverages"].as_array().unwrap();
                assert_eq!(coverages.len(), count);
                for (i, coverage) in coverages.iter().enumerate() {
                    assert_eq!(
                        coverage["domain"]["axes"]["x"]["values"][0],
                        (i * 2 + 1) as f64
                    );
                    assert_eq!(
                        coverage["domain"]["axes"]["y"]["values"][0],
                        (i * 2 + 2) as f64
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn engine_deadline_maps_to_query_timeout() {
        let app = api_edr::router(make_edr_state(Arc::new(BatchEngine {
            calls: Arc::new(AtomicUsize::new(0)),
            deadline: true,
        })));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/collections/weather/position?coords=POINT(1%202)")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["code"], "Timeout");
    }

    #[tokio::test]
    async fn coordinate_limits_reject_before_any_engine_call() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = api_edr::router(make_edr_state(Arc::new(CountingEngine {
            calls: calls.clone(),
            values_per_point: 0,
        })));
        let max = api_edr::params::MAX_POSITION_POINTS;
        let mut cases = vec![
            format!("MULTIPOINT({})", vec!["1%202"; max + 1].join(",")),
            format!(
                "POINT(1%202){}",
                "%20".repeat(api_edr::params::MAX_POSITION_COORD_BYTES)
            ),
            "POINT(NaN%202)".into(),
            "POINT(1%20inf)".into(),
            "POINT(181%202)".into(),
            "POINT(1%20-91)".into(),
            "MULTIPOINT(1%202,NaN%202)".into(),
        ];
        for coords in cases.drain(..) {
            let request = Request::builder()
                .uri(format!("/collections/weather/position?coords={coords}"))
                .body(Body::empty())
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(calls.load(Ordering::Relaxed), 0);
        }
        let coords = format!("MULTIPOINT({})", vec!["1%202"; max].join(","));
        let request = Request::builder()
            .uri(format!("/collections/weather/position?coords={coords}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
        assert_eq!(calls.load(Ordering::Relaxed), max);
    }

    #[tokio::test]
    async fn response_budget_stops_later_point_queries() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = api_edr::router(make_edr_state(Arc::new(CountingEngine {
            calls: calls.clone(),
            values_per_point: api_edr::params::MAX_POSITION_VALUES / 2 + 1,
        })));
        let request = Request::builder()
            .uri("/collections/weather/position?coords=MULTIPOINT(1%202,3%204,5%206)")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }
}

#[tokio::test]
async fn swagger_docs_and_local_assets_obey_security_policy() {
    // Nest under a prefix to catch asset URLs that only work at the root.
    let app = axum::Router::new().nest("/prefix/service", build_router());
    let docs = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/prefix/service/api/docs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(docs.status(), StatusCode::OK);
    assert_eq!(
        docs.headers()["content-security-policy"],
        ds_core::openapi::SWAGGER_UI_CSP
    );
    assert_eq!(docs.headers()["x-content-type-options"], "nosniff");
    let bytes = axum::body::to_bytes(docs.into_body(), 100_000)
        .await
        .unwrap();
    let html = std::str::from_utf8(&bytes).unwrap();
    assert!(!html.contains("unpkg.com"));
    assert!(!html.contains("<script>"));
    for name in [
        "swagger-ui-5.33.0.js",
        "swagger-ui-5.33.0.css",
        "init.js",
        "layout.css",
    ] {
        assert!(html.contains(&format!("docs/{name}")));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/prefix/service/api/docs/{name}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        let (mime, embedded) = ds_core::openapi::swagger_ui_asset(name).unwrap();
        assert_eq!(response.headers()["content-type"], mime);
        let bytes = axum::body::to_bytes(response.into_body(), 2_000_000)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), embedded);
    }
    let missing = app
        .oneshot(
            Request::builder()
                .uri("/prefix/service/api/docs/not-vendored.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}
