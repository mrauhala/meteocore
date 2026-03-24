# Metocean Data Server — Claude Instructions

## What This Is

Rust workspace implementing OGC API - EDR and OGC API - Features servers. Five crates: `ds-core` (traits + types), `engine-csv` (CSV data engine), `api-edr` (EDR HTTP layer), `api-features` (Features HTTP layer), `server` (binary).

## Build & Run

```bash
cargo build                  # Build all crates
cargo test                   # Run all tests
cargo run -p server          # Start server on port 3000 (reads config.toml)
cargo check -p <crate>       # Type-check a single crate
```

## Architecture Rules

- **Two core traits: `Engine` (EDR) and `FeatureEngine` (Features).** They are separate traits — not all engines need to support both APIs. Engines return domain types, never JSON. Serialization belongs in the API crates.
- **ds-core has no framework dependencies.** Only chrono, serde, thiserror, toml. Keep it that way. Use `PropertyValue` enum instead of `serde_json::Value` for feature properties.
- **API crates depend only on ds-core**, not on any engine crate. `api-edr` receives `Arc<dyn Engine>`, `api-features` receives `Arc<dyn FeatureEngine>` as axum state.
- **EDR and Features are separate services** with separate base routes (`/edr/...` and `/features/...`). They share data sources but have independent landing pages, conformance endpoints, and collection listings.
- **CORS is applied at the server level**, not in individual API crates. The `CorsLayer` lives in `server/src/main.rs`.
- **New engines** implement `Engine` and/or `FeatureEngine` traits in their own crate, get wired up in `server/src/main.rs`.

## Crate Name

The core crate is named `ds-core` in Cargo.toml (imported as `ds_core` in Rust). It was renamed from `core` to avoid shadowing Rust's built-in `core` crate, which breaks proc macros like `#[tokio::main]`.

## Route Structure

```
/                                              Root landing page (links to both services)
/edr/                                          EDR landing page
/edr/conformance                               EDR conformance classes
/edr/collections                               EDR collection listing
/edr/collections/{id}                          EDR collection detail
/edr/collections/{id}/locations                EDR locations query
/edr/collections/{id}/locations/{loc_id}       EDR location data query (CoverageJSON)

/features/                                     Features landing page
/features/conformance                          Features conformance classes
/features/collections                          Features collection listing
/features/collections/{id}                     Features collection detail
/features/collections/{id}/items               Feature items (paginated GeoJSON)
/features/collections/{id}/items/{feature_id}  Single feature (GeoJSON)
```

## Adding a New Engine

1. Create `crates/engine-<name>/` with `Cargo.toml` depending on `ds-core`
2. Implement `Engine` and/or `FeatureEngine` traits from `ds_core::engine` / `ds_core::feature_engine`
3. Add the crate to workspace members in root `Cargo.toml`
4. Wire it up in `server/src/main.rs` — cast to `Arc<dyn Engine>` and/or `Arc<dyn FeatureEngine>`

## Adding a New EDR Endpoint

1. Add the handler function in `crates/api-edr/src/handlers.rs`
2. Add the route in `crates/api-edr/src/lib.rs`
3. If new query params are needed, add them in `params.rs`
4. If new response formats are needed, add serializers in `response.rs`

## Adding a New Features Endpoint

1. Add the handler function in `crates/api-features/src/handlers.rs`
2. Add the route in `crates/api-features/src/lib.rs`
3. If new query params are needed, add them in `params.rs`
4. If new response formats are needed, add serializers in `response.rs`

## Features API Query Parameters

| Parameter | Format | Description |
|-----------|--------|-------------|
| `bbox` | `west,south,east,north` | Bounding box filter. Validated: finite numbers, lon -180..180, lat -90..90, west<=east, south<=north |
| `limit` | integer | Page size. Default 100, max 1000. Clamped silently if exceeded |
| `offset` | integer | Pagination offset. Default 0 |

## CSV Data Format

Fixed columns: `location, latitude, longitude, time` (in that order). All remaining columns become parameters. Parameter units are mapped in `engine-csv/src/loader.rs`.

## Config Format

```toml
[server]
host = "0.0.0.0"
port = 3000

[[collections]]
id = "weather"
title = "Finnish Weather Observations"
description = "Hourly weather observations from Finnish weather stations"
data_path = "testdata/weather.csv"
apis = ["edr", "features"]   # optional, defaults to ["edr"]
```

The `apis` field controls which services expose a collection. Currently both APIs are wired unconditionally in the server.

## CoverageJSON Schema Compliance

All CoverageJSON output **must** validate against the OGC CoverageJSON 1.0 schema at `schemas/coveragejson.json` (source: https://schemas.opengis.net/covjson/1.0/coveragejson.json).

### Schema validation tests

Integration tests in `crates/api-edr/tests/covjson_validation.rs` validate serializer output against the schema using the `jsonschema` crate. Run with `cargo test -p api-edr`.

**When modifying `response.rs` or adding new CoverageJSON output, always run these tests.** If adding a new domain type or response variant, add a corresponding test case.

### Key schema rules to follow

**Coverage object** (top level):
- Required fields: `type` ("Coverage"), `domain`, `parameters`, `ranges`
- `parameters` is a map of parameter name → Parameter object
- `ranges` is a map of parameter name → NdArray object

**Domain object** (`domain`):
- Required fields: `type` ("Domain"), `axes`, `referencing`
- `domainType` triggers axis constraints (e.g. PointSeries requires x, y, t)
- `referencing` array connects coordinate names to reference systems

**PointSeries domain** (currently implemented):
- `x` axis: `numericSingleValueAxis` — `{"values": [<single number>]}`
- `y` axis: `numericSingleValueAxis` — `{"values": [<single number>]}`
- `t` axis: `stringValuesAxis` — `{"values": ["2024-01-01T00:00:00+00:00", ...]}`
- No additional axes allowed (`additionalProperties: false`)

**Parameter object**:
- Required: `type` ("Parameter"), `observedProperty`
- `observedProperty` requires `label` as an i18n object: `{"en": "temperature"}`
- `unit` requires either `label` or `symbol` (we provide both)
- `description` must be an i18n object if present: `{"en": "..."}`

**NdArray object** (ranges):
- Required: `type` ("NdArray"), `dataType` ("float"/"integer"/"string"), `values`
- When values has >1 item: `shape` and `axisNames` are also required
- For `dataType: "float"`: values items must be `number | null`
- `values.length` must equal the product of `shape` dimensions

**Reference systems**:
- Spatial: `{"type": "GeographicCRS", "id": "http://www.opengis.net/def/crs/OGC/1.3/CRS84"}`
- Temporal: `{"type": "TemporalRS", "calendar": "Gregorian"}` — calendar must be "Gregorian" or an HTTP URL

**i18n objects**: Keys must be BCP 47 language tags (e.g. `"en"`). No additional properties allowed.

### Adding new domain types

When implementing a new domain type (Point, Grid, Trajectory, VerticalProfile, etc.):

1. Check the schema's `domainBase.dependencies.domainType` section for that type's axis requirements
2. Implement the serializer in `response.rs`
3. Add a validation test in `covjson_validation.rs` using `make_query_result` or a new helper
4. The schema enforces `additionalProperties: false` on axes for typed domains — only the specified axes are allowed

### Common pitfalls

- Axis values must have `uniqueItems: true` — no duplicate timestamps or coordinates
- NdArray `axisNames` must have `uniqueItems: true`
- Forgetting `referencing` on an inline domain object (required when domain is an object, not a URL string)
- Using non-BCP47 keys in i18n objects (use `"en"`, not `"english"`)

## Known Limitations (POC)

- Collection ID is hardcoded to `"weather"` in handlers — needs a registry for multi-collection support
- Parameter units are hardcoded in the CSV loader's match statement
- All data loaded into memory at startup
- Only the `locations` query type is implemented for EDR (no position, area, radius, trajectory, corridor)
- Features API serves locations as point features only (no complex geometries from CSV)
- CRS hardcoded to CRS84

## Code Style

- Use `thiserror` for error types, not manual `impl Display`
- Prefer returning `Result<T, DataServerError>` from engine methods
- Keep handlers thin — delegate logic to the engine, map errors to HTTP status codes
- Use `serde_json::json!` macro for building JSON responses
- Do not leak internal error details to clients — use generic messages for 500 errors
