# api-edr — OGC API - Environmental Data Retrieval 1.1 status

This page is the single source of truth for **what MeteoCore's EDR
implementation supports and what it does not**, per query type and per
engine. It is written for integrators and for anyone planning EDR work.

**Keep it current: every PR that touches EDR behaviour — `crates/api-edr`,
`EdrEngine` in `crates/core/src/edr_engine.rs`, or an engine's `EdrEngine`
implementation / `supported_query_types` — must update the tables below in
the same PR.** The `crates/api-edr/CLAUDE.md` rule points here.

Spec: OGC API - EDR 1.1 (OGC 19-086r6). Base route: `/edr`.

## Conformance classes

| Class | Declared | Notes |
|---|---|---|
| `core` | ✓ | |
| `collections` | ✓ | |
| `queries` | ✓ | one class for every query type; each collection's `data_queries` says which it supports |
| `json` | ✓ | |
| `covjson` | ✓ | every CoverageJSON body validates against `schemas/coveragejson.json` (`cargo test -p api-edr`) |
| `html` | ✓ | every metadata resource (landing, conformance, collections, collection, instances, instance) negotiates `?f=html` / `Accept` |
| `oas30` | ✓ | `/edr/api` (hand-written `api_definition()`), Swagger UI at `/edr/api/docs` |
| `geojson` | ✗ | data queries answer 400 for `f=GeoJSON`; only `/locations` is GeoJSON |
| `edr-geojson` | ✗ | same reason (a test pins that it is *not* declared) |

Also declared: OGC API - Common Part 1 (core, landing-page, oas30), Part 2
(collections, json, html) and Part 4 searchable-collections (`/collections?bbox=&datetime=&q=&limit=&offset=`).

## Query types

| Query type | Route | Status | Notes |
|---|---|---|---|
| `locations` | `/collections/{id}/locations`, `/locations/{locId}` | ✓ | GeoJSON list + CoverageJSON/PNG series per location |
| `position` | `/collections/{id}/position` | ✓ | `POINT` or `MULTIPOINT` (fanned out, flattened into one CoverageCollection — per-point grouping not preserved; fan-out unbounded, #585) |
| `area` | `/collections/{id}/area` | ✓ | WKT `POLYGON` (holes allowed) or `west,south,east,north`; PNG rejected |
| `radius` | `/collections/{id}/radius` | ✓ | `coords=POINT`, `within`, `within-units=km\|m\|mi`; default trait impl = 64-vertex geodesic polygon → `query_area`; capped at 1000 km; pole/antimeridian circles are 400 (#667) |
| `trajectory` | `/collections/{id}/trajectory` | partial | 2-D `LINESTRING` only, meaning a *vertical cross-section* (PVOL sites). `LINESTRINGZ/M` (per-node z/time) not accepted; no along-path sampling on gridded engines |
| `instances` | `/collections/{id}/instances`, `/instances/{instanceId}` | ✓ | forecast model runs (`ds_core::instances`); instance-scoped queries: position, area, radius only |
| `cube` | — | ✗ | not in the trait or the router |
| `corridor` | — | ✗ | not in the trait or the router (`corridor-width`/`-height` documented as follow-up on trajectory) |
| `items` | — | ✗ | not in the trait or the router; the natural surface for CAP/GeoJSON/PostGIS-events, overlaps Features |

### Instance-scoped routes

| Route | Status |
|---|---|
| `/instances/{instanceId}/position` | ✓ |
| `/instances/{instanceId}/area` | ✓ |
| `/instances/{instanceId}/radius` | ✓ |
| `/instances/{instanceId}/locations`, `/trajectory` | ✗ |

## Parameters

| Parameter | Status | Notes |
|---|---|---|
| `coords` | ✓ | WKT per query type (see above) |
| `datetime` | ✓ | RFC 3339 instant, `start/end`, `../end`, `start/..` |
| `parameter-name` | partial | comma-separated; 400 only when *no* requested name matches — a mix of valid and unknown names silently drops the unknown ones (#666, the #605 rule) |
| `z` | ✓ | single, list, or `min/max` interval, snapped to the collection's advertised levels; 400 on a collection with no vertical extent |
| `f` | partial | `CoverageJSON` (default) and `PNG` (position/locations/trajectory plots) only; media-type aliases such as `f=application/json` rejected (#510); no CSV/NetCDF/GeoJSON |
| `crs` | ✗ | data queries accept CRS84 only; `crs_details` not advertised; `bbox-crs` on `/collections` is CRS84 only |
| `within`, `within-units` | ✓ | radius only |
| `resolution-x`/`-y`/`-z` | ✗ | (cube / area resolution hints) not accepted |
| `limit` | ✓ | `/collections` and `/locations` pagination only |

Every 200 carries `Cache-Control` + a strong ETag; `If-None-Match` → 304 (#499).

## Domain types produced

`PointSeries`, `Point` (events), `Grid` (with optional `t` and `z` axes),
`VerticalProfile`, `Section` (trajectory cross-sections, with the
`meteocore:beamCoverage` foreign member). Everything validates against the
CoverageJSON 1.0 schema.

## Per-engine query-type matrix

✓ implemented · – not implemented · n/a not applicable (no model runs).

| Engine | locations | position | area | radius | trajectory | instances | Area semantics |
|---|---|---|---|---|---|---|---|
| CSV | ✓ | – | ✓ | ✓ | – | n/a | stations whose point is inside the polygon (≤ 500) |
| GeoTIFF | – | ✓ | ✓ | ✓ | – | n/a | polygon-tested |
| GRIB | – | ✓ | ✓ | ✓ | – | ✓ | Grid over the polygon's bbox at native resolution (≤ 1M cells), cells outside the polygon masked; antimeridian-crossing bboxes rejected (#667) |
| QueryData | – | ✓ | ✓ | ✓ | – | ✓ | Grid over bbox at native resolution, ≤ 256 cells/axis, cells outside the polygon masked (vertex fallback for sub-cell shapes); polygon outside the extent → 404; `t` axis when several steps |
| Zarr | – | ✓ | ✓ | ✓ | – | ✓ | Grid over bbox at native resolution, ≤ 256 cells/axis, one store read per variable for the whole time span (two across the antimeridian; cells within half a native cell of ±180° are not interpolated across the seam, #667), at most 8 variables per request, cells outside the polygon masked (vertex fallback for sub-cell shapes); polygon outside the extent → 404; `t` axis when several steps. Forecast stores (reference + lead axes) expose every run as an instance; `None` ⇒ latest |
| ODIM composite | – | ✓ | ✓ | ✓ | – | n/a | Grid over bbox, ≤ 256 cells/axis, masked to the polygon; `t` axis when several steps |
| ODIM PVOL site | ✓ | ✓ | ✓ | ✓ | ✓ | n/a | polar sampling; trajectory = RHI cross-section |
| PostGIS stations | ✓ | ✓ | ✓ | ✓ | – | n/a | stations-only `location_source`: exact `ST_Within` in SQL; observations-derived: exact point-in-polygon on the cached station set |
| PostGIS events | – | – | ✓ | ✓ | – | n/a | events in the polygon (exact, in SQL) as a `Point` CoverageCollection |
| Nowcast | – | – | ✓ (motion field) | ✓ | – | ✓ | motion blocks over the polygon's bbox, blocks outside the polygon masked; reflectivity via EDR = #523 |
| CAP, GeoJSON | — no `EdrEngine` (Features/Maps only) — | | | | | | |

Radius, cube, corridor and items have no engine-specific code: radius is
answered by every engine that answers area, the other three do not exist.

## Known gaps, in suggested order

1. `locations` and `trajectory` under `/instances/{id}/`.
2. `cube` and `corridor` (derivable from area / trajectory).
3. `crs` on data queries + `crs_details`; EDR GeoJSON output for point results (then declare `edr-geojson`).
4. `items` for the feature engines (CAP, GeoJSON, PostGIS events).

Related issues: #585 MULTIPOINT fan-out bound · #510 `f` aliases · #667
antimeridian bboxes · #668 400-vs-404 on unsupported query types · #666
shared parameter-name validation · #665 GRIB value rounding · #523 nowcast
reflectivity via EDR · #673 shared area budget.
