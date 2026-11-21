//! What a map render reads of one level of the source, in the raster's own
//! longitude frame (#1540).
//!
//! A map's longitudes are the view's: within ±180° in CRS84, past 180°
//! where OGC API Maps unwraps a box across the antimeridian (#828) or the
//! WMS meta-tiling passes a tile past it, a turn on in a world copy, and in
//! a projected output its inverse's own frame (EPSG:3035's runs from 170°W
//! to 190°E). A raster's columns have a frame of their own: from its
//! western edge, 0° to 360° for many global grids. The map path moves the
//! view into it by the rules the EDR queries follow (#1442), so WMS
//! GetFeatureInfo, which asks EDR position, answers what GetMap drew:
//!
//! - **Geographic source:** columns are linear in longitude, so the coarse
//!   grid maps the view's longitudes to columns as they come, continuous
//!   across the raster's seam ([`source_pixel`]). Moving each node into the
//!   raster's frame instead would put a jump of a turn between two nodes of
//!   every view across 0° of a raster stored from 0° to 360°. The windows
//!   are [`area::native_windows`] of the view: one, or two either side of
//!   the raster's seam, its last columns then its first. Each output pixel
//!   takes its column from the window holding it, a whole turn on or back
//!   where none holds it as it comes ([`MapRead::col_period`]).
//! - **Projected source:** each node's longitude moves to within half a
//!   turn of the CRS's central longitude, as EDR position's point does
//!   ([`area::source_longitude`]): a Lambert cone centred on 180° is not
//!   periodic in longitude (#1114), and draws -179.9° from its own side.
//!   Its one window is `GeoTransform::bbox_to_pixels` of the view moved by
//!   whole turns into that frame.
//!
//! A view inside the raster's frame reads the window it read before, and no
//! column moves: a raster stored from 180°W, or a regional one inside ±180°,
//! renders as it did.

use ds_core::feature::Bbox;
use ds_core::geo::{Crs, GeoTransform};
use ds_core::map_engine::OutputCrs;

use crate::area::{self, Window};

/// The native windows a map render reads of one level, and how its output
/// pixels find their columns in them.
pub(crate) struct MapRead {
    /// On the same rows, side by side west to east: none when the view
    /// misses the level, two across a geographic raster's seam.
    windows: Vec<Window>,
    /// A geographic level's columns per turn.
    col_period: Option<f64>,
}

impl MapRead {
    /// What a map of `bbox` (`[west, south, east, north]`, `west <= east`,
    /// `east` past 180° across the antimeridian) reads of the level whose
    /// transform is `geo_transform`.
    pub(crate) fn new(geo_transform: &GeoTransform, bbox: [f64; 4]) -> Self {
        let [west, south, east, north] = bbox;
        if matches!(geo_transform.crs, Crs::Wgs84) {
            let bbox = Bbox {
                west,
                south,
                east,
                north,
            };
            let pixel = geo_transform.pixel_width;
            return Self {
                // A view missing the raster reads nothing: the EDR area's
                // 400 is no map's answer.
                windows: area::native_windows(geo_transform, &bbox).unwrap_or_default(),
                col_period: (pixel.is_finite() && pixel > 0.0).then(|| 360.0 / pixel),
            };
        }
        let shift = area::frame_centre(geo_transform)
            .map_or(0.0, |centre| area::whole_turns((west + east) / 2.0, centre));
        Self {
            windows: geo_transform
                .bbox_to_pixels(west + shift, south, east + shift, north)
                .into_iter()
                .collect(),
            col_period: None,
        }
    }

    /// The windows, in the order their pixels lie west to east.
    pub(crate) fn windows(&self) -> &[Window] {
        &self.windows
    }

    /// A geographic level's columns per turn: an output pixel whose column
    /// no window holds takes the column a whole number of turns on that one
    /// holds. `None` for a projected level, whose nodes are already in its
    /// frame.
    pub(crate) fn col_period(&self) -> Option<f64> {
        self.col_period
    }

    /// The `(columns, rows)` the windows hold side by side, as overview
    /// selection counts a level's pixels; `None` when there are none.
    pub(crate) fn dims(&self) -> Option<(u32, u32)> {
        let (_, row_start, _, row_end) = *self.windows.first()?;
        let columns = self
            .windows
            .iter()
            .map(|&(col_start, _, col_end, _)| col_end - col_start)
            .sum();
        Some((columns, row_end - row_start))
    }

    /// The source pixels the windows hold.
    pub(crate) fn pixels(&self) -> usize {
        self.dims()
            .map_or(0, |(columns, rows)| columns as usize * rows as usize)
    }
}

/// The fractional source `(col, row)` of WGS84 `(lon, lat)` on the level
/// `geo_transform`, as a map's coarse grid evaluates it at its nodes: a
/// geographic level's continuous in longitude, a projected level's from the
/// longitude in its CRS's frame ([`area::source_longitude`]).
pub(crate) fn source_pixel(geo_transform: &GeoTransform, lon: f64, lat: f64) -> (f64, f64) {
    let lon = match geo_transform.crs {
        Crs::Wgs84 => lon,
        _ => area::source_longitude(geo_transform, lon),
    };
    geo_transform.world_to_pixel_f64(lon, lat)
}

/// The output window `(px_lo, px_hi, py_lo, py_hi)` a source's footprint
/// `envelope` (`GeoTransform::footprint_envelope`: `west <= east`, past 180°
/// across the antimeridian) can occupy in a `width` × `height` map of `bbox`
/// (`west <= east`), `None` when it lies wholly off the image: the low-zoom
/// ghost guard (#449, #453, #1129).
///
/// On a geographic or Web Mercator output the envelope moves by whole turns
/// to the view, and every copy of it the view meets counts, the nearest and
/// a turn either side, joined (#1540): a raster across the antimeridian
/// draws in a view from 180°W and on both edges of a whole-world view, and a
/// world copy a turn on draws. Before, the envelope stayed where the grid's
/// extent put it, so those pixels were left empty. An envelope spanning
/// every longitude (a grid holding a pole) bounds latitude only, over the
/// view's own longitudes, as the shared sampler's does
/// (`ds_core::resample::SourceGrid`). A projected output places the
/// envelope by its own forward transform, as it comes.
pub(crate) fn footprint_window(
    output_crs: &OutputCrs,
    bbox: [f64; 4],
    envelope: [f64; 4],
    width: u32,
    height: u32,
) -> Option<(u32, u32, u32, u32)> {
    let window = |envelope| output_crs.footprint_window_in_image(bbox, envelope, width, height);
    if matches!(output_crs, OutputCrs::Projected { .. }) {
        return window(envelope);
    }
    let [w, s, e, n] = envelope;
    let [west, _, east, _] = bbox;
    if e - w >= 360.0 {
        return window([west, s, east, n]);
    }
    let nearest = area::whole_turns((w + e) / 2.0, (west + east) / 2.0);
    [nearest - 360.0, nearest, nearest + 360.0]
        .into_iter()
        .filter_map(|turns| window([w + turns, s, e + turns, n]))
        .reduce(|a, b| (a.0.min(b.0), a.1.max(b.1), a.2.min(b.2), a.3.max(b.3)))
}

/// A window a render read, with its row-major pixels.
pub(crate) struct ReadWindow<'a, T> {
    window: Window,
    pixels: &'a [T],
    columns: usize,
}

impl<'a, T: Copy> ReadWindow<'a, T> {
    pub(crate) fn new(window: Window, pixels: &'a [T]) -> Self {
        let (col_start, _, col_end, _) = window;
        Self {
            window,
            pixels,
            columns: (col_end - col_start) as usize,
        }
    }

    /// The pixel at fractional source `col` on source row `row` (already
    /// floored), `None` when the window does not hold it; `fill` for a
    /// pixel the window holds but its buffer does not.
    #[inline]
    pub(crate) fn at(&self, col: f64, row: f64, fill: T) -> Option<T> {
        let col = col.floor();
        let (col_start, row_start, col_end, row_end) = self.window;
        if col >= col_start as f64
            && col < col_end as f64
            && row >= row_start as f64
            && row < row_end as f64
        {
            let index = (row as usize - row_start as usize) * self.columns
                + (col as usize - col_start as usize);
            Some(self.pixels.get(index).copied().unwrap_or(fill))
        } else {
            None
        }
    }

    /// `col` moved by whole turns of `period` columns to the turn starting
    /// at the window's first column.
    #[inline]
    pub(crate) fn turned(&self, col: f64, period: f64) -> f64 {
        let start = f64::from(self.window.0);
        start + (col - start).rem_euclid(period)
    }
}
