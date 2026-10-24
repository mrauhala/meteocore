# netcdf-opendap

What MET Norway's thredds.met.no served through OPeNDAP (DAP2, `dodsC`) for
two MEPS files of the 2026-10-23 12Z run, control member, fetched
2026-10-23 for engine-netcdf's OPeNDAP reads (#1264). CC-BY-4.0, MET Norway.

Base URL: `https://thredds.met.no/thredds/dodsC/meps25epsarchive/2026/10/23/12/member_00/`

| File | Request | Notes |
|---|---|---|
| `meps_pl_00_20261023T12Z.nc.dds` | `meps_pl_00_20261023T12Z.nc.dds` | whole, as served (4 818 bytes) |
| `meps_pl_00_20261023T12Z.nc.das` | `meps_pl_00_20261023T12Z.nc.das` | whole, as served (10 308 bytes) |
| `meps_pl_00_20261023T12Z_coordinates.dods` | `….nc.dods?time,forecast_reference_time,pressure,projection_lambert,x,y` | whole: the reply an open asks for |
| `meps_sfc_00_20261023T12Z.nc.dds` | `meps_sfc_00_20261023T12Z.nc.dds` | trimmed: every scalar and 1-D declaration, and the Grids of `longitude`, `latitude` and the eight variables `collections.d/meps-netcdf-opendap.toml.disabled` serves (the whole DDS is 49 929 bytes) |
| `meps_sfc_00_20261023T12Z.nc.das` | `meps_sfc_00_20261023T12Z.nc.das` | trimmed to the containers of the variables kept, `NC_GLOBAL` and `DODS_EXTRA` (the whole DAS is 98 124 bytes) |
| `meps_sfc_00_20261023T12Z_coordinates.dods` | `….nc.dods?time,forecast_reference_time,mean_sea_level,…,projection_lambert,x,y` (every scalar and 1-D coordinate, in DDS order) | whole: the reply an open asks for |
| `meps_sfc_00_20261023T12Z_t2m_4x4.dods` | `….nc.dods?time,forecast_reference_time,height1,x[0:1:3],air_temperature_2m.air_temperature_2m[0][0][500:1:503][400:1:403],projection_lambert` (brackets sent as `%5B`/`%5D`) | whole: a 4 × 4 hyperslab of 2 m temperature; the reply lists the variables in dataset order, not the constraint's |
| `meps_sfc_00_20261023T12Z_rh_t2m_2x2.dods` | `….nc.dods?relative_humidity_2m.relative_humidity_2m[0][0][501:1:502][401:1:402],air_temperature_2m.air_temperature_2m[0][0][501:1:502][401:1:402]` (brackets sent as `%5B`/`%5D`), fetched 2026-10-24 | whole (371 bytes): the 2 × 2 windows of two variables in one reply, each Grid's array in a `Structure` of its own, 2 m temperature first, in dataset order, though asked second |

Every reply came `Transfer-Encoding: chunked`, without `Content-Length`,
`Last-Modified` or ETag, and the `.dods` uncompressed although the request
asked for `Accept-Encoding: deflate, gzip`. The same files' fileServer URLs
answered 403 (`Service Not Allowed: HTTPServer not enabled for dataset`).

`src/dap/tests.rs` parses and decodes these; `src/opendap/tests.rs` serves
them as captured and ingests them as a MEPS file is ingested;
`src/opendap/tests/windows.rs` answers the windows EDR point queries ask
for (#1171) with the 4 × 4 reply and with cells of it, re-encoded, and a
point query of two variables (#1274) with the two-variable reply.
