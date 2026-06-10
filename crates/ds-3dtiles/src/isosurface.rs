//! Isosurface meshing of a [`ds_core::volume::VoxelGrid`] into glTF 2.0 binary
//! (`.glb`) triangle-mesh content for OGC 3D Tiles (#357).
//!
//! Unlike the draft `EXT_primitive_voxels` voxel path (#351 — experimental, in
//! flux, CesiumJS ≥1.127 only), an isosurface is a **plain glTF triangle mesh**:
//! it renders in *any* 3D Tiles 1.1 client, so it is the verifiable way to show
//! the 3-D structure of a radar volume (storm shells / echo tops at a chosen
//! reflectivity threshold).
//!
//! ## Why marching *tetrahedra*, not marching cubes
//!
//! Marching cubes needs a hand-transcribed 256-case edge/triangle table where a
//! single wrong entry yields holes or non-manifold junk that's only visible when
//! rendered. Marching *tetrahedra* needs **no table**: a tetrahedron is the
//! complete graph K4, so *every* pair of corners is an edge, and the iso-surface
//! crosses exactly `|inside| · |outside|` edges (3 → one triangle, 4 → a quad =
//! two triangles). The topology is correct by construction — the right property
//! when the output can't be eyeballed at encode time. Each cube is split into 6
//! tetrahedra by the Kuhn/Freudenthal decomposition along its main diagonal.
//!
//! ## Geometry
//!
//! The grid is cylindrical (radar-native): `radius` = ground range, `angle` =
//! azimuth, `height` = metres above the antenna. A surface vertex lands at a
//! fractional cell index, which maps to physical (ground, azimuth, height) with
//! the **same cell-centre convention the engine sampled with** — sample `i`
//! along an `n`-cell axis spanning `[lo, hi]` sits at `lo + (i + 0.5)(hi−lo)/n` —
//! then to a geographic point via [`ds_core::geo::destination_point`] and to
//! ECEF via [`ds_core::geo::geodetic_to_ecef`]. Positions are stored relative to
//! the antenna ECEF (the tile `transform`) and pre-rotated Z-up→Y-up, because a
//! 3D Tiles runtime applies the inverse Y-up→Z-up to glTF content (this is the
//! flip the `.pnts` path deliberately avoids; here the content *is* glTF).
//!
//! ## Clear air vs unmeasured (`NaN`)
//!
//! A radar volume stores `NaN` for both "the radar saw nothing" (clear air) and
//! "the radar couldn't see here" (cone of silence / beyond range) — they are
//! indistinguishable in the grid. The `background` parameter of
//! [`encode_isosurface_glb`] decides how `NaN` corners behave: `None` skips any
//! tetrahedron touching one (open surface — never fabricates), while `Some(bg)`
//! treats them as the below-threshold value `bg`, **sealing** the surface into
//! solid blobs by assuming absence of echo where unmeasured. Sealing is the
//! sensible default for a reflectivity shell (otherwise echo→clear-air
//! boundaries render as open vertical "curtains" instead of closed domes).

use crate::Tiles3dError;
use ds_core::geo::{destination_point, geodetic_to_ecef};
use ds_core::volume::VoxelGrid;
use serde_json::json;

/// Cap on emitted triangles. The mesh is non-indexed (3 vertices ×
/// `POSITION`+`NORMAL` = 72 bytes/triangle), so this bounds the encode buffer at
/// ~216 MB worst case. A radar reflectivity shell is far smaller; exceeding the
/// cap means the threshold is too low or the grid too fine — fail loudly rather
/// than allocate unbounded.
const MAX_TRIANGLES: usize = 3_000_000;

/// glTF component type `FLOAT` (5126) and primitive mode `TRIANGLES` (4).
const COMPONENT_FLOAT: u32 = 5126;
const MODE_TRIANGLES: u32 = 4;
/// glTF `bufferView.target` `ARRAY_BUFFER` (34962).
const TARGET_ARRAY_BUFFER: u32 = 34962;

/// Cube-corner offsets `(Δradius, Δangle, Δheight)`, indexed by the standard
/// corner numbering `bit0=Δr, bit1=Δa, bit2=Δh`.
const CORNER: [[usize; 3]; 8] = [
    [0, 0, 0], // 0
    [1, 0, 0], // 1
    [0, 1, 0], // 2
    [1, 1, 0], // 3
    [0, 0, 1], // 4
    [1, 0, 1], // 5
    [0, 1, 1], // 6
    [1, 1, 1], // 7
];

/// Kuhn/Freudenthal split of the cube into 6 tetrahedra, all sharing the main
/// diagonal 0–7 (one tet per axis-permutation path from corner 0 to corner 7).
/// They partition the cube exactly, so the surface is watertight across cube
/// boundaries.
const TETS: [[usize; 4]; 6] = [
    [0, 1, 3, 7],
    [0, 1, 5, 7],
    [0, 2, 3, 7],
    [0, 2, 6, 7],
    [0, 4, 5, 7],
    [0, 4, 6, 7],
];

/// Accumulates a non-indexed triangle mesh (one flat normal per face) plus the
/// `POSITION` accessor's `min`/`max` (required by the glTF spec).
struct MeshBuilder {
    positions: Vec<f32>, // xyz per vertex, 3 vertices per triangle
    normals: Vec<f32>,   // xyz per vertex
    triangles: usize,
    min: [f32; 3],
    max: [f32; 3],
}

impl MeshBuilder {
    fn new() -> Self {
        Self {
            positions: Vec::new(),
            normals: Vec::new(),
            triangles: 0,
            min: [f32::INFINITY; 3],
            max: [f32::NEG_INFINITY; 3],
        }
    }

    /// Append one triangle, with its face normal oriented outward. `out_ref` is
    /// a point on the OUTSIDE of the surface (the tet's outside-corner centroid);
    /// the outward direction is `out_ref − triangle_centroid`. The marching-tet
    /// cases don't emit a consistent winding on their own, so without this ~half
    /// the faces would have inward normals and render with inverted lighting;
    /// here the winding is flipped to match outward so the stored normal always
    /// points outward. Skips degenerate (zero-area) triangles so no `NaN` normal
    /// reaches the buffer. Errors with [`Tiles3dError::TooLarge`] past
    /// [`MAX_TRIANGLES`].
    fn push(
        &mut self,
        p0: [f32; 3],
        p1: [f32; 3],
        p2: [f32; 3],
        out_ref: [f32; 3],
    ) -> Result<(), Tiles3dError> {
        let u = [p1[0] - p0[0], p1[1] - p0[1], p1[2] - p0[2]];
        let v = [p2[0] - p0[0], p2[1] - p0[1], p2[2] - p0[2]];
        let n = [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ];
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        // Positive comparison (not `!(len > 0.0)`) keeps it NaN-safe — a
        // non-finite `len` falls to the `else` and is dropped — and clippy-clean.
        let mut nrm = if len > 0.0 {
            [n[0] / len, n[1] / len, n[2] / len]
        } else {
            return Ok(()); // degenerate sliver (zero area) — drop it
        };
        // Outward = from the triangle's own centroid toward the outside ref.
        let centroid = [
            (p0[0] + p1[0] + p2[0]) / 3.0,
            (p0[1] + p1[1] + p2[1]) / 3.0,
            (p0[2] + p1[2] + p2[2]) / 3.0,
        ];
        let outward = [
            out_ref[0] - centroid[0],
            out_ref[1] - centroid[1],
            out_ref[2] - centroid[2],
        ];
        // Orient outward: if the geometric normal points the wrong way, flip the
        // normal AND the winding (swap p1/p2) so both stay consistent.
        let dot = nrm[0] * outward[0] + nrm[1] * outward[1] + nrm[2] * outward[2];
        let (a, b, c) = if dot < 0.0 {
            nrm = [-nrm[0], -nrm[1], -nrm[2]];
            (p0, p2, p1)
        } else {
            (p0, p1, p2)
        };

        self.triangles += 1;
        if self.triangles > MAX_TRIANGLES {
            return Err(Tiles3dError::TooLarge("isosurface triangles"));
        }
        for p in [a, b, c] {
            for k in 0..3 {
                self.positions.push(p[k]);
                self.normals.push(nrm[k]);
                if p[k] < self.min[k] {
                    self.min[k] = p[k];
                }
                if p[k] > self.max[k] {
                    self.max[k] = p[k];
                }
            }
        }
        Ok(())
    }
}

/// Map a fractional cell index `(fr, fa, fh)` to a glTF-space position (metres),
/// relative to `rtc` (the antenna ECEF) and pre-rotated Z-up→Y-up.
pub(crate) fn index_to_gltf_pos(
    grid: &VoxelGrid,
    rtc: [f64; 3],
    fr: f64,
    fa: f64,
    fh: f64,
) -> [f32; 3] {
    let [n_r, n_a, n_h] = grid.dims;
    // Same cell-centre convention as the engine's sampler: sample i sits at
    // lo + (i + 0.5)(hi − lo)/n. Fractional i interpolates linearly between
    // cell centres.
    let ground = grid.radius_range[0]
        + (fr + 0.5) * (grid.radius_range[1] - grid.radius_range[0]) / n_r as f64;
    let azimuth =
        grid.angle_range[0] + (fa + 0.5) * (grid.angle_range[1] - grid.angle_range[0]) / n_a as f64;
    let height = grid.height_range[0]
        + (fh + 0.5) * (grid.height_range[1] - grid.height_range[0]) / n_h as f64;

    let (lon, lat) = destination_point(
        grid.origin_lon,
        grid.origin_lat,
        ground,
        azimuth.to_degrees(),
    );
    let e = geodetic_to_ecef(lon, lat, grid.origin_height + height);
    let dx = (e[0] - rtc[0]) as f32;
    let dy = (e[1] - rtc[1]) as f32;
    let dz = (e[2] - rtc[2]) as f32;
    // Z-up (ECEF offset) → glTF Y-up: a runtime re-applies Y-up→Z-up, so
    // (x, y, z)_zup must be stored as (x, z, −y). Net effect: world = rtc + ECEF
    // offset. (The `.pnts` path stores ECEF-native and skips this because pnts
    // is not glTF.)
    [dx, dz, -dy]
}

/// Interpolated surface vertex on the edge between cube corners `a` and `b`
/// (each `0..8`, indexing `cvals`/`cidx`). The interpolation is symmetric in
/// `a`/`b`, so callers need only name the edge, not which end is inside.
fn crossing(
    grid: &VoxelGrid,
    rtc: [f64; 3],
    threshold: f64,
    cvals: &[f64; 8],
    cidx: &[[f64; 3]; 8],
    a: usize,
    b: usize,
) -> [f32; 3] {
    let (va, vb) = (cvals[a], cvals[b]);
    // One end is inside (>= threshold), the other outside (< threshold), so
    // va != vb and t ∈ [0, 1] (t = 1 only when threshold == vb exactly, placing
    // the vertex on corner b — geometrically valid).
    let t = ((threshold - va) / (vb - va)).clamp(0.0, 1.0);
    let fr = cidx[a][0] + t * (cidx[b][0] - cidx[a][0]);
    let fa = cidx[a][1] + t * (cidx[b][1] - cidx[a][1]);
    let fh = cidx[a][2] + t * (cidx[b][2] - cidx[a][2]);
    index_to_gltf_pos(grid, rtc, fr, fa, fh)
}

/// Mesh one tetrahedron (`tet` = 4 cube-corner indices) at the current isovalue,
/// pushing 0–2 triangles into `mesh`. Skips the tet if any corner is `NaN`.
fn march_tet(
    mesh: &mut MeshBuilder,
    grid: &VoxelGrid,
    rtc: [f64; 3],
    threshold: f64,
    cvals: &[f64; 8],
    cidx: &[[f64; 3]; 8],
    tet: [usize; 4],
) -> Result<(), Tiles3dError> {
    // Any nodata corner ⇒ skip (don't fabricate surface across a gap).
    if tet.iter().any(|&c| !cvals[c].is_finite()) {
        return Ok(());
    }
    // Local tet indices (0..4) split into inside / outside. Fixed-size arrays +
    // length counters, not `Vec`s — `march_tet` runs millions of times per
    // encode (≈ cubes × 6), so a per-call heap allocation would dominate.
    let mut inside = [0usize; 4];
    let mut outside = [0usize; 4];
    let (mut ni, mut no) = (0usize, 0usize);
    for (k, &c) in tet.iter().enumerate() {
        if cvals[c] >= threshold {
            inside[ni] = k;
            ni += 1;
        } else {
            outside[no] = k;
            no += 1;
        }
    }
    if ni == 0 || no == 0 {
        return Ok(()); // tet fully inside or outside — no surface
    }
    let cross = |a_local: usize, b_local: usize| {
        crossing(
            grid,
            rtc,
            threshold,
            cvals,
            cidx,
            tet[a_local],
            tet[b_local],
        )
    };
    // The marching-tet cases don't produce a consistent winding, so give
    // `MeshBuilder::push` a reference point on the OUTSIDE of the surface — the
    // outside-corner centroid, in glTF space — and it orients each face's normal
    // to point from the triangle toward it (outward, toward lower values). Only
    // ONE glTF projection per tet (the inside-centroid round-trip the earlier
    // version did was redundant: push already has the triangle's own centroid).
    let oc = {
        let mut s = [0.0_f64; 3];
        for &k in &outside[..no] {
            let p = cidx[tet[k]];
            for d in 0..3 {
                s[d] += p[d];
            }
        }
        [s[0] / no as f64, s[1] / no as f64, s[2] / no as f64]
    };
    let out_ref = index_to_gltf_pos(grid, rtc, oc[0], oc[1], oc[2]);

    match (ni, no) {
        (1, 3) | (3, 1) => {
            // One odd corner vs three: the surface is the triangle on the three
            // edges from the odd corner to the others.
            let (odd, rest) = if ni == 1 {
                (inside[0], &outside)
            } else {
                (outside[0], &inside)
            };
            mesh.push(
                cross(odd, rest[0]),
                cross(odd, rest[1]),
                cross(odd, rest[2]),
                out_ref,
            )
        }
        (2, 2) => {
            // Two vs two: the surface is a quad on the four crossing edges.
            // Order its corners around the perimeter so the two triangles don't
            // self-intersect: (a–c, b–c, b–d, a–d) — consecutive edges share an
            // endpoint, forming a proper cycle. (We always fan from q0; picking
            // the shorter diagonal would give marginally better aspect ratios on
            // the mildly non-planar projected quad — a follow-up if vertex
            // sharing lands, negligible at radar grid scale.)
            let (a, b) = (inside[0], inside[1]);
            let (c, d) = (outside[0], outside[1]);
            let q0 = cross(a, c);
            let q1 = cross(b, c);
            let q2 = cross(b, d);
            let q3 = cross(a, d);
            mesh.push(q0, q1, q2, out_ref)?;
            mesh.push(q0, q2, q3, out_ref)
        }
        _ => unreachable!("a tetrahedron has exactly 4 corners"),
    }
}

/// Encode a [`VoxelGrid`] as a 3D Tiles **isosurface** at `threshold` (in the
/// grid's physical units, e.g. dBZ), as a glTF 2.0 binary (`.glb`) triangle
/// mesh shaded a single `color` (`[r, g, b, a]`, 0–255) — typically the
/// colormap colour at `threshold`, i.e. "the 20 dBZ shell".
///
/// The scalar field is lightly smoothed before marching (#381 — see
/// `crate::smoothing`), so the shell follows the echo, not the cell lattice;
/// expect extracted values to deviate from the raw grid by up to the
/// cell-to-cell contrast near sharp gradients and clamped grid edges.
///
/// ## `background` — sealing the surface against clear air
///
/// A radar volume stores **`NaN` for both** "the radar looked and saw nothing"
/// (clear air, *undetect*) and "the radar couldn't see here" (cone of silence /
/// beyond range, *nodata*) — the engine can't tell them apart in the grid. With
/// `background = None`, any tetrahedron touching a `NaN` corner is skipped, so
/// the surface **does not close** where echo meets clear air → open vertical
/// walls/blades, not solid blobs.
///
/// `background = Some(bg)` (with `bg < threshold`) treats every `NaN` corner as
/// the value `bg`, i.e. **assumes absence of echo** wherever the radar didn't
/// report one. The surface then seals into closed blobs. This never *invents*
/// echo (it assumes the conservative direction — no reflectivity); its only
/// cost is that a surface also caps at the true coverage boundary (the narrow
/// cone of silence above the antenna and the max-range cylinder), which is the
/// standard "no echo where unmeasured" convention every radar 3-D view uses.
/// For a reflectivity shell pass e.g. `Some(-32.0)` (the dBZ floor).
/// Distinguishing *undetect* from *nodata* in the engine sampler (so only the
/// real cone of silence stays open) is a follow-up.
///
/// Returns [`Tiles3dError::Empty`] when no cell straddles the threshold (an
/// empty surface — caller maps to 404), [`Tiles3dError::NonFinite`] for a
/// non-finite `threshold`/origin/`background`, and [`Tiles3dError::TooLarge`]
/// past [`MAX_TRIANGLES`]. Pair with [`tileset_json_glb`], which carries the
/// antenna ECEF as the tile `transform`.
pub fn encode_isosurface_glb(
    grid: &VoxelGrid,
    threshold: f64,
    color: [u8; 4],
    background: Option<f64>,
) -> Result<Vec<u8>, Tiles3dError> {
    if !threshold.is_finite() {
        return Err(Tiles3dError::NonFinite("threshold"));
    }
    // A `Some` background must be finite (else it acts like `None` — a NaN fill
    // leaves the corner non-finite → tet skipped) AND representable as `f32`
    // (the seal narrows `bg as f32`; an out-of-range f64 would cast to ±inf and
    // propagate through the dense blur) AND strictly below `threshold` (else
    // unmeasured cells would seal as *inside* the surface, inverting it).
    if let Some(bg) = background {
        if !bg.is_finite() || bg.abs() > f64::from(f32::MAX) {
            return Err(Tiles3dError::NonFinite("background"));
        }
        if bg >= threshold {
            return Err(Tiles3dError::BackgroundNotBelowThreshold {
                background: bg,
                threshold,
            });
        }
    }
    let rtc = geodetic_to_ecef(grid.origin_lon, grid.origin_lat, grid.origin_height);
    if rtc.iter().any(|c| !c.is_finite()) {
        return Err(Tiles3dError::NonFinite("rtc_center"));
    }
    let [n_r, n_a, n_h] = grid.dims;

    // Smooth the scalar field before marching (#381). Radar echo is cellular
    // (each cell a local reflectivity maximum), so a shell extracted from the
    // raw grid inherits the cell lattice as stair-steps along the contour —
    // worst at range, where 1° azimuth bins are km-wide. 2 passes (sigma ≈ 1
    // cell) round the contour without erasing storm-core structure (the voxel
    // ray-march uses 4 — see `crate::smoothing` — but a mesh shows its lattice
    // less than a volume seen from inside). `NaN` handling follows the sealing
    // decision: with a `background` the field is sealed FIRST and the dense
    // blur runs over finite values only; without one, the NaN-aware blur keeps
    // unmeasured cells `NaN` (still skipped below) and never bleeds them into
    // real echo — the open-boundary semantics (#360) survive the smoothing.
    const SMOOTH_PASSES: usize = 2;
    let field: Vec<f32> = match background {
        Some(bg) => {
            let sealed = grid
                .values
                .iter()
                .map(|&v| if v.is_finite() { v } else { bg as f32 })
                .collect();
            crate::smoothing::smooth_grid(sealed, grid.dims, SMOOTH_PASSES)
        }
        None => {
            crate::smoothing::smooth_grid_nan_aware(grid.values.clone(), grid.dims, SMOOTH_PASSES)
        }
    };

    let mut mesh = MeshBuilder::new();
    // Iterate cubes. The angular seam (i_a = n_a−1 → 0) is not wrapped in v1, so
    // a one-cell-wide gap remains at azimuth 0; acceptable for a visualisation
    // shell (a closed radar volume is rarely intersected exactly there).
    for i_r in 0..n_r.saturating_sub(1) {
        for i_a in 0..n_a.saturating_sub(1) {
            for i_h in 0..n_h.saturating_sub(1) {
                let mut cvals = [0.0_f64; 8];
                let mut cidx = [[0.0_f64; 3]; 8];
                for (c, off) in CORNER.iter().enumerate() {
                    let (ir, ia, ih) = (i_r + off[0], i_a + off[1], i_h + off[2]);
                    // Sealed + smoothed (or NaN-aware-smoothed) value; a
                    // remaining NaN means unmeasured with no seal → tet skipped.
                    cvals[c] = field[grid.index(ir, ia, ih)] as f64;
                    cidx[c] = [ir as f64, ia as f64, ih as f64];
                }
                for tet in TETS {
                    march_tet(&mut mesh, grid, rtc, threshold, &cvals, &cidx, tet)?;
                }
            }
        }
    }

    if mesh.triangles == 0 {
        return Err(Tiles3dError::Empty);
    }
    build_glb(&mesh, color)
}

/// Assemble a single-mesh `.glb` from the accumulated triangles.
fn build_glb(mesh: &MeshBuilder, color: [u8; 4]) -> Result<Vec<u8>, Tiles3dError> {
    let vertex_count = mesh.positions.len() / 3;

    // BIN buffer: POSITION (count·3·f32) then NORMAL (count·3·f32). Both are
    // 12-byte-strided, so byte offsets stay 4-aligned for free.
    let pos_bytes = mesh.positions.len() * 4;
    let nrm_off = pos_bytes;
    let mut bin = Vec::with_capacity(pos_bytes + mesh.normals.len() * 4);
    for f in &mesh.positions {
        bin.extend_from_slice(&f.to_le_bytes());
    }
    for f in &mesh.normals {
        bin.extend_from_slice(&f.to_le_bytes());
    }
    // BIN chunk must be 4-byte aligned (count·24 already is, but stay defensive).
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }

    let base_color = [
        color[0] as f64 / 255.0,
        color[1] as f64 / 255.0,
        color[2] as f64 / 255.0,
        color[3] as f64 / 255.0,
    ];
    let gltf = json!({
        "asset": { "version": "2.0", "generator": "MeteoCore ds-3dtiles isosurface" },
        "scene": 0,
        "scenes": [ { "nodes": [0] } ],
        "nodes": [ { "mesh": 0 } ],
        "meshes": [ {
            "primitives": [ {
                "attributes": { "POSITION": 0, "NORMAL": 1 },
                "material": 0,
                "mode": MODE_TRIANGLES,
            } ]
        } ],
        "materials": [ {
            "pbrMetallicRoughness": {
                "baseColorFactor": base_color,
                "metallicFactor": 0.0,
                "roughnessFactor": 1.0,
            },
            // Normals are oriented outward in `MeshBuilder::push`, so lighting
            // is correct from the front; `doubleSided` is belt-and-suspenders
            // for grazing/back views (and any residual ambiguous quad).
            "doubleSided": true,
        } ],
        "accessors": [
            {
                "bufferView": 0, "componentType": COMPONENT_FLOAT, "count": vertex_count,
                "type": "VEC3", "min": mesh.min, "max": mesh.max,
            },
            {
                "bufferView": 1, "componentType": COMPONENT_FLOAT, "count": vertex_count,
                "type": "VEC3",
            },
        ],
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": pos_bytes, "target": TARGET_ARRAY_BUFFER },
            { "buffer": 0, "byteOffset": nrm_off, "byteLength": mesh.normals.len() * 4, "target": TARGET_ARRAY_BUFFER },
        ],
        "buffers": [ { "byteLength": bin.len() } ],
    });

    // Shared GLB assembler: 4-byte chunk padding + `u32`-checked total length +
    // a serialize-error path instead of an `expect` panic. The BIN above is
    // f32-only (always 4-aligned), so the helper's padding is a no-op here.
    crate::assemble_glb(&gltf, bin)
}

/// Build the `tileset.json` for an isosurface `.glb`. Like
/// [`crate::tileset_json_for_region`] but for glTF content, which (unlike
/// `.pnts`) carries no embedded origin — so the antenna ECEF (`rtc_center`) is
/// the tile **`transform`** (a pure translation), placing the mesh's
/// antenna-relative positions at their true global location.
///
/// The `region` bounding volume stays geodetic (EPSG:4979) and is *not* affected
/// by the transform, per the 3D Tiles spec. Same `content_uri`/region/finite
/// validation and load-bearing non-zero `geometricError` as the `.pnts` tileset.
pub fn tileset_json_glb(
    region: [f64; 6],
    content_uri: &str,
    rtc_center: [f64; 3],
) -> Result<String, Tiles3dError> {
    // Reuse the shared content-URI + region validation, building on the tileset
    // `Value` directly (no serialize-then-reparse) and injecting the transform
    // (the only glTF-specific addition).
    if rtc_center.iter().any(|c| !c.is_finite()) {
        return Err(Tiles3dError::NonFinite("rtc_center"));
    }
    let mut tileset = crate::tileset_value_for_region(region, content_uri)?;
    let [cx, cy, cz] = rtc_center;
    // Column-major 4×4: identity rotation, translation = antenna ECEF.
    tileset["root"]["transform"] =
        json!([1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, cx, cy, cz, 1.0]);
    Ok(serde_json::to_string_pretty(&tileset).expect("tileset serializes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small grid whose value is a function of height only, so a horizontal
    /// isosurface is a clean cylindrical sheet (predictable, easy to assert).
    /// `value(i_h) = i_h` so threshold 1.5 separates the lower two height layers
    /// from the upper ones.
    fn ramp_grid(n_r: usize, n_a: usize, n_h: usize) -> VoxelGrid {
        let mut values = vec![f32::NAN; n_r * n_a * n_h];
        let dims = [n_r, n_a, n_h];
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

    /// Parse a `.glb`: returns (glTF JSON, BIN bytes) after validating the
    /// container structure.
    fn parse_glb(glb: &[u8]) -> (serde_json::Value, Vec<u8>) {
        assert_eq!(&glb[0..4], &0x46546C67u32.to_le_bytes(), "glTF magic");
        assert_eq!(
            u32::from_le_bytes(glb[4..8].try_into().unwrap()),
            2,
            "version"
        );
        let total = u32::from_le_bytes(glb[8..12].try_into().unwrap()) as usize;
        assert_eq!(total, glb.len(), "header length == actual length");

        let json_len = u32::from_le_bytes(glb[12..16].try_into().unwrap()) as usize;
        assert_eq!(
            &glb[16..20],
            &0x4E4F534Au32.to_le_bytes(),
            "JSON chunk type"
        );
        let json_end = 20 + json_len;
        let json: serde_json::Value =
            serde_json::from_slice(&glb[20..json_end]).expect("glTF JSON parses");

        let bin_len = u32::from_le_bytes(glb[json_end..json_end + 4].try_into().unwrap()) as usize;
        assert_eq!(
            &glb[json_end + 4..json_end + 8],
            &0x004E4942u32.to_le_bytes(),
            "BIN chunk type"
        );
        let bin = glb[json_end + 8..json_end + 8 + bin_len].to_vec();
        assert_eq!(json_len % 4, 0, "JSON chunk 4-aligned");
        assert_eq!(bin_len % 4, 0, "BIN chunk 4-aligned");
        (json, bin)
    }

    #[test]
    fn isosurface_glb_is_wellformed() {
        let grid = ramp_grid(3, 8, 5);
        let glb = encode_isosurface_glb(&grid, 1.5, [255, 0, 0, 255], None).expect("encode");
        let (json, bin) = parse_glb(&glb);

        assert_eq!(json["asset"]["version"], "2.0");
        assert_eq!(json["meshes"][0]["primitives"][0]["mode"], MODE_TRIANGLES);
        assert!(json["materials"][0]["doubleSided"].as_bool().unwrap());

        // POSITION + NORMAL accessors, same non-zero count, POSITION has min/max.
        let pos = &json["accessors"][0];
        let nrm = &json["accessors"][1];
        let count = pos["count"].as_u64().unwrap();
        assert!(
            count > 0 && count % 3 == 0,
            "vertex count {count} = 3·triangles"
        );
        assert_eq!(nrm["count"].as_u64().unwrap(), count);
        assert_eq!(pos["min"].as_array().unwrap().len(), 3);
        assert_eq!(pos["max"].as_array().unwrap().len(), 3);

        // BIN holds exactly POSITION + NORMAL (count·3·f32 each).
        assert_eq!(bin.len(), (count as usize) * 3 * 4 * 2);
        // The buffer byteLength matches the BIN payload.
        assert_eq!(
            json["buffers"][0]["byteLength"].as_u64().unwrap(),
            bin.len() as u64
        );

        // Every position/normal float is finite (no NaN leaked from nodata).
        for w in bin.chunks_exact(4) {
            assert!(f32::from_le_bytes(w.try_into().unwrap()).is_finite());
        }
    }

    #[test]
    fn isosurface_sits_at_the_expected_height() {
        // value = i_h, threshold 4.5 → the surface is a sheet where the field
        // crosses 4.5, i.e. fractional height index 4.5. With height_range
        // [0,9000] over 9 cells (cell centres 500,1500,…), index 4.5 maps to
        // height (4.5+0.5)·1000 = 5000 m above the origin — and ONLY there,
        // since the field depends solely on height. The pre-march smoothing
        // (#381) preserves a linear ramp exactly except within `passes` (=2)
        // cells of the clamped height ends, so a crossing between cells 4 and 5
        // of 9 is untouched. Use a small 5 km disc so earth-curvature/
        // up-deflection over the span is negligible, then reconstruct each
        // vertex's height above the antenna and assert ≈5000 m.
        let mut grid = ramp_grid(4, 16, 9);
        grid.radius_range = [0.0, 5_000.0];
        grid.height_range = [0.0, 9_000.0];
        let glb = encode_isosurface_glb(&grid, 4.5, [0, 128, 255, 255], None).unwrap();
        let (json, bin) = parse_glb(&glb);

        let rtc = geodetic_to_ecef(grid.origin_lon, grid.origin_lat, grid.origin_height);
        let up = {
            let m = (rtc[0] * rtc[0] + rtc[1] * rtc[1] + rtc[2] * rtc[2]).sqrt();
            [rtc[0] / m, rtc[1] / m, rtc[2] / m] // geocentric up ≈ local up
        };
        let pos_len = json["bufferViews"][0]["byteLength"].as_u64().unwrap() as usize;
        let floats: Vec<f32> = bin[..pos_len]
            .chunks_exact(4)
            .map(|w| f32::from_le_bytes(w.try_into().unwrap()))
            .collect();
        assert!(!floats.is_empty(), "surface has vertices");
        for v in floats.chunks_exact(3) {
            // Invert the stored Z-up→Y-up flip: glTF (x,y,z) → ECEF offset
            // (x,−z,y); height above antenna = offset · up.
            let offset = [v[0] as f64, -(v[2] as f64), v[1] as f64];
            let h = offset[0] * up[0] + offset[1] * up[1] + offset[2] * up[2];
            assert!((h - 5000.0).abs() < 50.0, "vertex height {h} ≉ 5000 m");
        }
    }

    #[test]
    fn isosurface_normals_point_outward() {
        // value = i_h increases with height, so inside (>= 1.5) is the UPPER
        // region and "outward" (toward lower values) is DOWNWARD. Every stored
        // face normal must therefore have a negative geocentric-up component —
        // this is the orientation fix (without it ~half would point up/inward).
        let mut grid = ramp_grid(4, 16, 5);
        grid.radius_range = [0.0, 5_000.0]; // small disc → up ≈ geocentric up
        let glb = encode_isosurface_glb(&grid, 1.5, [0, 128, 255, 255], None).unwrap();
        let (json, bin) = parse_glb(&glb);

        let rtc = geodetic_to_ecef(grid.origin_lon, grid.origin_lat, grid.origin_height);
        let m = (rtc[0] * rtc[0] + rtc[1] * rtc[1] + rtc[2] * rtc[2]).sqrt();
        let up = [rtc[0] / m, rtc[1] / m, rtc[2] / m];
        let nrm_off = json["bufferViews"][1]["byteOffset"].as_u64().unwrap() as usize;
        let nrm_len = json["bufferViews"][1]["byteLength"].as_u64().unwrap() as usize;
        let normals: Vec<f32> = bin[nrm_off..nrm_off + nrm_len]
            .chunks_exact(4)
            .map(|w| f32::from_le_bytes(w.try_into().unwrap()))
            .collect();
        assert!(!normals.is_empty(), "surface has normals");
        for n in normals.chunks_exact(3) {
            // Invert the Y-up flip: glTF normal (x,y,z) → ECEF (x,−z,y).
            let ne = [n[0] as f64, -(n[2] as f64), n[1] as f64];
            let up_comp = ne[0] * up[0] + ne[1] * up[1] + ne[2] * up[2];
            assert!(
                up_comp < 0.0,
                "normal must point outward (down): up={up_comp}"
            );
        }
    }

    #[test]
    fn empty_when_threshold_above_all_data() {
        let grid = ramp_grid(3, 8, 5); // values 0..4
        assert!(matches!(
            encode_isosurface_glb(&grid, 100.0, [255, 0, 0, 255], None),
            Err(Tiles3dError::Empty)
        ));
    }

    #[test]
    fn nodata_corners_do_not_fabricate_surface_without_background() {
        // With background = None, a grid that is all-NaN except a single isolated
        // finite cell can form no tetrahedron with all-finite corners → no surface
        // (NaN corners are skipped, not fabricated into a 1-cell blob).
        let mut grid = ramp_grid(3, 8, 5);
        grid.values.iter_mut().for_each(|v| *v = f32::NAN);
        let i = grid.index(1, 4, 2);
        grid.values[i] = 50.0;
        assert!(matches!(
            encode_isosurface_glb(&grid, 1.5, [255, 0, 0, 255], None),
            Err(Tiles3dError::Empty)
        ));
    }

    #[test]
    fn open_surface_meshes_crossings_inside_finite_region_only() {
        // background = None with a crossing entirely INSIDE the finite region:
        // a 40 dBZ core wrapped in a finite 0 dBZ shell, all surrounded by NaN.
        // The NaN-aware pre-smoothing must keep the finite 40↔0 crossing intact
        // (no erosion from the NaN surroundings) and the mesher must emit the
        // surface there while still skipping every NaN-touching tetrahedron —
        // closing the gap between the smoothing unit tests and the mesher.
        let mut grid = ramp_grid(10, 16, 10);
        grid.values.iter_mut().for_each(|v| *v = f32::NAN);
        // Finite shell (0 dBZ) spanning r/h 1..9, a 3..13…
        for ir in 1..9 {
            for ia in 3..13 {
                for ih in 1..9 {
                    let idx = grid.index(ir, ia, ih);
                    grid.values[idx] = 0.0;
                }
            }
        }
        // …with a 40 dBZ core in its middle. The 40↔0 crossing must sit
        // ≥ SMOOTH_PASSES (= 2) finite cells from any NaN: the NaN-aware blur
        // renormalizes over finite neighbours, so finite cells within `passes`
        // cells of the open boundary shift toward their finite neighbours — a
        // crossing closer than that could move or vanish. If SMOOTH_PASSES is
        // bumped, widen the shell margin here to match.
        for ir in 3..7 {
            for ia in 6..10 {
                for ih in 3..7 {
                    let idx = grid.index(ir, ia, ih);
                    grid.values[idx] = 40.0;
                }
            }
        }
        let glb = encode_isosurface_glb(&grid, 20.0, [0, 200, 0, 255], None)
            .expect("crossing inside the finite region must mesh without a background");
        let (json, _bin) = parse_glb(&glb);
        let count = json["accessors"][0]["count"].as_u64().unwrap();
        assert!(
            count > 0 && count % 3 == 0,
            "open surface has triangles: {count}"
        );
    }

    #[test]
    fn background_seals_echo_surrounded_by_clear_air() {
        // A compact echo core embedded in NaN (clear air). With background = None
        // the surface can't close (Empty). With background = Some(below-threshold),
        // the NaN corners are treated as no-echo and the surface seals into a
        // closed blob — exactly the fix for the "curtains" artifact.
        let mut grid = ramp_grid(8, 16, 8);
        grid.values.iter_mut().for_each(|v| *v = f32::NAN);
        // A 4×4×4 finite >threshold core in the interior, rest NaN. Big enough
        // that the pre-march smoothing (2 passes against the sealed −32 floor)
        // leaves its centre well above the 20 dBZ threshold (a 2×2×2 core would
        // smooth entirely below it — physical echoes span many cells).
        for ir in 2..6 {
            for ia in 6..10 {
                for ih in 2..6 {
                    let idx = grid.index(ir, ia, ih);
                    grid.values[idx] = 40.0;
                }
            }
        }
        // No finite-<threshold neighbours, so without a background it can't close.
        assert!(matches!(
            encode_isosurface_glb(&grid, 20.0, [0, 200, 0, 255], None),
            Err(Tiles3dError::Empty)
        ));
        // With a background floor below the threshold, it seals into a real mesh.
        let glb = encode_isosurface_glb(&grid, 20.0, [0, 200, 0, 255], Some(-32.0))
            .expect("sealed surface");
        let (json, _bin) = parse_glb(&glb);
        let count = json["accessors"][0]["count"].as_u64().unwrap();
        assert!(
            count > 0 && count % 3 == 0,
            "sealed blob has triangles: {count}"
        );
    }

    #[test]
    fn non_finite_threshold_or_background_is_rejected() {
        let grid = ramp_grid(3, 8, 5);
        assert!(matches!(
            encode_isosurface_glb(&grid, f64::NAN, [0, 0, 0, 255], None),
            Err(Tiles3dError::NonFinite("threshold"))
        ));
        // A Some(non-finite) background is rejected (would silently act like None).
        assert!(matches!(
            encode_isosurface_glb(&grid, 1.5, [0, 0, 0, 255], Some(f64::INFINITY)),
            Err(Tiles3dError::NonFinite("background"))
        ));
        // A finite f64 beyond f32 range is rejected too (the seal narrows to
        // f32, so 1e39 would cast to -inf and poison the dense blur).
        assert!(matches!(
            encode_isosurface_glb(&grid, 1.5, [0, 0, 0, 255], Some(-1e39)),
            Err(Tiles3dError::NonFinite("background"))
        ));
    }

    #[test]
    fn background_not_below_threshold_is_rejected() {
        // background >= threshold would seal NaN cells as INSIDE the surface,
        // inverting it — reject rather than return a meaningless mesh.
        let grid = ramp_grid(3, 8, 5);
        for bg in [20.0_f64, 25.0] {
            assert!(
                matches!(
                    encode_isosurface_glb(&grid, 20.0, [0, 0, 0, 255], Some(bg)),
                    Err(Tiles3dError::BackgroundNotBelowThreshold {
                        background,
                        threshold,
                    }) if background == bg && threshold == 20.0
                ),
                "background {bg} >= threshold 20 must be rejected"
            );
        }
        // Strictly below is accepted (the ramp has values 0..4, so 1.5 yields a
        // surface; background −32 < 1.5).
        assert!(encode_isosurface_glb(&grid, 1.5, [0, 0, 0, 255], Some(-32.0)).is_ok());
    }

    #[test]
    fn tileset_glb_carries_translation_transform_and_region() {
        let region = [0.42, 1.05, 0.44, 1.07, 100.0, 12_000.0];
        let rtc = [3_000_000.0, 1_000_000.0, 5_000_000.0];
        let s = tileset_json_glb(region, "content.glb", rtc).expect("tileset");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(v["geometricError"].as_f64().unwrap() > 0.0, "load-bearing");
        assert_eq!(v["root"]["content"]["uri"], "content.glb");
        let t = v["root"]["transform"].as_array().unwrap();
        assert_eq!(t.len(), 16);
        // Translation is the last column (indices 12,13,14) in column-major.
        assert_eq!(t[12].as_f64().unwrap(), 3_000_000.0);
        assert_eq!(t[13].as_f64().unwrap(), 1_000_000.0);
        assert_eq!(t[14].as_f64().unwrap(), 5_000_000.0);
        // Region passes straight through, unaffected by the transform.
        assert_eq!(
            v["root"]["boundingVolume"]["region"][0].as_f64().unwrap(),
            0.42
        );
    }

    #[test]
    fn tileset_glb_rejects_unsafe_uri_and_bad_region() {
        let rtc = [1.0, 2.0, 3.0];
        assert!(matches!(
            tileset_json_glb([0.0, 0.0, 0.1, 0.1, 0.0, 1.0], "../x.glb", rtc),
            Err(Tiles3dError::InvalidUri(_))
        ));
        // south > north is inverted.
        assert!(matches!(
            tileset_json_glb([0.0, 0.2, 0.1, 0.1, 0.0, 1.0], "content.glb", rtc),
            Err(Tiles3dError::InvalidRegion(_))
        ));
        // Non-finite rtc is rejected.
        assert!(matches!(
            tileset_json_glb(
                [0.0, 0.0, 0.1, 0.1, 0.0, 1.0],
                "content.glb",
                [f64::NAN, 0.0, 0.0]
            ),
            Err(Tiles3dError::NonFinite("rtc_center"))
        ));
    }
}
