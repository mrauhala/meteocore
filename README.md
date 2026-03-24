# Metocean Data Server

A high-performance modular metocean data server built in Rust. Implements the [OGC API - Environmental Data Retrieval (EDR)](https://ogcapi.ogc.org/edr/) standard with CoverageJSON and GeoJSON output.

## Quick Start

```bash
# Build
cargo build

# Run (serves on http://localhost:3000)
cargo run -p server

# Test endpoints
curl http://localhost:3000/collections
curl http://localhost:3000/collections/weather/locations
curl http://localhost:3000/collections/weather/locations/Helsinki
```

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
```

Multiple collections can be defined by repeating the `[[collections]]` section.

## API Endpoints

| Endpoint | Description | Response Format |
|---|---|---|
| `GET /` | Landing page with API links | JSON |
| `GET /conformance` | OGC EDR 1.1 conformance classes | JSON |
| `GET /collections` | List all collections | JSON |
| `GET /collections/{id}` | Collection metadata (extent, parameters, data queries) | JSON |
| `GET /collections/{id}/locations` | Available locations | GeoJSON |
| `GET /collections/{id}/locations/{locId}` | Query a location's time series | CoverageJSON |

### Query Parameters

The location query endpoint supports optional filtering:

| Parameter | Example | Description |
|---|---|---|
| `datetime` | `2024-01-01T00:00:00Z/2024-01-01T03:00:00Z` | Time interval (ISO 8601). Supports open intervals with `..` |
| `parameter-name` | `temperature,humidity` | Comma-separated list of parameters to include |

### Examples

```bash
# All data for Helsinki
curl "http://localhost:3000/collections/weather/locations/Helsinki"

# Filter by time range
curl "http://localhost:3000/collections/weather/locations/Helsinki?datetime=2024-01-01T00:00:00Z/2024-01-01T03:00:00Z"

# Filter by parameter
curl "http://localhost:3000/collections/weather/locations/Helsinki?parameter-name=temperature"

# Both filters combined
curl "http://localhost:3000/collections/weather/locations/Helsinki?datetime=2024-01-01T00:00:00Z/2024-01-01T03:00:00Z&parameter-name=temperature,humidity"
```

## Testing

```bash
# Run all tests
cargo test

# Run only CoverageJSON schema validation tests
cargo test -p api-edr

# Run only datetime parsing tests
cargo test -p ds-core
```

CoverageJSON output is validated against the official [OGC CoverageJSON 1.0 schema](https://schemas.opengis.net/covjson/1.0/coveragejson.json) stored in `schemas/coveragejson.json`. The validation tests cover multiple parameters, null values, single timesteps, and structural correctness.

## CSV Data Format

The first four columns are fixed. All subsequent columns are treated as observation parameters:

```csv
location,latitude,longitude,time,temperature,humidity,wind_speed
Helsinki,60.1699,24.9384,2024-01-01T00:00:00Z,-2.5,85.0,3.2
```

| Column | Type | Description |
|---|---|---|
| `location` | string | Location identifier (used in URL paths) |
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
