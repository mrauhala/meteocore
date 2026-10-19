# engine-zarr

Serves gridded data from Zarr V2/V3 stores with CF metadata through OGC API -
EDR, WMS 1.3.0, OGC API - Maps and OGC API - Tiles (`engine_type = "zarr"`).
It can also read Icechunk repositories.

## What it solves

Reanalyses and model forecasts are often published as Zarr stores in object
storage: one array per variable, split into compressed chunks that can be
fetched one at a time. This engine serves such a store in place, from a local
directory, S3 or HTTP. A request reads only the chunks under its point, area
or map window. Nothing is converted or copied first.

## How it works

- **Format.** The `zarrs` crate parses metadata and decodes chunks (gzip,
  zstd, Blosc, CRC32C, sharding, transpose); CF rules are applied here.
- **Catalog.** The engine lists the root group's arrays one level deep. A
  1-D array named after its own dimension is a coordinate variable; every
  other array on the latitude/longitude grid is a parameter, in name order.
  Dimension names come from V3 `dimension_names` or the V2
  `_ARRAY_DIMENSIONS` attribute. Axes are recognised by the coordinate's CF
  `standard_name`, then its `units`, then its name.
- **Poll and swap.** Every `poll_interval_secs`, on the server's background
  poll runtime, the catalog is rebuilt and swapped in atomically with the
  map metadata. A failed rebuild keeps the previous catalog.
- **Values.** A value is `raw * scale_factor + add_offset`. Raw values equal
  to `_FillValue`, `missing_value` or the array's Zarr fill value, and NaN
  or infinite values, are null.
- **Units.** Values are served in the display unit of the array's CF
  `units`, in any spelling: K → °C, Pa → hPa, kg m⁻² → mm, kg m⁻² s⁻¹ →
  mm/h, m² s⁻² → gpm, proportion → %. Other units are served as the store
  spells them. Every API, derived wind included, reads the converted
  values and reports the converted unit.
- **Reads.** A position reads the 2×2 cells around the point over the
  requested time span. An area, a trajectory segment or a map reads one
  window over its bounding box plus one cell. Sampling is bilinear; when a
  corner is null it takes the nearest corner, else any corner with data.
- **Plain stores** go through `ds-storage`. Each object is fetched whole and
  kept in a byte cache (`cache_mb`); byte ranges are sliced from it.
  Concurrent requests for one object share one download.
- **Icechunk repositories** (Cargo feature `icechunk`) use Icechunk's own
  storage, pinned to one snapshot per catalog. Reads fetch byte ranges,
  decoded inner chunks are cached (`decoded_cache_mb`), and up to four
  chunks decode in parallel.
- **Memory admission.** A read first reserves its estimated source buffers
  from one process-wide budget, `MC_ZARR_READ_MEMORY_MB` (default 1024 MiB,
  an estimate, not a RAM limit); one that does not fit fails at once with
  503. gzip, zstd and Blosc output is capped at the chunk's decoded size.
- **Diagnostics.** Catalog builds log chunk shapes and warn when each time
  step is one whole-field chunk (a point time series then decodes whole
  fields). `/metrics` has `zarr_read_*` and `zarr_decoded_cache_*`.

## What it serves

| Trait | APIs | Notes |
|---|---|---|
| `EdrEngine` | EDR | `position`, `area`, `radius`, `trajectory`; model runs as instances |
| `MapEngine` | WMS, Maps, Tiles | One layer per variable; the first is the default |
| `WindSource` | all of the above | Wind derived from u/v components (below) |

**EDR.**

- Without `parameter-name` a query covers every variable; names ignore
  case and an unknown one is a 400. Without `datetime` it covers every step
  of the run, else the steps in the interval, ends included (none: 404).
- `position` takes `POINT(lon lat)`; a `MULTIPOINT` is answered point by
  point. A point off the grid is null.
- `area` returns a CRS84 grid over the polygon's bounding box at the
  store's resolution, at most 256 cells per axis. Cells outside the polygon
  are null; a polygon outside the extent is a 404. A box crossing the
  antimeridian is read as two windows. `radius` is answered as an area.
- `trajectory` takes a WKT `LINESTRING` (also `Z`, `M`, `ZM`), densified at
  the grid spacing and read in path segments. A Z coordinate is ignored.
- No `locations` or `cube` queries. See the
  [Per-engine query-type matrix](../api-edr/README.md#per-engine-query-type-matrix).

**Maps.** A requested instant (WMS `TIME`, Maps/Tiles `datetime`) draws
the nearest step; without one, the last step. The run is the latest unless
WMS `DIM_REFERENCE_TIME` names another. A bounding box off the grid renders
transparent. Projected output CRSs are sampled through a coarse projection
grid, never per pixel.

**Model runs.** With a reference-time axis and a lead axis (CF
`forecast_reference_time` and `forecast_period`, or names such as
`init_time` and `lead_time`), each run is an EDR instance and a WMS
`DIM_REFERENCE_TIME` value. The latest run is the default; its valid times
(run + lead) are the time axis.

**Derived wind.** Components are paired by CF `standard_name`
(`eastward_wind`/`northward_wind` or `x_wind`/`y_wind`); `wind_u_10m` gives
`wind_speed_10m` and `wind_direction_10m`. Speed is served on every API,
direction in EDR only. `derive_wind = false` on the collection opts out.

## Configuration

```toml
id = "zarr-era5-t2m-local"
engine_type = "zarr"
apis = ["edr", "wms", "maps", "tiles"]   # default ["edr"]

[zarr]
data_path = "testdata/zarr-era5-t2m"

[wms]
colormap = "pressure"   # the collection layer draws msl, the first variable
```

In `config.toml` the tables are `[collections.zarr]` and `[collections.wms]`.
Set colormap ranges in the served units (°C, not K). On a store with several
variables, each variable's layer takes the built-in default its name and
unit select ahead of the `[wms]` palette; a layer that neither styles draws
a generic palette.

| Field | Default | Meaning |
|---|---|---|
| `data_path` | – | Local store root, or an `s3://` or `http(s)://` URL |
| `endpoint`, `bucket` | – | S3 source instead of `data_path`; needs `path` too |
| `path` | – | Store path in the bucket, or a sub-path under `data_path`; relative, no `..` |
| `parameters` | every variable | Variables to serve (names ignore case) |
| `poll_interval_secs` | `300` | Catalog rebuild period; must be above 0 |
| `cache_mb` | `256` | Plain: whole-object byte cache. Icechunk: compressed chunk cache. `0` keeps nothing |
| `zarr_version` | – | `2` or `3`; validated only, the version is read from the metadata |

**Icechunk.** A `[zarr.icechunk]` table reads the source (a local
`data_path` directory, or `endpoint` + `bucket` + `path`) as an Icechunk
repository. The server must be built with `--features icechunk`, or the
collection fails to load. Each poll resolves the selected version and
rebuilds the catalog only when the snapshot changes; requests in flight keep
theirs. At most one of `branch`, `tag` and `snapshot` may be set.

| Field | Default | Meaning |
|---|---|---|
| `branch` | `main` | Read the head of this branch |
| `tag` | – | Read this tag |
| `snapshot` | – | Read this snapshot id |
| `decoded_cache_mb` | `256` | Decoded inner-chunk cache, on top of `cache_mb`; `0` keeps nothing |
| `region` | – | S3 region of the repository |
| `force_path_style` | `true` | S3 path-style addressing; `false` for virtual-host style |

**Examples** in [`collections.d/`](../../collections.d/): `zarr-era5-t2m-local`
serves the committed fixture. The disabled `ecmwf-aifs-single-icechunk`,
`noaa-gfs-icechunk` and `dwd-icon-eu` read public dynamical.org Icechunk
forecasts on AWS; drop `.disabled` and build with `icechunk` to run one.

## Limits

- A store needs a time axis and 1-D, monotonic latitude and longitude axes;
  a projected grid or 2-D latitude/longitude fails to load. Non-standard CF
  calendars are read as proleptic Gregorian.
- One grid per collection. Variables off the first variable's grid, or of a
  type other than signed or unsigned 8–64-bit integers, `float32` and
  `float64`, are skipped with a WARN. Any other dimension (a level, an
  ensemble member) is read at index 0; there is no vertical axis.
- Longitudes are not normalised: on a 0…360° store, negative longitudes
  fall off the grid, and maps do not wrap past the store's longitude range.
  In an area across the antimeridian, cells within half a cell of ±180° are
  sampled from one side only, or are null.
- An area reads at most 8 variables and a trajectory at most 64 segment ×
  variable windows; more is a 400. An area's native read (cells × steps ×
  variables) must also stay within 1 M values.
- Plain stores fetch whole objects, so a sharded store downloads whole
  shards. They are not snapshot-isolated: a read that misses the cache after
  a poll replaced its catalog fails with 503, and every successful poll
  invalidates rendered map caches, even when the data did not change.
- `endpoint` + `bucket` and Icechunk read anonymously (public data only);
  an `s3://` `data_path` takes credentials from the AWS environment.
  Icechunk takes no URL `data_path` and cannot read virtual chunks (chunks
  stored outside the repository).

## Testing

```sh
cargo test -p engine-zarr                       # plain Zarr
cargo test -p engine-zarr --features icechunk   # adds the Icechunk tests
cargo run -p engine-zarr --example gen_fixture  # regenerate the fixture
```

`testdata/zarr-era5-t2m` is a committed Zarr V3 store: 4 steps on a 12 × 16
grid with `msl` (Pa), `mtpr` (kg m⁻² s⁻¹), `t2m` (K, `float32`) and
`t2m_packed` (K, `int16`, packed, a 2 × 2 block of fill cells in the first
step), served in hPa, mm/h and °C. Every field is linear in latitude and
longitude, so bilinear results are exact. `tests/integration.rs` queries it
and writes forecast and antimeridian stores to temporary directories.
`tests/icechunk.rs` writes a local repository; its `#[ignore]`d probes read
the live public ones. Unit tests in `src/` cover codec caps, memory
admission, caches, refresh and unit conversion.

Contributor rules for this crate are in [CLAUDE.md](CLAUDE.md).
