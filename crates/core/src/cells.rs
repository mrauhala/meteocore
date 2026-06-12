//! Storm-cell segmentation + tracking on cylindrical voxel grids (#367).
//!
//! [`extract_cells`] segments the connected echo regions of a [`VoxelGrid`]
//! at or above a reflectivity threshold into discrete [`StormCell`]s, each
//! carrying physical attributes (max dBZ, echo top/base, volume, area, VIL,
//! centroid) and a geographic footprint ring. [`track_cells`] then matches
//! cell centroids across consecutive volume scans into [`Track`]s with a
//! per-step motion vector — the seed for a future motion/optical-flow field
//! product (the per-cell `(u, v)` is exactly what a flow-field raster would
//! interpolate).
//!
//! Everything here is pure geometry + physics on the domain type — no
//! framework dependencies, no I/O — so any [`crate::volume::VolumeEngine`]
//! with voxel-grid capability gets cells for free (see
//! [`crate::volume::VolumeEngine::read_cells`]), and every API surface
//! (3D Tiles, Features, WMS/Maps/Tiles) renders one shared product.
//!
//! Values are assumed to be radar reflectivity in dBZ: the linear-Z centroid
//! weighting and the VIL integral are reflectivity physics. Callers gate the
//! quantity (the ODIM engine only computes cells for dBZ-unit quantities).

use crate::geo::{destination_point, EARTH_RADIUS_M};
use crate::volume::VoxelGrid;
use chrono::{DateTime, Utc};
use std::collections::VecDeque;
use std::sync::Arc;

/// Liquid-water cap (dBZ) for the VIL integral: reflectivity above this is
/// treated as hail contamination and clamped, per the standard NSSL practice.
const VIL_DBZ_CAP: f64 = 56.0;
/// VIL mass coefficient: `M = 3.44e-6 · Z^(4/7)` (**kg m⁻³**, Z in mm⁶ m⁻³;
/// the textbook form is `3.44e-3 g m⁻³`) — integrated over height in metres
/// it yields kg m⁻² directly.
const VIL_COEFF: f64 = 3.44e-6;

/// Knobs for [`extract_cells`]. `Default` is the canonical configuration
/// (35 dBZ — the classic TITAN cell threshold — with a 5 km³ speckle floor).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellExtractionOptions {
    /// Segmentation threshold in the grid's physical unit (dBZ): a voxel is a
    /// cell member iff `value >= threshold` (the isosurface convention).
    pub threshold: f64,
    /// Drop components smaller than this volume (km³) — beam-width speckle
    /// and bird/clutter residue segment into many tiny components.
    pub min_volume_km3: f64,
    /// Hard cap on returned cells (the largest by volume are kept) — bounds
    /// downstream encoding work on pathological scans.
    pub max_cells: usize,
}

impl Default for CellExtractionOptions {
    fn default() -> Self {
        Self {
            threshold: 35.0,
            min_volume_km3: 5.0,
            max_cells: 256,
        }
    }
}

/// One segmented storm cell. Geographic positions are WGS84 lon/lat degrees;
/// heights are metres above mean sea level (the grid origin's height datum);
/// the ENU bounding box is metres relative to the grid origin (the radar
/// antenna), so the 3D Tiles encoder can place it antenna-relative without
/// re-deriving the origin.
#[derive(Debug, Clone, PartialEq)]
pub struct StormCell {
    /// Per-scan component label, 1-based, ordered by volume descending —
    /// deterministic for a given grid + options, so it is usable in cache
    /// keys and stable feature ids.
    pub label: u32,
    /// Maximum value (dBZ) in the cell.
    pub max_dbz: f64,
    /// Location of the maximum: `[lon_deg, lat_deg, height_m_msl]`.
    pub max_dbz_pos: [f64; 3],
    /// Linear-Z-weighted centroid: `[lon_deg, lat_deg, height_m_msl]`.
    pub centroid: [f64; 3],
    /// Top of the highest member voxel (m MSL).
    pub echo_top_m: f64,
    /// Bottom of the lowest member voxel (m MSL).
    pub base_m: f64,
    /// Cell volume (km³) — sum of member-voxel volumes (radius-dependent:
    /// a cylindrical-grid voxel grows with ground range).
    pub volume_km3: f64,
    /// Footprint area (km²) — the union of the cell's `(radius, azimuth)`
    /// columns projected to the ground.
    pub area_km2: f64,
    /// Maximum vertically-integrated liquid (kg m⁻²) over the footprint
    /// columns, computed on the resampled grid (an approximation — beam
    /// broadening and the 56 dBZ hail cap apply).
    pub max_vil_kg_m2: f64,
    /// Closed footprint ring (`first == last`), WGS84 `[lon_deg, lat_deg]`,
    /// counter-clockwise (RFC 7946 exterior orientation), lightly simplified.
    pub footprint: Vec<[f64; 2]>,
    /// Axis-aligned local-ENU bounding box around the cell, metres relative
    /// to the grid origin: `[min_e, min_n, min_u, max_e, max_n, max_u]`
    /// (`u` is height above the origin, not MSL).
    pub bbox_enu_m: [f64; 6],
}

/// All cells segmented from one volume scan.
#[derive(Debug, Clone, PartialEq)]
pub struct CellSet {
    /// Valid time of the scanned volume.
    pub time: DateTime<Utc>,
    /// Quantity the grid was sampled from (e.g. `"DBZH"`).
    pub quantity: String,
    /// Threshold the segmentation used (dBZ).
    pub threshold: f64,
    /// Grid origin (radar antenna): `[lon_deg, lat_deg, height_m_msl]`.
    pub origin: [f64; 3],
    /// Cells, ordered by volume descending (label order).
    pub cells: Vec<StormCell>,
}

impl CellSet {
    /// A scan with no cells (no echo at/above the threshold) — still a valid
    /// tracking input (every track alive at the previous scan dies here).
    pub fn empty(
        time: DateTime<Utc>,
        quantity: impl Into<String>,
        threshold: f64,
        origin: [f64; 3],
    ) -> Self {
        Self {
            time,
            quantity: quantity.into(),
            threshold,
            origin,
            cells: Vec::new(),
        }
    }
}

/// One observation of a tracked cell.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackPoint {
    /// Scan valid time.
    pub time: DateTime<Utc>,
    /// Cell centroid at this scan (WGS84 degrees / m MSL).
    pub lon: f64,
    pub lat: f64,
    pub height_m: f64,
    /// The cell's label in that scan's [`CellSet`] (joins a track point back
    /// to its full [`StormCell`]).
    pub label: u32,
    /// The cell's maximum value at this scan (dBZ).
    pub max_dbz: f64,
}

/// A cell-centroid trajectory across consecutive scans.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    /// `"{first scan time:%Y%m%dT%H%M%SZ}-{first label}"` — deterministic for
    /// a given scan window. Known limitation (stateless recompute): when the
    /// retention window slides past the track's first scan, the id changes.
    pub id: String,
    /// Observations, ascending in time. Always non-empty.
    pub points: Vec<TrackPoint>,
    /// Motion from the most recent matched step: `(u east, v north)` m s⁻¹.
    /// `None` for a track observed only once.
    pub motion_ms: Option<(f64, f64)>,
}

impl Track {
    /// `(speed m s⁻¹, bearing°)` of the latest motion — bearing is the
    /// compass direction the cell is moving **toward** (0° = north,
    /// clockwise). `None` for a single-observation track.
    pub fn speed_direction(&self) -> Option<(f64, f64)> {
        let (u, v) = self.motion_ms?;
        let speed = u.hypot(v);
        let dir = u.atan2(v).to_degrees().rem_euclid(360.0);
        Some((speed, dir))
    }
}

/// All tracks over a scan window, ordered by (first time, first label).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackSet {
    pub tracks: Vec<Track>,
}

impl TrackSet {
    /// The track containing the cell `label` observed at `time` (exact scan
    /// time), if any. Linear scan — cell counts are tiny.
    pub fn track_for(&self, time: DateTime<Utc>, label: u32) -> Option<&Track> {
        self.tracks
            .iter()
            .find(|t| t.points.iter().any(|p| p.time == time && p.label == label))
    }
}

/// Knobs for [`track_cells`]. The gate is `max_speed_ms · Δt + base_gate_m`:
/// the speed term scales with scan cadence, the base term absorbs centroid
/// jitter between scans.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackingOptions {
    /// Fastest plausible cell motion (m s⁻¹). 40 m s⁻¹ ≈ 144 km h⁻¹ covers
    /// fast-moving squall lines.
    pub max_speed_ms: f64,
    /// Constant gate slack (m) for centroid jitter.
    pub base_gate_m: f64,
}

impl Default for TrackingOptions {
    fn default() -> Self {
        Self {
            max_speed_ms: 40.0,
            base_gate_m: 5_000.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Extraction
// ---------------------------------------------------------------------------

/// Per-component accumulator for the labeling pass.
struct Comp {
    count: usize,
    volume_m3: f64,
    /// Linear-Z weighted ENU sums (metres relative to the grid origin).
    sum_w: f64,
    sum_we: f64,
    sum_wn: f64,
    sum_wu: f64,
    max_dbz: f64,
    max_idx: (usize, usize, usize),
    min_ih: usize,
    max_ih: usize,
}

/// Segment the connected echo regions of `grid` with `value >= threshold`
/// into [`StormCell`]s. Connectivity is 6-neighbour (±radius, ±azimuth,
/// ±height) with the azimuth axis wrapping when the grid spans the full
/// circle. `NaN` (unmeasured) and sub-threshold cells are background.
///
/// `time` is the scan's valid time, carried into the [`CellSet`] (a grid does
/// not know its own time).
pub fn extract_cells(
    grid: &VoxelGrid,
    time: DateTime<Utc>,
    opts: &CellExtractionOptions,
) -> CellSet {
    let [n_r, n_a, n_h] = grid.dims;
    let total = n_r * n_a * n_h;
    let origin = [grid.origin_lon, grid.origin_lat, grid.origin_height];
    let mut set = CellSet {
        time,
        quantity: grid.quantity.clone(),
        threshold: opts.threshold,
        origin,
        cells: Vec::new(),
    };
    if total == 0 || grid.values.len() != total || opts.max_cells == 0 {
        return set;
    }

    let dr = (grid.radius_range[1] - grid.radius_range[0]) / n_r as f64;
    let da = (grid.angle_range[1] - grid.angle_range[0]) / n_a as f64;
    let dh = (grid.height_range[1] - grid.height_range[0]) / n_h as f64;
    if !(dr.is_finite() && dr > 0.0 && da.is_finite() && da > 0.0 && dh.is_finite() && dh > 0.0) {
        return set;
    }
    // Azimuth wraps only on a full-circle grid (guard for future sector scans).
    let wrap = (grid.angle_range[1] - grid.angle_range[0] - std::f64::consts::TAU).abs() < 1e-9;

    let r_centre = |i_r: usize| grid.radius_range[0] + (i_r as f64 + 0.5) * dr;
    let a_centre = |i_a: usize| grid.angle_range[0] + (i_a as f64 + 0.5) * da;
    let h_centre = |i_h: usize| grid.height_range[0] + (i_h as f64 + 0.5) * dh;

    let threshold = opts.threshold as f32;
    let member = |idx: usize| grid.values[idx] >= threshold; // NaN compares false

    // --- Pass 1: BFS connected components, scalar accumulation -------------
    let mut labels = vec![0u32; total];
    let mut comps: Vec<Comp> = Vec::new();
    let mut queue: VecDeque<(usize, usize, usize)> = VecDeque::new();

    for seed_r in 0..n_r {
        for seed_a in 0..n_a {
            for seed_h in 0..n_h {
                let seed_idx = VoxelGrid::index_of(grid.dims, seed_r, seed_a, seed_h);
                if labels[seed_idx] != 0 || !member(seed_idx) {
                    continue;
                }
                let raw_label = comps.len() as u32 + 1;
                let mut comp = Comp {
                    count: 0,
                    volume_m3: 0.0,
                    sum_w: 0.0,
                    sum_we: 0.0,
                    sum_wn: 0.0,
                    sum_wu: 0.0,
                    max_dbz: f64::NEG_INFINITY,
                    max_idx: (seed_r, seed_a, seed_h),
                    min_ih: seed_h,
                    max_ih: seed_h,
                };
                labels[seed_idx] = raw_label;
                queue.push_back((seed_r, seed_a, seed_h));
                while let Some((i_r, i_a, i_h)) = queue.pop_front() {
                    let idx = VoxelGrid::index_of(grid.dims, i_r, i_a, i_h);
                    let v = grid.values[idx] as f64;
                    let rc = r_centre(i_r);
                    comp.count += 1;
                    comp.volume_m3 += dr * (rc * da) * dh;
                    let w = 10f64.powf(v / 10.0); // linear Z
                    let az = a_centre(i_a);
                    comp.sum_w += w;
                    comp.sum_we += w * rc * az.sin();
                    comp.sum_wn += w * rc * az.cos();
                    comp.sum_wu += w * h_centre(i_h);
                    if v > comp.max_dbz {
                        comp.max_dbz = v;
                        comp.max_idx = (i_r, i_a, i_h);
                    }
                    comp.min_ih = comp.min_ih.min(i_h);
                    comp.max_ih = comp.max_ih.max(i_h);

                    // 6-connectivity: ±r, ±a (wrapping), ±h.
                    let mut visit = |r: usize, a: usize, h: usize| {
                        let nidx = VoxelGrid::index_of(grid.dims, r, a, h);
                        if labels[nidx] == 0 && member(nidx) {
                            labels[nidx] = raw_label;
                            queue.push_back((r, a, h));
                        }
                    };
                    if i_r > 0 {
                        visit(i_r - 1, i_a, i_h);
                    }
                    if i_r + 1 < n_r {
                        visit(i_r + 1, i_a, i_h);
                    }
                    if i_h > 0 {
                        visit(i_r, i_a, i_h - 1);
                    }
                    if i_h + 1 < n_h {
                        visit(i_r, i_a, i_h + 1);
                    }
                    if i_a > 0 {
                        visit(i_r, i_a - 1, i_h);
                    } else if wrap && n_a > 1 {
                        visit(i_r, n_a - 1, i_h);
                    }
                    if i_a + 1 < n_a {
                        visit(i_r, i_a + 1, i_h);
                    } else if wrap && n_a > 1 {
                        visit(i_r, 0, i_h);
                    }
                }
                comps.push(comp);
            }
        }
    }
    if comps.is_empty() {
        return set;
    }

    // --- Filter + rank: volume floor, largest first, hard cap --------------
    let min_volume_m3 = opts.min_volume_km3 * 1e9;
    let mut ranked: Vec<usize> = (0..comps.len())
        .filter(|&i| comps[i].volume_m3 >= min_volume_m3)
        .collect();
    ranked.sort_by(|&a, &b| {
        comps[b]
            .volume_m3
            .partial_cmp(&comps[a].volume_m3)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(&b)) // deterministic tie-break: discovery order
    });
    ranked.truncate(opts.max_cells);
    if ranked.is_empty() {
        return set;
    }
    // raw label (1-based) → final rank (0-based)
    let mut rank_of = vec![usize::MAX; comps.len() + 1];
    for (rank, &ci) in ranked.iter().enumerate() {
        rank_of[ci + 1] = rank;
    }

    // --- Pass 2: per-kept-cell footprint mask, column VIL, ENU bbox --------
    // Azimuth-boundary sin/cos tables (n_a + 1 boundaries) for the bbox
    // corner math, instead of 4 trig calls per member voxel.
    let a_boundary: Vec<(f64, f64)> = (0..=n_a)
        .map(|i| (grid.angle_range[0] + i as f64 * da).sin_cos())
        .collect();

    struct Detail {
        mask: Vec<bool>, // (r, a) footprint columns, n_r × n_a
        area_m2: f64,    // Σ over claimed columns of Δr · (r_c · Δa)
        max_vil: f64,
        bbox: [f64; 6],
    }
    let mut details: Vec<Detail> = ranked
        .iter()
        .map(|_| Detail {
            mask: vec![false; n_r * n_a],
            area_m2: 0.0,
            max_vil: 0.0,
            bbox: [
                f64::INFINITY,
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
            ],
        })
        .collect();

    // Per-column scratch: (rank, accumulated VIL) — a column rarely holds
    // more than one or two distinct cells.
    let mut col_vil: Vec<(usize, f64)> = Vec::with_capacity(4);
    for i_r in 0..n_r {
        let r_lo = grid.radius_range[0] + i_r as f64 * dr;
        let r_hi = r_lo + dr;
        for i_a in 0..n_a {
            col_vil.clear();
            for i_h in 0..n_h {
                let idx = VoxelGrid::index_of(grid.dims, i_r, i_a, i_h);
                let raw = labels[idx] as usize;
                if raw == 0 {
                    continue;
                }
                let rank = rank_of[raw];
                if rank == usize::MAX {
                    continue;
                }
                let d = &mut details[rank];
                // First claim of this column for this cell: count its ground
                // area here, in the voxel pass — a per-cell mask sweep after
                // the loop would cost O(max_cells × n_r × n_a).
                if !d.mask[i_r * n_a + i_a] {
                    d.mask[i_r * n_a + i_a] = true;
                    d.area_m2 += dr * (r_centre(i_r) * da);
                }

                // VIL layer contribution: M(Z)·Δh with the hail cap.
                let dbz = (grid.values[idx] as f64).min(VIL_DBZ_CAP);
                let z_lin = 10f64.powf(dbz / 10.0);
                let m = VIL_COEFF * z_lin.powf(4.0 / 7.0) * dh;
                match col_vil.iter_mut().find(|(r, _)| *r == rank) {
                    Some((_, vil)) => *vil += m,
                    None => col_vil.push((rank, m)),
                }

                // ENU bbox over the voxel's 4 (r, az) corners + height span.
                let (s_lo, c_lo) = a_boundary[i_a];
                let (s_hi, c_hi) = a_boundary[i_a + 1];
                for (r, (s, c)) in [
                    (r_lo, (s_lo, c_lo)),
                    (r_lo, (s_hi, c_hi)),
                    (r_hi, (s_lo, c_lo)),
                    (r_hi, (s_hi, c_hi)),
                ] {
                    let e = r * s;
                    let n = r * c;
                    d.bbox[0] = d.bbox[0].min(e);
                    d.bbox[1] = d.bbox[1].min(n);
                    d.bbox[3] = d.bbox[3].max(e);
                    d.bbox[4] = d.bbox[4].max(n);
                }
                let h_lo = grid.height_range[0] + i_h as f64 * dh;
                d.bbox[2] = d.bbox[2].min(h_lo);
                d.bbox[5] = d.bbox[5].max(h_lo + dh);
            }
            for &(rank, vil) in &col_vil {
                if vil > details[rank].max_vil {
                    details[rank].max_vil = vil;
                }
            }
        }
    }

    // --- Assemble StormCells ------------------------------------------------
    let to_lonlat = |ground_m: f64, bearing_rad: f64| -> (f64, f64) {
        destination_point(
            grid.origin_lon,
            grid.origin_lat,
            ground_m,
            bearing_rad.to_degrees(),
        )
    };
    let enu_to_lonlat = |e: f64, n: f64| -> (f64, f64) {
        let ground = e.hypot(n);
        if ground <= f64::EPSILON {
            (grid.origin_lon, grid.origin_lat)
        } else {
            to_lonlat(ground, e.atan2(n))
        }
    };
    // Simplification tolerance: half a radial cell — keeps the ring faithful
    // at the grid's own resolution without GeoJSON bloat.
    let simplify_tol_m = 0.5 * dr;

    for (rank, &ci) in ranked.iter().enumerate() {
        let comp = &comps[ci];
        let d = &details[rank];

        let (ce, cn, cu) = if comp.sum_w > 0.0 {
            (
                comp.sum_we / comp.sum_w,
                comp.sum_wn / comp.sum_w,
                comp.sum_wu / comp.sum_w,
            )
        } else {
            (0.0, 0.0, 0.0)
        };
        let (clon, clat) = enu_to_lonlat(ce, cn);

        let (mr, ma, mh) = comp.max_idx;
        let (mlon, mlat) = to_lonlat(r_centre(mr), a_centre(ma));

        let footprint = footprint_ring(
            &d.mask,
            n_r,
            n_a,
            wrap,
            grid.radius_range[0],
            dr,
            grid.angle_range[0],
            da,
            simplify_tol_m,
            &to_lonlat,
        );

        set.cells.push(StormCell {
            label: rank as u32 + 1,
            max_dbz: comp.max_dbz,
            max_dbz_pos: [mlon, mlat, origin[2] + h_centre(mh)],
            centroid: [clon, clat, origin[2] + cu],
            echo_top_m: origin[2] + grid.height_range[0] + (comp.max_ih as f64 + 1.0) * dh,
            base_m: origin[2] + grid.height_range[0] + comp.min_ih as f64 * dh,
            volume_km3: comp.volume_m3 / 1e9,
            area_km2: d.area_m2 / 1e6,
            max_vil_kg_m2: d.max_vil,
            footprint,
            bbox_enu_m: d.bbox,
        });
    }
    set
}

// ---------------------------------------------------------------------------
// Footprint contour
// ---------------------------------------------------------------------------

/// Trace the outer boundary of a cell's `(radius, azimuth)` column mask into
/// a closed WGS84 ring.
///
/// The boundary follows voxel-column **edges** (the exact outline of the
/// union of grid cells, not an interpolated level set). For a full-circle
/// grid the mask is rotated so the occupied arc is seam-free before tracing
/// (a storm spanning azimuth 359°→1° yields one ring, not two halves); a mask
/// occupying every azimuth (a full annulus — vanishingly rare) falls back to
/// the outer-radius circle, ignoring the inner hole. When several boundary
/// rings exist (pinched shapes), the largest by enclosed ENU area wins; holes
/// are dropped. The ring is simplified (Douglas–Peucker, `tol_m`) and
/// oriented counter-clockwise.
#[allow(clippy::too_many_arguments)]
fn footprint_ring(
    mask: &[bool],
    n_r: usize,
    n_a: usize,
    wrap: bool,
    r0: f64,
    dr: f64,
    a0: f64,
    da: f64,
    tol_m: f64,
    to_lonlat: &dyn Fn(f64, f64) -> (f64, f64),
) -> Vec<[f64; 2]> {
    // Rotate azimuth indices so the occupied region doesn't cross the seam:
    // find an azimuth column with no occupied cell and make it the last one.
    let occupied_col = |a: usize| (0..n_r).any(|r| mask[r * n_a + a]);
    let shift = if wrap {
        match (0..n_a).find(|&a| !occupied_col(a)) {
            // Empty column `gap`: rotate so it lands at shifted index n_a-1,
            // i.e. original index a maps to shifted (a + n_a - 1 - gap) % n_a.
            Some(gap) => (n_a - 1 - gap) % n_a,
            None => {
                // Full annulus: outer-radius circle (inner hole dropped).
                let max_r = (0..n_r)
                    .rev()
                    .find(|&r| (0..n_a).any(|a| mask[r * n_a + a]))
                    .map(|r| r0 + (r as f64 + 1.0) * dr)
                    .unwrap_or(r0);
                let mut ring: Vec<[f64; 2]> = (0..=n_a)
                    .map(|a| {
                        let (lon, lat) = to_lonlat(max_r, a0 + a as f64 * da);
                        [lon, lat]
                    })
                    .collect();
                if let Some(first) = ring.first().copied() {
                    *ring.last_mut().expect("non-empty ring") = first; // exact closure
                }
                return ring;
            }
        }
    } else {
        0
    };
    // shifted column s → original column (s + n_a - shift) % n_a
    let orig_a = |s: usize| (s + n_a - shift) % n_a;
    let at = |r: usize, s: usize| mask[r * n_a + orig_a(s)];

    // Directed boundary edges in (x = radius index, y = shifted azimuth
    // index) corner space, interior on the left (CCW in x/y).
    // Encoded as start-vertex → end-vertex; vertices packed as y * (n_r+1) + x.
    let pack = |x: usize, y: usize| y * (n_r + 1) + x;
    let mut edges_from: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::new();
    let mut edge_count = 0usize;
    for x in 0..n_r {
        for y in 0..n_a {
            if !at(x, y) {
                continue;
            }
            let mut emit = |sx: usize, sy: usize, ex: usize, ey: usize| {
                edges_from
                    .entry(pack(sx, sy))
                    .or_default()
                    .push(pack(ex, ey));
                edge_count += 1;
            };
            // Neighbours in shifted space: outside the array = unoccupied
            // (the seam column is empty by construction, so no wrap checks).
            if y == 0 || !at(x, y - 1) {
                emit(x, y, x + 1, y); // bottom, heading +x
            }
            if x + 1 >= n_r || !at(x + 1, y) {
                emit(x + 1, y, x + 1, y + 1); // right, heading +y
            }
            if y + 1 >= n_a || !at(x, y + 1) {
                emit(x + 1, y + 1, x, y + 1); // top, heading -x
            }
            if x == 0 || !at(x - 1, y) {
                emit(x, y + 1, x, y); // left, heading -y
            }
        }
    }

    // Chain edges into closed rings. At an ambiguous (pinch) vertex with two
    // outgoing edges, prefer the leftmost turn relative to the incoming
    // direction — keeps each ring tightly wound with interior on the left.
    let unpack = |v: usize| ((v % (n_r + 1)) as i64, (v / (n_r + 1)) as i64);
    let mut rings: Vec<Vec<usize>> = Vec::new();
    let mut consumed = 0usize;
    while consumed < edge_count {
        // Take any remaining start vertex.
        let (&start, _) = match edges_from.iter().find(|(_, v)| !v.is_empty()) {
            Some(kv) => kv,
            None => break,
        };
        let mut ring = vec![start];
        let mut current = start;
        let mut dir: Option<(i64, i64)> = None;
        loop {
            let outs = match edges_from.get_mut(&current) {
                Some(o) if !o.is_empty() => o,
                _ => break, // dangling (shouldn't happen on a well-formed mask)
            };
            let next = if outs.len() == 1 {
                outs.remove(0)
            } else {
                // Leftmost turn: maximise the CCW angle from the incoming
                // direction (cross asc, then dot desc ranks left > straight
                // > right > U-turn).
                let (cx, cy) = unpack(current);
                let score = |&cand: &usize| {
                    let (nx, ny) = unpack(cand);
                    let d = (nx - cx, ny - cy);
                    match dir {
                        None => 0i64,
                        Some(p) => {
                            let cross = p.0 * d.1 - p.1 * d.0;
                            let dot = p.0 * d.0 + p.1 * d.1;
                            // left: cross>0 → 3; straight: dot>0 → 2;
                            // right: cross<0 → 1; back: → 0
                            if cross > 0 {
                                3
                            } else if dot > 0 {
                                2
                            } else if cross < 0 {
                                1
                            } else {
                                0
                            }
                        }
                    }
                };
                let best = (0..outs.len())
                    .max_by_key(|i| score(&outs[*i]))
                    .expect("non-empty outs");
                outs.remove(best)
            };
            consumed += 1;
            let (cx, cy) = unpack(current);
            let (nx, ny) = unpack(next);
            dir = Some((nx - cx, ny - cy));
            if next == start {
                rings.push(ring);
                break;
            }
            ring.push(next);
            current = next;
        }
    }
    if rings.is_empty() {
        return Vec::new();
    }

    // Vertex (x, y) → ENU metres (for area + simplification) and lon/lat.
    // The azimuth must be mapped back through the **inverse** of the seam
    // rotation (the same `orig_a` mapping the occupancy lookups use) —
    // `y + shift` would rotate the whole ring around the radar. The modulo
    // is only valid on a full-circle grid (where the angle is periodic);
    // a sector grid never rotates (`shift == 0`), so `y` is already the
    // original boundary there.
    let vertex_polar = |v: usize| {
        let (x, y) = unpack(v);
        let radius = r0 + x as f64 * dr;
        let boundary = if wrap {
            (y as usize + n_a - shift) % n_a
        } else {
            y as usize
        };
        let angle = a0 + boundary as f64 * da;
        (radius, angle)
    };
    let vertex_enu = |v: usize| {
        let (radius, angle) = vertex_polar(v);
        (radius * angle.sin(), radius * angle.cos())
    };

    // Largest |area| ring is the outer boundary; holes are dropped.
    let ring = rings
        .into_iter()
        .max_by(|a, b| {
            let area = |r: &Vec<usize>| shoelace(r.iter().map(|&v| vertex_enu(v))).abs();
            area(a)
                .partial_cmp(&area(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .expect("non-empty rings");

    // Orient CCW in ENU (east/north), per RFC 7946 exterior convention.
    let mut ring = ring;
    if shoelace(ring.iter().map(|&v| vertex_enu(v))) < 0.0 {
        ring.reverse();
    }

    // Simplify in ENU, then materialise lon/lat and close the ring.
    let enu: Vec<(f64, f64)> = ring.iter().map(|&v| vertex_enu(v)).collect();
    let keep = douglas_peucker_closed(&enu, tol_m);
    let mut out: Vec<[f64; 2]> = keep
        .iter()
        .map(|&i| {
            let (radius, angle) = vertex_polar(ring[i]);
            let (lon, lat) = to_lonlat(radius, angle);
            [lon, lat]
        })
        .collect();
    if let Some(&first) = out.first() {
        out.push(first);
    }
    out
}

/// Signed shoelace area of a (not-necessarily-closed) vertex loop.
fn shoelace(points: impl Iterator<Item = (f64, f64)>) -> f64 {
    let pts: Vec<(f64, f64)> = points.collect();
    let n = pts.len();
    if n < 3 {
        return 0.0;
    }
    let mut sum = 0.0;
    for (i, &(x0, y0)) in pts.iter().enumerate() {
        let (x1, y1) = pts[(i + 1) % n];
        sum += x0 * y1 - x1 * y0;
    }
    sum / 2.0
}

/// Douglas–Peucker on a closed loop: anchor the two farthest-apart vertices,
/// simplify each arc, return kept indices (ascending). Always keeps ≥ 3
/// vertices so the ring stays a polygon.
fn douglas_peucker_closed(points: &[(f64, f64)], tol: f64) -> Vec<usize> {
    let n = points.len();
    if n <= 4 {
        return (0..n).collect();
    }
    // Anchors: vertex 0 and the vertex farthest from it.
    let far = (1..n)
        .max_by(|&a, &b| {
            let d = |i: usize| {
                let (x, y) = points[i];
                let (x0, y0) = points[0];
                (x - x0).hypot(y - y0)
            };
            d(a).partial_cmp(&d(b)).unwrap_or(std::cmp::Ordering::Equal)
        })
        .expect("n > 1");
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[far] = true;
    dp_arc(points, 0, far, tol, &mut keep);
    dp_arc_wrapping(points, far, n, tol, &mut keep);
    let kept: Vec<usize> = (0..n).filter(|&i| keep[i]).collect();
    if kept.len() < 3 {
        // Degenerate (collinear loop) — keep a triangle's worth of vertices.
        return vec![0, n / 3, 2 * n / 3];
    }
    kept
}

/// Simplify the open arc `points[i0..=i1]` in place (marks kept indices).
fn dp_arc(points: &[(f64, f64)], i0: usize, i1: usize, tol: f64, keep: &mut [bool]) {
    if i1 <= i0 + 1 {
        return;
    }
    let (ax, ay) = points[i0];
    let (bx, by) = points[i1];
    let (dx, dy) = (bx - ax, by - ay);
    let len = dx.hypot(dy);
    let mut worst = (0usize, 0.0f64);
    for (i, &(px, py)) in points.iter().enumerate().take(i1).skip(i0 + 1) {
        let dist = if len <= f64::EPSILON {
            (px - ax).hypot(py - ay)
        } else {
            ((px - ax) * dy - (py - ay) * dx).abs() / len
        };
        if dist > worst.1 {
            worst = (i, dist);
        }
    }
    if worst.1 > tol {
        keep[worst.0] = true;
        dp_arc(points, i0, worst.0, tol, keep);
        dp_arc(points, worst.0, i1, tol, keep);
    }
}

/// Simplify the arc from `i0` back around to vertex 0 (i.e. `i0..n` plus the
/// implicit wrap to index 0).
fn dp_arc_wrapping(points: &[(f64, f64)], i0: usize, n: usize, tol: f64, keep: &mut [bool]) {
    // Materialise the wrapped arc with the closing vertex appended; indices
    // map back as (i0 + k) % n.
    let arc: Vec<(f64, f64)> = (i0..=n).map(|k| points[k % n]).collect();
    let mut arc_keep = vec![false; arc.len()];
    arc_keep[0] = true;
    *arc_keep.last_mut().expect("non-empty arc") = true;
    dp_arc(&arc, 0, arc.len() - 1, tol, &mut arc_keep);
    for (k, &kept) in arc_keep.iter().enumerate() {
        if kept {
            keep[(i0 + k) % n] = true;
        }
    }
}

// ---------------------------------------------------------------------------
// Tracking
// ---------------------------------------------------------------------------

/// East/north displacement (metres) from `from` to `to` (`[lon, lat]`-ish
/// triples; only the first two components are read), via the equirectangular
/// approximation around the mid-latitude — exact enough at storm-tracking
/// scales (≤ a few hundred km).
fn local_delta_m(from: &[f64; 3], to: &[f64; 3]) -> (f64, f64) {
    let mut dlon = to[0] - from[0];
    // Shortest way around the antimeridian.
    if dlon > 180.0 {
        dlon -= 360.0;
    } else if dlon < -180.0 {
        dlon += 360.0;
    }
    let mid_lat = ((from[1] + to[1]) / 2.0).to_radians();
    let de = dlon.to_radians() * mid_lat.cos() * EARTH_RADIUS_M;
    let dn = (to[1] - from[1]).to_radians() * EARTH_RADIUS_M;
    (de, dn)
}

/// Match cells across consecutive scans into [`Track`]s.
///
/// `history` is ascending in time (the natural [`crate::volume::CellProduct`]
/// order). Matching between each consecutive pair is greedy minimum-cost:
/// the cost is the distance between a previous cell's **predicted** centroid
/// (constant-velocity extrapolation when the track already has a motion
/// estimate) and a current cell's centroid; pairs beyond the gate
/// (`max_speed_ms · Δt + base_gate_m`) are never matched. Unmatched previous
/// cells end their track; unmatched current cells start a new one. A merge or
/// split therefore resolves as "nearest wins" — the losing branch dies or is
/// born — which is the documented v1 behaviour.
pub fn track_cells(history: &[(DateTime<Utc>, Arc<CellSet>)], opts: &TrackingOptions) -> TrackSet {
    struct Build {
        points: Vec<TrackPoint>,
        motion: Option<(f64, f64)>,
        first_label: u32,
    }
    let mut tracks: Vec<Build> = Vec::new();
    // Track index per cell (by position in the scan's `cells`) of the
    // previous scan.
    let mut prev_assign: Vec<usize> = Vec::new();

    let point_of = |set: &CellSet, cell: &StormCell| TrackPoint {
        time: set.time,
        lon: cell.centroid[0],
        lat: cell.centroid[1],
        height_m: cell.centroid[2],
        label: cell.label,
        max_dbz: cell.max_dbz,
    };

    for (scan_idx, (_, set)) in history.iter().enumerate() {
        let mut assign: Vec<Option<usize>> = vec![None; set.cells.len()];

        if scan_idx > 0 {
            let (_, prev_set) = &history[scan_idx - 1];
            let dt = (set.time - prev_set.time).num_milliseconds() as f64 / 1000.0;
            if dt > 0.0 && !prev_set.cells.is_empty() && !set.cells.is_empty() {
                let gate = opts.max_speed_ms * dt + opts.base_gate_m;
                // (cost, prev cell idx, cur cell idx) for every in-gate pair.
                let mut candidates: Vec<(f64, usize, usize)> = Vec::new();
                for (pi, pcell) in prev_set.cells.iter().enumerate() {
                    let track_idx = prev_assign[pi];
                    let motion = tracks[track_idx].motion;
                    for (cj, ccell) in set.cells.iter().enumerate() {
                        let (de, dn) = local_delta_m(&pcell.centroid, &ccell.centroid);
                        let (pe, pn) = match motion {
                            Some((u, v)) => (u * dt, v * dt),
                            None => (0.0, 0.0),
                        };
                        let cost = (de - pe).hypot(dn - pn);
                        if cost <= gate {
                            candidates.push((cost, pi, cj));
                        }
                    }
                }
                candidates.sort_by(|a, b| {
                    a.0.partial_cmp(&b.0)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(a.1.cmp(&b.1))
                        .then(a.2.cmp(&b.2))
                });
                let mut prev_used = vec![false; prev_set.cells.len()];
                for (_, pi, cj) in candidates {
                    if prev_used[pi] || assign[cj].is_some() {
                        continue;
                    }
                    prev_used[pi] = true;
                    let track_idx = prev_assign[pi];
                    let pcell = &prev_set.cells[pi];
                    let ccell = &set.cells[cj];
                    let (de, dn) = local_delta_m(&pcell.centroid, &ccell.centroid);
                    let t = &mut tracks[track_idx];
                    t.points.push(point_of(set, ccell));
                    t.motion = Some((de / dt, dn / dt));
                    assign[cj] = Some(track_idx);
                }
            }
        }

        // Births: every unmatched current cell starts a track.
        for (cj, cell) in set.cells.iter().enumerate() {
            if assign[cj].is_none() {
                assign[cj] = Some(tracks.len());
                tracks.push(Build {
                    points: vec![point_of(set, cell)],
                    motion: None,
                    first_label: cell.label,
                });
            }
        }
        prev_assign = assign.into_iter().map(|a| a.expect("assigned")).collect();
    }

    let mut out: Vec<Track> = tracks
        .into_iter()
        .map(|b| Track {
            id: format!(
                "{}-{}",
                b.points[0].time.format("%Y%m%dT%H%M%SZ"),
                b.first_label
            ),
            points: b.points,
            motion_ms: b.motion,
        })
        .collect();
    out.sort_by(|a, b| {
        (a.points[0].time, a.points[0].label).cmp(&(b.points[0].time, b.points[0].label))
    });
    TrackSet { tracks: out }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Synthetic full-circle grid: 20 radial × 36 azimuth × 10 height cells
    /// over 100 km / 10 km, all background (`NaN`).
    fn empty_grid() -> VoxelGrid {
        let dims = [20, 36, 10];
        VoxelGrid {
            origin_lon: 24.5,
            origin_lat: 60.5,
            origin_height: 100.0,
            dims,
            radius_range: [0.0, 100_000.0],
            angle_range: [0.0, std::f64::consts::TAU],
            height_range: [0.0, 10_000.0],
            values: vec![f32::NAN; dims[0] * dims[1] * dims[2]],
            quantity: "DBZH".into(),
            unit: "dBZ".into(),
        }
    }

    fn fill(
        grid: &mut VoxelGrid,
        r: std::ops::Range<usize>,
        a: &[usize],
        h: std::ops::Range<usize>,
        v: f32,
    ) {
        for i_r in r {
            for &i_a in a {
                for i_h in h.clone() {
                    let idx = grid.index(i_r, i_a, i_h);
                    grid.values[idx] = v;
                }
            }
        }
    }

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap()
    }

    fn opts() -> CellExtractionOptions {
        CellExtractionOptions {
            threshold: 35.0,
            min_volume_km3: 0.0,
            max_cells: 256,
        }
    }

    #[test]
    fn two_separated_blobs_make_two_cells_ordered_by_volume() {
        let mut g = empty_grid();
        // Big blob: radii 4..8, azimuths 9..12, heights 2..6 (echoes 45 dBZ).
        fill(&mut g, 4..8, &[9, 10, 11], 2..6, 45.0);
        // Small blob far away in azimuth: one column, 50 dBZ.
        fill(&mut g, 14..15, &[27], 1..3, 50.0);
        let set = extract_cells(&g, t0(), &opts());
        assert_eq!(set.cells.len(), 2);
        // Label 1 = the larger.
        assert!(set.cells[0].volume_km3 > set.cells[1].volume_km3);
        assert_eq!(set.cells[0].label, 1);
        assert_eq!(set.cells[1].label, 2);
        assert_eq!(set.cells[0].max_dbz, 45.0);
        assert_eq!(set.cells[1].max_dbz, 50.0);
        // Footprints are closed rings.
        for c in &set.cells {
            let f = &c.footprint;
            assert!(f.len() >= 4, "ring has at least a triangle + closure");
            assert_eq!(f.first(), f.last(), "ring closed");
        }
    }

    #[test]
    fn azimuth_seam_blob_is_one_cell_with_one_ring() {
        let mut g = empty_grid();
        // Blob spanning the 0/2π seam: azimuth columns 34, 35, 0, 1.
        fill(&mut g, 5..9, &[34, 35, 0, 1], 2..5, 42.0);
        let set = extract_cells(&g, t0(), &opts());
        assert_eq!(set.cells.len(), 1, "seam blob must not split");
        let cell = &set.cells[0];
        assert_eq!(cell.footprint.first(), cell.footprint.last());
        // Every ring vertex must sit where the blob actually is: azimuth
        // boundaries 340°–20° (columns 34..1 of 36), radius boundaries
        // 25–45 km (rows 5..9). A wrong seam-rotation inverse rotates the
        // whole ring around the radar — these bounds catch any such
        // constant-angle offset.
        for v in &cell.footprint {
            let de = (v[0] - g.origin_lon).to_radians()
                * g.origin_lat.to_radians().cos()
                * crate::geo::EARTH_RADIUS_M;
            let dn = (v[1] - g.origin_lat).to_radians() * crate::geo::EARTH_RADIUS_M;
            let bearing = de.atan2(dn).to_degrees().rem_euclid(360.0);
            let from_north = bearing.min(360.0 - bearing);
            assert!(
                from_north <= 20.5,
                "vertex bearing {bearing}° outside the blob's 340°–20° span"
            );
            let ground = de.hypot(dn);
            assert!(
                (24_500.0..=45_500.0).contains(&ground),
                "vertex ground range {ground} m outside the blob's 25–45 km span"
            );
        }
        // Centroid bearing ≈ north (azimuth ≈ 0): the centroid must sit
        // north of the origin and essentially on its meridian.
        assert!(cell.centroid[1] > g.origin_lat);
        assert!((cell.centroid[0] - g.origin_lon).abs() < 0.2);
        // 4 azimuth columns × 3 height cells per radius ring.
        let da = std::f64::consts::TAU / 36.0;
        let (dr, dh) = (5_000.0, 1_000.0);
        let expected_vol: f64 = (5..9)
            .map(|i_r| {
                let rc = (i_r as f64 + 0.5) * dr;
                dr * rc * da * dh * 4.0 * 3.0
            })
            .sum::<f64>()
            / 1e9;
        assert!(
            (cell.volume_km3 - expected_vol).abs() / expected_vol < 1e-9,
            "volume {} vs expected {}",
            cell.volume_km3,
            expected_vol
        );
    }

    #[test]
    fn min_volume_filter_and_max_cells_cap() {
        let mut g = empty_grid();
        fill(&mut g, 4..8, &[9, 10, 11], 2..6, 45.0); // big
        fill(&mut g, 14..15, &[27], 1..2, 50.0); // tiny (1 column, 1 cell high)
        let set = extract_cells(
            &g,
            t0(),
            &CellExtractionOptions {
                threshold: 35.0,
                // The tiny blob is one voxel ≈ 63 km³ at these coarse dims;
                // the big one is ≈ 1200 km³.
                min_volume_km3: 100.0,
                max_cells: 256,
            },
        );
        assert_eq!(set.cells.len(), 1);
        assert_eq!(set.cells[0].max_dbz, 45.0);

        let capped = extract_cells(
            &g,
            t0(),
            &CellExtractionOptions {
                threshold: 35.0,
                min_volume_km3: 0.0,
                max_cells: 1,
            },
        );
        assert_eq!(capped.cells.len(), 1, "max_cells keeps the largest");
        assert_eq!(capped.cells[0].max_dbz, 45.0);
    }

    #[test]
    fn single_voxel_cell_attributes_are_hand_checkable() {
        let mut g = empty_grid();
        let (i_r, i_a, i_h) = (10, 9, 4); // azimuth column 9 → bearing 95°E-ish
        let idx = g.index(i_r, i_a, i_h);
        g.values[idx] = 40.0;
        let set = extract_cells(&g, t0(), &opts());
        assert_eq!(set.cells.len(), 1);
        let c = &set.cells[0];

        let dr = 5_000.0;
        let da = std::f64::consts::TAU / 36.0;
        let dh = 1_000.0;
        let rc = (i_r as f64 + 0.5) * dr; // 52.5 km
        assert!((c.volume_km3 - dr * rc * da * dh / 1e9).abs() < 1e-12);
        assert!((c.area_km2 - dr * rc * da / 1e6).abs() < 1e-12);
        // Echo top/base: heights are MSL (origin at 100 m).
        assert!((c.base_m - (100.0 + 4_000.0)).abs() < 1e-9);
        assert!((c.echo_top_m - (100.0 + 5_000.0)).abs() < 1e-9);
        // VIL: one 1 km layer at 40 dBZ.
        let z = 10f64.powf(4.0);
        let expected_vil = 3.44e-6 * z.powf(4.0 / 7.0) * dh;
        assert!((c.max_vil_kg_m2 - expected_vil).abs() / expected_vil < 1e-12);
        // Centroid = the voxel centre, azimuth (9.5/36)·360 = 95°.
        let (elon, elat) = destination_point(24.5, 60.5, rc, 95.0);
        assert!((c.centroid[0] - elon).abs() < 1e-9);
        assert!((c.centroid[1] - elat).abs() < 1e-9);
        assert!((c.centroid[2] - (100.0 + 4_500.0)).abs() < 1e-9);
        assert_eq!(c.max_dbz, 40.0);
        // Footprint ring should enclose the centroid's neighbourhood: all
        // four ring corners are distinct and the ring is closed.
        assert!(c.footprint.len() >= 5);
        assert_eq!(c.footprint.first(), c.footprint.last());
        // ENU bbox: radius span [50, 55] km, height span [4, 5] km.
        assert!((c.bbox_enu_m[2] - 4_000.0).abs() < 1e-9);
        assert!((c.bbox_enu_m[5] - 5_000.0).abs() < 1e-9);
        let diag_e = c.bbox_enu_m[3] - c.bbox_enu_m[0];
        assert!(diag_e > 0.0 && diag_e < 12_000.0);
    }

    #[test]
    fn vil_caps_hail_at_56_dbz() {
        let mut g = empty_grid();
        let idx = g.index(5, 0, 0);
        g.values[idx] = 70.0; // hail spike
        let set = extract_cells(&g, t0(), &opts());
        let z_capped = 10f64.powf(5.6);
        let expected = 3.44e-6 * z_capped.powf(4.0 / 7.0) * 1_000.0;
        assert!((set.cells[0].max_vil_kg_m2 - expected).abs() / expected < 1e-12);
    }

    #[test]
    fn no_echo_grid_yields_empty_set() {
        let g = empty_grid();
        let set = extract_cells(&g, t0(), &opts());
        assert!(set.cells.is_empty());
        // Clear-air floor (-32 dBZ) is also background at any sane threshold.
        let mut g2 = empty_grid();
        g2.values.fill(crate::volume::NO_ECHO_FLOOR_DBZ);
        assert!(extract_cells(&g2, t0(), &opts()).cells.is_empty());
    }

    // --- tracking -----------------------------------------------------------

    /// One-cell set with the cell centred `km_east` km east of the origin.
    fn set_at(time: DateTime<Utc>, km_east: f64, label: u32) -> Arc<CellSet> {
        let (lon, lat) = destination_point(24.5, 60.5, km_east * 1_000.0, 90.0);
        Arc::new(CellSet {
            time,
            quantity: "DBZH".into(),
            threshold: 35.0,
            origin: [24.5, 60.5, 100.0],
            cells: vec![StormCell {
                label,
                max_dbz: 45.0,
                max_dbz_pos: [lon, lat, 5_000.0],
                centroid: [lon, lat, 5_000.0],
                echo_top_m: 8_000.0,
                base_m: 1_000.0,
                volume_km3: 50.0,
                area_km2: 25.0,
                max_vil_kg_m2: 10.0,
                footprint: vec![[lon, lat]; 4],
                bbox_enu_m: [0.0; 6],
            }],
        })
    }

    fn minutes(m: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap() + chrono::Duration::minutes(m)
    }

    #[test]
    fn moving_blob_tracks_with_expected_motion() {
        // 10 km east per 5 min ⇒ u ≈ 33.3 m/s, v ≈ 0.
        let history = vec![
            (minutes(0), set_at(minutes(0), 0.0, 1)),
            (minutes(5), set_at(minutes(5), 10.0, 1)),
            (minutes(10), set_at(minutes(10), 20.0, 1)),
        ];
        let tracks = track_cells(&history, &TrackingOptions::default());
        assert_eq!(tracks.tracks.len(), 1);
        let t = &tracks.tracks[0];
        assert_eq!(t.points.len(), 3);
        let (u, v) = t.motion_ms.expect("motion after a matched step");
        assert!((u - 10_000.0 / 300.0).abs() < 0.5, "u = {u}");
        assert!(v.abs() < 0.5, "v = {v}");
        let (speed, dir) = t.speed_direction().unwrap();
        assert!((speed - 33.3).abs() < 0.5);
        assert!((dir - 90.0).abs() < 1.0, "moving east, dir = {dir}");
        assert_eq!(t.id, format!("{}-1", minutes(0).format("%Y%m%dT%H%M%SZ")));
        // track_for joins a scan's cell back to its track.
        assert!(tracks.track_for(minutes(5), 1).is_some());
        assert!(tracks.track_for(minutes(5), 2).is_none());
    }

    #[test]
    fn gate_rejects_teleporting_blob_and_births_a_new_track() {
        // 80 km in 5 min ⇒ 266 m/s ≫ the 40 m/s gate.
        let history = vec![
            (minutes(0), set_at(minutes(0), 0.0, 1)),
            (minutes(5), set_at(minutes(5), 80.0, 1)),
        ];
        let tracks = track_cells(&history, &TrackingOptions::default());
        assert_eq!(tracks.tracks.len(), 2, "no match across the gate");
        assert!(tracks.tracks.iter().all(|t| t.points.len() == 1));
        assert!(tracks.tracks.iter().all(|t| t.motion_ms.is_none()));
    }

    #[test]
    fn death_and_empty_scan_are_handled() {
        let history = vec![
            (minutes(0), set_at(minutes(0), 0.0, 1)),
            (
                minutes(5),
                Arc::new(CellSet::empty(
                    minutes(5),
                    "DBZH",
                    35.0,
                    [24.5, 60.5, 100.0],
                )),
            ),
            (minutes(10), set_at(minutes(10), 0.0, 1)),
        ];
        let tracks = track_cells(&history, &TrackingOptions::default());
        // The original dies at the empty scan; the reappearance is a birth.
        assert_eq!(tracks.tracks.len(), 2);
        assert!(tracks.tracks.iter().all(|t| t.points.len() == 1));
    }

    #[test]
    fn merge_resolves_as_nearest_wins() {
        // Two cells converge on one: the nearer keeps the track, the other dies.
        let two = |t: DateTime<Utc>| {
            let mut s = (*set_at(t, 0.0, 1)).clone();
            let far = set_at(t, 8.0, 2);
            s.cells.push(far.cells[0].clone());
            Arc::new(s)
        };
        let history = vec![
            (minutes(0), two(minutes(0))),
            (minutes(5), set_at(minutes(5), 1.0, 1)), // near the first cell
        ];
        let tracks = track_cells(&history, &TrackingOptions::default());
        assert_eq!(tracks.tracks.len(), 2);
        let continued: Vec<_> = tracks
            .tracks
            .iter()
            .filter(|t| t.points.len() == 2)
            .collect();
        assert_eq!(continued.len(), 1, "exactly one track continues");
        assert_eq!(continued[0].points[0].label, 1, "the nearer cell wins");
    }

    #[test]
    fn footprint_ring_is_ccw() {
        let mut g = empty_grid();
        fill(&mut g, 4..8, &[9, 10, 11], 2..6, 45.0);
        let set = extract_cells(&g, t0(), &opts());
        let ring = &set.cells[0].footprint;
        // Shoelace in lon/lat (a fine orientation proxy at this scale away
        // from the antimeridian).
        let mut area = 0.0;
        for w in ring.windows(2) {
            area += w[0][0] * w[1][1] - w[1][0] * w[0][1];
        }
        assert!(area > 0.0, "exterior ring must be counter-clockwise");
    }
}
