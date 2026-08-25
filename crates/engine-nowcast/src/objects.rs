//! Object-based verification primitives (#541 V2.1): 2D cell segmentation,
//! forecast↔observed cell matching, and growth/decay classification.
//!
//! Modeled on the object-based framework of Ritvanen et al. (GMD 18,
//! 1851–1878, 2025): cells are threshold contours on the 2D composite
//! (default 35 dBZ), forecast and observed cells are matched per lead time
//! with a Hungarian assignment under a centroid-distance gate, and observed
//! cells are stratified into growing/decaying by the sign of their
//! volume-proxy derivative at forecast creation time — the stratification
//! that exposes how pure advection misses growth and decay.
//!
//! Like `motion`/`advect`/`skill`, this module is dependency-free pure
//! functions over [`Grid`]s; the 3D `ds_core::cells` machinery stays on the
//! PVOL side.

use crate::Grid;

/// One segmented 2D cell: a connected component of pixels ≥ threshold.
#[derive(Debug, Clone)]
pub struct CellBlob {
    /// Intensity-weighted centroid, pixel coordinates (x, y).
    pub centroid: (f32, f32),
    /// Pixel count.
    pub area: usize,
    /// Sum of (value − threshold) over member pixels — the "volume rain
    /// rate" proxy used for growth/decay classification and ranking.
    pub volume: f32,
    /// Maximum value inside the cell.
    pub max_value: f32,
}

/// Segment `grid` into cells: 8-connected components of pixels with
/// `value ≥ threshold`, keeping components of at least `min_area` pixels.
pub fn segment_cells(grid: &Grid, threshold: f32, min_area: usize) -> Vec<CellBlob> {
    segment_cells_labeled(grid, threshold, min_area).0
}

/// Like [`segment_cells`], also returning a per-pixel label map: `0` =
/// no retained cell, `i+1` = member of the i-th returned blob. Used by the
/// per-cell tendency application (#546), which needs cell footprint
/// masks, not just centroids.
pub fn segment_cells_labeled(
    grid: &Grid,
    threshold: f32,
    min_area: usize,
) -> (Vec<CellBlob>, Vec<u32>) {
    let (w, h) = (grid.width, grid.height);
    let mut labels = vec![0u32; w * h];
    let mut cells: Vec<CellBlob> = Vec::new();
    let mut retained: Vec<u32> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut next_label = 0u32;

    for start in 0..w * h {
        let v = grid.data[start];
        if labels[start] != 0 || !(v.is_finite() && v >= threshold) {
            continue;
        }
        next_label += 1;
        labels[start] = next_label;
        stack.push(start);
        let (mut area, mut volume, mut max_value) = (0usize, 0f32, f32::MIN);
        let (mut wx, mut wy, mut wsum) = (0f64, 0f64, 0f64);

        while let Some(i) = stack.pop() {
            let val = grid.data[i];
            area += 1;
            volume += val - threshold;
            max_value = max_value.max(val);
            let (x, y) = (i % w, i / w);
            // Weight the centroid by exceedance so the core dominates.
            let wgt = (val - threshold).max(0.0) as f64 + 1e-6;
            wx += (x as f64 + 0.5) * wgt;
            wy += (y as f64 + 0.5) * wgt;
            wsum += wgt;

            let x0 = x.saturating_sub(1);
            let y0 = y.saturating_sub(1);
            for ny in y0..(y + 2).min(h) {
                for nx in x0..(x + 2).min(w) {
                    let j = ny * w + nx;
                    if labels[j] == 0 {
                        let nv = grid.data[j];
                        if nv.is_finite() && nv >= threshold {
                            labels[j] = next_label;
                            stack.push(j);
                        }
                    }
                }
            }
        }

        if area >= min_area {
            cells.push(CellBlob {
                centroid: ((wx / wsum) as f32, (wy / wsum) as f32),
                area,
                volume,
                max_value,
            });
            retained.push(next_label);
        }
    }
    // Compact retained raw labels to 1-based blob indices; drop the rest.
    let mut remap = vec![0u32; next_label as usize + 1];
    for (i, &raw) in retained.iter().enumerate() {
        remap[raw as usize] = i as u32 + 1;
    }
    for l in labels.iter_mut() {
        *l = remap[*l as usize];
    }
    (cells, labels)
}

/// Per-axis pixel scale for distance computations. A regular lat/lon grid
/// is anisotropic away from the equator: the east–west span carries a
/// `cos(lat)` factor the north–south span does not (at 65°N the y-axis
/// covers ~2.4× more km per pixel than the x-axis). Distances and gates are
/// computed in the scale's unit — km for a real grid, or pass `UNIT` to work
/// in raw pixels (tests, isotropic grids).
#[derive(Debug, Clone, Copy)]
pub struct PixelScale {
    pub x: f32,
    pub y: f32,
}

impl PixelScale {
    /// Identity scale: distances and gates are in pixels.
    pub const UNIT: PixelScale = PixelScale { x: 1.0, y: 1.0 };

    #[inline]
    /// Anisotropy-aware distance between two pixel coordinates, in the
    /// scale's unit. `pub(crate)` so the tracker measures path length the
    /// same way matching measures gates — a second hand-rolled copy is how
    /// this kind of thing drifts.
    pub(crate) fn distance(&self, a: (f32, f32), b: (f32, f32)) -> f32 {
        let dx = (a.0 - b.0) * self.x;
        let dy = (a.1 - b.1) * self.y;
        (dx * dx + dy * dy).sqrt()
    }
}

/// Match two cell sets by centroid distance with a hard gate (in the units
/// of `scale` — km for a real grid), minimizing total matched distance
/// (Hungarian assignment). Returns `(index_a, index_b)` pairs; unmatched
/// cells in either set are simply absent from the result.
pub fn match_cells(
    a: &[CellBlob],
    b: &[CellBlob],
    scale: PixelScale,
    gate: f32,
) -> Vec<(usize, usize)> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    // Square cost matrix padded with the forbidden cost; assignments at or
    // above `forbidden` are dropped afterwards, which is how the gate and
    // the padding both work.
    let n = a.len().max(b.len());
    let forbidden = gate * 10.0 + 1e6;
    let mut cost = vec![forbidden; n * n];
    for (i, ca) in a.iter().enumerate() {
        for (j, cb) in b.iter().enumerate() {
            let d = scale.distance(ca.centroid, cb.centroid);
            if d <= gate {
                cost[i * n + j] = d;
            }
        }
    }
    hungarian(&cost, n)
        .into_iter()
        .enumerate()
        .filter(|&(row, col)| row < a.len() && col < b.len() && cost[row * n + col] < forbidden)
        .collect()
}

/// Classic O(n³) Hungarian algorithm (Kuhn–Munkres, potentials + augmenting
/// paths). Returns `assignment[row] = col`. Small n (cells per frame), so
/// clarity beats cleverness.
fn hungarian(cost: &[f32], n: usize) -> Vec<usize> {
    // 1-indexed potentials formulation (standard competitive-programming
    // form, adapted from e2e-verified references).
    let inf = f32::INFINITY;
    let mut u = vec![0f32; n + 1];
    let mut v = vec![0f32; n + 1];
    let mut p = vec![0usize; n + 1]; // p[col] = row matched to col
    let mut way = vec![0usize; n + 1];

    for i in 1..=n {
        p[0] = i;
        let mut j0 = 0usize;
        let mut minv = vec![inf; n + 1];
        let mut used = vec![false; n + 1];
        loop {
            used[j0] = true;
            let i0 = p[j0];
            let mut delta = inf;
            let mut j1 = 0usize;
            for j in 1..=n {
                if used[j] {
                    continue;
                }
                let cur = cost[(i0 - 1) * n + (j - 1)] - u[i0] - v[j];
                if cur < minv[j] {
                    minv[j] = cur;
                    way[j] = j0;
                }
                if minv[j] < delta {
                    delta = minv[j];
                    j1 = j;
                }
            }
            for j in 0..=n {
                if used[j] {
                    u[p[j]] += delta;
                    v[j] -= delta;
                } else {
                    minv[j] -= delta;
                }
            }
            j0 = j1;
            if p[j0] == 0 {
                break;
            }
        }
        loop {
            let j1 = way[j0];
            p[j0] = p[j1];
            j0 = j1;
            if j0 == 0 {
                break;
            }
        }
    }

    let mut assignment = vec![usize::MAX; n];
    for j in 1..=n {
        if p[j] != 0 {
            assignment[p[j] - 1] = j - 1;
        }
    }
    assignment
}

/// Growth/decay class of an observed cell at forecast creation time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrowthClass {
    Growing,
    Decaying,
    /// No matched predecessor in the previous frame (newborn or fast mover).
    Unknown,
}

/// Classify `current` cells as growing/decaying from the sign of the
/// volume-proxy change against their matched predecessor in `previous`
/// (Ritvanen et al. 2025 use the volume-rain-rate derivative at creation
/// time). Returns one class per `current` cell.
pub fn classify_growth(
    previous: &[CellBlob],
    current: &[CellBlob],
    scale: PixelScale,
    gate: f32,
) -> Vec<GrowthClass> {
    let mut classes = vec![GrowthClass::Unknown; current.len()];
    for (pi, ci) in match_cells(previous, current, scale, gate) {
        classes[ci] = if current[ci].volume >= previous[pi].volume {
            GrowthClass::Growing
        } else {
            GrowthClass::Decaying
        };
    }
    classes
}

/// Object-level contingency for one (lead, stratum): forecast cells matched
/// to observed cells are hits; unmatched forecast cells are false alarms;
/// unmatched observed cells are misses.
#[derive(Debug, Clone, Copy, Default)]
pub struct ObjectScores {
    pub hits: u64,
    pub misses: u64,
    pub false_alarms: u64,
    /// Sum of matched centroid distances, in the units of the
    /// [`PixelScale`] used for matching (km for a real grid) — divide by
    /// `hits` for the mean location error.
    pub centroid_error: f64,
}

impl ObjectScores {
    pub fn pod(&self) -> Option<f64> {
        let den = self.hits + self.misses;
        (den > 0).then(|| self.hits as f64 / den as f64)
    }

    pub fn far(&self) -> Option<f64> {
        let den = self.hits + self.false_alarms;
        (den > 0).then(|| self.false_alarms as f64 / den as f64)
    }

    pub fn csi(&self) -> Option<f64> {
        let den = self.hits + self.misses + self.false_alarms;
        (den > 0).then(|| self.hits as f64 / den as f64)
    }

    pub fn mean_centroid_error(&self) -> Option<f64> {
        (self.hits > 0).then(|| self.centroid_error / self.hits as f64)
    }

    pub fn merge(&mut self, other: &ObjectScores) {
        self.hits += other.hits;
        self.misses += other.misses;
        self.false_alarms += other.false_alarms;
        self.centroid_error += other.centroid_error;
    }
}

/// Score forecast cells against observed cells (one lead time). When
/// `observed_classes` is given (from [`classify_growth`] on the observation
/// side at forecast creation), misses and hits are attributed to the
/// returned per-class scores as well.
///
/// **Metric caveat**: false alarms only enter the overall score — a
/// spurious forecast cell has no observed class to attribute to — so the
/// per-class scores carry hits and misses only. Report them as
/// [`ObjectScores::pod`]; their `csi()` would numerically equal POD and
/// must not be presented next to the overall CSI as the same metric.
pub fn score_objects(
    forecast: &[CellBlob],
    observed: &[CellBlob],
    observed_classes: Option<&[GrowthClass]>,
    scale: PixelScale,
    gate: f32,
) -> (ObjectScores, ObjectScores, ObjectScores) {
    let matches = match_cells(forecast, observed, scale, gate);
    let mut overall = ObjectScores::default();
    let mut growing = ObjectScores::default();
    let mut decaying = ObjectScores::default();

    let mut observed_hit = vec![false; observed.len()];
    for &(fi, oi) in &matches {
        observed_hit[oi] = true;
        let err = scale.distance(forecast[fi].centroid, observed[oi].centroid) as f64;
        overall.hits += 1;
        overall.centroid_error += err;
        if let Some(classes) = observed_classes {
            match classes[oi] {
                GrowthClass::Growing => {
                    growing.hits += 1;
                    growing.centroid_error += err;
                }
                GrowthClass::Decaying => {
                    decaying.hits += 1;
                    decaying.centroid_error += err;
                }
                GrowthClass::Unknown => {}
            }
        }
    }
    overall.false_alarms += (forecast.len() - matches.len()) as u64;
    for (oi, hit) in observed_hit.iter().enumerate() {
        if !hit {
            overall.misses += 1;
            if let Some(classes) = observed_classes {
                match classes[oi] {
                    GrowthClass::Growing => growing.misses += 1,
                    GrowthClass::Decaying => decaying.misses += 1,
                    GrowthClass::Unknown => {}
                }
            }
        }
    }
    (overall, growing, decaying)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid_with_discs(w: usize, h: usize, discs: &[(f32, f32, f32, f32)]) -> Grid {
        // discs: (cx, cy, r, value)
        let mut data = vec![0.0f32; w * h];
        for (i, cell) in data.iter_mut().enumerate() {
            let (x, y) = ((i % w) as f32 + 0.5, (i / w) as f32 + 0.5);
            for &(cx, cy, r, v) in discs {
                if (x - cx).powi(2) + (y - cy).powi(2) <= r * r {
                    *cell = (*cell).max(v);
                }
            }
        }
        Grid::new(w, h, data)
    }

    #[test]
    fn segments_distinct_cells_with_centroids_and_min_area() {
        let g = grid_with_discs(
            200,
            120,
            &[(40.0, 40.0, 10.0, 45.0), (150.0, 80.0, 6.0, 40.0)],
        );
        let cells = segment_cells(&g, 35.0, 5);
        assert_eq!(cells.len(), 2);
        let mut byx = cells.clone();
        byx.sort_by(|a, b| a.centroid.0.total_cmp(&b.centroid.0));
        assert!((byx[0].centroid.0 - 40.0).abs() < 1.5);
        assert!((byx[0].centroid.1 - 40.0).abs() < 1.5);
        assert!((byx[1].centroid.0 - 150.0).abs() < 1.5);
        assert!(byx[0].area > byx[1].area);

        // A speck below min_area disappears.
        let speck = grid_with_discs(64, 64, &[(30.0, 30.0, 1.0, 50.0)]);
        assert!(segment_cells(&speck, 35.0, 8).is_empty());
    }

    #[test]
    fn label_map_is_one_based_compacted_and_drops_specks() {
        // The label-map contract is load-bearing for the growth/decay
        // tendency application (label k ⇔ blobs[k-1]): raw component labels
        // are compacted to 1-based retained-blob indices in creation
        // (row-major first-encounter) order; background and sub-min_area
        // components are 0. An off-by-one here silently applies one cell's
        // tendency to another's pixels.
        let g = grid_with_discs(
            60,
            30,
            &[
                (10.0, 10.0, 4.0, 45.0), // blob 1 (encountered first)
                (40.0, 20.0, 3.0, 40.0), // blob 2
                (55.0, 5.0, 0.8, 50.0),  // speck, dropped by min_area
            ],
        );
        let (blobs, labels) = segment_cells_labeled(&g, 35.0, 5);
        assert_eq!(blobs.len(), 2);
        assert_eq!(labels.len(), 60 * 30);
        // Every pixel of blobs[k] carries label k+1; the speck and the
        // background are 0.
        for (i, &l) in labels.iter().enumerate() {
            let (x, y) = ((i % 60) as f32 + 0.5, (i / 60) as f32 + 0.5);
            let expect = if (x - 10.0).powi(2) + (y - 10.0).powi(2) <= 16.0 {
                1
            } else if (x - 40.0).powi(2) + (y - 20.0).powi(2) <= 9.0 {
                2
            } else {
                0 // background AND the dropped speck
            };
            assert_eq!(l, expect, "pixel ({x}, {y})");
        }
        // Consistency with the blob list: label areas match blob areas and
        // blobs come back in creation order.
        assert!(blobs[0].centroid.0 < blobs[1].centroid.0);
        for (k, b) in blobs.iter().enumerate() {
            let count = labels.iter().filter(|&&l| l == (k + 1) as u32).count();
            assert_eq!(count, b.area);
        }
    }

    #[test]
    fn matching_respects_gate_and_prefers_nearest() {
        let a = grid_with_discs(200, 100, &[(50.0, 50.0, 8.0, 45.0)]);
        let b = grid_with_discs(
            200,
            100,
            &[(58.0, 50.0, 8.0, 45.0), (150.0, 50.0, 8.0, 45.0)],
        );
        let ca = segment_cells(&a, 35.0, 5);
        let cb = segment_cells(&b, 35.0, 5);
        let m = match_cells(&ca, &cb, PixelScale::UNIT, 20.0);
        assert_eq!(m.len(), 1, "only the near cell is inside the gate");
        let (_, bi) = m[0];
        assert!((cb[bi].centroid.0 - 58.0).abs() < 1.5);
    }

    #[test]
    fn anisotropic_scale_gates_the_y_axis_correctly() {
        // Two cells 10 px apart in y. With y = 2 km/px that is 20 km: inside
        // a 25 km gate, outside a 15 km gate — an isotropic 1 km/px scale
        // would wrongly accept the latter.
        let a = grid_with_discs(100, 100, &[(50.0, 40.0, 6.0, 45.0)]);
        let b = grid_with_discs(100, 100, &[(50.0, 50.0, 6.0, 45.0)]);
        let ca = segment_cells(&a, 35.0, 5);
        let cb = segment_cells(&b, 35.0, 5);
        let scale = PixelScale { x: 1.0, y: 2.0 };
        assert_eq!(match_cells(&ca, &cb, scale, 25.0).len(), 1);
        assert_eq!(match_cells(&ca, &cb, scale, 15.0).len(), 0);
        assert_eq!(match_cells(&ca, &cb, PixelScale::UNIT, 15.0).len(), 1);
    }

    #[test]
    fn hungarian_finds_globally_optimal_pairing() {
        // The classic greedy-fails swap, on a vertical line (gate 15 px):
        //   f0 (60,50): 5 px from o0 (60,45), 6 px from o1 (60,56)
        //   f1 (60,38.5): 6.5 px from o0, 17.5 px (gated out) from o1
        // Greedy nearest-first pairs f0–o0 and strands f1; the optimal
        // assignment pairs f0–o1 and f1–o0, matching both.
        let f = grid_with_discs(
            120,
            100,
            &[(60.0, 50.0, 3.0, 45.0), (60.0, 38.5, 3.0, 45.0)],
        );
        let o = grid_with_discs(
            120,
            100,
            &[(60.0, 45.0, 3.0, 45.0), (60.0, 56.0, 3.0, 45.0)],
        );
        let cf = segment_cells(&f, 35.0, 5);
        let co = segment_cells(&o, 35.0, 5);
        assert_eq!((cf.len(), co.len()), (2, 2), "discs must not merge");
        let m = match_cells(&cf, &co, PixelScale::UNIT, 15.0);
        assert_eq!(m.len(), 2, "optimal assignment matches both");
    }

    #[test]
    fn growth_classification_by_volume_change() {
        let prev = grid_with_discs(
            200,
            100,
            &[(50.0, 50.0, 6.0, 40.0), (150.0, 50.0, 10.0, 50.0)],
        );
        let cur = grid_with_discs(
            200,
            100,
            &[(54.0, 50.0, 9.0, 46.0), (154.0, 50.0, 6.0, 42.0)],
        );
        let cp = segment_cells(&prev, 35.0, 5);
        let cc = segment_cells(&cur, 35.0, 5);
        let classes = classify_growth(&cp, &cc, PixelScale::UNIT, 20.0);
        let mut by_x: Vec<(f32, GrowthClass)> = cc
            .iter()
            .zip(&classes)
            .map(|(c, k)| (c.centroid.0, *k))
            .collect();
        by_x.sort_by(|a, b| a.0.total_cmp(&b.0));
        assert_eq!(by_x[0].1, GrowthClass::Growing);
        assert_eq!(by_x[1].1, GrowthClass::Decaying);
    }

    #[test]
    fn object_scores_attribute_hits_misses_and_false_alarms() {
        let observed = grid_with_discs(
            300,
            100,
            &[(50.0, 50.0, 8.0, 45.0), (150.0, 50.0, 8.0, 45.0)],
        );
        // Forecast: hits the first cell 6 px off, misses the second, and
        // invents a third far away.
        let forecast = grid_with_discs(
            300,
            100,
            &[(56.0, 50.0, 8.0, 45.0), (250.0, 50.0, 8.0, 45.0)],
        );
        let co = segment_cells(&observed, 35.0, 5);
        let cf = segment_cells(&forecast, 35.0, 5);
        let classes = vec![GrowthClass::Growing, GrowthClass::Decaying];
        let (overall, growing, decaying) =
            score_objects(&cf, &co, Some(&classes), PixelScale::UNIT, 20.0);
        assert_eq!(
            (overall.hits, overall.misses, overall.false_alarms),
            (1, 1, 1)
        );
        assert_eq!(overall.csi(), Some(1.0 / 3.0));
        let err = overall.mean_centroid_error().unwrap();
        assert!((err - 6.0).abs() < 1.5, "centroid error ~6 px, got {err}");
        assert_eq!((growing.hits, growing.misses), (1, 0));
        assert_eq!((decaying.hits, decaying.misses), (0, 1));
    }
}
