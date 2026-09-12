# engine-cap crate — Claude Instructions

OASIS Common Alerting Protocol (CAP) v1.2 alert engine (#396). Implements
BOTH `FeatureEngine` (one Feature per alert area) and `MapEngine`
(severity-shaded polygon fill) over one poll-and-swap `Catalog`. It is the
first vector→raster `MapEngine`: it fills alert polygons into the output
pixel grid with `ds_render::rasterize::fill_polygon` (#397) fed by
`ds_core::geo::geometry_to_pixels` — vertices projected via
`OutputCrs::world_to_fraction`, never per pixel. Its ds-render dependency is
an approved exception (root CLAUDE.md, Shared Domain Machinery).

## The load-bearing gotcha: coordinate order

**CAP polygons/circles are `lat,lon` (spec §3.3.4); `ds_core::Geometry` is
`[lon, lat]`. `src/parser.rs` swaps on ingest** — pinned by an
absolute-position test (a Helsinki alert must land at lon≈25, lat≈60).
Rings are closed defensively; `<circle>` → an N-gon (`circle_segments`,
default 64) on the geodesic via `destination_point`, carrying `radius_km`
as a property.

## Source & SSRF guard

- **Exactly one of `data_path`** (local dir of `*.xml`), **`feed_url`**
  (Atom/RSS index → linked CAP docs) **or `[cap.wis2]`** (WIS2 push, see
  below). The first two go through `ds-storage` from the background poll
  runtime only. The feed fetches the index then the linked
  docs with `DataStore::get_many` (bounded concurrency, per-object timeout,
  origin-grouped) — never a sequential blocking loop.
- **Feed SSRF guard:** an entry link is fetched only if it shares the feed's
  EXACT origin (scheme+host+port — not a prefix; `https://feed` rejects
  `https://feed.evil.com`) or matches an explicit `feed_allowlist` URL
  prefix; others are dropped with a WARN. Stops a compromised feed pivoting
  the server to `http://169.254.169.254/…` or internal hosts.
- Known limitation: the allowlist constrains request URLs, not redirect
  responses (object_store's reqwest client follows redirects; still no
  disable knob as of object_store 0.14). A proper fix belongs in
  ds-storage (#431).
- Config (`CapConfig` in ds-core) validated at load: exactly one of
  `data_path` / `feed_url` / `[cap.wis2]`, `feed_url` http(s), non-empty
  `language`, `poll_interval_secs > 0`, positive ISO 8601 `default_ttl` /
  `retention_grace`, `circle_segments >= 3`, `max_alerts > 0`,
  `validate_wis2` for the subscription.

## WIS2 mode (`[cap.wis2]`, `src/wis2.rs`)

Push instead of poll: the engine subscribes to WIS2 Global Broker topics
through `ds-wis2` (read `crates/ds-wis2/CLAUDE.md` first — sessions, the
6×-per-cache duplicate fact, download policy) and accumulates alerts in
memory. Things that differ from the pull sources:

- **Lifecycle.** `new()` builds the accumulator only — no network — and the
  collection boots `Degraded("connecting to WIS2 broker")`. `poll_loop()`
  starts the pipeline (it is the only entry point guaranteed to run on
  `poll_runtime()`; the constructor runs on the request runtime), applies
  every resolved notification, rebuilds the catalog at most every 5 s when
  dirty and unconditionally every `poll_interval_secs`. `live_health()`
  reports the session (`/health` overrides the boot status at runtime;
  a disconnect shorter than `degrade_after_secs` is not surfaced — the last
  catalog keeps serving and the session resumes its QoS-1 backlog).
- **The 5 s dirty floor is load-bearing.** Every rebuild sets `as_of =
  now`, and `as_of` is the TIME-less WMS cache key; two catalogs must never
  share one second. The forced rebuild keeps expiries evicting when the
  feed is quiet.
- **Accumulator semantics** (`Wis2CapSource`): one entry per CAP
  `<identifier>`, newest `pubtime` wins; `rel=deletion` withdraws the alert
  its `data_id` produced and leaves a tombstone so a late copy from another
  Global Cache cannot resurrect it (a genuinely newer re-issue can);
  eviction once every info's validity end (`<expires>`, else onset +
  `default_ttl`, else receipt + 7 d) is more than `retention_grace` (PT1H)
  in the past; `max_alerts` (10 000) evicts oldest-received first.
- **Supersede/Cancel is NOT WIS2-specific.** `supersede::resolve_references`
  runs in `refresh()` for every source mode: newest `<sent>` per identifier,
  identifiers named in an `Update`/`Cancel` `<references>` are withdrawn,
  `Cancel`/`Ack`/`Error` are never rendered. Counted in
  `cap_alerts_superseded_total`.
- **MeteoAlarm geometry.** The hub's CAP XML is geocode-only (NUTS3 /
  EMMA_ID, no `<polygon>`), but each notification (one per alert × info ×
  area, `indexInfo`/`indexArea` 0-based in document order) carries a
  `rel=geometry` link to the exact zone polygon. With `geometry_links`
  (default on) it is downloaded **on arrival** — the links are pre-signed
  and expire about an hour after publication, so a late replay cannot
  recover them — sanity-checked against the notification bbox, and attached
  to that one area as `CapArea.hint_geometry`. `build_geometry` order:
  inline polygons/circles → `geocode_geometry` lookup → hint → (opt-in)
  notification bbox. `properties.geometry_source` says which
  (`inline|geocode|notification|bbox`). `bbox_fallback` is off by default:
  a bounding box drawn as a warning area misleads; opt in per feed. When
  on, the bbox is scoped like the exact hint — only the `(indexInfo,
  indexArea)` the notification names; a notification with no indices
  (whole-document producers) fills every geometry-less area. Both the
  lookup file and the hints can be configured together.
- `cap_alerts_superseded_total` counts each withdrawn identifier once
  (`superseded_ids` = the set as of the last rebuild); a cancelled alert
  lingering in the source/accumulator is not re-counted every rebuild.
- Tests use `CapEngine::refresh_at(now)`: the WIS2 accumulator evicts by
  clock, so a wall-clock `refresh()` empties a catalog built from captured
  documents once they age past `<expires>` + grace (this bit CI at 17:16Z).
- **Fixtures** for the offline tests live in `tests/wis2-fixtures/` (NOT
  under `tests/fixtures/` — the directory source lists recursively and
  would pick the CAP XML up as a demo alert).

## Feature model

- **One Feature per `(alert, info, area)`**, id =
  `{identifier}.{infoIdx}.{areaIdx}` (stable, URL-safe). The emitted
  `Feature.id` is **percent-encoded** to a single URL path segment so the
  api-features verbatim self-link routes; axum's `Path` decodes it back.
  **Clients must use `Feature.id` as-is, not re-percent-encode it**; the raw
  CAP `<identifier>` is in `properties.identifier`.
- Multiple `<info>` (languages) and multiple `<area>` per info each fan out.
  `language` config keeps matching `<info>`s (primary-subtag,
  case-insensitive), falling back to the first info. `status_filter`
  (default `["Actual"]`) drops Test/Exercise/Draft at the alert level.
- **Geocode-only areas** (UGC/EMMA_ID/FIPS, no polygon/circle) get geometry
  from the optional `geocode_geometry` lookup — a GeoJSON FeatureCollection
  mapping zone codes → polygons (`geocode_property`, default `"code"`;
  `geocode_value_name` restricts which `<geocode>` valueName resolves, e.g.
  `"EMMA_ID"`). **MeteoAlarm requires this** — its CAP areas are
  geocode-only EMMA_ID zones; without the lookup they render nothing.
  `testdata/cap/emma-fi.geojson` is the Finland EMMA zone set. An area that
  still resolves to nothing becomes a `Geometry::Null` Feature (valid per
  RFC 7946 §3.2; listed, never on the map), counted as `geocode_only` in
  the load log. The lookup file loads once at construction; a bad path is a
  hard `new()` error.

## Time semantics

- **Active window** = `[onset ∨ effective ∨ sent,
  expires ∨ (start + default_ttl) ∨ open]`.
- Features `datetime=` selects areas whose window overlaps; no datetime ⇒
  all loaded areas. Map/WMS `TIME` selects areas active at that instant;
  **no TIME ⇒ active now** (the snapshot's `as_of`, advanced each poll so
  expired alerts drop out).
- **WMS TIME shape (load-bearing):** `RasterInfo.times` = distinct window
  boundaries ≤ `as_of` plus `as_of` itself (always the max entry, capped to
  256). The WMS handler resolves a TIME-less GetMap to `times.last()`, so
  `as_of` being last is what makes the default render "now".
- `data_version()` (Feature ETags) hashes record ids + severity + window +
  the text fields (event/headline/description/instruction/areaDesc) — an
  in-place text correction invalidates the ETag — but NOT `as_of`, so it
  stays stable across polls when content is unchanged.

## Rendering & extents

- Single layer per collection, parameter `"severity"`, value = CAP severity
  code (Unknown=0, Minor=1, Moderate=2, Severe=3, Extreme=4). Overlaps use
  `Combine::Max` — highest severity wins, order-independent.
- Style: the `cap_severity` builtin colormap (grey→green→yellow→orange→red
  with alpha; codes sit exactly on the 0–4 stops — no inter-code blending).
  Set `[wms] colormap = "cap_severity"`.
- `raster_info()` is O(1) from a prebuilt `Arc<RasterInfo>` in the snapshot.
- `spatial_extent()` = union of resolved geometry bboxes;
  `FeatureEngine::temporal_extent()` = `[min start, max end]` of alert
  windows (open bounds clamp to `as_of`), so the collection JSON advertises
  both extents via `ds_core::ogc_extent::build_extent`.

## Lifecycle

`CapEngine::new` does a best-effort initial load (never fails on an
empty/unreachable source — starts degraded, the poll loop fills in). Wired
in `server/src/admin.rs` (`"cap" => ["features","wms","maps","tiles"]`);
poll loop on `poll_runtime()`; `shutdown()` on reload. Demo:
`collections.d/cap-alerts.toml` over `testdata/cap/`.

Out of scope (follow-ups): XML-DSig verification, per-`event` sub-layers,
conditional-GET feed caching, antimeridian splitting, Global Cache backfill
on a cold WIS2 boot (the accumulator starts empty until alerts arrive).
