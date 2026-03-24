# Metocean Data Server — Claude Instructions

## What This Is

Rust workspace implementing OGC API - EDR and OGC API - Features servers. Six crates: `ds-core` (traits + types), `engine-csv` (CSV data engine), `engine-geojson` (GeoJSON data engine), `api-edr` (EDR HTTP layer), `api-features` (Features HTTP layer), `server` (binary).

## Build & Run

```bash
cargo build                  # Build all crates
cargo test                   # Run all tests
cargo run -p server          # Start server (reads config.toml)
cargo check -p <crate>       # Type-check a single crate
```

## Architecture Rules

- **Two core traits: `Engine` (EDR) and `FeatureEngine` (Features).** They are separate traits — not all engines need to support both APIs. Engines return domain types, never JSON. Serialization belongs in the API crates.
- **ds-core has no framework dependencies.** Only chrono, serde, thiserror, toml. Keep it that way. Use `PropertyValue` enum instead of `serde_json::Value` for feature properties.
- **API crates depend only on ds-core**, not on any engine crate. API state is a registry of engines keyed by collection ID (`EdrState` / `FeaturesState`), not a single engine.
- **EDR and Features are separate services** with separate base routes (`/edr/...` and `/features/...`). They share data sources but have independent landing pages, conformance endpoints, and collection listings.
- **CORS is applied at the server level**, not in individual API crates. The `CorsLayer` lives in `server/src/main.rs`.
- **New engines** implement `Engine` and/or `FeatureEngine` traits in their own crate, get wired up in `server/src/main.rs`.
- **Collection routing is dynamic.** Handlers look up engines from a `HashMap<String, Arc<dyn Engine/FeatureEngine>>` by collection ID from the URL path. No collection IDs are hardcoded.
- **The `apis` config field is enforced.** Only collections listing a given API in their `apis` array are wired to that API's router. A GeoJSON collection with `apis = ["features"]` will not appear in EDR.

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
/features/api                                  Features OpenAPI definition
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
4. Add the crate as a dependency of `crates/server/Cargo.toml`
5. Add a match arm for the new `engine_type` in `server/src/main.rs`
6. Wire it into the appropriate registries (`edr_engines` / `feature_engines`) based on the collection's `apis` config

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
| `bbox` | `west,south,east,north` or `west,south,min-h,east,north,max-h` | Bounding box filter. Supports 4-value (2D) and 6-value (3D, height ignored). Supports antimeridian-crossing (west > east). |
| `limit` | integer | Page size. Default 100, max 1000, min 1. Clamped silently if out of range |
| `offset` | integer | Pagination offset. Default 0 |
| `datetime` | RFC 3339 instant or interval | Temporal filter. Supports instants, intervals (`start/end`), and open bounds (`../end`, `start/..`, `../..`) |

## Features API Response Details

- **Content-Type**: Items/item endpoints return `application/geo+json` (not `application/json`)
- **`timeStamp`**: FeatureCollection responses include `timeStamp` (RFC 3339)
- **`numberMatched`/`numberReturned`**: Always present in FeatureCollection responses
- **Collection metadata**: Includes `extent.spatial.bbox`, `crs`, `storageCrs`
- **OpenAPI**: Served at `/features/api` (linked from landing page via `rel: service-desc`)
- **Conformance**: Declares `core`, `oas30`, `geojson`

## Geometry Types

The `Geometry` enum in `ds_core::feature` supports:

- **`Point { x, y }`** — Single coordinate pair (lon, lat).
- **`Polygon { exterior, holes }`** — Exterior ring as `Vec<[f64; 2]>` plus optional hole rings. Coordinates are `[lon, lat]` pairs.
- **`MultiPolygon { polygons }`** — Vec of `(exterior, holes)` tuples.
- **`Null`** — Null geometry for features without spatial location (RFC 7946 §3.2).

Helper methods on `Geometry`:
- `bbox() -> Option<[f64; 4]>` — Computes bounding box `[west, south, east, north]`. Returns `None` for null geometry.
- `centroid() -> Option<(f64, f64)>` — Computes centroid `(lon, lat)`. Returns `None` for null geometry.

The `Bbox` struct provides:
- `contains(x, y) -> bool` — Point-in-bbox test. Handles antimeridian-crossing bboxes.
- `intersects_bbox(&[f64; 4]) -> bool` — AABB intersection test. Handles antimeridian-crossing bboxes.
- `crosses_antimeridian() -> bool` — True when west > east.

The `FeatureEngine` trait provides:
- `get_features(&self, query) -> Result<FeaturePage>` — Paginated feature query with bbox/datetime filtering.
- `get_feature(&self, id) -> Result<Feature>` — Single feature by ID.
- `feature_count(&self) -> usize` — Total features (default: delegates to get_features with limit=0).
- `spatial_extent(&self) -> Option<[f64; 4]>` — Overall spatial extent (default: None).

## CSV Data Format

Fixed columns: `location, latitude, longitude, time` (in that order). All remaining columns become parameters. Parameter units are mapped in `engine-csv/src/loader.rs`.

## GeoJSON Data Format

Standard GeoJSON FeatureCollection files (RFC 7946). Requirements:

- **Coordinates must be in WGS84 (EPSG:4326).** The engine validates all coordinates fall within lon -180..180, lat -90..90 and rejects files in projected CRS with a helpful error message.
- **Supported geometry types:** Point, Polygon, MultiPolygon.
- **Feature IDs:** Extracted from the top-level `"id"` field on each GeoJSON feature object. Falls back to array index if absent.
- **Properties:** Mapped to `PropertyValue` enum (String, Integer, Float, Bool, Null). Nested objects/arrays are serialized to string.

### Security limits (hardcoded in `engine-geojson/src/loader.rs`)

| Limit | Value | Purpose |
|-------|-------|---------|
| Max file size | 500 MB | Prevents memory exhaustion |
| Max features | 1,000,000 | Prevents excessive load time |
| Max coords per geometry | 100,000 | Prevents geometry bombs |

### Spatial indexing

The GeoJSON engine uses an R-tree (`rstar` crate) built from per-feature bounding boxes. Bbox queries use AABB envelope intersection (not exact polygon intersection), which is both fast and OGC API Features spec-compliant. The R-tree is bulk-loaded at startup in O(n log n).

### Converting projected data to WGS84

If your source data uses a projected CRS (e.g., EPSG:3067 for Finnish data), convert it before loading:

```bash
ogr2ogr -f GeoJSON -t_srs EPSG:4326 output.geojson input.geojson
```

Or with Python:
```python
from pyproj import Transformer
transformer = Transformer.from_crs("EPSG:3067", "EPSG:4326", always_xy=True)
lon, lat = transformer.transform(easting, northing)
```

## Config Format

```toml
[server]
host = "0.0.0.0"
port = 8000

[[collections]]
id = "weather"
title = "Finnish Weather Observations"
description = "Hourly weather observations from Finnish weather stations"
data_path = "testdata/weather.csv"
apis = ["edr", "features"]     # optional, defaults to ["edr"]
engine_type = "csv"             # optional, defaults to "csv"

[[collections]]
id = "municipalities"
title = "Finnish Municipalities"
description = "Municipality boundaries from Statistics Finland"
data_path = "testdata/municipalities.geojson"
engine_type = "geojson"
apis = ["features"]
```

### Config fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `id` | yes | — | Unique collection identifier, used in URL paths |
| `title` | yes | — | Human-readable collection title |
| `description` | yes | — | Collection description |
| `data_path` | yes | — | Path to data file (CSV or GeoJSON) |
| `apis` | no | `["edr"]` | Which APIs expose this collection: `"edr"`, `"features"`, or both |
| `engine_type` | no | `"csv"` | Data engine: `"csv"` or `"geojson"` |

## API State Architecture

Both API crates use registry-based state instead of a single engine:

```rust
// api-edr
pub struct EdrState {
    pub engines: HashMap<String, Arc<dyn Engine>>,
    pub collections: HashMap<String, CollectionConfig>,
}

// api-features
pub struct FeaturesState {
    pub engines: HashMap<String, Arc<dyn FeatureEngine>>,
    pub collections: HashMap<String, CollectionConfig>,
}
```

Handlers look up the engine by collection ID from the URL path parameter. Unknown collection IDs return 404. Collection metadata (title, description, links) is built from `CollectionConfig`, not hardcoded.

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

## Known Limitations

- Parameter units are hardcoded in the CSV loader's match statement
- All data loaded into memory at startup
- Only the `locations` query type is implemented for EDR (no position, area, radius, trajectory, corridor)
- CRS hardcoded to CRS84 (no on-the-fly reprojection)
- GeoJSON engine implements `FeatureEngine` only (not `Engine`/EDR) — polygon boundary data has no time-series parameters

## Code Style

- Use `thiserror` for error types, not manual `impl Display`
- Prefer returning `Result<T, DataServerError>` from engine methods
- Keep handlers thin — delegate logic to the engine, map errors to HTTP status codes
- Use `serde_json::json!` macro for building JSON responses
- Do not leak internal error details to clients — use generic messages for 500 errors
