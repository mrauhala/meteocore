//! The grid an EDR area or radius query answers (#1435).
//!
//! A geographic source's pixel centres are a regular CRS84 grid already, so
//! the answer is its native pixel window. A projected source's are not: a
//! cell's longitude depends on its row as well as its column, so no pair of
//! `x` and `y` axes places its native cells, and axes taken along the
//! window's middle row and column put the corners of a few degrees' window
//! kilometres from their data. A projected source answers a regular CRS84
//! grid instead, the cell centres of [`QueryPolygon::sample_grid`], each cell
//! the value of the native pixel holding its centre: nearest, never
//! interpolated, so radar classes stay the values the file holds. The cells
//! reach their pixels through a coarse [`ProjectionGrid`], never one CRS
//! transform per cell (root Critical Rule 5).
//!
//! [`AreaGrid::new`] settles the answer's extent and cell count without
//! allocating either; [`AreaGrid::plan`] runs the budgets on them before it
//! builds the axes, the projection grid and the native window to read.

use ds_core::error::DataServerError;
use ds_core::feature::{check_area_budget, check_mask_budget, Bbox, QueryPolygon, MAX_AREA_VALUES};
use ds_core::geo::{great_circle_distance_m, Crs, GeoTransform, EARTH_RADIUS_M};
use ds_core::resample::ProjectionGrid;

/// The answer to one area query over one source grid, before its budgets.
pub(crate) struct AreaGrid {
    /// The source grid the answer is read from: the first matching entry's.
    geo_transform: GeoTransform,
    kind: Kind,
}

enum Kind {
    /// A geographic source: its native pixel window
    /// `(col_start, row_start, col_end, row_end)` is the answer.
    Native { window: (u32, u32, u32, u32) },
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
    /// antimeridian, for [`QueryPolygon::sample_grid`].
    crs84: Bbox,
    /// The cell size `(dlon, dlat)` in degrees.
    step: (f64, f64),
    /// `(nx, ny)`: the cells `sample_grid` gives `crs84` at `step`.
    dims: (usize, usize),
}

/// What an area query reads and answers, once its budgets have passed.
pub(crate) struct AreaPlan {
    /// The source grid the answer is read from.
    pub(crate) geo_transform: GeoTransform,
    /// The answer's cell-centre axes: `x` ascending, `y` descending.
    pub(crate) x: Vec<f64>,
    pub(crate) y: Vec<f64>,
    /// The native window `(col_start, row_start, col_end, row_end)` each
    /// timestep reads.
    pub(crate) window: (u32, u32, u32, u32),
    /// A projected source's cell → source pixel map; `None` for a
    /// geographic source, whose window is the answer.
    pub(crate) projection: Option<ProjectionGrid>,
}

impl AreaGrid {
    /// The answer to an area query over `bbox` on `geo_transform`; a bbox
    /// that misses the raster is the 400 it always was.
    pub(crate) fn new(geo_transform: &GeoTransform, bbox: &Bbox) -> Result<Self, DataServerError> {
        let kind = if matches!(geo_transform.crs, Crs::Wgs84) {
            Kind::Native {
                window: crate::area_pixel_window(
                    geo_transform,
                    bbox.west,
                    bbox.south,
                    bbox.east,
                    bbox.north,
                )?,
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
            Kind::Native { window } => window_dims(*window),
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
            Kind::Native { window } => {
                let (col_start, row_start, col_end, row_end) = window;
                let mid_row = (row_start + row_end) / 2;
                let mid_col = (col_start + col_end) / 2;
                let x = (col_start..col_end)
                    .map(|c| geo_transform.pixel_to_world(c, mid_row).0)
                    .collect();
                let y = (row_start..row_end)
                    .map(|r| geo_transform.pixel_to_world(mid_col, r).1)
                    .collect();
                Ok(AreaPlan {
                    geo_transform,
                    x,
                    y,
                    window,
                    projection: None,
                })
            }
            Kind::Regular(regular) => {
                let cells = QueryPolygon {
                    exterior: Vec::new(),
                    holes: Vec::new(),
                    bbox: regular.crs84,
                };
                let axes = cells.sample_grid(regular.step.0, regular.step.1, nx.max(ny));
                debug_assert_eq!(axes.dims(), (nx, ny));
                let projection = regular.projection(&geo_transform, axes.dims(), collection);
                // No cell on the raster: a bbox in a corner of the envelope
                // a rotated raster leaves empty misses the raster, the 400 it
                // always was, never a grid of nulls (and on a STAC source no
                // cold item loads for it).
                let window = source_window(&projection, axes.dims(), &geo_transform)
                    .ok_or_else(misses_raster)?;
                let (window_nx, window_ny) = window_dims(window);
                let pixels = window_nx * window_ny;
                if pixels > MAX_AREA_VALUES {
                    return Err(DataServerError::QueryTooLarge(format!(
                        "Area query would read {pixels} source pixels ({window_ny} × \
                         {window_nx} of the native grid) for each timestep; the limit is \
                         {MAX_AREA_VALUES} — narrow the polygon"
                    )));
                }
                Ok(AreaPlan {
                    geo_transform,
                    x: axes.x,
                    y: axes.y,
                    window,
                    projection: Some(projection),
                })
            }
        }
    }
}

impl AreaPlan {
    /// The native window's `(nx, ny)` pixels.
    pub(crate) fn window_dims(&self) -> (usize, usize) {
        window_dims(self.window)
    }
}

/// The 400 of an area query whose bbox misses the raster, as
/// `crate::area_pixel_window` gives it for a geographic source.
fn misses_raster() -> DataServerError {
    DataServerError::InvalidParameter("Requested bbox does not intersect the raster".into())
}

/// A pixel window's `(nx, ny)`.
fn window_dims((col_start, row_start, col_end, row_end): (u32, u32, u32, u32)) -> (usize, usize) {
    (
        (col_end - col_start) as usize,
        (row_end - row_start) as usize,
    )
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
    /// Never coarsened to `MAX_AREA_DIM`: GeoTIFF answers at native
    /// resolution and refuses an area over budget (#858).
    fn new(geo_transform: &GeoTransform, bbox: &Bbox) -> Result<Self, DataServerError> {
        let [env_west, env_south, env_east, env_north] = geo_transform
            .footprint_envelope()
            .ok_or_else(misses_raster)?;
        let (south, north) = (bbox.south.max(env_south), bbox.north.min(env_north));
        if south > north {
            return Err(misses_raster());
        }
        let bbox_span = if bbox.crosses_antimeridian() {
            bbox.east + 360.0 - bbox.west
        } else {
            bbox.east - bbox.west
        };
        let (west, east) = lon_arc(
            (bbox.west, bbox_span),
            (env_west, env_east - env_west),
            central_longitude(&geo_transform.crs),
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
        // `sample_grid`'s own count, computed the same way, so the budgets
        // run on it before the axes are allocated.
        let lon_span = if crs84.crosses_antimeridian() {
            crs84.east + 360.0 - crs84.west
        } else {
            crs84.east - crs84.west
        };
        let cells = |span: f64, res: f64| -> usize {
            if !(res.is_finite() && res > 0.0) {
                return 1;
            }
            ((span / res).ceil() as usize).max(1)
        };
        Ok(Self {
            frame: [west, south, east, north],
            crs84,
            step,
            dims: (cells(lon_span, step.0), cells(north - south, step.1)),
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
/// `(west, east)` with `west <= east`, placed within half a turn of the
/// source's central longitude `centre` where it has one, where its forward
/// transform is meant to be evaluated: a Lambert conformal conic's is not
/// periodic in longitude (#1114). None give `None`.
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
    let shift = centre.map_or(0.0, |centre| {
        360.0 * ((centre - (west + east) / 2.0) / 360.0).round()
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
