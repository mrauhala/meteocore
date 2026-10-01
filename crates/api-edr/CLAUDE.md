# api-edr crate — Claude Instructions

OGC API - EDR HTTP layer. Read the root `CLAUDE.md` first.

## README.md is the EDR status page — keep it current

`crates/api-edr/README.md` holds the conformance-class, query-type,
parameter and per-engine support matrices (what works, what is partial,
what is missing, and the known-gap order). **Any change to EDR behaviour
— this crate, `EdrEngine` in ds-core, an engine's `EdrEngine` impl or its
`supported_query_types` — must update that README in the same PR.** A
reviewer should be able to answer "does engine X support query type Y?"
from the README alone, without reading code.

## CoverageJSON schema compliance (critical)

All CoverageJSON output MUST validate against the OGC CoverageJSON 1.0 schema
at `schemas/coveragejson.json`. Integration tests in
`tests/covjson_validation.rs` validate against it.

**When modifying `src/response.rs` or adding new CoverageJSON output, always
run `cargo test -p api-edr`.**

Key schema rules:

- **Coverage**: requires `type` ("Coverage"), `domain`, `parameters`,
  `ranges`.
- **Domain**: requires `type` ("Domain"), `axes`, `referencing`.
  `domainType` triggers axis constraints.
- **PointSeries**: x/y single-value axes, t string-values axis, z optional
  single-value axis.
- **Grid**: x/y numeric-values axes, t and z optional. NdArray shape:
  `[t, y, x]`, `[y, x]`, `[z, y, x]`, or `[t, z, y, x]`.
- **VerticalProfile**: x/y single-value, z numeric-values axis, t optional
  single-value. NdArray shape: `[z]`.
- **Parameter**: requires `observedProperty` with `label` as an i18n object
  (`{"en": "..."}`).
- **NdArray**: requires `shape` and `axisNames` when values has >1 item;
  `values.length` must equal the product of `shape`.
- **i18n objects**: keys must be BCP 47 language tags (e.g. `"en"`).
- **Reference systems**: spatial uses `GeographicCRS` with the CRS84 id;
  temporal uses `TemporalRS` with `"Gregorian"` calendar.

### Adding a new domain type

1. Add a variant to the `DomainDescription` enum in `ds-core/src/model.rs`.
2. Add a match arm in `build_domain()` in `src/response.rs`.
3. Check the schema's `domainBase.dependencies.domainType` for axis
   requirements.
4. Add a validation test in `tests/covjson_validation.rs`.

Currently implemented: `PointSeries`, `Grid`, `VerticalProfile`.

## Instances (forecast model runs, #337)

The shared machinery is `ds_core::instances` (see root CLAUDE.md). This crate
owns the instance-id string form:

- Routes: `GET /collections/{id}/instances`, `/instances/{instanceId}`,
  `/instances/{instanceId}/{position,area}`. No-instance routes default to
  the latest run.
- Collection metadata gains an `instances` data_query, and the OpenAPI spec
  advertises the instance paths — both gated on `get_instances()` being
  non-empty.
- Unknown instance id → 404 (`select_run` returns `None`).

## Radius queries (#513)

`GET /collections/{id}/radius?coords=POINT(lon lat)&within=<n>&within-units=km|m|mi`
(and the `/instances/{id}/radius` twin) is the area query in disguise:
`EdrEngine::query_radius` has a default that turns the circle into a
64-vertex geodesic `POLYGON` (`ds_core::feature::radius_polygon_wkt`) and
calls `query_area`, so an engine gets radius for free by adding `"radius"`
next to `"area"` in `supported_query_types` — do both or neither. Every
gridded engine returns the circle's bbox as the `Grid` domain with the
cells outside the disc masked to null (a CoverageJSON Grid must be
rectangular); station engines test each point (#671). The
handler 404s a collection that doesn't advertise `radius` (same capability
guard as trajectory), rejects `PNG`, and `data_queries.radius.link.
variables.within_units` advertises the accepted units (`params::WITHIN_UNITS`).
The radius is capped at 1000 km (`params::MAX_WITHIN_M`); a circle
containing a pole or crossing the antimeridian is a 400 (#667). Engine
errors from all data-query handlers map through `map_query_error`.

## Items (#928)

`GET /collections/{id}/items[/{itemId}]` (`src/items.rs`) delegates to the
collection's `FeatureEngine`; there is no `EdrEngine` method for it. The
server fills `EdrState::feature_engines` in `admin.rs`
(`EngineHandle::edr_items_engine`) for every EDR collection whose engine
implements both traits over the same collection (CSV, BUFR, PostGIS stations,
nowcast) — `edr` in `apis` enables it, `features` is not required. Only those
collections get `data_queries.items`, the `/api` paths and a non-404 route,
and never under an instance.

- Reuse, do not re-implement: bbox/datetime parsing, the GeoJSON encoding and
  the paging links come from `api-features` (`params`, `response`) — the one
  API-to-API dependency. A Features change to them changes EDR items too.
- EDR's own parts: `limit` per `/req/edr/rc-limit-*` (default 10, max 10 000
  clamped, anything but a positive integer 400), links under `/edr`, GeoJSON
  only, execution on the EDR executor, ETag over the page with `timeStamp`
  blanked.
- Only `bbox`, `datetime`, `limit`, `offset`, `f` (item: `f`); anything else,
  or a repeat, is a 400 naming them. Adding a Features extension (`sortby`,
  property filters, `crs`) means validating it here and documenting it in
  `openapi_parameters`.
- No `itemType` or `rel=items` link on the collection: the workbench and
  Features clients read those as a Features resource with an HTML view.

## Misc

- **`data_queries` link variables (#918).** `data_query_variables` in
  `src/handlers.rs` is the one table of what each query type advertises:
  `title`, `description`, `output_formats`, `default_output_format` and
  `crs_details` (`params::{DATA_QUERY_CRS, CRS84_WKT}`), all required by
  EDR 1.2. A query type without an arm there is not advertised at all, so a
  new query type needs one. When `crs` lands (#84), its CRSs join
  `crs_details`; keep the WKT longitude-first (`crs84_wkt_is_longitude_first`).
- Cross-section responses (`query_trajectory`, ODIM PVOL) are CoverageJSON
  `Section` with a composite `[t,x,y]` axis + numeric `z` axis. When the
  domain carries `coverage_floor` (#514), the JSON emits it as the
  `meteocore:beamCoverage` **foreign member** on the domain (raw metres,
  one per node) — NOT an axis (the schema forbids extra axes) and NOT a
  parameter (naive clients would plot it as data); the `f=png` heatmap
  draws it as a hatched-below "lowest beam" overlay line.
- `f=png` time-series plots are why this crate (alone among API crates)
  depends on ds-render.
- A location with no data in the requested window returns `LocationNotFound`
  → 404 (an empty PointSeries would fail schema validation).
- When adding endpoints or params, update `api_definition()` in
  `src/handlers.rs` (OpenAPI).
- **Per-parameter time axes (#819).** When
  `EdrEngine::get_parameter_available_times(name)` is `Some`, that
  parameter's `parameter_names` entry carries its own `extent.temporal`
  (EDR 1.1's parameter schema allows an `extent`); the collection extent
  stays the union. Instances keep the run's axis. Built by
  `temporal_extent_json`, the same builder as the collection extent;
  `per_parameter_temporal_extent_validates` pins it against the schema.

## `z` and `datetime` grammar (EDR 1.2, #921)

- `z` is parsed by `params::parse_z` (level, list, closed and open
  intervals, recurring `Rn/min/step` → a list) and resolved by the
  handlers' `resolve_request_z`: a collection with no vertical extent
  **ignores** a well-formed `z` (`/req/edr/z-response` A, a SHALL); a
  malformed one is a 400. Do not reintroduce the old 400 — WMS/Maps/Tiles
  `ELEVATION`/`elevation` keep theirs, EDR does not.
- A `datetime` list `T1,T2,T3` (`params::parse_datetime`) is one engine
  query per instant `(t, t)`, merged by `src/datetime_list.rs`. Every data
  query handler calls the engine through `datetime_list::run`; a new one
  must too, rather than converting the selector to one engine window itself.

## Caching headers (#499)

Every 200 carries `Cache-Control` + a strong content-derived ETag, and a
matching `If-None-Match` short-circuits to 304 — added by the
`caching::conditional_get` middleware wrapping the whole router (an
intentional near-twin of `api-features/src/caching.rs`; the pure pieces are
shared via `ds_core::http_cache`). Policy:

- Default (metadata, "latest"/open-ended queries): `public, max-age=60`.
- *Settled* data queries — closed `datetime` interval whose end is ≥1 h in
  the past — get `public, max-age=86400` via `with_data_cache_control` in the
  data-query handlers. Never `immutable`: observations back-fill and rolling
  retention prunes, so the response can still change; expiry + ETag
  revalidation bounds the staleness to a day.
- A 304 still recomputes the query (no content cache at this layer — EDR
  query keys barely repeat, #202); it saves the transfer, and `max-age`
  saves the request.

A new handler needs no caching code unless its 200 body embeds a per-request
value (a generation timestamp, a random id): a body-hash ETag then never
matches, so precompute the ETag over the body with that field blanked and
set the `ETag` header yourself — the middleware honours it (see the
Features `items` handler).

## Query execution and limits (#178, #585)

Every data query uses `executor`: normal engines run on a dedicated multi-thread
runtime (storage sync bridges are valid there), trajectories use its blocking
pool for their explicit-handle I/O. Never call synchronous engines directly on
HTTP workers. The admission queue is bounded to 32 waiting requests; deadlines include queue
time. Beyond the queue, admission fails fast. Permits live with the
work, including after client timeout/disconnect. Check `QueryBudget::expired`
between MULTIPOINT elements. Validate the whole coordinate list before dispatch;
keep point, byte and combined-value limits and OpenAPI/README documentation aligned.

Position handlers delegate the validated POINT list to `EdrEngine::query_positions`.
The default queries sequentially; engines may share field reads across points.
The response callback enforces the combined value limit and cancellation before
accepting each point. Batch implementations must also check their full allocation
budget before fetching. The executor installs `ds_core::deadline` for storage
and engine loops; propagate it explicitly to field workers and map expiry to 504.

## `limit` and locations paging (EDR 1.2, #922)

`params::parse_limit` is the one parser: 1…`MAX_LIMIT` (10000), larger values
clamp, anything else is a 400, absent means no limit. Every data-query handler
that returns CoverageJSON passes its result through `limit_coverages` (top-level
coverages only); position also truncates the point list before dispatch, which
is safe because an answered point always yields at least one coverage. A new
data-query route must do the same and list `#/components/parameters/limit` in
`api_definition()`. `/locations` without `limit` must stay byte-identical to
the unpaged inventory (clients and ETags rely on it); with `limit` it pages
through `ds_core::collection_search::page_window`, the `/collections`
arithmetic, under the same `location_budget` writer.

## Shared Common discovery (#739)

Use `api-common` for `/collections` validation, responses, links, Common metadata
fields, OpenAPI operation definitions and Common conformance declarations. The
adapter supplies engine extents; search and advertised metadata must agree.
See `crates/api-common/CLAUDE.md` and update `docs/ogc-api-common-matrix.md` when
changing this behavior. Do not restore the Part 4 class without implementing
and checking the recorded draft's remaining requirements.
