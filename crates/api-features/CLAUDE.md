# api-features crate — Claude Instructions

OGC API - Features HTTP layer. Read the root `CLAUDE.md` first.

## README.md is the Features status page — keep it current

`crates/api-features/README.md` holds the conformance-class, route,
parameter and per-engine support matrices (what works, what is partial,
what is silently ignored, what is missing, and the known-gap order). **Any
change to Features behaviour — this crate, `FeatureEngine` / `FeatureQuery`
/ `Bbox` in ds-core, or an engine's `FeatureEngine` impl (including
`sortables`, `spatial_extent`, `temporal_extent`, `data_version`) — must
update that README in the same PR.** A reviewer should be able to answer
"does engine X honour `datetime` on `/items`?" from the README alone,
without reading code.

## Rules specific to this crate

- **Unknown or unsupported query parameters must not be silently ignored**
  (#605). serde drops unrecognized fields, so a parameter that is parsed but
  never validated returns 200 having done nothing. `sortby` is the model:
  validate against what the engine advertises (`FeatureEngine::sortables`)
  and return 400 naming the valid options. The README's parameter table lists
  the parameters that still violate this — shrink that list, never grow it.
- **Sort before paging.** Engines that advertise sortables apply `sortby`
  via `ds_core::feature::sort_features` before `offset`/`limit`; sorting a
  slice returns the wrong rows.
- **Pagination links carry the caller's filters and sort** (`preserved_query`).
  A `next` link that drops them serves page 2 unfiltered. Past the end,
  `prev` is the last page with features, not another empty one (#1143).
- **Engines name, this crate addresses (#1143).** A feature's label comes
  from `FeatureEngine::label` (served as the `title` property unless the
  feature has one), its first HTML columns from `summary_properties`, and
  where it leads from `Feature::related` targets, which
  `response::Annotations` resolves below the base URL or drops when no API
  here serves them (`FeaturesState::collection_pages`, `edr` in `apis`).
  `html.rs` never interprets weather property names, and an engine never
  builds a URL.
- **`/items` precomputes its ETag with `timeStamp` blanked**; the
  `caching.rs` middleware honours a handler-set ETag. It is a near-twin of
  `api-edr/src/caching.rs` — keep the two in sync (ds-core is
  framework-free, so the axum glue cannot be shared; #306).
- **An engine's error maps to its class's status** (`handlers::engine_failure`,
  both `/items` and `/items/{featureId}`, by `DataServerError::class()`, #17):
  `BadRequest` a 400 with the engine's message, `NotFound` a 404
  (`FeatureNotFound`'s body stays "Feature not found"), `Busy` /
  `DeadlineExceeded` a 503 with `Retry-After: 1`, `SourceUnavailable` the
  shared 503, and `Internal` a generic 500. `get_features` runs on the request worker
  with no deadline, so an engine must answer from memory or fail fast,
  never block (the station grid's residuals answer 503 while computing).
  The one exception is an engine whose `reads_on_request` is true (PostGIS
  `events`, #503): both reads go through `executor::read`, onto
  ds-executor's shared query runtime under this crate's own admission and
  30 s deadline, the twin of api-edr's `executor`. Never call such an
  engine directly from a handler: route every new engine call through
  `executor::read`.
- **Two ways to page (#503).** An engine pages by `offset`, or by cursor
  when `pages_by_cursor` (a source read in key order, where an offset would
  re-read every row before it). `items_request` turns `cursor` down on the
  first kind and a non-zero `offset` on the second; `feature_page_to_geojson`
  links `next` with the page's `next_cursor` and gives a cursor page no
  `prev`. A cursor is the engine's: this crate only percent-encodes it.
  `numberItems` comes from `number_items`, `None` leaving it out.
- **A default window is named once per request (#503).** Without
  `datetime`, `items_request` asks `FeatureEngine::default_datetime(now)`
  (whole seconds) and passes the answer as the query's `datetime`;
  `feature_page_to_geojson` takes `PageLinks { this, onward }`, `self`
  carrying the request and `next`/`prev` that interval. Never let a chain
  recompute a window ending now per page: newest first, it drops the
  oldest features the chain has yet to reach
  (`a_default_window_chain_reads_one_fixed_interval`).
- **`FeaturePage::number_matched` is optional (#503).** `None` leaves
  `numberMatched` out of the document (never `null`; OGC makes it a
  recommendation), and the HTML page then states only what it shows. Every
  offset-paged engine reports `Some`. `feature_page_to_geojson` keeps the
  members in their old order, so a counted page's bytes and ETag are as
  before (`number_matched_is_written_only_when_known`).
- **Part 2 CRS lives in `src/crs.rs`** (#685). `crs::SUPPORTED` is both the
  advertised collection `crs` list and the set `/items` accepts in `crs` and
  `bbox-crs` — change them together. Projection math comes from
  `ds_core::web_mercator` / `ds_core::geo` (Critical Rule 4); this crate only
  picks the transform and the EPSG axis order (EPSG:4326 lat/lon, EPSG:3035
  northing/easting). Output is per vertex (Critical Rule 5); a projected
  `bbox-crs` becomes the envelope of densely sampled edges, never two corners.
  HTML is CRS84 only.
- **`api_definition()` is hand-written `serde_json::json!`** and validated
  against `schemas/openapi-3.0.json` in tests. New route or parameter ⇒
  update it in the same PR; copy standard parameter schemas verbatim,
  including `style`/`explode`.
- **EDR `items` reuses this crate** (#928): `params::{parse_bbox,
  parse_datetime}` and `response::{feature_to_geojson,
  feature_page_to_geojson, preserved_query}` also serve
  `/edr/collections/{id}/items`. A change to them changes EDR items too; run
  `cargo test -p api-edr --test items_tests` with them.

## Mount-agnostic, and a block of the shared root (#789)

Build every link, OpenAPI path key and HTML URL from the router's `Mount`
(`Mount::root(base)`; `router_at(state, mount)` adds it) — never a `/features`
literal. `collection_parts` (with `Layout::PerApi` / `Layout::Shared`),
`items_openapi_paths` and `openapi_components` are shared by the per-API
service and `FeaturesBlock` (`shared.rs`); change them once. At the shared root
the block's OpenAPI components are namespaced (`features-*`), and only
collections with a feature engine get `itemType`/`items`.

## Shared Common discovery (#739)

Use `api-common` for `/collections` validation, responses, links, Common metadata
fields, OpenAPI operation definitions and Common conformance declarations. The
adapter supplies engine extents; search and advertised metadata must agree.
See `crates/api-common/CLAUDE.md` and update `docs/ogc-api-common-matrix.md` when
changing this behavior. Do not restore the Part 4 class without implementing
and checking the recorded draft's remaining requirements.
