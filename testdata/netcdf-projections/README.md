# CF grid-mapping fixtures

Synthetic grids for engine-netcdf's `polar_stereographic`,
`lambert_azimuthal_equal_area` and `transverse_mercator` grid mappings
(#1167): one directory per mapping, each a collection of its own, with the
CDL written by `make_cdl.py .` and its `ncgen -k nc4` output.

Each file has 1-D projected `x`/`y` in metres, a grid-mapping variable
`crs`, one step (2026-01-01T00Z) and no `forecast_reference_time`. Its two
fields are each cell centre's own projected coordinates, `easting` (x) and
`northing` (y), linear in the grid: a read at a longitude/latitude returns
the projected coordinates the server puts that point at, which the tests
compare with PROJ's `cs2cs`.

| Directory | Mapping | Grid |
|---|---|---|
| `polar-stereographic/` | EPSG:3413's parameters: true scale at 70°N, straight vertical longitude −45°, WGS84 | 9 × 9 cells of 250 km, x and y −1000…1000 km: the North Pole is the centre cell. Rows north to south |
| `laea/` | EPSG:3035's parameters: origin 52°N 10°E, false origin 4321 km, 3210 km, GRS80 | 9 × 9 cells of 250 km, x 3500…5500 km, y 2250…4250 km. Rows south to north |
| `transverse-mercator/` | UTM zone 33N: 15°E, k0 0.9996, false easting 500 km, WGS84 | 11 × 13 cells of 50 km, x 250…750 km, y 7000…6400 km. Rows north to south |
