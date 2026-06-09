//! Echo-Top-Height (ETH) draped-surface meshing of a
//! [`ds_core::volume::VoxelGrid`] into glTF 2.0 `.glb` content (#362).
//!
//! Unlike the isosurface ([`crate::isosurface`]), which extracts a closed shell
//! at one reflectivity value, the echo-top product collapses the volume to a
//! **2-D height field** — for each `(radius, azimuth)` ground column, the
//! highest altitude where reflectivity ≥ a threshold — and drapes it as a single
//! continuous surface, **per-vertex coloured by that height**. It answers "how
//! tall is each storm", the classic forecaster echo-top view, in 3-D.
//!
//! Two encoders off the same per-column echo-top height:
//! - [`encode_echo_top_columns_glb`] — **extruded bins**: one solid box per echo
//!   cell from the ground up to its echo top (walls, flat normals, coloured
//!   ground→top). The preferred 3-D look — grounded, no open edges, the blocky
//!   bins read as a bar field of storm depth.
//! - [`encode_echo_top_glb`] — the thin **draped surface**: a regular grid of
//!   cell-centre columns triangulated into quads (a quad only where all four
//!   corners have a top, so clear air is a hole), smooth normals. The flat 2-D
//!   height-field product — cheaper, but floats at echo-top height with no sides.
//!
//! Both reuse the isosurface's cell-index → ground/azimuth/height → ECEF →
//! glTF-Y-up mapping ([`crate::isosurface::index_to_gltf_pos`]) and pair with
//! [`crate::tileset_json_glb`] (antenna-ECEF tile transform).

use crate::isosurface::index_to_gltf_pos;
use crate::Tiles3dError;
use ds_core::geo::geodetic_to_ecef;
use ds_core::volume::VoxelGrid;
use ds_render::ColorMap;
use serde_json::json;

/// glTF component types / mode used here.
const COMPONENT_FLOAT: u32 = 5126; // FLOAT
const COMPONENT_U32: u32 = 5125; // UNSIGNED_INT (indices)
const COMPONENT_U8: u32 = 5121; // UNSIGNED_BYTE (normalized vertex colour)
const MODE_TRIANGLES: u32 = 4;
const TARGET_ARRAY_BUFFER: u32 = 34962;
const TARGET_ELEMENT_ARRAY_BUFFER: u32 = 34963;

/// Cap on emitted triangles — the surface is at most `(n_r-1)·(n_a-1)·2`
/// (≈90k at the default grid), so this is a generous backstop.
const MAX_TRIANGLES: usize = 4_000_000;

/// The fractional height *index* of the echo top in column `(i_r, i_a)`, or
/// `None` if the column has no cell `>= threshold`. The highest cell at/above
/// the threshold defines the top; if the cell *above* it is finite and below
/// the threshold, the crossing is interpolated for a smooth surface (otherwise
/// — top-of-grid or unmeasured above — the cell centre is used).
fn echo_top_index(grid: &VoxelGrid, threshold: f64, i_r: usize, i_a: usize) -> Option<f64> {
    let n_h = grid.dims[2];
    // Highest finite cell at/above the threshold.
    let mut top = None;
    for i_h in (0..n_h).rev() {
        let v = grid.values[grid.index(i_r, i_a, i_h)] as f64;
        if v.is_finite() && v >= threshold {
            top = Some(i_h);
            break;
        }
    }
    let top = top?;
    // Interpolate the threshold crossing toward the cell above, when that cell
    // is finite and below the threshold.
    if top + 1 < n_h {
        let v_top = grid.values[grid.index(i_r, i_a, top)] as f64;
        let v_above = grid.values[grid.index(i_r, i_a, top + 1)] as f64;
        if v_above.is_finite() && v_above < threshold && v_top > v_above {
            let t = ((threshold - v_top) / (v_above - v_top)).clamp(0.0, 1.0);
            return Some(top as f64 + t);
        }
    }
    Some(top as f64)
}

/// Physical height (metres above the origin) of fractional height index `fh`,
/// using the grid's cell-centre convention (matching the engine sampler).
fn height_at(grid: &VoxelGrid, fh: f64) -> f64 {
    let n_h = grid.dims[2] as f64;
    grid.height_range[0] + (fh + 0.5) * (grid.height_range[1] - grid.height_range[0]) / n_h
}

/// Encode a [`VoxelGrid`] as an **echo-top-height draped surface**: a glTF 2.0
/// `.glb` triangle mesh at the per-column echo-top height (highest cell with
/// reflectivity ≥ `threshold`), with each vertex coloured by `height_colormap`
/// applied to its **height in metres** (e.g. a blue→red ramp over 0–15 km).
///
/// Returns [`Tiles3dError::Empty`] when no column reaches the threshold (caller
/// → 404), [`Tiles3dError::NonFinite`] for a non-finite `threshold`/origin, and
/// [`Tiles3dError::TooLarge`] past [`MAX_TRIANGLES`]. Pair with
/// [`crate::tileset_json_glb`] (the tile transform = the antenna ECEF).
pub fn encode_echo_top_glb(
    grid: &VoxelGrid,
    threshold: f64,
    height_colormap: &dyn ColorMap,
) -> Result<Vec<u8>, Tiles3dError> {
    if !threshold.is_finite() {
        return Err(Tiles3dError::NonFinite("threshold"));
    }
    let rtc = geodetic_to_ecef(grid.origin_lon, grid.origin_lat, grid.origin_height);
    if rtc.iter().any(|c| !c.is_finite()) {
        return Err(Tiles3dError::NonFinite("rtc_center"));
    }
    let [n_r, n_a, _n_h] = grid.dims;
    // Geocentric up at the antenna ≈ local up — used to orient the surface
    // normals upward (the draped sheet faces the sky).
    let up = {
        let m = (rtc[0] * rtc[0] + rtc[1] * rtc[1] + rtc[2] * rtc[2]).sqrt();
        [
            (rtc[0] / m) as f32,
            (rtc[1] / m) as f32,
            (rtc[2] / m) as f32,
        ]
    };

    // Pass 1: one vertex per column that has an echo top.
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut colors: Vec<[u8; 4]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new(); // accumulated, normalized at the end
    let mut vidx = vec![u32::MAX; n_r * n_a]; // column → vertex index (MAX = none)
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for i_r in 0..n_r {
        for i_a in 0..n_a {
            let Some(fh) = echo_top_index(grid, threshold, i_r, i_a) else {
                continue;
            };
            let pos = index_to_gltf_pos(grid, rtc, i_r as f64, i_a as f64, fh);
            let color = height_colormap.color(Some(height_at(grid, fh)));
            vidx[i_r * n_a + i_a] = positions.len() as u32;
            for c in 0..3 {
                min[c] = min[c].min(pos[c]);
                max[c] = max[c].max(pos[c]);
            }
            positions.push(pos);
            colors.push(color);
            normals.push([0.0; 3]);
        }
    }
    if positions.is_empty() {
        return Err(Tiles3dError::Empty);
    }

    // Pass 2: triangulate quads whose four corner columns all have a vertex,
    // accumulating face normals into the corner vertices (smooth shading). The
    // azimuth seam (i_a = n_a−1 → 0) is not wrapped in v1 (one-column gap at
    // azimuth 0 — acceptable for a draped surface).
    let mut indices: Vec<u32> = Vec::new();
    let mut tris = 0usize;
    for i_r in 0..n_r.saturating_sub(1) {
        for i_a in 0..n_a.saturating_sub(1) {
            let a = vidx[i_r * n_a + i_a];
            let b = vidx[(i_r + 1) * n_a + i_a];
            let c = vidx[(i_r + 1) * n_a + (i_a + 1)];
            let d = vidx[i_r * n_a + (i_a + 1)];
            if a == u32::MAX || b == u32::MAX || c == u32::MAX || d == u32::MAX {
                continue;
            }
            for (i0, i1, i2) in [(a, b, c), (a, c, d)] {
                tris += 1;
                if tris > MAX_TRIANGLES {
                    return Err(Tiles3dError::TooLarge("echo-top triangles"));
                }
                indices.extend_from_slice(&[i0, i1, i2]);
                // Accumulate the face normal into its three vertices.
                let (p0, p1, p2) = (
                    positions[i0 as usize],
                    positions[i1 as usize],
                    positions[i2 as usize],
                );
                let u = [p1[0] - p0[0], p1[1] - p0[1], p1[2] - p0[2]];
                let v = [p2[0] - p0[0], p2[1] - p0[1], p2[2] - p0[2]];
                let fn_ = [
                    u[1] * v[2] - u[2] * v[1],
                    u[2] * v[0] - u[0] * v[2],
                    u[0] * v[1] - u[1] * v[0],
                ];
                for idx in [i0, i1, i2] {
                    let nrm = &mut normals[idx as usize];
                    nrm[0] += fn_[0];
                    nrm[1] += fn_[1];
                    nrm[2] += fn_[2];
                }
            }
        }
    }
    if indices.is_empty() {
        return Err(Tiles3dError::Empty);
    }

    // Normalize per-vertex normals and orient them upward (toward the sky). The
    // gltf-space up is the Z-up→Y-up flip of geocentric up: (x, z, −y).
    let up_gltf = [up[0], up[2], -up[1]];
    for nrm in &mut normals {
        let len = (nrm[0] * nrm[0] + nrm[1] * nrm[1] + nrm[2] * nrm[2]).sqrt();
        if len > 0.0 {
            *nrm = [nrm[0] / len, nrm[1] / len, nrm[2] / len];
            let dot = nrm[0] * up_gltf[0] + nrm[1] * up_gltf[1] + nrm[2] * up_gltf[2];
            if dot < 0.0 {
                *nrm = [-nrm[0], -nrm[1], -nrm[2]];
            }
        } else {
            *nrm = up_gltf; // degenerate — point up
        }
    }

    Ok(build_glb(&positions, &normals, &colors, &indices, min, max))
}

/// The 8 corners of a cell column, indexed by bits `r<<0 | a<<1 | top<<2`
/// (r/a = high edge?, top = at the echo top vs the ground).
const CORNER_BITS: [(bool, bool, bool); 8] = [
    (false, false, false), // 0: r_lo a_lo ground
    (true, false, false),  // 1: r_hi a_lo ground
    (false, true, false),  // 2: r_lo a_hi ground
    (true, true, false),   // 3: r_hi a_hi ground
    (false, false, true),  // 4: r_lo a_lo top
    (true, false, true),   // 5: r_hi a_lo top
    (false, true, true),   // 6: r_lo a_hi top
    (true, true, true),    // 7: r_hi a_hi top
];

/// The 6 faces of a column, each 4 corner indices wound around the quad. The
/// outward orientation is enforced at emit time (vs the box centre), so the
/// winding here only needs to form the quad.
const COLUMN_FACES: [[usize; 4]; 6] = [
    [4, 5, 7, 6], // top (at echo-top height)
    [0, 2, 3, 1], // bottom (on the ground)
    [0, 1, 5, 4], // wall a_lo
    [2, 6, 7, 3], // wall a_hi
    [0, 4, 6, 2], // wall r_lo
    [1, 3, 7, 5], // wall r_hi
];

/// Encode a [`VoxelGrid`] as **extruded echo-top columns**: one solid box per
/// `(radius, azimuth)` cell that has an echo, from the **ground** up to that
/// cell's echo-top height (highest cell ≥ `threshold`), vertices coloured by
/// height (`height_colormap`, metres) so each bar runs from the ground colour
/// up to its top colour. Unlike [`encode_echo_top_glb`] (a thin draped sheet),
/// the columns have side walls and sit on the ground — no open edges, nothing
/// floating. Flat-shaded (per-face normals) for crisp blocky bars.
///
/// Errors as [`encode_echo_top_glb`]. Pair with [`crate::tileset_json_glb`].
pub fn encode_echo_top_columns_glb(
    grid: &VoxelGrid,
    threshold: f64,
    height_colormap: &dyn ColorMap,
) -> Result<Vec<u8>, Tiles3dError> {
    if !threshold.is_finite() {
        return Err(Tiles3dError::NonFinite("threshold"));
    }
    let rtc = geodetic_to_ecef(grid.origin_lon, grid.origin_lat, grid.origin_height);
    if rtc.iter().any(|c| !c.is_finite()) {
        return Err(Tiles3dError::NonFinite("rtc_center"));
    }
    let [n_r, n_a, _n_h] = grid.dims;

    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut colors: Vec<[u8; 4]> = Vec::new();
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    let mut tris = 0usize;

    for i_r in 0..n_r {
        for i_a in 0..n_a {
            let Some(fh_top) = echo_top_index(grid, threshold, i_r, i_a) else {
                continue;
            };
            // The 8 corners (position + the colour for their height). The cell
            // spans index ±0.5 around its centre; the ground is `fh = -0.5`
            // (height 0 above the antenna ≈ the surface).
            let mut corner_pos = [[0.0f32; 3]; 8];
            let mut corner_col = [[0u8; 4]; 8];
            let mut center = [0.0f32; 3];
            for (c, &(rh, ah, top)) in CORNER_BITS.iter().enumerate() {
                let fr = i_r as f64 + if rh { 0.5 } else { -0.5 };
                let fa = i_a as f64 + if ah { 0.5 } else { -0.5 };
                let fh = if top { fh_top } else { -0.5 };
                let p = index_to_gltf_pos(grid, rtc, fr, fa, fh);
                let h = if top { height_at(grid, fh_top) } else { 0.0 };
                corner_pos[c] = p;
                corner_col[c] = height_colormap.color(Some(h));
                for k in 0..3 {
                    center[k] += p[k] / 8.0;
                }
            }
            // Emit the 6 faces, each as 2 outward-wound flat-shaded triangles.
            for face in COLUMN_FACES {
                let [q0, q1, q2, q3] = face;
                let fc = [
                    (corner_pos[q0][0] + corner_pos[q1][0] + corner_pos[q2][0] + corner_pos[q3][0])
                        / 4.0,
                    (corner_pos[q0][1] + corner_pos[q1][1] + corner_pos[q2][1] + corner_pos[q3][1])
                        / 4.0,
                    (corner_pos[q0][2] + corner_pos[q1][2] + corner_pos[q2][2] + corner_pos[q3][2])
                        / 4.0,
                ];
                // Outward direction = face centre − box centre.
                let out = [fc[0] - center[0], fc[1] - center[1], fc[2] - center[2]];
                for &(a, b, cc) in &[(q0, q1, q2), (q0, q2, q3)] {
                    tris += 1;
                    if tris > MAX_TRIANGLES {
                        return Err(Tiles3dError::TooLarge("echo-top column triangles"));
                    }
                    let (p0, p1, p2) = (corner_pos[a], corner_pos[b], corner_pos[cc]);
                    let u = [p1[0] - p0[0], p1[1] - p0[1], p1[2] - p0[2]];
                    let v = [p2[0] - p0[0], p2[1] - p0[1], p2[2] - p0[2]];
                    let mut nrm = [
                        u[1] * v[2] - u[2] * v[1],
                        u[2] * v[0] - u[0] * v[2],
                        u[0] * v[1] - u[1] * v[0],
                    ];
                    let len = (nrm[0] * nrm[0] + nrm[1] * nrm[1] + nrm[2] * nrm[2]).sqrt();
                    if len > 0.0 {
                        nrm = [nrm[0] / len, nrm[1] / len, nrm[2] / len];
                        if nrm[0] * out[0] + nrm[1] * out[1] + nrm[2] * out[2] < 0.0 {
                            nrm = [-nrm[0], -nrm[1], -nrm[2]];
                        }
                    }
                    for ci in [a, b, cc] {
                        let p = corner_pos[ci];
                        positions.push(p);
                        normals.push(nrm);
                        colors.push(corner_col[ci]);
                        for k in 0..3 {
                            min[k] = min[k].min(p[k]);
                            max[k] = max[k].max(p[k]);
                        }
                    }
                }
            }
        }
    }
    if positions.is_empty() {
        return Err(Tiles3dError::Empty);
    }
    // Non-indexed (flat shading needs per-face vertices), so indices are trivial.
    let indices: Vec<u32> = (0..positions.len() as u32).collect();
    Ok(build_glb(&positions, &normals, &colors, &indices, min, max))
}

/// Assemble an indexed `.glb` with POSITION + NORMAL + COLOR_0 (per-vertex
/// height colour) + u32 indices.
fn build_glb(
    positions: &[[f32; 3]],
    normals: &[[f32; 3]],
    colors: &[[u8; 4]],
    indices: &[u32],
    min: [f32; 3],
    max: [f32; 3],
) -> Vec<u8> {
    let vcount = positions.len();
    let icount = indices.len();
    // BIN layout (each section 4-aligned): POSITION f32×3, NORMAL f32×3,
    // COLOR_0 u8×4 (normalized), INDICES u32.
    let pos_len = vcount * 12;
    let nrm_off = pos_len;
    let nrm_len = vcount * 12;
    let col_off = nrm_off + nrm_len;
    let col_len = vcount * 4;
    let idx_off = col_off + col_len; // col_len is vcount*4 → 4-aligned
    let idx_len = icount * 4;

    let mut bin = Vec::with_capacity(idx_off + idx_len);
    for p in positions {
        for f in p {
            bin.extend_from_slice(&f.to_le_bytes());
        }
    }
    for n in normals {
        for f in n {
            bin.extend_from_slice(&f.to_le_bytes());
        }
    }
    for c in colors {
        bin.extend_from_slice(c);
    }
    for i in indices {
        bin.extend_from_slice(&i.to_le_bytes());
    }
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }

    let gltf = json!({
        "asset": { "version": "2.0", "generator": "MeteoCore ds-3dtiles echo-top" },
        "scene": 0,
        "scenes": [ { "nodes": [0] } ],
        "nodes": [ { "mesh": 0 } ],
        "meshes": [ {
            "primitives": [ {
                "attributes": { "POSITION": 0, "NORMAL": 1, "COLOR_0": 2 },
                "indices": 3,
                "material": 0,
                "mode": MODE_TRIANGLES,
            } ]
        } ],
        "materials": [ {
            // White base × per-vertex COLOR_0 = the height colour; lit so the
            // dome/peak shape reads. doubleSided for grazing/underside views.
            "pbrMetallicRoughness": {
                "baseColorFactor": [1.0, 1.0, 1.0, 1.0],
                "metallicFactor": 0.0,
                "roughnessFactor": 1.0,
            },
            "doubleSided": true,
        } ],
        "accessors": [
            { "bufferView": 0, "componentType": COMPONENT_FLOAT, "count": vcount, "type": "VEC3", "min": min, "max": max },
            { "bufferView": 1, "componentType": COMPONENT_FLOAT, "count": vcount, "type": "VEC3" },
            { "bufferView": 2, "componentType": COMPONENT_U8, "normalized": true, "count": vcount, "type": "VEC4" },
            { "bufferView": 3, "componentType": COMPONENT_U32, "count": icount, "type": "SCALAR" },
        ],
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": pos_len, "target": TARGET_ARRAY_BUFFER },
            { "buffer": 0, "byteOffset": nrm_off, "byteLength": nrm_len, "target": TARGET_ARRAY_BUFFER },
            { "buffer": 0, "byteOffset": col_off, "byteLength": col_len, "target": TARGET_ARRAY_BUFFER },
            { "buffer": 0, "byteOffset": idx_off, "byteLength": idx_len, "target": TARGET_ELEMENT_ARRAY_BUFFER },
        ],
        "buffers": [ { "byteLength": bin.len() } ],
    });

    let mut json_chunk = serde_json::to_vec(&gltf).expect("glTF JSON serializes");
    while !json_chunk.len().is_multiple_of(4) {
        json_chunk.push(b' ');
    }
    let total = 12 + 8 + json_chunk.len() + 8 + bin.len();
    let mut glb = Vec::with_capacity(total);
    glb.extend_from_slice(&0x4654_6C67u32.to_le_bytes()); // "glTF"
    glb.extend_from_slice(&2u32.to_le_bytes());
    glb.extend_from_slice(&(total as u32).to_le_bytes());
    glb.extend_from_slice(&(json_chunk.len() as u32).to_le_bytes());
    glb.extend_from_slice(&0x4E4F_534Au32.to_le_bytes()); // "JSON"
    glb.extend_from_slice(&json_chunk);
    glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    glb.extend_from_slice(&0x004E_4942u32.to_le_bytes()); // "BIN\0"
    glb.extend_from_slice(&bin);
    glb
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_render::{ColorStop, LutColorMap};

    /// A grid where reflectivity is 40 dBZ below a per-column "top" height and
    /// NaN above — so the echo-top height is a known, controllable surface.
    /// `top_index(i_r)` rises with radius, giving a sloped surface.
    fn ramp_top_grid(n_r: usize, n_a: usize, n_h: usize) -> VoxelGrid {
        let dims = [n_r, n_a, n_h];
        let mut values = vec![f32::NAN; n_r * n_a * n_h];
        for i_r in 0..n_r {
            // Echo fills heights 0..=top; clear air (well below threshold) just
            // above so the crossing interpolates; NaN higher still.
            let top = (i_r % n_h).min(n_h - 2);
            for i_a in 0..n_a {
                for i_h in 0..=top {
                    values[VoxelGrid::index_of(dims, i_r, i_a, i_h)] = 40.0;
                }
                values[VoxelGrid::index_of(dims, i_r, i_a, top + 1)] = -10.0; // below threshold
            }
        }
        VoxelGrid {
            origin_lon: 24.5,
            origin_lat: 60.5,
            origin_height: 100.0,
            dims,
            radius_range: [0.0, 100_000.0],
            angle_range: [0.0, std::f64::consts::TAU],
            height_range: [0.0, 12_000.0],
            values,
            quantity: "DBZH".into(),
            unit: "dBZ".into(),
        }
    }

    /// A height ramp with stops AT height values (blue → red over 0–12 km). A
    /// *builtin* colormap's stops are in its own units (e.g. Temperature's °C),
    /// so it collapses when stretched over a height range — build it explicitly.
    fn height_map() -> LutColorMap {
        let stops = [
            (0.0_f64, [40u8, 70, 200, 255]),
            (6_000.0, [40, 200, 80, 255]),
            (12_000.0, [220, 40, 40, 255]),
        ]
        .map(|(value, color)| ColorStop { value, color });
        LutColorMap::from_stops(&stops, 0.0, 12_000.0)
    }

    fn parse_glb(glb: &[u8]) -> serde_json::Value {
        assert_eq!(&glb[0..4], &0x4654_6C67u32.to_le_bytes(), "glTF magic");
        assert_eq!(
            u32::from_le_bytes(glb[8..12].try_into().unwrap()) as usize,
            glb.len(),
            "header length"
        );
        let json_len = u32::from_le_bytes(glb[12..16].try_into().unwrap()) as usize;
        serde_json::from_slice(&glb[20..20 + json_len]).expect("glTF JSON parses")
    }

    #[test]
    fn echo_top_glb_is_wellformed() {
        let grid = ramp_top_grid(6, 12, 8);
        let glb = encode_echo_top_glb(&grid, 20.0, &height_map()).expect("encode");
        let json = parse_glb(&glb);
        assert_eq!(json["asset"]["version"], "2.0");
        let prim = &json["meshes"][0]["primitives"][0];
        assert_eq!(prim["mode"], MODE_TRIANGLES);
        assert!(
            prim["attributes"]["COLOR_0"].is_number(),
            "has vertex colours"
        );
        assert!(prim["indices"].is_number(), "indexed mesh");
        let vcount = json["accessors"][0]["count"].as_u64().unwrap();
        assert!(vcount > 0, "has vertices");
        // COLOR_0 accessor is normalized u8 VEC4.
        assert_eq!(json["accessors"][2]["componentType"], COMPONENT_U8);
        assert_eq!(json["accessors"][2]["normalized"], true);
        assert_eq!(json["accessors"][2]["type"], "VEC4");
        // POSITION has min/max.
        assert_eq!(json["accessors"][0]["min"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn echo_top_height_tracks_the_fill() {
        // Flat-top grid: echo fills 0..=3, clear air at 4, so the echo top is the
        // crossing between index 3 and 4 → a single flat sheet. Use a small disc
        // so geocentric-up ≈ local up; check every vertex is at the same height.
        let dims = [4, 16, 8];
        let mut values = vec![f32::NAN; 4 * 16 * 8];
        for i_r in 0..4 {
            for i_a in 0..16 {
                for i_h in 0..=3 {
                    values[VoxelGrid::index_of(dims, i_r, i_a, i_h)] = 40.0;
                }
                values[VoxelGrid::index_of(dims, i_r, i_a, 4)] = 0.0; // below 20
            }
        }
        let grid = VoxelGrid {
            origin_lon: 24.5,
            origin_lat: 60.5,
            origin_height: 100.0,
            dims,
            radius_range: [0.0, 5_000.0],
            angle_range: [0.0, std::f64::consts::TAU],
            height_range: [0.0, 8_000.0],
            values,
            quantity: "DBZH".into(),
            unit: "dBZ".into(),
        };
        // Crossing: v_top(=40) at idx 3, v_above(=0) at idx 4, threshold 20 →
        // t = (20-40)/(0-40) = 0.5 → fh = 3.5 → height = (3.5+0.5)*8000/8 = 4000 m.
        let fh = echo_top_index(&grid, 20.0, 0, 0).unwrap();
        assert!((fh - 3.5).abs() < 1e-9, "fractional top index {fh}");
        assert!((height_at(&grid, fh) - 4000.0).abs() < 1e-9);

        // Reconstruct heights from the encoded positions: all ≈ 4000 m.
        let glb = encode_echo_top_glb(&grid, 20.0, &height_map()).unwrap();
        let json = parse_glb(&glb);
        let rtc = geodetic_to_ecef(grid.origin_lon, grid.origin_lat, grid.origin_height);
        let m = (rtc[0] * rtc[0] + rtc[1] * rtc[1] + rtc[2] * rtc[2]).sqrt();
        let up = [rtc[0] / m, rtc[1] / m, rtc[2] / m];
        let pos_len = json["bufferViews"][0]["byteLength"].as_u64().unwrap() as usize;
        let bin_start = {
            let json_len = u32::from_le_bytes(glb[12..16].try_into().unwrap()) as usize;
            20 + json_len + 8
        };
        let floats: Vec<f32> = glb[bin_start..bin_start + pos_len]
            .chunks_exact(4)
            .map(|w| f32::from_le_bytes(w.try_into().unwrap()))
            .collect();
        for v in floats.chunks_exact(3) {
            // Invert the Y-up flip: glTF (x,y,z) → ECEF offset (x,−z,y).
            let off = [v[0] as f64, -(v[2] as f64), v[1] as f64];
            let h = off[0] * up[0] + off[1] * up[1] + off[2] * up[2];
            assert!((h - 4000.0).abs() < 30.0, "vertex height {h} ≉ 4000 m");
        }
    }

    #[test]
    fn echo_top_columns_glb_is_wellformed_and_grounded() {
        let grid = ramp_top_grid(6, 12, 8);
        let glb = encode_echo_top_columns_glb(&grid, 20.0, &height_map()).expect("encode");
        let json = parse_glb(&glb);
        let prim = &json["meshes"][0]["primitives"][0];
        assert!(prim["attributes"]["COLOR_0"].is_number());
        let vcount = json["accessors"][0]["count"].as_u64().unwrap();
        // Per-face vertices (36 per box) → a multiple of 3, non-empty.
        assert!(
            vcount > 0 && vcount.is_multiple_of(3),
            "triangle-list vertices: {vcount}"
        );

        // Every column reaches the ground: reconstruct height-above-antenna and
        // assert the minimum is ≈ 0 (the bottom face), not floating.
        let rtc = geodetic_to_ecef(grid.origin_lon, grid.origin_lat, grid.origin_height);
        let m = (rtc[0] * rtc[0] + rtc[1] * rtc[1] + rtc[2] * rtc[2]).sqrt();
        let up = [rtc[0] / m, rtc[1] / m, rtc[2] / m];
        let pos_len = json["bufferViews"][0]["byteLength"].as_u64().unwrap() as usize;
        let bin_start = {
            let json_len = u32::from_le_bytes(glb[12..16].try_into().unwrap()) as usize;
            20 + json_len + 8
        };
        let mut min_h = f64::INFINITY;
        for w in glb[bin_start..bin_start + pos_len].chunks_exact(12) {
            let v = [
                f32::from_le_bytes(w[0..4].try_into().unwrap()),
                f32::from_le_bytes(w[4..8].try_into().unwrap()),
                f32::from_le_bytes(w[8..12].try_into().unwrap()),
            ];
            // small-disc grid (ramp_top_grid radius 100 km) — curvature keeps it
            // modest; the ground vertices land near 0.
            let off = [v[0] as f64, -(v[2] as f64), v[1] as f64];
            min_h = min_h.min(off[0] * up[0] + off[1] * up[1] + off[2] * up[2]);
        }
        assert!(
            min_h.abs() < 1500.0,
            "columns reach the ground, min height {min_h}"
        );
    }

    #[test]
    fn empty_when_no_column_reaches_threshold() {
        let grid = ramp_top_grid(6, 12, 8); // echo is 40 dBZ
        assert!(matches!(
            encode_echo_top_glb(&grid, 60.0, &height_map()),
            Err(Tiles3dError::Empty)
        ));
    }

    #[test]
    fn non_finite_threshold_is_rejected() {
        let grid = ramp_top_grid(4, 8, 6);
        assert!(matches!(
            encode_echo_top_glb(&grid, f64::NAN, &height_map()),
            Err(Tiles3dError::NonFinite("threshold"))
        ));
    }
}
