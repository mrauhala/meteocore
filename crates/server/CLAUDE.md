# server crate — Claude Instructions

The binary. Config loading, engine wiring (`src/admin.rs`), CLI parsing
(`src/main.rs`), auto-collections (`src/auto.rs`), the background poll
runtime, reload, health, metrics. Read the root `CLAUDE.md` first — Critical
Rules 6 and 7 (poll runtime, ds-storage bridge) are enforced here.

## CLI flags

Hand-rolled parsing (no `clap`) in `parse_cli_args` (`src/main.rs`). Every
flag also accepts `--flag=value`.

- `--collections <id1,id2,…>` — only load these collection IDs.
- `--host <HOST>` / `--port <PORT>` — override `[server].host`/`port` (CLI
  wins over config). `BASE_URL` env still wins for link generation.
- `--config <PATH>` — config file path (wins over `CONFIG_PATH` env, then
  `./config.toml`). A missing `--config` path is a hard error.
- `--auto-collections <DIR>` — auto-discover collections from a directory
  tree (repeatable); see below.

**No-config boot:** if the default config path is absent AND no `--config` is
given, the server starts from built-in defaults — host `127.0.0.1`, and it
auto-scans for the first free port at/above 8000 (up to 100 ports). A port
pinned by config or `--port` does NOT auto-scan: a bind conflict is fatal.
Combine with `--auto-collections` for a zero-config
`server --auto-collections ./data`.

## Auto-collections (`src/auto.rs`, #411 phase 1)

`--auto-collections <DIR>` synthesizes `CollectionConfig`s from data files on
disk, no TOML needed.

- **Mapping:** per-subdirectory + loose files. Each immediate subdir → one
  collection; loose files in the root → grouped under the root name. The root
  may itself be a Zarr store.
- **Detection (first match wins):** zarr store
  (`zarr.json`/`.zgroup`/`*.zarr` name) → `zarr`; `*.sqd` → `querydata`;
  `*.grib2` + index sidecars → `grib` (`.idx` → wgrib2, `.index` →
  ecmwf-json; **no index ⇒ skipped** — the engine never builds indexes);
  `*.tif`/`*.h5` → phase 2, skipped (need filename-template inference + ODIM
  COMP/PVOL probe); `*.geojson` and `*.csv` → one collection per file.
- **APIs:** each collection enables ALL APIs relevant to its type (mirrors
  the `engine_type → supported_apis` allowlist): raster/grid
  (zarr/grib/querydata) get edr+wms+maps+tiles, csv gets edr+features,
  geojson gets features+tiles. Rendering works without a `[wms]` block via a
  default viridis colormap; range is generic `0..1` until a per-collection
  colormap/min-max is set (#320).
- Synthesized configs are appended to `config.collections` and run through
  the same `ServerConfig::validate()` as TOML (duplicate ids rejected).
- Resolved once at startup — reload does NOT re-scan auto roots (v1).
- Engine-config defaults come from `{QueryData,Grib,Zarr}Config::auto_*`
  constructors in `ds-core` — reuse the serde default fns, keep them DRY.
- **Symlinks are followed** (`is_dir`/`is_file` resolve them). Same trust
  model as `collections_dir`: whoever can write the scan root controls what
  is served.

## Reload & the collections_dir watcher

- `POST /admin/collections/reload` re-reads config and atomically swaps
  engines. Shared core: `admin::do_reload`.
- **Reload is incremental (#574).** Validation stays all-or-nothing (any
  config error rejects the whole reload, live registry untouched), but the
  build phase diffs per collection id against the last accepted config
  (`ServerState::last_collections`, derived `PartialEq` on
  `CollectionConfig`):
  - **Unchanged → the live engine `Arc` is reused verbatim** (from
    `ServerState::engine_handles` via `EngineReuse`): poll loop keeps
    running, catalog/caches stay warm, no remote re-bootstrap. Poll-loop
    rotation is by `Arc` identity (`diff_by_identity`): reused engines are
    neither `shutdown()` nor re-spawned (either would break — #442).
  - **Exceptions:** `csv`/`geojson` always rebuild (no poll loop — a reload
    is the only way they re-read a changed data file). A `nowcast` reuses
    only if EVERY second-pass dependency — `source`, `lightning_source`,
    `impact_source` — is reused too (`reusable_collections` encodes them).
    A dependency missing from that list would be rebuilt while the wrapper
    kept an `Arc` to the old one, so add new second-pass deps in both places.
  - **Cache hygiene:** previously-live collections that were NOT reused get
    their entries evicted from the rendered/meta-tile/vector-tile caches
    (`evict_collection`; matches `{id}`, `{id}/param`, `{id}-derived`).
    Global style inputs ([[colormaps]], bundles, parameter_defaults,
    colormaps_dir bytes) still drop the rendered/meta-tile caches wholesale
    via `style_fingerprint`; per-collection `[wms]` edits are deliberately
    NOT in that fingerprint — they rebuild + evict just their collection.
  - Style-only global changes rebuild NO engines: styles re-resolve for every
    collection on each load, reused engines included.
- With `[server] watch_collections_dir = true` (default off), a filesystem
  watcher (`notify`) triggers the same reload on add/edit/remove — debounced
  (`watch_debounce_ms`, default 500), running on the background
  `poll_runtime`, keeping the live registry if the new config is
  invalid/empty. The same flag also watches `colormaps_dir` when set (#571):
  a palette edit runs the full reload, which rebuilds the palette registry
  and drops the rendered/meta-tile caches via `style_fingerprint`. Under
  `colormaps_dir` there is NO extension filter (palettes span
  .toml/.cpt/.pal/.txt/.clr/.sld and `.disabled` renames must retrigger);
  `collections_dir` events keep the `.toml`-name filter. Watch roots are
  resolved once at boot — re-pointing either dir needs a restart.
- **Filter watcher events by `event.kind`, not path alone.** notify's inotify
  mask includes IN_OPEN; do_reload itself reads every `.toml` and every
  palette file, so a path-only filter creates a self-sustaining reload loop
  (#424). Drop `Access(Open/Read/Close(Read))` and `Modify(Metadata)`; keep
  create/remove/rename/data-write/CLOSE_WRITE.
- **Trust model:** the watcher's reload is authorized by filesystem write
  access to the watched directories (`collections_dir`, `colormaps_dir`), NOT
  the HTTP `ADMIN_TOKEN` that gates the reload endpoint — different control
  planes. Anyone who can write a collection file already controls what the
  server serves; anyone who can write a palette file controls served styles.
  When the watcher is enabled and `ADMIN_TOKEN` is set, a startup WARN makes
  the asymmetry explicit, naming the watched dirs. Keep them writable only by
  trusted principals; avoid shared/NFS mounts for them.

## Reverse-proxy base URL (`trust_proxy_headers`, #12)

Absolute self-links (landing pages, `collections`, GeoJSON `links`, WMS
GetCapabilities OnlineResource/legend URLs, 3D Tiles docs, …) are built from a
base URL. By default it is resolved once at startup: `BASE_URL` env >
`[server] base_url` > `http://{host}:{port}` — wrong behind a reverse proxy.

With `[server] trust_proxy_headers = true`, the base is resolved per request:

1. RFC 7239 `Forwarded` — `proto=`/`host=` of the **last** element (the
   closest trusted proxy; the first element is client-injectable via RFC 7239
   §4 append semantics),
2. `X-Forwarded-Proto` + `X-Forwarded-Host` (+ optional `X-Forwarded-Port`),
3. the static fallback above.

The pure resolver is `ds_core::proxy::resolve_base_url` (framework-free; axum
handlers pass header values via a closure). Each API crate calls it through a
small `request_base_url` helper.

**Security:** default false because forwarding headers are
client-controllable. Even when enabled: host values are sanitised
(whitespace/slashes/`@`/non-ASCII rejected), scheme restricted to
http/https, only the first value of a comma list used; malformed headers fall
through to the next source. Enable only when a trusted proxy sets/overwrites
these headers, the proxy strips `Forwarded`/`X-Forwarded-*` on untrusted
client requests, and clients cannot reach the backend directly — otherwise a
client could spoof the emitted self-links (open-redirect risk downstream).

## Operational notes

- A brief "no images" period right after a deploy is the expected readiness
  blip: the port binds before collections finish loading (#247). Not a code
  bug.
- Engine poll loops are spawned on `poll_runtime()` at boot (`main.rs`) and
  on reload (`admin.rs`), with `shutdown()` called on the old engines at
  reload. When adding an engine with a poll loop, wire BOTH paths (a missing
  reload-path spawn was bug #442).
