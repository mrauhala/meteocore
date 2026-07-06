# ds-3dtiles crate — Claude Instructions

Framework-free OGC 3D Tiles byte encoder (mirrors `ds-render`/`ds-mvt`):
`.pnts` point clouds, glTF `.glb` isosurface/echo-top meshes,
`EXT_primitive_voxels` voxel content, `tileset.json`. Engines return domain
types (`VolumePointCloud`, `VoxelGrid`); this crate turns them into bytes.
Epic #346.

**Many rules here are render-verified against CesiumJS, not derivable from
specs. When changing an encoder, render-verify in the bundled viewer before
claiming correctness.**

## Encoder gotchas (load-bearing, all render-verified)

- Tileset `geometricError` must be > 0.
- `.pnts` POSITION is ECEF-native, with `RTC_CENTER` for placement. Use real
  `.pnts`, NOT glb-with-POINTS.
- **Do NOT add `BATCH_ID` to the `.pnts`** to make points pickable — it
  disables per-point `pointSize` in CesiumJS's point-cloud pipeline
  (verified; `pickMetadata` also can't read a `.pnts` batch table in 1.124).
  The per-point physical `value` goes in the batch table (no BATCH_ID ⇒
  per-point properties, exposed to the style engine as `${value}`), and is
  read off color + size.
- `content_uri` is validated (no `..`/absolute/scheme). Datetime query
  strings contain colons — validate the *path* portion, not the whole URI.
- glTF content: a runtime re-applies Y-up→Z-up to glTF, so mesh vertices are
  pre-flipped Z-up→Y-up at encode. The `.pnts` path skips this flip (pnts is
  not glTF).
- `.pnts` tilesets are region-only (RTC_CENTER self-places); glTF tilesets
  carry the antenna ECEF as the tile **`transform`** (`tileset_json_glb`) —
  glTF content has no embedded origin. The geodetic `region` is unaffected.

## Isosurface (`src/isosurface.rs`, #357/#363/#382)

- `encode_isosurface_glb(grid, threshold, color, background)` extracts a
  constant-value shell from a `VoxelGrid` as a plain glTF 2.0 `.glb` — renders
  in any 3D Tiles 1.1 client (the verifiable alternative to the draft voxel
  path).
- **Marching tetrahedra, not marching cubes** — a tet is K4, so the surface
  crosses exactly `|inside|·|outside|` edges and topology is correct by
  construction (no 256-case table to mis-transcribe; chosen because output
  isn't render-checkable at encode time). Cube → 6 tets (Kuhn split).
- Vertex mapping: fractional cell index → ground/azimuth/height (same
  cell-centre convention as the engine sampler) → `destination_point` +
  `geodetic_to_ecef` (both in `ds_core::geo`), stored antenna-relative.
- **Sealing (load-bearing for radar):** `background=Some(bg < threshold)`
  treats every NaN corner as no-echo so the surface closes into solid blobs.
  This is the DEFAULT the API + demo use (`Some(-32.0)`) — open boundaries
  render as vertical curtains. `None` skips NaN-touching tets (open surface;
  the "honest boundary" mode: clear air still seals, unmeasured stays open).
- **Nested multi-threshold shells (#363):** `encode_isosurfaces_glb(grid,
  &[IsoShell], background)` meshes several thresholds into ONE `.glb` — one
  primitive + material per shell; alpha < 255 ⇒ `alphaMode: "BLEND"`, opaque
  shells stay OPAQUE (keeps depth-writes). `nested_shells(thresholds,
  colormap)` is the shared colour/alpha-ramp policy (outer 35% opacity →
  inner opaque; single threshold = opaque). Primitives are emitted
  **innermost-first**: glTF has no draw order, but nested shells share a
  bounding-sphere centre, so CesiumJS's back-to-front translucency sort ties
  and primitive order breaks the tie (render-verified). The blur runs ONCE
  (shells re-march the same smoothed field). A threshold above all data is
  skipped (a weak storm still shows its envelope); all-empty ⇒ `Empty`;
  `background` must sit below the LOWEST threshold; the triangle cap bounds
  the SUM.
- **Indexed mesh + smooth normals (#382):** `MeshBuilder` interns vertices by
  exact position bits (marching-tet shared crossings are bit-identical — no
  quantisation) and accumulates area-weighted outward face normals,
  normalized at encode. Kills the flat-shaded facets and shrinks the `.glb`
  ~2.5×. Keep the `out_ref` outward-orientation logic — it keeps winding +
  normals consistent.

## Echo-top (`src/echo_top.rs`, #362)

Two encoders off the per-`(radius, azimuth)`-column echo top (highest cell ≥
threshold, crossing-interpolated), each a height-coloured `.glb` (per-vertex
`COLOR_0`, normalized u8 VEC4; reuses `index_to_gltf_pos` +
`tileset_json_glb`):

- `encode_echo_top_columns_glb` — **extruded bins**: one solid box per column
  from the ground up to its echo top, walls + flat normals. The PREFERRED
  look (no open edges; reads as a 3-D bar field of storm depth). ~7 MB.
- `encode_echo_top_glb` — thin draped surface (quad only where all four
  corner columns have a top; clear air is a hole), smooth normals. ~0.3 MB;
  floats at echo-top height with no sides — columns look better.

**Colormap gotcha:** colour by HEIGHT with stops AT height values
(`LutColorMap::from_stops`). A builtin colormap's stops are in its own units
(Temperature's are °C) and collapse to one colour over a 0–15 km range.

The echo-top mesher will be reused for VIL (#365).

## True cylindrical voxels (`src/voxels.rs`, #351)

`VoxelGrid` → `EXT_primitive_voxels` glTF `.glb` + a
`3DTILES_content_voxels`/`3DTILES_bounding_volume_cylinder` tileset that
CesiumJS ≥ 1.142 ray-marches (render-verified).

- **The extension is a CesiumGS draft, NOT in the Khronos registry; CesiumJS
  is the sole implementation. Encode against the live `VoxelCylinder3DTiles`
  fixtures, NOT the README** — the README is stale (cylinder `mode` =
  `2147483650` = 0x80000002; box 0x80000000, ellipsoid 0x80000001 — not the
  README's 2147483647).
- **Axis swap:** content `[radius, angle, height]` → glTF `dimensions
  [radius, height, angle]`; data is radius-fastest → height → angle-slowest
  (our grid is the transpose).
- `EXT_structural_metadata` (schema + `propertyAttributes`) goes at the glTF
  **top level**; BIN buffer embedded; **implicit OCTREE tiling + a constant
  `.subtree` is required** even for a single tile.
- **Tile `transform` must be the real ENU→ECEF frame** (east/north/up), NOT
  identity — identity works for the mesh products (absolute-ECEF vertices)
  but tilts the *parametric* cylinder by the latitude.
- **Azimuth remap (else the volume is rotated 90° AND mirrored vs the
  point/mesh products):** the grid angle axis is radar azimuth (index 0 →
  bearing 0, clockwise from North), but CesiumJS's `VoxelCylinderShape`
  (verified in 1.142 source) uses angle bounds −π..+π, counter-clockwise
  from local +X (= East via the ENU transform). The encoder maps each output
  angle slot `s` to the radar bin at compass bearing `270° − φ` where
  `φ = −π + (s+0.5)/nA·2π` (`grid_azimuth_index`). **The +180° twist (270°,
  not the 90° the stated convention implies) is render-verified, not
  derived** — ALWAYS render-verify a voxel-cylinder azimuth mapping against
  the point cloud.
- Unmeasured (NaN) cells → the no-echo floor (−32 dBZ), **no `noData`
  sentinel** — an extreme sentinel trilinearly interpolates into hard walls
  at the data boundary. The floor (faded by the transfer function) keeps
  boundaries soft, at the cost of a dense volume (no empty-space skipping).
- The cellular field is smoothed by a multi-pass separable blur
  (`smooth_grid`, 4 passes) so the cell lattice doesn't show at close zoom.
- Cylinder extents come from `VoxelGridCaps.radius_m`/`height_m` (O(1)); the
  bounding cylinder is lifted by `height/2` so data sits 0..H above the
  antenna.
- v1 MVP: single tile, latest time. Octree/mosaic/animation are follow-ups.
