# Pressure levels in Pa (synthetic)

A synthetic CF-NetCDF file for engine-netcdf's pressure-level tests (#1067
PR 8): `pressure_levels_pa.cdl`, written by `make_cdl.py <output dir>`, and
its `ncgen -k nc4` output `pressure_levels_pa.nc`. Not real data.

- A pressure axis `level` in Pa (`standard_name = air_pressure`,
  `positive = down`), stored top first: 50000, 85000, 100000 Pa, served as
  1000, 850 and 500 hPa.
- A 1° latitude/longitude grid: 4 rows from 62°N (stored north to south)
  and 5 columns from 20°E.
- Two steps at 00Z and 06Z on 2026-01-01, `hours since 2026-01-01
  00:00:00`, and no `forecast_reference_time`: one timeline without runs.
- `t` (air_temperature, K), `u` (eastward_wind) and `v` (northward_wind),
  m s-1, each value a formula of its indices (in `make_cdl.py`), so the
  tests recompute it: earth-relative components on a north-aligned grid,
  which give a derived speed and direction at each level.
- One level of one field per chunk, shuffle and deflate level 3.
