//! The shared `f32` map sampler over a regular source grid (#1067).
//!
//! A new gridded engine decodes a field to a row-major `&[f32]` on its
//! [`GeoTransform`] and renders it with [`SourceGrid::render_f32`] rather
//! than writing a sampler of its own (the exceptions are below). The
//! rules, first written for engine-querydata's projected render path:
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
//!   outside [`OutputCrs::footprint_window_in_image`] of the grid's WGS84
//!   envelope stays empty (#449, #453), and the coordinate map is refined
//!   for that window alone ([`ProjectionGrid::build_2d_within`], #1110).
//!   A footprint wholly off the image builds no map at all (#1129). The
//!   envelope is the grid's inverse-projected cell edges
//!   ([`GeoTransform::bbox`]), computed once by [`SourceGrid::new`], never
//!   per request.
//!
//! The output is [`RasterValues::F32`]: interpolation runs in `f64` and only
//! the stored value is narrowed, with NaN for nodata. An engine serving
//! display units converts each sampled value in `f64` before that
//! ([`OutputMap::sample_f32_with`]), and a point query converts what
//! [`SourceGrid::value_at`] returns the same way (engine-netcdf, #1209).
//!
//! Three engines keep a sampler of their own. engine-querydata cannot
//! afford a decoded field, since its memory-mapped layout strides one field
//! by levels × times, so it shares the map alone: [`OutputMap::position`]
//! is where each output pixel samples, and the engine reads its own
//! storage there by the rules above (#1190). engine-grib and
//! engine-satellite keep their own samplers, and their own maps, by design:
//! moving either onto this one changes their pixels (#1172).

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

    /// Columns per turn of a grid whose columns fill the turn of longitude
    /// (a global lat/lon or Web Mercator grid), `None` for any other grid.
    /// Such a grid's columns repeat every turn, and the column one past its
    /// last is its first: what an engine sampling its own storage at
    /// [`OutputMap::position`] wraps.
    pub fn global_period(&self) -> Option<f64> {
        match self.layout {
            Layout::Periodic {
                period,
                global: true,
            } => Some(period),
            _ => None,
        }
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
    ///
    /// A map whose coarse grid stops at its cell cap over the interpolation
    /// budget is reported on stderr ([`ProjectionGrid::build_2d`]).
    pub fn output_map(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        output_crs: &OutputCrs,
    ) -> OutputMap<'_> {
        let map = self.output_map_quiet(bbox, width, height, output_crs);
        OutputMap {
            grid: map.grid.map(ProjectionGrid::warn_if_capped),
            ..map
        }
    }

    /// [`Self::output_map`] for a caller that reports a capped map itself:
    /// nothing goes to stderr, and [`OutputMap::capped_error`] says whether
    /// its coarse grid stopped at the cell cap over budget. ds-core has no
    /// `tracing`, so an engine logs it with what only it knows, the
    /// collection and the request (#1129).
    pub fn output_map_quiet(
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
                Some(ProjectionGrid::refined(
                    width,
                    height,
                    gt.width,
                    gt.height,
                    Some(period),
                    None,
                    node,
                    |lon, lat| centred(gt.world_to_pixel_f64(lon, lat)),
                )),
                None,
            ),
            Layout::Projected { envelope } => {
                let centre = envelope.map(|[w, _, e, _]| (w + e) / 2.0);
                let to_src = |lon, lat| {
                    let lon = centre.map_or(lon, |c| within_half_turn(lon, c));
                    centred(gt.world_to_pixel_f64(lon, lat))
                };
                // Only the footprint is sampled, so only its error counts
                // (#1110). A footprint off the image, or an empty image,
                // leaves nothing to sample and builds no grid (#1129).
                let window = envelope.map(|env| {
                    output_crs.footprint_window_in_image(
                        bbox,
                        envelope_near(env, bbox, output_crs),
                        width,
                        height,
                    )
                });
                let grid = match window {
                    Some(None) => None,
                    read => Some(ProjectionGrid::refined(
                        width,
                        height,
                        gt.width,
                        gt.height,
                        None,
                        read.flatten(),
                        node,
                        to_src,
                    )),
                };
                (grid, window.flatten())
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

    /// The fractional source position of WGS84 `(lon, lat)`, measured from
    /// the first cell's centre: the one forward projection a point query
    /// makes (an EDR position, or an area's cell centre), reused for every
    /// field it samples with [`Self::value_at`]. As in a map, a projected
    /// grid takes the longitude within half a turn of its own centre, and a
    /// longitude-linear grid's columns repeat every turn ([`Self::value_at`]
    /// wraps them), so a point given at 190°E or -170°E is the same point.
    pub fn position(&self, lon: f64, lat: f64) -> (f64, f64) {
        let lon = match self.layout {
            Layout::Projected {
                envelope: Some([w, _, e, _]),
            } => within_half_turn(lon, (w + e) / 2.0),
            _ => lon,
        };
        centred(self.transform.world_to_pixel_f64(lon, lat))
    }

    /// The value of `values`, the grid's row-major field (row 0 north) with
    /// `nodata` as its sentinel, at `position` ([`Self::position`]): what a
    /// map draws there. Bilinear between the four surrounding cell centres,
    /// or the nearest cell's value where one of them is nodata or off the
    /// grid; `None` off the grid (beyond the half cell past its edges), at
    /// nodata, or for a field of the wrong length.
    pub fn value_at(
        &self,
        values: &[f32],
        nodata: Option<f32>,
        (col, row): (f64, f64),
    ) -> Option<f64> {
        let cells = self.transform.width as usize * self.transform.height as usize;
        if values.len() != cells {
            return None;
        }
        self.sample(values, nodata, col, row)
    }

    /// Whether `position` ([`Self::position`]) lies on the grid, within the
    /// half cell past its edges: where [`Self::value_at`] finds a cell to
    /// read. A query whose positions all lie off the grid answers nothing
    /// but `None` and can skip reading its fields.
    pub fn covers(&self, (col, row): (f64, f64)) -> bool {
        self.locate(col, row).is_some()
    }

    /// The value at fractional source position `(col, row)`
    /// ([`centred`]): bilinear, or the nearest cell's where a neighbour is
    /// nodata or off the grid; `None` off the grid (beyond the half cell
    /// past its edges, [`Self::locate`]) or where the mapping is undefined.
    fn sample(&self, values: &[f32], nodata: Option<f32>, col: f64, row: f64) -> Option<f64> {
        let (col, global) = self.locate(col, row)?;
        let nx = i64::from(self.transform.width);
        let ny = i64::from(self.transform.height);
        let col0 = col.floor() as i64;
        let row0 = row.floor() as i64;
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

    /// Where `(col, row)` ([`centred`]) falls on the grid: the column moved
    /// into the turn the grid is drawn in (a periodic grid's) and whether
    /// the grid is global, or `None` off the grid (beyond the half cell past
    /// its edges) or where the mapping is undefined. [`Self::sample`] and
    /// [`Self::covers`] share it, so a query's skip and a map's draw agree.
    ///
    /// The non-finite check comes first: NaN fails every comparison and
    /// `NaN as i64` is 0, so it would otherwise pass the bounds checks and
    /// read the grid's first cell.
    #[inline]
    fn locate(&self, col: f64, row: f64) -> Option<(f64, bool)> {
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
        Some((col, global))
    }
}

/// One map request's output→source pixel map onto a [`SourceGrid`]
/// ([`SourceGrid::output_map`]).
pub struct OutputMap<'a> {
    source: &'a SourceGrid,
    /// `None` when a projected grid's footprint lies wholly off the image:
    /// nothing to draw, and no grid built.
    grid: Option<ProjectionGrid>,
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
        self.sample_f32_with(values, nodata, |v| v)
    }

    /// [`Self::sample_f32`] with `convert` applied to each sampled value in
    /// `f64`, before it is narrowed: a display conversion
    /// ([`crate::units::DisplayConversion::convert`]) after sampling, as
    /// engine-grib converts, so a pixel holds `convert` of what
    /// [`SourceGrid::value_at`] reads at its position. Nodata stays NaN.
    pub fn sample_f32_with(
        &self,
        values: &[f32],
        nodata: Option<f32>,
        convert: impl Fn(f64) -> f64,
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
        let mut data = Vec::with_capacity(self.width as usize * self.height as usize);
        for oy in 0..self.height {
            crate::deadline::check()?;
            for ox in 0..self.width {
                let value = self
                    .mapped(ox, oy)
                    .and_then(|(col, row)| self.source.sample(values, nodata, col, row));
                data.push(value.map_or(f32::NAN, |v| convert(v) as f32));
            }
        }
        Ok(RasterTile {
            width: self.width,
            height: self.height,
            values: RasterValues::F32 { data, nodata: None },
        })
    }

    /// The fractional source position output pixel `(ox, oy)` samples, as
    /// [`SourceGrid::position`] measures it, with a longitude-linear grid's
    /// column moved into the turn the grid is drawn in; `None` where the
    /// map draws nothing: outside a projected grid's footprint, off the
    /// grid (beyond the half cell past its edges), or where the mapping is
    /// undefined.
    ///
    /// For an engine that samples its own storage rather than a decoded
    /// field (engine-querydata, #1190), by [`Self::sample_f32`]'s rules:
    /// bilinear between the four surrounding cell centres, else the nearest
    /// cell, and on a global grid ([`SourceGrid::global_period`]) the
    /// column one past the last is the first. The engine checks the
    /// request deadline per row, as `sample_f32` does.
    pub fn position(&self, ox: u32, oy: u32) -> Option<(f64, f64)> {
        let (col, row) = self.mapped(ox, oy)?;
        let (col, _) = self.source.locate(col, row)?;
        Some((col, row))
    }

    /// The estimated interpolation error of the map's coarse grid when its
    /// refinement stopped at the cell cap over budget, `None` when it met
    /// the budget or no grid was built. In source pixels, or output pixels
    /// on a longitude-linear grid ([`ProjectionGrid::capped_error`]).
    pub fn capped_error(&self) -> Option<f64> {
        self.grid.as_ref()?.capped_error()
    }

    /// Output pixel `(ox, oy)`'s source position as the coarse grid
    /// interpolates it, before [`SourceGrid::locate`]; `None` outside the
    /// footprint window or with no grid.
    fn mapped(&self, ox: u32, oy: u32) -> Option<(f64, f64)> {
        let grid = self.grid.as_ref()?;
        match self.window {
            Some((x0, x1, y0, y1)) if !((x0..=x1).contains(&ox) && (y0..=y1).contains(&oy)) => None,
            _ => Some(grid.sample(ox, oy)),
        }
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
/// inside its cells reaches that pole, at every longitude
/// ([`GeoTransform::bbox_with_poles`]).
fn wgs84_envelope(transform: &GeoTransform) -> Option<[f64; 4]> {
    let [w, s, mut e, n] = transform.bbox_with_poles();
    if !(w.is_finite() && s.is_finite() && e.is_finite() && n.is_finite()) || s > n {
        return None;
    }
    if w > e {
        e += 360.0;
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
