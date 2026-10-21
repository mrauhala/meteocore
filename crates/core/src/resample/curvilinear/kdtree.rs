//! A static 3-D k-d tree over a curvilinear grid's node unit vectors, for
//! the node nearest a point.
//!
//! Points on the unit sphere: the straight-line (chord) distance between
//! two of them grows monotonically with their great-circle distance, so
//! the nearest by chord is the nearest on the Earth, and the antimeridian
//! and the poles are ordinary places. The tree holds node indices only and
//! reads the vectors from the grid's own array: 5 bytes per node.

/// A balanced k-d tree stored implicitly in one array: the node of the
/// range `lo..hi` is its middle element, its children the halves either
/// side.
pub(super) struct KdTree {
    /// Node indices into the grid's vectors, in tree order.
    order: Vec<u32>,
    /// The splitting axis of the node at each position.
    axes: Vec<u8>,
}

impl KdTree {
    /// Build over the nodes of `xyz` that have a position (finite), in
    /// O(n log n).
    pub(super) fn new(xyz: &[[f32; 3]]) -> Self {
        let mut order: Vec<u32> = (0..xyz.len())
            .filter(|&k| xyz[k].iter().all(|c| c.is_finite()))
            .map(|k| k as u32)
            .collect();
        let mut axes = vec![0u8; order.len()];
        build(&mut order, &mut axes, xyz);
        Self { order, axes }
    }

    /// The node of `xyz` nearest `q`, with its squared chord distance, when
    /// one lies within `max_d2` of it. Ties go to the lower index, so equal
    /// grids give equal answers.
    pub(super) fn nearest(
        &self,
        xyz: &[[f32; 3]],
        q: [f64; 3],
        max_d2: f64,
    ) -> Option<(usize, f64)> {
        let mut best = None;
        self.search(0, self.order.len(), xyz, q, max_d2, &mut best);
        best.map(|(d2, k)| (k as usize, d2))
    }

    fn search(
        &self,
        lo: usize,
        hi: usize,
        xyz: &[[f32; 3]],
        q: [f64; 3],
        max_d2: f64,
        best: &mut Option<(f64, u32)>,
    ) {
        if lo >= hi {
            return;
        }
        let mid = lo + (hi - lo) / 2;
        let k = self.order[mid];
        let p = vector(xyz[k as usize]);
        let d2 = dist2(p, q);
        if d2 <= max_d2 && best.is_none_or(|b| (d2, k) < b) {
            *best = Some((d2, k));
        }
        let axis = usize::from(self.axes[mid]);
        let diff = q[axis] - p[axis];
        let (near, far) = if diff < 0.0 {
            ((lo, mid), (mid + 1, hi))
        } else {
            ((mid + 1, hi), (lo, mid))
        };
        self.search(near.0, near.1, xyz, q, max_d2, best);
        // `<=`: a node exactly on the plane may tie the best, and win it
        // on its index.
        let limit = best.map_or(max_d2, |(d2, _)| d2);
        if diff * diff <= limit {
            self.search(far.0, far.1, xyz, q, max_d2, best);
        }
    }
}

/// A stored vector in `f64`.
#[inline]
pub(super) fn vector(v: [f32; 3]) -> [f64; 3] {
    v.map(f64::from)
}

#[inline]
pub(super) fn dist2(a: [f64; 3], b: [f64; 3]) -> f64 {
    let (x, y, z) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
    x * x + y * y + z * z
}

fn build(order: &mut [u32], axes: &mut [u8], xyz: &[[f32; 3]]) {
    if order.len() <= 1 {
        return;
    }
    // Split on the axis of widest spread: a regional grid is a thin patch
    // of the sphere, which a cycling axis would cut along its thin side.
    let mut lo = [f32::INFINITY; 3];
    let mut hi = [f32::NEG_INFINITY; 3];
    for &k in order.iter() {
        let p = xyz[k as usize];
        for a in 0..3 {
            lo[a] = lo[a].min(p[a]);
            hi[a] = hi[a].max(p[a]);
        }
    }
    let axis = (0..3)
        .max_by(|&a, &b| (hi[a] - lo[a]).total_cmp(&(hi[b] - lo[b])))
        .unwrap_or(0);
    let mid = order.len() / 2;
    order.select_nth_unstable_by(mid, |&a, &b| {
        xyz[a as usize][axis]
            .total_cmp(&xyz[b as usize][axis])
            .then(a.cmp(&b))
    });
    axes[mid] = axis as u8;
    let (left, rest) = order.split_at_mut(mid);
    let (left_axes, rest_axes) = axes.split_at_mut(mid);
    build(left, left_axes, xyz);
    build(&mut rest[1..], &mut rest_axes[1..], xyz);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic scatter of unit vectors, some of them invalid.
    fn points(n: usize, seed: u64) -> Vec<[f32; 3]> {
        let mut state = seed;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        (0..n)
            .map(|i| {
                if i % 97 == 13 {
                    return [f32::NAN; 3];
                }
                let lon = (next() * 360.0 - 180.0_f64).to_radians();
                let lat = (next() * 2.0 - 1.0_f64).asin();
                [
                    (lat.cos() * lon.cos()) as f32,
                    (lat.cos() * lon.sin()) as f32,
                    lat.sin() as f32,
                ]
            })
            .collect()
    }

    /// The tree finds what a scan of every valid node finds, ties to the
    /// lower index, and nothing past the radius.
    #[test]
    fn nearest_matches_a_brute_force_scan() {
        let xyz = points(2000, 0x2545_f491_4f6c_dd1d);
        let tree = KdTree::new(&xyz);
        assert_eq!(tree.order.len(), 2000 - 21);
        for q in points(300, 0x9e37_79b9_7f4a_7c15) {
            if !q[0].is_finite() {
                continue;
            }
            let q = vector(q);
            let brute = xyz
                .iter()
                .enumerate()
                .filter(|(_, p)| p[0].is_finite())
                .map(|(k, &p)| (dist2(vector(p), q), k))
                .min_by(|a, b| a.partial_cmp(b).unwrap())
                .unwrap();
            assert_eq!(tree.nearest(&xyz, q, 4.0), Some((brute.1, brute.0)));
            assert_eq!(tree.nearest(&xyz, q, brute.0 * 0.999), None);
        }
    }
}
