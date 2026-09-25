//! Offline validation of tileset and tile-matrix-set responses against the
//! pinned OGC 2D Tile Matrix Set 2.0 JSON schemas (`schemas/tms-2.0`, see
//! `schemas/README.md`). Relative `$ref`s resolve to the vendored copies; any
//! other reference fails instead of downloading.
use std::sync::LazyLock;

use jsonschema::{Retrieve, Uri, Validator};
use serde_json::Value;

const BASE: &str = "https://schemas.opengis.net/tms/2.0/json/";
const FILES: &[(&str, &str)] = &[
    (
        "2DBoundingBox.json",
        include_str!("../../../../schemas/tms-2.0/2DBoundingBox.json"),
    ),
    (
        "2DPoint.json",
        include_str!("../../../../schemas/tms-2.0/2DPoint.json"),
    ),
    (
        "crs.json",
        include_str!("../../../../schemas/tms-2.0/crs.json"),
    ),
    (
        "dataType.json",
        include_str!("../../../../schemas/tms-2.0/dataType.json"),
    ),
    (
        "geospatialData.json",
        include_str!("../../../../schemas/tms-2.0/geospatialData.json"),
    ),
    (
        "link.json",
        include_str!("../../../../schemas/tms-2.0/link.json"),
    ),
    (
        "projJSON.json",
        include_str!("../../../../schemas/tms-2.0/projJSON.json"),
    ),
    (
        "propertiesSchema.json",
        include_str!("../../../../schemas/tms-2.0/propertiesSchema.json"),
    ),
    (
        "style.json",
        include_str!("../../../../schemas/tms-2.0/style.json"),
    ),
    (
        "tileMatrix.json",
        include_str!("../../../../schemas/tms-2.0/tileMatrix.json"),
    ),
    (
        "tileMatrixLimits.json",
        include_str!("../../../../schemas/tms-2.0/tileMatrixLimits.json"),
    ),
    (
        "tileMatrixSet.json",
        include_str!("../../../../schemas/tms-2.0/tileMatrixSet.json"),
    ),
    (
        "tilePoint.json",
        include_str!("../../../../schemas/tms-2.0/tilePoint.json"),
    ),
    (
        "tileSet.json",
        include_str!("../../../../schemas/tms-2.0/tileSet.json"),
    ),
    (
        "timeStamp.json",
        include_str!("../../../../schemas/tms-2.0/timeStamp.json"),
    ),
    (
        "variableMatrixWidth.json",
        include_str!("../../../../schemas/tms-2.0/variableMatrixWidth.json"),
    ),
];

struct Pinned;

impl Retrieve for Pinned {
    fn retrieve(
        &self,
        uri: &Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let name = uri
            .as_str()
            .strip_prefix(BASE)
            .ok_or_else(|| format!("unpinned schema reference {uri}"))?;
        let (_, source) = FILES
            .iter()
            .find(|(file, _)| *file == name)
            .ok_or_else(|| format!("{name} is not vendored"))?;
        Ok(serde_json::from_str(source)?)
    }
}

fn validator(file: &str) -> Validator {
    let (_, source) = FILES.iter().find(|(f, _)| *f == file).expect("vendored");
    jsonschema::options()
        .with_base_uri(format!("{BASE}{file}"))
        .with_retriever(Pinned)
        .build(&serde_json::from_str(source).expect("pinned TMS schema parses"))
        .unwrap_or_else(|error| panic!("{file}: {error}"))
}

static TILESET: LazyLock<Validator> = LazyLock::new(|| validator("tileSet.json"));
static TILE_MATRIX_SET: LazyLock<Validator> = LazyLock::new(|| validator("tileMatrixSet.json"));

fn assert_valid(validator: &Validator, body: &Value, context: &str) {
    let errors: Vec<_> = validator
        .iter_errors(body)
        .map(|error| format!("{} at {}", error, error.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "TMS 2.0 {context}:\n{}",
        errors.join("\n")
    );
}

pub fn assert_tileset(body: &Value, context: &str) {
    assert_valid(&TILESET, body, context);
}

pub fn assert_tile_matrix_set(body: &Value, context: &str) {
    assert_valid(&TILE_MATRIX_SET, body, context);
}

pub fn is_valid_tileset(body: &Value) -> bool {
    TILESET.is_valid(body)
}
