//! Contour lines as a map style (#1378): isobars, isotherms, height
//! contours, drawn into the RGBA image every map API serves.
//!
//! A contour style reads its field through the normal
//! `MapEngine::get_raster_tile` path at the output grid, grown by a few
//! pixels on every side ([`ContourPlan`]). Projection, time and run
//! selection, cache keys and budgets are the colormap path's, and nothing
//! projects per pixel (root CLAUDE.md Critical Rules 4 and 5). Marching
//! squares over that grid traces the lines, which are drawn anti-aliased
//! into the view, optionally over the style's colormap fill.
//!
//! **Tiles meet.** A line leaving one tile meets the same line in the next,
//! so a 2 × 2 block of tiles is one render at twice the size: both trace the
//! same cells from the same samples. That holds because
//!
//! - the view is grown by [`ContourStyle::line_margin`] pixels, at least the
//!   reach of a line's anti-aliasing, so every segment that touches a view
//!   pixel is traced, also one in a neighbour's cells;
//! - an edge crossing is interpolated from its lower-index sample, so the
//!   two cells sharing an edge, in any tile, find one point;
//! - a saddle is decided by its cell's four corners (their mean), never by
//!   a neighbour;
//! - a pixel's coverage is the largest any segment gives it, which does not
//!   depend on the order segments are drawn in;
//! - smoothing, when it applies, is a function of each sample's
//!   neighbourhood, read from a margin wide enough to hold it, and decided
//!   per zoom, not per tile ([`smoothing_passes`]).
//!
//! **Nodata.** A cell with a nodata corner traces nothing, so lines end where
//! the data does.
//!
//! **Smoothing.** Where the source is much finer than the output, the samples
//! alias its small-scale structure and the lines come out jagged. With two
//! or more source cells per output pixel the grid is smoothed lightly
//! (binomial 1-2-1 passes) before tracing. A finer view than the source
//! traces the samples as read: they are the engine's bilinear interpolation
//! of the source.

use std::sync::Arc;

use ds_core::error::DataServerError;
use ds_core::map_engine::{OutputCrs, RasterInfo, RasterTile, RasterValues};

use crate::colormap::ColorMap;
use crate::{font, ImageFormat, StyleInfo};

/// The name of the contour style the built-in parameter defaults offer on
/// a parameter layer next to its `default` fill (#1378).
pub const CONTOUR_STYLE: &str = "contours";

/// Levels traced in one grid cell at most. A field whose interval puts
/// more lines than this through one output cell draws a solid band
/// anyway; the cap bounds the work of a misconfigured interval.
const MAX_LEVELS_PER_CELL: i64 = 64;

/// Metres of the equator per degree of longitude, to compare a projected
/// output pixel with a lat/lon source cell.
const METRES_PER_DEGREE: f64 = ds_core::web_mercator::EARTH_RADIUS * std::f64::consts::PI / 180.0;

/// A render-ready contour style: lines at every `base + k × interval`, in
/// the parameter's served unit.
#[derive(Debug, Clone, PartialEq)]
pub struct ContourStyle {
    pub interval: f64,
    pub base: f64,
    /// Straight-alpha RGBA line colour.
    pub color: [u8; 4],
    /// Line width in output pixels.
    pub width: f64,
    /// Whether the lines are drawn over the style's colormap
    /// ([`StyleInfo::colormap`]); otherwise they are drawn alone.
    pub fill: bool,
}

impl ContourStyle {
    /// A style from config values, with the defaults for an unset colour
    /// and width. The values were validated at load
    /// (`ds_core::config::ContourConfig::validate`); an invalid one here is
    /// an error, never a guess.
    pub fn new(
        interval: f64,
        base: f64,
        color: Option<&str>,
        width: Option<f64>,
        fill: bool,
    ) -> Result<Self, String> {
        let color =
            crate::parse_hex_color(color.unwrap_or(ds_core::config::DEFAULT_CONTOUR_COLOR))?;
        let width = width.unwrap_or(ds_core::config::DEFAULT_CONTOUR_WIDTH);
        if !(interval.is_finite() && interval > 0.0) || !base.is_finite() {
            return Err(format!(
                "invalid contour interval {interval} or base {base}"
            ));
        }
        if !(width.is_finite() && width > 0.0 && width <= ds_core::config::MAX_CONTOUR_WIDTH) {
            return Err(format!("invalid contour width {width}"));
        }
        Ok(Self {
            interval,
            base,
            color,
            width,
            fill,
        })
    }

    /// What the lines mark, for a legend: `every 4 hPa`, and the value a
    /// line passes through when it is not a multiple of the interval
    /// (`every 2 K through 273.15 K`).
    pub fn description(&self, unit: Option<&str>) -> String {
        let unit = unit.map(str::trim).filter(|u| !u.is_empty());
        let with_unit = |v: f64| match unit {
            Some(u) => format!("{} {u}", crate::format_tick(v)),
            None => crate::format_tick(v),
        };
        let mut text = format!("every {}", with_unit(self.interval));
        let offset = (self.base / self.interval).fract().abs();
        if offset > 1e-9 && offset < 1.0 - 1e-9 {
            text.push_str(&format!(" through {}", with_unit(self.base)));
        }
        text
    }

    /// The pixels a render reads past each edge of its view so that every
    /// line touching the view is traced: the reach of a line's
    /// anti-aliasing, `width / 2 + 0.5`, rounded up.
    pub fn line_margin(&self) -> u32 {
        (self.width / 2.0 + 0.5).ceil().max(1.0) as u32
    }

    /// The legend's `contour` member: interval, base, colour, width and
    /// [`Self::description`].
    pub fn legend_json(&self, unit: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "interval": self.interval,
            "base": self.base,
            "color": crate::palette::Palette::color_hex(self.color),
            "width": self.width,
            "fill": self.fill,
            "description": self.description(unit),
        })
    }
}

/// How a map request paints a contour style: the lines, the colormap under
/// them when the style has a fill, and the source's cell size, which
/// decides the smoothing ([`ContourPlan::new`]).
#[derive(Clone)]
pub struct ContourPaint {
    pub style: Arc<ContourStyle>,
    pub fill: Option<Arc<dyn ColorMap>>,
    pub source_deg: Option<f64>,
}

impl ContourPaint {
    /// The paint of `style` on a collection described by `info`, `None`
    /// when the style draws no contours. O(1), per request.
    pub fn of(style: &StyleInfo, info: &RasterInfo) -> Option<Self> {
        let contour = style.contour.clone()?;
        let fill = contour.fill.then(|| style.colormap.clone());
        Some(Self {
            style: contour,
            fill,
            source_deg: source_spacing_deg(info),
        })
    }

    /// The plan for a `width × height` view of `bbox` in `output_crs`.
    pub fn plan(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        output_crs: &OutputCrs,
    ) -> ContourPlan {
        ContourPlan::new(
            &self.style,
            bbox,
            width,
            height,
            output_crs,
            self.source_deg,
        )
    }

    /// [`ContourPlan::draw`] of this paint.
    pub fn draw(
        &self,
        plan: &ContourPlan,
        tile: &RasterTile,
    ) -> Result<Option<Vec<u8>>, DataServerError> {
        plan.draw(tile, &self.style, self.fill.as_deref())
    }
}

/// The source's cell size in degrees of longitude (a projected source's
/// metres over [`METRES_PER_DEGREE`]), from the grid the engine reports,
/// else from its extent and cell counts; `None` when it reports neither.
/// O(1): read per request, never recomputed from data.
pub fn source_spacing_deg(info: &RasterInfo) -> Option<f64> {
    let spacing = match &info.native_grid {
        Some(grid) => {
            let dx = grid.spacing()[0];
            match grid.crs() {
                ds_core::geo::Crs::Wgs84 | ds_core::geo::Crs::RotatedLatLon { .. } => dx,
                _ => dx / METRES_PER_DEGREE,
            }
        }
        None => {
            let [west, _, east, _] = info.spatial_extent?;
            let [nx, _] = info.grid_size?;
            (east - west) / f64::from(nx.max(1))
        }
    };
    (spacing.is_finite() && spacing > 0.0).then_some(spacing)
}

/// An output pixel's width in degrees of longitude, the unit
/// [`source_spacing_deg`] compares with: the same for every tile of one
/// zoom, so neighbouring tiles smooth alike.
fn output_spacing_deg(bbox: [f64; 4], width: u32, output_crs: &OutputCrs) -> f64 {
    let width = f64::from(width.max(1));
    match output_crs {
        OutputCrs::Wgs84 | OutputCrs::WebMercator => (bbox[2] - bbox[0]) / width,
        OutputCrs::Projected { bbox, .. } => (bbox[2] - bbox[0]) / width / METRES_PER_DEGREE,
    }
}

/// Binomial smoothing passes before tracing: none while an output pixel
/// spans less than two source cells, one up to four, two beyond.
pub fn smoothing_passes(output_deg: f64, source_deg: Option<f64>) -> u32 {
    let Some(source) = source_deg.filter(|s| s.is_finite() && *s > 0.0) else {
        return 0;
    };
    let cells_per_pixel = output_deg.abs() / source;
    if !cells_per_pixel.is_finite() || cells_per_pixel < 2.0 {
        0
    } else if cells_per_pixel < 4.0 {
        1
    } else {
        2
    }
}

/// The geometry of one contour render: the `width × height` view, and the
/// grown view the engine is asked for ([`OutputCrs::grown_by_pixels`]).
#[derive(Debug, Clone)]
pub struct ContourPlan {
    width: u32,
    height: u32,
    line_margin: u32,
    smoothing: u32,
    fetch_bbox: [f64; 4],
    fetch_crs: OutputCrs,
}

impl ContourPlan {
    /// The plan for drawing `style` over the view `bbox` (WGS84, the
    /// engine's read window) of `width × height` in `output_crs`.
    /// `source_deg` is [`source_spacing_deg`] of the collection, which
    /// decides the smoothing.
    pub fn new(
        style: &ContourStyle,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        output_crs: &OutputCrs,
        source_deg: Option<f64>,
    ) -> Self {
        let line_margin = style.line_margin();
        let smoothing = smoothing_passes(output_spacing_deg(bbox, width, output_crs), source_deg);
        let (fetch_bbox, fetch_crs) =
            output_crs.grown_by_pixels(bbox, width, height, line_margin + smoothing);
        Self {
            width,
            height,
            line_margin,
            smoothing,
            fetch_bbox,
            fetch_crs,
        }
    }

    /// Pixels read past each edge of the view.
    fn grow(&self) -> u32 {
        self.line_margin + self.smoothing
    }

    /// The WGS84 box to ask the engine for.
    pub fn fetch_bbox(&self) -> [f64; 4] {
        self.fetch_bbox
    }

    /// The output CRS to ask the engine for (a projected one carries its
    /// grown metres).
    pub fn fetch_crs(&self) -> &OutputCrs {
        &self.fetch_crs
    }

    pub fn fetch_width(&self) -> u32 {
        self.width + 2 * self.grow()
    }

    pub fn fetch_height(&self) -> u32 {
        self.height + 2 * self.grow()
    }

    /// Smoothing passes applied before tracing.
    pub fn smoothing(&self) -> u32 {
        self.smoothing
    }

    /// The view's RGBA pixels from the engine's tile of the grown view:
    /// the fill (when `fill` is given) under the lines of `style`. `None`
    /// when nothing is drawn: no data, or lines only and none in view.
    pub fn draw(
        &self,
        tile: &RasterTile,
        style: &ContourStyle,
        fill: Option<&dyn ColorMap>,
    ) -> Result<Option<Vec<u8>>, DataServerError> {
        let (fw, fh) = (self.fetch_width(), self.fetch_height());
        if tile.width != fw || tile.height != fh || tile.values.len() != fw as usize * fh as usize {
            return Err(DataServerError::Render(format!(
                "contour render got a {}x{} tile, expected {fw}x{fh}",
                tile.width, tile.height
            )));
        }
        ds_core::deadline::check()?;
        if tile.is_empty() {
            return Ok(None);
        }
        let grow = self.grow();
        let (w, h) = (self.width as usize, self.height as usize);
        let mut rgba = match fill {
            Some(colormap) => {
                crate::colorize_window(tile, colormap, grow, grow, self.width, self.height)
            }
            None => vec![0u8; w * h * 4],
        };
        let mut grid = Grid::from_tile(tile);
        for _ in 0..self.smoothing {
            ds_core::deadline::check()?;
            grid.smooth();
        }
        let mut coverage = vec![0u8; w * h];
        let mut canvas = Canvas {
            coverage: &mut coverage,
            width: self.width,
            height: self.height,
            half_width: style.width / 2.0,
        };
        trace(
            &grid,
            self.smoothing,
            f64::from(grow),
            style,
            &mut |p, q| canvas.segment(p, q),
        )?;
        drop(grid);
        paint_lines(&mut rgba, &coverage, style.color);
        Ok(rgba
            .as_chunks::<4>()
            .0
            .iter()
            .any(|p| p[3] != 0)
            .then_some(rgba))
    }
}

/// A grid of samples, row-major, NaN for nodata.
struct Grid {
    width: usize,
    height: usize,
    values: Vec<f32>,
}

impl Grid {
    fn from_tile(tile: &RasterTile) -> Self {
        let values = match &tile.values {
            RasterValues::F32 { data, nodata } => data
                .iter()
                .map(|&raw| match RasterValues::decode_f32(raw, *nodata) {
                    Some(_) => raw,
                    None => f32::NAN,
                })
                .collect(),
            values => values
                .iter_values()
                .map(|v| v.map_or(f32::NAN, |v| v as f32))
                .collect(),
        };
        Self {
            width: tile.width as usize,
            height: tile.height as usize,
            values,
        }
    }

    #[inline]
    fn at(&self, i: usize, j: usize) -> f32 {
        self.values[j * self.width + i]
    }

    /// One binomial 1-2-1 pass along each axis. A nodata sample stays
    /// nodata; a sample beside nodata or the grid's edge averages the
    /// neighbours it has. The outer ring is therefore not the value a
    /// larger grid would give, and is not traced.
    fn smooth(&mut self) {
        let (w, h) = (self.width, self.height);
        let pass = |src: &[f32], dst: &mut [f32], stride: usize, len: usize, count: usize| {
            // `count` lines of `len` samples, `stride` apart along a line.
            let line_step = if stride == 1 { len } else { 1 };
            for line in 0..count {
                let start = line * line_step;
                for k in 0..len {
                    let at = start + k * stride;
                    let centre = src[at];
                    if centre.is_nan() {
                        dst[at] = f32::NAN;
                        continue;
                    }
                    let mut sum = 2.0 * f64::from(centre);
                    let mut weight = 2.0;
                    if k > 0 {
                        let v = src[at - stride];
                        if !v.is_nan() {
                            sum += f64::from(v);
                            weight += 1.0;
                        }
                    }
                    if k + 1 < len {
                        let v = src[at + stride];
                        if !v.is_nan() {
                            sum += f64::from(v);
                            weight += 1.0;
                        }
                    }
                    dst[at] = (sum / weight) as f32;
                }
            }
        };
        let mut rows = vec![0f32; self.values.len()];
        pass(&self.values, &mut rows, 1, w, h);
        pass(&rows, &mut self.values, w, h, w);
    }
}

/// The view's line coverage, one byte per pixel.
struct Canvas<'a> {
    coverage: &'a mut [u8],
    width: u32,
    height: u32,
    half_width: f64,
}

impl Canvas<'_> {
    /// Draw the segment `p`–`q` (view pixel coordinates: pixel `(x, y)`
    /// covers `[x, x + 1) × [y, y + 1)`), keeping each pixel's largest
    /// coverage. Coverage falls linearly from 1 at `half_width − 0.5` from
    /// the centre line to 0 at `half_width + 0.5`.
    fn segment(&mut self, p: (f64, f64), q: (f64, f64)) {
        let reach = self.half_width + 0.5;
        let x_lo = (p.0.min(q.0) - reach - 0.5).ceil().max(0.0);
        let x_hi = (p.0.max(q.0) + reach - 0.5)
            .floor()
            .min(f64::from(self.width) - 1.0);
        let y_lo = (p.1.min(q.1) - reach - 0.5).ceil().max(0.0);
        let y_hi = (p.1.max(q.1) + reach - 0.5)
            .floor()
            .min(f64::from(self.height) - 1.0);
        if x_lo > x_hi || y_lo > y_hi {
            return;
        }
        let (dx, dy) = (q.0 - p.0, q.1 - p.1);
        let length2 = dx * dx + dy * dy;
        for y in y_lo as u32..=y_hi as u32 {
            let cy = f64::from(y) + 0.5;
            let row = y as usize * self.width as usize;
            for x in x_lo as u32..=x_hi as u32 {
                let cx = f64::from(x) + 0.5;
                let t = if length2 > 0.0 {
                    (((cx - p.0) * dx + (cy - p.1) * dy) / length2).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let (ex, ey) = (p.0 + t * dx - cx, p.1 + t * dy - cy);
                let distance = (ex * ex + ey * ey).sqrt();
                let cover = (reach - distance).clamp(0.0, 1.0);
                if cover > 0.0 {
                    let byte = (cover * 255.0).round() as u8;
                    let pixel = &mut self.coverage[row + x as usize];
                    *pixel = (*pixel).max(byte);
                }
            }
        }
    }
}

/// Marching squares over `grid`, skipping its outer `skip` rings (the
/// samples smoothing could not finish), handing every segment to `segment`
/// in view pixel coordinates. Sample `(i, j)` lies at the centre of view
/// pixel `(i − grow, j − grow)`.
fn trace(
    grid: &Grid,
    skip: u32,
    grow: f64,
    style: &ContourStyle,
    segment: &mut dyn FnMut((f64, f64), (f64, f64)),
) -> Result<(), DataServerError> {
    let skip = skip as usize;
    if grid.width < 2 * skip + 2 || grid.height < 2 * skip + 2 {
        return Ok(());
    }
    let (interval, base) = (style.interval, style.base);
    // View pixel coordinates of a sample position.
    let at = |x: f64, y: f64| (x - grow + 0.5, y - grow + 0.5);
    for j in skip..grid.height - skip - 1 {
        ds_core::deadline::check()?;
        for i in skip..grid.width - skip - 1 {
            let (tl, tr) = (grid.at(i, j), grid.at(i + 1, j));
            let (bl, br) = (grid.at(i, j + 1), grid.at(i + 1, j + 1));
            if tl.is_nan() || tr.is_nan() || bl.is_nan() || br.is_nan() {
                continue; // lines end at nodata
            }
            let [tl, tr, br, bl] = [tl, tr, br, bl].map(f64::from);
            let lo = tl.min(tr).min(br).min(bl);
            let hi = tl.max(tr).max(br).max(bl);
            if lo == hi {
                continue;
            }
            let k_lo = ((lo - base) / interval).ceil();
            let k_hi = ((hi - base) / interval).floor();
            // Past 1e15 levels from the base the level arithmetic itself
            // loses whole intervals; nothing sensible is drawn there.
            if !(k_lo.abs() <= 1e15 && k_hi.abs() <= 1e15) || k_hi < k_lo {
                continue;
            }
            let (k_lo, k_hi) = (k_lo as i64, k_hi as i64);
            let k_hi = k_hi.min(k_lo + MAX_LEVELS_PER_CELL - 1);
            let (x, y) = (i as f64, j as f64);
            for k in k_lo..=k_hi {
                let level = base + k as f64 * interval;
                // Each edge from its lower-index sample, so the two cells
                // that share it, in any tile, find the same point.
                let top = || at(x + (level - tl) / (tr - tl), y);
                let bottom = || at(x + (level - bl) / (br - bl), y + 1.0);
                let left = || at(x, y + (level - tl) / (bl - tl));
                let right = || at(x + 1.0, y + (level - tr) / (br - tr));
                let case = u8::from(tl >= level)
                    | u8::from(tr >= level) << 1
                    | u8::from(br >= level) << 2
                    | u8::from(bl >= level) << 3;
                match case {
                    0 | 15 => {}
                    1 | 14 => segment(left(), top()),
                    2 | 13 => segment(top(), right()),
                    3 | 12 => segment(left(), right()),
                    4 | 11 => segment(right(), bottom()),
                    6 | 9 => segment(top(), bottom()),
                    7 | 8 => segment(left(), bottom()),
                    // Saddles: the cell's mean decides whether the corners
                    // at or above the level join through the centre.
                    5 | 10 => {
                        let centre_inside = (tl + tr + br + bl) / 4.0 >= level;
                        // Case 5 has tl and br inside, case 10 tr and bl.
                        if centre_inside == (case == 5) {
                            segment(top(), right());
                            segment(left(), bottom());
                        } else {
                            segment(left(), top());
                            segment(right(), bottom());
                        }
                    }
                    _ => unreachable!("four corner bits"),
                }
            }
        }
    }
    Ok(())
}

/// Draw the line colour over `rgba` by `coverage`.
fn paint_lines(rgba: &mut [u8], coverage: &[u8], color: [u8; 4]) {
    let (pixels, _) = rgba.as_chunks_mut::<4>();
    for (pixel, &cover) in pixels.iter_mut().zip(coverage) {
        if cover == 0 {
            continue;
        }
        let alpha = (u32::from(color[3]) * u32::from(cover) + 127) / 255;
        crate::encode::over_pixel(pixel, [color[0], color[1], color[2], alpha as u8]);
    }
}

/// A legend for a contour style: the title, the fill's colour bar when the
/// style has one, and a line swatch in the style's colour and width with
/// [`ContourStyle::description`] beside it (`every 4 hPa`).
#[allow(clippy::too_many_arguments)]
pub fn render_contour_legend(
    style: &StyleInfo,
    contour: &ContourStyle,
    unit: Option<&str>,
    width: u32,
    height: u32,
    format: ImageFormat,
    title: Option<&str>,
) -> Result<Vec<u8>, DataServerError> {
    const PAD: u32 = 4;
    const SWATCH: u32 = 24;
    const GAP: u32 = 4;
    const TEXT: [u8; 4] = [0, 0, 0, 255];
    let block = font::GLYPH_H + 2 * PAD;
    let mut rgba = if contour.fill && height > block + 2 * PAD {
        crate::legend_canvas(
            style.colormap.as_ref(),
            style.min,
            style.max,
            width,
            height,
            title,
            block,
        )
    } else {
        let mut canvas = vec![255u8; width as usize * height as usize * 4];
        if let Some(title) = title {
            for (i, line) in title.split('\n').filter(|l| !l.is_empty()).enumerate() {
                let y = PAD + i as u32 * (font::GLYPH_H + 2);
                if y + font::GLYPH_H + PAD <= height.saturating_sub(block) {
                    font::draw_text(
                        &mut canvas,
                        width,
                        height,
                        PAD as i32,
                        y as i32,
                        line,
                        TEXT,
                        1,
                    );
                }
            }
        }
        canvas
    };
    // The swatch, on the centre of the bottom block's middle pixel row.
    let row = height.saturating_sub(block / 2 + 1);
    let mut coverage = vec![0u8; width as usize * height as usize];
    Canvas {
        coverage: &mut coverage,
        width,
        height,
        half_width: contour.width / 2.0,
    }
    .segment(
        (f64::from(PAD), f64::from(row) + 0.5),
        (f64::from(PAD + SWATCH), f64::from(row) + 0.5),
    );
    paint_lines(&mut rgba, &coverage, contour.color);
    let text_y = row.saturating_sub(font::GLYPH_H / 2);
    font::draw_text(
        &mut rgba,
        width,
        height,
        (PAD + SWATCH + GAP) as i32,
        text_y as i32,
        &contour.description(unit),
        TEXT,
        1,
    );
    crate::encode_rgba(&mut rgba, width, height, format, None)
}

#[cfg(test)]
#[path = "contour_tests.rs"]
mod tests;
