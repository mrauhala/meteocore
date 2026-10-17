# Remote-read NetCDF fixture

Two synthetic per-lead files of one MEPS-like run, for engine-netcdf's
HTTP range tests (`crates/engine-netcdf/src/remote/tests.rs`). Not real
data: the values are a smooth field plus deterministic noise.

- `meps_sfc_{00,01}_20261015T06Z.nc`: run 2026-10-15 06Z, leads 00 and 01
  (valid 06Z and 07Z), written by `make_fixture.py <output dir>` through
  `ncgen -k nc4` (netCDF-C). The script is the source: the same files on
  every run.

## What is MEPS-like

- The grid mapping `projection_lambert` and MEPS's `x`/`y` origin and
  2 500 m step, cut to 96 × 64 cells; `y` ascends, so row 0 is south.
- `time` unlimited, a scalar `forecast_reference_time`, both in
  `seconds since 1970-01-01 00:00:00 +00:00`.
- Two fields, `air_temperature_2m` (on `height1`) and `wind_speed_of_gust`
  (on `height0`), each stored as one whole 2-D field per chunk with
  shuffle and deflate level 3.
- The noise keeps each chunk about 15 KB compressed: the tests read with
  production's read sizes divided by 64 (1 KiB blocks, a 16 KiB first
  read, chunk reads over 4 KiB), so the 62 KB files have metadata blocks, a
  first read and chunk reads as a 190 MB MEPS file has at full size.
