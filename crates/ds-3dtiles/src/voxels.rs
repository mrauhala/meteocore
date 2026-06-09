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
use ds_core::volume::VoxelGrid;
use serde_json::json;

use crate::Tiles3dError;

/// glTF primitive `mode` for a **cylinder** voxel grid (`0x80000002`). Box is
/// `0x80000000`, ellipsoid `0x80000001`; the high bit marks voxels, the low bits
/// the shape. Read from the CesiumJS fixtures (not the draft README).
const VOXEL_MODE_CYLINDER: u64 = 2_147_483_650;

/// Finite sentinel written for `NaN`/nodata cells (the cone of silence / clear
/// air the grid leaves unmeasured). Declared as the attribute's `noData`, so
/// CesiumJS treats those cells as empty. Well outside any physical dBZ.
const NODATA_SENTINEL: f32 = -9999.0;

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

    // Transpose our `[radius, angle, height]` grid (height-fastest) into the glТF
    // cylinder layout `[radius, height, angle]` (radius-fastest → height →
    // angle-slowest). Track the finite value range for the transfer function;
    // NaN/nodata → the sentinel.
    let mut bin = Vec::with_capacity(count * 4);
    let mut vmin = f32::INFINITY;
    let mut vmax = f32::NEG_INFINITY;
    for i_a in 0..n_a {
        for i_h in 0..n_h {
            for i_r in 0..n_r {
                let v = grid.values[grid.index(i_r, i_a, i_h)];
                let f = if v.is_finite() {
                    vmin = vmin.min(v);
                    vmax = vmax.max(v);
                    v
                } else {
                    NODATA_SENTINEL
                };
                bin.extend_from_slice(&f.to_le_bytes());
            }
        }
    }
    if !vmin.is_finite() {
        return Err(Tiles3dError::Empty); // every cell nodata
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
                    "EXT_primitive_voxels": {
                        "dimensions": [n_r, n_h, n_a],
                        "noData": { "_VOXEL": [NODATA_SENTINEL] }
                    }
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

    assemble_glb(&gltf, bin)
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
                        "componentType": "FLOAT32",
                        "noData": NODATA_SENTINEL
                    }
                }
            }
        }
    })
}

/// Assemble a binary glTF (`.glb`) from a glТF JSON value + a BIN buffer. Both
/// chunks are padded to a 4-byte boundary (JSON with spaces, BIN with zeros) per
/// the glTF 2.0 spec.
fn assemble_glb(gltf: &serde_json::Value, mut bin: Vec<u8>) -> Result<Vec<u8>, Tiles3dError> {
    let mut json_bytes = serde_json::to_vec(gltf).expect("voxel glTF serializes");
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(b' ');
    }
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let total = 12 + 8 + json_bytes.len() + 8 + bin.len();
    let total = u32::try_from(total).map_err(|_| Tiles3dError::TooLarge("glb byteLength"))?;

    let mut glb = Vec::with_capacity(total as usize);
    glb.extend_from_slice(b"glTF");
    glb.extend_from_slice(&2u32.to_le_bytes()); // version
    glb.extend_from_slice(&total.to_le_bytes());
    glb.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"JSON");
    glb.extend_from_slice(&json_bytes);
    glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"BIN\0");
    glb.extend_from_slice(&bin);
    Ok(glb)
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
                        quantity: { "min": [stat_min], "max": [stat_max] }
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
    Ok(serde_json::to_string_pretty(&tileset).expect("voxel tileset serializes"))
}

/// The implicit-tiling **subtree** for a single available tile: the one level-0
/// tile and its content are present, with no child subtrees. A JSON subtree (3D
/// Tiles 1.1) with constant availability — matches the CesiumJS fixture.
pub fn voxel_subtree_json() -> String {
    let subtree = json!({
        "tileAvailability": { "constant": 1, "availableCount": 1 },
        "contentAvailability": { "constant": 1, "availableCount": 1 },
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

        // Accessor: SCALAR FLOAT, one element per cell, range = [0, 2] (height idx).
        let acc = &j["accessors"][0];
        assert_eq!(acc["componentType"], 5126);
        assert_eq!(acc["count"], 24);
        assert_eq!(acc["min"], json!([0.0]));
        assert_eq!(acc["max"], json!([2.0]));
        assert_eq!(bin.len(), 24 * 4);

        // Data layout: glТF order is radius-fastest → height → angle-slowest, and
        // value == height index. So the first `n_r` floats (radius run at h=0)
        // are 0.0, the next `n_r` (h=1) are 1.0, then h=2 → 2.0; that block of
        // n_r*n_h repeats per angle slice.
        let f = |i: usize| f32::from_le_bytes(bin[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!([f(0), f(1)], [0.0, 0.0]); // radius run, height 0
        assert_eq!([f(2), f(3)], [1.0, 1.0]); // radius run, height 1
        assert_eq!([f(4), f(5)], [2.0, 2.0]); // radius run, height 2
        assert_eq!(f(6), 0.0); // next angle slice restarts at height 0
    }

    #[test]
    fn nodata_becomes_sentinel_and_all_nodata_is_empty() {
        let mut g = grid(2, 2, 2);
        let idx = g.index(0, 0, 0);
        g.values[idx] = f32::NAN;
        let glb = encode_voxels_glb(&g).expect("encode");
        let (_, bin) = parse_glb(&glb);
        let f = |i: usize| f32::from_le_bytes(bin[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!(f(0), NODATA_SENTINEL, "NaN cell → sentinel");

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
            json!([70.0])
        );
    }
}
