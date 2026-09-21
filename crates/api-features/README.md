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
| Part 1 `geojson` | ✓ | GeoJSON feature encoding (default) |
| Part 1 `html` | ✓ | metadata, feature pages and individual features negotiate HTML; property tables, geometry details and a map using the preview’s vendored MapLibre assets |
| Part 1 `gmlsf0` / `gmlsf2` | ✗ | no GML |
| Part 2 CRS by reference (`crs`) | ✗ | CRS84 only; `crs` on `/items` returns 400; collections advertise `crs: [CRS84]` + `storageCrs` |
| Part 3 Filtering / CQL2 (`filter`, `queryables`) | ✗ | `filter` controls return 400; no `/queryables` |
| Part 4 Create/Replace/Update/Delete | ✗ | read-only server by design |
| Part 5 Schemas | ✗ | no `/schema`; properties are untyped `PropertyValue`s |
| Part 8 Sorting (draft) | partial | `sortby` implemented (#605 rule: validated against `FeatureEngine::sortables`, 400 naming the valid ones) but the class is not declared and no `/sortables` resource exists |

Also declared: OGC API - Common Part 1 (core, landing-page, oas30) and
Part 2 (collections, json, html). Collection discovery supports `bbox`,
`bbox-crs` (CRS84 only), `datetime`, `q`, `query`, `limit`, `offset` and `f` through
[api-common](../api-common/README.md). Unknown/unsupported or duplicate controls
return structured HTTP 400 errors. Filters run before paging; JSON/HTML links
preserve filters and the negotiated format. Collection descriptions expose HTML
alternate links and configured keywords/license metadata through the shared helper.

Text discovery supports whitespace-normalized whole-word phrases in `q`, and
`query` adds required (`+`) and excluded (`-`) terms within comma-separated OR
alternatives. Encode `+` as `%2B`; `q` and `query` are ANDed when both supplied.
See [the shared text-search contract](../api-common/README.md)
for examples and validation choices.

Part 4 `searchable-collections` is **not declared**: the 2026-09-17 draft also
requires `sd` and `resolution`, which are not implemented. Existing
basic search remains supported. See the [Common Parts 1–4 matrix](../../docs/ogc-api-common-matrix.md)
for the specification baselines and remaining gaps.

Features collection search now uses `FeatureEngine::temporal_extent()`, matching
the advertised metadata. Unknown extents remain eligible; temporal bounds do not
imply a regular sampling grid.

## Routes

| Route | Status | Notes |
|---|---|---|
| `/features/` | ✓ | JSON + HTML |
| `/features/api`, `/api/docs` | ✓ | OpenAPI 3.0 + Swagger UI |
| `/features/conformance` | ✓ | JSON + HTML |
| `/features/collections` | ✓ | JSON + HTML; Common Part 4 search |
| `/features/collections/{id}` | ✓ | JSON + HTML; `extent.spatial` from `spatial_extent`, `extent.temporal` from `temporal_extent` (omitted when `None`); `keywords`, `license` link; a `tilesets-vector` link when the collection also lists `tiles` in `apis` |
| `/features/collections/{id}/items` | ✓ | GeoJSON `FeatureCollection` or HTML with `numberMatched`, `numberReturned`, `timeStamp`, `self`/`next`/`prev` links that carry the caller's filters and sort |
| `/features/collections/{id}/items/{featureId}` | ✓ | GeoJSON `Feature` or HTML with `self`, `alternate` + `collection` links |
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
| `f` | ✓ | `json` (GeoJSON for features) / `html` on metadata, `/items` and `/items/{featureId}`; overrides `Accept`; feature routes also accept `application/geo+json` (encode `+` as `%2B`), `application/json`, and `text/html` aliases, case-insensitively; unsupported formats → 400. HTML pagination retains format, filters and sort |
| `crs`, `bbox-crs` (on `/items`) | ✗ | 400; CRS84 only |
| `filter`, `filter-lang`, `filter-crs` | ✗ | 400 (CQL2 is not implemented) |
| `properties` | ✗ | 400; every property is always returned |
| `<property>=value` (Part 1 §7.15.5–6 optional property filters) | ✓ | validated against `FeatureEngine::filterables`; unknown names → 400 listing valid ones; exact strings, list membership, canonical numbers/bools; numeric comma-separated alternatives; predicates ANDed before counting/sorting/paging |

Property filter names are case-sensitive. Values are exact (no wildcards,
substring search or numeric coercion). Numeric properties additionally accept
comma-separated alternatives: `size=10,30` means 10 OR 30, with no spaces.
Commas remain literal in string properties; this is not generic text OR or CQL2.
Integers/floats use Rust's shortest
`Display` string (`12`, `1.5`), booleans `true`/`false`; missing, null and
non-finite values never match. Multiple predicates, including repeated names,
are ANDed with each other and with `bbox`/`datetime`. For a list, each predicate
can match a different element. Empty strings remain valid literal values.
Names and values are URL-encoded in every `self`/`next`/`prev` link, including
numeric alternative lists.

`/features/api` lists the accepted property parameters per collection from
cached engine catalogs. CAP, GeoJSON and PostGIS advertise names present in
at least one loaded record (including null values), refreshed with the data.
CSV, BUFR and ODIM use fixed schemas; Nowcast includes optional property groups
only when their sources are wired. Reserved API controls (`bbox`, `datetime`,
`limit`, `offset`, `sortby`, `f`, `crs`, `bbox-crs`, `filter`, `filter-lang`,
`filter-crs`, `properties`) take precedence over source property names and
cannot be used as property filters. Duplicate control parameters return 400.
This does not declare Part 3 conformance or add `/queryables`.

The GeoJSON engine preserves flat arrays of scalars (including empty arrays)
as typed lists, so JSON responses retain arrays, HTML can display chips, and
property filters match list elements. Strings containing JSON remain strings.
Objects and arrays containing nested structures retain the engine's existing
JSON-string fallback; they are not represented as nested typed properties.

Example: `/features/collections/cap-meteoalarm-wis2/items?awareness_type=1%3B%20Wind&severity=Severe`.

CAP also exposes `awareness_type_code`, derived from the positive integer
prefix of MeteoAlarm's `code; label` convention. Original `awareness_type`
values remain unchanged. Repeated awareness types produce a list of numeric
codes; malformed values contribute no code. A producer parameter named
`awareness_type_code` is preserved as `parameter:awareness_type_code`.
The derived code remains queryable when the collection has no valid codes or
no alerts, returning no matches rather than an unknown-property error.

One request selects three types across label capitalization variants:
`/features/collections/cap-meteoalarm-wis2/items?awareness_type_code=1,3,5`.
A single code is `?awareness_type_code=3`; add `&severity=Severe` for an AND
condition. This remains exact numeric equality, not a text prefix match.
The code convention is defined in the
[MeteoAlarm CAP profile](https://gitlab.com/meteoalarm-pm-group/documents/-/raw/master/MeteoAlarm_CAP_Profile_v2.0.pdf), §2.2.17.


Every 200 carries `Cache-Control` + a strong ETag; `If-None-Match` → 304
(#499). Both feature routes send `Vary: Accept`, explicit-format pagination and alternate
links, and representation-specific ETags. `/items` hashes the selected
representation with `timeStamp` blanked, and a closed
`datetime` window entirely in the past gets the long cache policy.

Error bodies are `{ "code", "description" }`; 500s never leak internals.
Structured `ErrorReason` codes are #119.

`/api/docs` uses embedded Swagger UI 5.33.0 assets, served under
`/api/docs/{asset}`, with a same-origin script policy and `nosniff` headers.
No executable documentation assets or validation requests use a CDN (#587).

## Geometry types produced

`Point`, `Polygon`, `MultiPolygon`, and `null` (CAP geocode-only areas).

Feature IDs are opaque domain strings; URL path segments in advertised links
are percent-encoded once by the API. This includes CAP sender-scoped IDs with
URL-shaped senders, slashes, brackets, literal percent sequences or Unicode.
CAP IDs now retain those characters in JSON rather than pre-encoding them in
the engine; producer `sender`/`identifier` properties are unchanged. Follow
the advertised links, or encode the complete ID as one path segment when
constructing a URL. Existing correctly encoded URLs remain valid.
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

CSV pagination uses an immutable station inventory built at load time, in
first-observation order. Unfiltered pages slice that inventory directly;
filtered pages count matching stations but clone only the requested page.
Requests never scan the CSV observation history or rebuild all station features
(#532). Other engines retain their own filtering/sorting/paging strategies;
responses are buffered rather than streamed.

## Known gaps, in suggested order

1. Reject or honour query parameters on `/items/{featureId}` (#681);
   `/items` now rejects unsupported/unknown names (#700). Honour or reject
   `datetime` on engines without a time dimension (#682).
2. Declare Part 8 sorting + serve `/sortables` (#683).
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

Nowcast tracks survive one missed detection for reassociation only: the missing
cell is absent from that generation's features, and `track_age` counts observed
frames. Startup reconstructs up to eight retained source frames without publishing
historical forecast runs. Lightning fields require explicitly known source
coverage of the labeled footprint and attribution radius. With a source wired,
`lightning_coverage` is `true`, `false`, or `null` (unknown); flash fields are null
outside/unknown coverage and on failed or capped joins. An advertised source
extent alone does not establish coverage.

Compatible nowcast reloads preserve retained cell snapshots and track IDs (#604).
Edited GeoJSON impact areas are used by the next generation; past snapshots
keep their original facts. Replaced lightning sources restart the jump baseline
without restarting radar tracks. Reuse requires unchanged nowcast config, the
same raster engine and a matching retained geometry/product contract. Missing
or invalid configured dependencies still fail load validation.

Nowcast cells have a 2.5 km² minimum footprint, summed at each pixel row's
latitude. `area_km2`, severity and flash density use that physical area;
tracking distances and speeds use local latitude. Working resolution depends
on the source and configured pixel budget.

BUFR decoding supports compressed character fields, operator 208, and numeric
fields through 64 bits. Unsupported operators or unknown national descriptors
skip the affected message (counted in `bufr_decode_failures_total`); other
messages in the same file remain available. See
[`engine-bufr` decoder notes](../engine-bufr/CLAUDE.md#the-decoder-boundary).

### HTML workbench

Metadata and items share the `api-common::workbench` navigation, themes and
current-resource JSON switch. The item query builder offers bbox/paging plus
temporal, exact property and sort controls advertised by the selected engine.
Repeated predicates and intentional empty predicates from an existing URL are
preserved. Collection search uses Common `q`/`query`; item queries keep their
existing Features semantics.

Individual items show every property with its JSON type, distinguishing null,
empty strings, arrays and nested objects. Flat arrays render as visible wrapping
chips, including in selected listing columns; empty arrays are labelled explicitly.
Nested arrays and objects retain expandable JSON views. The renderer does not assign weather
semantics, units, severity, expiry state or observation times based on property
names. Display titles use case-insensitive common label keys in priority order:
`name`, `label`, `title`, `display_name`, `displayname`, `nimi`, `namn`, `nom`,
`nombre`, `naam`, `bezeichnung`, then the feature ID. Coordinates are rounded to
five decimal places in the summary; raw geometry retains full precision.

Listings derive selectable columns from response properties and advertised
filter fields, defaulting to the first four non-null scalar fields other than
label keys. Up to eight property columns can be selected, with a per-collection
session preference. Missing properties are shown as **Absent**, separately from
JSON null. Stored columns survive pages where those fields are absent. Column
choices never change API requests or JSON output. Without JavaScript, the default
columns, paging and item links still work.

The response table shows every row returned for the requested limit and grows
with the page, without a separate vertical scrollbar. Horizontal scrolling is
available for wide sets of selected columns. The map sits to the right on wide
screens and below the table on narrow screens. Paging and spaced page-size
controls appear above and below the table. An empty offset with
matches offers a first-page link that retains filters, repeated/empty predicates
and sorting. The request builder and applied/draft URL tools are collapsible;
builder and advanced-filter disclosure choices persist per collection;
applied filters stay visible and the global JSON switch retains the applied URL.

The MapLibre locator shows this response page over locally bundled Natural Earth
outlines, with a generic property quick look and item links. It makes no additional
feature or external tile requests. Raw geometry is displayed on item details,
not repeated beneath the listing map. JavaScript enables query editing, property
column selection, map interaction and property search. Server HTML remains
deterministic. Response generation timestamps are labelled explicitly.

In HTML, the collection’s **Request data** tab opens the `/items` data request
builder. **Request features** applies filters within that collection; collection
discovery remains under **Collections → Find collections**.

HTML breadcrumbs display collection and item titles when available; resource
URLs continue to use the original IDs. Parent titles are passed from the existing
collection snapshot, without additional data queries.

Collection advanced search starts closed and retains the user's open/closed
choice across searches in the browser session. Resource URLs are clickable.

HTML catalogs now use compact search and metadata summaries, retain UTC time
precision, and distinguish empty offsets from zero matching collections. Returning
to the first page preserves discovery filters; list/cards preference persists.

The shared collection HTML renderer displays vertical bounds and available levels
when advertised in metadata. Features collections without a vertical extent
continue to omit that dimension.
