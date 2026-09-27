# Himawari-9 ISatSS fixture

Three tiles of one real Himawari-9 AHI band 13 (10.4 µm) full-disk scan,
2026-09-27 19:20 UTC. They come from the ISatSS tiles NOAA republishes
(`s3://noaa-himawari9/AHI-L2-FLDK-ISatSS/2026/09/27/1920/`, NOAA/JMA open
data). A full disk is 88 tiles of 550 × 550 on a 10 × 10 lattice over the
5500 × 5500 grid; the 12 corner cells are space and have no tile.

The crops are 64 × 64 around the lattice corner at grid pixel
(row 550, col 1100), on the north-west limb:

| File | Lattice cell | Crop of the tile | Grid rows × cols |
|---|---|---|---|
| `…-T001_…nc` | (0, 2) | rows 486–550, cols 0–64 | 486–550 × 1100–1164 |
| `…-T007_…nc` | (1, 1) | rows 0–64, cols 486–550 | 550–614 × 1036–1100 |
| `…-T008_…nc` | (1, 2) | rows 0–64, cols 0–64 | 550–614 × 1100–1164 |
| — | (0, 1) | no tile (space) | 486–550 × 1036–1100 |

Together they are a 2 × 2 mosaic of 64-pixel tiles with one missing, cut
across the limb. The disk lies in the south-east. Space just off the limb
carries the unmasked stray-light halo (~110–170 K).

The files keep the original packed integers and the ISatSS conventions:
- x/y are packed absolute grid indices with the full-disk `add_offset`.
- The geostationary mapping uses sweep `y`, `lon_0` 140.7 and the
  non-CF `semi_major`/`semi_minor` names.
- There is no `_FillValue` or `valid_range`; far space is packed −1076 ≈ 0 K.

`tile_row_offset`/`tile_column_offset` and `product_tile_width`/`height`
follow the crop, and the `meteocore_fixture` global attribute records it.
Each crop was read from the source tile and written as CDL through
`ncgen -k nc4`.
