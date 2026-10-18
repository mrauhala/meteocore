//! A static 3-D k-d tree over unit vectors on the sphere, for the k nearest
//! stations within a search radius.
//!
//! Stations are points on the unit sphere, so the straight-line (chord)
//! distance between two of them grows monotonically with their great-circle
//! distance: the nearest by chord are the nearest on the Earth. Working in
//! 3-D keeps the antimeridian and the poles ordinary places, with no
//! longitude wrapping and no shrinking meridians to special-case.

/// A balanced k-d tree stored implicitly in one array: the node of the range
/// `lo..hi` is its middle element, its children the halves either side.
pub(crate) struct KdTree {
    /// Points with their index in the caller's list.
    items: Vec<([f64; 3], u32)>,
    /// The splitting axis of the node at each position.
    axes: Vec<u8>,
}

impl KdTree {
    /// Build over `points`, in O(n log n).
    pub(crate) fn new(points: &[[f64; 3]]) -> Self {
        let mut items: Vec<([f64; 3], u32)> = points
            .iter()
            .enumerate()
            .map(|(i, &p)| (p, i as u32))
            .collect();
        let mut axes = vec![0u8; items.len()];
        build(&mut items, &mut axes, 0);
        Self { items, axes }
    }

    /// The up to `k` points nearest `q` whose squared chord distance is at
    /// most `max_d2`, ascending by distance and then by index, into `out`
    /// (cleared first) as `(squared chord distance, index)`. Ties go to the
    /// lower index, so equal layouts give equal answers.
    pub(crate) fn nearest(&self, q: [f64; 3], k: usize, max_d2: f64, out: &mut Vec<(f64, u32)>) {
        out.clear();
        if k == 0 {
            return;
        }
        self.search(0, self.items.len(), q, k, max_d2, out);
    }

    fn search(
        &self,
        lo: usize,
        hi: usize,
        q: [f64; 3],
        k: usize,
        max_d2: f64,
        out: &mut Vec<(f64, u32)>,
    ) {
        if lo >= hi {
            return;
        }
        let mid = lo + (hi - lo) / 2;
        let (p, idx) = self.items[mid];
        let d2 = dist2(p, q);
        if d2 <= limit(out, k, max_d2) {
            insert(out, k, (d2, idx));
        }
        let axis = self.axes[mid] as usize;
        let diff = q[axis] - p[axis];
        let (near, far) = if diff < 0.0 {
            ((lo, mid), (mid + 1, hi))
        } else {
            ((mid + 1, hi), (lo, mid))
        };
        self.search(near.0, near.1, q, k, max_d2, out);
        // `<=`: a point exactly on the plane may tie the current worst.
        if diff * diff <= limit(out, k, max_d2) {
            self.search(far.0, far.1, q, k, max_d2, out);
        }
    }
}

fn dist2(a: [f64; 3], b: [f64; 3]) -> f64 {
    let (x, y, z) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
    x * x + y * y + z * z
}

/// The squared distance a candidate must not exceed: the worst kept one
/// once `k` are kept, the search radius before.
fn limit(out: &[(f64, u32)], k: usize, max_d2: f64) -> f64 {
    if out.len() == k {
        out[k - 1].0.min(max_d2)
    } else {
        max_d2
    }
}

/// Insert `item` into `out`, kept sorted by `(distance, index)` and at most
/// `k` long.
fn insert(out: &mut Vec<(f64, u32)>, k: usize, item: (f64, u32)) {
    let at = out.partition_point(|&(d, i)| (d, i) < item);
    if at >= k {
        return;
    }
    out.insert(at, item);
    out.truncate(k);
}

fn build(items: &mut [([f64; 3], u32)], axes: &mut [u8], depth: usize) {
    if items.len() <= 1 {
        if let Some(axis) = axes.first_mut() {
            *axis = (depth % 3) as u8;
        }
        return;
    }
    // Split on the axis of widest spread: stations cluster on the sphere,
    // and a cycling axis would cut a thin network along its thin side.
    let mut lo = [f64::INFINITY; 3];
    let mut hi = [f64::NEG_INFINITY; 3];
    for (p, _) in items.iter() {
        for a in 0..3 {
            lo[a] = lo[a].min(p[a]);
            hi[a] = hi[a].max(p[a]);
        }
    }
    let axis = (0..3)
        .max_by(|&a, &b| (hi[a] - lo[a]).total_cmp(&(hi[b] - lo[b])))
        .unwrap_or(0);
    let mid = items.len() / 2;
    items.select_nth_unstable_by(mid, |a, b| {
        a.0[axis].total_cmp(&b.0[axis]).then(a.1.cmp(&b.1))
    });
    axes[mid] = axis as u8;
    let (left_items, rest) = items.split_at_mut(mid);
    let (left_axes, rest_axes) = axes.split_at_mut(mid);
    build(left_items, left_axes, depth + 1);
    build(&mut rest[1..], &mut rest_axes[1..], depth + 1);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic scatter of points on the unit sphere.
    fn points(n: usize) -> Vec<[f64; 3]> {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        (0..n)
            .map(|_| {
                let lon = (next() * 360.0 - 180.0_f64).to_radians();
                let lat = (next() * 2.0 - 1.0_f64).asin();
                [lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin()]
            })
            .collect()
    }

    /// The tree finds exactly what a brute-force scan finds, including the
    /// radius cut and the index tie-break.
    #[test]
    fn nearest_matches_brute_force() {
        let pts = points(500);
        let tree = KdTree::new(&pts);
        let mut out = Vec::new();
        for (qi, &q) in points(60).iter().enumerate() {
            let k = 1 + qi % 9;
            let max_d2 = 0.002 + 0.01 * (qi % 5) as f64;
            tree.nearest(q, k, max_d2, &mut out);
            let mut brute: Vec<(f64, u32)> = pts
                .iter()
                .enumerate()
                .map(|(i, &p)| (dist2(p, q), i as u32))
                .filter(|&(d, _)| d <= max_d2)
                .collect();
            brute.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            brute.truncate(k);
            assert_eq!(out, brute, "query {qi}");
        }
    }

    /// Duplicate points tie, and the lower index comes first.
    #[test]
    fn duplicates_tie_by_index() {
        let p = [0.0, 0.0, 1.0];
        let tree = KdTree::new(&[p, [1.0, 0.0, 0.0], p, p]);
        let mut out = Vec::new();
        tree.nearest(p, 2, 4.0, &mut out);
        assert_eq!(out, [(0.0, 0), (0.0, 2)]);
        tree.nearest(p, 0, 4.0, &mut out);
        assert!(out.is_empty());
        KdTree::new(&[]).nearest(p, 3, 4.0, &mut out);
        assert!(out.is_empty());
    }
}
