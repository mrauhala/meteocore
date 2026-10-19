# engine-netcdf

MeteoCore engine that serves gridded CF-NetCDF forecasts and analyses through
WMS 1.3.0, OGC API - Maps, OGC API - Tiles and OGC API - EDR
(`engine_type = "netcdf"`, issue
[#1067](https://github.com/mrauhala/meteocore/issues/1067)). The first
target is MET Norway's MEPS, as published per lead time on thredds.met.no.

## Scope

**In:**

- A local directory of NetCDF-4 (HDF5) or classic CDF-1/2/5 files of one
  family: every file carries the collection's variables on one grid.
- Or the same family on an HTTP server that cannot be listed, such as MET
  Norway's thredds.met.no fileServer: files found by probing the names a
  template gives for each run and lead, read in place by HTTP range.
- Grids: 1-D latitude/longitude, or 1-D projected x/y in metres with a CF
  `grid_mapping`: `lambert_conformal_conic` (a sphere or WGS84),
  `geostationary`, `latitude_longitude`, `polar_stereographic` (its scale
  by `standard_parallel` or `scale_factor_at_projection_origin`; both must
  agree), `lambert_azimuthal_equal_area` and `transverse_mercator`. The
  mapping must state its earth figure; the last three are served on WGS84
  (GRS80 matches it to 0.1 mm) and refused on a sphere or another
  ellipsoid. A grid around a pole (a polar stereographic Arctic grid)
  advertises an extent reaching the pole. Each mapping is pinned to PROJ
  `cs2cs` (`ds_core::cf` tests and `tests/grid_mappings.rs`); transverse
  Mercator keeps the server's bound of 50° from the central meridian
  (#1113). Rows stored south to north (MEPS) are drawn north-up.
- Valid times from the fields' `time` axis; model runs from
  `forecast_reference_time`, each run a WMS `reference_time` value. Files
  without one form one timeline without runs.
- Single-level fields (any axis other than time and ensemble has one
  value), or fields on one pressure axis: a dimension whose coordinate's
  units are a pressure (`hPa`, `Pa`, `mbar`, …). Its levels, in hPa, are
  the collection's vertical axis (below). Ensemble files are read at one
  member.
- CF packing and masking: `scale_factor`/`add_offset`, `_FillValue`,
  `missing_value`, NaN.
- Display units, as GRIB and BUFR serve them (#1209): each variable's CF
  `units`, in any spelling, through `ds_core::units::cf_display_conversion`:
  K as °C, Pa as hPa, kg m⁻² as mm, kg m⁻² s⁻¹ as mm/h, m² s⁻² as gpm, and
  CF's dimensionless `1` as % where the variable's `standard_name` is a
  fraction (`relative_humidity`, any `…_area_fraction` or `…_area_fraction_in_atmosphere_layer`). Values are sampled
  from the stored field and converted after, in `f64`, on every API, so a
  map pixel, an EDR value and GetFeatureInfo agree. A unit without a rule,
  such as m/s, or `1` with any other standard name or none
  (`specific_humidity`, `land_binary_mask`), is served as stored.
- EDR position (one point or several), area and radius, each model run an
  EDR instance; WMS GetFeatureInfo reads a layer's value through the
  position query.
- Wind speed derived from u/v components the CF `standard_name` identifies
  (`x_wind`/`y_wind`, `eastward_wind`/`northward_wind`), on every API:
  MEPS's `x_wind_10m`/`y_wind_10m` give `wind_speed_10m`, and on pressure
  levels `x_wind_pl`/`y_wind_pl` give `wind_speed_pl`, level by level.
  Grid-relative components on a projected grid give no direction yet.

**Not yet:**

- Height above ground and hybrid model levels (MEPS `hl`, `ml`): their axes
  are not served (`VerticalKind::Height` is height above mean sea level,
  and `ModelLevel` expects ordinal levels). EDR trajectory and cube, wind
  direction from grid-relative components (rotation by the convergence
  angle, #897).
- Per-variable file layouts, curvilinear 2-D latitude/longitude grids,
  the oblique `stereographic`, `rotated_latitude_longitude` and other
  mappings, polar stereographic, azimuthal equal-area and transverse
  Mercator grids on a sphere or a non-WGS84 ellipsoid (NSIDC's Hughes
  1980 grids, EPSG:3411), projected x/y in kilometres, ensemble members
  as a dimension.

## Configuration

```toml
id = "meps-netcdf-local"
engine_type = "netcdf"
apis = ["edr", "wms", "maps", "tiles"]   # default ["edr"]

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
committed MEPS sample, and `collections.d/meps-netcdf-pl-local.toml` one on
the committed pressure-level sample (below). Values and their reported
units are in display units: MEPS's temperatures in °C (stored in K), MSLP
in hPa (Pa), the accumulated precipitation in mm (`kg/m^2`), geopotential
in gpm (`m^2/s^2`), and relative humidity and cloud cover in % (unit `1`,
standard names `relative_humidity` and `cloud_area_fraction`); wind in m/s
as stored. Layers without a `[[wms.parameters]]` entry take the built-in
default style their name and served unit select: temperature over
−90…50 °C, MSLP over 950–1050 hPa, wind speed and gusts, the grid-relative
`x_wind_10m`/`y_wind_10m` components (±40 m/s), relative humidity and cloud
and fog area fractions in % on the percent palettes (0–100), the
run-start `precipitation_amount_acc` (0–200 mm,
`precipitation_accumulation`) and geopotential (0–21 000 gpm). The surface
examples keep one override, the 10 m wind components at ±20 m/s.

### Pressure levels

A collection has one vertical axis, so a family on pressure levels, such as
MEPS's per-lead `pl` files (18 variables on 13 levels, 1000 to 50 hPa), is
a collection of its own:

```toml
[netcdf]
data_path = "testdata/meps-netcdf-pl"
filename_template = "meps_pl_{lead}_%Y%m%dT{run}Z.nc"
variables = ["air_temperature_pl", "x_wind_pl", "y_wind_pl", "geopotential_pl",
             "relative_humidity_pl"]
```

- **The axis:** `RasterInfo.vertical` and EDR's `extent.vertical` are
  `VerticalKind::Pressure` in hPa, bottom first (the largest pressure
  first, as GRIB's pressure views). A file storing Pa (ERA5 and many CF
  writers) is converted; MEPS stores hPa.
- **Selecting a level:** WMS `ELEVATION`, Maps/Tiles `elevation` and EDR
  `z` match a level exactly; a level the collection lacks is a 400 (WMS
  GetMap: the error image). A map without one draws the first, bottom
  level (1000 hPa for MEPS), and GetFeatureInfo reports and reads that
  level.
- **EDR `z`:** a level, a list, an interval or `Rn/min/step`, the levels
  kept in request order; without `z` every level. One level is a
  `PointSeries` with a one-value `z` axis, several a `VerticalProfile` per
  step; an area puts the levels on a `z` axis, `[t, z, y, x]`.
- **One level is one field:** fetched (one ~1.4 MB chunk for MEPS),
  decoded, cached and counted by the query cap on its own, so a query
  reads steps × levels × variables fields. A position series over a whole
  MEPS run at all 13 levels (871 fields) is past the cap: name the levels.
- **One axis per collection:** with `variables`, every listed variable must
  be on the same levels, else the file is skipped naming the one that is
  not; without, the largest group of the first file's fields on the same
  levels wins (single-level fields on a tie) and the rest are skipped with a WARN. Later files must
  have the same levels.
- **Styles:** the built-in defaults give `x_wind_pl`/`y_wind_pl` ±80 m/s and
  the derived `wind_speed_pl` 0–80 m/s, twice the surface scale: one range
  serves every level, and the jet at 300–250 hPa reaches 60–80 m/s (the
  committed sample's 300 hPa x component, 72 m/s). Temperature
  (−90…50 °C), geopotential (0–21 000 gpm) and relative humidity
  (0–100 %) keep their all-levels ranges.

`collections.d/meps-netcdf-pl-thredds.toml.disabled` reads the MEPS `pl`
family, control member, 67 leads, from thredds.met.no, with its traffic
stated (measured 2026-10-18: 4 GETs and 1.23 MB to open a pl file for one
variable, 1.42 MB per field).

### EDR

- **Run and steps:** an `/instances/{id}/…` query reads that run; otherwise
  a `datetime` window reads the newest run with a step inside it that also
  spans the window's start, else the newest with any step inside it. An
  instant matches a step valid at exactly that time. No step in the window
  is a 404, so a `datetime` list skips that instant. Without `datetime`, a
  position series is every step of the newest run, and an area is the step
  a map without `TIME` draws (the newest run's last).
- **Position** projects the point onto the grid once and samples every step
  bilinearly in the grid's x/y, then converts to the display unit, as the
  map draws it, so GetFeatureInfo answers what GetMap drew. A point off the
  grid is null.
- **Area and radius** are a CRS84 grid over the polygon's bbox at the
  grid's mean cell size in degrees, at most 256 cells per axis; each cell
  centre is projected once, and cells outside the polygon are null. A
  polygon outside the grid's extent is a 404. A `datetime` interval puts
  its steps on a `t` axis.
- **Cost:** each field is read once per query for every point, at most four
  at a time. A field a map has just drawn is reused; any other is decoded
  for the query and not kept, so a series never evicts the frames being
  drawn. A remote field's ~1.5 MB chunk is cached, and a read that outlives
  the 30 s query deadline still lands in that cache, so a retry makes
  progress. A query may decode at most `MC_NETCDF_EDR_QUERY_MB` (544 MiB):
  one parameter over one MEPS run of 67 leads as the client sees it,
  including a derived wind speed; more is a 400 that names the fields and
  the limit, before anything is read. The cap counts the fields read, steps
  × levels × variables: a derived wind speed reads both of its components,
  134 fields over a whole run, and three variables over a run do not fit,
  nor one variable over a run at all 13 MEPS pressure levels. A query whose
  points all lie off the grid reads nothing.

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
prewarm_variables = []                                # newest run fetched ahead (bottom level)
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
parsed metadata, so 67 leads × 2 runs is some 110 MB. Prewarm fetches the
level a map without `ELEVATION` draws, the bottom one on a pressure family.

| Variable | Default | Meaning |
|---|---|---|
| `MC_NETCDF_FIELD_CACHE_MB` | `256` | Decoded fields (`f32`, ~4 MB per MEPS field), shared by every NetCDF collection |
| `MC_NETCDF_CHUNK_CACHE_MB` | `512` | Compressed chunks of remote files (~1.5 MB per MEPS field) |
| `MC_NETCDF_BLOCK_CACHE_MB` | `64` | 64 KiB metadata blocks of remote files; at least 8 |
| `MC_NETCDF_READ_MEMORY_MB` | `1024` | Transient buffers of field reads in progress; a read that does not fit is a 503 |
| `MC_NETCDF_EDR_QUERY_MB` | `544` | Decoded bytes one EDR query may read, counted from the metadata before any read: steps × pressure levels × variables read × one field (4.06 MB for MEPS); over it a 400. The default admits one parameter over one MEPS run, including a derived wind speed, which reads both components |

Each catalogued file stays open with its metadata while the catalog keeps
it, one file descriptor each for local files. Developer notes, measurements
and the rules the code follows are in `CLAUDE.md`.
