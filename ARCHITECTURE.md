# Architecture

## Overview

The server is structured as a Rust workspace with five crates. Two separate OGC API services (EDR and Features) share the same data engines through trait abstractions defined in the core crate.

```
┌──────────────────────────────────────────────────────────┐
│  server (binary)                                         │
│  Config loading → Engine init → Mount both API routers   │
│  CORS layer applied here                                 │
├──────────────────────┬───────────────────────────────────┤
│  api-edr             │  api-features                     │
│  /edr/... routes     │  /features/... routes             │
│  CoverageJSON,       │  GeoJSON FeatureCollection,       │
│  GeoJSON             │  pagination, bbox filtering       │
├──────────────────────┴───────────────────────────────────┤
│  engine-csv              │  (future engines)             │
│  CSV loader + indices    │                               │
│  Engine + FeatureEngine  │                               │
│  trait impls             │                               │
├──────────────────────────┴───────────────────────────────┤
│  ds-core                                                 │
│  Engine trait, FeatureEngine trait, domain model,         │
│  feature types, config, errors                           │
└──────────────────────────────────────────────────────────┘
```

## Crates

### ds-core (`crates/core/`)

Foundation crate with no framework dependencies. Defines:

- **`Engine` trait** — Abstraction for EDR data access. Returns `QueryResult`, `Location`, and related types.
- **`FeatureEngine` trait** — Abstraction for Features data access. Returns `Feature`, `FeaturePage`. Separate from `Engine` — not all engines need to support both APIs.
- **EDR domain model** — `QueryResult`, `Location`, `DomainDescription`, `NdArray`, `ParameterDescription`. Mirrors CoverageJSON structure at the type level.
- **Feature domain model** — `Feature`, `FeaturePage`, `Geometry`, `Bbox`, `FeatureQuery`, `PropertyValue`. Uses `PropertyValue` enum instead of `serde_json::Value` to keep ds-core format-agnostic.
- **`DataServerError`** — Unified error enum using `thiserror`.
- **`ServerConfig`** — TOML-based configuration (serde deserialization). Collections have an `apis` field to declare which services they support.
- **Datetime parsing** — OGC datetime interval parser supporting instants, closed intervals, and open intervals (`..`).

### engine-csv (`crates/engine-csv/`)

CSV-based engine implementation. Loads data fully into memory at startup and builds indices for fast lookups:

- **`CsvDataStore`** — Holds all rows plus two index structures:
  - `HashMap<location_name, Vec<row_index>>` — O(1) location lookup
  - `HashMap<location_name, BTreeMap<DateTime, Vec<row_index>>>` — O(log n) time range queries via `BTreeMap::range()`
- **`CsvEngine`** — Implements both `Engine` and `FeatureEngine` traits:
  - `Engine` methods serve EDR time-series queries (CoverageJSON)
  - `FeatureEngine` methods serve locations as point features with bbox filtering and pagination

The CSV format uses fixed columns (location, latitude, longitude, time) followed by any number of parameter columns. Parameter units are mapped in the loader.

### api-edr (`crates/api-edr/`)

OGC API - EDR HTTP layer built on axum:

- **Router** — 6 GET endpoints following the EDR specification, mounted under `/edr/`.
- **Handlers** — Extract path/query params, call the engine, map errors to HTTP status codes (404, 400, 500).
- **Response serializers** — `QueryResult` → CoverageJSON, `Vec<Location>` → GeoJSON FeatureCollection.

The API layer has no knowledge of CSV or any specific engine — it only depends on `ds-core`.

### api-features (`crates/api-features/`)

OGC API - Features HTTP layer built on axum:

- **Router** — 6 GET endpoints following the Features specification, mounted under `/features/`.
- **Handlers** — Landing page, conformance, collections, items (paginated), single feature.
- **Query parameters** — `bbox` (validated bounding box), `limit` (default 100, max 1000), `offset`.
- **Response serializers** — `Feature` → GeoJSON Feature, `FeaturePage` → GeoJSON FeatureCollection with `numberMatched`, `numberReturned`, and pagination links (`self`, `next`, `prev`).

Depends only on `ds-core`, not on any engine crate.

### server (`crates/server/`)

Thin binary that wires everything together:

1. Load `config.toml` → `ServerConfig`
2. Initialize `CsvDataStore` → `CsvEngine`
3. Cast engine to both `Arc<dyn Engine>` and `Arc<dyn FeatureEngine>`
4. Mount EDR router under `/edr`, Features router under `/features`
5. Add root landing page at `/` with links to both services
6. Apply CORS layer at server level (covers all routes)
7. Bind and serve with tokio

## Key Design Decisions

**Two separate traits, not one.** `Engine` and `FeatureEngine` are independent traits. EDR returns coverage data (`QueryResult`, `NdArray`); Features returns vector features (`Feature`, `FeaturePage`). These have zero overlap. An engine can implement one or both.

**Separate service base routes.** EDR lives under `/edr/...`, Features under `/features/...`. This avoids route collisions on `/collections` and allows each service to have its own landing page, conformance declaration, and collection metadata format.

**Shared data sources.** A single `CsvEngine` instance implements both traits and is wired to both routers. The `config.toml` `apis` field declares which services a collection supports.

**Engines return domain objects, not JSON.** Serialization to CoverageJSON/GeoJSON happens in the API crates. This keeps engines testable and format-agnostic.

**PropertyValue enum in ds-core.** Feature properties use a custom `PropertyValue` enum (String, Float, Integer, Bool, Null) instead of `serde_json::Value` to keep ds-core free of serialization format dependencies.

**CORS at server level.** A single `CorsLayer::permissive()` in `server/src/main.rs` covers all routes uniformly, rather than each API crate managing its own CORS.

**BTreeMap for temporal indexing.** `BTreeMap::range()` gives O(log n) time interval queries without scanning all rows. Combined with the location HashMap, a query touches only the exact rows needed.

**All data in memory.** The CSV engine loads everything at startup. This is appropriate for small-to-medium datasets and keeps query latency minimal. Larger datasets would need a different engine (e.g., database-backed).

## Data Flow

### EDR Query

```
GET /edr/collections/weather/locations/Helsinki?datetime=...
  → axum extracts Path + Query params
  → Handler calls engine.query_location(id, datetime, params)
  → CsvEngine looks up location in HashMap
  → BTreeMap.range() filters by time interval
  → Builds QueryResult (domain + parameters + ranges)
  → response.rs serializes to CoverageJSON
  → axum returns JSON response
```

### Features Query

```
GET /features/collections/weather/items?bbox=24,60,25,61&limit=10
  → axum extracts Path + Query params
  → Handler parses and validates bbox, clamps limit
  → Builds FeatureQuery, calls engine.get_features(&query)
  → CsvEngine iterates unique locations, applies bbox filter
  → Applies offset/limit pagination
  → Returns FeaturePage with matched/returned counts
  → response.rs serializes to GeoJSON FeatureCollection with pagination links
  → axum returns JSON response
```

## Project Layout

```
dataserver/
├── Cargo.toml                  # Workspace root
├── config.toml                 # Server configuration
├── testdata/
│   └── weather.csv             # Sample data (3 stations × 3 params × 7 hours)
└── crates/
    ├── core/src/
    │   ├── lib.rs
    │   ├── model.rs            # EDR domain types
    │   ├── feature.rs          # Feature domain types (Geometry, Feature, Bbox, etc.)
    │   ├── engine.rs           # Engine trait (EDR)
    │   ├── feature_engine.rs   # FeatureEngine trait (Features)
    │   ├── config.rs           # TOML config types
    │   ├── error.rs            # Error enum
    │   └── datetime.rs         # OGC datetime parsing
    ├── engine-csv/src/
    │   ├── lib.rs
    │   ├── loader.rs           # CSV parsing + index building
    │   └── engine.rs           # Engine + FeatureEngine trait implementations
    ├── api-edr/src/
    │   ├── lib.rs              # Router builder (mounted at /edr)
    │   ├── handlers.rs         # 6 endpoint handlers
    │   ├── response.rs         # CoverageJSON/GeoJSON serialization
    │   └── params.rs           # Query parameter types
    ├── api-features/src/
    │   ├── lib.rs              # Router builder (mounted at /features)
    │   ├── handlers.rs         # 6 endpoint handlers
    │   ├── response.rs         # GeoJSON Feature/FeatureCollection serialization
    │   └── params.rs           # Query parameter types (bbox, limit, offset)
    └── server/src/
        └── main.rs             # Binary entry point, router composition, CORS
```
