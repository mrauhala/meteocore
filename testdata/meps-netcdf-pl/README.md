# MEPS CF-NetCDF pressure-level fixture

A crop of five real MetCoOp MEPS pressure-level files (control member), as
MET Norway publishes them per lead time on thredds.met.no: run 2026-10-15
06Z leads 00–02 and run 09Z leads 00–01, the same runs and grid as the
surface sample in `../meps-netcdf`. CC-BY-4.0, MET Norway.

- `meps_pl_LL_YYYYMMDDTHHZ.cdl`: the source, named like the file on the
  server; `….nc` is built from it with `ncgen -k nc4 -o <name>.nc
  <name>.cdl`.
- `make_cdl.py`: writes the CDLs from the OPeNDAP ASCII responses listed
  below (`make_cdl.py <dir of .ascii files> <output dir>`).

## Provenance

Fetched 2026-10-18 through OPeNDAP, one request per file and pressure
index `K` (5, 7, 10, 12: 300, 500, 850 and 1000 hPa), saved as
`<file>.k<K>.ascii`:

```
https://thredds.met.no/thredds/dodsC/meps25epsarchive/2026/10/15/{06,09}/member_00/meps_pl_{LL}_20261015T{RR}Z.nc.ascii?time,forecast_reference_time,pressure[K:K],air_temperature_pl[0:0][K:K][0:100:1000][0:100:900],x_wind_pl[…],y_wind_pl[…],geopotential_pl[…],relative_humidity_pl[…]
```

with the same hyperslab `[0:0][K:K][0:100:1000][0:100:900]` for every
field: every 100th row and column, 11 rows × 10 columns. `x[0:100:900]`,
`y[0:100:1000]` and the 2-D `latitude`/`longitude[0:100:1000][0:100:900]`
(`latlon.ascii`) came from run 06Z lead 00. The attributes are those of
`meps_pl_00_20261015T06Z.nc`'s DAS; its DDS lists the family's 18
variables, all on the one `pressure` axis, and a request for `pressure`
itself gave its 13 values. 24 requests, about 265 KB in all. Each file's `meteocore_fixture` global attribute names its source
file.

## What is kept from the source

- The values, as OPeNDAP printed them (8 significant digits). The 300 hPa
  `x_wind_pl` reaches 72 m/s in run 06Z lead 00: the jet the pressure-level
  wind styles are sized for.
- The pressure axis `pressure`: Float32 in hPa, `standard_name =
  air_pressure`, `positive = down`, stored top first as in the source (the
  source has 13 levels, 50…1000 hPa; the fixture keeps 4 of them in the
  same order). engine-netcdf serves them bottom first.
- `y` ascends: row 0 is the southern edge, as in the source.
- The grid mapping `projection_lambert` (tangent cone at 63.3°N, central
  meridian 15°E, sphere of 6 371 000 m), units (`m^2/s^2` for geopotential,
  `1` for relative humidity), standard names, long names,
  `_FillValue = 9.96921e36` and the time units
  `seconds since 1970-01-01 00:00:00 +00:00`.
- The storage: one level of one field per chunk (`1, 1, 11, 10`), shuffle
  and deflate level 3, and `time` unlimited.
- The wrong global `min_time`/`max_time` (the run time, not the valid time).

Left out: the source's other 13 variables, its other 9 levels and its other
global attributes. The real grid is 949 × 1069 at 2 500 m.
