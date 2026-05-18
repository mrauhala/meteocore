use std::collections::HashMap;

use chrono::{DateTime, Utc};
use jsonschema::Validator;
use serde_json::Value;

use api_edr::response::{area_query_result_to_json, query_result_to_coverage_json};
use ds_core::model::*;

fn load_schema() -> Value {
    let schema_str = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../schemas/coveragejson.json"
    ))
    .expect("Failed to read CoverageJSON schema");
    serde_json::from_str(&schema_str).expect("Failed to parse schema JSON")
}

fn validate(json: &Value, schema: &Value) {
    let validator = Validator::new(schema).expect("Failed to compile schema");
    let errors: Vec<_> = validator.iter_errors(json).collect();
    if !errors.is_empty() {
        let error_messages: Vec<String> = errors
            .iter()
            .map(|e| format!("  - {e} (at {})", e.instance_path))
            .collect();
        panic!(
            "CoverageJSON schema validation failed:\n{}\n\nJSON:\n{}",
            error_messages.join("\n"),
            serde_json::to_string_pretty(json).unwrap()
        );
    }
}

fn make_time(hour: u32) -> DateTime<Utc> {
    format!("2024-01-01T{hour:02}:00:00Z").parse().unwrap()
}

fn make_query_result(
    times: Vec<DateTime<Utc>>,
    params: Vec<(&str, &str, Vec<Option<f64>>)>,
) -> QueryResult {
    let mut parameters = HashMap::new();
    let mut ranges = HashMap::new();

    for (name, unit, values) in &params {
        parameters.insert(
            name.to_string(),
            ParameterDescription {
                label: name.replace('_', " "),
                unit: unit.to_string(),
                observed_property: name.to_string(),
            },
        );
        ranges.insert(
            name.to_string(),
            NdArray {
                shape: vec![times.len()],
                axis_names: vec!["t".to_string()],
                values: values.clone(),
            },
        );
    }

    QueryResult {
        domain: DomainDescription::PointSeries {
            x: 24.9384,
            y: 60.1699,
            t: times,
        },
        parameters,
        ranges,
    }
}

#[test]
fn coverage_with_multiple_params_validates() {
    let schema = load_schema();
    let times: Vec<DateTime<Utc>> = (0..7).map(make_time).collect();
    let result = make_query_result(
        times,
        vec![
            (
                "temperature",
                "°C",
                vec![
                    Some(-2.5),
                    Some(-2.8),
                    Some(-3.1),
                    Some(-3.0),
                    Some(-2.9),
                    Some(-2.7),
                    Some(-2.5),
                ],
            ),
            (
                "humidity",
                "%",
                vec![
                    Some(85.0),
                    Some(86.0),
                    Some(87.0),
                    Some(86.5),
                    Some(85.5),
                    Some(84.0),
                    Some(83.0),
                ],
            ),
            (
                "wind_speed",
                "m/s",
                vec![
                    Some(3.2),
                    Some(3.5),
                    Some(3.8),
                    Some(4.1),
                    Some(4.5),
                    Some(4.2),
                    Some(3.9),
                ],
            ),
        ],
    );
    let json = query_result_to_coverage_json(&result);
    validate(&json, &schema);
}

/// The ODIM polar-volume engine emits multi-quantity `PointSeries`
/// coverages with **blank units** (ODIM moment groups carry no unit
/// attribute). Confirm that shape — distinct from the populated-unit
/// case above — still validates.
#[test]
fn coverage_with_blank_units_validates() {
    let schema = load_schema();
    let times: Vec<DateTime<Utc>> = (0..3).map(make_time).collect();
    let result = make_query_result(
        times,
        vec![
            ("DBZH", "", vec![Some(12.5), None, Some(18.0)]),
            ("VRADH", "", vec![Some(-4.2), Some(-3.8), None]),
        ],
    );
    let json = query_result_to_coverage_json(&result);
    validate(&json, &schema);
}

#[test]
fn coverage_with_single_param_validates() {
    let schema = load_schema();
    let times = vec![make_time(0), make_time(1), make_time(2)];
    let result = make_query_result(
        times,
        vec![(
            "temperature",
            "°C",
            vec![Some(-2.5), Some(-2.8), Some(-3.1)],
        )],
    );
    let json = query_result_to_coverage_json(&result);
    validate(&json, &schema);
}

#[test]
fn coverage_with_null_values_validates() {
    let schema = load_schema();
    let times = vec![make_time(0), make_time(1), make_time(2)];
    let result = make_query_result(
        times,
        vec![("temperature", "°C", vec![Some(-2.5), None, Some(-3.1)])],
    );
    let json = query_result_to_coverage_json(&result);
    validate(&json, &schema);
}

#[test]
fn coverage_with_single_timestep_validates() {
    let schema = load_schema();
    let times = vec![make_time(0)];
    let result = make_query_result(times, vec![("pressure", "hPa", vec![Some(1013.25)])]);
    let json = query_result_to_coverage_json(&result);
    validate(&json, &schema);
}

#[test]
fn coverage_with_all_nulls_validates() {
    let schema = load_schema();
    let times = vec![make_time(0), make_time(1)];
    let result = make_query_result(times, vec![("temperature", "°C", vec![None, None])]);
    let json = query_result_to_coverage_json(&result);
    validate(&json, &schema);
}

#[test]
fn coverage_required_fields_present() {
    let schema = load_schema();
    let times = vec![make_time(0)];
    let result = make_query_result(times, vec![("temperature", "°C", vec![Some(20.0)])]);
    let json = query_result_to_coverage_json(&result);

    // Verify top-level required fields
    assert_eq!(json["type"], "Coverage");
    assert!(json["domain"].is_object(), "domain must be present");
    assert!(json["parameters"].is_object(), "parameters must be present");
    assert!(json["ranges"].is_object(), "ranges must be present");

    // Verify domain structure
    let domain = &json["domain"];
    assert_eq!(domain["type"], "Domain");
    assert_eq!(domain["domainType"], "PointSeries");
    assert!(domain["axes"].is_object());
    assert!(domain["referencing"].is_array());

    // Verify PointSeries axes
    let axes = &domain["axes"];
    assert!(axes["x"]["values"].is_array());
    assert!(axes["y"]["values"].is_array());
    assert!(axes["t"]["values"].is_array());
    assert_eq!(
        axes["x"]["values"].as_array().unwrap().len(),
        1,
        "x must be single-value for PointSeries"
    );
    assert_eq!(
        axes["y"]["values"].as_array().unwrap().len(),
        1,
        "y must be single-value for PointSeries"
    );

    // Verify referencing systems
    let refs = domain["referencing"].as_array().unwrap();
    assert_eq!(refs.len(), 2);
    assert_eq!(refs[1]["system"]["type"], "TemporalRS");
    assert_eq!(refs[1]["system"]["calendar"], "Gregorian");

    // Verify NdArray structure
    let temp_range = &json["ranges"]["temperature"];
    assert_eq!(temp_range["type"], "NdArray");
    assert_eq!(temp_range["dataType"], "float");
    assert!(temp_range["shape"].is_array());
    assert!(temp_range["axisNames"].is_array());
    assert!(temp_range["values"].is_array());

    // Verify parameter structure
    let param = &json["parameters"]["temperature"];
    assert_eq!(param["type"], "Parameter");
    assert!(param["observedProperty"].is_object());
    assert!(param["observedProperty"]["label"].is_object());

    validate(&json, &schema);
}

#[test]
fn coverage_ndarray_shape_matches_values() {
    let times = vec![make_time(0), make_time(1), make_time(2), make_time(3)];
    let result = make_query_result(
        times,
        vec![(
            "temperature",
            "°C",
            vec![Some(1.0), Some(2.0), Some(3.0), Some(4.0)],
        )],
    );
    let json = query_result_to_coverage_json(&result);

    let range = &json["ranges"]["temperature"];
    let shape: Vec<usize> = range["shape"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as usize)
        .collect();
    let values_len = range["values"].as_array().unwrap().len();
    let expected_len: usize = shape.iter().product();
    assert_eq!(
        values_len, expected_len,
        "values length must equal product of shape dimensions"
    );
}

#[test]
fn coverage_time_axis_matches_values_count() {
    let times = vec![make_time(0), make_time(1), make_time(2)];
    let result = make_query_result(
        times,
        vec![("temperature", "°C", vec![Some(1.0), Some(2.0), Some(3.0)])],
    );
    let json = query_result_to_coverage_json(&result);

    let t_values = json["domain"]["axes"]["t"]["values"].as_array().unwrap();
    let range_values = json["ranges"]["temperature"]["values"].as_array().unwrap();
    assert_eq!(
        t_values.len(),
        range_values.len(),
        "time axis values count must match range values count for 1D PointSeries"
    );
}

// --- Grid domain tests ---

fn make_grid_query_result(
    x: Vec<f64>,
    y: Vec<f64>,
    t: Option<Vec<DateTime<Utc>>>,
    params: Vec<(&str, &str, Vec<Option<f64>>)>,
) -> QueryResult {
    let mut parameters = HashMap::new();
    let mut ranges = HashMap::new();

    for (name, unit, values) in &params {
        parameters.insert(
            name.to_string(),
            ParameterDescription {
                label: name.replace('_', " "),
                unit: unit.to_string(),
                observed_property: name.to_string(),
            },
        );

        let (shape, axis_names) = match &t {
            Some(times) => (
                vec![times.len(), y.len(), x.len()],
                vec!["t".to_string(), "y".to_string(), "x".to_string()],
            ),
            None => (
                vec![y.len(), x.len()],
                vec!["y".to_string(), "x".to_string()],
            ),
        };

        ranges.insert(
            name.to_string(),
            NdArray {
                shape,
                axis_names,
                values: values.clone(),
            },
        );
    }

    QueryResult {
        domain: DomainDescription::Grid { x, y, t },
        parameters,
        ranges,
    }
}

#[test]
fn grid_coverage_without_time_validates() {
    let schema = load_schema();
    let x = vec![10.0, 10.5, 11.0];
    let y = vec![60.0, 60.5];
    // 2 rows * 3 cols = 6 values
    let values: Vec<Option<f64>> = vec![
        Some(1.0),
        Some(2.0),
        Some(3.0),
        Some(4.0),
        Some(5.0),
        Some(6.0),
    ];
    let result = make_grid_query_result(x, y, None, vec![("temperature", "K", values)]);
    let json = query_result_to_coverage_json(&result);
    validate(&json, &schema);
}

#[test]
fn grid_coverage_with_time_validates() {
    let schema = load_schema();
    let x = vec![10.0, 10.5];
    let y = vec![60.0, 60.5];
    let t = vec![make_time(0), make_time(1)];
    // 2 times * 2 rows * 2 cols = 8 values
    let values: Vec<Option<f64>> = vec![
        Some(1.0),
        Some(2.0),
        Some(3.0),
        Some(4.0),
        Some(5.0),
        Some(6.0),
        Some(7.0),
        Some(8.0),
    ];
    let result = make_grid_query_result(x, y, Some(t), vec![("reflectivity", "dBZ", values)]);
    let json = query_result_to_coverage_json(&result);
    validate(&json, &schema);
}

#[test]
fn grid_coverage_with_nulls_validates() {
    let schema = load_schema();
    let x = vec![10.0, 10.5, 11.0];
    let y = vec![60.0, 60.5];
    let values: Vec<Option<f64>> = vec![Some(1.0), None, Some(3.0), None, Some(5.0), None];
    let result = make_grid_query_result(x, y, None, vec![("temperature", "K", values)]);
    let json = query_result_to_coverage_json(&result);
    validate(&json, &schema);
}

#[test]
fn grid_ndarray_shape_matches_values() {
    let x = vec![10.0, 10.5, 11.0];
    let y = vec![60.0, 60.5];
    let t = vec![make_time(0), make_time(1), make_time(2)];
    // 3 times * 2 rows * 3 cols = 18 values
    let values: Vec<Option<f64>> = (0..18).map(|i| Some(i as f64)).collect();
    let result = make_grid_query_result(x, y, Some(t), vec![("temperature", "K", values)]);
    let json = query_result_to_coverage_json(&result);

    let range = &json["ranges"]["temperature"];
    let shape: Vec<usize> = range["shape"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as usize)
        .collect();
    let values_len = range["values"].as_array().unwrap().len();
    let expected_len: usize = shape.iter().product();
    assert_eq!(values_len, expected_len);
}

#[test]
fn grid_domain_structure() {
    let x = vec![10.0, 10.5];
    let y = vec![60.0, 60.5];
    let result = make_grid_query_result(
        x.clone(),
        y.clone(),
        None,
        vec![(
            "temperature",
            "K",
            vec![Some(1.0), Some(2.0), Some(3.0), Some(4.0)],
        )],
    );
    let json = query_result_to_coverage_json(&result);

    let domain = &json["domain"];
    assert_eq!(domain["type"], "Domain");
    assert_eq!(domain["domainType"], "Grid");

    let axes = &domain["axes"];
    let x_vals: Vec<f64> = axes["x"]["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();
    let y_vals: Vec<f64> = axes["y"]["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();
    assert_eq!(x_vals, x);
    assert_eq!(y_vals, y);
    assert!(axes.get("t").is_none() || axes["t"].is_null());

    // Referencing should have spatial but no temporal
    let refs = domain["referencing"].as_array().unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0]["system"]["type"], "GeographicCRS");
}

// --- CoverageCollection tests ---

#[test]
fn coverage_collection_validates() {
    let schema = load_schema();
    let times = vec![make_time(0), make_time(1), make_time(2)];

    let coverages = vec![
        make_query_result(
            times.clone(),
            vec![(
                "temperature",
                "°C",
                vec![Some(-2.5), Some(-2.8), Some(-3.1)],
            )],
        ),
        make_query_result(
            times.clone(),
            vec![("temperature", "°C", vec![Some(1.0), Some(1.5), Some(2.0)])],
        ),
    ];

    let result = AreaQueryResult::Collection(coverages);
    let json = area_query_result_to_json(&result);

    assert_eq!(json["type"], "CoverageCollection");
    assert_eq!(json["domainType"], "PointSeries");
    assert!(json["parameters"].is_object());
    assert!(json["referencing"].is_array());
    let items = json["coverages"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    for item in items {
        assert_eq!(item["type"], "Coverage");
        assert!(item["domain"].is_object());
        assert!(item["ranges"].is_object());
    }

    validate(&json, &schema);
}

#[test]
fn coverage_collection_single_station_validates() {
    let schema = load_schema();
    let times = vec![make_time(0), make_time(1)];

    let coverages = vec![make_query_result(
        times,
        vec![
            ("temperature", "°C", vec![Some(5.0), Some(6.0)]),
            ("humidity", "%", vec![Some(80.0), Some(82.0)]),
        ],
    )];

    let result = AreaQueryResult::Collection(coverages);
    let json = area_query_result_to_json(&result);
    validate(&json, &schema);
}

#[test]
fn coverage_collection_empty_validates() {
    let schema = load_schema();
    let result = AreaQueryResult::Collection(vec![]);
    let json = area_query_result_to_json(&result);
    assert_eq!(json["type"], "CoverageCollection");
    assert!(json["coverages"].as_array().unwrap().is_empty());
    validate(&json, &schema);
}

#[test]
fn area_query_single_result_validates() {
    let schema = load_schema();
    let x = vec![10.0, 10.5, 11.0];
    let y = vec![60.0, 60.5];
    let values: Vec<Option<f64>> =
        vec![Some(1.0), Some(2.0), Some(3.0), Some(4.0), None, Some(6.0)];
    let qr = make_grid_query_result(x, y, None, vec![("reflectivity", "dBZ", values)]);
    let result = AreaQueryResult::Single(qr);
    let json = area_query_result_to_json(&result);
    assert_eq!(json["type"], "Coverage");
    validate(&json, &schema);
}
