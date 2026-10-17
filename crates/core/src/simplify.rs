//! Douglas–Peucker simplification of closed rings and open lines — the one
//! implementation, shared by the storm-cell footprints ([`crate::cells`]) and
//! the Features items page's map preview (#1135).
//!
//! The algorithm is planar: the caller projects vertices into a metric plane
//! first (local ENU metres, Web Mercator metres) and gives the tolerance in
//! the same units. It returns kept indices so the caller can keep each
//! vertex's original coordinates.

/// Douglas–Peucker on a closed loop (`points` without the repeated closing
/// vertex): anchor vertex 0 and the vertex farthest from it, simplify each
/// arc to `tol`, return the kept indices in ascending order. Always keeps at
/// least 3 vertices so the ring stays a polygon; a loop of 4 or fewer
/// vertices is returned whole.
pub fn douglas_peucker_closed(points: &[(f64, f64)], tol: f64) -> Vec<usize> {
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
    mark_arc(points, 0, far, tol, &mut keep);
    mark_arc_wrapping(points, far, n, tol, &mut keep);
    let kept: Vec<usize> = (0..n).filter(|&i| keep[i]).collect();
    if kept.len() < 3 {
        // Degenerate (collinear loop) — keep a triangle's worth of vertices.
        return vec![0, n / 3, 2 * n / 3];
    }
    kept
}

/// Douglas–Peucker on an open line: keep both ends, simplify the vertices
/// between them to `tol`, return the kept indices in ascending order. A line
/// of 2 or fewer vertices is returned whole.
pub fn douglas_peucker_open(points: &[(f64, f64)], tol: f64) -> Vec<usize> {
    let n = points.len();
    if n <= 2 {
        return (0..n).collect();
    }
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[n - 1] = true;
    mark_arc(points, 0, n - 1, tol, &mut keep);
    (0..n).filter(|&i| keep[i]).collect()
}

/// Mark the vertices of the open arc `points[i0..=i1]` that a `tol` chord
/// deviation requires. Iterative, so no input can deepen the stack of the
/// request worker the Features caller runs on (a recursive split is as deep
/// as the arc is long in the worst case). The kept set does not depend on the
/// visiting order.
fn mark_arc(points: &[(f64, f64)], i0: usize, i1: usize, tol: f64, keep: &mut [bool]) {
    let mut arcs = vec![(i0, i1)];
    while let Some((i0, i1)) = arcs.pop() {
        if i1 <= i0 + 1 {
            continue;
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
            arcs.push((i0, worst.0));
            arcs.push((worst.0, i1));
        }
    }
}

/// Simplify the arc from `i0` back around to vertex 0 (i.e. `i0..n` plus the
/// implicit wrap to index 0).
fn mark_arc_wrapping(points: &[(f64, f64)], i0: usize, n: usize, tol: f64, keep: &mut [bool]) {
    // Materialise the wrapped arc with the closing vertex appended; indices
    // map back as (i0 + k) % n.
    let arc: Vec<(f64, f64)> = (i0..=n).map(|k| points[k % n]).collect();
    let mut arc_keep = vec![false; arc.len()];
    arc_keep[0] = true;
    *arc_keep.last_mut().expect("non-empty arc") = true;
    mark_arc(&arc, 0, arc.len() - 1, tol, &mut arc_keep);
    for (k, &kept) in arc_keep.iter().enumerate() {
        if kept {
            keep[(i0 + k) % n] = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collinear_edge_vertices_drop_and_corners_stay() {
        // A 10 × 10 square sampled every unit along its edges: 40 vertices,
        // only the four corners carry shape.
        let mut square = Vec::new();
        for i in 0..10 {
            square.push((i as f64, 0.0));
        }
        for i in 0..10 {
            square.push((10.0, i as f64));
        }
        for i in 0..10 {
            square.push((10.0 - i as f64, 10.0));
        }
        for i in 0..10 {
            square.push((0.0, 10.0 - i as f64));
        }
        assert_eq!(douglas_peucker_closed(&square, 0.01), vec![0, 10, 20, 30]);
    }

    #[test]
    fn a_bump_survives_only_below_its_height() {
        let ring = [
            (0.0, 0.0),
            (5.0, 0.0),
            (6.0, 0.5),
            (7.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (0.0, 10.0),
        ];
        assert!(douglas_peucker_closed(&ring, 0.4).contains(&2));
        assert!(!douglas_peucker_closed(&ring, 0.6).contains(&2));
    }

    #[test]
    fn small_and_collinear_loops_keep_a_polygon() {
        let tiny = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
        assert_eq!(douglas_peucker_closed(&tiny, 100.0), vec![0, 1, 2, 3]);
        let line: Vec<_> = (0..9).map(|i| (i as f64, 0.0)).collect();
        assert!(douglas_peucker_closed(&line, 1.0).len() >= 3);
    }

    #[test]
    fn an_open_line_keeps_its_ends_and_its_bends() {
        // Two straight legs meeting at a right angle, sampled every unit.
        let mut line: Vec<_> = (0..=10).map(|i| (i as f64, 0.0)).collect();
        line.extend((1..=10).map(|i| (10.0, i as f64)));
        assert_eq!(douglas_peucker_open(&line, 0.01), vec![0, 10, 20]);
        assert_eq!(douglas_peucker_open(&line[..2], 100.0), vec![0, 1]);
        assert_eq!(douglas_peucker_open(&[], 1.0), Vec::<usize>::new());
    }
}
