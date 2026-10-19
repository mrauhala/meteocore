# Global CF-NetCDF fixture

A synthetic global grid for engine-netcdf's tests: `global_5deg.cdl`,
written by `make_cdl.py > global_5deg.cdl`, and its `ncgen -k nc4` output
`global_5deg.nc`.

- 72 × 36 cells of 5°, centres at longitude −177.5…177.5 and latitude
  87.5…−87.5 (stored north to south), so the columns fill 360° and wrap.
- Three 6-hourly steps from 2026-01-01T00Z, `hours since 2026-01-01
  00:00:00`, and no `forecast_reference_time`: one timeline without runs.
- `air_temperature` (float) = 273.15 + 30 cos(lat) + 20 sin(lon) + step,
  smooth across the antimeridian, for the seam box 170,10,−170,20.
- `t_packed`: the same in a `short` with `scale_factor` 0.01 and
  `add_offset` 273.15, and its `_FillValue` in the cells between 0° and
  10°E and 0° and 10°N.
- Both chunked one step per chunk, shuffle and deflate level 3.
