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
//!   envelope is the grid's extent ([`GeoTransform::footprint_envelope`]),
//!   computed once by [`SourceGrid::new`], never per request.
//! - **Curvilinear grids** (a latitude and longitude per cell, no CRS,
//!   #1174) are placed by [`CurvilinearGrid::locate`], the inverse lookup
//!   over the grid's nodes, at the coarse grid's nodes only, and a point
//!   query's position is the same lookup: the rules above then sample the
//!   field (bilinear, nearest at a nodata neighbour, the half cell past the
//!   outer cell edges). The footprint guard walks the grid's extent
//!   ([`CurvilinearGrid::footprint_envelope`]), as for a projected grid.
//! - **Windows** (#1171): [`SourceGrid::cells_read`] is the window of cells
//!   [`SourceGrid::value_at`] reads at a point query's position, and
//!   [`SourceGrid::value_in`] samples a field read over such a window
//!   alone, bit for bit as `value_at` samples the whole field: an engine
//!   whose source can serve part of a field (engine-netcdf through
//!   OPeNDAP) reads only what its positions need.
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

use std::sync::Arc;

use super::{CurvilinearGrid, ProjectionGrid};
use crate::error::DataServerError;
use crate::geo::{Crs, GeoTransform};
use crate::map_engine::{OutputCrs, RasterTile, RasterValues};

/// A source grid: a regular one's [`GeoTransform`], or a curvilinear
/// one's [`CurvilinearGrid`], with what sampling derives from it, built
/// once per grid and kept by the engine (it is cheap to clone and
/// immutable). A regular grid's rows run north to south, as the
/// transform's do: a source stored south-up is flipped when decoded. A
/// curvilinear grid's rows are as stored.
#[derive(Debug, Clone)]
pub struct SourceGrid {
    /// Columns and rows.
    width: u32,
    height: u32,
    layout: Layout,
}

#[derive(Debug, Clone)]
enum Layout {
    /// Columns linear in longitude, `period` columns per turn. `global`
    /// when the columns fill the turn (within half a column).
    Periodic {
        transform: GeoTransform,
        period: f64,
        global: bool,
    },
    /// Any other CRS. `envelope` is the grid's WGS84 `[w, s, e, n]`, with
    /// `e > 180°` when it crosses the antimeridian
    /// ([`GeoTransform::footprint_envelope`]), and `centre` the longitude
    /// its pixels are projected within half a turn of
    /// ([`GeoTransform::inverse_frame_centre`]); `None` when no edge point
    /// inverse-projects, which leaves longitudes as they come and turns the
    /// footprint guard off.
    Projected {
        transform: GeoTransform,
        envelope: Option<[f64; 4]>,
        centre: Option<f64>,
    },
    /// A latitude and longitude per cell and no CRS (#1174): every point
    /// placed by the grid's inverse lookup. `envelope` as a projected
    /// grid's ([`CurvilinearGrid::footprint_envelope`]).
    Curvilinear {
        grid: Arc<CurvilinearGrid>,
        envelope: Option<[f64; 4]>,
    },
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
        let (width, height) = (transform.width, transform.height);
        let layout = match turn {
            Some(turn) if transform.pixel_width.is_finite() && transform.pixel_width > 0.0 => {
                let period = turn / transform.pixel_width;
                Layout::Periodic {
                    global: f64::from(transform.width) >= period - 0.5,
                    transform,
                    period,
                }
            }
            _ => Layout::Projected {
                envelope: transform.footprint_envelope(),
                centre: transform.inverse_frame_centre(),
                transform,
            },
        };
        SourceGrid {
            width,
            height,
            layout,
        }
    }

    /// The sampler's view of a curvilinear grid (#1174), which
    /// [`CurvilinearGrid::new`] has indexed once.
    pub fn from_curvilinear(grid: Arc<CurvilinearGrid>) -> Self {
        let (nx, ny) = grid.size();
        SourceGrid {
            width: nx as u32,
            height: ny as u32,
            layout: Layout::Curvilinear {
                envelope: grid.footprint_envelope(),
                grid,
            },
        }
    }

    /// A regular grid's transform; `None` for a curvilinear grid, which
    /// has none.
    pub fn transform(&self) -> Option<&GeoTransform> {
        match &self.layout {
            Layout::Periodic { transform, .. } | Layout::Projected { transform, .. } => {
                Some(transform)
            }
            Layout::Curvilinear { .. } => None,
        }
    }

    /// A curvilinear grid's lookup; `None` for a regular grid.
    pub fn curvilinear(&self) -> Option<&CurvilinearGrid> {
        match &self.layout {
            Layout::Curvilinear { grid, .. } => Some(grid),
            _ => None,
        }
    }

    /// `(columns, rows)`: the length of a field is their product.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
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
                ..
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
        let node = |fx: f64, fy: f64| output_crs.project_node(bbox, fx, fy);
        let (grid, window) = match &self.layout {
            Layout::Periodic {
                transform, period, ..
            } => (
                Some(ProjectionGrid::refined(
                    width,
                    height,
                    self.width,
                    self.height,
                    Some(*period),
                    None,
                    node,
                    |lon, lat| centred(transform.world_to_pixel_f64(lon, lat)),
                )),
                None,
            ),
            Layout::Projected {
                transform,
                envelope,
                centre,
            } => self.footprint_map(
                *envelope,
                (bbox, width, height, output_crs),
                node,
                |lon, lat| {
                    let lon = centre.map_or(lon, |c| within_half_turn(lon, c));
                    centred(transform.world_to_pixel_f64(lon, lat))
                },
            ),
            Layout::Curvilinear { grid, envelope } => self.footprint_map(
                *envelope,
                (bbox, width, height, output_crs),
                node,
                |lon, lat| grid.locate(lon, lat),
            ),
        };
        OutputMap {
            source: self,
            grid,
            width,
            height,
            window,
        }
    }

    /// The coarse map of a grid drawn only within its footprint (a
    /// projected or curvilinear grid): only the footprint is sampled, so
    /// only its error counts (#1110). A footprint off the image, or an
    /// empty image, leaves nothing to sample and builds no grid (#1129).
    /// `envelope` `None` turns the guard off.
    fn footprint_map(
        &self,
        envelope: Option<[f64; 4]>,
        (bbox, width, height, output_crs): ([f64; 4], u32, u32, &OutputCrs),
        node: impl Fn(f64, f64) -> (f64, f64),
        to_src: impl Fn(f64, f64) -> (f64, f64),
    ) -> MapGrid {
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
                self.width,
                self.height,
                None,
                read.flatten(),
                node,
                to_src,
            )),
        };
        (grid, window.flatten())
    }

    /// The fractional source position of WGS84 `(lon, lat)`, measured from
    /// the first cell's centre: the one forward projection a point query
    /// makes (an EDR position, or an area's cell centre), reused for every
    /// field it samples with [`Self::value_at`]. As in a map, a projected
    /// grid takes the longitude within half a turn of its own centre, and a
    /// longitude-linear grid's columns repeat every turn ([`Self::value_at`]
    /// wraps them), so a point given at 190°E or -170°E is the same point.
    /// A curvilinear grid's is its inverse lookup
    /// ([`CurvilinearGrid::locate`]), the one a map makes at its nodes.
    pub fn position(&self, lon: f64, lat: f64) -> (f64, f64) {
        match &self.layout {
            Layout::Periodic { transform, .. } => centred(transform.world_to_pixel_f64(lon, lat)),
            Layout::Projected {
                transform, centre, ..
            } => {
                let lon = centre.map_or(lon, |c| within_half_turn(lon, c));
                centred(transform.world_to_pixel_f64(lon, lat))
            }
            Layout::Curvilinear { grid, .. } => grid.locate(lon, lat),
        }
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
        let cells = self.width as usize * self.height as usize;
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

    /// The cells [`Self::value_at`] may read at `position`
    /// ([`Self::position`]): the four around it that lie on the grid, the
    /// nearest of them included, as an inclusive window; `None` off the
    /// grid ([`Self::covers`]). On a global grid the column one past the
    /// last is the first, so a position between them reads a window of
    /// every column. Widened over several positions by
    /// [`CellWindow::union`], it is the part of a field a point query
    /// needs, which [`Self::value_in`] samples as `value_at` samples the
    /// whole field.
    pub fn cells_read(&self, (col, row): (f64, f64)) -> Option<CellWindow> {
        let (col, global) = self.locate(col, row)?;
        let ([c0, c1], [r0, r1]) = self.corners(col, row, global);
        let bounds = |a: Option<i64>, b: Option<i64>| {
            let (lo, hi) = match (a, b) {
                (Some(a), Some(b)) => (a.min(b), a.max(b)),
                (Some(a), None) | (None, Some(a)) => (a, a),
                (None, None) => return None,
            };
            Some((lo as u32, hi as u32))
        };
        Some(CellWindow {
            columns: bounds(c0, c1)?,
            rows: bounds(r0, r1)?,
        })
    }

    /// [`Self::value_at`] of a field read only over `window`: `values` are
    /// the window's cells, row-major (`window.width() × window.height()`,
    /// its first row the northernmost), with `nodata` as their sentinel.
    /// Wherever `window` holds [`Self::cells_read`] of `position`, the
    /// value is `value_at`'s on the whole field, bit for bit: the same
    /// cells in the same arithmetic. `None` where `value_at` is, for values
    /// of the wrong length, and for a position whose cells lie outside
    /// `window`.
    pub fn value_in(
        &self,
        window: CellWindow,
        values: &[f32],
        nodata: Option<f32>,
        position: (f64, f64),
    ) -> Option<f64> {
        if values.len() as u64 != window.cells() || !window.holds(self.cells_read(position)?) {
            return None;
        }
        let (c0, r0) = (window.columns.0 as usize, window.rows.0 as usize);
        let width = window.width() as usize;
        self.sample_cells(position.0, position.1, |c, r| {
            RasterValues::decode_f32(values[(r - r0) * width + (c - c0)], nodata)
        })
    }

    /// The value at fractional source position `(col, row)`
    /// ([`centred`]): bilinear, or the nearest cell's where a neighbour is
    /// nodata or off the grid; `None` off the grid (beyond the half cell
    /// past its edges, [`Self::locate`]) or where the mapping is undefined.
    fn sample(&self, values: &[f32], nodata: Option<f32>, col: f64, row: f64) -> Option<f64> {
        let nx = self.width as usize;
        self.sample_cells(col, row, |c, r| {
            RasterValues::decode_f32(values[r * nx + c], nodata)
        })
    }

    /// Column `c` of the grid, `None` off it; a global grid's column one
    /// past its last is its first. Every cell [`Self::sample_cells`] reads
    /// and [`Self::cells_read`] bounds goes through it.
    #[inline]
    fn column(&self, c: i64, global: bool) -> Option<i64> {
        let nx = i64::from(self.width);
        if global && c >= nx {
            Some(0)
        } else {
            (0..nx).contains(&c).then_some(c)
        }
    }

    /// The columns and rows of the four cells around `(col, row)`, the
    /// column already moved by [`Self::locate`] (`global` its grid's), each
    /// `None` off the grid ([`Self::column`]): the cells
    /// [`Self::sample_cells`] reads, its nearest cell among them, which
    /// [`Self::cells_read`] bounds.
    #[inline]
    fn corners(&self, col: f64, row: f64, global: bool) -> ([Option<i64>; 2], [Option<i64>; 2]) {
        let ny = i64::from(self.height);
        let (col0, row0) = (col.floor() as i64, row.floor() as i64);
        let row = |r: i64| (0..ny).contains(&r).then_some(r);
        (
            [self.column(col0, global), self.column(col0 + 1, global)],
            [row(row0), row(row0 + 1)],
        )
    }

    /// [`Self::sample`] over cells read by `cell(column, row)`, each on the
    /// grid and among [`Self::corners`] (the same `floor` and
    /// [`Self::column`]): the whole field's for a map or [`Self::value_at`],
    /// a window's for [`Self::value_in`].
    #[inline]
    fn sample_cells(
        &self,
        col: f64,
        row: f64,
        cell: impl Fn(usize, usize) -> Option<f64>,
    ) -> Option<f64> {
        let (col, global) = self.locate(col, row)?;
        let nx = i64::from(self.width);
        let ny = i64::from(self.height);
        let col0 = col.floor() as i64;
        let row0 = row.floor() as i64;
        let at = |c: Option<i64>, r: i64| {
            let c = c.filter(|_| (0..ny).contains(&r))?;
            cell(c as usize, r as usize)
        };
        let (dx, dy) = (col - col0 as f64, row - row0 as f64);
        let (c0, c1) = (self.column(col0, global), self.column(col0 + 1, global));
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
                    self.column((col + 0.5).floor() as i64, true)
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
        let nx = i64::from(self.width);
        let ny = i64::from(self.height);
        let (col, global) = match self.layout {
            // A global grid's columns run [0, period); one past its last
            // column reads its first.
            Layout::Periodic {
                period,
                global: true,
                ..
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
            Layout::Projected { .. } | Layout::Curvilinear { .. } => (col, false),
        };
        let col0 = col.floor() as i64;
        let row0 = row.floor() as i64;
        if row0 < -1 || row0 >= ny || (!global && (col0 < -1 || col0 >= nx)) {
            return None;
        }
        Some((col, global))
    }
}

/// An inclusive window of a [`SourceGrid`]'s cells, row 0 north as
/// [`SourceGrid::value_at`] counts rows: the cells a point query reads
/// ([`SourceGrid::cells_read`] of each position, joined by
/// [`Self::union`]), so an engine that can read part of a field reads only
/// these and samples them with [`SourceGrid::value_in`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellWindow {
    /// The first and last column.
    pub columns: (u32, u32),
    /// The first and last row.
    pub rows: (u32, u32),
}

impl CellWindow {
    /// The smallest window holding both.
    pub fn union(self, other: Self) -> Self {
        CellWindow {
            columns: (
                self.columns.0.min(other.columns.0),
                self.columns.1.max(other.columns.1),
            ),
            rows: (self.rows.0.min(other.rows.0), self.rows.1.max(other.rows.1)),
        }
    }

    /// Columns in the window.
    pub fn width(&self) -> u32 {
        self.columns.1 - self.columns.0 + 1
    }

    /// Rows in the window.
    pub fn height(&self) -> u32 {
        self.rows.1 - self.rows.0 + 1
    }

    /// Cells in the window: the length of its values.
    pub fn cells(&self) -> u64 {
        u64::from(self.width()) * u64::from(self.height())
    }

    /// Whether `other` lies within this window.
    pub fn holds(&self, other: CellWindow) -> bool {
        self.columns.0 <= other.columns.0
            && other.columns.1 <= self.columns.1
            && self.rows.0 <= other.rows.0
            && other.rows.1 <= self.rows.1
    }
}

/// A map's coarse grid, `None` when nothing is drawn, and the inclusive
/// output window `(px_lo, px_hi, py_lo, py_hi)` its footprint occupies,
/// `None` for every pixel.
type MapGrid = (Option<ProjectionGrid>, Option<(u32, u32, u32, u32)>);

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
        let (width, height) = self.source.size();
        let cells = width as usize * height as usize;
        if values.len() != cells {
            return Err(DataServerError::Engine(format!(
                "field has {} values for a {width}x{height} grid",
                values.len(),
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
