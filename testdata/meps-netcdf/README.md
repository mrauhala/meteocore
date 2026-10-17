# MEPS CF-NetCDF fixture

A crop of seven real MetCoOp MEPS surface files (control member), as MET
Norway publishes them per lead time on thredds.met.no: run 2026-10-15 06Z
leads 00–03 and run 09Z leads 00–02, so 09Z is valid in both runs.
CC-BY-4.0, MET Norway.

- `meps_sfc_LL_YYYYMMDDTHHZ.cdl`: the source, named like the file on the
  server; `….nc` is built from it with `ncgen -k nc4 -o <name>.nc
  <name>.cdl`.
- `make_cdl.py`: writes the CDLs from the OPeNDAP ASCII responses listed
  below (`make_cdl.py <dir of .ascii files> <output dir>`).

## Provenance

Fetched 2026-10-17 through OPeNDAP:

```
https://thredds.met.no/thredds/dodsC/meps25epsarchive/2026/10/15/{06,09}/member_00/meps_sfc_{LL}_20261015T{RR}Z.nc.ascii?<constraint>
```

The constraint read `time`, `forecast_reference_time`, `height0`,
`height1`, `height6`, `height_above_msl` and, for each of
`air_temperature_2m`, `relative_humidity_2m`, `x_wind_10m`, `y_wind_10m`,
`wind_speed_of_gust`, `air_pressure_at_sea_level`,
`precipitation_amount_acc` and `cloud_area_fraction`,
`[0:0][0:0][0:100:1000][0:100:900]`: every 100th row and column, 11 rows ×
10 columns. `x[0:100:900]` and `y[0:100:1000]` (Float32, a 250 000 m step)
and the 2-D `latitude`/`longitude[0:100:1000][0:100:900]` (Float64, from run
06Z lead 00) came from the same file. The attributes are those of
`meps_sfc_12_20261015T06Z.nc`'s DAS. Each file's `meteocore_fixture` global
attribute names its source file.

## What is kept from the source

- The values, as OPeNDAP printed them (8 significant digits).
- `y` ascends: row 0 is the southern edge, as in the source.
- The grid mapping `projection_lambert` (tangent cone at 63.3°N, central
  meridian 15°E, sphere of 6 371 000 m), units, standard names, long names,
  `_FillValue = 9.96921e36` and the time units
  `seconds since 1970-01-01 00:00:00 +00:00`.
- Each field's level on its own size-1 dimension (`height1` = 2 m,
  `height6` = 10 m, `height0`, `height_above_msl`), and `time` unlimited.
- The storage: one whole 2-D field per chunk (`1, 1, 11, 10`), shuffle and
  deflate level 3.
- The wrong global `min_time`/`max_time` (the run time, not the valid time).

Left out: the source's other 213 variables and its other global
attributes. The real grid is 949 × 1069 at 2 500 m.
