# Metocean Data Server

A high-performance modular metocean data server built in Rust. Implements [OGC API - Environmental Data Retrieval (EDR)](https://ogcapi.ogc.org/edr/) and [OGC API - Features](https://ogcapi.ogc.org/features/) as separate services sharing the same data sources.

## Quick Start

```bash
# Build (debug)
cargo build

# Run (serves on http://localhost:3000)
cargo run -p server

# Test endpoints
curl http://localhost:3000/
curl http://localhost:3000/edr/collections/weather/locations
curl http://localhost:3000/features/collections/weather/items
```

## Production Build

```bash
# Build optimized release binary
cargo build --release -p server

# Binary is at target/release/server
./target/release/server
```

The release build enables full optimizations (LTO, codegen-units=1 can be added to `Cargo.toml` for further gains). The resulting binary is self-contained — deploy it alongside `config.toml` and your data files.

## Configuration

Edit `config.toml` to configure the server and data collections:

```toml
[server]
host = "0.0.0.0"
port = 3000

[[collections]]
id = "weather"
title = "Finnish Weather Observations"
description = "Hourly weather observations from Finnish weather stations"
data_path = "testdata/weather.csv"
apis = ["edr", "features"]
```

Multiple collections can be defined by repeating the `[[collections]]` section. The `apis` field controls which services expose a collection (defaults to `["edr"]`).

## API Endpoints

### Root

| Endpoint | Description |
|----------|-------------|
| `GET /` | Landing page with links to both services |

### EDR Service (`/edr/`)

| Endpoint | Description | Response Format |
|----------|-------------|-----------------|
| `GET /edr/` | EDR landing page | JSON |
| `GET /edr/conformance` | OGC EDR 1.1 conformance classes | JSON |
| `GET /edr/collections` | List all EDR collections | JSON |
| `GET /edr/collections/{id}` | Collection metadata (extent, parameters, data queries) | JSON |
| `GET /edr/collections/{id}/locations` | Available locations | GeoJSON |
| `GET /edr/collections/{id}/locations/{locId}` | Query a location's time series | CoverageJSON |

#### EDR Query Parameters

| Parameter | Example | Description |
|-----------|---------|-------------|
| `datetime` | `2024-01-01T00:00:00Z/2024-01-01T03:00:00Z` | Time interval (ISO 8601). Supports open intervals with `..` |
| `parameter-name` | `temperature,humidity` | Comma-separated list of parameters to include |

### Features Service (`/features/`)

| Endpoint | Description | Response Format |
|----------|-------------|-----------------|
| `GET /features/` | Features landing page | JSON |
| `GET /features/conformance` | OGC Features 1.0 conformance classes | JSON |
| `GET /features/collections` | List all feature collections | JSON |
| `GET /features/collections/{id}` | Collection metadata | JSON |
| `GET /features/collections/{id}/items` | Paginated feature items | GeoJSON |
| `GET /features/collections/{id}/items/{featureId}` | Single feature | GeoJSON |

#### Features Query Parameters

| Parameter | Example | Description |
|-----------|---------|-------------|
| `bbox` | `24,60,25,61` | Bounding box filter (west,south,east,north) |
| `limit` | `10` | Page size (default: 100, max: 1000) |
| `offset` | `20` | Pagination offset (default: 0) |

### Examples

```bash
# Root landing page
curl http://localhost:3000/

# EDR: All data for Helsinki
curl "http://localhost:3000/edr/collections/weather/locations/Helsinki"

# EDR: Filter by time range and parameter
curl "http://localhost:3000/edr/collections/weather/locations/Helsinki?datetime=2024-01-01T00:00:00Z/2024-01-01T03:00:00Z&parameter-name=temperature"

# Features: List all station features
curl "http://localhost:3000/features/collections/weather/items"

# Features: Paginated with limit
curl "http://localhost:3000/features/collections/weather/items?limit=2"

# Features: Filter by bounding box
curl "http://localhost:3000/features/collections/weather/items?bbox=24,60,25,61"

# Features: Single station feature
curl "http://localhost:3000/features/collections/weather/items/Helsinki"
```

## Testing

```bash
# Run all tests
cargo test

# Run only CoverageJSON schema validation tests
cargo test -p api-edr

# Run only Features API tests (params, response serialization)
cargo test -p api-features

# Run only core tests (datetime parsing, bbox validation)
cargo test -p ds-core

# Run only engine tests (feature queries, pagination, bbox filtering)
cargo test -p engine-csv
```

CoverageJSON output is validated against the official [OGC CoverageJSON 1.0 schema](https://schemas.opengis.net/covjson/1.0/coveragejson.json) stored in `schemas/coveragejson.json`. The validation tests cover multiple parameters, null values, single timesteps, and structural correctness.

## CSV Data Format

The first four columns are fixed. All subsequent columns are treated as observation parameters:

```csv
location,latitude,longitude,time,temperature,humidity,wind_speed
Helsinki,60.1699,24.9384,2024-01-01T00:00:00Z,-2.5,85.0,3.2
```

| Column | Type | Description |
|--------|------|-------------|
| `location` | string | Location identifier (used in URL paths and as feature ID) |
| `latitude` | f64 | WGS84 latitude |
| `longitude` | f64 | WGS84 longitude |
| `time` | RFC 3339 | Timestamp (e.g. `2024-01-01T00:00:00Z`) |
| *remaining* | f64 | Observation values (empty = missing data) |

## Tech Stack

- **Rust** with axum + tokio async runtime
- **chrono** for datetime handling
- **serde** + serde_json for serialization
- **tower-http** for CORS
- **thiserror** for error types
