# ARPEGE 10 m wind: components and native speed/direction

The cross-check fixture for derived wind (#897): one source carrying both
the 10 m wind components and Météo-France's own 10 m wind speed and
direction for the same analysis, so derived values can be compared with
native ones over the whole domain.

- **Source:** Météo-France ARPEGE, 0.1° Europe (EURAT01), run
  2026-05-11T00:00Z, step 0, package SP1, from the public open-data bucket
  `https://object.data.gouv.fr/meteofrance-pnt/pnt/2026-05-11T00:00:00Z/arpege/01/SP1/arpege__01__SP1__000H012H__2026-05-11T00:00:00Z.grib2`
  (78 MB, 13 steps).
- **Licence:** Météo-France open data, Licence Ouverte / Open Licence
  Etalab 2.0. Source: Météo-France.
- **What was fetched:** four messages, 1.4 MB in total, by HTTP range reads
  after walking the file's message headers with 1 KiB range reads (the file
  has no index): `10wdir` (GRIB2 0/2/0), `10si` (0/2/1), `10u` (0/2/2) and
  `10v` (0/2/3), all at 10 m above ground, step 0. Template 3.0 regular
  lat/lon, 741 × 521 nodes from 72°N 32°W to 20°N 42°E, flag table 3.3 =
  `0x30` (earth-relative components), CCSDS packing.
- **Crop:** every 20th node in both directions, so 38 × 27 nodes at 2°
  covering the whole domain, written with ecCodes 2.47 `grib_filter`
  (`Ni`, `Nj`, the 2° increments and the first/last grid points set, then
  `values`, simple packing at 24 bits per value). Nothing is interpolated:
  every value is an original node's. Every other key is the source's.
  Result: four 3257-byte messages, 13 028 bytes,
  sha256 `9bc5889a8a7f4b8dda798f38b398f88fe9ffbcd3ad367dab1fad1ef658cbbce5`.
- **Index:** `arpege-10m-wind.index` is an ECMWF-JSON sidecar written for
  the four messages (`levtype` `sfc`).

At the 1026 nodes, `hypot(10u, 10v)` is within 0.012 m/s of `10si`, and
`atan2(-u, -v)` within 0.46° of `10wdir` wherever the speed exceeds 1 m/s;
44 nodes lie within 5° of north, across the 0/360 wrap.

`derived-wind.toml` serves the fixture twice: as published (`arpege`, whose
native `10si`/`10wdir` stop the derivation) and as components only
(`arpege-uv`, `parameters = ["10u", "10v"]`, which derives `10si` and
`10wdir`). Run from the repository root:

```
cargo run -p server -- --config testdata/grib-arpege-wind/derived-wind.toml
```
