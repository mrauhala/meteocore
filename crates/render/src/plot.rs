//! Minimal dependency-free line-chart rasteriser.
//!
//! Renders EDR vertical-profile and time-series responses to PNG without a
//! charting or font crate — it draws into an RGBA buffer with a small
//! embedded 5×7 bitmap font and hands the buffer to [`crate::encode_png`].
//! All label text is upper-cased before drawing so the font only needs
//! `A–Z`, `0–9`, space and a handful of symbols.
//!
//! One [`Panel`] is drawn per parameter (stacked vertically); within a panel
//! every [`Series`] is overlaid in a cycling colour with a legend.

use chrono::DateTime;

use ds_core::error::DataServerError;

use crate::colormap::ColorMap;
use crate::encode_png;

/// One labelled line within a panel. `points` are `(x, y)` in data
/// coordinates; a `None` in *either* coordinate breaks the line (a gap, not a
/// zero). Both ends are optional because the nullable coordinate differs by
/// plot kind: a time series has a null *value* (y), a vertical profile has a
/// null *value* on x with the level always present on y.
#[derive(Debug, Clone)]
pub struct Series {
    pub label: String,
    pub points: Vec<(Option<f64>, Option<f64>)>,
}

/// One stacked sub-plot (one parameter). The vertical axis carries the
/// domain coordinate (profile) or the value (time series); see the api-edr
/// `plot_convert` module for how domains map onto these fields.
#[derive(Debug, Clone)]
pub struct Panel {
    /// Centred title (the parameter label).
    pub title: String,
    /// Horizontal-axis caption.
    pub x_label: String,
    /// Vertical-axis caption.
    pub y_label: String,
    /// When true the y axis grows downward (pressure / model level): the
    /// largest value sits at the bottom.
    pub y_invert: bool,
    /// When true x values are epoch seconds and ticks are formatted as time.
    pub x_is_time: bool,
    pub series: Vec<Series>,
}

/// A 2-D colour-mapped field for [`render_heatmap`] — the EDR `Section`
/// (radar cross-section) plot. `values` is row-major over the along-path
/// axis then the vertical axis: `values[node * z_len + level]`, matching
/// the CoverageJSON `Section` ndarray shape `[n_nodes, n_z]`. A `None`
/// cell renders with the colormap's nodata colour.
#[derive(Debug, Clone)]
pub struct Heatmap {
    /// Centred title (the parameter label).
    pub title: String,
    /// Horizontal-axis caption (e.g. "Distance (km)").
    pub x_label: String,
    /// Vertical-axis caption (e.g. "Height above antenna (m)").
    pub y_label: String,
    /// Caption for the colour bar (the value unit, e.g. "dBZ").
    pub value_label: String,
    /// Monotonic along-path coordinate per column (length `x_len`).
    pub x_values: Vec<f64>,
    /// Monotonic vertical coordinate per row (length `z_len`), ascending.
    pub y_values: Vec<f64>,
    /// Row-major `[node][level]` cell values; length `x_values.len() *
    /// y_values.len()`.
    pub values: Vec<Option<f64>>,
    /// Colour-bar lower / upper bounds (the colormap's value range).
    pub value_min: f64,
    pub value_max: f64,
}

const BLACK: [u8; 3] = [0x20, 0x20, 0x20];
const FRAME: [u8; 3] = [0x55, 0x55, 0x55];
const GRID: [u8; 3] = [0xe2, 0xe2, 0xe2];

/// Distinct, colour-blind-friendly series colours (cycled).
const SERIES_COLORS: [[u8; 3]; 8] = [
    [0x1f, 0x77, 0xb4],
    [0xd6, 0x27, 0x28],
    [0x2c, 0xa0, 0x2c],
    [0xff, 0x7f, 0x0e],
    [0x94, 0x67, 0xbd],
    [0x8c, 0x56, 0x4b],
    [0x17, 0xbe, 0xcf],
    [0xe3, 0x77, 0xc2],
];

/// A mutable RGBA canvas with primitive drawing ops.
struct Canvas {
    w: i32,
    h: i32,
    buf: Vec<u8>,
}

impl Canvas {
    fn new(w: u32, h: u32) -> Self {
        Canvas {
            w: w as i32,
            h: h as i32,
            buf: vec![0xff; (w as usize) * (h as usize) * 4],
        }
    }

    #[inline]
    fn put(&mut self, x: i32, y: i32, c: [u8; 3]) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let i = ((y * self.w + x) as usize) * 4;
        self.buf[i] = c[0];
        self.buf[i + 1] = c[1];
        self.buf[i + 2] = c[2];
        self.buf[i + 3] = 0xff;
    }

    fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, c: [u8; 3]) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.put(xx, yy, c);
            }
        }
    }

    fn hline(&mut self, x0: i32, x1: i32, y: i32, c: [u8; 3]) {
        for x in x0.min(x1)..=x0.max(x1) {
            self.put(x, y, c);
        }
    }

    fn vline(&mut self, x: i32, y0: i32, y1: i32, c: [u8; 3]) {
        for y in y0.min(y1)..=y0.max(y1) {
            self.put(x, y, c);
        }
    }

    /// Bresenham line.
    fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 3]) {
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        let (mut x, mut y) = (x0, y0);
        loop {
            self.put(x, y, c);
            if x == x1 && y == y1 {
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

    fn rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 3]) {
        self.hline(x0, x1, y0, c);
        self.hline(x0, x1, y1, c);
        self.vline(x0, y0, y1, c);
        self.vline(x1, y0, y1, c);
    }

    /// Draw upper-cased `text` with the top-left of the first glyph at
    /// `(x, y)`. Returns the x just past the text.
    fn text(&mut self, x: i32, y: i32, text: &str, c: [u8; 3], scale: i32) -> i32 {
        let mut cx = x;
        for ch in text.chars() {
            let glyph = glyph(ch.to_ascii_uppercase());
            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..GLYPH_W {
                    if bits & (1 << (GLYPH_W - 1 - col)) != 0 {
                        self.fill_rect(
                            cx + col as i32 * scale,
                            y + row as i32 * scale,
                            scale,
                            scale,
                            c,
                        );
                    }
                }
            }
            cx += (GLYPH_W as i32 + 1) * scale;
        }
        cx
    }
}

/// Rendered width of `text` at `scale`, in pixels.
fn text_width(text: &str, scale: i32) -> i32 {
    text.chars().count() as i32 * (GLYPH_W as i32 + 1) * scale
}

/// Render `panels` (stacked vertically) to PNG bytes.
pub fn render_chart(panels: &[Panel], width: u32, height: u32) -> Result<Vec<u8>, DataServerError> {
    if panels.is_empty() {
        return Err(DataServerError::Render("no panels to plot".into()));
    }
    // Bound the worst-case canvas: this runs synchronously on the request
    // worker, so cap the buffer well below the WMS raster sizes.
    let width = width.clamp(160, 2000);
    let height = height.clamp(120, 2000);
    let mut cv = Canvas::new(width, height);

    let n = panels.len() as i32;
    let panel_h = cv.h / n;
    for (i, panel) in panels.iter().enumerate() {
        let top = i as i32 * panel_h;
        draw_panel(&mut cv, panel, top, panel_h);
    }

    encode_png(&cv.buf, width, height)
}

/// Render one or more [`Heatmap`]s (stacked vertically) to PNG bytes —
/// the EDR cross-section (`Section`) plot. Each heatmap is colour-mapped
/// with `colormap`; the value range for the colour bar comes from each
/// heatmap's `value_min`/`value_max`.
pub fn render_heatmap(
    heatmaps: &[Heatmap],
    colormap: &dyn ColorMap,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, DataServerError> {
    if heatmaps.is_empty() {
        return Err(DataServerError::Render("no heatmaps to plot".into()));
    }
    let width = width.clamp(160, 2000);
    let height = height.clamp(120, 2000);
    let mut cv = Canvas::new(width, height);

    let n = heatmaps.len() as i32;
    let panel_h = cv.h / n;
    for (i, hm) in heatmaps.iter().enumerate() {
        let top = i as i32 * panel_h;
        draw_heatmap_panel(&mut cv, hm, colormap, top, panel_h);
    }
    encode_png(&cv.buf, width, height)
}

/// Composite an RGBA colormap sample over the white canvas background and
/// return the opaque RGB to store. Nodata (alpha 0) becomes white.
fn over_white(c: [u8; 4]) -> [u8; 3] {
    let a = c[3] as u32;
    if a == 255 {
        return [c[0], c[1], c[2]];
    }
    let blend = |ch: u8| -> u8 { ((ch as u32 * a + 255 * (255 - a)) / 255) as u8 };
    [blend(c[0]), blend(c[1]), blend(c[2])]
}

fn draw_heatmap_panel(
    cv: &mut Canvas,
    hm: &Heatmap,
    colormap: &dyn ColorMap,
    top: i32,
    panel_h: i32,
) {
    // Margins: extra room on the right for the colour bar.
    let m_left = 58;
    let m_right = 64;
    let m_top = 26;
    let m_bottom = 30;

    let x0 = m_left;
    let x1 = cv.w - m_right;
    let y0 = top + m_top;
    let y1 = top + panel_h - m_bottom;
    if x1 <= x0 + 4 || y1 <= y0 + 4 {
        return;
    }

    // Centred title.
    let title = hm.title.to_ascii_uppercase();
    let tw = text_width(&title, 2);
    cv.text((cv.w - tw) / 2, top + 6, &title, BLACK, 2);

    let nx = hm.x_values.len();
    let nz = hm.y_values.len();
    if nx == 0 || nz == 0 || hm.values.len() != nx * nz {
        cv.rect(x0, y0, x1, y1, FRAME);
        let msg = "NO DATA";
        let w = text_width(msg, 2);
        cv.text((x0 + x1) / 2 - w / 2, (y0 + y1) / 2 - 7, msg, FRAME, 2);
        draw_heatmap_captions(cv, hm, x0, x1, y1, top);
        return;
    }

    let (ymin, ymax) = (hm.y_values[0], hm.y_values[nz - 1]);

    // Fill the plot area. Each output pixel maps to a (node, level) cell:
    // the column is index-linear in node, the row is *value-linear* in the
    // vertical coordinate — the pixel's height value is found, then its
    // nearest level by value. Value-linear matches the y-tick placement
    // below (`py`), so labels line up even when the z levels are not
    // uniformly spaced (a caller passing irregular `y_values`).
    let pw = x1 - x0;
    let ph = y1 - y0;
    let nearest_level = |h: f64| -> usize {
        // y_values is ascending; pick the index minimising |value − h|.
        let mut best = 0usize;
        let mut best_d = f64::INFINITY;
        for (i, &yv) in hm.y_values.iter().enumerate() {
            let d = (yv - h).abs();
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        best
    };
    let span = (ymax - ymin).max(f64::EPSILON);
    for sy in 0..ph {
        // Screen y grows downward; the top row is the max height value.
        let frac_y = (sy as f64 + 0.5) / ph as f64;
        let h = ymax - frac_y * span;
        let level = nearest_level(h);
        for sx in 0..pw {
            let frac_x = (sx as f64 + 0.5) / pw as f64;
            let node = ((frac_x * nx as f64) as usize).min(nx - 1);
            let v = hm.values[node * nz + level];
            let rgb = over_white(colormap.color(v));
            cv.put(x0 + sx, y0 + sy, rgb);
        }
    }
    cv.rect(x0, y0, x1, y1, FRAME);

    // Y ticks (height) — value grows up, so the top is the max.
    let py = |y: f64| -> i32 {
        let frac = (y - ymin) / (ymax - ymin).max(f64::EPSILON);
        y1 - (frac * (y1 - y0) as f64).round() as i32
    };
    for t in nice_ticks(ymin, ymax, 5) {
        if t < ymin || t > ymax {
            continue;
        }
        let yy = py(t);
        cv.hline(x0 - 3, x0, yy, FRAME);
        let label = format_value(t);
        let lw = text_width(&label, 1);
        cv.text(x0 - 6 - lw, yy - 3, &label, BLACK, 1);
    }

    // X ticks (distance).
    let (xmin, xmax) = (hm.x_values[0], hm.x_values[nx - 1]);
    let px = |x: f64| -> i32 {
        let frac = (x - xmin) / (xmax - xmin).max(f64::EPSILON);
        x0 + (frac * (x1 - x0) as f64).round() as i32
    };
    for t in nice_ticks(xmin, xmax, 5) {
        if t < xmin || t > xmax {
            continue;
        }
        let xx = px(t);
        cv.vline(xx, y1, y1 + 3, FRAME);
        let label = format_value(t);
        let lw = text_width(&label, 1);
        cv.text(xx - lw / 2, y1 + 6, &label, BLACK, 1);
    }

    draw_colorbar(cv, hm, colormap, x1, y0, y1);
    draw_heatmap_captions(cv, hm, x0, x1, y1, top);
}

/// Vertical colour bar just right of the plot frame, with min/mid/max
/// value ticks.
fn draw_colorbar(
    cv: &mut Canvas,
    hm: &Heatmap,
    colormap: &dyn ColorMap,
    x1: i32,
    y0: i32,
    y1: i32,
) {
    let bar_x = x1 + 10;
    let bar_w = 12;
    let h = y1 - y0;
    if h <= 1 {
        return;
    }
    for sy in 0..h {
        // Top = max value.
        let frac = 1.0 - (sy as f64 + 0.5) / h as f64;
        let v = hm.value_min + frac * (hm.value_max - hm.value_min);
        let rgb = over_white(colormap.color(Some(v)));
        for bx in 0..bar_w {
            cv.put(bar_x + bx, y0 + sy, rgb);
        }
    }
    cv.rect(bar_x, y0, bar_x + bar_w, y1, FRAME);

    // Three labels: max (top), mid, min (bottom).
    let label_at = |cv: &mut Canvas, frac: f64, v: f64| {
        let yy = y1 - (frac * h as f64).round() as i32;
        let label = format_value(v);
        cv.hline(bar_x + bar_w, bar_x + bar_w + 3, yy, FRAME);
        cv.text(bar_x + bar_w + 5, yy - 3, &label, BLACK, 1);
    };
    let mid = (hm.value_min + hm.value_max) / 2.0;
    label_at(cv, 1.0, hm.value_max);
    label_at(cv, 0.5, mid);
    label_at(cv, 0.0, hm.value_min);

    // Colour-bar caption (the unit) just above the bar.
    if !hm.value_label.is_empty() {
        let cap = hm.value_label.to_ascii_uppercase();
        cv.text(bar_x, y0 - 9, &cap, BLACK, 1);
    }
}

fn draw_heatmap_captions(cv: &mut Canvas, hm: &Heatmap, x0: i32, x1: i32, y1: i32, top: i32) {
    let y_cap = hm.y_label.to_ascii_uppercase();
    cv.text(4, top + 16, &y_cap, BLACK, 1);
    let x_cap = hm.x_label.to_ascii_uppercase();
    let w = text_width(&x_cap, 1);
    cv.text((x0 + x1) / 2 - w / 2, y1 + 16, &x_cap, BLACK, 1);
}

fn draw_panel(cv: &mut Canvas, panel: &Panel, top: i32, panel_h: i32) {
    // Margins carve the plot frame out of the panel's slice.
    let m_left = 58;
    let m_right = 12;
    let m_top = 26;
    let m_bottom = 30;

    let x0 = m_left;
    let x1 = cv.w - m_right;
    let y0 = top + m_top;
    let y1 = top + panel_h - m_bottom;
    if x1 <= x0 + 4 || y1 <= y0 + 4 {
        return; // panel too small to draw anything meaningful
    }

    // Centred panel title.
    let title = panel.title.to_ascii_uppercase();
    let tw = text_width(&title, 2);
    cv.text((cv.w - tw) / 2, top + 6, &title, BLACK, 2);

    // Collect finite data bounds across every series.
    let mut xmin = f64::INFINITY;
    let mut xmax = f64::NEG_INFINITY;
    let mut ymin = f64::INFINITY;
    let mut ymax = f64::NEG_INFINITY;
    for s in &panel.series {
        for &(x, y) in &s.points {
            let (Some(x), Some(y)) = (x, y) else { continue };
            if !x.is_finite() || !y.is_finite() {
                continue;
            }
            xmin = xmin.min(x);
            xmax = xmax.max(x);
            ymin = ymin.min(y);
            ymax = ymax.max(y);
        }
    }

    cv.rect(x0, y0, x1, y1, FRAME);

    if !xmin.is_finite() || !ymin.is_finite() {
        let msg = "NO DATA";
        let w = text_width(msg, 2);
        cv.text((x0 + x1) / 2 - w / 2, (y0 + y1) / 2 - 7, msg, FRAME, 2);
        // Still label the axes so the empty plot is self-describing.
        draw_axis_captions(cv, panel, x0, x1, y1, top);
        return;
    }

    // Pad ranges; expand a degenerate (single-value) range so it maps.
    let (xmin, xmax) = pad_range(xmin, xmax);
    let (ymin, ymax) = pad_range(ymin, ymax);

    let px =
        |x: f64| -> i32 { x0 + (((x - xmin) / (xmax - xmin)) * (x1 - x0) as f64).round() as i32 };
    let py = |y: f64| -> i32 {
        let frac = (y - ymin) / (ymax - ymin);
        // Screen y grows downward: an "up" axis puts ymax at the top (y0);
        // an inverted ("down") axis puts ymax at the bottom (y1).
        if panel.y_invert {
            y0 + (frac * (y1 - y0) as f64).round() as i32
        } else {
            y1 - (frac * (y1 - y0) as f64).round() as i32
        }
    };

    // Y ticks + gridlines + labels.
    for t in nice_ticks(ymin, ymax, 5) {
        if t < ymin || t > ymax {
            continue;
        }
        let yy = py(t);
        cv.hline(x0 + 1, x1 - 1, yy, GRID);
        cv.hline(x0 - 3, x0, yy, FRAME);
        let label = format_value(t);
        let lw = text_width(&label, 1);
        cv.text(x0 - 6 - lw, yy - 3, &label, BLACK, 1);
    }

    // X ticks + gridlines + labels. A time axis spanning more than a day
    // needs the date too, else both ends can read "00:00".
    let multiday = panel.x_is_time && (xmax - xmin) > 86_400.0;
    for t in x_ticks(panel, xmin, xmax, 5) {
        if t < xmin || t > xmax {
            continue;
        }
        let xx = px(t);
        cv.vline(xx, y0 + 1, y1 - 1, GRID);
        cv.vline(xx, y1, y1 + 3, FRAME);
        let label = if panel.x_is_time {
            format_time(t, multiday)
        } else {
            format_value(t)
        };
        let lw = text_width(&label, 1);
        cv.text(xx - lw / 2, y1 + 6, &label, BLACK, 1);
    }

    // Series polylines + point markers.
    for (i, s) in panel.series.iter().enumerate() {
        let color = SERIES_COLORS[i % SERIES_COLORS.len()];
        let mut prev: Option<(i32, i32)> = None;
        for &(x, y) in &s.points {
            match (x, y) {
                (Some(x), Some(y)) if x.is_finite() && y.is_finite() => {
                    let p = (px(x), py(y));
                    if let Some(q) = prev {
                        cv.line(q.0, q.1, p.0, p.1, color);
                    }
                    cv.fill_rect(p.0 - 1, p.1 - 1, 3, 3, color);
                    prev = Some(p);
                }
                _ => prev = None, // gap
            }
        }
    }

    draw_legend(cv, panel, x0, y0);
    draw_axis_captions(cv, panel, x0, x1, y1, top);
}

/// Y-axis caption (horizontal, top-left above the frame) and centred X-axis
/// caption (below the frame).
fn draw_axis_captions(cv: &mut Canvas, panel: &Panel, x0: i32, x1: i32, y1: i32, top: i32) {
    let y_cap = panel.y_label.to_ascii_uppercase();
    cv.text(4, top + 16, &y_cap, BLACK, 1);

    let x_cap = panel.x_label.to_ascii_uppercase();
    let w = text_width(&x_cap, 1);
    cv.text((x0 + x1) / 2 - w / 2, y1 + 16, &x_cap, BLACK, 1);
}

/// Top-right legend: a colour swatch + label per series.
fn draw_legend(cv: &mut Canvas, panel: &Panel, x0: i32, y0: i32) {
    let any_labeled = panel.series.iter().any(|s| !s.label.is_empty());
    if panel.series.len() < 2 && !any_labeled {
        return; // a single unlabelled series needs no legend
    }
    let mut ly = y0 + 4;
    for (i, s) in panel.series.iter().enumerate() {
        let color = SERIES_COLORS[i % SERIES_COLORS.len()];
        let label = s.label.to_ascii_uppercase();
        let lw = text_width(&label, 1);
        let lx = x0 + 8;
        // Right-aligned block: swatch (8px) + gap (3px) + text.
        let block_w = 8 + 3 + lw;
        let bx = (cv.w - 14 - block_w).max(lx);
        cv.fill_rect(bx, ly + 1, 8, 5, color);
        cv.text(bx + 11, ly, &label, BLACK, 1);
        ly += 9;
    }
}

/// Expand a data range by 5%, widening a degenerate range to ±1 (or ±|v|).
fn pad_range(min: f64, max: f64) -> (f64, f64) {
    if (max - min).abs() < f64::EPSILON {
        let d = if min.abs() > 1.0 {
            min.abs() * 0.1
        } else {
            1.0
        };
        return (min - d, max + d);
    }
    let pad = (max - min) * 0.05;
    (min - pad, max + pad)
}

/// ~`n` round tick values spanning `[min, max]` (1/2/5 × 10^k steps).
fn nice_ticks(min: f64, max: f64, n: usize) -> Vec<f64> {
    if !(min.is_finite() && max.is_finite()) || max <= min || n == 0 {
        return vec![];
    }
    let range = max - min;
    let raw = range / n as f64;
    let mag = 10f64.powf(raw.log10().floor());
    let norm = raw / mag;
    let step = if norm < 1.5 {
        mag
    } else if norm < 3.0 {
        2.0 * mag
    } else if norm < 7.0 {
        5.0 * mag
    } else {
        10.0 * mag
    };
    let mut t = (min / step).ceil() * step;
    let mut out = Vec::new();
    while t <= max + step * 0.5 && out.len() < 64 {
        // Snap values that are integers-in-disguise (e.g. 1.9999999).
        out.push((t / step).round() * step);
        t += step;
    }
    out
}

/// Tick values for the x axis — nice numbers for values, evenly spaced for time.
fn x_ticks(panel: &Panel, min: f64, max: f64, n: usize) -> Vec<f64> {
    if panel.x_is_time {
        if max <= min {
            return vec![min];
        }
        (0..=n)
            .map(|i| min + (max - min) * i as f64 / n as f64)
            .collect()
    } else {
        nice_ticks(min, max, n)
    }
}

/// Format a numeric tick: integer when whole, else one decimal, dropping a
/// trailing `.0`.
fn format_value(v: f64) -> String {
    let r = (v * 10.0).round() / 10.0;
    if (r - r.round()).abs() < 0.05 && r.abs() < 1e7 {
        format!("{}", r.round() as i64)
    } else {
        format!("{r:.1}")
    }
}

/// Format epoch seconds as `HH:MM` (UTC), or `MM-DD HH:MM` when the axis
/// spans more than a day and bare times would be ambiguous.
fn format_time(epoch_secs: f64, with_date: bool) -> String {
    match DateTime::from_timestamp(epoch_secs.round() as i64, 0) {
        Some(dt) if with_date => dt.format("%m-%d %H:%M").to_string(),
        Some(dt) => dt.format("%H:%M").to_string(),
        None => String::new(),
    }
}

const GLYPH_W: usize = 5;

/// 5×7 glyph (7 rows, low 5 bits each, MSB = leftmost column). Covers the
/// upper-cased label charset; unknown glyphs render as a hollow box.
fn glyph(c: char) -> [u8; 7] {
    match c {
        ' ' => [0; 7],
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ],
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        'C' => [
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ],
        'D' => [
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'F' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'G' => [
            0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111,
        ],
        'H' => [
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'I' => [
            0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        'J' => [
            0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100,
        ],
        'K' => [
            0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'M' => [
            0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
        ],
        'N' => [
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'Q' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'U' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'V' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        'W' => [
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b10101, 0b01010,
        ],
        'X' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
        ],
        'Y' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'Z' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111,
        ],
        '.' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00110, 0b00110,
        ],
        ',' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00110, 0b00110, 0b01100,
        ],
        '-' => [
            0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000,
        ],
        '+' => [
            0b00000, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0b00000,
        ],
        ':' => [
            0b00000, 0b00110, 0b00110, 0b00000, 0b00110, 0b00110, 0b00000,
        ],
        '/' => [
            0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000,
        ],
        '(' => [
            0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00100, 0b00010,
        ],
        ')' => [
            0b01000, 0b00100, 0b00010, 0b00010, 0b00010, 0b00100, 0b01000,
        ],
        '%' => [
            0b11001, 0b11010, 0b00100, 0b01000, 0b10011, 0b00011, 0b00000,
        ],
        _ => [
            0b11111, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11111,
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_dims(png: &[u8]) -> (u32, u32) {
        // PNG IHDR width/height are big-endian u32 at bytes 16..24.
        assert_eq!(&png[0..8], b"\x89PNG\r\n\x1a\n", "PNG signature");
        let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
        let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
        (w, h)
    }

    fn sample_panel() -> Panel {
        Panel {
            title: "DBZH".into(),
            x_label: "Reflectivity (dBZ)".into(),
            y_label: "Elevation angle (deg)".into(),
            y_invert: false,
            x_is_time: false,
            series: vec![Series {
                label: "00:00".into(),
                points: vec![
                    (Some(5.0), Some(0.5)),
                    (Some(12.0), Some(2.0)),
                    (Some(8.0), Some(5.0)),
                    (Some(3.0), Some(9.0)),
                ],
            }],
        }
    }

    #[test]
    fn renders_valid_png_of_requested_size() {
        let png = render_chart(&[sample_panel()], 800, 600).unwrap();
        assert_eq!(decode_dims(&png), (800, 600));
    }

    #[test]
    fn render_is_deterministic() {
        let a = render_chart(&[sample_panel()], 640, 480).unwrap();
        let b = render_chart(&[sample_panel()], 640, 480).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn all_null_series_still_renders() {
        let panel = Panel {
            title: "VRADH".into(),
            x_label: "x".into(),
            y_label: "y".into(),
            y_invert: false,
            x_is_time: false,
            series: vec![Series {
                label: "s".into(),
                points: vec![(Some(1.0), None), (Some(2.0), None)],
            }],
        };
        let png = render_chart(&[panel], 400, 300).unwrap();
        assert_eq!(decode_dims(&png), (400, 300));
    }

    #[test]
    fn multi_panel_stacks() {
        let png = render_chart(&[sample_panel(), sample_panel()], 500, 600).unwrap();
        assert_eq!(decode_dims(&png), (500, 600));
    }

    #[test]
    fn dimensions_are_clamped() {
        let png = render_chart(&[sample_panel()], 10, 10).unwrap();
        // Clamped up to the 160×120 floor.
        assert_eq!(decode_dims(&png), (160, 120));
    }

    #[test]
    fn empty_panels_is_error() {
        assert!(render_chart(&[], 100, 100).is_err());
    }

    #[test]
    fn nice_ticks_are_round() {
        let t = nice_ticks(0.0, 10.0, 5);
        assert!(t.contains(&0.0) || t.contains(&2.0));
        assert!(t.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn time_axis_ticks_are_evenly_spaced() {
        let p = Panel {
            title: "T".into(),
            x_label: "TIME".into(),
            y_label: "V".into(),
            y_invert: false,
            x_is_time: true,
            series: vec![],
        };
        let t = x_ticks(&p, 0.0, 3600.0, 4);
        assert_eq!(t.len(), 5);
        assert_eq!(t[0], 0.0);
        assert_eq!(t[4], 3600.0);
    }

    #[test]
    fn format_value_drops_trailing_zero() {
        assert_eq!(format_value(2.0), "2");
        assert_eq!(format_value(2.5), "2.5");
        assert_eq!(format_value(-12.34), "-12.3");
    }

    #[test]
    fn format_time_is_hh_mm() {
        let ts = chrono::DateTime::parse_from_rfc3339("2026-05-15T13:45:00Z")
            .unwrap()
            .timestamp() as f64;
        assert_eq!(format_time(ts, false), "13:45");
        assert_eq!(format_time(ts, true), "05-15 13:45");
    }

    fn sample_heatmap() -> Heatmap {
        // 4 nodes × 3 levels, row-major [node][level].
        let values = vec![
            Some(10.0),
            Some(20.0),
            Some(30.0), // node 0
            Some(15.0),
            None,
            Some(35.0), // node 1 (one nodata cell)
            Some(5.0),
            Some(25.0),
            Some(40.0), // node 2
            Some(0.0),
            Some(12.0),
            Some(28.0), // node 3
        ];
        Heatmap {
            title: "DBZH".into(),
            x_label: "Distance (km)".into(),
            y_label: "Height (m)".into(),
            value_label: "dBZ".into(),
            x_values: vec![0.0, 10.0, 20.0, 30.0],
            y_values: vec![0.0, 1000.0, 2000.0],
            values,
            value_min: 0.0,
            value_max: 40.0,
        }
    }

    #[test]
    fn heatmap_renders_valid_png_of_requested_size() {
        let cmap = crate::colormap::LutColorMap::from_builtin(
            crate::colormap::BuiltinColormap::Viridis,
            0.0,
            40.0,
        );
        let png = render_heatmap(&[sample_heatmap()], &cmap, 800, 400).unwrap();
        assert_eq!(decode_dims(&png), (800, 400));
    }

    #[test]
    fn heatmap_render_is_deterministic() {
        let cmap = crate::colormap::LutColorMap::from_builtin(
            crate::colormap::BuiltinColormap::RadarDbz,
            -32.0,
            95.0,
        );
        let a = render_heatmap(&[sample_heatmap()], &cmap, 400, 300).unwrap();
        let b = render_heatmap(&[sample_heatmap()], &cmap, 400, 300).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn heatmap_empty_is_error() {
        let cmap = crate::colormap::LutColorMap::from_builtin(
            crate::colormap::BuiltinColormap::Viridis,
            0.0,
            1.0,
        );
        assert!(render_heatmap(&[], &cmap, 100, 100).is_err());
    }

    #[test]
    fn heatmap_mismatched_values_renders_no_data() {
        // values length != nx*nz → "NO DATA" panel, but still a valid PNG.
        let cmap = crate::colormap::LutColorMap::from_builtin(
            crate::colormap::BuiltinColormap::Viridis,
            0.0,
            1.0,
        );
        let bad = Heatmap {
            title: "T".into(),
            x_label: "x".into(),
            y_label: "y".into(),
            value_label: "u".into(),
            x_values: vec![0.0, 1.0],
            y_values: vec![0.0, 1.0],
            values: vec![Some(1.0)], // should be 4
            value_min: 0.0,
            value_max: 1.0,
        };
        let png = render_heatmap(&[bad], &cmap, 300, 200).unwrap();
        assert_eq!(decode_dims(&png), (300, 200));
    }

    #[test]
    fn over_white_blends_transparent_to_white() {
        assert_eq!(over_white([0, 0, 0, 0]), [255, 255, 255]);
        assert_eq!(over_white([10, 20, 30, 255]), [10, 20, 30]);
        // 50% black over white ≈ mid grey.
        let g = over_white([0, 0, 0, 128]);
        assert!(g[0] > 120 && g[0] < 135);
    }

    #[test]
    fn heatmap_handles_non_uniform_z_levels() {
        // Strongly non-uniform z levels (0, 100, 9000 m). Value-linear
        // cell-fill + tick placement must agree, and the result is a
        // valid PNG of the requested size. Before the fix the bottom two
        // bands (0 and 100 m) each occupied a third of the height while
        // ticks were placed by value — labels floated off their bands.
        let cmap = crate::colormap::LutColorMap::from_builtin(
            crate::colormap::BuiltinColormap::Viridis,
            0.0,
            30.0,
        );
        let hm = Heatmap {
            title: "DBZH".into(),
            x_label: "Distance (km)".into(),
            y_label: "Height (m)".into(),
            value_label: "dBZ".into(),
            x_values: vec![0.0, 10.0, 20.0],
            y_values: vec![0.0, 100.0, 9000.0],
            values: vec![
                Some(5.0),
                Some(10.0),
                Some(15.0),
                Some(20.0),
                Some(25.0),
                Some(30.0),
            ],
            value_min: 0.0,
            value_max: 30.0,
        };
        let png = render_heatmap(&[hm], &cmap, 600, 400).unwrap();
        assert_eq!(decode_dims(&png), (600, 400));
    }

    #[test]
    fn font_draws_known_pixels() {
        // 'I' (0x0E top row 01110) should set the middle columns of a glyph.
        let mut cv = Canvas::new(20, 20);
        cv.text(2, 2, "I", BLACK, 1);
        // top row of 'I' spans cols 1..4 (bits 01110) at y=2.
        let lit = |x: i32, y: i32| -> bool {
            let i = ((y * cv.w + x) as usize) * 4;
            cv.buf[i] != 0xff
        };
        assert!(lit(3, 2), "I top bar pixel set");
        assert!(!lit(2, 2), "I top-left corner clear");
    }
}
