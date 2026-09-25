# api-common — shared Common HTTP layer

Read the root CLAUDE.md. This crate may depend on Axum/serde_json and ds-core,
but not engines or the concrete API crates. Keep framework-free policy in ds-core.

- Collection control names and validation are based on `CollectionParameter`
  and `SearchQueryParams::from_pairs`; do not add a second name allowlist here.
- `collection_operation` generates the discovery OpenAPI operation for all four
  APIs. Keep schemas, defaults, validation and response representations aligned.
- Text search follows recorded draft 25-046 §7.6–7.7: comma OR, local required/
  excluded query terms; phrases never span fields or keyword entries. q and query
  combine with AND. Keep operator encoding (`%2B`) intact through navigation.
  See README.md for explicit tokenizer and malformed-input choices.
- Apply filters before paging. Navigation preserves supported filters and the
  negotiated format, including requests that used Accept without an explicit f.
- Adapters supply metadata and extents from their registry snapshot. Time bounds
  and advertised collection metadata must agree. Preserve EDR's distinct extent
  representation. Do not infer a sampling grid from feature interval endpoints.
- Never hard-code an API path such as `/maps/…` in a Maps/Tiles/Features link, OpenAPI key
  or HTML URL: build it from `Mount::root(base)` (the `Mount` extension added by
  `router_at`). Cross-API links use `mounts::*`. Pass the workbench a `Surface`;
  gate its data panels on advertised links, not on the API kind (#789).
- Shared OGC API root (`shared.rs`, #789): blocks contribute routes, conformance,
  OpenAPI fragments and per-collection `Contribution`s; the composer owns `/`,
  `/api`, `/conformance`, `/collections`, `/collections/{id}`. Merge rules: links
  concatenate in block order, the first block to describe a field wins unless an
  earlier block `claims` it (raster blocks claim `storageCrs`), `styles` merge by
  id. Block order (Maps, Tiles, Features in `server/src/main.rs`) is therefore
  field precedence; discovery bounds follow the kept extent. Blocks must keep
  overlapping OpenAPI components identical — or namespace them, as Features
  does — and operation ids unique (tested); one route per path across blocks
  (the legend belongs to Maps). Tag every block router with its API kind
  (`tag_api_kind`).
- Shared metadata owns keywords, license and representation links. API-specific
  fields may override id/title for instances; do not override shared links.
- Conformance declarations are deliberate. Part 4 searchable-collections is absent
  until all mandatory requirements of the recorded draft are implemented/tested.
- Update README.md here, affected API READMEs and docs/ogc-api-common-matrix.md.
  Run cross-API contracts (`cargo test -p server --test common_discovery`) and the
  affected API suites. Do not require identical real production catalogs.
