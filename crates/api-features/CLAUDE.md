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
  A `next` link that drops them serves page 2 unfiltered.
- **`/items` precomputes its ETag with `timeStamp` blanked**; the
  `caching.rs` middleware honours a handler-set ETag. It is a near-twin of
  `api-edr/src/caching.rs` — keep the two in sync (ds-core is
  framework-free, so the axum glue cannot be shared; #306).
- **`api_definition()` is hand-written `serde_json::json!`** and validated
  against `schemas/openapi-3.0.json` in tests. New route or parameter ⇒
  update it in the same PR; copy standard parameter schemas verbatim,
  including `style`/`explode`.

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
