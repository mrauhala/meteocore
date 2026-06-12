//! Framework-free **value-space** paint primitives for derived overlay
//! layers (#367): stroke segments, polylines, rings, and point markers
//! directly into a raster's `Vec<Option<f64>>` *before* colorization, so the
//! existing colormap pipeline styles the overlay like any other layer.
//!
//! Inputs are pre-projected **pixel** coordinates (f64; the engine projects
//! the handful of geometry vertices via `OutputCrs::world_to_fraction` —
//! never per pixel). Segments are clipped to the tile before walking
//! (Liang–Barsky), so a vertex projected far outside the tile costs nothing;
//! non-finite coordinates are skipped. All painting is last-writer-wins.

/// A mutable paint target: the raster's value buffer plus its dimensions.
pub struct Canvas<'a> {
    values: &'a mut [Option<f64>],
    width: i64,
    height: i64,
}

impl<'a> Canvas<'a> {
    /// Wrap a row-major `width × height` value buffer. Returns `None` when
    /// the buffer size doesn't match the dimensions.
    pub fn new(values: &'a mut [Option<f64>], width: u32, height: u32) -> Option<Self> {
        if values.len() != (width as usize).checked_mul(height as usize)? {
            return None;
        }
        Some(Self {
            values,
            width: width as i64,
            height: height as i64,
        })
    }

    fn set(&mut self, x: i64, y: i64, value: f64) {
        if x >= 0 && x < self.width && y >= 0 && y < self.height {
            self.values[(y * self.width + x) as usize] = Some(value);
        }
    }

    /// Paint the pixel at `(x, y)` and, for `thickness > 1`, its
    /// `thickness × thickness` neighbourhood (anchored so the stroke stays
    /// visually centred).
    fn set_thick(&mut self, x: i64, y: i64, thickness: u32, value: f64) {
        let t = thickness.max(1) as i64;
        let lo = -(t - 1) / 2;
        for dy in lo..lo + t {
            for dx in lo..lo + t {
                self.set(x + dx, y + dy, value);
            }
        }
    }

    /// Stroke a straight segment between two pixel positions (Bresenham over
    /// the rounded endpoints). Endpoints may lie anywhere — the segment is
    /// clipped to the canvas first; non-finite coordinates skip the call.
    pub fn stroke_segment(&mut self, from: (f64, f64), to: (f64, f64), thickness: u32, value: f64) {
        let (x0, y0) = from;
        let (x1, y1) = to;
        if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()) {
            return;
        }
        // Liang–Barsky clip against the canvas expanded by the stroke
        // thickness, so a segment whose visible part is only its thick edge
        // still paints, while a fully-off-tile segment costs nothing.
        let pad = thickness.max(1) as f64;
        let Some(((cx0, cy0), (cx1, cy1))) = clip_segment(
            (x0, y0),
            (x1, y1),
            (-pad, -pad),
            (self.width as f64 + pad, self.height as f64 + pad),
        ) else {
            return;
        };

        let (mut x, mut y) = (cx0.round() as i64, cy0.round() as i64);
        let (ex, ey) = (cx1.round() as i64, cy1.round() as i64);
        let dx = (ex - x).abs();
        let dy = -(ey - y).abs();
        let sx = if x < ex { 1 } else { -1 };
        let sy = if y < ey { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            self.set_thick(x, y, thickness, value);
            if x == ex && y == ey {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }

    /// Stroke an open polyline through the given pixel positions.
    pub fn stroke_polyline(&mut self, points: &[(f64, f64)], thickness: u32, value: f64) {
        for pair in points.windows(2) {
            self.stroke_segment(pair[0], pair[1], thickness, value);
        }
    }

    /// Stroke a closed ring: the polyline plus the closing segment (a no-op
    /// when the input already repeats its first vertex). Empty/degenerate
    /// inputs are skipped.
    pub fn stroke_ring(&mut self, points: &[(f64, f64)], thickness: u32, value: f64) {
        if points.len() < 2 {
            return;
        }
        self.stroke_polyline(points, thickness, value);
        let (first, last) = (points[0], points[points.len() - 1]);
        if first != last {
            self.stroke_segment(last, first, thickness, value);
        }
    }

    /// Paint a `+` marker centred on a pixel position, arms `half_size`
    /// pixels long.
    pub fn paint_marker(&mut self, at: (f64, f64), half_size: u32, value: f64) {
        let (x, y) = at;
        if !(x.is_finite() && y.is_finite()) {
            return;
        }
        let (cx, cy) = (x.round() as i64, y.round() as i64);
        let h = half_size as i64;
        for d in -h..=h {
            self.set(cx + d, cy, value);
            self.set(cx, cy + d, value);
        }
    }
}

/// Liang–Barsky segment clip against an axis-aligned box. Returns the
/// clipped endpoints, or `None` when the segment misses the box entirely.
fn clip_segment(
    (x0, y0): (f64, f64),
    (x1, y1): (f64, f64),
    (min_x, min_y): (f64, f64),
    (max_x, max_y): (f64, f64),
) -> Option<((f64, f64), (f64, f64))> {
    let (dx, dy) = (x1 - x0, y1 - y0);
    let mut t0 = 0.0f64;
    let mut t1 = 1.0f64;
    for (p, q) in [
        (-dx, x0 - min_x),
        (dx, max_x - x0),
        (-dy, y0 - min_y),
        (dy, max_y - y0),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None; // parallel and outside
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                t0 = t0.max(r);
            } else {
                t1 = t1.min(r);
            }
            if t0 > t1 {
                return None;
            }
        }
    }
    Some(((x0 + t0 * dx, y0 + t0 * dy), (x0 + t1 * dx, y0 + t1 * dy)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas_of(w: u32, h: u32) -> Vec<Option<f64>> {
        vec![None; (w * h) as usize]
    }

    fn painted(values: &[Option<f64>]) -> usize {
        values.iter().filter(|v| v.is_some()).count()
    }

    #[test]
    fn segment_paints_both_endpoints_and_line() {
        let mut v = canvas_of(16, 16);
        let mut c = Canvas::new(&mut v, 16, 16).unwrap();
        c.stroke_segment((2.0, 2.0), (13.0, 9.0), 1, 42.0);
        assert_eq!(v[2 * 16 + 2], Some(42.0), "start endpoint");
        assert_eq!(v[9 * 16 + 13], Some(42.0), "end endpoint");
        assert!(painted(&v) >= 12, "the line in between is painted");
    }

    #[test]
    fn off_canvas_segment_paints_nothing_and_crossing_segment_clips() {
        let mut v = canvas_of(8, 8);
        Canvas::new(&mut v, 8, 8)
            .unwrap()
            .stroke_segment((-100.0, -100.0), (-50.0, -90.0), 1, 1.0);
        assert_eq!(painted(&v), 0, "fully outside paints nothing");
        // A segment crossing the whole canvas paints a clipped run.
        Canvas::new(&mut v, 8, 8)
            .unwrap()
            .stroke_segment((-100.0, 4.2), (100.0, 4.2), 1, 7.0);
        assert_eq!(painted(&v), 8, "horizontal crossing paints one full row");
        assert_eq!(v[4 * 8], Some(7.0));
        assert_eq!(v[4 * 8 + 7], Some(7.0));
    }

    #[test]
    fn non_finite_inputs_are_skipped() {
        let mut v = canvas_of(8, 8);
        let mut c = Canvas::new(&mut v, 8, 8).unwrap();
        c.stroke_segment((f64::NAN, 1.0), (4.0, 4.0), 1, 1.0);
        c.paint_marker((f64::INFINITY, 2.0), 2, 1.0);
        assert_eq!(painted(&v), 0);
    }

    #[test]
    fn ring_closes_and_thickness_widens() {
        let mut v = canvas_of(16, 16);
        let mut c = Canvas::new(&mut v, 16, 16).unwrap();
        // Open square — stroke_ring must add the closing edge.
        c.stroke_ring(
            &[(3.0, 3.0), (12.0, 3.0), (12.0, 12.0), (3.0, 12.0)],
            1,
            5.0,
        );
        assert_eq!(v[7 * 16 + 3], Some(5.0), "closing left edge painted");
        let thin = painted(&v);

        let mut v2 = canvas_of(16, 16);
        let mut c2 = Canvas::new(&mut v2, 16, 16).unwrap();
        c2.stroke_ring(
            &[(3.0, 3.0), (12.0, 3.0), (12.0, 12.0), (3.0, 12.0)],
            2,
            5.0,
        );
        assert!(painted(&v2) > thin, "thickness 2 paints more pixels");
    }

    #[test]
    fn marker_is_a_plus() {
        let mut v = canvas_of(9, 9);
        let mut c = Canvas::new(&mut v, 9, 9).unwrap();
        c.paint_marker((4.0, 4.0), 2, 9.0);
        assert_eq!(painted(&v), 9, "two 5-px arms sharing the centre");
        assert_eq!(v[4 * 9 + 4], Some(9.0));
        assert_eq!(v[4 * 9 + 2], Some(9.0));
        assert_eq!(v[2 * 9 + 4], Some(9.0));
        assert_eq!(v[0], None);
    }

    #[test]
    fn marker_clips_at_edges() {
        let mut v = canvas_of(4, 4);
        let mut c = Canvas::new(&mut v, 4, 4).unwrap();
        c.paint_marker((0.0, 0.0), 3, 1.0);
        assert!(painted(&v) > 0, "in-bounds part painted");
        // No panic and nothing outside the buffer — implied by the type.
    }

    #[test]
    fn canvas_rejects_mismatched_buffer() {
        let mut v = canvas_of(4, 4);
        assert!(Canvas::new(&mut v, 5, 4).is_none());
    }
}
