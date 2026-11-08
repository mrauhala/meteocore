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

use ds_core::error::DataServerError;
use ds_core::feature::{
    check_area_budget, check_mask_budget, AreaGridAxes, Bbox, QueryPolygon, MAX_AREA_VALUES,
};
use ds_core::geo::{great_circle_distance_m, Crs, GeoTransform, EARTH_RADIUS_M};
use ds_core::resample::ProjectionGrid;

/// The grid of one area query over one source grid.
pub(crate) struct AreaGrid {
    /// The source grid the answer is read from: the first matching entry's.
    pub(crate) geo_transform: GeoTransform,
    /// The native pixel window `(col_start, row_start, col_end, row_end)`
    /// read for each timestep.
    pub(crate) window: (u32, u32, u32, u32),
    /// A projected source's answer grid; `None` for a geographic source,
    /// whose window is the answer.
    pub(crate) regular: Option<RegularGrid>,
}

/// A projected source's answer: a regular CRS84 grid over the bbox.
pub(crate) struct RegularGrid {
    /// The cell centres, longitudes in `(-180, 180]`.
    pub(crate) axes: AreaGridAxes,
    /// The outer edges `[west, south, east, north]` of the cells, its
    /// longitudes in the source's frame (see [`source_frame`]): `west <=
    /// east`, and either may lie past ±180°. The longitudes the source's
    /// forward transform is given.
    frame: [f64; 4],
}

impl AreaGrid {
    /// The grid an area query over `bbox` answers on `geo_transform`; a bbox
    /// that misses the raster is the 400 it always was.
    pub(crate) fn new(geo_transform: &GeoTransform, bbox: &Bbox) -> Result<Self, DataServerError> {
        if matches!(geo_transform.crs, Crs::Wgs84) {
            let window = crate::area_pixel_window(
                geo_transform,
                bbox.west,
                bbox.south,
                bbox.east,
                bbox.north,
            )?;
            return Ok(Self {
                geo_transform: geo_transform.clone(),
                window,
                regular: None,
            });
        }
        let frame = source_frame(&geo_transform.crs, bbox);
        let [west, south, east, north] = frame;
        let window = crate::area_pixel_window(geo_transform, west, south, east, north)?;
        // A pixel more on every side: the window is the envelope of points
        // along the bbox's edges, and a cell centre just inside the bbox
        // may fall a fraction of a pixel past it.
        let (col_start, row_start, col_end, row_end) = window;
        let window = (
            col_start.saturating_sub(1),
            row_start.saturating_sub(1),
            (col_end + 1).min(geo_transform.width),
            (row_end + 1).min(geo_transform.height),
        );
        let regular = RegularGrid::new(geo_transform, window, frame);
        Ok(Self {
            geo_transform: geo_transform.clone(),
            window,
            regular: Some(regular),
        })
    }

    /// The answer's `(nx, ny)` cells.
    pub(crate) fn dims(&self) -> (usize, usize) {
        match &self.regular {
            Some(regular) => regular.axes.dims(),
            None => {
                let (col_start, row_start, col_end, row_end) = self.window;
                (
                    (col_end - col_start) as usize,
                    (row_end - row_start) as usize,
                )
            }
        }
    }

    /// The native window's `(nx, ny)` pixels.
    pub(crate) fn window_dims(&self) -> (usize, usize) {
        let (col_start, row_start, col_end, row_end) = self.window;
        (
            (col_end - col_start) as usize,
            (row_end - row_start) as usize,
        )
    }

    /// The query's budgets on this grid, before any file is read: the
    /// shared polygon mask budget over the answer's cells (#889), when a
    /// polygon is masked, and the shared response budget over `timesteps ×
    /// ny × nx` (#858). A projected source's native window is also held to
    /// [`MAX_AREA_VALUES`] pixels: each timestep reads it whole before
    /// sampling, as `reader::read_bbox` requires.
    pub(crate) fn check(
        &self,
        polygon: Option<&QueryPolygon>,
        timesteps: usize,
    ) -> Result<(), DataServerError> {
        let (nx, ny) = self.dims();
        if let Some(polygon) = polygon {
            check_mask_budget(nx * ny, polygon)?;
        }
        check_area_budget(timesteps, ny, nx, 1)?;
        let (window_nx, window_ny) = self.window_dims();
        let pixels = window_nx * window_ny;
        if self.regular.is_some() && pixels > MAX_AREA_VALUES {
            return Err(DataServerError::QueryTooLarge(format!(
                "Area query would read {pixels} source pixels ({window_ny} × {window_nx} of the \
                 native grid) for each timestep; the limit is {MAX_AREA_VALUES} — narrow the \
                 polygon"
            )));
        }
        Ok(())
    }
}

impl RegularGrid {
    /// The regular CRS84 grid over `frame`, the bbox in the source's
    /// frame, read from `window` of `geo_transform`.
    ///
    /// Resolution: the native pixel's ground size at the window's centre in
    /// CRS84 degrees ([`native_step`]), one cell per native pixel there
    /// whatever the grid's rotation against the meridians. The CRS84 span
    /// of the window's envelope over its pixels would be coarser wherever
    /// the grid is rotated, by 7 % per axis on TM35FIN at 68°N, 5° from its
    /// meridian, and nearest sampling would then skip about one pixel in
    /// seven: small echoes lost. Never coarsened to `MAX_AREA_DIM`: GeoTIFF
    /// answers at native resolution and refuses an area over budget (#858).
    /// A window holding a pole spans every longitude, so its longitude step
    /// is its latitude step: finer than a pixel away from the pole, never
    /// coarser.
    ///
    /// The grid covers the bbox where the window's envelope does, as the
    /// native window covered only the raster: a bbox reaching past the
    /// raster answers no larger, and no more cells off it, than before. An
    /// axis on which the two do not meet keeps the bbox's span.
    fn new(geo_transform: &GeoTransform, window: (u32, u32, u32, u32), frame: [f64; 4]) -> Self {
        let (col_start, row_start, col_end, row_end) = window;
        let (nx, ny) = (col_end - col_start, row_end - row_start);
        let window_grid = GeoTransform {
            origin_x: geo_transform.origin_x + f64::from(col_start) * geo_transform.pixel_width,
            origin_y: geo_transform.origin_y - f64::from(row_start) * geo_transform.pixel_height,
            width: nx,
            height: ny,
            ..geo_transform.clone()
        };
        let [mut west, mut south, mut east, mut north] = frame;
        // Without a centre pixel that inverse-projects, unreachable for a
        // window of a catalogued raster, `sample_grid` answers one cell.
        let mut resolution = native_step(&window_grid).unwrap_or((f64::NAN, f64::NAN));
        if let Some([env_west, env_south, env_east, env_north]) = window_grid.footprint_envelope() {
            if env_east - env_west >= 360.0 {
                resolution.0 = resolution.1;
            } else {
                // The envelope moved by whole turns into the bbox's frame.
                let turns = 360.0 * ((west + east - env_west - env_east) / 720.0).round();
                let (lo, hi) = (west.max(env_west + turns), east.min(env_east + turns));
                if lo < hi {
                    (west, east) = (lo, hi);
                }
            }
            let (lo, hi) = (south.max(env_south), north.min(env_north));
            if lo < hi {
                (south, north) = (lo, hi);
            }
        }
        // The same box with CRS84 longitudes, `west > east` across the
        // antimeridian, for the shared cell centres: only its bbox is read.
        let crs84_west = west - 360.0 * ((west + 180.0) / 360.0).floor();
        let mut crs84_east = crs84_west + (east - west);
        if crs84_east > 180.0 {
            crs84_east -= 360.0;
        }
        let cells = QueryPolygon {
            exterior: Vec::new(),
            holes: Vec::new(),
            bbox: Bbox {
                west: crs84_west,
                south,
                east: crs84_east,
                north,
            },
        };
        // A cap one past the budget: an axis it clamps is over the budget
        // on its own, so the query is refused rather than coarsened.
        let axes = cells.sample_grid(resolution.0, resolution.1, MAX_AREA_VALUES + 1);
        Self {
            axes,
            frame: [west, south, east, north],
        }
    }

    /// The fractional source `(col, row)` of each answer cell's centre, the
    /// coarse grid [`crate::resample_nearest`] samples. Built only once the
    /// budgets have passed: its output is the answer's cells. A grid that
    /// stopped at its cell cap over the error budget logs a WARN.
    pub(crate) fn projection(
        &self,
        geo_transform: &GeoTransform,
        collection: &str,
    ) -> ProjectionGrid {
        let (nx, ny) = self.axes.dims();
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

/// The ground size of `grid`'s centre pixel as CRS84 degrees `(dlon,
/// dlat)` at its latitude: a column step's length for longitude, a row
/// step's for latitude, so a grid of non-square pixels keeps each. The
/// centre and its next column and row are inverse-projected once each, the
/// lengths taken on the sphere `great_circle_distance_m` measures, and so
/// turned into degrees on it too. `None` when one does not inverse-project.
fn native_step(grid: &GeoTransform) -> Option<(f64, f64)> {
    let centre = |columns: f64, rows: f64| {
        let col = f64::from(grid.width / 2) + 0.5 + columns;
        let row = f64::from(grid.height / 2) + 0.5 + rows;
        grid.crs.inverse(
            grid.origin_x + col * grid.pixel_width,
            grid.origin_y - row * grid.pixel_height,
        )
    };
    let (lon, lat) = centre(0.0, 0.0)?;
    let (next_col_lon, next_col_lat) = centre(1.0, 0.0)?;
    let (next_row_lon, next_row_lat) = centre(0.0, 1.0)?;
    let column = great_circle_distance_m(lon, lat, next_col_lon, next_col_lat);
    let row = great_circle_distance_m(lon, lat, next_row_lon, next_row_lat);
    let metres_per_degree = EARTH_RADIUS_M * std::f64::consts::PI / 180.0;
    let dlat = row / metres_per_degree;
    let dlon = column / (metres_per_degree * lat.to_radians().cos());
    let positive = |d: f64| d.is_finite() && d > 0.0;
    (positive(dlon) && positive(dlat)).then_some((dlon, dlat))
}

/// `bbox` as `[west, south, east, north]` with `west <= east`, both moved by
/// whole turns to lie within half a turn of the source CRS's central
/// longitude, where its forward transform is meant to be evaluated. A
/// Lambert conformal conic's is not periodic in longitude (#1114); the
/// others' are, and only an antimeridian-crossing bbox, `west > east`, then
/// changes: its edges sampled from `west` to `east + 360°` rather than the
/// long way round through every other longitude.
fn source_frame(crs: &Crs, bbox: &Bbox) -> [f64; 4] {
    let east = if bbox.crosses_antimeridian() {
        bbox.east + 360.0
    } else {
        bbox.east
    };
    let shift = central_longitude(crs).map_or(0.0, |centre| {
        360.0 * ((centre - (bbox.west + east) / 2.0) / 360.0).round()
    });
    [bbox.west + shift, bbox.south, east + shift, bbox.north]
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
