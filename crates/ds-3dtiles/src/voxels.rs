//! True cylindrical voxels (#351): a [`ds_core::volume::VoxelGrid`] →
//! `EXT_primitive_voxels` glTF `.glb` + a `3DTILES_content_voxels` /
//! `3DTILES_bounding_volume_cylinder` tileset that CesiumJS ray-marches as a
//! volume. The **Tier-B** path — draft, CesiumGS-only (not in the Khronos
//! registry), CesiumJS-only — kept alongside the always-works `.pnts`/mesh
//! products.
//!
//! Encoded against the live CesiumJS `VoxelCylinder3DTiles` fixtures (the draft
//! README is stale), which pin the load-bearing details:
//! - primitive `mode` encodes the shape: **cylinder = `2147483650`** (the
//!   README's `2147483647` is wrong);
//! - the per-cell scalar is a custom vertex attribute (`_VOXEL`), no `POSITION`,
//!   no indices; the buffer is embedded in the glb BIN chunk;
//! - `EXT_structural_metadata` (schema + `propertyAttributes`) lives at the glТF
//!   **top level**, not on the primitive;
//! - **axis swap**: a content grid `[radius, angle, height]` becomes glTF
//!   `EXT_primitive_voxels.dimensions` `[radius, height, angle]`, and the data
//!   is laid out **radius-fastest → height → angle-slowest** (our `VoxelGrid` is
//!   the transpose — height-fastest — so the encoder reorders);
//! - the tileset needs **implicit OCTREE tiling** + a `.subtree` availability
//!   file (CesiumJS's voxel traversal requires it) — for one tile that's a
//!   constant-availability subtree.

use ds_core::geo::geodetic_to_ecef;
use ds_core::volume::{VoxelGrid, NO_ECHO_FLOOR_DBZ};
use serde_json::json;

use crate::Tiles3dError;

/// glTF primitive `mode` for a **cylinder** voxel grid (`0x80000002`). Box is
/// `0x80000000`, ellipsoid `0x80000001`; the high bit marks voxels, the low bits
/// the shape. Read from the CesiumJS fixtures (not the draft README).
const VOXEL_MODE_CYLINDER: u64 = 2_147_483_650;

/// Encode a cylindrical [`VoxelGrid`] as an `EXT_primitive_voxels` glТF `.glb`
/// (self-contained: the float data is embedded in the BIN chunk). The single
/// scalar property is named after `grid.quantity`.
///
/// Returns [`Tiles3dError::Empty`] for a zero-size grid or one with no finite
/// cells (the tileset would have nothing to ray-march).
pub fn encode_voxels_glb(grid: &VoxelGrid) -> Result<Vec<u8>, Tiles3dError> {
    let [n_r, n_a, n_h] = grid.dims;
    let count = n_r
        .checked_mul(n_a)
        .and_then(|x| x.checked_mul(n_h))
        .ok_or(Tiles3dError::TooLarge("voxel count"))?;
    if count == 0 {
        return Err(Tiles3dError::Empty);
    }
    let count_u32 = u32::try_from(count).map_err(|_| Tiles3dError::TooLarge("voxel count"))?;
    // Checked `* 4` (one f32/cell): caller may not honour MAX_VOXELS, so don't
    // let the byte length wrap a `usize` before the `u32` bound catches it.
    let byte_len = count
        .checked_mul(4)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or(Tiles3dError::TooLarge("voxel buffer"))?;

    // `count` (the accessor/buffer length) is `dims` product, but the transpose
    // and `smooth_grid` index `grid.values` by `grid.index(dims, …)` — a values
    // length that disagrees with `dims` would panic out-of-bounds deep inside the
    // `spawn_blocking` task. The engine builds the two consistently; assert it
    // (debug-only, zero release cost) so an engine bug fails loud and clear here.
    debug_assert_eq!(
        grid.values.len(),
        count,
        "voxel grid values ({}) must match dims product ({count})",
        grid.values.len(),
    );

    // Fill **unmeasured** (`NaN`: cone of silence, below the lowest beam, beyond
    // range) cells with the no-echo floor — NOT an extreme nodata sentinel.
    // CesiumJS *trilinearly interpolates* the metadata before the shader, so an
    // extreme sentinel makes the echo→unmeasured transition razor-sharp and
    // grid-aligned. The floor (which the transfer function makes transparent,
    // like real clear air) keeps those transitions gentle; no `noData` is
    // declared, so the whole cylinder is a dense field whose low end fades out.
    let mut native: Vec<f32> = Vec::with_capacity(count);
    let mut any_finite = false;
    for &v in &grid.values {
        if v.is_finite() {
            any_finite = true;
            native.push(v);
        } else {
            native.push(NO_ECHO_FLOOR_DBZ);
        }
    }
    if !any_finite {
        return Err(Tiles3dError::Empty); // every cell unmeasured — nothing to render
    }
    // Radar echo is **cellular** — each ~native-resolution cell is a local
    // reflectivity maximum, so even with GPU trilinear interpolation a raw render
    // shows one blob per cell (a visible grid of cells at close zoom). A light
    // separable 3-D smoothing merges adjacent cells into a continuous field — the
    // standard radar-volume reconstruction. Angle is periodic (full circle);
    // radius/height clamp at the ends.
    let native = smooth_grid(native, grid.dims);

    // Transpose the smoothed grid from native `[radius, angle, height]`
    // (height-fastest) into the glТF cylinder layout `[radius, height, angle]`
    // (radius-fastest → height → angle-slowest), remapping the angle axis from
    // the radar-azimuth convention to CesiumJS's cylinder convention (see
    // [`grid_azimuth_index`]), and track the range.
    let mut bin = Vec::with_capacity(count * 4);
    let mut vmin = f32::INFINITY;
    let mut vmax = f32::NEG_INFINITY;
    for slot in 0..n_a {
        let i_a = grid_azimuth_index(slot, n_a);
        for i_h in 0..n_h {
            for i_r in 0..n_r {
                let f = native[grid.index(i_r, i_a, i_h)];
                vmin = vmin.min(f);
                vmax = vmax.max(f);
                bin.extend_from_slice(&f.to_le_bytes());
            }
        }
    }

    let q = grid.quantity.clone();
    let gltf = json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "mesh": 0 }],
        "meshes": [{
            "primitives": [{
                "mode": VOXEL_MODE_CYLINDER,
                "attributes": { "_VOXEL": 0 },
                "extensions": {
                    // No `noData`: unmeasured cells carry the no-echo floor (a
                    // real low dBZ the transfer function fades out), so the field
                    // is dense and CesiumJS interpolates it without hard walls.
                    "EXT_primitive_voxels": { "dimensions": [n_r, n_h, n_a] }
                }
            }]
        }],
        "extensionsUsed": ["EXT_primitive_voxels", "EXT_structural_metadata"],
        "extensionsRequired": ["EXT_primitive_voxels", "EXT_structural_metadata"],
        "extensions": {
            "EXT_structural_metadata": {
                "schema": voxel_schema(&q),
                "propertyAttributes": [{
                    "class": "voxel",
                    "properties": { q.clone(): { "attribute": "_VOXEL" } }
                }]
            }
        },
        "accessors": [{
            "bufferView": 0,
            "type": "SCALAR",
            "componentType": 5126, // FLOAT
            "count": count_u32,
            "min": [vmin],
            "max": [vmax]
        }],
        "bufferViews": [{ "buffer": 0, "byteLength": byte_len }],
        "buffers": [{ "byteLength": byte_len }]
    });

    crate::assemble_glb(&gltf, bin)
}

/// Map a glTF/CesiumJS cylinder **angle slot** to the source **radar-azimuth**
/// index in the native grid.
///
/// The grid's angle axis is radar azimuth: index `a` is the bearing
/// `(a+0.5)·360/nA` **degrees clockwise from North**. CesiumJS places glTF angle
/// slot `s` at cylinder angle `φ = -π + (s+0.5)/nA·2π` (counter-clockwise; the
/// 1.142 `VoxelCylinderShape` default bounds are -π..+π). Mapping that to a
/// compass bearing and reading the matching radar bin corrects the North-CW vs
/// East-CCW mismatch (a reflection + rotation). The bearing is
/// `270° - φ`, **not** the `90° - φ` that the +X=East tile transform alone
/// predicts: render-verification against the (absolutely-positioned) point cloud
/// showed the naive `90° - φ` still 180°-rotated — CesiumJS's effective cylinder
/// angle origin sits opposite (-X) to where the stated -π bound + ENU transform
/// imply, so a +180° term is required. With it, the voxel echo footprint matches
/// the point cloud exactly. Periodic, so the result is always a valid `0..nA`
/// index.
fn grid_azimuth_index(slot: usize, n_a: usize) -> usize {
    use std::f64::consts::{PI, TAU};
    let phi = -PI + (slot as f64 + 0.5) * TAU / n_a as f64;
    let bearing_deg = 270.0 - phi.to_degrees();
    (bearing_deg / 360.0 * n_a as f64 - 0.5)
        .round()
        .rem_euclid(n_a as f64) as usize
}

/// Separable 3-D smoothing of the (NaN-filled) grid in its native
/// `[radius, angle, height]` index order — `PASSES` applications of a
/// `[0.25, 0.5, 0.25]` kernel along each axis.
///
/// Radar echo is **cellular** (each native cell a local reflectivity maximum)
/// and the voxel grid is coarse in height (~300 m layers), so a *single* pass
/// still leaves the cell floors (height-layer boundaries) and walls
/// (radial/angular boundaries) visible under GPU trilinear interpolation when
/// the camera is close or inside the volume — trilinear is only C0, so every
/// cell boundary is a faint crease whose visibility scales with the field's
/// cell-to-cell contrast. Repeated passes widen the effective Gaussian
/// (`sigma ≈ sqrt(PASSES/2)` cells), dropping that contrast until the field
/// reads as a continuous volume. Angle wraps (full circle); radius/height clamp
/// at the ends. Each axis sweep ping-pongs `src`↔`dst`, so the result is always
/// in `src` regardless of the pass count.
fn smooth_grid(vals: Vec<f32>, dims: [usize; 3]) -> Vec<f32> {
    // 4 passes ⇒ sigma ≈ 1.4 cells (FWHM ≈ 3.3): dissolves the cell lattice —
    // height floors and the fainter angular-sector walls — at close zoom without
    // smearing storm cores into a flat haze.
    const PASSES: usize = 4;
    let [n_r, n_a, n_h] = dims;
    let idx = |r: usize, a: usize, h: usize| VoxelGrid::index_of(dims, r, a, h);
    let blur = |lo: f32, mid: f32, hi: f32| 0.25 * lo + 0.5 * mid + 0.25 * hi;

    let mut src = vals;
    let mut dst = vec![0.0f32; src.len()];

    for _ in 0..PASSES {
        // Height (clamp).
        for r in 0..n_r {
            for a in 0..n_a {
                for h in 0..n_h {
                    let lo = src[idx(r, a, h.saturating_sub(1))];
                    let hi = src[idx(r, a, (h + 1).min(n_h - 1))];
                    dst[idx(r, a, h)] = blur(lo, src[idx(r, a, h)], hi);
                }
            }
        }
        std::mem::swap(&mut src, &mut dst);
        // Angle (periodic).
        for r in 0..n_r {
            for h in 0..n_h {
                for a in 0..n_a {
                    let lo = src[idx(r, (a + n_a - 1) % n_a, h)];
                    let hi = src[idx(r, (a + 1) % n_a, h)];
                    dst[idx(r, a, h)] = blur(lo, src[idx(r, a, h)], hi);
                }
            }
        }
        std::mem::swap(&mut src, &mut dst);
        // Radius (clamp). Loop with `r` OUTERMOST and `h` (the fastest-varying
        // axis, stride 1) innermost: `r` has the largest stride (`n_a*n_h`), so
        // an `r`-innermost sweep would miss cache on nearly every step. Same
        // result, cache-friendly access.
        for r in 0..n_r {
            for a in 0..n_a {
                for h in 0..n_h {
                    let lo = src[idx(r.saturating_sub(1), a, h)];
                    let hi = src[idx((r + 1).min(n_r - 1), a, h)];
                    dst[idx(r, a, h)] = blur(lo, src[idx(r, a, h)], hi);
                }
            }
        }
        std::mem::swap(&mut src, &mut dst);
    }
    src
}

/// The `EXT_structural_metadata` schema for one scalar voxel property named
/// `quantity` (shared by the glТF and the tileset so the classes match).
fn voxel_schema(quantity: &str) -> serde_json::Value {
    json!({
        "id": "voxel",
        "classes": {
            "voxel": {
                "properties": {
                    quantity: {
                        "type": "SCALAR",
                        "componentType": "FLOAT32"
                    }
                }
            }
        }
    })
}

/// Build the implicit-tiling voxel **tileset.json** for one cylinder tile. The
/// tile `transform` places the cylinder's local frame at the antenna ECEF (like
/// the mesh products); `stat_min`/`stat_max` are the reflectivity display range
/// the transfer function maps over (a fixed colormap domain — no grid scan).
///
/// `content_uri` and `subtree_uri` are the implicit-tiling templates
/// (`…/{level}/{x}/{y}/{z}.…`) the API serves; for a single tile only the
/// `0/0/0/0` slot is available (see [`voxel_subtree_json`]).
#[allow(clippy::too_many_arguments)]
pub fn tileset_json_voxels(
    antenna_lon: f64,
    antenna_lat: f64,
    antenna_height: f64,
    max_radius_m: f64,
    height_m: f64,
    dims: [usize; 3],
    quantity: &str,
    stat_min: f64,
    stat_max: f64,
    content_uri: &str,
    subtree_uri: &str,
) -> Result<String, Tiles3dError> {
    // Both URIs are embedded into the tileset CesiumJS fetches; reject anything
    // that could traverse or redirect (same guard as the `.pnts`/mesh tilesets —
    // the contract documented in CLAUDE.md). Today's caller builds them
    // server-side, so this is defence-in-depth for a `pub` API.
    crate::validate_uri(content_uri)?;
    crate::validate_uri(subtree_uri)?;
    for v in [
        antenna_lon,
        antenna_lat,
        antenna_height,
        max_radius_m,
        height_m,
        stat_min,
        stat_max,
    ] {
        if !v.is_finite() {
            return Err(Tiles3dError::NonFinite("voxel tileset parameter"));
        }
    }
    if max_radius_m <= 0.0 || height_m <= 0.0 {
        return Err(Tiles3dError::InvalidRegion([
            0.0,
            0.0,
            max_radius_m,
            height_m,
            0.0,
            0.0,
        ]));
    }
    let [cx, cy, cz] = geodetic_to_ecef(antenna_lon, antenna_lat, antenna_height);
    let [n_r, n_a, n_h] = dims;

    // East-North-Up → ECEF rotation at the antenna, so the cylinder's local z is
    // local **up** (its height axis) and its radial plane is horizontal. (An
    // identity rotation — fine for the mesh products, whose vertices are absolute
    // ECEF — would align the cylinder's z with global ECEF-Z, tilting the whole
    // volume by the latitude; the parametric cylinder needs the real local frame.)
    let (sl, cl) = antenna_lon.to_radians().sin_cos();
    let (sp, cp) = antenna_lat.to_radians().sin_cos();
    let east = [-sl, cl, 0.0];
    let north = [-sp * cl, -sp * sl, cp];
    let up = [cp * cl, cp * sl, sp];

    let tileset = json!({
        "asset": { "version": "1.1" },
        "schema": voxel_schema(quantity),
        "statistics": {
            "classes": {
                "voxel": {
                    "properties": {
                        // `min`/`max` are BARE SCALARS for a SCALAR property per
                        // the 3D Tiles 1.1 EXT_structural_metadata statistics
                        // schema (the array form is for VEC2/VEC3/…). CesiumJS
                        // 1.142 tolerates the array, but a conforming validator
                        // rejects it.
                        quantity: { "min": stat_min, "max": stat_max }
                    }
                }
            }
        },
        // 0 is correct here (NOT the `> 0` rule the .pnts/mesh tilesets need):
        // this is a single-tile implicit-tiling voxel set — there is no finer LOD
        // to refine to, and CesiumJS's voxel traversal renders the leaf directly.
        // (When octree LOD lands, the non-leaf levels get a positive error.)
        "geometricError": 0.0,
        "extensionsUsed": ["3DTILES_bounding_volume_cylinder", "3DTILES_content_voxels"],
        "extensionsRequired": ["3DTILES_bounding_volume_cylinder", "3DTILES_content_voxels"],
        "root": {
            // ENU→ECEF tile transform (column-major): local x=east, y=north,
            // z=up at the antenna, translation = antenna ECEF.
            "transform": [
                east[0], east[1], east[2], 0.0,
                north[0], north[1], north[2], 0.0,
                up[0], up[1], up[2], 0.0,
                cx, cy, cz, 1.0
            ],
            "boundingVolume": {
                "extensions": {
                    // Centred at the local origin, height along z; lift it so the
                    // data sits from the antenna (z=0) up to `height_m`.
                    "3DTILES_bounding_volume_cylinder": {
                        "minRadius": 0.0,
                        "maxRadius": max_radius_m,
                        "height": height_m,
                        "translation": [0.0, 0.0, height_m / 2.0]
                    }
                }
            },
            "content": {
                "uri": content_uri,
                "extensions": {
                    "3DTILES_content_voxels": {
                        // Content/radar order `[radius, angle, height]` here —
                        // deliberately NOT the glb's `EXT_primitive_voxels.dimensions`
                        // `[radius, height, angle]` (the axis-swapped glТF order).
                        // Render-verified: CesiumJS takes the actual layout from
                        // the glb field, so this one is advisory; keep it in the
                        // natural content order.
                        "dimensions": [n_r, n_a, n_h],
                        "class": "voxel"
                    }
                }
            },
            "implicitTiling": {
                "subdivisionScheme": "OCTREE",
                "subtreeLevels": 1,
                "availableLevels": 1,
                "subtrees": { "uri": subtree_uri }
            },
            "geometricError": 0.0,
            "refine": "REPLACE"
        }
    });
    serde_json::to_string_pretty(&tileset).map_err(|e| Tiles3dError::Serialize(e.to_string()))
}

/// The implicit-tiling **subtree** for a single available tile: the one level-0
/// tile and its content are present, with no child subtrees. A JSON subtree (3D
/// Tiles 1.1) with constant availability — matches the CesiumJS fixture.
pub fn voxel_subtree_json() -> String {
    let subtree = json!({
        "tileAvailability": { "constant": 1, "availableCount": 1 },
        // Per the 3D Tiles 1.1 implicit-tiling spec, `contentAvailability` is an
        // ARRAY (one entry per content layer) — unlike the scalar
        // `tileAvailability`/`childSubtreeAvailability`. CesiumJS 1.142 accepts a
        // bare object, but strict validators (and likely future CesiumJS) require
        // the array form.
        "contentAvailability": [{ "constant": 1, "availableCount": 1 }],
        "childSubtreeAvailability": { "constant": 0, "availableCount": 0 }
    });
    serde_json::to_string(&subtree).expect("subtree serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(n_r: usize, n_a: usize, n_h: usize) -> VoxelGrid {
        let dims = [n_r, n_a, n_h];
        let mut values = vec![f32::NAN; n_r * n_a * n_h];
        // value = height index, finite everywhere so the range is [0, n_h-1].
        for i_r in 0..n_r {
            for i_a in 0..n_a {
                for i_h in 0..n_h {
                    values[VoxelGrid::index_of(dims, i_r, i_a, i_h)] = i_h as f32;
                }
            }
        }
        VoxelGrid {
            origin_lon: 24.5,
            origin_lat: 60.5,
            origin_height: 100.0,
            dims,
            radius_range: [0.0, 100_000.0],
            angle_range: [0.0, std::f64::consts::TAU],
            height_range: [0.0, 10_000.0],
            values,
            quantity: "DBZH".into(),
            unit: "dBZ".into(),
        }
    }

    fn parse_glb(glb: &[u8]) -> (serde_json::Value, Vec<u8>) {
        assert_eq!(&glb[0..4], b"glTF", "magic");
        assert_eq!(
            u32::from_le_bytes(glb[4..8].try_into().unwrap()),
            2,
            "version"
        );
        assert_eq!(
            u32::from_le_bytes(glb[8..12].try_into().unwrap()) as usize,
            glb.len(),
            "byteLength"
        );
        let jlen = u32::from_le_bytes(glb[12..16].try_into().unwrap()) as usize;
        assert_eq!(&glb[16..20], b"JSON");
        let json: serde_json::Value =
            serde_json::from_slice(&glb[20..20 + jlen]).expect("JSON chunk parses");
        let bin_off = 20 + jlen;
        let blen = u32::from_le_bytes(glb[bin_off..bin_off + 4].try_into().unwrap()) as usize;
        assert_eq!(&glb[bin_off + 4..bin_off + 8], b"BIN\0");
        let bin = glb[bin_off + 8..bin_off + 8 + blen].to_vec();
        (json, bin)
    }

    #[test]
    fn voxel_glb_matches_the_cesium_cylinder_fixture_shape() {
        let g = grid(2, 4, 3); // [radius, angle, height]
        let glb = encode_voxels_glb(&g).expect("encode");
        let (j, bin) = parse_glb(&glb);

        let prim = &j["meshes"][0]["primitives"][0];
        // Cylinder voxel mode (NOT the README's 2147483647).
        assert_eq!(prim["mode"], 2_147_483_650u64);
        assert_eq!(prim["attributes"]["_VOXEL"], 0);
        // Axis swap: content [radius, angle, height] → glTF [radius, height, angle].
        assert_eq!(
            prim["extensions"]["EXT_primitive_voxels"]["dimensions"],
            json!([2, 3, 4])
        );
        // Structural metadata at the top level, binding the class property to _VOXEL.
        let pa = &j["extensions"]["EXT_structural_metadata"]["propertyAttributes"][0];
        assert_eq!(pa["class"], "voxel");
        assert_eq!(pa["properties"]["DBZH"]["attribute"], "_VOXEL");
        assert_eq!(j["extensionsRequired"][0], "EXT_primitive_voxels");

        // Accessor: SCALAR FLOAT, one element per cell. The 3-D smoothing shifts
        // the boundary values inward, so the range is within [0, 2] (height idx).
        let acc = &j["accessors"][0];
        assert_eq!(acc["componentType"], 5126);
        assert_eq!(acc["count"], 24);
        assert_eq!(bin.len(), 24 * 4);
        let amin = acc["min"][0].as_f64().unwrap();
        let amax = acc["max"][0].as_f64().unwrap();
        assert!(amin >= 0.0 && amax <= 2.0 && amin < amax, "{amin}..{amax}");

        // Data layout: glТF order is radius-fastest → height → angle-slowest, and
        // value == height index (constant across radius+angle). Assert the
        // *structure* (robust to the smoothing): each radius run is flat, height
        // increases monotonically, and the per-angle block repeats.
        let f = |i: usize| f32::from_le_bytes(bin[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!(
            f(0),
            f(1),
            "radius run is flat (value independent of radius)"
        );
        assert!(
            f(2) > f(0) && f(4) > f(2),
            "height increases: {} {} {}",
            f(0),
            f(2),
            f(4)
        );
        assert_eq!(
            f(6),
            f(0),
            "next angle slice repeats (value independent of angle)"
        );
    }

    #[test]
    fn unmeasured_becomes_floor_and_all_unmeasured_is_empty() {
        let mut g = grid(2, 2, 2);
        let idx = g.index(0, 0, 0);
        g.values[idx] = f32::NAN;
        let glb = encode_voxels_glb(&g).expect("encode");
        let (j, bin) = parse_glb(&glb);
        // Unmeasured → the no-echo floor (a real low dBZ), NOT an extreme nodata
        // sentinel — so every (smoothed) cell stays ≥ the floor and CesiumJS's
        // interpolation has no razor-sharp wall toward -9999.
        let vals: Vec<f32> = bin
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        assert!(
            vals.iter().all(|&v| v >= NO_ECHO_FLOOR_DBZ - 1e-3),
            "no extreme sentinel; min = {:?}",
            vals.iter().cloned().fold(f32::INFINITY, f32::min)
        );
        // No `noData` is declared, on the primitive or the schema.
        assert!(
            j["meshes"][0]["primitives"][0]["extensions"]["EXT_primitive_voxels"]
                .get("noData")
                .is_none()
        );
        assert!(
            j["extensions"]["EXT_structural_metadata"]["schema"]["classes"]["voxel"]["properties"]
                ["DBZH"]
                .get("noData")
                .is_none()
        );

        // Every cell unmeasured → nothing real to render → Empty.
        let mut empty = grid(2, 2, 2);
        empty.values.iter_mut().for_each(|v| *v = f32::NAN);
        assert!(matches!(
            encode_voxels_glb(&empty),
            Err(Tiles3dError::Empty)
        ));
    }

    #[test]
    fn voxel_tileset_has_cylinder_bv_content_voxels_and_transform() {
        let ts = tileset_json_voxels(
            24.5,
            60.5,
            100.0,
            250_000.0,
            15_000.0,
            [128, 360, 48],
            "DBZH",
            0.0,
            70.0,
            "content/{level}/{x}/{y}/{z}.glb",
            "subtrees/{level}/{x}/{y}/{z}.json",
        )
        .expect("tileset");
        let v: serde_json::Value = serde_json::from_str(&ts).unwrap();
        let bv = &v["root"]["boundingVolume"]["extensions"]["3DTILES_bounding_volume_cylinder"];
        assert_eq!(bv["maxRadius"], 250_000.0);
        assert_eq!(bv["height"], 15_000.0);
        assert_eq!(bv["translation"][2], 7_500.0); // lifted so data sits 0..height
        let cv = &v["root"]["content"]["extensions"]["3DTILES_content_voxels"];
        assert_eq!(cv["dimensions"], json!([128, 360, 48])); // content order
        assert_eq!(cv["class"], "voxel");
        assert_eq!(v["root"]["transform"].as_array().unwrap().len(), 16);
        assert_eq!(v["root"]["implicitTiling"]["subdivisionScheme"], "OCTREE");
        // Statistics drive the transfer-function range.
        assert_eq!(
            v["statistics"]["classes"]["voxel"]["properties"]["DBZH"]["max"],
            json!(70.0) // bare SCALAR per EXT_structural_metadata, not an array
        );
    }

    #[test]
    fn voxel_tileset_rejects_unsafe_uris() {
        let call = |content: &str, subtree: &str| {
            tileset_json_voxels(
                24.5,
                60.5,
                100.0,
                250_000.0,
                15_000.0,
                [128, 360, 48],
                "DBZH",
                0.0,
                70.0,
                content,
                subtree,
            )
        };
        let safe = "content/{level}/{x}/{y}/{z}.glb";
        let safe_sub = "subtrees/{level}/{x}/{y}/{z}.json";
        // A bad value in either URI slot is rejected.
        for bad in ["", "/abs/path.glb", "http://evil/x.glb", "../escape.glb"] {
            assert!(
                matches!(call(bad, safe_sub), Err(Tiles3dError::InvalidUri(_))),
                "content_uri {bad:?} should be rejected"
            );
            assert!(
                matches!(call(safe, bad), Err(Tiles3dError::InvalidUri(_))),
                "subtree_uri {bad:?} should be rejected"
            );
        }
        assert!(call(safe, safe_sub).is_ok());
    }
}
