# Architecture

## Overview

The server is structured as a Rust workspace with four crates. Data flows from engines (data access) through a domain model to API serialization, keeping each concern isolated.

```
┌─────────────────────────────────────────────────┐
│  server (binary)                                │
│  Config loading → Engine init → HTTP server     │
├─────────────────────────────────────────────────┤
│  api-edr                                        │
│  Axum router → Handlers → Response serializers  │
│  (CoverageJSON, GeoJSON)                        │
├─────────────────────────────────────────────────┤
│  engine-csv              │  (future engines)    │
│  CSV loader + indices    │                      │
│  Engine trait impl       │                      │
├──────────────────────────┴──────────────────────┤
│  ds-core                                        │
│  Engine trait, domain model, config, errors      │
└─────────────────────────────────────────────────┘
```

## Crates

### ds-core (`crates/core/`)

Foundation crate with no framework dependencies. Defines:

- **`Engine` trait** — The abstraction boundary between data access and the API layer. Engines return domain objects, never JSON.
- **Domain model** — `QueryResult`, `Location`, `DomainDescription`, `NdArray`, `ParameterDescription`. Mirrors CoverageJSON structure at the type level.
- **`DataServerError`** — Unified error enum using `thiserror`.
- **`ServerConfig`** — TOML-based configuration (serde deserialization).
- **Datetime parsing** — OGC datetime interval parser supporting instants, closed intervals, and open intervals (`..`).

### engine-csv (`crates/engine-csv/`)

CSV-based engine implementation. Loads data fully into memory at startup and builds indices for fast lookups:

- **`CsvDataStore`** — Holds all rows plus two index structures:
  - `HashMap<location_name, Vec<row_index>>` — O(1) location lookup
  - `HashMap<location_name, BTreeMap<DateTime, Vec<row_index>>>` — O(log n) time range queries via `BTreeMap::range()`
- **`CsvEngine`** — Implements the `Engine` trait using the indexed store.

The CSV format uses fixed columns (location, latitude, longitude, time) followed by any number of parameter columns. Parameter units are mapped in the loader.

### api-edr (`crates/api-edr/`)

OGC API - EDR HTTP layer built on axum:

- **Router** — 6 GET endpoints following the EDR specification.
- **Handlers** — Extract path/query params, call the engine, map errors to HTTP status codes (404, 400, 500).
- **Response serializers** — `QueryResult` → CoverageJSON, `Vec<Location>` → GeoJSON FeatureCollection.
- **CORS** — Permissive CORS via `tower-http` for browser clients.

The API layer has no knowledge of CSV or any specific engine — it only depends on `ds-core`.

### server (`crates/server/`)

Thin binary that wires everything together:

1. Load `config.toml` → `ServerConfig`
2. Initialize `CsvDataStore` → `CsvEngine`
3. Build axum router with engine as shared state (`Arc<dyn Engine>`)
4. Bind and serve with tokio

## Key Design Decisions

**Engines return domain objects, not JSON.** The `Engine` trait returns `QueryResult` and `Location` types. Serialization to CoverageJSON/GeoJSON happens in the API layer. This keeps engines testable and format-agnostic.

**BTreeMap for temporal indexing.** `BTreeMap::range()` gives O(log n) time interval queries without scanning all rows. Combined with the location HashMap, a query touches only the exact rows needed.

**Single collection hardcoded in handlers.** The POC handlers check `id == "weather"`. Generalizing to multi-collection requires a registry mapping collection IDs to engine instances.

**All data in memory.** The CSV engine loads everything at startup. This is appropriate for small-to-medium datasets and keeps query latency minimal. Larger datasets would need a different engine (e.g., database-backed).

## Data Flow

```
HTTP Request
  → axum extracts Path + Query params
  → Handler calls engine.query_location(id, datetime, params)
  → CsvEngine looks up location in HashMap
  → BTreeMap.range() filters by time interval
  → Builds QueryResult (domain + parameters + ranges)
  → response.rs serializes to CoverageJSON
  → axum returns JSON response with CORS headers
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
    │   ├── model.rs            # Domain types
    │   ├── engine.rs           # Engine trait
    │   ├── config.rs           # TOML config types
    │   ├── error.rs            # Error enum
    │   └── datetime.rs         # OGC datetime parsing
    ├── engine-csv/src/
    │   ├── lib.rs
    │   ├── loader.rs           # CSV parsing + index building
    │   └── engine.rs           # Engine trait implementation
    ├── api-edr/src/
    │   ├── lib.rs              # Router builder
    │   ├── handlers.rs         # 6 endpoint handlers
    │   ├── response.rs         # CoverageJSON/GeoJSON serialization
    │   └── params.rs           # Query parameter types
    └── server/src/
        └── main.rs             # Binary entry point
```
