//! The grid an EDR area or radius query answers (#1435).
//!
//! A geographic source's pixel centres are a regular CRS84 grid already, so
//! the answer is its native pixel window, or the two windows either side of
//! a global raster's seam when the bbox holds it (#1442). A projected
//! source's are not: a
//! cell's longitude depends on its row as well as its column, so no pair of
//! `x` and `y` axes places its native cells, and axes taken along the
//! window's middle row and column put the corners of a few degrees' window
//! kilometres from their data. A projected source answers a regular CRS84
//! grid instead, the cell centres of [`QueryPolygon::grid_axes`], each cell
//! the value of the native pixel holding its centre: nearest, never
//! interpolated, so radar classes stay the values the file holds. The cells
//! reach their pixels through a coarse [`ProjectionGrid`], never one CRS
//! transform per cell (root Critical Rule 5).
//!
//! Both kinds place the bbox's longitudes in the source's own frame first
//! ([`lon_arc`] round [`frame_centre`]): a geographic raster's columns, a
//! turn wide at most, and a projection's longitudes near its central
//! meridian. An antimeridian-crossing bbox is then one arc, never the sweep
//! through 0° that `GeoTransform::bbox_to_pixels` takes from `west` to
//! `east`. The position query places its point by the same rule
//! ([`source_longitude`]).
//!
//! [`AreaGrid::new`] settles the answer's extent and cell count without
//! allocating either; [`AreaGrid::plan`] runs the budgets on them before it
//! builds the axes, the projection grid and the native windows to read.

use ds_core::error::DataServerError;
use ds_core::feature::{check_area_budget, check_mask_budget, Bbox, QueryPolygon, MAX_AREA_VALUES};
use ds_core::geo::{
    great_circle_distance_m, lon_within_half_turn, wrap_lon, Crs, GeoTransform, EARTH_RADIUS_M,
};
use ds_core::resample::ProjectionGrid;

/// A native pixel window `(col_start, row_start, col_end, row_end)`, ends
/// exclusive.
pub(crate) type Window = (u32, u32, u32, u32);

/// The answer to one area query over one source grid, before its budgets.
pub(crate) struct AreaGrid {
    /// The source grid the answer is read from: the first matching entry's.
    geo_transform: GeoTransform,
    kind: Kind,
}

enum Kind {
    /// A geographic source: its native pixel windows ([`native_windows`]),
    /// on the same rows, side by side west to east, are the answer.
    /// `antimeridian` is the CRS84 longitude a pixel centred on ±180° takes
    /// ([`crs84_lon`]).
    Native {
        windows: Vec<Window>,
        antimeridian: f64,
    },
    /// A projected source: a regular CRS84 grid.
    Regular(RegularGrid),
}

/// A projected source's answer grid: where it lies and how many cells it
/// has, the cells themselves built only once the budgets pass.
struct RegularGrid {
    /// The outer edges `[west, south, east, north]` of the cells, the
    /// longitudes near the source's central meridian (`west <= east`, either
    /// may lie past ±180°): the longitudes its forward transform is given.
    frame: [f64; 4],
    /// The same box with CRS84 longitudes, `west > east` across the
    /// antimeridian, for [`QueryPolygon::grid_axes`].
    crs84: Bbox,
    /// `(nx, ny)`: [`QueryPolygon::native_dims`] of `crs84` at the cell
    /// size [`RegularGrid::new`] measures.
    dims: (usize, usize),
}

/// What an area query reads and answers, once its budgets have passed.
pub(crate) struct AreaPlan {
    /// The answer's cell-centre axes: `x` eastward in CRS84 (from 180° on
    /// to -180° across the antimeridian, as `QueryPolygon::grid_axes`
    /// steps), `y` descending.
    pub(crate) x: Vec<f64>,
    pub(crate) y: Vec<f64>,
    /// What each timestep reads, and how its pixels become the cells.
    pub(crate) read: AreaRead,
}

/// The native windows an area query reads per timestep, and how their
/// pixels become the answer's cells.
pub(crate) enum AreaRead {
    /// A geographic source: windows on the same rows whose pixels, side by
    /// side west to east, are the cells. One window, or two where a global
    /// raster's seam lies inside the bbox: its last columns, then its first.
    Native(Vec<Window>),
    /// A projected source: one native window, each cell the pixel of it
    /// holding the cell's centre, through `projection`.
    Sampled {
        window: Window,
        projection: ProjectionGrid,
    },
}

impl AreaGrid {
    /// The answer to an area query over `bbox` on `geo_transform`; a bbox
    /// that misses the raster is the 400 it always was.
    pub(crate) fn new(geo_transform: &GeoTransform, bbox: &Bbox) -> Result<Self, DataServerError> {
        let kind = if matches!(geo_transform.crs, Crs::Wgs84) {
            Kind::Native {
                windows: native_windows(geo_transform, bbox)?,
                antimeridian: antimeridian_name(bbox),
            }
        } else {
            Kind::Regular(RegularGrid::new(geo_transform, bbox)?)
        };
        Ok(Self {
            geo_transform: geo_transform.clone(),
            kind,
        })
    }

    /// The answer's `(nx, ny)` cells.
    pub(crate) fn dims(&self) -> (usize, usize) {
        match &self.kind {
            Kind::Regular(regular) => regular.dims,
            Kind::Native { windows, .. } => windows_dims(windows),
        }
    }

    /// Run the query's budgets on this grid, then build what it reads and
    /// answers. The budgets, before any file is read and before the axes
    /// are allocated: the shared polygon mask budget over the answer's cells
    /// (#889), when a polygon is masked, and the shared response budget over
    /// `timesteps × ny × nx` (#858). A projected source's native window,
    /// which each timestep reads whole before sampling, is held to
    /// [`MAX_AREA_VALUES`] pixels too.
    pub(crate) fn plan(
        self,
        polygon: Option<&QueryPolygon>,
        timesteps: usize,
        collection: &str,
    ) -> Result<AreaPlan, DataServerError> {
        let (nx, ny) = self.dims();
        if let Some(polygon) = polygon {
            check_mask_budget(nx.saturating_mul(ny), polygon)?;
        }
        check_area_budget(timesteps, ny, nx, 1)?;
        let geo_transform = self.geo_transform;
        match self.kind {
            Kind::Native {
                windows,
                antimeridian,
            } => {
                let (col_start, row_start, col_end, row_end) = windows[0];
                let mid_row = (row_start + row_end) / 2;
                let mid_col = (col_start + col_end) / 2;
                // The pixel centres' longitudes in CRS84, as the bbox names
                // them: a raster stored past 180° (0–360°, or across the
                // antimeridian), and a pixel centred on ±180°.
                let pixel = geo_transform.pixel_width;
                let x = windows
                    .iter()
                    .flat_map(|&(col_start, _, col_end, _)| col_start..col_end)
                    .map(|c| {
                        let lon = geo_transform.pixel_to_world(c, mid_row).0;
                        crs84_lon(lon, antimeridian, pixel)
                    })
                    .collect();
                let y = (row_start..row_end)
                    .map(|r| geo_transform.pixel_to_world(mid_col, r).1)
                    .collect();
                Ok(AreaPlan {
                    x,
                    y,
                    read: AreaRead::Native(windows),
                })
            }
            Kind::Regular(regular) => {
                let cells = QueryPolygon {
                    exterior: Vec::new(),
                    holes: Vec::new(),
                    bbox: regular.crs84,
                };
                let axes = cells.grid_axes(nx, ny);
                let projection = regular.projection(&geo_transform, axes.dims(), collection);
                // No cell on the raster: a bbox in a corner of the envelope
                // a rotated raster leaves empty misses the raster, the 400 it
                // always was, never a grid of nulls (and on a STAC source no
                // cold item loads for it).
                let window = source_window(&projection, axes.dims(), &geo_transform)
                    .ok_or_else(misses_raster)?;
                let (window_nx, window_ny) = windows_dims(&[window]);
                let pixels = window_nx * window_ny;
                if pixels > MAX_AREA_VALUES {
                    return Err(DataServerError::QueryTooLarge(format!(
                        "Area query would read {pixels} source pixels ({window_ny} × \
                         {window_nx} of the native grid) for each timestep; the limit is \
                         {MAX_AREA_VALUES} — narrow the polygon"
                    )));
                }
                Ok(AreaPlan {
                    x: axes.x,
                    y: axes.y,
                    read: AreaRead::Sampled { window, projection },
                })
            }
        }
    }
}

impl AreaPlan {
    /// The native windows each timestep reads, in the order
    /// [`Self::cells`] takes their pixels.
    pub(crate) fn windows(&self) -> &[Window] {
        match &self.read {
            AreaRead::Native(windows) => windows,
            AreaRead::Sampled { window, .. } => std::slice::from_ref(window),
        }
    }

    /// The `(nx, ny)` native pixels each timestep reads, its windows side
    /// by side.
    #[cfg(test)]
    pub(crate) fn window_dims(&self) -> (usize, usize) {
        windows_dims(self.windows())
    }

    /// One timestep's cells, row-major `y × x`, from `reads`: what
    /// `reader::read_bbox` gave for each of [`Self::windows`], in order. A
    /// geographic source's cells are the windows' pixels side by side; a
    /// projected source's each the pixel holding its centre, nearest and
    /// never interpolated, so radar classes stay the file's values.
    pub(crate) fn cells(&self, reads: Vec<Vec<Option<f64>>>) -> Vec<Option<f64>> {
        let (nx, ny) = (self.x.len(), self.y.len());
        match &self.read {
            AreaRead::Native(windows) if windows.len() == 1 => {
                reads.into_iter().next().unwrap_or_default()
            }
            AreaRead::Native(windows) => {
                let mut cells = Vec::with_capacity(nx * ny);
                for row in 0..ny {
                    for (&(col_start, _, col_end, _), read) in windows.iter().zip(&reads) {
                        let width = (col_end - col_start) as usize;
                        cells.extend_from_slice(&read[row * width..(row + 1) * width]);
                    }
                }
                cells
            }
            AreaRead::Sampled { window, projection } => {
                let (window_nx, _) = windows_dims(&[*window]);
                let mut cells = Vec::with_capacity(nx * ny);
                crate::resample_nearest(
                    projection,
                    nx as u32,
                    ny as u32,
                    reads.first().map_or(&[][..], Vec::as_slice),
                    window_nx,
                    *window,
                    None,
                    (0, nx as u32 - 1, 0, ny as u32 - 1),
                    &mut cells,
                );
                cells
            }
        }
    }
}

/// The 400 of an area query whose bbox misses the raster.
fn misses_raster() -> DataServerError {
    DataServerError::InvalidParameter("Requested bbox does not intersect the raster".into())
}

/// The `(nx, ny)` pixels of windows on the same rows, side by side.
fn windows_dims(windows: &[Window]) -> (usize, usize) {
    let nx = windows
        .iter()
        .map(|&(col_start, _, col_end, _)| (col_end - col_start) as usize)
        .sum();
    let ny = windows.first().map_or(0, |&(_, row_start, _, row_end)| {
        (row_end - row_start) as usize
    });
    (nx, ny)
}

/// A geographic source's native windows under `bbox` (#1442), on the same
/// rows, west to east: the pixels `GeoTransform::bbox_to_pixels` gives the
/// bbox's arc in the raster's own longitude frame ([`lon_arc`] round
/// [`frame_centre`]), never its sweep from `west` to `east` through 0°,
/// which across the antimeridian took nearly every column of a global
/// raster and none of a regional one.
///
/// A regional raster answers the part of the bbox it covers, one side of
/// the antimeridian if that is all it holds, and a raster stored past 180°
/// (170–190°E) the bbox's longitudes on it. An arc over the raster's seam,
/// its western edge a turn on, is split there, whatever the raster's width:
/// on a global raster (170–190°E on one from 180°W, 350–370°E on one from
/// 0°) two windows, its last columns then its first; on a near-global one
/// (1439 columns of 0.25° from 180°W) each piece clamped to the columns it
/// has, and a piece off the raster reads none. No column is read twice,
/// nor more than the raster's width: on a raster whose seam lies half a
/// pixel off ±180° (edges at -180.5° and 179.5°), the pixel centred on
/// ±180° meets both ends of a whole-world arc, and is read once, at its
/// west end. The cells and budgets count those pixels alone. A bbox off
/// the raster is the 400.
fn native_windows(
    geo_transform: &GeoTransform,
    bbox: &Bbox,
) -> Result<Vec<Window>, DataServerError> {
    let pixel = geo_transform.pixel_width;
    let span = f64::from(geo_transform.width) * pixel;
    // A whole turn within half a pixel holds every longitude, as the
    // shared sampler's periodic grids do.
    let global = span >= 360.0 - pixel / 2.0;
    let (west, east) = lon_arc(
        bbox_arc(bbox),
        (geo_transform.origin_x, if global { 360.0 } else { span }),
        frame_centre(geo_transform),
    )
    .ok_or_else(misses_raster)?;
    let (origin, seam) = (geo_transform.origin_x, geo_transform.origin_x + 360.0);
    let mut pieces = if west < origin {
        vec![(west + 360.0, seam), (origin, east)]
    } else if east > seam {
        vec![(west, seam), (origin, east - 360.0)]
    } else {
        vec![(west, east)]
    };
    // A piece of the split narrower than a millionth of a pixel is a
    // rounding of the seam beside a real one, no column of the answer.
    let sliver = |&(west, east): &(f64, f64)| east - west <= pixel * 1e-6;
    if pieces.len() == 2 && pieces.iter().filter(|piece| sliver(piece)).count() == 1 {
        pieces.retain(|piece| !sliver(piece));
    }
    let mut windows: Vec<Window> = pieces
        .into_iter()
        .filter_map(|(west, east)| geo_transform.bbox_to_pixels(west, bbox.south, east, bbox.north))
        .collect();
    // The second window starts at column 0; where it reaches the first's
    // columns, the arc's east end has come round to its west end: those
    // columns are the first window's.
    if let [(first_start, ..), (second_start, _, second_end, _)] = &mut windows[..] {
        *second_end = (*second_end).min(*first_start);
        if *second_end <= *second_start {
            windows.pop();
        }
    }
    if windows.is_empty() {
        return Err(misses_raster());
    }
    // The pieces share the bbox's latitudes, so their rows.
    debug_assert!(windows
        .iter()
        .all(|&(_, row_start, _, row_end)| (row_start, row_end) == (windows[0].1, windows[0].3)));
    Ok(windows)
}

/// The bbox's longitude arc `(start, span)` in degrees, eastward from
/// `west`: across the antimeridian (`west > east`) it reaches `east` a turn
/// on.
fn bbox_arc(bbox: &Bbox) -> (f64, f64) {
    let span = if bbox.crosses_antimeridian() {
        bbox.east + 360.0 - bbox.west
    } else {
        bbox.east - bbox.west
    };
    (bbox.west, span)
}

/// The CRS84 longitude the bbox names the antimeridian by: 180° in a bbox
/// whose middle lies east of the prime meridian, so reaching it from the
/// west (170,10,180,20, 0,-10,180,10, or across it), else -180°
/// (-180,-10,0,10, the whole world). Its pixel's centre then lies inside
/// the bbox, which the centre-in mask keeps, and the `x` axis steps on.
fn antimeridian_name(bbox: &Bbox) -> f64 {
    let (west, span) = bbox_arc(bbox);
    if west + span / 2.0 > 0.0 {
        180.0
    } else {
        -180.0
    }
}

/// A pixel centre's longitude in CRS84: as is within ±180°, else wrapped
/// there; on the antimeridian, within a millionth of a `pixel`, the name
/// `antimeridian` the bbox gives it ([`antimeridian_name`]).
fn crs84_lon(lon: f64, antimeridian: f64, pixel: f64) -> f64 {
    let lon = if (-180.0..=180.0).contains(&lon) {
        lon
    } else {
        wrap_lon(lon)
    };
    if 180.0 - lon.abs() <= pixel * 1e-6 {
        antimeridian
    } else {
        lon
    }
}

impl RegularGrid {
    /// The regular CRS84 grid over `bbox` on a projected `geo_transform`.
    ///
    /// Extent: the bbox where the raster's CRS84 envelope covers it, so a
    /// bbox reaching past the raster answers the raster's part, and one
    /// missing the envelope is the 400. The envelope comes from the grid's
    /// edges (`footprint_envelope`, poles and the antimeridian included),
    /// not from points sampled along the bbox's edges: a wide bbox's
    /// sampled edges miss the projected bulge of a parallel between
    /// samples, at the central meridian, which cut rows off the answer.
    ///
    /// Step ([`pixel_ground_size`]): the native pixel's ground size in
    /// degrees, measured at the grid's centre, its longitude step taken at
    /// the grid's latitude nearest the equator. A degree of longitude is
    /// longest there, so no cell anywhere is wider than a native pixel and
    /// nearest sampling skips none; poleward cells are finer. The CRS84 span
    /// of the window over its pixels would be coarser wherever the grid is
    /// rotated against the meridians, and a step at the centre's latitude
    /// coarser on the grid's equatorward side: both skipped pixels, small
    /// echoes with them. A polar cap takes the same rule; its latitude step
    /// for longitude would multiply its cells by 1 / cos(edge latitude).
    /// Never fitted to the budget (`QueryPolygon::fit_grid`, #1436):
    /// GeoTIFF answers at native resolution and refuses an area over budget
    /// (#858).
    fn new(geo_transform: &GeoTransform, bbox: &Bbox) -> Result<Self, DataServerError> {
        let [env_west, env_south, env_east, env_north] = geo_transform
            .footprint_envelope()
            .ok_or_else(misses_raster)?;
        let (south, north) = (bbox.south.max(env_south), bbox.north.min(env_north));
        if south > north {
            return Err(misses_raster());
        }
        let (west, east) = lon_arc(
            bbox_arc(bbox),
            (env_west, env_east - env_west),
            frame_centre(geo_transform),
        )
        .ok_or_else(misses_raster)?;

        let centre = ((west + east) / 2.0, (south + north) / 2.0);
        let (column_m, row_m) = pixel_ground_size(geo_transform, centre)
            .or_else(|| pixel_ground_size(geo_transform, raster_centre(geo_transform)?))
            .unwrap_or((f64::NAN, f64::NAN));
        let equatorward = if south <= 0.0 && north >= 0.0 {
            0.0
        } else {
            south.abs().min(north.abs())
        };
        let metres_per_degree = EARTH_RADIUS_M * std::f64::consts::PI / 180.0;
        let step = (
            column_m / (metres_per_degree * equatorward.to_radians().cos()),
            row_m / metres_per_degree,
        );

        // CRS84 longitudes for the shared cell centres.
        let crs84_west = west - 360.0 * ((west + 180.0) / 360.0).floor();
        let mut crs84_east = crs84_west + (east - west);
        if crs84_east > 180.0 {
            crs84_east -= 360.0;
        }
        let crs84 = Bbox {
            west: crs84_west,
            south,
            east: crs84_east,
            north,
        };
        // The shared native count, so the budgets run on it before the
        // axes are allocated.
        let dims = QueryPolygon {
            exterior: Vec::new(),
            holes: Vec::new(),
            bbox: crs84,
        }
        .native_dims(step.0, step.1);
        Ok(Self {
            frame: [west, south, east, north],
            crs84,
            dims,
        })
    }

    /// The fractional source `(col, row)` of each cell's centre, the coarse
    /// grid [`crate::resample_nearest`] samples, over `dims` cells. A grid
    /// that stopped at its cell cap over the error budget logs a WARN.
    fn projection(
        &self,
        geo_transform: &GeoTransform,
        (nx, ny): (usize, usize),
        collection: &str,
    ) -> ProjectionGrid {
        let (nx, ny) = (nx as u32, ny as u32);
        let [west, south, east, north] = self.frame;
        let grid = ProjectionGrid::build_2d_within_quiet(
            nx,
            ny,
            (0, nx - 1, 0, ny - 1),
            geo_transform.width,
            geo_transform.height,
            |fx, fy| (west + fx * (east - west), north - fy * (north - south)),
            |lon, lat| geo_transform.world_to_pixel_f64(lon, lat),
        );
        if let Some(error_px) = grid.capped_error() {
            tracing::warn!(
                collection,
                bbox = ?self.frame,
                nx,
                ny,
                error_px,
                "EDR area projection grid hit its cell cap over the interpolation budget; \
                 cells near a pixel edge may take a neighbouring pixel"
            );
        }
        grid
    }
}

/// The native window holding every pixel a cell of `projection` takes, as
/// [`crate::resample_nearest`] takes it: the floor of each cell's
/// interpolated source position, on the raster. Exact by construction,
/// whatever the bbox's shape; `None` when no cell lies on the raster,
/// which [`AreaGrid::plan`] answers as a bbox missing the raster.
fn source_window(
    projection: &ProjectionGrid,
    (nx, ny): (usize, usize),
    geo_transform: &GeoTransform,
) -> Option<(u32, u32, u32, u32)> {
    let (width, height) = (
        f64::from(geo_transform.width),
        f64::from(geo_transform.height),
    );
    let (mut col_start, mut row_start, mut col_end, mut row_end) = (u32::MAX, u32::MAX, 0, 0);
    for oy in 0..ny as u32 {
        for ox in 0..nx as u32 {
            let (col, row) = projection.sample(ox, oy);
            let (col, row) = (col.floor(), row.floor());
            // Non-finite positions fail every comparison.
            if col >= 0.0 && row >= 0.0 && col < width && row < height {
                let (col, row) = (col as u32, row as u32);
                col_start = col_start.min(col);
                row_start = row_start.min(row);
                col_end = col_end.max(col + 1);
                row_end = row_end.max(row + 1);
            }
        }
    }
    (col_start < col_end).then_some((col_start, row_start, col_end, row_end))
}

/// The intersection of two longitude arcs, `(start, span)` in degrees: the
/// bbox's and the raster envelope's (`span >= 360` is every longitude), as
/// `(west, east)` with `west <= east`, its middle placed within half a turn
/// of the source's frame centre `centre` ([`frame_centre`]) where it has
/// one, by the rule [`source_longitude`] places a point by: where its
/// forward transform is meant to be evaluated, a Lambert conformal conic's
/// not being periodic in longitude (#1114), and a geographic raster's
/// columns linear in it. None give `None`.
///
/// Two pieces, when the arcs meet at both ends (a bbox nearly a whole turn
/// wide, its gap inside the envelope), give the shorter of the two arcs
/// joining them: the envelope's side, never one reaching round the bbox
/// past the raster. The rectangle mask nulls the gap's cells. A zero-width
/// piece, an arc touching the other's end, is no piece beside a real one.
pub(crate) fn lon_arc(
    (bbox_start, bbox_span): (f64, f64),
    (env_start, env_span): (f64, f64),
    centre: Option<f64>,
) -> Option<(f64, f64)> {
    let (west, east) = if env_span >= 360.0 {
        (bbox_start, bbox_start + bbox_span)
    } else if bbox_span >= 360.0 {
        (env_start, env_start + env_span)
    } else {
        // The envelope's start measured eastward from the bbox's, so both
        // arcs begin within one turn of `bbox_start`.
        let offset = (env_start - bbox_start).rem_euclid(360.0);
        let bbox_end = bbox_start + bbox_span;
        let first = (offset <= bbox_span).then(|| {
            (
                bbox_start + offset,
                bbox_end.min(bbox_start + offset + env_span),
            )
        });
        // The envelope's end, a turn on, reaching past the bbox's start.
        let wrapped_end = bbox_start + offset + env_span - 360.0;
        let second = (wrapped_end >= bbox_start).then(|| (bbox_start, bbox_end.min(wrapped_end)));
        match (first, second) {
            (Some(first), Some(second)) if second.1 <= second.0 => first,
            (Some(first), Some(second)) if first.1 <= first.0 => second,
            // The arc on from the first piece through the bbox's seam to
            // the second, a turn on, or the one back across the bbox's
            // interior: whichever is shorter.
            (Some(first), Some(second)) => {
                if second.1 + 360.0 - first.0 <= first.1 - second.0 {
                    (first.0, second.1 + 360.0)
                } else {
                    (second.0, first.1)
                }
            }
            (Some(arc), None) | (None, Some(arc)) => arc,
            (None, None) => return None,
        }
    };
    // Whole turns, so both ends move by exactly the same amount.
    let shift = centre.map_or(0.0, |centre| {
        let middle = (west + east) / 2.0;
        360.0 * ((lon_within_half_turn(middle, centre) - middle) / 360.0).round()
    });
    Some((west + shift, east + shift))
}

/// The ground size `(column, row)` in metres of the native pixel at
/// `(lon, lat)`: the lengths of a column step and a row step there,
/// inverse-projected and measured on the sphere `great_circle_distance_m`
/// measures (and [`RegularGrid::new`] turns into degrees on). `None` where
/// the point or a step does not project.
fn pixel_ground_size(geo_transform: &GeoTransform, (lon, lat): (f64, f64)) -> Option<(f64, f64)> {
    let crs = &geo_transform.crs;
    let (x, y) = crs.forward(lon, lat);
    if !(x.is_finite() && y.is_finite()) {
        return None;
    }
    let (lon, lat) = crs.inverse(x, y)?;
    let (col_lon, col_lat) = crs.inverse(x + geo_transform.pixel_width, y)?;
    let (row_lon, row_lat) = crs.inverse(x, y - geo_transform.pixel_height)?;
    let column = great_circle_distance_m(lon, lat, col_lon, col_lat);
    let row = great_circle_distance_m(lon, lat, row_lon, row_lat);
    let positive = |d: f64| d.is_finite() && d > 0.0;
    (positive(column) && positive(row)).then_some((column, row))
}

/// The raster's centre pixel's `(lon, lat)`.
fn raster_centre(geo_transform: &GeoTransform) -> Option<(f64, f64)> {
    geo_transform.crs.inverse(
        geo_transform.origin_x + f64::from(geo_transform.width) / 2.0 * geo_transform.pixel_width,
        geo_transform.origin_y - f64::from(geo_transform.height) / 2.0 * geo_transform.pixel_height,
    )
}

/// The longitude the source's forward transform is meant to be given
/// longitudes within half a turn of: a geographic raster's middle, its
/// columns linear in longitude from its western edge, else the CRS's
/// central longitude where it has one ([`central_longitude`]).
fn frame_centre(geo_transform: &GeoTransform) -> Option<f64> {
    match &geo_transform.crs {
        Crs::Wgs84 => {
            let middle = geo_transform.origin_x
                + f64::from(geo_transform.width) * geo_transform.pixel_width / 2.0;
            middle.is_finite().then_some(middle)
        }
        crs => central_longitude(crs),
    }
}

/// `lon` as the source's forward transform is meant to be given it: moved
/// by whole turns to within half a turn of [`frame_centre`] (#1442). A
/// position query's point takes it, as [`lon_arc`] places an area's frame:
/// a Lambert conformal conic's forward is not periodic in longitude (#1114),
/// so a point near ±180° on a cone centred there lands on its own side, and
/// a geographic raster stored past 180° (0–360°, or across the
/// antimeridian) finds its columns for -170°.
pub(crate) fn source_longitude(geo_transform: &GeoTransform, lon: f64) -> f64 {
    frame_centre(geo_transform).map_or(lon, |centre| lon_within_half_turn(lon, centre))
}

/// The source CRS's central longitude in degrees, where it has one.
fn central_longitude(crs: &Crs) -> Option<f64> {
    match crs {
        Crs::TransverseMercator { lon0, .. }
        | Crs::LambertAzimuthalEqualArea { lon0, .. }
        | Crs::LambertConformalConic { lon0, .. }
        | Crs::Stereographic { lon0, .. }
        | Crs::Geostationary { lon0, .. } => Some(lon0.to_degrees()),
        _ => None,
    }
}
