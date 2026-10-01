//! Offline response validation against the pinned, unmodified OGC API - EDR
//! OpenAPI 3.0 bundles (`schemas/README.md`): 1.1, whose schemas are inlined,
//! and 1.2, which puts them in `components` behind `$ref`. Shared by the
//! api-edr suites and `crates/server/tests/satellite_edr.rs`; each binary uses
//! a subset.
//!
//! The approach mirrors `crates/server/tests/common_discovery/schema.rs`:
//! select a response schema by the bundle's own resource path and media type,
//! compile it next to the whole `components` object so nested references
//! resolve, fail on missing or external references, validate formats, and use
//! JSON Schema Draft 4 with OpenAPI `nullable: true` converted in memory. The
//! checked-in bytes are never rewritten. Each `parameter_names` entry is also
//! checked against the bundle's parameter schema, which the bundle's own
//! `parameter_names` schema never applies (see `parameter_schema`).
//!
//! CoverageJSON data responses are validated against `schemas/coveragejson.json`
//! instead: the 1.2 bundle's NdArray `oneOf` rejects valid float arrays.
#![allow(dead_code)]

use std::sync::LazyLock;

use jsonschema::{Draft, Validator};
use serde_json::{json, Value};

/// The EDR versions whose bundles are vendored. MeteoCore declares 1.1 and
/// validates against both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edr {
    V1_1,
    V1_2,
}

pub const VERSIONS: [Edr; 2] = [Edr::V1_1, Edr::V1_2];

/// One instance document. Neither bundle has an `/instances/{instanceId}`
/// path; an instance is an item of the instances list, so its schema is that
/// list's `instances` item schema.
pub const INSTANCE: &str = "/collections/{collectionId}/instances/{instanceId}";

pub const JSON: &str = "application/json";
pub const GEOJSON: &str = "application/geo+json";

static EDR_1_1: LazyLock<Value> = LazyLock::new(|| {
    bundle(
        include_str!("../../../../schemas/ogcapi-edr-1.1-bundled.json"),
        "3.0.3",
        "1.1.0",
    )
});

static EDR_1_2: LazyLock<Value> = LazyLock::new(|| {
    bundle(
        include_str!("../../../../schemas/ogcapi-edr-1.2-oas30-bundled.json"),
        "3.0.4",
        "1.2.0",
    )
});

fn bundle(source: &str, openapi: &str, version: &str) -> Value {
    let mut bundle: Value = serde_json::from_str(source).expect("pinned EDR bundle parses");
    assert_eq!(bundle["openapi"], openapi);
    assert_eq!(bundle["info"]["version"], version);
    normalize_nullable(&mut bundle);
    bundle
}

fn document(version: Edr) -> &'static Value {
    match version {
        Edr::V1_1 => &EDR_1_1,
        Edr::V1_2 => &EDR_1_2,
    }
}

/// Follow `$ref`s to the node they name: local references only.
fn deref<'a>(bundle: &'a Value, mut node: &'a Value) -> &'a Value {
    while let Some(reference) = node.get("$ref") {
        let pointer = reference
            .as_str()
            .and_then(|r| r.strip_prefix('#'))
            .expect("only local $refs permitted");
        node = bundle
            .pointer(pointer)
            .unwrap_or_else(|| panic!("unresolved {reference}"));
    }
    node
}

/// The `200` schema for `media` at the bundle's resource `path`.
fn response_schema(version: Edr, path: &str, media: &str) -> &'static Value {
    let bundle = document(version);
    let schema = if path == INSTANCE {
        let list = response_schema(version, "/collections/{collectionId}/instances", media);
        &deref(bundle, list)["properties"]["instances"]["items"]
    } else {
        let response = deref(bundle, &bundle["paths"][path]["get"]["responses"]["200"]);
        &response["content"][media]["schema"]
    };
    assert!(
        schema.is_object(),
        "EDR {version:?} {path}: no {media} response schema"
    );
    schema
}

/// The schema of one `parameter_names` entry. Both bundles describe
/// `parameter_names` as an object whose `additionalProperties` holds this
/// schema under `items`, an array keyword that constrains no object member,
/// so the response schema alone checks no entry.
fn parameter_schema(version: Edr) -> &'static Value {
    let bundle = document(version);
    let collection = deref(
        bundle,
        response_schema(version, "/collections/{collectionId}", JSON),
    );
    let schema = &collection["properties"]["parameter_names"]["additionalProperties"]["items"];
    assert!(
        schema.is_object(),
        "EDR {version:?}: no parameter_names entry schema"
    );
    schema
}

/// Compile `schema` next to the bundle's whole `components` object, so nested
/// `$ref`s resolve. Validate a selected schema, never the OpenAPI document
/// as a schema.
fn compile(version: Edr, schema: &Value, context: &str) -> Validator {
    let schema = json!({
        "allOf": [schema],
        "components": document(version).get("components").cloned().unwrap_or(json!({})),
    });
    check_local_refs(&schema, &schema);
    jsonschema::options()
        .with_draft(Draft::Draft4)
        .should_validate_formats(true)
        .build(&schema)
        .unwrap_or_else(|error| panic!("EDR {version:?} {context}: {error}"))
}

/// The collection-shaped documents in a response at `path`.
fn collections<'a>(path: &str, body: &'a Value) -> Vec<&'a Value> {
    let list = |key: &str| body[key].as_array().into_iter().flatten().collect();
    match path {
        "/collections" => list("collections"),
        "/collections/{collectionId}/instances" => list("instances"),
        "/collections/{collectionId}" | INSTANCE => vec![body],
        _ => Vec::new(),
    }
}

/// Every violation of the `version` schema for `path` and `media`, plus every
/// `parameter_names` entry's violations of the entry schema the bundle
/// intends (see [`parameter_schema`]).
pub fn errors(version: Edr, path: &str, media: &str, body: &Value) -> Vec<String> {
    let mut errors: Vec<String> = compile(
        version,
        response_schema(version, path, media),
        &format!("{path} {media}"),
    )
    .iter_errors(body)
    .map(|error| format!("- {}", describe(&error)))
    .collect();
    let documents = collections(path, body);
    if documents.is_empty() {
        return errors;
    }
    let parameter = compile(version, parameter_schema(version), "parameter_names entry");
    for document in documents {
        for (name, entry) in document["parameter_names"]
            .as_object()
            .into_iter()
            .flatten()
        {
            errors.extend(
                parameter
                    .iter_errors(entry)
                    .map(|error| format!("- parameter_names.{name}: {}", describe(&error))),
            );
        }
    }
    errors
}

fn describe(error: &jsonschema::ValidationError<'_>) -> String {
    format!("{error} (at {})", error.instance_path())
}

/// `body` validates against both the EDR 1.1 and 1.2 response schemas.
pub fn assert_valid(path: &str, media: &str, body: &Value, context: &str) {
    for version in VERSIONS {
        let errors = errors(version, path, media, body);
        assert!(
            errors.is_empty(),
            "EDR {version:?} {path} {context}:\n{}\n\nResponse:\n{}",
            errors.join("\n"),
            serde_json::to_string_pretty(body).unwrap()
        );
    }
}

// The only dialect conversion these pinned response schemas need. Keep
// enum/oneOf/other constraints intact; nullable does not override them in
// OAS 3.0.
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
