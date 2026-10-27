# GFS fields in complex packing with spatial differencing

Fixtures for the template 5.3 point reader (#1302): two global fields at
1°, one with a bitmap, in GRIB2 data representation template 5.3 (complex
packing, second-order spatial differencing) as GFS 0.25° ships every field.

- **Source:** NOAA GFS 0.25°, run 2026-10-26T00:00Z, step 6, from
  [NOAA's public GFS bucket](https://noaa-gfs-bdp-pds.s3.amazonaws.com/gfs.20261026/00/atmos/gfs.t00z.pgrb2.0p25.f006),
  two messages by HTTP range reads at the offsets its `.idx` sidecar gives:
  `TMP:2 m above ground` (868 624 bytes at 409 669 967) and
  `SOILW:0-0.1 m below ground` (363 437 bytes at 403 140 083, a land-only
  field with a bitmap).
- **Licence:** NOAA data, public domain (U.S. Government work).
- **Crop:** every 4th node in both directions, so 360 × 181 nodes at 1° from
  90°N 0°E, written with ecCodes 2.47 `grib_filter` (`Ni`, `Nj`, the 1°
  increments and the first/last grid points set, then `values`; for SOILW
  `bitmapPresent = 1` with the source's masked nodes missing), then
  `packingType = "grid_complex_spatial_differencing"`. Nothing is
  interpolated: every value is an original node's. ecCodes chose its own
  groups and scaling (second order, 2-octet descriptors, no missing value
  management).
  - `gfs-tmp2m-1deg.grib2`: 60 668 bytes, 65 160 values, 2 309 groups,
    sha256 `49a06fddb98b91474f76adf833cd53e746f48f4087970a59b207c2fe333fa2b7`.
  - `gfs-soilw-1deg.grib2`: 29 149 bytes, 22 071 values of 65 160 nodes
    under an 8 145-byte bitmap, 508 groups,
    sha256 `9cc1a443158775d30222c409c6596116d1b1e8f9bcd228504345fbc7133d0965`.
