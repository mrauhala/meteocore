# engine-netcdf

MeteoCore engine that serves gridded CF-NetCDF forecasts and analyses through
WMS 1.3.0, OGC API - Maps and OGC API - Tiles (`engine_type = "netcdf"`,
issue [#1067](https://github.com/mrauhala/meteocore/issues/1067)). The first
target is MET Norway's MEPS, as published per lead time on thredds.met.no.

## Scope

**In:**

- A local directory of NetCDF-4 (HDF5) or classic CDF-1/2/5 files of one
  family: every file carries the collection's variables on one grid.
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

- Remote files by HTTP range (PR 5), EDR position/area/radius and derived
  wind speed (PR 7), pressure levels as a vertical axis (PR 8).
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

| Variable | Default | Meaning |
|---|---|---|
| `MC_NETCDF_FIELD_CACHE_MB` | `256` | Decoded fields (`f32`, ~4 MB per MEPS field), shared by every NetCDF collection |
| `MC_NETCDF_READ_MEMORY_MB` | `1024` | Transient buffers of field reads in progress; a read that does not fit is a 503 |

Each catalogued file stays open with its metadata while the catalog keeps
it, one file descriptor each. Developer notes, measurements and the rules
the code follows are in `CLAUDE.md`.
