# GFS index fixtures

These are NOAA wgrib2 sidecars; tests do not download the corresponding large
GRIB files.

- `gfs.t00z.pgrb2.0p25.f006.idx`: reference time 2026-04-08 00 UTC. Includes
  genuinely repeated surface `APCP` and `ACPCP` six-hour accumulation keys.
- `gfs.t00z.pgrb2.0p25.f384.idx`: reference time 2026-09-19 00 UTC, downloaded
  from [NOAA's public GFS bucket](https://noaa-gfs-bdp-pds.s3.amazonaws.com/gfs.20260919/00/atmos/gfs.t00z.pgrb2.0p25.f384.idx).
  Reproduces the reported false ambiguity warnings: distinct named surfaces
  and soil layers must retain distinct catalog identities. The supported
  records contain no repeated catalog keys.
