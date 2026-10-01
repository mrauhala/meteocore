//! OGC API - EDR `cube` queries (#925), against a mock engine that records
//! the request it receives: discovery (`data_queries`, `/api`), parameter
//! validation, the 404 for collections without cube, instance routing and
//! the CoverageJSON the route serves.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::{DateTime, TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::util::ServiceExt;

use api_edr::handlers::EdrState;
use ds_core::config::CollectionConfig;
use ds_core::cube::CubeResolution;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::Bbox;
use ds_core::instances::RunInfo;
use ds_core::model::*;
use ds_core::vertical::{VerticalDimension, VerticalKind};

const LEVELS: [f64; 5] = [1000.0, 850.0, 700.0, 500.0, 250.0];

fn run() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 7, 0, 0, 0).unwrap()
}

/// One recorded `query_cube` call.
#[derive(Debug, Clone, PartialEq)]
struct Call {
    bbox: [f64; 4],
    datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
    parameters: Option<Vec<String>>,
    z: Option<Vec<f64>>,
    resolution: CubeResolution,
    reference_time: Option<DateTime<Utc>>,
}

/// A forecast engine on pressure levels, with (`cube`) or without cube
/// support, with (`vertical`) or without a vertical axis.
struct CubeMock {
    cube: bool,
    vertical: bool,
    calls: Mutex<Vec<Call>>,
}

impl CubeMock {
    fn new(cube: bool, vertical: bool) -> Arc<Self> {
        Arc::new(Self {
            cube,
            vertical,
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

impl EdrEngine for CubeMock {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(vec![])
    }

    fn get_instances(&self) -> Vec<RunInfo> {
        vec![RunInfo {
            reference_time: run(),
            valid_times: vec![run(), run() + chrono::Duration::hours(6)],
        }]
    }

    fn has_instances(&self) -> bool {
        true
    }

    fn query_location(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Err(DataServerError::InvalidParameter("no locations".into()))
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["temperature".to_string()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((run(), run() + chrono::Duration::hours(6)))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([-180.0, -90.0, 180.0, 90.0])
    }

    fn get_vertical_extent(&self) -> Option<VerticalDimension> {
        self.vertical
            .then(|| VerticalDimension::new(VerticalKind::Pressure, LEVELS.to_vec()))
    }

    fn supported_query_types(&self) -> Vec<String> {
        let mut types = vec!["position".to_string()];
        if self.cube {
            types.push("cube".to_string());
        }
        types
    }

    fn query_cube(
        &self,
        bbox: &Bbox,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        resolution: CubeResolution,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.calls.lock().unwrap().push(Call {
            bbox: [bbox.west, bbox.south, bbox.east, bbox.north],
            datetime,
            parameters: parameters.map(<[String]>::to_vec),
            z: z.map(<[f64]>::to_vec),
            resolution,
            reference_time,
        });
        if reference_time.is_some_and(|rt| rt != run()) {
            return Err(DataServerError::ReferenceTimeNotFound("no such run".into()));
        }
        let (nx, ny) = (resolution.x.unwrap_or(2), resolution.y.unwrap_or(2));
        let levels = z.map_or(LEVELS.to_vec(), <[f64]>::to_vec);
        ds_core::cube::check_cube_budget(1, levels.len(), ny, nx, 1)?;
        let values = (0..levels.len() * ny * nx)
            .map(|i| (i % 7 != 3).then_some(i as f64))
            .collect();
        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::Grid {
                x: ds_core::cube::axis_positions(bbox.west, bbox.east, nx),
                y: ds_core::cube::axis_positions(bbox.south, bbox.north, ny),
                t: Some(vec![run()]),
                z: Some(VerticalCoord {
                    kind: VerticalKind::Pressure,
                    values: levels.clone(),
                }),
            },
            parameters: HashMap::from([(
                "temperature".to_string(),
                ParameterDescription {
                    label: "Temperature".to_string(),
                    unit: "K".to_string(),
                    observed_property: "temperature".to_string(),
                    standard_name: None,
                },
            )]),
            ranges: HashMap::from([(
                "temperature".to_string(),
                NdArray {
                    shape: vec![1, levels.len(), ny, nx],
                    axis_names: ["t", "z", "y", "x"].map(String::from).to_vec(),
                    values,
                },
            )]),
        }))
    }
}

fn config(id: &str) -> CollectionConfig {
    CollectionConfig {
        id: id.to_string(),
        title: format!("{id} title"),
        description: String::new(),
        data_path: None,
        apis: vec!["edr".to_string()],
        engine_type: "grib".to_string(),
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
    }
}

fn app(engines: &[(&str, Arc<CubeMock>)]) -> axum::Router {
    let state = EdrState {
        engines: engines
            .iter()
            .map(|(id, e)| (id.to_string(), e.clone() as Arc<dyn EdrEngine>))
            .collect(),
        collections: engines
            .iter()
            .map(|(id, _)| (id.to_string(), config(id)))
            .collect(),
        styles: HashMap::new(),
        base_url: String::new(),
        trust_proxy_headers: false,
    };
    api_edr::router(Arc::new(ArcSwap::from_pointee(state)))
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Option<String>, Value) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_string());
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        content_type,
        serde_json::from_slice(&body).unwrap_or(Value::Null),
    )
}

fn schema(name: &str) -> Value {
    let path = format!("{}/../../schemas/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn assert_valid(schema: &Value, doc: &Value) {
    let validator = jsonschema::Validator::new(schema).unwrap();
    let errors: Vec<String> = validator
        .iter_errors(doc)
        .map(|e| format!("- {e} (at {})", e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "{}\n{}",
        errors.join("\n"),
        serde_json::to_string_pretty(doc).unwrap()
    );
}

#[tokio::test]
async fn cube_is_a_data_query_with_height_units_on_the_collection_and_its_runs() {
    let app = app(&[("model", CubeMock::new(true, true))]);
    let (status, _, doc) = get(&app, "/collections/model").await;
    assert_eq!(status, StatusCode::OK);
    let link = &doc["data_queries"]["cube"]["link"];
    assert_eq!(link["href"], "/edr/collections/model/cube");
    assert_eq!(link["rel"], "data");
    let variables = &link["variables"];
    assert_eq!(variables["query_type"], "cube");
    assert_eq!(variables["output_formats"], json!(["CoverageJSON"]));
    assert_eq!(variables["default_output_format"], "CoverageJSON");
    assert_eq!(variables["height_units"], json!(["hPa"]));
    // EDR 1.2 link variables every query carries (#918).
    assert_eq!(variables["title"], "Cube query");
    assert!(variables["description"]
        .as_str()
        .is_some_and(|d| !d.is_empty()));
    assert_eq!(variables["crs_details"][0]["crs"], "CRS84");
    let bundle = schema("ogcapi-edr-1.1-bundled.json");
    assert_valid(
        &bundle["paths"]["/collections/{collectionId}"]["get"]["responses"]["200"]["content"]
            ["application/json"]["schema"],
        &doc,
    );

    let (status, _, instance) = get(&app, "/collections/model/instances/20260607T0000Z").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        instance["data_queries"]["cube"]["link"]["href"],
        "/edr/collections/model/instances/20260607T0000Z/cube"
    );
}

#[tokio::test]
async fn api_documents_cube_with_the_standard_parameters() {
    let app = app(&[
        ("model", CubeMock::new(true, true)),
        ("plain", CubeMock::new(false, true)),
    ]);
    let (status, _, api) = get(&app, "/api").await;
    assert_eq!(status, StatusCode::OK);
    assert_valid(&schema("openapi-3.0.json"), &api);
    let resolve = |p: &Value| -> Value {
        match p["$ref"].as_str() {
            Some(r) => api.pointer(r.strip_prefix('#').unwrap()).unwrap().clone(),
            None => p.clone(),
        }
    };
    for path in [
        "/edr/collections/model/cube",
        "/edr/collections/model/instances/{instanceId}/cube",
    ] {
        let operation = &api["paths"][path]["get"];
        assert_eq!(operation["tags"], json!(["model"]), "{path}");
        let params: Vec<Value> = operation["parameters"]
            .as_array()
            .unwrap_or_else(|| panic!("no operation at {path}"))
            .iter()
            .map(resolve)
            .collect();
        let names: Vec<&str> = params
            .iter()
            .filter(|p| p["in"] == "query")
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, api_edr::params::CUBE_PARAMETERS, "{path}");
        let param = |name: &str| params.iter().find(|p| p["name"] == name).unwrap();
        // EDR 1.2 `cube-bbox`: required, four or six numbers, form, not exploded.
        let bbox = param("bbox");
        assert_eq!(bbox["required"], true);
        assert_eq!(
            (&bbox["style"], &bbox["explode"]),
            (&json!("form"), &json!(false))
        );
        let lengths: Vec<_> = bbox["schema"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["minItems"].clone(), s["maxItems"].clone()))
            .collect();
        assert_eq!(lengths, [(json!(4), json!(4)), (json!(6), json!(6))]);
        for name in ["resolution-x", "resolution-y", "resolution-z"] {
            let p = param(name);
            assert_eq!(p["schema"], json!({"type": "string"}), "{name}");
            assert_eq!(
                (&p["style"], &p["explode"]),
                (&json!("form"), &json!(false))
            );
        }
        assert_eq!(param("z")["schema"], json!({"type": "string"}));
        assert_eq!(param("crs")["example"], api_edr::params::CRS84);
    }
    for path in [
        "/edr/collections/plain/cube",
        "/edr/collections/plain/instances/{instanceId}/cube",
    ] {
        assert!(api["paths"].get(path).is_none(), "{path}");
    }
}

#[tokio::test]
async fn collections_without_cube_have_no_cube_resource() {
    let engine = CubeMock::new(false, true);
    let app = app(&[("plain", engine.clone())]);
    let (_, _, doc) = get(&app, "/collections/plain").await;
    assert!(doc["data_queries"].get("cube").is_none());
    for uri in [
        "/collections/plain/cube?bbox=0,0,1,1",
        "/collections/plain/instances/20260607T0000Z/cube?bbox=0,0,1,1",
        // Before any parameter validation: the resource does not exist.
        "/collections/plain/cube?nonsense=1",
    ] {
        let (status, _, body) = get(&app, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(body["code"], "NotFound");
    }
    assert!(engine.calls().is_empty());
}

#[tokio::test]
async fn cube_passes_the_validated_request_and_serves_valid_coveragejson() {
    let engine = CubeMock::new(true, true);
    let app = app(&[("model", engine.clone())]);
    let (status, content_type, body) = get(
        &app,
        "/collections/model/cube?bbox=20,55,30,65&z=850,500&datetime=2026-06-07T00:00:00Z\
         &parameter-name=temperature&resolution-x=10&resolution-y=0&resolution-z=2\
         &crs=CRS84&f=CoverageJSON",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(content_type.as_deref(), Some("application/vnd.cov+json"));
    assert_eq!(
        engine.calls(),
        [Call {
            bbox: [20.0, 55.0, 30.0, 65.0],
            datetime: Some((run(), run())),
            parameters: Some(vec!["temperature".into()]),
            z: Some(vec![850.0, 500.0]),
            resolution: CubeResolution {
                x: Some(10),
                y: None,
                z: Some(2),
            },
            reference_time: None,
        }]
    );
    assert_eq!(body["domain"]["domainType"], "Grid");
    assert_eq!(body["domain"]["axes"]["z"]["values"], json!([850.0, 500.0]));
    assert_eq!(body["ranges"]["temperature"]["shape"], json!([1, 2, 2, 10]));
    assert_eq!(
        body["ranges"]["temperature"]["axisNames"],
        json!(["t", "z", "y", "x"])
    );
    assert_valid(&schema("coveragejson.json"), &body);
}

#[tokio::test]
async fn z_comes_from_the_parameter_or_a_six_number_bbox() {
    let engine = CubeMock::new(true, true);
    let app = app(&[("model", engine.clone())]);
    for (query, z) in [
        // All levels when neither gives one.
        ("bbox=0,0,1,1", None),
        // The bbox's vertical pair is an interval of the advertised levels.
        (
            "bbox=0,0,500,1,1,1000",
            Some(vec![1000.0, 850.0, 700.0, 500.0]),
        ),
        // An explicit z overrides it.
        ("bbox=0,0,500,1,1,1000&z=250", Some(vec![250.0])),
        ("bbox=0,0,1,1&z=700/300", Some(vec![700.0, 500.0])),
    ] {
        let (status, _, body) = get(&app, &format!("/collections/model/cube?{query}")).await;
        assert_eq!(status, StatusCode::OK, "{query}: {body}");
        assert_eq!(engine.calls().last().unwrap().z, z, "{query}");
    }
}

#[tokio::test]
async fn an_antimeridian_bbox_reaches_the_engine_as_given() {
    let engine = CubeMock::new(true, true);
    let app = app(&[("model", engine.clone())]);
    let (status, _, body) = get(&app, "/collections/model/cube?bbox=170,10,-170,20").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(engine.calls()[0].bbox, [170.0, 10.0, -170.0, 20.0]);
    assert_eq!(
        body["domain"]["axes"]["x"]["values"],
        json!([170.0, -170.0])
    );
}

#[tokio::test]
async fn invalid_cube_requests_are_400_before_the_engine_runs() {
    let engine = CubeMock::new(true, true);
    let flat = CubeMock::new(true, false);
    let app = app(&[("model", engine.clone()), ("flat", flat.clone())]);
    for (uri, needle) in [
        ("/collections/model/cube", "require a bbox"),
        ("/collections/model/cube?bbox=0,0,1", "4 numbers"),
        (
            "/collections/model/cube?bbox=0,0,1,x",
            "not a finite number",
        ),
        (
            "/collections/model/cube?bbox=0,0,1,1&coords=POINT(0%200)",
            "accepted: bbox, z",
        ),
        (
            "/collections/model/cube?bbox=0,0,1,1&z=850&z=500",
            "more than once",
        ),
        (
            "/collections/model/cube?bbox=0,0,1,1&crs=EPSG:4326",
            "CRS84",
        ),
        ("/collections/model/cube?bbox=0,0,1,1&f=PNG", "PNG"),
        (
            "/collections/model/cube?bbox=0,0,1,1&f=GeoJSON",
            "Unsupported output format",
        ),
        (
            "/collections/model/cube?bbox=0,0,1,1&resolution-x=-1",
            "from 0",
        ),
        (
            "/collections/model/cube?bbox=0,0,1,1&resolution-y=2.5",
            "resolution-y",
        ),
        (
            "/collections/model/cube?bbox=0,0,1,1&z=10/20",
            "selects none",
        ),
        (
            "/collections/model/cube?bbox=0,0,1,1&datetime=yesterday",
            "",
        ),
        (
            "/collections/flat/cube?bbox=0,0,1,1&z=850",
            "no vertical dimension",
        ),
        (
            "/collections/flat/cube?bbox=0,0,850,1,1,500",
            "six-number bbox",
        ),
        (
            "/collections/flat/cube?bbox=0,0,1,1&resolution-z=3",
            "resolution-z",
        ),
    ] {
        let (status, _, body) = get(&app, uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
        assert_eq!(body["code"], "BadRequest", "{uri}");
        let description = body["description"].as_str().unwrap();
        assert!(description.contains(needle), "{uri}: {description}");
    }
    assert!(engine.calls().is_empty());
    assert!(flat.calls().is_empty());
    // Engine budget errors are 400 as for area.
    let (status, _, body) = get(
        &app,
        "/collections/model/cube?bbox=0,0,1,1&resolution-x=1000000&resolution-y=2",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["description"]
        .as_str()
        .unwrap()
        .contains("Cube query would return"));
}

#[tokio::test]
async fn instance_cube_selects_that_run() {
    let engine = CubeMock::new(true, true);
    let app = app(&[("model", engine.clone())]);
    let (status, _, body) = get(
        &app,
        "/collections/model/instances/20260607T0000Z/cube?bbox=0,0,1,1",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(engine.calls()[0].reference_time, Some(run()));
    for (uri, status) in [
        (
            "/collections/model/instances/20260607T1200Z/cube?bbox=0,0,1,1",
            StatusCode::NOT_FOUND,
        ),
        (
            "/collections/model/instances/not-a-run/cube?bbox=0,0,1,1",
            StatusCode::BAD_REQUEST,
        ),
    ] {
        assert_eq!(get(&app, uri).await.0, status, "{uri}");
    }
}
