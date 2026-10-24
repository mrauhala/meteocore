# api-3dtiles

The 3D Tiles HTTP API of MeteoCore, with a bundled CesiumJS viewer. It
serves volumetric weather data (radar polar volumes) as OGC 3D Tiles.

## What it solves

A radar polar volume is a stack of elevation sweeps: the 3-D structure of a
storm. Map APIs (WMS, Maps, Tiles) show one sweep at a time. This crate turns
the volume into 3D Tiles content that a globe client such as CesiumJS can
draw in 3-D: a point cloud, reflectivity shells, echo-top columns or a voxel
grid. It also bounds the CPU cost of those products and caches the results,
so a viewer that animates dozens of volumes does not recompute them.

## How it works

- The server registers each `"3dtiles"` collection's `VolumeEngine` by
  collection id, in an `ArcSwap` that a reload replaces. The crate never
  depends on an engine crate; `ds-3dtiles` does the encoding.
- `tileset.json` is built from the engine's `VolumeInfo` (coverage region,
  antenna origin) without reading data. Its `content.uri` carries the
  resolved quantity, the time in UTC `…Z` form, the filter and, for grid
  products, the resolution, so the content fetch is deterministic.
- On a cache miss, a content request reads the engine (`read_point_cloud`
  or `read_voxel_grid`) and encodes on a blocking thread. The work holds a
  slot of the 3D Tiles content pool (`CONTENT_SLOTS`: a quarter of the CPU
  cores, at least 2), never a WMS/Maps/Tiles render slot. Points and meshes
  also count against the shared render queue (`MC_RENDER_QUEUE_CAPACITY`)
  and have 30 s to wait and compute. Voxels wait for a slot with no queue
  limit or deadline.
- Points and isosurface shells are painted with the default style of the
  quantity's WMS/Maps/Tiles parameter layer (`[[wms.parameters]]`, style
  bundles, built-in parameter defaults, the collection's `[wms]` fields), so
  a quantity reads the same in 2-D and 3-D. Styles are resolved once at load.

## Representations

| Representation | Tileset | Content | Colours |
|---|---|---|---|
| `points` (default) | `tileset.json` | `.pnts`: one point per echo cell, RGB plus a per-point `value` | quantity ramp |
| `isosurface` | `tileset.json?representation=isosurface` | `.glb` shell at a threshold (marching tetrahedra); up to 5 nested translucent shells | quantity ramp |
| `echotop` | `tileset.json?representation=echotop` | `.glb` columns from the ground to the highest echo at or above the threshold | height, 0–15 km |
| `voxels` | `voxel/tileset.json` | `.glb` cylinder grid, `EXT_primitive_voxels` (draft extension), implicit tiling | values only; the client colours them |

## Routes

Mounted at `/3dtiles`. `{c}` is `/3dtiles/collections/{id}`.

| Route | Parameters | Returns |
|---|---|---|
| `GET /3dtiles/` | `f` (`json` only) | Landing document |
| `GET /3dtiles/collections` | `f` (`json` only) | All 3D Tiles collections, without per-quantity legends |
| `GET {c}` | — | Collection document |
| `GET {c}/tileset.json` | `representation`, `quantity`, `datetime`, `min_value`, `threshold`, `resolution` | Tileset (`application/json`) |
| `GET {c}/content.pnts` | `quantity`, `datetime`, `min_value` | Point cloud (`application/octet-stream`) |
| `GET {c}/content.glb` | `representation`, `quantity`, `datetime`, `threshold`, `resolution` | Mesh (`model/gltf-binary`) |
| `GET {c}/voxel/tileset.json` | `quantity`, `datetime`, `resolution` | Voxel tileset |
| `GET {c}/voxel/subtrees/0/0/0/0.json` | — | Implicit-tiling subtree |
| `GET {c}/voxel/content/0/0/0/0.glb` | `quantity`, `datetime`, `resolution` | Voxel content |
| `GET /3dtiles/viewer` | see Viewer | CesiumJS viewer page; script at `/3dtiles/viewer.js` |

Parameters:

- `quantity`: an id from the collection's `quantities`. Default: the
  collection's default quantity. Unknown: 400.
- `datetime`: an RFC 3339 instant. Selects the nearest retained volume.
  Absent: the latest.
- `min_value` (points): drop points below this value. Must be finite.
- `threshold` (meshes): isosurface default 20; a comma list such as
  `20,35,50` gives nested shells (at most 5 distinct values). Echo-top
  default 18, one value only. Every value must be above the −32 dBZ no-echo
  floor.
- `resolution` (meshes, voxels): grid cells, radius × azimuth × height:
  `low` 128 × 360 × 48, `med` (default) 256 × 360 × 56, `high`
  512 × 360 × 64. `tileset.json` rejects a bad value for any representation.

The collection document has `id`, `title`, `description`, `quantities` (each
with `id`, `label`, `unit`, a suggested `min_value` of 5 for dBZ and `null`
otherwise, and a `legend` of 24 sampled `#rrggbb` stops), `times` (retained
volume times, ascending, each usable as `datetime`), `representations` (the
ones this collection serves), the default quantity's `legend`, and links:
one `rel=3dtiles` link per representation, the viewer (`alternate`), and the
collection's HTML page in the shared OGC API or EDR (`related`) when it has
one.

Errors are JSON `{"error": "…"}`: 400 for a bad parameter (one given twice
or of the wrong type included), an unsupported
representation or a threshold too low to mesh; 404 for an unknown collection
or no data for the request; 503 with `Retry-After: 1` when the queue is full
or the 30 s deadline passed. A 500 never carries internal detail.

## Caching

- A process-wide, byte-bounded LRU holds encoded content and its strong
  ETag: `MC_3DTILES_CONTENT_CACHE_MB` (default 512; `0` keeps nothing). The
  key covers collection, product, quantity, time, parameters, grid size,
  colour ramp, and a version hashed from the volume time axis, so a new
  volume moves "latest" while an exactly advertised time keeps its entry.
- Concurrent requests for one key share one computation, errors included.
  It finishes and fills the cache even if every client disconnects. Errors
  are not kept. A matching `If-None-Match` gets 304; on a cache hit that
  costs no recompute.
- `Cache-Control`: a `datetime` equal to an advertised volume time (and the
  voxel subtree) gets `public, max-age=86400, immutable`; latest and other
  times get `public, max-age=60`. The viewer page and script are `no-cache`.
- Metrics: the `tiles3d_content_cache_*` family on `/metrics`. engine-odim
  keeps a second cache of resampled voxel grids, shared by isosurface,
  echo-top and voxels (`MC_PVOL_VOXEL_GRID_CACHE_MB`, default 512;
  `pvol_voxel_grid_cache_*`).

## Viewer

`GET /3dtiles/viewer` is a CesiumJS page built into the binary, with
pickers for collection, quantity, representation and resolution, a numeric
filter (`min_value` or `threshold`) and a time slider. Isosurface, echo-top
and voxels are offered for dBZ quantities only. It preloads the most recent
48 volumes, six at a time, and refreshes `times` every minute and when the
tab becomes visible.

- The URL holds the view: `collection`, `quantity`, `representation`,
  `min_value`/`threshold`, `resolution`, `datetime`. `?base=` selects
  another API path on the same origin; other origins are ignored.
- The page loads CesiumJS 1.142 from cesium.com (with SRI hashes) and a
  CARTO basemap; its Content-Security-Policy allows only this origin and
  those sources. If CesiumJS has not loaded after 12 s, or WebGL is off, it
  lists each collection with links to its 3D Tiles document and collection
  page.

## Configuration

Add `"3dtiles"` to `apis` of an `odim-volume` collection, the only engine
that implements `VolumeEngine`; any other engine type with `"3dtiles"` fails
to load. The engine serves one collection per radar site, `<id>-<nod>` (the
site's ODIM node code). Colours come from the collection's `[wms]` styling,
even when `wms` is not in `apis`.

```toml
id = "radar-volume"
title = "Radar polar volumes"                 # required
description = "ODIM_H5 PVOL files, per site"  # required
engine_type = "odim-volume"
apis = ["edr", "wms", "maps", "tiles", "3dtiles"]
data_path = "data/pvol"    # directory of ODIM_H5 PVOL .h5 files

[odim]                     # required for odim-volume
poll_interval_secs = 300   # default 30
max_files = 24             # volumes kept per site; default unbounded
# time_window = "-PT6H"    # or bound retention by age

[wms]
colormap = "radar_dbz"     # fallback; [[wms.parameters]] styles one quantity
```

Runnable example: `collections.d/radar-fi-volume-s3-h5.toml` reads FMI's
public S3 bucket. `radar-fi-volume-local-h5.toml` and
`radar-se-volume-local-h5.toml` read local sample files that are not in the
repository.

## Limits

- Every tileset is one root tile, with no level-of-detail hierarchy.
- Threshold checks and grid sealing use the −32 dBZ floor for every
  quantity, and the voxel tileset's value range is fixed at 0–70 dBZ. The
  grid products are meaningful for reflectivity only.
- `datetime` picks the nearest volume however old; no model-run parameter.
- Unknown query parameters are ignored, and so are parameters that do not
  apply to the chosen representation.
- A quantity a poll adds after load uses the fallback `radar_dbz` ramp until
  the next reload.
- `EXT_primitive_voxels` is a draft extension; generic 3D Tiles clients may
  not render voxels.

## Testing

```bash
cargo test -p api-3dtiles
node --test crates/api-3dtiles/tests/viewer.test.cjs
```

`tests/integration.rs` drives the router over a mock `VolumeEngine` (a tiny
point cloud and a synthetic voxel grid; no data files). `viewer.test.cjs`
runs the shipped `viewer.js` against stubbed CesiumJS objects to check
loading, refresh, URL state and the fallback page; it does not render. After
a CesiumJS bump, run `scripts/check-cesium-sri.sh` (needs network).
Contributor notes: [CLAUDE.md](CLAUDE.md).
