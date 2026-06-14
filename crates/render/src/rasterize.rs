//! Pure 2-D scanline polygon fill — the framework-free rasterizer shared by
//! every vector map layer (alert/hazard *areas*: CAP #396, GeoJSON Maps #398,
//! IWXXM #399). Built once here so scan-conversion (winding, holes, tile-edge
//! clipping) is implemented and tested in one place rather than re-derived per
//! engine.
//!
//! Inputs are **pixel-space** rings: the engine projects geometry *vertices*
//! via [`ds_core::geo::geometry_to_pixels`] (never per output pixel — #203);
//! this module knows nothing about CRS or projection. It fills polygons into a
//! `RasterTile`-shaped `Vec<Option<f64>>` *before* colorization, so the existing
//! colormap pipeline styles the result like any other raster layer (the value
//! is a severity code, category id, … that a `LutColorMap`/`IntegerLutColorMap`
//! turns into colour).
//!
//! The fill uses the **even-odd** rule over the combined edge set of the
//! exterior ring plus its holes, so a hole punches out the interior regardless
//! of its winding direction — robust against inconsistently-wound source data.
//! Pixels are sampled at their centre (`x + 0.5`, `y + 0.5`); edges are hard
//! (no anti-aliasing — alert zones are categorical). Spans are clipped to the
//! tile, so a polygon partly off-tile fills only its visible part.

/// How a fill value combines with whatever a pixel already holds — the
/// overlap policy when multiple features (or rings) cover the same pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Combine {
    /// Overwrite the existing value (last writer wins).
    Replace,
    /// Keep the larger of the existing and new value. Overlapping alerts
    /// resolve to the highest severity **deterministically, regardless of the
    /// order features are filled** — CAP's policy.
    Max,
    /// Only fill nodata pixels; keep an already-set value (first writer wins).
    First,
}

/// One edge of a polygon ring in pixel space. Horizontal and non-finite edges
/// are filtered out before this is constructed, so `y0 != y1` and all fields
/// are finite — the scanline crossing math never divides by zero.
struct Edge {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

/// Fill a polygon (one exterior ring + interior-ring holes), given in
/// **pixel** coordinates, into a row-major `width × height` value buffer.
///
/// - `out` — the target raster's `values`; must be exactly `width * height`
///   long (a mismatch is a no-op, never a panic).
/// - `exterior` / `holes` — pixel-space rings (`[x, y]`). Rings need not be
///   explicitly closed (the last→first edge is added implicitly); a ring with
///   fewer than 3 vertices contributes nothing. Vertices may lie outside the
///   tile (the fill clips) and edges touching a non-finite vertex are skipped.
/// - `value` — the value written into each covered pixel (severity, category,
///   …), later colorized.
/// - `combine` — the [`Combine`] overlap policy for pixels already set.
///
/// Uses even-odd scanline conversion: holes are just additional rings in the
/// edge set, so they punch out the interior for any winding. Degenerate
/// (zero-area / collinear) rings fill nothing.
pub fn fill_polygon(
    out: &mut [Option<f64>],
    width: u32,
    height: u32,
    exterior: &[[f64; 2]],
    holes: &[Vec<[f64; 2]>],
    value: f64,
    combine: Combine,
) {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || out.len() != w.saturating_mul(h) {
        return;
    }

    // Collect every ring's edges into one set (even-odd treats them uniformly).
    let mut edges: Vec<Edge> = Vec::new();
    push_ring_edges(exterior, &mut edges);
    for hole in holes {
        push_ring_edges(hole, &mut edges);
    }
    if edges.is_empty() {
        return;
    }

    // Bound the scanline sweep to the polygon's vertical extent ∩ the tile.
    let (mut y_min, mut y_max) = (f64::INFINITY, f64::NEG_INFINITY);
    for e in &edges {
        y_min = y_min.min(e.y0.min(e.y1));
        y_max = y_max.max(e.y0.max(e.y1));
    }
    let y_start = y_min.floor().clamp(0.0, h as f64) as usize;
    let y_end = y_max.ceil().clamp(0.0, h as f64) as usize;

    let mut crossings: Vec<f64> = Vec::new();
    for y in y_start..y_end {
        let yc = y as f64 + 0.5;
        crossings.clear();
        for e in &edges {
            // Half-open crossing test: each shared vertex is counted for
            // exactly one of its two edges, so no double counts at vertices.
            if (e.y0 <= yc) != (e.y1 <= yc) {
                let t = (yc - e.y0) / (e.y1 - e.y0);
                crossings.push(e.x0 + t * (e.x1 - e.x0));
            }
        }
        if crossings.len() < 2 {
            continue;
        }
        crossings.sort_by(f64::total_cmp);

        // Even-odd: fill the span between each consecutive crossing pair.
        let row = y * w;
        let mut i = 0;
        while i + 1 < crossings.len() {
            let (span_start, span_end) = (crossings[i], crossings[i + 1]);
            // Pixels whose centre (x + 0.5) lies in [span_start, span_end).
            let x_start = (span_start - 0.5).ceil().max(0.0) as usize;
            let x_end = ((span_end - 0.5).ceil().max(0.0) as usize).min(w);
            for x in x_start..x_end {
                paint(&mut out[row + x], value, combine);
            }
            i += 2;
        }
    }
}

/// Append the (non-horizontal, finite) edges of a closed ring to `edges`.
/// Consecutive vertices form edges and the ring is closed implicitly
/// (`last → first`); rings with < 3 vertices add nothing.
fn push_ring_edges(ring: &[[f64; 2]], edges: &mut Vec<Edge>) {
    let n = ring.len();
    if n < 3 {
        return;
    }
    for i in 0..n {
        let a = ring[i];
        let b = ring[(i + 1) % n];
        if !(a[0].is_finite() && a[1].is_finite() && b[0].is_finite() && b[1].is_finite()) {
            continue;
        }
        // Horizontal edges contribute no scanline crossing.
        if a[1] == b[1] {
            continue;
        }
        edges.push(Edge {
            x0: a[0],
            y0: a[1],
            x1: b[0],
            y1: b[1],
        });
    }
}

/// Apply the overlap policy to a single pixel slot.
#[inline]
fn paint(slot: &mut Option<f64>, value: f64, combine: Combine) {
    match combine {
        Combine::Replace => *slot = Some(value),
        Combine::First => {
            if slot.is_none() {
                *slot = Some(value);
            }
        }
        Combine::Max => {
            *slot = Some(match *slot {
                Some(existing) => existing.max(value),
                None => value,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Count the set (non-nodata) pixels.
    fn filled(out: &[Option<f64>]) -> usize {
        out.iter().filter(|v| v.is_some()).count()
    }

    fn at(out: &[Option<f64>], w: u32, x: u32, y: u32) -> Option<f64> {
        out[(y * w + x) as usize]
    }

    fn square(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<[f64; 2]> {
        vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]]
    }

    #[test]
    fn fills_convex_square() {
        let mut out = vec![None; 100];
        // Square spanning integer pixel edges 2..8 → 6×6 = 36 pixels filled.
        fill_polygon(
            &mut out,
            10,
            10,
            &square(2.0, 2.0, 8.0, 8.0),
            &[],
            1.0,
            Combine::Replace,
        );
        assert_eq!(filled(&out), 36);
        assert_eq!(at(&out, 10, 5, 5), Some(1.0));
        assert_eq!(at(&out, 10, 2, 2), Some(1.0)); // inclusive corner pixel
        assert_eq!(at(&out, 10, 0, 0), None);
        assert_eq!(at(&out, 10, 8, 8), None); // exclusive far edge
    }

    #[test]
    fn fills_concave_polygon() {
        // An L-shape: bottom bar x∈[1,5] y∈[1,3] + left bar x∈[1,3] y∈[1,5].
        let mut out = vec![None; 64];
        let l = vec![
            [1.0, 1.0],
            [5.0, 1.0],
            [5.0, 3.0],
            [3.0, 3.0],
            [3.0, 5.0],
            [1.0, 5.0],
        ];
        fill_polygon(&mut out, 8, 8, &l, &[], 1.0, Combine::Replace);
        assert_eq!(at(&out, 8, 2, 2), Some(1.0)); // bottom bar
        assert_eq!(at(&out, 8, 2, 4), Some(1.0)); // left bar
        assert_eq!(at(&out, 8, 4, 4), None); // the notch (concavity) is empty
    }

    #[test]
    fn punches_hole() {
        // 10×10 outer square with a 3..7 hole.
        let mut out = vec![None; 100];
        fill_polygon(
            &mut out,
            10,
            10,
            &square(0.0, 0.0, 10.0, 10.0),
            &[square(3.0, 3.0, 7.0, 7.0)],
            1.0,
            Combine::Replace,
        );
        assert_eq!(at(&out, 10, 5, 5), None); // inside the hole
        assert_eq!(at(&out, 10, 1, 1), Some(1.0)); // outside the hole, inside outer
        assert_eq!(at(&out, 10, 8, 8), Some(1.0));
        // Outer 100 minus the 4×4 hole = 84.
        assert_eq!(filled(&out), 84);
    }

    #[test]
    fn hole_winding_does_not_matter() {
        // Same hole wound the opposite way still punches out (even-odd rule).
        let mut cw = vec![None; 100];
        let mut ccw = vec![None; 100];
        let hole_cw = vec![[3.0, 3.0], [7.0, 3.0], [7.0, 7.0], [3.0, 7.0]];
        let hole_ccw = vec![[3.0, 3.0], [3.0, 7.0], [7.0, 7.0], [7.0, 3.0]];
        fill_polygon(
            &mut cw,
            10,
            10,
            &square(0.0, 0.0, 10.0, 10.0),
            &[hole_cw],
            1.0,
            Combine::Replace,
        );
        fill_polygon(
            &mut ccw,
            10,
            10,
            &square(0.0, 0.0, 10.0, 10.0),
            &[hole_ccw],
            1.0,
            Combine::Replace,
        );
        assert_eq!(cw, ccw);
        assert_eq!(filled(&cw), 84);
    }

    #[test]
    fn clips_at_every_tile_edge() {
        // A polygon larger than the tile on all four sides fills every pixel.
        let mut out = vec![None; 100];
        fill_polygon(
            &mut out,
            10,
            10,
            &square(-5.0, -5.0, 15.0, 15.0),
            &[],
            1.0,
            Combine::Replace,
        );
        assert_eq!(filled(&out), 100);

        // Each edge individually: a polygon poking across just one edge fills
        // only its visible span, never panics, never touches the far side.
        // Off the LEFT edge: x∈[-5,4] visible as 0..4.
        let mut left = vec![None; 100];
        fill_polygon(
            &mut left,
            10,
            10,
            &square(-5.0, 2.0, 4.0, 8.0),
            &[],
            1.0,
            Combine::Replace,
        );
        assert_eq!(at(&left, 10, 0, 5), Some(1.0));
        assert_eq!(at(&left, 10, 3, 5), Some(1.0));
        assert_eq!(at(&left, 10, 5, 5), None);

        // Off the RIGHT edge: x∈[6,15] visible as 6..10.
        let mut right = vec![None; 100];
        fill_polygon(
            &mut right,
            10,
            10,
            &square(6.0, 2.0, 15.0, 8.0),
            &[],
            1.0,
            Combine::Replace,
        );
        assert_eq!(at(&right, 10, 9, 5), Some(1.0));
        assert_eq!(at(&right, 10, 6, 5), Some(1.0));
        assert_eq!(at(&right, 10, 4, 5), None);

        // Off the TOP edge: y∈[-5,4] visible as 0..4.
        let mut top = vec![None; 100];
        fill_polygon(
            &mut top,
            10,
            10,
            &square(2.0, -5.0, 8.0, 4.0),
            &[],
            1.0,
            Combine::Replace,
        );
        assert_eq!(at(&top, 10, 5, 0), Some(1.0));
        assert_eq!(at(&top, 10, 5, 3), Some(1.0));
        assert_eq!(at(&top, 10, 5, 5), None);

        // Off the BOTTOM edge: y∈[6,15] visible as 6..10.
        let mut bottom = vec![None; 100];
        fill_polygon(
            &mut bottom,
            10,
            10,
            &square(2.0, 6.0, 8.0, 15.0),
            &[],
            1.0,
            Combine::Replace,
        );
        assert_eq!(at(&bottom, 10, 5, 9), Some(1.0));
        assert_eq!(at(&bottom, 10, 5, 6), Some(1.0));
        assert_eq!(at(&bottom, 10, 5, 4), None);
    }

    #[test]
    fn degenerate_rings_fill_nothing() {
        // Collinear exterior (zero area).
        let mut collinear = vec![None; 100];
        fill_polygon(
            &mut collinear,
            10,
            10,
            &[[0.0, 0.0], [5.0, 0.0], [10.0, 0.0]],
            &[],
            1.0,
            Combine::Replace,
        );
        assert_eq!(filled(&collinear), 0);

        // Fewer than 3 vertices.
        let mut two = vec![None; 100];
        fill_polygon(
            &mut two,
            10,
            10,
            &[[1.0, 1.0], [8.0, 8.0]],
            &[],
            1.0,
            Combine::Replace,
        );
        assert_eq!(filled(&two), 0);

        // Empty ring.
        let mut empty = vec![None; 100];
        fill_polygon(&mut empty, 10, 10, &[], &[], 1.0, Combine::Replace);
        assert_eq!(filled(&empty), 0);
    }

    #[test]
    fn combine_max_is_order_independent() {
        // Three overlapping fills of the whole tile with values 1, 3, 2 in any
        // order resolve to the max (3) under Combine::Max.
        let orders: [[f64; 3]; 2] = [[1.0, 3.0, 2.0], [2.0, 1.0, 3.0]];
        for vals in orders {
            let mut out = vec![None; 100];
            for v in vals {
                fill_polygon(
                    &mut out,
                    10,
                    10,
                    &square(0.0, 0.0, 10.0, 10.0),
                    &[],
                    v,
                    Combine::Max,
                );
            }
            assert_eq!(at(&out, 10, 5, 5), Some(3.0), "order {vals:?}");
        }
    }

    #[test]
    fn combine_replace_and_first() {
        let sq = square(0.0, 0.0, 10.0, 10.0);
        // Replace → last writer wins.
        let mut replace = vec![None; 100];
        fill_polygon(&mut replace, 10, 10, &sq, &[], 1.0, Combine::Replace);
        fill_polygon(&mut replace, 10, 10, &sq, &[], 9.0, Combine::Replace);
        assert_eq!(at(&replace, 10, 5, 5), Some(9.0));

        // First → first writer wins (only nodata is filled).
        let mut first = vec![None; 100];
        fill_polygon(&mut first, 10, 10, &sq, &[], 1.0, Combine::First);
        fill_polygon(&mut first, 10, 10, &sq, &[], 9.0, Combine::First);
        assert_eq!(at(&first, 10, 5, 5), Some(1.0));
    }

    #[test]
    fn mismatched_buffer_is_noop() {
        let mut out = vec![None; 50]; // not 10*10
        fill_polygon(
            &mut out,
            10,
            10,
            &square(0.0, 0.0, 10.0, 10.0),
            &[],
            1.0,
            Combine::Replace,
        );
        assert_eq!(filled(&out), 0);
    }

    #[test]
    fn non_finite_vertices_are_skipped() {
        // A vertex outside a projection's domain (NaN) must not panic or
        // corrupt the fill; the touching edges are dropped.
        let mut out = vec![None; 100];
        let ring = vec![[2.0, 2.0], [f64::NAN, 4.0], [8.0, 8.0], [2.0, 8.0]];
        fill_polygon(&mut out, 10, 10, &ring, &[], 1.0, Combine::Replace);
        // No panic; the result is well-defined (whatever the finite edges give).
        // The key contract is robustness, not a specific shape.
        let _ = filled(&out);
    }
}
