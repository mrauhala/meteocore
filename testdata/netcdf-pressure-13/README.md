# MEPS's 13 pressure levels (synthetic)

A synthetic CF-NetCDF file for engine-netcdf's OPeNDAP EDR window tests:
`pressure_13_levels.cdl`, written by `make_cdl.py <output dir>`, and its
`ncgen -k nc4` output `pressure_13_levels.nc`. Not real data.

- A pressure axis `pressure` in hPa (`standard_name = air_pressure`,
  `positive = down`) with MEPS's 13 levels, stored top first as MEPS stores
  them: 50, 100, 150, 200, 250, 300, 400, 500, 700, 800, 850, 925, 1000,
  served bottom first (1000 hPa is stored index 12).
- A 0.5° latitude/longitude grid: 8 rows from 59°N, stored south to north
  as MEPS stores its rows, and 10 columns from 20°E.
- One run, `forecast_reference_time` 2026-10-15T06Z, and its first six
  hourly steps in one file.
- `air_temperature_pl` (K) and `relative_humidity_pl` (1), each value a
  formula of its indices (in `make_cdl.py`), every level distinct; one
  `_FillValue` cell (step 0, 500 hPa, stored row 4, column 5).
- One level of one field per chunk, shuffle and deflate level 3.
