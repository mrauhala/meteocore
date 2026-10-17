//! The shared `f32` map sampler over a regular source grid (#1067).
//!
//! A gridded engine decodes a field to a row-major `&[f32]` on its
//! [`GeoTransform`] and renders it with [`SourceGrid::render_f32`]; it does
//! not write a sampler of its own. The rules, taken from engine-querydata's
//! projected render path:
//!
//! - **Output → source** goes through [`OutputCrs::project_node`] and a
//!   coarse [`ProjectionGrid`], never a projection per output pixel
//!   (Critical Rule 5), whatever the output and source CRS.
//! - **Half-pixel convention:** source position `(col, row)` is measured
//!   from the first cell's *centre*, so a whole number names a cell centre.
//!   A point up to half a cell past the outer cell edges still samples the
//!   edge cell.
//! - **Bilinear** between the four surrounding cell centres; where one of
//!   them is nodata (the sentinel, NaN or ±∞) or off the grid, the
//!   **nearest** cell's value instead, itself possibly nodata.
//! - **Longitude-linear sources** (`Crs::Wgs84`, `Crs::WebMercator`) repeat
//!   every turn: the coordinate map is built with the grid's column period
//!   ([`ProjectionGrid::build_2d_periodic`]), so a view across the source's
//!   seam, the antimeridian or a projected output's longitude cut, or around
//!   a pole, stays registered (#323). A global grid wraps its columns, so a
//!   seam box such as 170,10,-170,20 is continuous; a regional one is drawn
//!   in every turn a view reaches, as a box Maps unwraps past ±180° (#828)
//!   asks for.
//! - **Any other source** (LCC, stereographic, TM, …) is projected from a
//!   longitude within half a turn of the grid's own centre, so a box past
//!   ±180° reaches a grid near the antimeridian. Its forward transform can
//!   alias a far-away output pixel onto valid data at low zoom, so output
//!   outside [`OutputCrs::footprint_pixel_window`] of the grid's WGS84
//!   envelope stays empty (#449, #453). The envelope is the grid's
//!   inverse-projected cell edges ([`GeoTransform::bbox`]), computed once
//!   by [`SourceGrid::new`], never per request.
//!
//! The output is [`RasterValues::F32`]: interpolation runs in `f64` and only
//! the stored value is narrowed, with NaN for nodata.

use super::ProjectionGrid;
use crate::error::DataServerError;
use crate::geo::{Crs, GeoTransform};
use crate::map_engine::{OutputCrs, RasterTile, RasterValues};

/// A regular source grid: its [`GeoTransform`] with what sampling derives
/// from it, built once per grid and kept by the engine (it is cheap to
/// clone and immutable). Rows run north to south, as the transform's do: a
/// source stored south-up is flipped when decoded.
#[derive(Debug, Clone)]
pub struct SourceGrid {
    transform: GeoTransform,
    layout: Layout,
}

#[derive(Debug, Clone)]
enum Layout {
    /// Columns linear in longitude, `period` columns per turn. `global`
    /// when the columns fill the turn (within half a column).
    Periodic { period: f64, global: bool },
    /// Any other CRS. `envelope` is the grid's WGS84 `[w, s, e, n]`, with
    /// `e > 180°` when it crosses the antimeridian; `None` when no edge
    /// point inverse-projects, which leaves longitudes as they come and
    /// turns the footprint guard off.
    Projected { envelope: Option<[f64; 4]> },
}

impl SourceGrid {
    /// The sampler's view of `transform`. A projected grid's WGS84 envelope
    /// is inverse-projected here, once.
    pub fn new(transform: GeoTransform) -> Self {
        let turn = match transform.crs {
            Crs::Wgs84 => Some(360.0),
            Crs::WebMercator => {
                Some(crate::web_mercator::lon_to_x(180.0) - crate::web_mercator::lon_to_x(-180.0))
            }
            _ => None,
        };
        let layout = match turn {
            Some(turn) if transform.pixel_width.is_finite() && transform.pixel_width > 0.0 => {
                let period = turn / transform.pixel_width;
                Layout::Periodic {
                    period,
                    global: f64::from(transform.width) >= period - 0.5,
                }
            }
            _ => Layout::Projected {
                envelope: wgs84_envelope(&transform),
            },
        };
        SourceGrid { transform, layout }
    }

    /// The grid's transform.
    pub fn transform(&self) -> &GeoTransform {
        &self.transform
    }

    /// Render `values`, the grid's row-major field with `nodata` as its
    /// sentinel, onto a `width`×`height` map of `bbox` in `output_crs`:
    /// [`Self::output_map`] then [`OutputMap::sample_f32`].
    pub fn render_f32(
        &self,
        values: &[f32],
        nodata: Option<f32>,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        output_crs: &OutputCrs,
    ) -> Result<RasterTile, DataServerError> {
        self.output_map(bbox, width, height, output_crs)
            .sample_f32(values, nodata)
    }

    /// The output→source pixel map of one map request: build it once and
    /// sample every field of the grid from it (several parameters, or the
    /// bands of a composite).
    ///
    /// `bbox` is the request's WGS84 `[west, south, east, north]`, as
    /// [`crate::map_engine::MapEngine::get_raster_tile`] receives it. A box
    /// crossing the antimeridian may come either way: `west > east`
    /// (170, 10, -170, 20), or unwrapped with `east` past 180°
    /// (170, 10, 190, 20), as OGC API Maps passes it (#828).
    pub fn output_map(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        output_crs: &OutputCrs,
    ) -> OutputMap<'_> {
        let [west, south, mut east, north] = bbox;
        if west > east {
            east += 360.0;
        }
        let bbox = [west, south, east, north];
        let gt = &self.transform;
        let node = |fx: f64, fy: f64| output_crs.project_node(bbox, fx, fy);
        let (grid, window) = match self.layout {
            Layout::Periodic { period, .. } => (
                ProjectionGrid::build_2d_periodic(
                    width,
                    height,
                    gt.width,
                    gt.height,
                    Some(period),
                    node,
                    |lon, lat| centred(gt.world_to_pixel_f64(lon, lat)),
                ),
                None,
            ),
            Layout::Projected { envelope } => {
                let centre = envelope.map(|[w, _, e, _]| (w + e) / 2.0);
                let grid = ProjectionGrid::build_2d(
                    width,
                    height,
                    gt.width,
                    gt.height,
                    node,
                    |lon, lat| {
                        let lon = centre.map_or(lon, |c| within_half_turn(lon, c));
                        centred(gt.world_to_pixel_f64(lon, lat))
                    },
                );
                let window = envelope.filter(|_| width > 0 && height > 0).map(|env| {
                    output_crs.footprint_pixel_window(
                        bbox,
                        envelope_near(env, bbox, output_crs),
                        width,
                        height,
                    )
                });
                (grid, window)
            }
        };
        OutputMap {
            source: self,
            grid,
            width,
            height,
            window,
        }
    }

    /// The value at fractional source position `(col, row)`
    /// ([`centred`]): bilinear, or the nearest cell's where a neighbour is
    /// nodata or off the grid; `None` off the grid (beyond the half cell
    /// past its edges) or where the mapping is undefined.
    ///
    /// The non-finite check comes first: NaN fails every comparison and
    /// `NaN as i64` is 0, so it would otherwise pass the bounds checks and
    /// read the grid's first cell.
    fn sample(&self, values: &[f32], nodata: Option<f32>, col: f64, row: f64) -> Option<f64> {
        if !col.is_finite() || !row.is_finite() {
            return None;
        }
        let nx = i64::from(self.transform.width);
        let ny = i64::from(self.transform.height);
        let (col, global) = match self.layout {
            // A global grid's columns run [0, period); one past its last
            // column reads its first.
            Layout::Periodic {
                period,
                global: true,
            } => (col.rem_euclid(period), true),
            // A regional one is drawn in the turn centred on it.
            Layout::Periodic { period, .. } => {
                let start = (nx - 1) as f64 / 2.0 - period / 2.0;
                let col = if (start..start + period).contains(&col) {
                    col
                } else {
                    start + (col - start).rem_euclid(period)
                };
                (col, false)
            }
            Layout::Projected { .. } => (col, false),
        };
        let col0 = col.floor() as i64;
        let row0 = row.floor() as i64;
        if row0 < -1 || row0 >= ny || (!global && (col0 < -1 || col0 >= nx)) {
            return None;
        }
        let column = |c: i64| {
            if global && c >= nx {
                Some(0)
            } else {
                (0..nx).contains(&c).then_some(c)
            }
        };
        let at = |c: Option<i64>, r: i64| {
            let c = c.filter(|_| (0..ny).contains(&r))?;
            RasterValues::decode_f32(values[(r * nx + c) as usize], nodata)
        };
        let (dx, dy) = (col - col0 as f64, row - row0 as f64);
        let (c0, c1) = (column(col0), column(col0 + 1));
        match (
            at(c0, row0),
            at(c1, row0),
            at(c0, row0 + 1),
            at(c1, row0 + 1),
        ) {
            (Some(tl), Some(tr), Some(bl), Some(br)) => {
                let top = tl + (tr - tl) * dx;
                let bot = bl + (br - bl) * dx;
                Some(top + (bot - top) * dy)
            }
            _ => {
                let nearest = |f: f64, n: i64| ((f + 0.5).floor() as i64).clamp(0, n - 1);
                let nc = if global {
                    column((col + 0.5).floor() as i64)
                } else {
                    Some(nearest(col, nx))
                };
                at(nc, nearest(row, ny))
            }
        }
    }
}

/// One map request's output→source pixel map onto a [`SourceGrid`]
/// ([`SourceGrid::output_map`]).
pub struct OutputMap<'a> {
    source: &'a SourceGrid,
    grid: ProjectionGrid,
    width: u32,
    height: u32,
    /// The inclusive output window `(px_lo, px_hi, py_lo, py_hi)` a
    /// projected grid's footprint can occupy; `None` samples every pixel.
    window: Option<(u32, u32, u32, u32)>,
}

impl OutputMap<'_> {
    /// Sample `values`, the source grid's row-major field (`width × height`
    /// of its transform, row 0 north) with `nodata` as its sentinel, onto
    /// the map: a [`RasterValues::F32`] tile, NaN where there is no data.
    ///
    /// A field of the wrong length is an [`DataServerError::Engine`] error,
    /// and the request deadline ([`crate::deadline`]) is checked per row.
    pub fn sample_f32(
        &self,
        values: &[f32],
        nodata: Option<f32>,
    ) -> Result<RasterTile, DataServerError> {
        let gt = &self.source.transform;
        let cells = gt.width as usize * gt.height as usize;
        if values.len() != cells {
            return Err(DataServerError::Engine(format!(
                "field has {} values for a {}x{} grid",
                values.len(),
                gt.width,
                gt.height
            )));
        }
        let (px, py) = match self.window {
            Some((x0, x1, y0, y1)) => (x0..=x1, y0..=y1),
            None => (0..=u32::MAX, 0..=u32::MAX),
        };
        let mut data = Vec::with_capacity(self.width as usize * self.height as usize);
        for oy in 0..self.height {
            crate::deadline::check()?;
            for ox in 0..self.width {
                let value = if py.contains(&oy) && px.contains(&ox) {
                    let (col, row) = self.grid.sample(ox, oy);
                    self.source.sample(values, nodata, col, row)
                } else {
                    None
                };
                data.push(value.map_or(f32::NAN, |v| v as f32));
            }
        }
        Ok(RasterTile {
            width: self.width,
            height: self.height,
            values: RasterValues::F32 { data, nodata: None },
        })
    }
}

/// [`GeoTransform::world_to_pixel_f64`]'s position, measured from the
/// first cell's centre rather than its corner.
fn centred((col, row): (f64, f64)) -> (f64, f64) {
    (col - 0.5, row - 0.5)
}

/// `lon` moved by whole turns into `[centre - 180°, centre + 180°)`. A
/// longitude already there is returned as is, so it never moves by a
/// rounding error; NaN stays NaN.
fn within_half_turn(lon: f64, centre: f64) -> f64 {
    let start = centre - 180.0;
    if (start..start + 360.0).contains(&lon) {
        lon
    } else {
        start + (lon - start).rem_euclid(360.0)
    }
}

/// The grid's WGS84 envelope, `[w, s, e, n]` with `w <= e` (`e` past 180°
/// when it crosses the antimeridian), or `None` when no edge point
/// inverse-projects.
///
/// The edges miss a pole the grid holds (a polar stereographic grid's
/// edges stay south of it), so a grid whose forward transform puts a pole
/// inside its cells reaches that pole, at every longitude.
fn wgs84_envelope(transform: &GeoTransform) -> Option<[f64; 4]> {
    let [mut w, mut s, mut e, mut n] = transform.bbox();
    if !(w.is_finite() && s.is_finite() && e.is_finite() && n.is_finite()) || s > n {
        return None;
    }
    if w > e {
        e += 360.0;
    }
    let (cols, rows) = (f64::from(transform.width), f64::from(transform.height));
    for pole in [90.0, -90.0] {
        let (col, row) = transform.world_to_pixel_f64(0.0, pole);
        if (0.0..cols).contains(&col) && (0.0..rows).contains(&row) {
            (w, e) = (-180.0, 180.0);
            s = s.min(pole);
            n = n.max(pole);
        }
    }
    Some([w, s, e, n])
}

/// The envelope [`OutputCrs::footprint_pixel_window`] needs for `bbox`: on
/// geographic output, moved by whole turns to the box's longitudes, which
/// may lie past ±180°; spanning the whole turn, it bounds latitude only.
/// A projected output places the envelope by its own forward transform.
fn envelope_near(envelope: [f64; 4], bbox: [f64; 4], output_crs: &OutputCrs) -> [f64; 4] {
    if matches!(output_crs, OutputCrs::Projected { .. }) {
        return envelope;
    }
    let [w, s, e, n] = envelope;
    let [west, _, east, _] = bbox;
    if e - w >= 360.0 {
        return [west, s, east, n];
    }
    let turns = ((west + east - w - e) / 720.0).round() * 360.0;
    [w + turns, s, e + turns, n]
}

#[cfg(test)]
mod tests;
