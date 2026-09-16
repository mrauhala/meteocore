# api-features — OGC API - Features 1.0 status

This page is the single source of truth for **what MeteoCore's Features
implementation supports and what it does not**, per conformance class,
parameter and engine. It is written for integrators and for anyone planning
Features work.

**Keep it current: every PR that touches Features behaviour —
`crates/api-features`, `FeatureEngine` in `crates/core/src/feature_engine.rs`,
`FeatureQuery` / `Bbox` in `crates/core/src/feature.rs`, or an engine's
`FeatureEngine` implementation (including `sortables`, `filterables`, `spatial_extent`,
`temporal_extent`) — must update the tables below in the same PR.** The
`crates/api-features/CLAUDE.md` rule points here.

Spec: OGC API - Features - Part 1: Core 1.0 (OGC 17-069r4). Base route:
`/features`.

## Conformance classes

| Class | Declared | Notes |
|---|---|---|
| Part 1 `core` | ✓ | landing, `/conformance`, `/collections`, `/collections/{id}`, `/items`, `/items/{featureId}`; optional Part 1 property equality filters implemented (#700) |
| Part 1 `oas30` | ✓ | `/features/api` (hand-written `api_definition()`, validated against the bundled OpenAPI 3.0 meta-schema in tests), Swagger UI at `/features/api/docs` |
| Part 1 `geojson` | ✓ | the only feature encoding |
| Part 1 `html` | ✗ | metadata resources negotiate HTML (Common Part 2 `html` is declared), but `/items` and `/items/{featureId}` are GeoJSON only |
| Part 1 `gmlsf0` / `gmlsf2` | ✗ | no GML |
| Part 2 CRS by reference (`crs`) | ✗ | CRS84 only; `crs` on `/items` returns 400; collections advertise `crs: [CRS84]` + `storageCrs` |
| Part 3 Filtering / CQL2 (`filter`, `queryables`) | ✗ | `filter` controls return 400; no `/queryables` |
| Part 4 Create/Replace/Update/Delete | ✗ | read-only server by design |
| Part 5 Schemas | ✗ | no `/schema`; properties are untyped `PropertyValue`s |
| Part 8 Sorting (draft) | partial | `sortby` implemented (#605 rule: validated against `FeatureEngine::sortables`, 400 naming the valid ones) but the class is not declared and no `/sortables` resource exists |

Also declared: OGC API - Common Part 1 (core, landing-page, oas30), Part 2
(collections, json, html) and Part 4 searchable-collections
(`/collections?bbox=&bbox-crs=&datetime=&q=&limit=&offset=`; `bbox-crs` is
CRS84 only).

## Routes

| Route | Status | Notes |
|---|---|---|
| `/features/` | ✓ | JSON + HTML |
| `/features/api`, `/api/docs` | ✓ | OpenAPI 3.0 + Swagger UI |
| `/features/conformance` | ✓ | JSON + HTML |
| `/features/collections` | ✓ | JSON + HTML; Common Part 4 search |
| `/features/collections/{id}` | ✓ | JSON + HTML; `extent.spatial` from `spatial_extent`, `extent.temporal` from `temporal_extent` (omitted when `None`); `keywords`, `license` link; a `tilesets-vector` link when the collection also lists `tiles` in `apis` |
| `/features/collections/{id}/items` | ✓ | GeoJSON `FeatureCollection` with `numberMatched`, `numberReturned`, `timeStamp`, `self`/`next`/`prev` links that carry the caller's filters and sort |
| `/features/collections/{id}/items/{featureId}` | ✓ | GeoJSON `Feature` with `self` + `collection` links |
| `/features/collections/{id}/queryables`, `/schema`, `/sortables` | ✗ | not routed |

Vector tiles: a collection with a `FeatureEngine` and `tiles` in its `apis`
serves Mapbox Vector Tiles through `api-tiles` (`?f=mvt`), keyed on
`FeatureEngine::data_version` for ETags. CAP registers both raster and vector
sources, so its advertised vector tileset links resolve; vector tiles contain
the loaded alert areas, including historical/future windows, while raster
tiles render the selected instant. Not part of this crate.

## Parameters on `/items`

| Parameter | Status | Notes |
|---|---|---|
| `bbox` | ✓ | 4 or 6 values (heights ignored); `west > east` is an antimeridian-crossing box (Features §7.15.3); 400 on malformed input |
| `datetime` | partial | RFC 3339 instant, `start/end`, `../end`, `start/..`; parsed by the API layer but **silently ignored by engines with no time dimension** (CSV, GeoJSON, PostGIS stations — see matrix) |
| `limit` | ✓ | default 100, clamped to `[1, 1000]` (out-of-range values are clamped, not rejected) |
| `offset` | ✓ | offset pagination (non-standard extension; Part 1 only mandates `next`) |
| `sortby` | ✓ | Part 8 syntax `[+\|-]property,…`; a decoded `+` (space) is accepted as ascending; 400 unless every property is in `FeatureEngine::sortables`; applied before paging (`ds_core::feature::sort_features`) |
| `f` | partial | negotiated (`json` / `html`) on every metadata resource; **400 on `/items`**; still silently ignored on `/items/{featureId}` (GeoJSON only) |
| `crs`, `bbox-crs` (on `/items`) | ✗ | 400; CRS84 only |
| `filter`, `filter-lang`, `filter-crs` | ✗ | 400 (CQL2 is not implemented) |
| `properties` | ✗ | 400; every property is always returned |
| `<property>=value` (Part 1 §7.15.5–6 optional property filters) | ✓ | validated against `FeatureEngine::filterables`; unknown names → 400 listing valid ones; exact strings, list membership, canonical numbers/bools; ANDed before counting/sorting/paging |

Property filter names are case-sensitive. Values are exact (no wildcards,
substring search or numeric coercion); integers/floats use Rust's shortest
`Display` string (`12`, `1.5`), booleans `true`/`false`; missing, null and
non-finite values never match. Multiple predicates, including repeated names,
are ANDed with each other and with `bbox`/`datetime`. For a list, each predicate
can match a different element. Empty strings remain valid literal values.
Names and values are URL-encoded in every `self`/`next`/`prev` link.

`/features/api` lists the accepted property parameters per collection from
cached engine catalogs. CAP, GeoJSON and PostGIS advertise names present in
at least one loaded record (including null values), refreshed with the data.
CSV, BUFR and ODIM use fixed schemas; Nowcast includes optional property groups
only when their sources are wired. Reserved API controls (`bbox`, `datetime`,
`limit`, `offset`, `sortby`, `f`, `crs`, `bbox-crs`, `filter`, `filter-lang`,
`filter-crs`, `properties`) take precedence over source property names and
cannot be used as property filters. Duplicate control parameters return 400.
This does not declare Part 3 conformance or add `/queryables`.

Example: `/features/collections/cap-meteoalarm-wis2/items?awareness_type=1%3B%20Wind&severity=Severe`.

Every 200 carries `Cache-Control` + a strong ETag; `If-None-Match` → 304
(#499). `/items` hashes the ETag with `timeStamp` blanked, and a closed
`datetime` window entirely in the past gets the long cache policy.

Error bodies are `{ "code", "description" }`; 500s never leak internals.
Structured `ErrorReason` codes are #119.

## Geometry types produced

`Point`, `Polygon`, `MultiPolygon`, and `null` (CAP geocode-only areas).
`LineString` does not exist in `ds_core::feature::Geometry` (#408 — storm
tracks).

## Per-engine matrix

✓ implemented · – not implemented · ignored = parameter accepted by the API
layer but has no effect on this engine.

| Engine | Feature = | `bbox` semantics | `datetime` semantics | `sortby` | Property filters | `spatial_extent` | `temporal_extent` | `data_version` |
|---|---|---|---|---|---|---|---|---|
| CSV | one station per distinct location (Point) | station point inside box | ignored | – (400) | ✓ name, latitude, longitude | – | – | 0 (static) |
| GeoJSON file | file features as loaded | feature *bounding box* intersects query box (R-tree), not exact geometry test | ignored | – (400) | ✓ all loaded property names | ✓ | – | ✓ |
| CAP | one alert area (Polygon / MultiPolygon / null geometry); `properties.geometry_source` = `inline`/`geocode`/`notification`/`bbox`; producer `<parameter>`s as top-level properties under their valueName (MeteoAlarm `awareness_level`, `awareness_type`; repeats → list; `impacts`), `<eventCode>`s as `eventCode:<valueName>`. Identity is scoped to sender; canonical IDs include a length-prefixed sender, and old identifier-only URLs resolve only when unambiguous. Status filtering precedes exact sender/identifier/sent Update/Cancel resolution (at ingest for WIS2, on rebuild for directory/feed). Failed documents retain their last good data while successfully fetched documents update | area bbox intersects query box; crossing bboxes split at the antimeridian; null-geometry areas excluded when `bbox` is set | alert active window intersects the interval | – (400) | ✓ standard + producer property names (including lists) | ✓ | ✓ (union of active windows; `None` when fully open) | ✓ |
| PostGIS stations | one station (Point) from the cached location set | station point inside box (in memory, not SQL) | ignored | – (400) | ✓ cached station property names | ✓ | – | ✓ |
| PostGIS events | — no `FeatureEngine` (EDR area + WMS only; Features items = #503) — | | | | — | | | |
| BUFR | one station (Point) from the observation store; properties `wigos_station_identifier`, `name`, `elevation`, `first_report`, `last_report`, `report_count` | station point inside box | station has ≥ 1 report inside the interval | ✓ last_report, first_report, report_count, name | ✓ name, wigos_station_identifier, elevation, first_report, last_report, report_count | ✓ | ✓ (oldest → newest report held) | ✓ (snapshot version) |
| ODIM PVOL network | one radar site (Point) — site inventory | site point inside box | sites with a volume inside the interval | – (400) | ✓ all site inventory properties (including quantities/elevation_angles lists) | ✓ | – | ✓ (inventory-sensitive) |
| Nowcast | one tracked storm cell (Point + fact-sheet properties) from one generation | cell centroid inside box | with none: latest generation; with `datetime`: the newest retained generation inside the interval (~4 h history) | ✓ significance, significance_rank, max_dbz, area_km2, track_age, speed_ms, bearing_deg, intensity_trend_dbz_min + lightning / impact / radar extras when those sources are wired | ✓ all base cell properties + wired lightning/impact/radar groups | ✓ | ✓ (retained history span) | ✓ |
| GeoTIFF, GRIB, QueryData, Zarr, ODIM composite, ODIM PVOL site | — no `FeatureEngine` (EDR / Maps only) — | | | | — | | | |

Pagination: every engine materializes the whole filtered set and slices it
(`offset`/`limit`); nothing streams (#532).

## Known gaps, in suggested order

1. Reject or honour query parameters on `/items/{featureId}` (#681);
   `/items` now rejects unsupported/unknown names (#700). Honour or reject
   `datetime` on engines without a time dimension (#682).
2. Declare Part 8 sorting + serve `/sortables` (#683); HTML for `/items`
   and declare Part 1 `html` (#684).
3. PostGIS events shape as Features items with keyset pagination (#503).
4. `Geometry::LineString` for storm tracks (#408); polygon cells.
5. Part 2 CRS (`crs`, `bbox-crs` on `/items`) — engines hold CRS84 only, so
   this is an output-transform concern in the API layer (#685).
6. Part 3 CQL2 filtering + `/queryables`; Part 5 `/schema` (#686).
7. GeoJSON engine `bbox` refines envelope hits against the geometry (#687).

Related issues: #605 sortby · #532 pagination without materializing · #503
PostGIS events items · #408 LineString · #119 ErrorReason · #127 MVT ·
#138 vector-tile cache key · #303 Part 4 search follow-ups · #306 shared
content-negotiation glue.
