//! Offline response validation against the unmodified, pinned OGC bundles.
//! OpenAPI 3.0 Schema Objects use Draft 4-style constraints plus `nullable`.
use std::{collections::HashMap, sync::LazyLock};

use jsonschema::{Draft, Validator};
use serde_json::{json, Value};

const PATHS: &[&str] = &[
    "/",
    "/conformance",
    "/collections",
    "/collections/{collectionId}",
];
const BUNDLES: &[(u8, &str)] = &[
    (
        2,
        include_str!("../../../../schemas/ogcapi-common-2.bundled.json"),
    ),
    (
        4,
        include_str!("../../../../schemas/ogcapi-common-4.bundled.json"),
    ),
];

struct CommonSchemas {
    part: u8,
    responses: HashMap<&'static str, Validator>,
}

static SCHEMAS: LazyLock<Vec<CommonSchemas>> = LazyLock::new(|| {
    BUNDLES
        .iter()
        .map(|&(part, source)| {
            let bundle: Value = serde_json::from_str(source).expect("pinned Common bundle parses");
            assert_eq!(bundle["openapi"], "3.0.0");
            let responses = PATHS
                .iter()
                .map(|&path| {
                    let response = &bundle["paths"][path]["get"]["responses"]["200"];
                    let reference = response["$ref"]
                        .as_str()
                        .expect("bundled response reference");
                    let response = bundle
                        .pointer(reference.strip_prefix('#').expect("local reference"))
                        .expect("bundled response reference resolves");
                    let body = &response["content"]["application/json"]["schema"];
                    assert!(
                        body.is_object(),
                        "Part {part} {path}: missing JSON response schema"
                    );
                    // Keep the original component paths so nested $refs resolve. Validate
                    // the selected response schema, never the OpenAPI document as a schema.
                    let mut schema = json!({
                        "allOf": [body],
                        "components": {"schemas": bundle["components"]["schemas"]},
                    });
                    normalize_nullable(&mut schema);
                    check_local_refs(&schema, &schema);
                    let validator = jsonschema::options()
                        .with_draft(Draft::Draft4)
                        .should_validate_formats(true)
                        .build(&schema)
                        .unwrap_or_else(|error| panic!("Part {part} {path}: {error}"));
                    (path, validator)
                })
                .collect();
            CommonSchemas { part, responses }
        })
        .collect()
});

// The only dialect conversion needed by these pinned response schemas. Keep
// enum/oneOf/other constraints intact; nullable does not override them in OAS 3.0.
fn normalize_nullable(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if object.remove("nullable") == Some(Value::Bool(true)) {
                if let Some(Value::String(kind)) = object.get("type") {
                    object.insert("type".into(), json!([kind, "null"]));
                }
            }
            for child in object.values_mut() {
                normalize_nullable(child);
            }
        }
        Value::Array(array) => array.iter_mut().for_each(normalize_nullable),
        _ => {}
    }
}

// Fail on missing or external references even in optional schema branches.
// No schema downloads or filesystem resolution are permitted during tests.
fn check_local_refs(value: &Value, root: &Value) {
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref") {
                let reference = reference.as_str().expect("string $ref");
                let pointer = reference
                    .strip_prefix('#')
                    .expect("only local $refs permitted");
                assert!(root.pointer(pointer).is_some(), "unresolved {reference}");
            }
            for child in object.values() {
                check_local_refs(child, root);
            }
        }
        Value::Array(array) => array.iter().for_each(|child| check_local_refs(child, root)),
        _ => {}
    }
}

pub fn assert_valid(path: &str, body: &Value, context: &str) {
    for schemas in SCHEMAS.iter() {
        let errors: Vec<_> = schemas.responses[path]
            .iter_errors(body)
            .map(|error| format!("{} at {}", error, error.instance_path()))
            .collect();
        assert!(
            errors.is_empty(),
            "Common Part {} {context}:\n{}",
            schemas.part,
            errors.join("\n")
        );
    }
}

/// The pinned bundles' `extent` also accepts any object through a general
/// extension branch, so a dimension that breaks the Uniform Additional
/// Dimensions rules still passes `assert_valid`. Validate every extent member
/// besides `spatial`/`temporal` against the UAD dimension schema itself, as a
/// UAD-aware validator does.
static DIMENSIONS: LazyLock<Vec<(u8, Validator)>> = LazyLock::new(|| {
    BUNDLES
        .iter()
        .map(|&(part, source)| {
            let bundle: Value = serde_json::from_str(source).expect("pinned Common bundle parses");
            let dimension = bundle
                .pointer("/components/schemas/extent/allOf/1/anyOf/1/additionalProperties")
                .expect("UAD dimension schema")
                .clone();
            let mut schema = json!({
                "allOf": [dimension],
                "components": {"schemas": bundle["components"]["schemas"]},
            });
            normalize_nullable(&mut schema);
            check_local_refs(&schema, &schema);
            let validator = jsonschema::options()
                .with_draft(Draft::Draft4)
                .build(&schema)
                .unwrap_or_else(|error| panic!("Part {part} UAD dimension: {error}"));
            (part, validator)
        })
        .collect()
});

pub fn assert_dimensions_valid(collection: &Value, context: &str) {
    let Some(extent) = collection["extent"].as_object() else {
        return;
    };
    for (name, dimension) in extent {
        if name == "spatial" || name == "temporal" {
            continue;
        }
        for (part, validator) in DIMENSIONS.iter() {
            let errors: Vec<_> = validator
                .iter_errors(dimension)
                .map(|error| format!("{} at {}", error, error.instance_path()))
                .collect();
            assert!(
                errors.is_empty(),
                "Common Part {part} UAD {context} extent.{name}:\n{}",
                errors.join("\n")
            );
        }
    }
    for bbox in collection["extent"]["spatial"]["bbox"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let b: Vec<f64> = bbox
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap())
            .collect();
        let (lons, lats) = if b.len() == 6 {
            ([b[0], b[3]], [b[1], b[4]])
        } else {
            ([b[0], b[2]], [b[1], b[3]])
        };
        assert!(
            lons.iter().all(|x| (-180.0..=180.0).contains(x))
                && lats.iter().all(|y| (-90.0..=90.0).contains(y))
                && lats[0] <= lats[1],
            "{context}: bbox {b:?} outside the CRS84 domain"
        );
    }
}

pub fn assert_invalid(path: &str, body: &Value, context: &str) {
    for schemas in SCHEMAS.iter() {
        assert!(
            !schemas.responses[path].is_valid(body),
            "Common Part {} accepted {context}",
            schemas.part
        );
    }
}
