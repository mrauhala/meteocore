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
- Apply filters, then `sortby`, before paging. Navigation preserves supported
  filters and the negotiated format, including requests that used Accept without
  an explicit f. JSON and HTML default differently (id and 1,000; title and 24,
  #1141), so an alternate link must spell out `limit` and `sortby`
  (`SearchQueryParams::explicit_page`) to name the same page.
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
  id, `crs` lists union in block order (Features Part 2: a feature collection's
  list must be exactly what `/items` accepts, #685). Block order (Maps, Tiles, Features in `server/src/main.rs`) is therefore
  field precedence; discovery bounds follow the kept extent. Blocks must keep
  overlapping OpenAPI components identical — or namespace them, as Features
  does — and operation ids unique (tested); one route per path across blocks
  (the legend belongs to Maps). Tag every block router with its API kind
  (`tag_api_kind`). The per-API Maps/Tiles/Features services stay routed but
  are not advertised: not in the landing links (`related_services()` in
  `server/src/main.rs`) nor the workbench switcher (`SUPERSEDED`).
- Shared metadata owns keywords, license and representation links. API-specific
  fields may override id/title for instances; do not override shared links.
- Conformance declarations are deliberate. Part 4 searchable-collections is absent
  until all mandatory requirements of the recorded draft are implemented/tested.
  A class that holds only in the shared layout goes in the block's
  `shared_conformance`, never in its per-API list: Maps "Map Tilesets" needs
  `{map}/tiles`, which only the shared root has (Tiles declares it there, #946).
  `common_discovery` pins each declaration against the links and OpenAPI
  parameters it requires. Tiles DateTime covers map and vector tiles; both
  parse time through `subset`, and vector tiles hand it to the engine as
  `FeatureQuery::datetime`.
- `workbench/map.js` prints a property number by the rule of
  `metadata::number_text` (#1140); change both together. Its tests run the
  shipped script on a stub DOM and MapLibre over `tests/fixtures/map-panel.html`,
  the panel as `map_html` renders it: `node --test crates/api-common/tests/map.test.cjs`
  (CI). After a deliberate markup change regenerate the fixture with
  `UPDATE_MAP_FIXTURE=1 cargo test -p api-common map_panel_fixture`. It also
  keeps the map's view as a CRS84 box in the map element's `data-view`,
  which the items builder's "Use map view" reads (#1143).
- The collection map (#1220): every request reads one drawn-state object that
  the controls and the time bar write (`apply`, `goTo`); never read a control
  in a render path. Only `stageFrame` adds data layers: a new raster source at
  opacity 0 with zero `raster-opacity-transition` and `raster-fade-duration`.
  `swapTo` shows a loaded frame in one paint; the old stays hidden in the pool
  while it is one of the frames ahead, else it is removed after that paint.
  Never remove or `setTiles` the visible source. A 404 tile fires no MapLibre `error`: no tile
  loaded and none failed means no data. The controls form is a no-JS GET to the
  Maps endpoint whose only named fields are Maps parameters (api-maps
  `workbench_form` submits it); script-only controls, the time bar and the WMS mode
  stay unnamed or outside it. The fragment is the map's only when empty, `#overview` or its own keys;
  never overwrite `#metadata`.
- The time bar (#1220): `mapRequest.axis` is `{start, step, count}` (step in
  seconds) or `{times}`, never an option per step; a parameter's own axis is
  on its `parameter_names` entry. Every frame names its time from the axis.
  Nothing is buffered until the viewer uses the bar; a frame not yet asked for
  goes before prefetch; tiles stage one prefetch frame at a time, images fetch
  at most two. The status line announces steps, pauses and errors, never a
  played frame; the slider moves only on a viewer's move or a pause. No
  autoplay. WMS mode comes from the collection's `apis`
  (`shared::wms_endpoint`): the collection document names no WMS, so this is
  the one map control gated on config rather than an advertised link.
- The Overview's "Filter by" / "Sort by" chips (`ItemsOverview`, carried by
  `Contribution::items`, rendered by `collection_page`) open the items
  builder through the URL fragment (`#filter=<name>`, `#sort=<name>`), never
  a query parameter: `/items` rejects unknown ones (#1143). The stylesheet
  and scripts are inline, so a page test asserting an element's absence
  matches its markup, not a bare class name.
- Update README.md here, affected API READMEs and docs/ogc-api-common-matrix.md.
  Run cross-API contracts (`cargo test -p server --test common_discovery`) and the
  affected API suites. Do not require identical real production catalogs.
