# THREDDS catalog fixture

One real THREDDS `catalog.xml`, for the catalog discovery of engine-netcdf
(`catalog_url`, #1170) and its parser in ds-storage
(`crates/storage/src/discovery/thredds.rs`).

- `meps25epsarchive_2026_10_19_catalog.xml`: the catalog of one MEPS day
  directory on MET Norway's thredds.met.no, fetched 2026-10-20 with one
  plain `curl` of
  `https://thredds.met.no/thredds/catalog/meps25epsarchive/2026/10/19/catalog.xml`.
  Unedited: 21 022 bytes, CRLF line ends as served, SHA-256
  `b7c8fcfb5e08a7e1373f10aef64cedd61d36f17125e27848283bb86695472d66`.
  Small enough to keep whole: no trimming.

## What it holds

- Five compound `service`s. The datasets inherit `dapOnly` (OpenDAP only),
  but `allServices`, `ncssServices` and `DODShttp` also name the
  `HTTPServer` service, base `/thredds/fileServer/`: the fileServer the
  files are read from by range.
- 48 datasets of the day directory, each with `name`, `ID`, `urlPath`, a
  `dataSize` and a `date type="modified"`: the eight per-run
  `meps_det_2_5km_20261019T{00,03,…,21}Z.nc` and eight
  `meps_det_vc_2_5km_…` files in `Gbytes` (71.56 for the 00Z run), and
  32 NcML aggregations in `Kbytes` (8.052). THREDDS writes sizes with four
  significant digits, so a size is never a file's exact byte count.
- 24 `catalogRef`s, `00/catalog.xml` to `23/catalog.xml`, one per hour
  directory, each holding the per-lead files of its run under
  `member_NN/`. The examples read `member_00` of the runs every 3 hours.

Where it is used:

- ds-storage `discovery::thredds::tests` parse it, and serve it in a
  scripted store to pin the catalog route against the template route on
  its eight `meps_det_2_5km` files.
- engine-netcdf `tests/integration.rs` serves it from a local HTTP server
  as the catalog of the day of a run an hour old: its `catalogRef`s are
  relative, so any day's path works.

Both write the catalogs above and below it with `quick_xml::Writer`.

Licence: catalog metadata of MET Norway's open data, CC-BY-4.0.
