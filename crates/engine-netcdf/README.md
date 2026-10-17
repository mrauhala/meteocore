# engine-netcdf

MeteoCore engine that serves gridded CF-NetCDF forecasts and analyses through
WMS 1.3.0, OGC API - Maps and OGC API - Tiles (`engine_type = "netcdf"`,
issue [#1067](https://github.com/mrauhala/meteocore/issues/1067)). The first
target is MET Norway's MEPS, as published per lead time on thredds.met.no.

## Scope

**In:**

- A local directory of NetCDF-4 (HDF5) or classic CDF-1/2/5 files of one
  family: every file carries the collection's variables on one grid.
- Or the same family on an HTTP server that cannot be listed, such as MET
  Norway's thredds.met.no fileServer: files found by probing the names a
  template gives for each run and lead, read in place by HTTP range.
- Grids: 1-D latitude/longitude, or 1-D projected x/y with a CF
  `grid_mapping` (`lambert_conformal_conic`, `geostationary`,
  `latitude_longitude`). Rows stored south to north (MEPS) are drawn north-up.
- Valid times from the fields' `time` axis; model runs from
  `forecast_reference_time`, each run a WMS `reference_time` value. Files
  without one form one timeline without runs.
- Single-level fields: any axis other than time and ensemble must have one
  value. Ensemble files are read at one member.
- CF packing and masking: `scale_factor`/`add_offset`, `_FillValue`,
  `missing_value`, NaN.

**Out (later phases of #1067):**

- EDR position/area/radius and derived wind speed (PR 7), pressure levels
  as a vertical axis (PR 8).
- Per-variable file layouts, curvilinear 2-D latitude/longitude grids,
  `polar_stereographic`/`lambert_azimuthal_equal_area`/`transverse_mercator`
  mappings, ensemble members as a dimension.

## Configuration

```toml
id = "meps-netcdf-local"
engine_type = "netcdf"
apis = ["wms", "maps", "tiles"]     # required: the default ["edr"] is rejected

[netcdf]
data_path = "testdata/meps-netcdf"  # local directory, read non-recursively
filename_template = "meps_sfc_{lead}_%Y%m%dT{run}Z.nc"  # default: names ending in .nc
variables = ["air_temperature_2m", "precipitation_amount_acc"]  # default: every variable on the grid
max_runs = 4                        # newest runs kept
max_files = 1024                    # most files kept open, over all runs
member = 0                          # ensemble member, files with an ensemble axis
poll_interval_secs = 60

[netcdf.titles]                     # layer titles in place of the files' long_name
precipitation_amount_acc = "Precipitation accumulated since run start"
```

`collections.d/meps-netcdf-local.toml` is a runnable example on the
committed MEPS sample with a palette per variable. Values are served in the
files' own units (K, Pa, m/s, kg/m², fractions).

### Remote files

```toml
[netcdf]
data_path = "https://thredds.met.no/thredds/fileServer/meps25epsarchive/"
prefix_pattern = "%Y/%m/%d/{run}/member_00/"          # {run}: the run hour
filename_template = "meps_sfc_{lead}_%Y%m%dT{run}Z.nc"
run_hours = [0, 3, 6, 9, 12, 15, 18, 21]              # required
leads = [0, 1, 2]                                     # {lead}; the first is the sentinel
time_window = "-PT12H"                                # required: runs probed
min_age = "PT10M"                                     # a file this old is used at once
run_grace = "PT30M"                                   # a run missing leads is served after this
prewarm_variables = []                                # newest run fetched ahead
```

A run not yet out costs one HEAD per poll (its first lead's); once it
appears its other leads are HEADed and, settled, opened: each file's first
1 MiB in one request plus a few 64 KiB blocks of its chunk indexes. A field
is one request for its compressed chunk (~1.5 MB for MEPS). Every request
is conditional on the version opened: a file replaced on the server is a
503 and is opened again by the next poll. At most 8 requests go to one host
at once, over every collection on it, and polls take at most 2. A cold field
slower than a render's 3 s deadline fails that render and still lands in the
cache for the next. The collection opens the newest run when it loads, for
at most 15 s; its poll loop opens the rest at once.

`collections.d/meps-netcdf-thredds.toml.disabled` is MEPS surface, control
member, 67 leads, with its traffic stated (about 125 MB of metadata per run
serving eight variables; ~1.5 MB per field first drawn; ~100 MB per prewarm
variable and run) and `max_runs = 2`: each kept file holds about 820 KB of
parsed metadata, so 67 leads × 2 runs is some 110 MB.

| Variable | Default | Meaning |
|---|---|---|
| `MC_NETCDF_FIELD_CACHE_MB` | `256` | Decoded fields (`f32`, ~4 MB per MEPS field), shared by every NetCDF collection |
| `MC_NETCDF_CHUNK_CACHE_MB` | `512` | Compressed chunks of remote files (~1.5 MB per MEPS field) |
| `MC_NETCDF_BLOCK_CACHE_MB` | `64` | 64 KiB metadata blocks of remote files; at least 8 |
| `MC_NETCDF_READ_MEMORY_MB` | `1024` | Transient buffers of field reads in progress; a read that does not fit is a 503 |

Each catalogued file stays open with its metadata while the catalog keeps
it, one file descriptor each for local files. Developer notes, measurements
and the rules the code follows are in `CLAUDE.md`.
