# engine-satellite — geostationary satellite imagery

Read the root CLAUDE.md. Epic #819 holds the plan, the provider survey
(GOES, Himawari, GK2A, MTG, GMGSI) and the licensing decisions.

## Load-bearing rules

- **One collection = one satellite + sector; one parameter = one product**
  with its own time axis. `parameter_times` and `resolve_parameter_time`
  MUST go through `SatelliteEngine::select`, the same selection
  `get_raster_tile` renders with (latest scan at or before, first when
  earlier, newest for `None`). Drift reintroduces the #507 cache poisoning.
  Before a product's first scan, `select` is `None` and the render is an
  empty tile, not an error, so `resolve_parameter_time` must stay `None`
  too. Falling back to the requested time, as the error-rendering engines
  do, keyed a reload's backfill-window empties on a scan that landed
  minutes later, and the meta-tile cache served that frame blank.
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
  the grid's edges. A single file's block is one chunk over (y, x)
  (`Dataset::chunks()`, via hdf5-reader on the same in-memory storage as
  netcdf-reader, so no second copy), so a block read inflates each chunk
  once. A GOES-R chunk spans the full width, so its block is a strip;
  GMGSI's is 793 × 1322, a quarter of the width. The reader's own chunk
  cache is off because decoded blocks are cached in `cache::STRIPS`, which
  keeps its phase-2 name, env var and metric family.
- **No request-time S3 in the steady state: the window stays in memory.**
  The poll loop downloads each new scan whole into `cache::FRAMES`, which
  every satellite collection shares (`MC_SATELLITE_FRAME_CACHE_MB`).
  Production 2026-09-29: scans were never removed when they left the
  window, the cache filled with dead scans, and its LRU evicted in-window
  scans nobody had read yet. A 12-frame Himawari animation then refetched
  10 scans of 88 tiles each inside the render deadline, and all 10 were 503.
  - Each poll sweeps out of `FRAMES` its own scans the new catalog does not
    hold, before ingesting new ones: those that left the window, and any a
    request fetched again from an older snapshot. Their decoded blocks
    leave `STRIPS` by key (the frame's block count). Blocks of a scan
    already evicted age out.
  - `FrameKey.engine` is the engine instance, not the collection. A
    rebuilt engine and a rejected reload's candidate carry the same
    collection id. `Drop` releases the instance's scans, blocks and window
    record. A reused engine (unchanged config, #574) is not dropped, so a
    reload keeps its scans.
  - After ingest, `keep_resident` downloads again the in-window scans the
    cache evicted, newest first, with what `MAX_INGEST_PER_POLL` left for
    the product. It does so only while every live engine's window
    (`cache::windows`) fits the capacity. Past that, each download would
    evict another in-window scan and the polls would download in a loop:
    it fills free room only, and logs one WARN per poll with every
    collection's window. `FRAMES` is one shard so that "fits" is exact:
    `quick_cache` otherwise gives each shard an equal slice of the budget.
  - The re-download claims the fill guard untracked: it counts in
    `satellite_frame_reingests_total`, and cache misses stay request-time
    downloads. `/metrics` also has `satellite_frame_window_bytes` and
    `satellite_frame_window_resident_bytes` per collection.
  - A scan still evicted is fetched again by `Source::fetch` →
    `DataStore::get`, from a render job (blocking worker) or an EDR query
    (async request worker). That bridge serves both — the plain-Zarr
    exception to Critical Rule 7 — where an explicit `get_on` handle panics
    on the async worker. ISatSS tiles come `get_many`-concurrently.
  - `tests/refetch.rs` runs both call sites with zero-size caches (`0` also
    turns re-downloading off). `tests/window_capacity.rs`, on a 1 MB cache,
    pins the thrash guard, its WARN and the recovery; `lib.rs` tests pin the
    sweep, the re-download and instance-scoped keys.
- **Downloads are capped per poll** (`MAX_INGEST_PER_POLL` per product: new
  scans first, newest first, then re-downloads): bootstrapping a window
  spreads over polls (Critical Rule 9).
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
- **GMGSI is a global spherical-Mercator mosaic** (`provider = "gmgsi"`,
  NOAA's hourly Global Mosaic of Geostationary Satellite Imagery).
  - The file has no `grid_mapping`, only 2-D `lat`/`lon` arrays.
    `mercator::from_lat_lon` recognises the grid from them at ingest:
    separable (checked on the first and last rows and columns), longitude
    linear in the column, latitude linear in the row in Mercator northing
    (`ds_core::web_mercator`), all to 0.01 px, rows north to south, and
    columns spanning 360° to within a pixel. Anything else fails the scan.
    No pixel's lat/lon is ever looked up at render time (Critical Rule 5).
  - Each array is one deflated chunk of 15 M floats. Reading its first and
    last rows or columns still inflates the whole chunk: ~55 ms and a
    ~120 MB transient per array. A whole scan ingests in ~240 ms (release).
  - The CRS is `Crs::WebMercator` (EPSG:3857, via `ds_core::web_mercator`
    only), `RasterInfo.native_crs` `"EPSG:3857"`.
  - The grid starts at 179.99962°E and its x runs on past 180° unwrapped.
    Columns wrap modulo `Frame::col_period`, 4999.378 columns per 360°:
    8016 m square pixels on the 3857 sphere, 0.072009°, not 0.072°. The 4999
    columns fall 0.38 px short of a turn (NOAA cut the 5000th with
    `ncks -d xc,0,4998`). `Frame::pixel` reads that sliver as the nearer edge
    column: nearest-neighbour across the seam, so the seam renders
    continuously. The overview resolves a position on the full grid too,
    then reads the cell holding that pixel: its 1250 cells span 5000
    pixels, past the turn, so resolved at its own scale the sliver's
    eastern half would read the last cell (#906 review). Off the seam, the
    overview's own extent bounds it: the outer slice of its last row, or of
    its last column on an axis that does not wrap, reads that cell.
  - Every column lookup in a render or an EDR query goes through
    `Frame::pixel`, and every bbox → pixel window through `Frame::windows`
    (two windows across the seam). `ProjectionGrid::build_2d_periodic` gets
    the level's period (`render::CoordinateMaps::onto` for renders), so
    cells across the seam or a projected output's longitude cut (EPSG:3035
    along 170°W) interpolate correctly.
    `tests/gmgsi.rs` pins the seam (170°E → 170°W), wrapped world copies and
    that cut against the file's own lat/lon.
  - **Values are 8-bit display counts, not Kelvin**, despite
    `units = "K"`: `long_name` "0-255 Brightness Temperature", range 3–255,
    cold/moist high. Serve them with unit `"1"` and a grey palette, never as
    an input to physical composites.
  - They are stored `float`: `Stored::FloatCounts` carries each as its
    whole `u16`, the fill and NaN as `FLOAT_MISSING`. A value that is not a
    whole count fails its block, and so the scan: the overview reads every
    block at ingest.
  - The field is `data(time, yc, xc)`. Leading dimensions of size 1 are
    read at index 0.
  - Products `LW`, `SW`, `WV`, `VIS` (`GMGSI_<product>/%Y/%m/%d/%H/`, files
    `GLOBCOMP{LIR,SIR,WV,VIS}_v3r0_blend_s%Y%m%d%H%M%S<tenths>_…nc`); `band`
    is refused. `GMGSI_SSR` stopped in 2025 and names its files otherwise.
    Hourly, ~7.4 MB (WV ~3.4, VIS ~8.9), published ~35–45 min after the
    hour, so a bucket window must reach back past the latest published hour
    (the example uses `-PT3H`).
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
    full resolution or overview, compared field by field with its column
    period (a global grid's). Bands of one resolution share it. `render::tests` counts the builds.
  - Frames and blocks go through `frame()` and `PixelReader` exactly as in
    a single-band render, so the refetch rules above hold unchanged.
- **RGB composites are layers, not parameters** (`[[satellite.composites]]`,
  decision of 2026-09-29 on #819).
  - `MapEngine::composites` returns the `CompositeDef`s built at
    construction. Their names are not in `RasterInfo.parameters` or EDR's
    parameters: the parameter layers stay the products.
  - A composite's time axis, `parameter_times(<composite>)`, is the scans
    every band has: `Catalog::composite_times`, intersected by
    `shared_times` in `Catalog::build`. Every poll rebuilds the snapshot, so
    the axis follows ingest and eviction, and a call only clones an `Arc`.
  - `resolve_parameter_time(Some(<composite>))` is `select_common` over its
    bands, the call `resolve_parameters_time` and `render_bands` make. Do
    not resolve it from the advertised axis instead: one helper cannot
    drift (#507).
  - A composite name given where a band is expected (`get_raster_tile`,
    `get_raster_tiles`, EDR) is `InvalidParameter` naming its bands
    (`not_a_band`). The API layer renders one by passing
    `CompositeDef::parameters()` to `get_raster_tiles` and composing the
    tiles with `ds_render::CompositeSpec::from(&def)`, whose planes follow
    that order. WMS, Maps and Tiles do exactly that, keyed on
    `resolve_parameter_time(Some(<composite>))`.
- **Built-in recipes resolve in ds-core, through one path.**
  `recipe = "airmass"` / `"night_microphysics"` stands for the red, green
  and blue channels written out.
  - `ds_core::config::satellite_composite_def` builds every `CompositeDef`,
    written out or from a recipe. `validate_satellite` checks what it builds
    and `SatelliteEngine::new` serves it, so a recipe and its channels
    spelled out give equal layers (`tests/composites.rs`).
  - The coefficients live only in `ds_core::satellite_recipes`, one entry
    per recipe and instrument, with the satpy files and agency quick guides
    cited there. The provider picks the instrument: `goes-r` is ABI,
    `isatss` AHI. Any other provider is refused, so GMGSI, whose values are
    8-bit counts, must stay unmapped.
  - Each band comes from the one product whose `band` is that number. The
    load fails, naming the bands, if a band is missing, held by two
    products, or declared in a unit other than `K` (an L1b radiance).
  - satpy's AHI recipes read band 14 (11.2 µm) where ABI reads band 13, and
    its AHI Night Microphysics keeps EUMETSAT's SEVIRI ranges. JMA's own
    Himawari guides differ from both; the table follows satpy.

## Config

`[satellite]`: `provider = "goes-r" | "isatss" | "gmgsi"`, `data_path` XOR
`endpoint`+`bucket`, `time_window` (required for a bucket: ≤ 24 h of GOES-R
or GMGSI hourly prefixes, ≤ 6 h of ISatSS ten-minute scan directories;
`Naming::validate_window`),
`poll_interval_secs`, and `[[satellite.products]]` with `parameter`, `title`,
`unit` (declared: styles resolve at load, before any scan), `product`,
`band` (required for ISatSS, whose `product` is the sector, `HFD`; refused
for GMGSI, whose `product` is the mosaic, `ds_core::config::GMGSI_PRODUCTS`),
`variable` (`data` for GMGSI). `[[satellite.composites]]` with `name`
(`^[a-z0-9_]+$`, not a product parameter), `title`, and either `recipe` or
`red`, `green`, `blue`, never both. A channel is `{ parameter, minus, min,
max, gamma }`: `minus` makes a band difference, `min > max` inverts,
`gamma` defaults to 1. `parameter` and `minus` name product parameters. A
`recipe` (`airmass`, `night_microphysics`) finds its bands by the products'
`band` numbers; recipes exist for ABI (goes-r) and AHI (isatss) only, so
GMGSI's counts are refused. `title` defaults to the recipe's title, else
the name. Unknown keys in a composite are a load error. Validation:
`ds_core::config::validate_satellite`. `collections.d/goes19-fd-rgb.toml`
is the runnable recipe example.

## Bandwidth

Each scan is downloaded whole: GOES-19 band 13 ~24 MB, cloud top temperature
~30 MB per 10 minutes, Himawari-9 band 13 ~26 MB in 88 tiles, a GMGSI
mosaic ~7.4 MB per hour; startup ingests the whole window. A recipe
multiplies it: `goes19-fd-rgb`'s six bands are ~135 MB per scan, ~810 MB an
hour, and hold ~157 MB per scan in `MC_SATELLITE_FRAME_CACHE_MB`, which
every satellite collection shares. Size that cache above the windows of all
satellite collections together: in production, `goes19-fd` and `goes18-fd`
(C13 + ACHT) and `himawari9-fd` at `-PT2H` hold ~1.6 GB. The user is often
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
`testdata/gmgsi/`: a real GMGSI longwave-IR mosaic decimated to every 50th
row and 49th column (60 × 102), still a global Mercator grid with a seam
sliver, plus the CDL it is built from (its README has the details).

## EDR

Position, area and radius (radius via area). GMGSI serves its display
counts with unit `"1"`, like any product. A response's time axis is the
union of the selected products' scans, null where a product has none; an
instant snaps per product through `select`. `get_parameter_available_times`
feeds each product's own `extent.temporal` in `parameter_names`. Area grids
sample at the finest selected product's nadir pixel size through a
`ProjectionGrid` (never a per-cell geostationary forward). Update `crates/api-edr/README.md` with any change here.
Budgets, checked before any work: at most `MAX_QUERY_FETCHES` (8) evicted
scans to download and `MAX_QUERY_BLOCKS` (1024) blocks to decode per query,
summed per product on its own grid (products may mix 0.5/1/2 km).

## Not yet

Other providers (GK2A CGMS navigation, MTG) are phases 3 and 5. A
regional (non-global) Mercator grid is refused: only GMGSI needs one, and
it is global. A new provider gets recipes only with its own instrument
table in `ds_core::satellite_recipes`: GK2A AMI and MTG FCI number their
bands differently. WMS, Maps and Tiles serve the composites from
`composites()`: see "RGB composite layers" in `crates/api-wms/CLAUDE.md`.
