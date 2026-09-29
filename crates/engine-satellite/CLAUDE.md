# engine-satellite — geostationary satellite imagery

Read the root CLAUDE.md. Epic #819 holds the plan, the provider survey
(GOES, Himawari, GK2A, MTG, GMGSI) and the licensing decisions.

## Load-bearing rules

- **One collection = one satellite + sector; one parameter = one product**
  with its own time axis. `parameter_times` and `resolve_parameter_time`
  MUST go through `SatelliteEngine::select`, the same selection
  `get_raster_tile` renders with (latest scan at or before, first when
  earlier, newest for `None`). Drift reintroduces the #507 cache poisoning.
  `RasterInfo.times` is the union over products.
- **Scan time = scan start truncated to the minute** (`naming.rs`). A
  full-disk scan starts ~20 s past its ten-minute slot; keying on the
  second would make a nominal `TIME=…T19:00:00Z` snap to 18:50.
- **Storage type ≠ value signedness.** GOES-R CMI is stored `short` with
  `_Unsigned = "true"`. Read blocks with the *storage* type (hdf5-reader 0.9
  rejects a signedness mismatch), carry values as `u16` bit patterns, and
  interpret them per `_Unsigned`. `_FillValue`/`valid_range` are in the
  storage type: a `short` fill of −1 is `0xFFFF` (`frame.rs` `as_raw`).
- **Reads go by blocks** (`Frame::{locate, read_block, blocks_in}`): a
  rectangle of `block_rows` × `block_cols` pixels, row-major, clipped at
  the grid's edges. A GOES-R block is a strip: the chunk rows
  (`Dataset::chunks()`, via hdf5-reader on the same in-memory storage as
  netcdf-reader, so no second copy) across the full width, so a block read
  inflates each chunk once. The reader's own chunk cache is off because
  decoded blocks are cached in `cache::STRIPS`, which keeps its phase-2
  name, env var and metric family.
- **No request-time S3 in the steady state.** The poll loop downloads each
  new scan whole and inserts it into `cache::FRAMES`. A scan the cache
  evicted is fetched again by `Source::fetch` → `DataStore::get`, from a
  render job (blocking worker) or an EDR query (async request worker). That
  bridge serves both — the plain-Zarr exception to Critical Rule 7 — where
  an explicit `get_on` handle panics on the async worker.
  `tests/refetch.rs` runs both call sites with zero-size caches.
- **Ingest is capped per poll** (`MAX_INGEST_PER_POLL`, newest first):
  bootstrapping a window spreads over polls (Critical Rule 9).
- **No footprint guard.** `Crs::Geostationary::forward` is NaN behind the
  Earth, so no coarse projection cell can alias far-away output onto the
  disk; `ProjectionGrid` refines cells on the limb (#821). The collection
  extent is the seam-aware union of product extents (`union_extent`).
- **Extents may cross the antimeridian** (GOES-West, Himawari, GK2A):
  `spatial_extent` is `west > east` then. Requests reach the engine with
  longitudes past ±180° (a WMS client wrapping the world): the
  geostationary forward is periodic in longitude, so they need no
  normalising. `tests/antimeridian.rs` pins renders and EDR positions
  either side of the seam on a real GOES-18 crop. The API edges handle the
  wrapped extent: WMS CRS:84 `BoundingBox` and Tiles limits span every
  longitude. OGC API Maps rejects a west > east `bbox` (#828).
- **A scan may be many files.** Himawari ISatSS (`provider = "isatss"`)
  publishes a full disk as 88 tiles of 550 × 550 in the scan's own
  ten-minute directory (`AHI-L2-FLDK-ISatSS/%Y/%m/%d/%H%M/`).
  - `Source::list` groups a scan's tiles, keeping the newest copy of each
    tile. `Source::fetch` downloads them `get_many`-concurrently
    (Critical Rule 9).
  - `Frame::open` places the tiles on one lattice from their CF x/y
    coordinates, which are absolute packed grid indices. Block = tile, and a
    lattice cell without a tile reads as missing.
  - `tiled_scan_ready` holds a scan back while its tiles may still be
    arriving.
  - A bucket lists only the slots not yet ingested, and at most 6 h of them.
    A slot directory holds every band, so one poll lists each prefix once
    for all products (`source::Listings`).
  - A tile with corrupt coordinates fails its scan: the lattice is capped
    at `MAX_MOSAIC_PIXELS` per axis and `MAX_MOSAIC_CELLS` before anything
    is allocated.
- **ISatSS quirks.** The geostationary mapping writes `semi_major` and
  `semi_minor` (aliased in `Part::open`; `ds_core::cf` stays strictly CF).
  The field has no `_FillValue` or `valid_range`: far space is packed
  −1076 ≈ 0 K (`valid_fallback` = packed ≥ 0). Space next to the limb
  carries an unmasked stray-light halo (~110–170 K).
  - Map and EDR reads only land on the disk, but an overview cell
    straddling the limb would keep an off-disk centre, so `mask_off_disk`
    blanks those cells. It costs two bisections per row.
  - A limb crop's warm values can lie wholly off the ellipsoid: that is the
    atmosphere a grazing line of sight crosses.
- **Overview**: a render whose source window spans ≥ `OVERVIEW_FACTOR`
  (4) source pixels per output pixel samples the ingest-time overview
  instead of decoding strips. A full-disk decode is ~160 ms.
- **Multi-band renders** (`get_raster_tiles`, for RGB composites) live in
  `render.rs` next to the single-band path, and share `render_scan` with it.
  - Every band comes from the scan `time` names exactly, with no per-band
    snapping. A band without that scan fails with `InvalidParameter`. For
    `None`, the latest scan every band has is used, and every tile is
    empty when they share none.
  - `resolve_parameters_time` and the `None` case both go through
    `select_common`: `select`'s rule over the shared scans, via
    `ds_core::map_engine::select_common_time`.
  - `CoordinateMaps` builds one `ProjectionGrid` per distinct sampled grid,
    full resolution or overview, compared field by field. Bands of one
    resolution share it. `render::tests` counts the builds.
  - Frames and blocks go through `frame()` and `PixelReader` exactly as in
    a single-band render, so the refetch rules above hold unchanged.

## Config

`[satellite]`: `provider = "goes-r" | "isatss"`, `data_path` XOR
`endpoint`+`bucket`, `time_window` (required for a bucket: ≤ 24 h of GOES-R
hourly prefixes, ≤ 6 h of ISatSS ten-minute scan directories;
`Naming::validate_window`),
`poll_interval_secs`, and `[[satellite.products]]` with `parameter`, `title`,
`unit` (declared: styles resolve at load, before any scan), `product`,
`band` (required for ISatSS, whose `product` is the sector, `HFD`),
`variable`. Validation: `ds_core::config::validate_satellite`.

## Bandwidth

Each scan is downloaded whole: GOES-19 band 13 ~24 MB, cloud top temperature
~30 MB per 10 minutes, Himawari-9 band 13 ~26 MB in 88 tiles; startup
ingests the whole window. The user is often
on a metered connection — never run a bucket-backed collection for tests
without asking; use the local fixtures.

## Fixtures

`testdata/goes19-abi/OR_ABI-L2-{CMIPF-M6C13,ACHTF-M6}_G19_s20262681900199_*.nc`:
real GOES-19 scans cropped to 240×320 (C13 across the NE limb, ACHT from the
disk interior) with the original packed integers, chunking and compression;
the global `meteocore_fixture` attribute records each crop. Tests copy them
into a nested temp directory (local discovery lists recursively).
`testdata/goes18-abi/…_G18_s20262701850224_*.nc`: a GOES-18 C13 crop
straddling 180° at 11–16°N (`lon_0` −137.0, read from the file).
`testdata/himawari9-isatss/`: three real Himawari-9 band 13 tiles of one
scan, cropped to 64 × 64 around a lattice corner on the NW limb whose fourth
cell has no tile (its README has the layout).

## EDR

Position, area and radius (radius via area). A response's time axis is the
union of the selected products' scans, null where a product has none; an
instant snaps per product through `select`. `get_parameter_available_times`
feeds each product's own `extent.temporal` in `parameter_names`. Area grids
sample at the finest selected product's nadir pixel size through a
`ProjectionGrid` (never a per-cell geostationary forward). Update `crates/api-edr/README.md` with any change here.
Budgets, checked before any work: at most `MAX_QUERY_FETCHES` (8) evicted
scans to download and `MAX_QUERY_BLOCKS` (1024) blocks to decode per query,
summed per product on its own grid (products may mix 0.5/1/2 km).

## Not yet

Other providers (GMGSI lat/lon mosaics, GK2A CGMS navigation, MTG) are
phases 3 and 5.
