# Shared OGC API Common HTTP layer

EDR, Maps, Tiles and Features use this crate for collection discovery. Pure search
and HTML/extent types remain in `ds-core`; this crate owns Axum/JSON plumbing.
API adapters keep their engine registries, access links, extent representations
and API-specific fields.

| Shared component | Responsibility |
|---|---|
| `CollectionRequest` | Decode controls, reject unsupported/duplicate names, validate values, negotiate representation before consulting engines |
| `collections_response` | Sort by ID, filter before paging, counts, JSON/HTML, explicit-format self/next/prev and alternate links |
| `collection_metadata`, `collection_card` | Common descriptive fields, keywords, license and HTML alternate links; API fields may override defaults for EDR instances |
| `collection_operation` | OpenAPI discovery operation, supported parameters, JSON/HTML responses and structured 400 errors |
| `CONFORMANCE_CLASSES` | One Common class inventory, combined with each API's own declarations |

The supported parameter names come from `ds_core::collection_search::CollectionParameter`;
the same inventory drives pair validation and OpenAPI generation.

| Control | Behavior |
|---|---|
| `bbox` | Four/six comma-separated coordinates, horizontal intersection; antimeridian supported, vertical bounds ignored |
| `bbox-crs` | CRS84 only, including the existing URI/CURIE/short aliases |
| `datetime` | RFC 3339 instant or interval overlap; unknown temporal extents remain eligible |
| `q` | Case-insensitive title/description/keyword search; comma-separated OR and literal phrases |
| `limit` | Default/max 1,000; values above max clamp; invalid values and zero return 400 |
| `offset` | Nonnegative number of matching collections to skip; default zero |
| `f` | `json` or `html`, overrides Accept; navigation retains the selected format |

Unsupported controls (`query`, `sd`, `resolution`, `sortby`, CQL2 and hierarchy
controls, for example) and repeated controls return JSON `{code, description}`
with HTTP 400. This applies to collection lists; it does not change the parameter
contracts on feature items, maps, tiles, EDR data queries or instance lists.

Features supplies feature temporal bounds; Tiles uses raster times when present
and feature temporal bounds as a fallback. Metadata and filtering use the same
precedence. Bounds alone are not advertised as a sampled time grid.

The Common Part 4 Searchable Collections URI is intentionally not advertised:
[draft 25-046](https://docs.ogc.org/DRAFTS/25-046.html), retrieved 2026-09-17,
also requires `query`, `sd` and `resolution`. Removing the former declaration
does not remove working basic search. The
[Parts 1–4 matrix](../../docs/ogc-api-common-matrix.md) records the exact baselines
and other limitations; retained declarations are not a certification claim.

Cross-API contracts live in
[server/tests/common_discovery.rs](../server/tests/common_discovery.rs):

```sh
cargo test -p server --test common_discovery
cargo test -p ds-core -p api-edr -p api-features -p api-maps -p api-tiles
```
