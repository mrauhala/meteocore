use std::f64::consts::PI;

/// WGS84 ellipsoid parameters
const WGS84_A: f64 = 6_378_137.0; // semi-major axis (meters)
const WGS84_F: f64 = 1.0 / 298.257223563; // flattening
const WGS84_E2: f64 = 2.0 * WGS84_F - WGS84_F * WGS84_F; // eccentricity squared

/// Convert WGS84 geodetic coordinates to Earth-Centered, Earth-Fixed (ECEF)
/// metres (EPSG:4978). `lon_deg`/`lat_deg` are degrees, `h` is metres above
/// the WGS84 ellipsoid. Used to place 3D content (e.g. OGC 3D Tiles volumes)
/// at its true global position; the inverse is not needed here.
pub fn geodetic_to_ecef(lon_deg: f64, lat_deg: f64, h: f64) -> [f64; 3] {
    let lon = lon_deg.to_radians();
    let lat = lat_deg.to_radians();
    let (sin_lat, cos_lat) = lat.sin_cos();
    let (sin_lon, cos_lon) = lon.sin_cos();
    // Radius of curvature in the prime vertical.
    let n = WGS84_A / (1.0 - WGS84_E2 * sin_lat * sin_lat).sqrt();
    [
        (n + h) * cos_lat * cos_lon,
        (n + h) * cos_lat * sin_lon,
        (n * (1.0 - WGS84_E2) + h) * sin_lat,
    ]
}

/// Coordinate reference system.
///
/// Stores projection parameters and provides forward/inverse transforms
/// between WGS84 geographic coordinates and projected coordinates.
#[derive(Debug, Clone, PartialEq)]
pub enum Crs {
    /// WGS84 geographic (EPSG:4326 / CRS84). Coordinates are (lon, lat) in degrees.
    Wgs84,
    /// Transverse Mercator (e.g., EPSG:3067 TM35FIN).
    TransverseMercator {
        lat0: f64,    // latitude of natural origin (radians)
        lon0: f64,    // central meridian (radians)
        k0: f64,      // scale factor at natural origin
        false_e: f64, // false easting (meters)
        false_n: f64, // false northing (meters)
    },
    /// Lambert Azimuthal Equal Area (e.g., EPSG:3035).
    LambertAzimuthalEqualArea {
        lat0: f64,    // latitude of natural origin (radians)
        lon0: f64,    // longitude of natural origin (radians)
        false_e: f64, // false easting (meters)
        false_n: f64, // false northing (meters)
    },
    /// Lambert Conformal Conic with 2 standard parallels.
    LambertConformalConic {
        lat1: f64,    // first standard parallel (radians)
        lat2: f64,    // second standard parallel (radians)
        lat0: f64,    // latitude of false origin (radians)
        lon0: f64,    // longitude of false origin (radians)
        false_e: f64, // false easting (meters)
        false_n: f64, // false northing (meters)
    },
    /// Polar Stereographic (e.g., FMI ODIM radar composites).
    ///
    /// Oblique stereographic on WGS84 ellipsoid using the conformal sphere
    /// approach (Gauss conformal latitude). Parameters match PROJ +proj=stere.
    Stereographic {
        lat0: f64,    // latitude of natural origin (radians)
        lon0: f64,    // central meridian (radians)
        k0: f64,      // scale factor at natural origin
        false_e: f64, // false easting (meters)
        false_n: f64, // false northing (meters)
    },
    /// Rotated latitude-longitude grid (e.g., HIRLAM NWP models).
    ///
    /// Grid coordinates are in a rotated coordinate system where the south pole
    /// is moved to (south_pole_lon, south_pole_lat). Used by FMI's querydata format
    /// for regional NWP grids.
    RotatedLatLon {
        south_pole_lat: f64, // latitude of the rotated south pole (radians)
        south_pole_lon: f64, // longitude of the rotated south pole (radians)
    },
}

/// Map an engine's internal native-CRS label (as stored in
/// `RasterInfo.native_crs`) to a canonical OGC CRS URI, or `None` when the
/// label has no stable URI — engines tag projected grids they can't pin to a
/// specific EPSG code with generic names ("TM", "LAEA", "projected",
/// "rotated_ll"). Used for the OGC API `storageCrs` field, where a wrong URI is
/// worse than an absent one, so this deliberately never falls back to CRS84.
///
/// `"EPSG:4326"` (lat-first) and `"CRS:84"` (lon-first) map to their distinct
/// URIs: emitting one for the other would invite a conformant client to swap
/// axes and transpose the image.
///
/// This is keyed on the engine's `native_crs` *label*: a CRS only gets a
/// `storageCrs` if some engine's `crs_label` emits the matching string. The
/// `"EPSG:4326"` and `"EPSG:3857"` arms are forward-looking — no current engine
/// emits those labels (WGS84 grids are tagged `"CRS:84"`, and there is no Web
/// Mercator `Crs` variant) — so a new engine for one of those must emit the
/// label here for the URI to apply.
pub fn native_crs_uri(label: &str) -> Option<&'static str> {
    match label {
        "CRS:84" => Some("http://www.opengis.net/def/crs/OGC/1.3/CRS84"),
        "EPSG:4326" => Some("http://www.opengis.net/def/crs/EPSG/0/4326"),
        "EPSG:3857" => Some("http://www.opengis.net/def/crs/EPSG/0/3857"),
        "EPSG:3067" => Some("http://www.opengis.net/def/crs/EPSG/0/3067"),
        "EPSG:3035" => Some("http://www.opengis.net/def/crs/EPSG/0/3035"),
        _ => None,
    }
}

/// True when `label` denotes a CRS:84 lon/lat degree grid — the case where a
/// lon-first `extent.spatial.grid` (axis 0 = longitude, axis 1 = latitude) is
/// an exact description of the source.
///
/// Deliberately matches **only** `"CRS:84"`, not `"EPSG:4326"`: we always emit
/// the grid axes lon-first to match the CRS84 spatial extent, whereas EPSG:4326
/// is lat-first, so emitting our lon-first grid for an EPSG:4326-labelled source
/// would violate the OGC API Common Part 2 axis-order rule. Projected grids
/// (EPSG:3067/3035/3857, TM/LAEA/LCC/stere) and rotated lat/lon are excluded
/// too — their cells aren't degree-regular — so callers omit the grid rather
/// than imply a regularity (or axis order) that doesn't hold.
pub fn is_crs84_grid(label: &str) -> bool {
    label == "CRS:84"
}

/// Positive longitude and latitude spans (degrees) of a CRS84 bbox
/// `[west, south, east, north]`. Handles an anti-meridian crossing where
/// `east < west` (e.g. a STAC bbox like `[170.0, …, -170.0, …]`, a 20°-wide
/// box, not a 340°-wide one). Used to derive spatial grid resolution, which
/// must be positive per the OGC API Common Part 2 schema.
pub fn crs84_bbox_spans(bbox: [f64; 4]) -> (f64, f64) {
    let [w, s, e, n] = bbox;
    let lon_span = if e >= w { e - w } else { e - w + 360.0 };
    (lon_span, (n - s).abs())
}

/// Projection definition for a projected CRS the server accepts as a WMS / OGC
/// API Maps **output** CRS, keyed by its EPSG identifier.
///
/// Returns `None` for the geographic codes (`CRS:84` / `EPSG:4326`, which need
/// no projection) and for `EPSG:3857` (Web Mercator has its own dedicated
/// output path, [`crate::map_engine::OutputCrs::WebMercator`]). The parameters
/// match the authoritative EPSG definitions and the ones engine-geotiff already
/// derives from GeoTIFF GeoKeys:
/// - **EPSG:3067** (ETRS89 / TM35FIN) — Transverse Mercator, central meridian
///   27°E, k₀=0.9996, false easting 500 km.
/// - **EPSG:3035** (ETRS89-extended / LAEA Europe) — Lambert Azimuthal Equal
///   Area centred at 52°N 10°E, false easting 4 321 km, northing 3 210 km.
///
/// This is the single source of truth so api-wms and api-maps build identical
/// output projections (#160).
pub fn projected_output_crs(epsg: &str) -> Option<Crs> {
    match epsg {
        "EPSG:3067" => Some(Crs::TransverseMercator {
            lat0: 0.0,
            lon0: 27.0_f64.to_radians(),
            k0: 0.9996,
            false_e: 500_000.0,
            false_n: 0.0,
        }),
        "EPSG:3035" => Some(Crs::LambertAzimuthalEqualArea {
            lat0: 52.0_f64.to_radians(),
            lon0: 10.0_f64.to_radians(),
            false_e: 4_321_000.0,
            false_n: 3_210_000.0,
        }),
        _ => None,
    }
}

/// Edge samples per side used by [`wgs84_envelope`] / [`projected_envelope`].
/// 20 matches `GeoTransform::bbox`'s sampling — enough to pin the bow of a
/// continental TM/LAEA box to well under a pixel.
const ENVELOPE_EDGE_SAMPLES: usize = 20;

/// Accumulate the axis-aligned min/max of points produced by `map` along the
/// four edges of `bbox` (`[min_a, min_b, max_a, max_b]`). `map` returns `None`
/// for points that don't transform (e.g. outside a projection's valid domain);
/// those are skipped. Returns `None` if every sampled point failed.
fn edge_envelope(bbox: [f64; 4], map: impl Fn(f64, f64) -> Option<(f64, f64)>) -> Option<[f64; 4]> {
    let [min_a, min_b, max_a, max_b] = bbox;
    let mut min_x = f64::MAX;
    let mut max_x = f64::MIN;
    let mut min_y = f64::MAX;
    let mut max_y = f64::MIN;
    for i in 0..=ENVELOPE_EDGE_SAMPLES {
        let frac = i as f64 / ENVELOPE_EDGE_SAMPLES as f64;
        // Top + bottom edges (a varies, b pinned to each end).
        let a = min_a + frac * (max_a - min_a);
        for &b in &[min_b, max_b] {
            if let Some((x, y)) = map(a, b) {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
        // Left + right edges (b varies, a pinned to each end).
        let b = min_b + frac * (max_b - min_b);
        for &a in &[min_a, max_a] {
            if let Some((x, y)) = map(a, b) {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
    }
    // Both axes update together from the same `Some((x, y))`, so either bound
    // being un-touched means *no* point transformed — check both for symmetry /
    // defence rather than relying on x alone.
    if min_x > max_x || min_y > max_y {
        None
    } else {
        Some([min_x, min_y, max_x, max_y])
    }
}

/// WGS84 lon/lat envelope `[west, south, east, north]` of a projected bbox
/// `[min_e, min_n, max_e, max_n]` (in `crs`'s metres), found by inverse-
/// projecting points sampled along the four edges.
///
/// Projection curvature means the extreme lon/lat can fall in the middle of an
/// edge, not only at a corner, so this samples the edges rather than just the
/// corners. Used to derive the WGS84 read window for a projected-output render
/// (the engine still reads source pixels by lon/lat).
///
/// Returns `None` when **every** sampled point fails to inverse-project — i.e.
/// the projected bbox lies entirely outside the projection's valid domain. The
/// caller must surface that as a client error (HTTP 400), *not* fall back to a
/// global extent: a global read window on a planet-scale GRIB/COG source would
/// decode the whole dataset for one bogus request (a DoS vector). Unreachable
/// for any valid EPSG:3067/3035 bbox.
pub fn wgs84_envelope(crs: &Crs, bbox: [f64; 4]) -> Option<[f64; 4]> {
    edge_envelope(bbox, |x, y| crs.inverse(x, y))
}

/// Projected envelope `[min_e, min_n, max_e, max_n]` (in `crs`'s metres) of a
/// WGS84 bbox `[west, south, east, north]`, found by forward-projecting points
/// sampled along the four edges.
///
/// The inverse of [`wgs84_envelope`]: used by OGC API Maps, where the request
/// `bbox` is in CRS:84 but the output `crs` is projected — the projected map
/// frame must cover the requested geographic box. `Crs::forward` is total, so
/// this always returns `Some`; the `unwrap_or` is a defensive identity fallback.
pub fn projected_envelope(crs: &Crs, bbox: [f64; 4]) -> [f64; 4] {
    edge_envelope(bbox, |lon, lat| Some(crs.forward(lon, lat))).unwrap_or(bbox)
}

impl Crs {
    /// Forward-transform WGS84 (lon_deg, lat_deg) to projected (easting, northing).
    /// For Wgs84, returns (lon, lat) unchanged.
    /// For RotatedLatLon, returns rotated (lon, lat) in degrees.
    pub fn forward(&self, lon_deg: f64, lat_deg: f64) -> (f64, f64) {
        match self {
            Crs::Wgs84 => (lon_deg, lat_deg),
            Crs::TransverseMercator {
                lat0,
                lon0,
                k0,
                false_e,
                false_n,
            } => tm_forward(
                lat_deg.to_radians(),
                lon_deg.to_radians(),
                *lat0,
                *lon0,
                *k0,
                *false_e,
                *false_n,
            ),
            Crs::LambertAzimuthalEqualArea {
                lat0,
                lon0,
                false_e,
                false_n,
            } => laea_forward(
                lat_deg.to_radians(),
                lon_deg.to_radians(),
                *lat0,
                *lon0,
                *false_e,
                *false_n,
            ),
            Crs::LambertConformalConic {
                lat1,
                lat2,
                lat0,
                lon0,
                false_e,
                false_n,
            } => lcc_forward(
                lat_deg.to_radians(),
                lon_deg.to_radians(),
                *lat1,
                *lat2,
                *lat0,
                *lon0,
                *false_e,
                *false_n,
            ),
            Crs::Stereographic {
                lat0,
                lon0,
                k0,
                false_e,
                false_n,
            } => stere_forward(
                lat_deg.to_radians(),
                lon_deg.to_radians(),
                *lat0,
                *lon0,
                *k0,
                *false_e,
                *false_n,
            ),
            Crs::RotatedLatLon {
                south_pole_lat,
                south_pole_lon,
            } => rotlatlon_forward(
                lat_deg.to_radians(),
                lon_deg.to_radians(),
                *south_pole_lat,
                *south_pole_lon,
            ),
        }
    }

    /// Inverse-transform projected (easting, northing) to WGS84 (lon_deg, lat_deg).
    /// For Wgs84, returns (x, y) unchanged.
    /// For RotatedLatLon, input is rotated (lon, lat) in degrees.
    /// Returns `None` if the projection math produces NaN/Inf (e.g., near poles).
    pub fn inverse(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let result = match self {
            Crs::Wgs84 => (x, y),
            Crs::TransverseMercator {
                lat0,
                lon0,
                k0,
                false_e,
                false_n,
            } => {
                let (lat, lon) = tm_inverse(x, y, *lat0, *lon0, *k0, *false_e, *false_n);
                (lon.to_degrees(), lat.to_degrees())
            }
            Crs::LambertAzimuthalEqualArea {
                lat0,
                lon0,
                false_e,
                false_n,
            } => {
                let (lat, lon) = laea_inverse(x, y, *lat0, *lon0, *false_e, *false_n);
                (lon.to_degrees(), lat.to_degrees())
            }
            Crs::LambertConformalConic {
                lat1,
                lat2,
                lat0,
                lon0,
                false_e,
                false_n,
            } => {
                let (lat, lon) = lcc_inverse(x, y, *lat1, *lat2, *lat0, *lon0, *false_e, *false_n);
                (lon.to_degrees(), lat.to_degrees())
            }
            Crs::Stereographic {
                lat0,
                lon0,
                k0,
                false_e,
                false_n,
            } => {
                let (lat, lon) = stere_inverse(x, y, *lat0, *lon0, *k0, *false_e, *false_n);
                (lon.to_degrees(), lat.to_degrees())
            }
            Crs::RotatedLatLon {
                south_pole_lat,
                south_pole_lon,
            } => rotlatlon_inverse(
                x.to_radians(),
                y.to_radians(),
                *south_pole_lat,
                *south_pole_lon,
            ),
        };
        if result.0.is_finite() && result.1.is_finite() {
            Some(result)
        } else {
            None
        }
    }
}

/// Affine transform mapping pixel coordinates to projected (or geographic) coordinates.
///
/// Supports both axis-aligned rasters (from ModelPixelScaleTag + ModelTiepointTag)
/// and general affine transforms (from ModelTransformationTag, tag 34264).
/// Rotated or skewed rasters are detected and rejected at parse time.
#[derive(Debug, Clone)]
pub struct GeoTransform {
    /// Origin X in source CRS (easting or longitude).
    pub origin_x: f64,
    /// Origin Y in source CRS (northing or latitude).
    pub origin_y: f64,
    pub pixel_width: f64,
    pub pixel_height: f64,
    pub width: u32,
    pub height: u32,
    /// Coordinate reference system of the raster.
    pub crs: Crs,
}

impl GeoTransform {
    /// Create from a 4x4 ModelTransformationTag matrix (row-major, 16 doubles).
    /// Rejects rotated/skewed rasters (non-zero off-diagonal terms).
    pub fn from_transformation_matrix(
        matrix: &[f64],
        width: u32,
        height: u32,
        crs: Crs,
    ) -> Result<Self, String> {
        if matrix.len() < 16 {
            return Err(format!(
                "ModelTransformationTag has {} values, expected 16",
                matrix.len()
            ));
        }

        // 4x4 row-major: [a b c d / e f g h / i j k l / m n o p]
        // For 2D raster→model: x' = a*col + b*row + d, y' = e*col + f*row + h
        // Axis-aligned means b ≈ 0 and e ≈ 0 (no rotation/skew)
        let a = matrix[0]; // pixel_width (x scale)
        let b = matrix[1]; // rotation term (should be ~0)
        let d = matrix[3]; // origin_x
        let e = matrix[4]; // rotation term (should be ~0)
        let f = matrix[5]; // -pixel_height (y scale, typically negative)
        let h = matrix[7]; // origin_y

        let rotation_threshold = 1e-10;
        if b.abs() > rotation_threshold || e.abs() > rotation_threshold {
            return Err(format!(
                "Rotated/skewed raster detected (off-diagonal terms: b={b:.6e}, e={e:.6e}). \
                 This server requires axis-aligned pixels. Use gdalwarp to remove rotation: \
                 gdalwarp -r bilinear input.tif output.tif"
            ));
        }

        if a.abs() < 1e-15 || f.abs() < 1e-15 {
            return Err(format!(
                "Degenerate affine transform: pixel_width={a}, pixel_height={f}"
            ));
        }

        Ok(GeoTransform {
            origin_x: d,
            origin_y: h,
            pixel_width: a,
            pixel_height: -f, // pixel_height stored as positive; f is typically negative
            width,
            height,
            crs,
        })
    }
}

impl GeoTransform {
    /// Convert WGS84 (lon, lat) to *fractional, unclamped* pixel coordinates.
    ///
    /// Returns `(col, row)` as floats **before** flooring or bounds-checking;
    /// either value may be negative or exceed the raster size when the point
    /// lies outside the raster. This exposes the raw projection primitive so
    /// callers that need many nearby points can sample the (expensive) CRS
    /// forward transform on a coarse grid and bilinearly interpolate the
    /// result — see [`crate::resample::ProjectionGrid`].
    pub fn world_to_pixel_f64(&self, lon: f64, lat: f64) -> (f64, f64) {
        let (x, y) = self.crs.forward(lon, lat);
        let col = (x - self.origin_x) / self.pixel_width;
        let row = (self.origin_y - y) / self.pixel_height;
        (col, row)
    }

    /// Convert WGS84 (lon, lat) to pixel coordinate (col, row).
    /// Handles CRS transformation internally.
    /// Returns None if the coordinate is outside the raster bounds.
    pub fn world_to_pixel(&self, lon: f64, lat: f64) -> Option<(u32, u32)> {
        // Use floor() to match bbox_to_pixels() rounding — `as i64` truncates
        // toward zero which maps slightly-negative values to 0 (wrong pixel).
        let (col_f, row_f) = self.world_to_pixel_f64(lon, lat);
        let col_f = col_f.floor();
        let row_f = row_f.floor();

        if col_f >= 0.0 && col_f < self.width as f64 && row_f >= 0.0 && row_f < self.height as f64 {
            Some((col_f as u32, row_f as u32))
        } else {
            None
        }
    }

    /// Compute the bounding box in WGS84 [west, south, east, north].
    /// For projected CRS, samples points along all edges (not just corners)
    /// to handle projection distortion. Skips points that fail to reproject.
    pub fn bbox(&self) -> [f64; 4] {
        let x_min = self.origin_x;
        let x_max = self.origin_x + self.width as f64 * self.pixel_width;
        let y_max = self.origin_y;
        let y_min = self.origin_y - self.height as f64 * self.pixel_height;

        let mut min_lon = f64::MAX;
        let mut max_lon = f64::MIN;
        let mut min_lat = f64::MAX;
        let mut max_lat = f64::MIN;

        // Sample N points along each edge
        let n = 20;
        for i in 0..=n {
            let frac = i as f64 / n as f64;
            // Top and bottom edges
            let x = x_min + frac * (x_max - x_min);
            for &y in &[y_max, y_min] {
                if let Some((lon, lat)) = self.crs.inverse(x, y) {
                    min_lon = min_lon.min(lon);
                    max_lon = max_lon.max(lon);
                    min_lat = min_lat.min(lat);
                    max_lat = max_lat.max(lat);
                }
            }
            // Left and right edges
            let y = y_min + frac * (y_max - y_min);
            for &x in &[x_min, x_max] {
                if let Some((lon, lat)) = self.crs.inverse(x, y) {
                    min_lon = min_lon.min(lon);
                    max_lon = max_lon.max(lon);
                    min_lat = min_lat.min(lat);
                    max_lat = max_lat.max(lat);
                }
            }
        }

        [min_lon, min_lat, max_lon, max_lat]
    }

    /// Convert pixel coordinate to WGS84 (lon, lat) at pixel center.
    /// Returns (0, 0) if reprojection fails (degenerate edge case).
    pub fn pixel_to_world(&self, col: u32, row: u32) -> (f64, f64) {
        let x = self.origin_x + (col as f64 + 0.5) * self.pixel_width;
        let y = self.origin_y - (row as f64 + 0.5) * self.pixel_height;
        self.crs.inverse(x, y).unwrap_or((0.0, 0.0))
    }

    /// Convert a WGS84 bbox [west, south, east, north] to pixel range.
    /// Transforms all four corners to the source CRS, takes the envelope, then maps to pixels.
    /// Returns (col_start, row_start, col_end, row_end) clamped to raster bounds. Exclusive end.
    pub fn bbox_to_pixels(
        &self,
        west: f64,
        south: f64,
        east: f64,
        north: f64,
    ) -> Option<(u32, u32, u32, u32)> {
        // Transform bbox to source CRS by sampling points along all 4 edges
        // AND interior grid. For non-linear projections (TM, LAEA, LCC), bbox
        // edges project as curves — edge midpoints can extend beyond the
        // corner envelope. TM35FIN at high latitudes needs dense sampling.
        let mut min_x = f64::MAX;
        let mut max_x = f64::MIN;
        let mut min_y = f64::MAX;
        let mut max_y = f64::MIN;

        let n = 20; // samples per edge (matching bbox() inverse sampling)
        for i in 0..=n {
            let frac = i as f64 / n as f64;
            let lon = west + frac * (east - west);
            let lat = south + frac * (north - south);

            // South and north edges
            for &edge_lat in &[south, north] {
                let (x, y) = self.crs.forward(lon, edge_lat);
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
            // West and east edges
            for &edge_lon in &[west, east] {
                let (x, y) = self.crs.forward(edge_lon, lat);
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }

        let col_start = ((min_x - self.origin_x) / self.pixel_width)
            .floor()
            .max(0.0) as u32;
        let col_end = ((max_x - self.origin_x) / self.pixel_width)
            .ceil()
            .min(self.width as f64) as u32;
        let row_start = ((self.origin_y - max_y) / self.pixel_height)
            .floor()
            .max(0.0) as u32;
        let row_end = ((self.origin_y - min_y) / self.pixel_height)
            .ceil()
            .min(self.height as f64) as u32;

        if col_start >= col_end || row_start >= row_end {
            return None;
        }

        Some((col_start, row_start, col_end, row_end))
    }
}

// ============================================================================
// Transverse Mercator — used by EPSG:3067 (TM35FIN) and UTM zones
// Reference: Snyder, USGS PP 1395, equations 8-9 through 8-15
// ============================================================================

fn tm_forward(
    lat: f64,
    lon: f64,
    lat0: f64,
    lon0: f64,
    k0: f64,
    false_e: f64,
    false_n: f64,
) -> (f64, f64) {
    let e2 = WGS84_E2;
    let ep2 = e2 / (1.0 - e2); // e'^2

    let dl = lon - lon0;
    let cos_lat = lat.cos();
    let sin_lat = lat.sin();
    let tan_lat = lat.tan();

    let n_val = WGS84_A / (1.0 - e2 * sin_lat * sin_lat).sqrt();
    let t = tan_lat;
    let t2 = t * t;
    let c = ep2 * cos_lat * cos_lat;
    let a_coeff = dl * cos_lat;
    let a2 = a_coeff * a_coeff;

    let m = meridian_arc(lat);
    let m0 = meridian_arc(lat0);

    let x = k0
        * n_val
        * (a_coeff
            + (1.0 - t2 + c) * a2 * a_coeff / 6.0
            + (5.0 - 18.0 * t2 + t2 * t2 + 72.0 * c - 58.0 * ep2) * a2 * a2 * a_coeff / 120.0);

    let y = k0
        * (m - m0
            + n_val
                * tan_lat
                * (a2 / 2.0
                    + (5.0 - t2 + 9.0 * c + 4.0 * c * c) * a2 * a2 / 24.0
                    + (61.0 - 58.0 * t2 + t2 * t2 + 600.0 * c - 330.0 * ep2) * a2 * a2 * a2
                        / 720.0));

    (false_e + x, false_n + y)
}

fn tm_inverse(
    x: f64,
    y: f64,
    lat0: f64,
    lon0: f64,
    k0: f64,
    false_e: f64,
    false_n: f64,
) -> (f64, f64) {
    // Newton iteration approach — works at any distance from central meridian.
    // Start with the footpoint latitude, then iterate to find (lat, lon).
    let e2 = WGS84_E2;
    let ep2 = e2 / (1.0 - e2);
    let e1 = (1.0 - (1.0 - e2).sqrt()) / (1.0 + (1.0 - e2).sqrt());

    let m0 = meridian_arc(lat0);
    let m = m0 + (y - false_n) / k0;

    // Footpoint latitude from meridian arc
    let mu = m / (WGS84_A * (1.0 - e2 / 4.0 - 3.0 * e2 * e2 / 64.0 - 5.0 * e2 * e2 * e2 / 256.0));
    let lat1 = mu
        + (3.0 * e1 / 2.0 - 27.0 * e1 * e1 * e1 / 32.0) * (2.0 * mu).sin()
        + (21.0 * e1 * e1 / 16.0 - 55.0 * e1 * e1 * e1 * e1 / 32.0) * (4.0 * mu).sin()
        + (151.0 * e1 * e1 * e1 / 96.0) * (6.0 * mu).sin()
        + (1097.0 * e1 * e1 * e1 * e1 / 512.0) * (8.0 * mu).sin();

    // Use series for initial guess, then refine with Newton iteration
    let sin_lat1 = lat1.sin();
    let cos_lat1 = lat1.cos();
    let tan_lat1 = lat1.tan();
    let n1 = WGS84_A / (1.0 - e2 * sin_lat1 * sin_lat1).sqrt();
    let r1 = WGS84_A * (1.0 - e2) / (1.0 - e2 * sin_lat1 * sin_lat1).powf(1.5);
    let t12 = tan_lat1 * tan_lat1;
    let c1 = ep2 * cos_lat1 * cos_lat1;
    let d = (x - false_e) / (n1 * k0);
    let d2 = d * d;

    // Series initial guess
    let mut lat = lat1
        - (n1 * tan_lat1 / r1)
            * (d2 / 2.0
                - (5.0 + 3.0 * t12 + 10.0 * c1 - 4.0 * c1 * c1 - 9.0 * ep2) * d2 * d2 / 24.0
                + (61.0 + 90.0 * t12 + 298.0 * c1 + 45.0 * t12 * t12
                    - 252.0 * ep2
                    - 3.0 * c1 * c1)
                    * d2
                    * d2
                    * d2
                    / 720.0);

    let mut lon = lon0
        + (d - (1.0 + 2.0 * t12 + c1) * d2 * d / 6.0
            + (5.0 - 2.0 * c1 + 28.0 * t12 - 3.0 * c1 * c1 + 8.0 * ep2 + 24.0 * t12 * t12)
                * d2
                * d2
                * d
                / 120.0)
            / cos_lat1;

    // Newton refinement: iterate forward(lat,lon) towards target (x,y)
    for _ in 0..20 {
        let (fx, fy) = tm_forward(lat, lon, lat0, lon0, k0, false_e, false_n);
        let ex = x - fx; // easting residual
        let ey = y - fy; // northing residual
        if ex.abs() < 0.001 && ey.abs() < 0.001 {
            break;
        }
        // Numerical Jacobian: d(easting,northing)/d(lat,lon)
        let h = 1e-8;
        let (fx_dlat, fy_dlat) = tm_forward(lat + h, lon, lat0, lon0, k0, false_e, false_n);
        let (fx_dlon, fy_dlon) = tm_forward(lat, lon + h, lat0, lon0, k0, false_e, false_n);
        // J = [[de/dlat, de/dlon], [dn/dlat, dn/dlon]]
        let de_dlat = (fx_dlat - fx) / h;
        let de_dlon = (fx_dlon - fx) / h;
        let dn_dlat = (fy_dlat - fy) / h;
        let dn_dlon = (fy_dlon - fy) / h;
        let det = de_dlat * dn_dlon - de_dlon * dn_dlat;
        if det.abs() < 1e-30 {
            break;
        }
        // J^-1 * [ex, ey]
        let dlat = (dn_dlon * ex - de_dlon * ey) / det;
        let dlon = (-dn_dlat * ex + de_dlat * ey) / det;
        lat += dlat;
        lon += dlon;
    }

    (lat, lon)
}

/// Meridian arc from equator to latitude (Helmert series).
fn meridian_arc(lat: f64) -> f64 {
    let e2 = WGS84_E2;
    let e4 = e2 * e2;
    let e6 = e4 * e2;
    WGS84_A
        * ((1.0 - e2 / 4.0 - 3.0 * e4 / 64.0 - 5.0 * e6 / 256.0) * lat
            - (3.0 * e2 / 8.0 + 3.0 * e4 / 32.0 + 45.0 * e6 / 1024.0) * (2.0 * lat).sin()
            + (15.0 * e4 / 256.0 + 45.0 * e6 / 1024.0) * (4.0 * lat).sin()
            - (35.0 * e6 / 3072.0) * (6.0 * lat).sin())
}

// ============================================================================
// Lambert Azimuthal Equal Area — used by EPSG:3035 (ETRS89-LAEA)
// Reference: Snyder, "Map Projections: A Working Manual", USGS PP 1395, p.187
// ============================================================================

fn laea_forward(
    lat: f64,
    lon: f64,
    lat0: f64,
    lon0: f64,
    false_e: f64,
    false_n: f64,
) -> (f64, f64) {
    let e2 = WGS84_E2;
    let e = e2.sqrt();

    let q_val = q_authalic(lat, e);
    let q0 = q_authalic(lat0, e);
    let qp = q_authalic(PI / 2.0, e);

    let beta = (q_val / qp).clamp(-1.0, 1.0).asin();
    let beta0 = (q0 / qp).clamp(-1.0, 1.0).asin();

    let rq = WGS84_A * (qp / 2.0).sqrt();
    let m0 = lat0.cos() / (1.0 - e2 * lat0.sin().powi(2)).sqrt();
    let dd = WGS84_A * m0 / (rq * beta0.cos());

    let dl = lon - lon0;

    // Snyder eq. 24-2, 24-3 (oblique)
    let b = rq
        * (2.0 / (1.0 + beta0.sin() * beta.sin() + beta0.cos() * beta.cos() * dl.cos()))
            .max(0.0)
            .sqrt();

    let x = false_e + b * dd * beta.cos() * dl.sin();
    let y = false_n + (b / dd) * (beta0.cos() * beta.sin() - beta0.sin() * beta.cos() * dl.cos());

    (x, y)
}

fn laea_inverse(x: f64, y: f64, lat0: f64, lon0: f64, false_e: f64, false_n: f64) -> (f64, f64) {
    // Newton iteration from projection center — robust at any distance.
    let mut lat = lat0;
    let mut lon = lon0;

    for _ in 0..20 {
        let (fx, fy) = laea_forward(lat, lon, lat0, lon0, false_e, false_n);
        let ex = x - fx;
        let ey = y - fy;
        if ex.abs() < 0.01 && ey.abs() < 0.01 {
            break;
        }
        let h = 1e-8;
        let (fx_dlat, fy_dlat) = laea_forward(lat + h, lon, lat0, lon0, false_e, false_n);
        let (fx_dlon, fy_dlon) = laea_forward(lat, lon + h, lat0, lon0, false_e, false_n);
        let de_dlat = (fx_dlat - fx) / h;
        let de_dlon = (fx_dlon - fx) / h;
        let dn_dlat = (fy_dlat - fy) / h;
        let dn_dlon = (fy_dlon - fy) / h;
        let det = de_dlat * dn_dlon - de_dlon * dn_dlat;
        if det.abs() < 1e-30 {
            break;
        }
        lat += (dn_dlon * ex - de_dlon * ey) / det;
        lon += (-dn_dlat * ex + de_dlat * ey) / det;
    }

    (lat, lon)
}

fn q_authalic(lat: f64, e: f64) -> f64 {
    let sin_lat = lat.sin();
    let e_sin = e * sin_lat;
    (1.0 - e * e)
        * (sin_lat / (1.0 - e_sin * e_sin)
            - (1.0 / (2.0 * e)) * ((1.0 - e_sin) / (1.0 + e_sin)).ln())
}

#[allow(dead_code)]
fn authalic_inverse(q: f64, e: f64) -> f64 {
    let e2 = e * e;
    let e4 = e2 * e2;
    let e6 = e4 * e2;
    let qp = q_authalic(PI / 2.0, e);

    // Initial approximation using series (Snyder eq. 3-18)
    let beta = (q / qp).clamp(-1.0, 1.0).asin();
    let mut lat = beta
        + (e2 / 3.0 + 31.0 * e4 / 180.0 + 517.0 * e6 / 5040.0) * (2.0 * beta).sin()
        + (23.0 * e4 / 360.0 + 251.0 * e6 / 3780.0) * (4.0 * beta).sin()
        + (761.0 * e6 / 45360.0) * (6.0 * beta).sin();

    // Newton iteration
    for _ in 0..6 {
        let sin_lat = lat.sin();
        let cos_lat = lat.cos();
        if cos_lat.abs() < 1e-14 {
            break;
        }
        let q_lat = q_authalic(lat, e);
        let delta = q - q_lat;
        let denom = (1.0 - e2 * sin_lat * sin_lat).powi(2) / (2.0 * (1.0 - e2) * cos_lat);
        lat += delta * denom;
        if delta.abs() < 1e-12 {
            break;
        }
    }
    lat
}

// ============================================================================
// Lambert Conformal Conic (2SP) — used by MET Norway radar
// Reference: Snyder, USGS PP 1395, p.107
// ============================================================================

#[allow(clippy::too_many_arguments)]
fn lcc_forward(
    lat: f64,
    lon: f64,
    lat1: f64,
    lat2: f64,
    lat0: f64,
    lon0: f64,
    false_e: f64,
    false_n: f64,
) -> (f64, f64) {
    let e2 = WGS84_E2;
    let e = e2.sqrt();

    let m1 = lcc_m(lat1, e2);
    let m2 = lcc_m(lat2, e2);
    let t0 = lcc_t(lat0, e);
    let t1 = lcc_t(lat1, e);
    let t2 = lcc_t(lat2, e);
    let t = lcc_t(lat, e);

    let n = (m1.ln() - m2.ln()) / (t1.ln() - t2.ln());
    let f_val = m1 / (n * t1.powf(n));
    let rho0 = WGS84_A * f_val * t0.powf(n);
    let rho = WGS84_A * f_val * t.powf(n);
    let theta = n * (lon - lon0);

    let x = false_e + rho * theta.sin();
    let y = false_n + rho0 - rho * theta.cos();

    (x, y)
}

#[allow(clippy::too_many_arguments)]
fn lcc_inverse(
    x: f64,
    y: f64,
    lat1: f64,
    lat2: f64,
    lat0: f64,
    lon0: f64,
    false_e: f64,
    false_n: f64,
) -> (f64, f64) {
    let e2 = WGS84_E2;
    let e = e2.sqrt();

    let m1 = lcc_m(lat1, e2);
    let m2 = lcc_m(lat2, e2);
    let t0 = lcc_t(lat0, e);
    let t1 = lcc_t(lat1, e);
    let t2 = lcc_t(lat2, e);

    let n = (m1.ln() - m2.ln()) / (t1.ln() - t2.ln());
    let f_val = m1 / (n * t1.powf(n));
    let rho0 = WGS84_A * f_val * t0.powf(n);

    let xp = x - false_e;
    let yp = rho0 - (y - false_n);

    let rho = (xp * xp + yp * yp).sqrt().copysign(n);
    let t = (rho / (WGS84_A * f_val)).powf(1.0 / n);
    let theta = xp.atan2(yp);

    let lon = theta / n + lon0;

    // Iterative inverse
    let mut lat = PI / 2.0 - 2.0 * t.atan();
    for _ in 0..10 {
        let e_sin = e * lat.sin();
        let new_lat = PI / 2.0 - 2.0 * (t * ((1.0 - e_sin) / (1.0 + e_sin)).powf(e / 2.0)).atan();
        if (new_lat - lat).abs() < 1e-12 {
            break;
        }
        lat = new_lat;
    }

    (lat, lon)
}

fn lcc_m(lat: f64, e2: f64) -> f64 {
    lat.cos() / (1.0 - e2 * lat.sin().powi(2)).sqrt()
}

fn lcc_t(lat: f64, e: f64) -> f64 {
    let e_sin = e * lat.sin();
    (PI / 4.0 - lat / 2.0).tan() / ((1.0 - e_sin) / (1.0 + e_sin)).powf(e / 2.0)
}

// ============================================================================
// Oblique Stereographic — used by FMI/DMI ODIM radar composites
// Reference: EPSG guidance note 7-2, method 9809 (Oblique Stereographic)
// Uses conformal sphere approach (Gauss conformal latitude)
// ============================================================================

/// Oblique stereographic projection on the conformal sphere, EPSG 9809.
///
/// The previous implementation had an `n = sqrt(1 + e²·cos⁴(lat0)/(1-e²))`
/// factor borrowed from Lambert Conformal Conic (where it's the cone
/// constant) and multiplied it into the longitude difference. That
/// over-scaled projected coordinates by ~50% at latitudes around 56°,
/// invisible to roundtrip tests (forward & inverse used the same
/// wrong formula) but immediately visible when rendering ODIM radar
/// composites — output pixels missed the source grid by hundreds of
/// kilometres.
///
/// EPSG 9809 ("Oblique Stereographic") uses no LCC cone constant:
///   χ(φ)   — conformal latitude (Snyder eq. 3-1)
///   R_c    — conformal sphere radius at origin: √(M(φ₀) · N(φ₀))
///   k(P)   — scale factor at the point P
///   x = false_e + 2·R_c·k₀·cos(χ)·sin(λ-λ₀)/B
///   y = false_n + 2·R_c·k₀·(cos(χ₀)·sin(χ) - sin(χ₀)·cos(χ)·cos(λ-λ₀))/B
/// where B = 1 + sin(χ₀)·sin(χ) + cos(χ₀)·cos(χ)·cos(λ-λ₀).
///
/// Polar stereographic (lat₀ = ±90°) falls out of the same formula
/// without special-casing, so a single implementation handles ODIM's
/// polar (FMI/OPERA) and oblique (DMI) variants.
fn stere_forward(
    lat: f64,
    lon: f64,
    lat0: f64,
    lon0: f64,
    k0: f64,
    false_e: f64,
    false_n: f64,
) -> (f64, f64) {
    // Snyder, "Map Projections — A Working Manual" (USGS PP 1395,
    // 1987), §21 "Stereographic", ellipsoidal aspect. Matches
    // PROJ's `+proj=stere`. The earlier `(rn0·rm0).sqrt()` form
    // was the double-stereographic / EPSG 9809 radius (PROJ's
    // `+proj=sterea`), a different projection that diverges from
    // `+proj=stere` by ~300 m at 100 km from origin and would
    // misalign DMI/OPERA tiles even though forward+inverse
    // roundtripped self-consistently.
    //
    // Two ellipsoidal cases:
    //   - Oblique (|lat0| < π/2): Snyder eq. 21-26..30. Uses
    //     `m_c / cos(χ_c)` for the radius factor.
    //   - Polar (|lat0| = π/2): Snyder eq. 21-33. The oblique
    //     formula's `m_c` and `cos(χ_c)` both numerically vanish
    //     at the pole (their ratio happens to stay finite by IEEE
    //     754 rounding alone), so use the dedicated polar form
    //     `ρ = 2·a·k₀·t / D` with `D = √((1+e)^(1+e)·(1-e)^(1-e))`.
    let e2 = WGS84_E2;
    let e = e2.sqrt();
    let a = WGS84_A;

    // Polar branch — check before computing oblique-only terms
    // that would be ill-conditioned at the pole.
    const POLAR_THRESHOLD: f64 = 1e-10;
    if (lat0.abs() - std::f64::consts::FRAC_PI_2).abs() < POLAR_THRESHOLD {
        let north_polar = lat0 > 0.0;
        let sin_lat = lat.sin();
        let t = ((std::f64::consts::FRAC_PI_4 - if north_polar { lat / 2.0 } else { -lat / 2.0 })
            .tan())
            * ((1.0 + e * sin_lat * if north_polar { 1.0 } else { -1.0 })
                / (1.0 - e * sin_lat * if north_polar { 1.0 } else { -1.0 }))
            .powf(e / 2.0);
        let d = ((1.0 + e).powf(1.0 + e) * (1.0 - e).powf(1.0 - e)).sqrt();
        let rho = 2.0 * a * k0 * t / d;
        let dl = lon - lon0;
        let x = false_e + rho * dl.sin();
        let y = false_n
            + if north_polar {
                -rho * dl.cos()
            } else {
                rho * dl.cos()
            };
        return (x, y);
    }

    // Oblique branch
    let sin_lat0 = lat0.sin();
    let cos_lat0 = lat0.cos();
    let m_c = cos_lat0 / (1.0 - e2 * sin_lat0 * sin_lat0).sqrt();

    let chi0 = conformal_latitude(lat0, e);
    let chi = conformal_latitude(lat, e);

    let dl = lon - lon0;
    let (sin_chi0, cos_chi0) = chi0.sin_cos();
    let (sin_chi, cos_chi) = chi.sin_cos();
    let cos_dl = dl.cos();

    let b = 1.0 + sin_chi0 * sin_chi + cos_chi0 * cos_chi * cos_dl;
    // A = 2·a·k₀·m_c / (cos(χ₀) · b)
    let factor = 2.0 * a * k0 * m_c / (cos_chi0 * b);

    let x = false_e + factor * cos_chi * dl.sin();
    let y = false_n + factor * (cos_chi0 * sin_chi - sin_chi0 * cos_chi * cos_dl);

    (x, y)
}

/// Conformal latitude χ(φ) = 2·atan(tan(π/4 + φ/2) · ((1-e·sinφ)/(1+e·sinφ))^(e/2)) - π/2.
/// At the equator and poles χ = φ. For WGS84 the maximum deviation
/// from geodetic latitude is ~12′ near 45°.
///
/// **Not called at φ = ±π/2.** `tan(π/4 + π/4) = tan(π/2)` is
/// algebraically ∞; IEEE 754 happens to evaluate it to ~1.63e16 so
/// `2·atan(...) - π/2 ≈ π/2` would still be numerically correct,
/// but the polar branches in `stere_forward` and `stere_inverse`
/// short-circuit before reaching this helper. That keeps the
/// hot path off the FP overflow corner case.
fn conformal_latitude(lat: f64, e: f64) -> f64 {
    let sin_lat = lat.sin();
    let ratio = (1.0 - e * sin_lat) / (1.0 + e * sin_lat);
    let inner = (std::f64::consts::FRAC_PI_4 + lat / 2.0).tan() * ratio.powf(e / 2.0);
    2.0 * inner.atan() - std::f64::consts::FRAC_PI_2
}

fn stere_inverse(
    x: f64,
    y: f64,
    lat0: f64,
    lon0: f64,
    k0: f64,
    false_e: f64,
    false_n: f64,
) -> (f64, f64) {
    // Polar branch — must mirror `stere_forward`'s polar branch.
    // The oblique Newton iteration breaks at the pole because the
    // Jacobian is singular there (longitude is degenerate when
    // ρ = 0; finite-difference derivatives at lat = ±π/2 step
    // outside the valid latitude range). Use Snyder eq. 21-35..37
    // (ellipsoidal polar inverse) instead.
    const POLAR_THRESHOLD: f64 = 1e-10;
    if (lat0.abs() - std::f64::consts::FRAC_PI_2).abs() < POLAR_THRESHOLD {
        let e2 = WGS84_E2;
        let e = e2.sqrt();
        let a = WGS84_A;
        let north_polar = lat0 > 0.0;

        let dx = x - false_e;
        let dy = y - false_n;
        let rho = (dx * dx + dy * dy).sqrt();

        // ρ = 0 means we're at the pole exactly; lon is degenerate,
        // return the central meridian.
        if rho < 1e-9 {
            return (lat0, lon0);
        }

        let d = ((1.0 + e).powf(1.0 + e) * (1.0 - e).powf(1.0 - e)).sqrt();
        let t = rho * d / (2.0 * a * k0);

        // Iterate Snyder eq. 21-37:
        //   φ_{i+1} = π/2 - 2·atan(t · ((1 - e·sin φ_i)/(1 + e·sin φ_i))^(e/2))
        // Initial guess uses the spherical inverse φ₀ = π/2 - 2·atan(t).
        let mut phi = std::f64::consts::FRAC_PI_2 - 2.0 * t.atan();
        for _ in 0..15 {
            let sin_phi = phi.sin();
            let new_phi = std::f64::consts::FRAC_PI_2
                - 2.0 * (t * ((1.0 - e * sin_phi) / (1.0 + e * sin_phi)).powf(e / 2.0)).atan();
            if (new_phi - phi).abs() < 1e-12 {
                phi = new_phi;
                break;
            }
            phi = new_phi;
        }

        let lat_out = if north_polar { phi } else { -phi };
        let lon_out = if north_polar {
            lon0 + dx.atan2(-dy)
        } else {
            lon0 + dx.atan2(dy)
        };
        return (lat_out, lon_out);
    }

    // Oblique branch — Newton iteration on the forward function.
    // Robust at any distance from center, locked to `stere_forward`
    // by construction so any future change to forward automatically
    // applies here.
    let mut lat = lat0;
    let mut lon = lon0;

    for _ in 0..20 {
        let (fx, fy) = stere_forward(lat, lon, lat0, lon0, k0, false_e, false_n);
        let ex = x - fx;
        let ey = y - fy;
        if ex.abs() < 0.01 && ey.abs() < 0.01 {
            break;
        }
        let h = 1e-8;
        let (fx_dlat, fy_dlat) = stere_forward(lat + h, lon, lat0, lon0, k0, false_e, false_n);
        let (fx_dlon, fy_dlon) = stere_forward(lat, lon + h, lat0, lon0, k0, false_e, false_n);
        let de_dlat = (fx_dlat - fx) / h;
        let de_dlon = (fx_dlon - fx) / h;
        let dn_dlat = (fy_dlat - fy) / h;
        let dn_dlon = (fy_dlon - fy) / h;
        let det = de_dlat * dn_dlon - de_dlon * dn_dlat;
        if det.abs() < 1e-30 {
            break;
        }
        lat += (dn_dlon * ex - de_dlon * ey) / det;
        lon += (-dn_dlat * ex + de_dlat * ey) / det;
    }

    (lat, lon)
}

// ============================================================================
// Rotated Latitude-Longitude — used by HIRLAM and other NWP models
// The south pole is moved to a new position, rotating the coordinate system.
// Input/output: geographic degrees. Projected coords: rotated degrees.
// ============================================================================

/// Forward: WGS84 (lat, lon) in radians → rotated (lon, lat) in degrees.
///
/// The rotation moves the south pole from (-90°, 0°) to (south_pole_lat, south_pole_lon).
/// Derived from two sequential rotations:
/// 1. Rotate by -sp_lon around Z axis (align south pole to prime meridian)
/// 2. Rotate by (90° + sp_lat) around Y axis (tilt south pole to correct latitude)
fn rotlatlon_forward(lat: f64, lon: f64, south_pole_lat: f64, south_pole_lon: f64) -> (f64, f64) {
    let sin_sp = south_pole_lat.sin();
    let cos_sp = south_pole_lat.cos();
    let dl = lon - south_pole_lon;

    // Rotated latitude
    let sin_lat_r = -sin_sp * lat.sin() - cos_sp * lat.cos() * dl.cos();
    let lat_r = sin_lat_r.clamp(-1.0, 1.0).asin();

    // Rotated longitude
    let x_r = -sin_sp * lat.cos() * dl.cos() + cos_sp * lat.sin();
    let y_r = lat.cos() * dl.sin();
    let lon_r = y_r.atan2(x_r);

    (lon_r.to_degrees(), lat_r.to_degrees())
}

/// Inverse: rotated (lon, lat) in radians → WGS84 (lon_deg, lat_deg).
fn rotlatlon_inverse(
    lon_r: f64,
    lat_r: f64,
    south_pole_lat: f64,
    south_pole_lon: f64,
) -> (f64, f64) {
    let sin_sp = south_pole_lat.sin();
    let cos_sp = south_pole_lat.cos();

    // Geographic latitude from rotated coordinates
    let sin_lat = cos_sp * lat_r.cos() * lon_r.cos() - sin_sp * lat_r.sin();
    let lat = sin_lat.clamp(-1.0, 1.0).asin();

    // Geographic longitude from rotated coordinates
    let x = -sin_sp * lat_r.cos() * lon_r.cos() - cos_sp * lat_r.sin();
    let y = lat_r.cos() * lon_r.sin();
    let dl = y.atan2(x);
    let lon = south_pole_lon + dl;

    // Normalize longitude to [-180, 180]
    let lon_deg = lon.to_degrees();
    let lon_norm = ((lon_deg + 180.0) % 360.0 + 360.0) % 360.0 - 180.0;

    (lon_norm, lat.to_degrees())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projected_output_crs_pins_epsg_parameters() {
        // EPSG:3067 (TM35FIN): on the central meridian (27°E) easting is exactly
        // the false easting (500 km), independent of latitude — this pins the
        // central meridian + false easting against the authoritative definition.
        let tm = projected_output_crs("EPSG:3067").expect("3067 defined");
        let (e, _n) = tm.forward(27.0, 60.0);
        assert!((e - 500_000.0).abs() < 1e-3, "3067 easting at λ0 = {e}");
        // EPSG:3035 (ETRS89-LAEA): at the projection centre (10°E, 52°N) the
        // result is exactly the false easting/northing (4321 km, 3210 km).
        let laea = projected_output_crs("EPSG:3035").expect("3035 defined");
        let (e, n) = laea.forward(10.0, 52.0);
        assert!((e - 4_321_000.0).abs() < 1e-3, "3035 FE = {e}");
        assert!((n - 3_210_000.0).abs() < 1e-3, "3035 FN = {n}");
        // Geographic and Web Mercator codes have no projected definition here.
        for code in ["CRS:84", "EPSG:4326", "EPSG:3857", "EPSG:9999", ""] {
            assert!(projected_output_crs(code).is_none(), "{code} must be None");
        }
    }

    #[test]
    fn envelope_roundtrip_contains_original_box() {
        // Forward a CRS:84 Finland box into EPSG:3067, then inverse-envelope
        // back: the WGS84 envelope must *contain* the original box (projection
        // bow can only widen it). This mirrors the api-maps read-window logic.
        let crs = projected_output_crs("EPSG:3067").unwrap();
        let original = [19.0, 59.0, 32.0, 70.0];
        let proj = projected_envelope(&crs, original);
        assert!(proj[0] < proj[2] && proj[1] < proj[3], "proj {proj:?}");
        let back = wgs84_envelope(&crs, proj).expect("in-domain bbox has an envelope");
        assert!(
            back[0] <= original[0] + 1e-6
                && back[1] <= original[1] + 1e-6
                && back[2] >= original[2] - 1e-6
                && back[3] >= original[3] - 1e-6,
            "envelope {back:?} must contain {original:?}"
        );
    }

    #[test]
    fn wgs84_envelope_of_projected_metres_is_degrees_not_metres() {
        // Regression for #251: a projected-metres bbox must come back as
        // plausible degrees, never the metres passed through unchanged.
        let crs = projected_output_crs("EPSG:3067").unwrap();
        // FMI's native TM35FIN extent.
        let env = wgs84_envelope(
            &crs,
            [
                -118331.366408,
                6335621.167014,
                875567.731907,
                7907751.537264,
            ],
        )
        .expect("in-domain bbox has an envelope");
        let [w, s, e, n] = env;
        assert!(
            (10.0..30.0).contains(&w) && (20.0..40.0).contains(&e),
            "lon {w}..{e}"
        );
        assert!(
            (55.0..72.0).contains(&s) && (60.0..72.0).contains(&n),
            "lat {s}..{n}"
        );
    }

    #[test]
    fn edge_envelope_is_none_when_every_sample_fails() {
        // Backs the wgs84_envelope DoS guard (#267 review): if no sampled point
        // transforms, the envelope is None so the caller returns HTTP 400 —
        // never a fabricated or global-extent box that would trigger a
        // whole-dataset read. (The projection inverses are Newton-based and
        // return finite values almost everywhere, so this contract is exercised
        // at the edge-sampling level rather than via a specific CRS.)
        assert_eq!(edge_envelope([0.0, 0.0, 1.0, 1.0], |_, _| None), None);
        // And Some, with correct min/max, when points do transform.
        assert_eq!(
            edge_envelope([0.0, 0.0, 2.0, 4.0], |a, b| Some((a, b))),
            Some([0.0, 0.0, 2.0, 4.0])
        );
    }

    #[test]
    fn wgs84_envelope_some_for_valid_projected_bbox() {
        let crs = projected_output_crs("EPSG:3035").unwrap();
        let proj = projected_envelope(&crs, [5.0, 48.0, 15.0, 55.0]);
        assert!(wgs84_envelope(&crs, proj).is_some());
    }

    #[test]
    fn native_crs_uri_maps_known_labels_and_omits_the_rest() {
        assert_eq!(
            native_crs_uri("CRS:84"),
            Some("http://www.opengis.net/def/crs/OGC/1.3/CRS84")
        );
        // EPSG:4326 (lat-first) maps to its own distinct URI, never CRS84.
        assert_eq!(
            native_crs_uri("EPSG:4326"),
            Some("http://www.opengis.net/def/crs/EPSG/0/4326")
        );
        assert_eq!(
            native_crs_uri("EPSG:3067"),
            Some("http://www.opengis.net/def/crs/EPSG/0/3067")
        );
        assert_eq!(
            native_crs_uri("EPSG:3035"),
            Some("http://www.opengis.net/def/crs/EPSG/0/3035")
        );
        // Generic engine labels for projections without a stable code: omitted.
        for label in ["TM", "LAEA", "LCC", "stere", "rotated_ll", "projected", ""] {
            assert_eq!(native_crs_uri(label), None, "label {label:?} must not map");
        }
    }

    #[test]
    fn is_crs84_grid_only_for_lonfirst_crs84() {
        assert!(is_crs84_grid("CRS:84"));
        // EPSG:4326 is lat-first: our lon-first grid axes wouldn't match it.
        assert!(!is_crs84_grid("EPSG:4326"));
        // Projected / rotated grids are not degree-regular.
        for label in [
            "EPSG:3857",
            "EPSG:3067",
            "EPSG:3035",
            "TM",
            "LAEA",
            "stere",
            "rotated_ll",
        ] {
            assert!(!is_crs84_grid(label), "{label} must not emit a CRS84 grid");
        }
    }

    #[test]
    fn crs84_bbox_spans_handles_normal_and_antimeridian() {
        // Normal bbox.
        let (lon, lat) = crs84_bbox_spans([10.0, 55.0, 30.0, 70.0]);
        assert!((lon - 20.0).abs() < 1e-9);
        assert!((lat - 15.0).abs() < 1e-9);
        // Anti-meridian crossing: east < west is a 20°-wide box, not 340°.
        let (lon, _) = crs84_bbox_spans([170.0, 60.0, -170.0, 70.0]);
        assert!(
            (lon - 20.0).abs() < 1e-9,
            "anti-meridian span should be 20, got {lon}"
        );
        assert!(lon > 0.0, "resolution span must be positive");
    }

    fn wgs84_transform() -> GeoTransform {
        GeoTransform {
            origin_x: 0.419,
            origin_y: 74.810,
            pixel_width: 0.01,
            pixel_height: 0.01,
            width: 3249,
            height: 1750,
            crs: Crs::Wgs84,
        }
    }

    #[test]
    fn wgs84_world_to_pixel_inside() {
        let gt = wgs84_transform();
        let (col, row) = gt.world_to_pixel(10.0, 65.0).unwrap();
        assert_eq!(col, ((10.0 - 0.419) / 0.01) as u32);
        assert_eq!(row, ((74.810 - 65.0) / 0.01) as u32);
    }

    #[test]
    fn wgs84_world_to_pixel_outside() {
        let gt = wgs84_transform();
        assert!(gt.world_to_pixel(-10.0, 65.0).is_none());
        assert!(gt.world_to_pixel(10.0, 80.0).is_none());
    }

    #[test]
    fn world_to_pixel_f64_matches_world_to_pixel() {
        let gt = wgs84_transform();
        // Inside the raster: world_to_pixel is floor() + bounds-check of the
        // fractional coordinates from world_to_pixel_f64.
        let (cf, rf) = gt.world_to_pixel_f64(10.0, 65.0);
        let (col, row) = gt.world_to_pixel(10.0, 65.0).unwrap();
        assert_eq!(col, cf.floor() as u32);
        assert_eq!(row, rf.floor() as u32);
        // Outside the raster: world_to_pixel_f64 still returns finite
        // (negative / out-of-range) coordinates rather than clamping.
        let (cf, _) = gt.world_to_pixel_f64(-10.0, 65.0);
        assert!(cf < 0.0 && cf.is_finite());
        assert!(gt.world_to_pixel(-10.0, 65.0).is_none());
    }

    #[test]
    fn wgs84_bbox_correct() {
        let gt = wgs84_transform();
        let bbox = gt.bbox();
        assert!((bbox[0] - 0.419).abs() < 1e-6);
        assert!((bbox[2] - (0.419 + 3249.0 * 0.01)).abs() < 1e-6);
        assert!((bbox[3] - 74.810).abs() < 1e-6);
    }

    // TM35FIN: Helsinki (24.9384, 60.1699) ≈ (388356, 6672362)
    #[test]
    fn tm35fin_roundtrip() {
        let crs = Crs::TransverseMercator {
            lat0: 0.0,
            lon0: 27.0_f64.to_radians(),
            k0: 0.9996,
            false_e: 500_000.0,
            false_n: 0.0,
        };
        let (e, n) = crs.forward(24.9384, 60.1699);
        // Known approximate values for TM35FIN
        assert!((e - 385_597.0).abs() < 500.0, "easting={e}");
        assert!((n - 6_672_097.0).abs() < 500.0, "northing={n}");

        let (lon, lat) = crs.inverse(e, n).unwrap();
        assert!((lon - 24.9384).abs() < 0.001, "lon={lon}");
        assert!((lat - 60.1699).abs() < 0.001, "lat={lat}");
    }

    // LAEA EPSG:3035-like: center (10, 55), false origin (1950000, -2100000)
    // OPERA radar uses this
    #[test]
    fn laea_roundtrip() {
        let crs = Crs::LambertAzimuthalEqualArea {
            lat0: 55.0_f64.to_radians(),
            lon0: 10.0_f64.to_radians(),
            false_e: 1_950_000.0,
            false_n: -2_100_000.0,
        };
        // Center of projection should map to (false_e, false_n)
        let (e, n) = crs.forward(10.0, 55.0);
        assert!((e - 1_950_000.0).abs() < 1.0, "easting={e}");
        assert!((n - (-2_100_000.0)).abs() < 1.0, "northing={n}");

        // Helsinki roundtrip
        let (e, n) = crs.forward(24.9384, 60.1699);
        let (lon, lat) = crs.inverse(e, n).unwrap();
        assert!((lon - 24.9384).abs() < 0.001, "lon={lon}");
        assert!((lat - 60.1699).abs() < 0.001, "lat={lat}");
    }

    // Lambert Conformal Conic: MET Norway radar
    #[test]
    fn lcc_roundtrip() {
        let crs = Crs::LambertConformalConic {
            lat1: 58.964_f64.to_radians(),
            lat2: 69.987_f64.to_radians(),
            lat0: 0.0,
            lon0: 0.0,
            false_e: 0.0,
            false_n: 0.0,
        };
        // Oslo roundtrip
        let (e, n) = crs.forward(10.75, 59.91);
        let (lon, lat) = crs.inverse(e, n).unwrap();
        assert!((lon - 10.75).abs() < 0.001, "lon={lon}");
        assert!((lat - 59.91).abs() < 0.001, "lat={lat}");
    }

    // FMI radar extreme corners — verified against GDAL gdaltransform
    #[test]
    fn tm35fin_extreme_inverse() {
        let crs = Crs::TransverseMercator {
            lat0: 0.0,
            lon0: 27.0_f64.to_radians(),
            k0: 0.9996,
            false_e: 500_000.0,
            false_n: 0.0,
        };
        // UL: (-208000, 7926000) should be ~(7.79, 70.42)
        let (lon, lat) = crs.inverse(-208_000.0, 7_926_000.0).unwrap();
        assert!((lon - 7.79).abs() < 0.5, "UL lon={lon}, expected ~7.79");
        assert!((lat - 70.42).abs() < 0.5, "UL lat={lat}, expected ~70.42");

        // LR: (1072000, 6390000) should be ~(36.51, 57.29)
        let (lon, lat) = crs.inverse(1_072_000.0, 6_390_000.0).unwrap();
        assert!((lon - 36.51).abs() < 0.5, "LR lon={lon}, expected ~36.51");
        assert!((lat - 57.29).abs() < 0.5, "LR lat={lat}, expected ~57.29");

        // UR: (1072000, 7926000) should be ~(42.71, 70.77)
        let (lon, lat) = crs.inverse(1_072_000.0, 7_926_000.0).unwrap();
        assert!((lon - 42.71).abs() < 0.5, "UR lon={lon}, expected ~42.71");
        assert!((lat - 70.77).abs() < 0.5, "UR lat={lat}, expected ~70.77");
    }

    // OPERA LAEA extreme corners — verified against GDAL
    #[test]
    fn laea_extreme_inverse() {
        let crs = Crs::LambertAzimuthalEqualArea {
            lat0: 55.0_f64.to_radians(),
            lon0: 10.0_f64.to_radians(),
            false_e: 1_950_000.0,
            false_n: -2_100_000.0,
        };
        // UL: (-1000, 1000) should be ~(-39.57, 67.02)
        let (lon, lat) = crs.inverse(-1000.0, 1000.0).unwrap();
        assert!(
            (lon - (-39.57)).abs() < 1.0,
            "UL lon={lon}, expected ~-39.57"
        );
        assert!((lat - 67.02).abs() < 1.0, "UL lat={lat}, expected ~67.02");

        // LR: (3799000, -4399000) should be ~(29.41, 31.99)
        let (lon, lat) = crs.inverse(3_799_000.0, -4_399_000.0).unwrap();
        assert!((lon - 29.41).abs() < 1.0, "LR lon={lon}, expected ~29.41");
        assert!((lat - 31.99).abs() < 1.0, "LR lat={lat}, expected ~31.99");
    }

    // Test GeoTransform with projected CRS
    #[test]
    fn projected_world_to_pixel() {
        // Simplified OPERA-like raster: LAEA, origin at (-1000, 1000), 2km pixels
        let gt = GeoTransform {
            origin_x: -1000.0,
            origin_y: 1000.0,
            pixel_width: 2000.0,
            pixel_height: 2000.0,
            width: 1900,
            height: 2200,
            crs: Crs::LambertAzimuthalEqualArea {
                lat0: 55.0_f64.to_radians(),
                lon0: 10.0_f64.to_radians(),
                false_e: 1_950_000.0,
                false_n: -2_100_000.0,
            },
        };

        // Center of projection (10, 55) should map to approximately (1950000, -2100000)
        // in projected coords, which in pixel coords is ((1950000+1000)/2000, (1000+2100000)/2000)
        let pixel = gt.world_to_pixel(10.0, 55.0);
        assert!(
            pixel.is_some(),
            "Center of projection should be inside raster"
        );
    }

    // ModelTransformationTag (tag 34264) — axis-aligned matrix
    #[test]
    fn from_transformation_matrix_axis_aligned() {
        // Identity-like: pixel_width=2000, pixel_height=2000 (f is -2000)
        // origin_x=-1000, origin_y=1000
        #[rustfmt::skip]
        let matrix = [
            2000.0, 0.0, 0.0, -1000.0,
            0.0, -2000.0, 0.0, 1000.0,
            0.0, 0.0, 0.0, 0.0,
            0.0, 0.0, 0.0, 1.0,
        ];
        let gt = GeoTransform::from_transformation_matrix(&matrix, 100, 100, Crs::Wgs84).unwrap();
        assert!((gt.origin_x - (-1000.0)).abs() < 1e-10);
        assert!((gt.origin_y - 1000.0).abs() < 1e-10);
        assert!((gt.pixel_width - 2000.0).abs() < 1e-10);
        assert!((gt.pixel_height - 2000.0).abs() < 1e-10);
    }

    // Rotated raster must be rejected
    #[test]
    fn from_transformation_matrix_rotated_rejected() {
        #[rustfmt::skip]
        let matrix = [
            1000.0, 500.0, 0.0, 0.0,   // b=500 → rotation
            -500.0, 1000.0, 0.0, 0.0,   // e=-500 → rotation
            0.0, 0.0, 0.0, 0.0,
            0.0, 0.0, 0.0, 1.0,
        ];
        let result = GeoTransform::from_transformation_matrix(&matrix, 100, 100, Crs::Wgs84);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("Rotated/skewed"), "got: {err}");
    }

    // Too few values
    #[test]
    fn from_transformation_matrix_too_short() {
        let matrix = [1.0, 0.0, 0.0];
        let result = GeoTransform::from_transformation_matrix(&matrix, 100, 100, Crs::Wgs84);
        assert!(result.is_err());
    }

    // ========================================================================
    // Stereographic projection tests
    // ========================================================================

    // FMI radar composite: oblique stereographic centered on Finland
    // PROJ: +proj=stere +lat_0=60 +lon_0=25 +k=1 +x_0=0 +y_0=0 +datum=WGS84
    #[test]
    fn stereographic_roundtrip_center() {
        let crs = Crs::Stereographic {
            lat0: 60.0_f64.to_radians(),
            lon0: 25.0_f64.to_radians(),
            k0: 1.0,
            false_e: 0.0,
            false_n: 0.0,
        };
        // Center of projection should map to (0, 0)
        let (e, n) = crs.forward(25.0, 60.0);
        assert!(e.abs() < 1.0, "easting at center={e}, expected ~0");
        assert!(n.abs() < 1.0, "northing at center={n}, expected ~0");
    }

    #[test]
    fn stereographic_roundtrip_helsinki() {
        let crs = Crs::Stereographic {
            lat0: 60.0_f64.to_radians(),
            lon0: 25.0_f64.to_radians(),
            k0: 1.0,
            false_e: 0.0,
            false_n: 0.0,
        };
        // Helsinki roundtrip
        let (e, n) = crs.forward(24.9384, 60.1699);
        let (lon, lat) = crs.inverse(e, n).unwrap();
        assert!((lon - 24.9384).abs() < 0.001, "lon={lon}");
        assert!((lat - 60.1699).abs() < 0.001, "lat={lat}");
    }

    // DMI radar: +proj=stere +lat_0=56 +lon_0=10.5667 +k=1.0
    #[test]
    fn stereographic_roundtrip_dmi() {
        let crs = Crs::Stereographic {
            lat0: 56.0_f64.to_radians(),
            lon0: 10.5667_f64.to_radians(),
            k0: 1.0,
            false_e: 0.0,
            false_n: 0.0,
        };
        // Copenhagen roundtrip
        let (e, n) = crs.forward(12.5683, 55.6761);
        let (lon, lat) = crs.inverse(e, n).unwrap();
        assert!((lon - 12.5683).abs() < 0.001, "lon={lon}");
        assert!((lat - 55.6761).abs() < 0.001, "lat={lat}");
    }

    /// Roundtrip the four explicit DMI corner coordinates that sit
    /// hundreds of km from the projection origin in opposite
    /// directions. The earlier LCC-conflated `stere_forward` paired
    /// with the Newton-iteration `stere_inverse` was self-consistent
    /// (forward + inverse cancel) but produced wrong absolute
    /// coordinates that broke ODIM rendering. Pinning the round-trip
    /// at off-center points wouldn't have caught the old bug — but
    /// it guards against a future regression that changes `forward`
    /// without keeping `inverse` consistent (the inverse is Newton
    /// iteration calling `forward`, so they stay locked by
    /// construction, and this test confirms that contract).
    #[test]
    fn stereographic_roundtrip_dmi_far_corners() {
        let crs = Crs::Stereographic {
            lat0: 56.0_f64.to_radians(),
            lon0: 10.5666_f64.to_radians(),
            k0: 1.0,
            false_e: 0.0,
            false_n: 0.0,
        };
        for (lon, lat) in [
            (3.0, 60.0),
            (20.735, 59.828),
            (4.379, 52.294),
            (18.893, 52.294),
        ] {
            let (x, y) = crs.forward(lon, lat);
            let (lon_back, lat_back) = crs.inverse(x, y).unwrap();
            assert!(
                (lon_back - lon).abs() < 1e-6,
                "lon roundtrip failed at ({lon}, {lat}): got {lon_back}"
            );
            assert!(
                (lat_back - lat).abs() < 1e-6,
                "lat roundtrip failed at ({lon}, {lat}): got {lat_back}"
            );
        }
    }

    /// Validate `stere_inverse` against **independent** reference
    /// projected coordinates, not just roundtrip self-consistency.
    /// Reference values were computed with `cs2cs +proj=longlat
    /// +ellps=WGS84 +to +proj=stere +lat_0=56 +lon_0=10.5666 +k_0=1
    /// +ellps=WGS84 +units=m` (PROJ 9.x).
    ///
    /// Roundtrip tests like `stereographic_roundtrip_dmi_far_corners`
    /// can pass even when both forward and inverse converge to the
    /// wrong absolute coordinates — the earlier LCC-conflated
    /// `stere_forward` + Newton-iteration `stere_inverse` was
    /// self-consistent but at the wrong absolute coordinates,
    /// shifting tiles by ~50%. This test eliminates that residual
    /// doubt by asserting `inverse(known_xy) ≈ expected_lonlat`,
    /// where the (x, y) values come from PROJ rather than from
    /// `stere_forward`.
    #[test]
    fn stereographic_inverse_absolute_dmi() {
        let crs = Crs::Stereographic {
            lat0: 56.0_f64.to_radians(),
            lon0: 10.5666_f64.to_radians(),
            k0: 1.0,
            false_e: 0.0,
            false_n: 0.0,
        };
        for (x_proj, y_proj, lon_exp, lat_exp) in [
            (91726.258980, -110388.867174, 12.0, 55.0),
            (0.0, 0.0, 10.5666, 56.0),
            (-153890.951450, 169926.823502, 8.0, 57.5),
        ] {
            let (lon, lat) = crs.inverse(x_proj, y_proj).unwrap();
            assert!(
                (lon - lon_exp).abs() < 1e-5,
                "lon mismatch at ({x_proj}, {y_proj}): got {lon}, expected {lon_exp}"
            );
            assert!(
                (lat - lat_exp).abs() < 1e-5,
                "lat mismatch at ({x_proj}, {y_proj}): got {lat}, expected {lat_exp}"
            );
        }
    }

    /// Polar-aspect counterpart of `stereographic_inverse_absolute_dmi`.
    /// Snyder's ellipsoidal oblique formula relies on `cos(χ_c)`
    /// being nonzero, which at the pole holds only because IEEE 754
    /// `cos(π/2) ≈ 6.12e-17` — *numerically* nonzero by rounding,
    /// not algebraically. Validate that the polar case actually
    /// matches PROJ rather than silently riding on float behaviour.
    ///
    /// Reference: `cs2cs +proj=longlat +ellps=WGS84 +to
    /// +proj=stere +lat_0=90 +lon_0=25 +lat_ts=60 +ellps=WGS84
    /// +units=m` (PROJ 9.x). This is FMI/OPERA-style polar
    /// stereographic with standard parallel 60°N.
    #[test]
    fn stereographic_inverse_absolute_polar() {
        // `k0` is the **ellipsoidal** PROJ-compatible scale factor
        // for `+lat_ts=60` on WGS84 — i.e. the value that makes
        // case-1 `ρ = 2·a·k₀·t/D` agree with Snyder's case-2
        // `ρ = a·m_c·t/t_c` (eq. 21-39) which is what PROJ's
        // `+proj=stere +lat_ts=…` uses internally. The
        // engine-odim PROJ-string parser produces this value; the
        // spherical shortcut `(1 + sin|lat_ts|)/2 ≈ 0.9330127` is
        // ~200 m off on a 3000 km radius.
        let k0 = 0.933_069_071_736_356_6;
        let crs = Crs::Stereographic {
            lat0: 90.0_f64.to_radians(),
            lon0: 25.0_f64.to_radians(),
            k0,
            false_e: 0.0,
            false_n: 0.0,
        };
        for (x_proj, y_proj, lon_exp, lat_exp) in [
            (0.0, -3197104.586924, 25.0, 60.0),
            (0.0, 0.0, 25.0, 90.0),
            (-1118215.792079, -2398021.504736, 0.0, 65.0),
        ] {
            let (lon, lat) = crs.inverse(x_proj, y_proj).unwrap();
            assert!(
                (lon - lon_exp).abs() < 1e-4,
                "lon mismatch at ({x_proj}, {y_proj}): got {lon}, expected {lon_exp}"
            );
            assert!(
                (lat - lat_exp).abs() < 1e-4,
                "lat mismatch at ({x_proj}, {y_proj}): got {lat}, expected {lat_exp}"
            );
        }
    }

    // Test at ~1000km from center (edge of typical radar composite)
    #[test]
    fn stereographic_far_from_center() {
        let crs = Crs::Stereographic {
            lat0: 60.0_f64.to_radians(),
            lon0: 25.0_f64.to_radians(),
            k0: 1.0,
            false_e: 0.0,
            false_n: 0.0,
        };
        // Tromsø, ~1200km from center
        let (e, n) = crs.forward(19.0, 69.65);
        let (lon, lat) = crs.inverse(e, n).unwrap();
        assert!((lon - 19.0).abs() < 0.01, "lon={lon}");
        assert!((lat - 69.65).abs() < 0.01, "lat={lat}");
    }

    // ========================================================================
    // Rotated lat-lon tests
    // ========================================================================

    // HIRLAM-like: south pole at (-30, -170)
    #[test]
    fn rotlatlon_roundtrip_center() {
        let sp_lat = (-30.0_f64).to_radians();
        let sp_lon = (-170.0_f64).to_radians();
        let crs = Crs::RotatedLatLon {
            south_pole_lat: sp_lat,
            south_pole_lon: sp_lon,
        };

        // The rotated equator should pass through the geographic north pole
        // of the rotated system (antipode of south pole = 30N, 10E)
        // At the rotated origin (0, 0) → should map to the rotated north pole
        let (rlon, rlat) = crs.forward(10.0, 30.0);
        // Should be near rotated (0, 0) since this is the rotated north pole's position
        // Actually the rotated north pole is at rotated (0, 90) — let's just test roundtrip
        let (lon, lat) = crs.inverse(rlon, rlat).unwrap();
        assert!((lon - 10.0).abs() < 0.01, "lon={lon}");
        assert!((lat - 30.0).abs() < 0.01, "lat={lat}");
    }

    #[test]
    fn rotlatlon_roundtrip_helsinki() {
        let sp_lat = (-30.0_f64).to_radians();
        let sp_lon = (-170.0_f64).to_radians();
        let crs = Crs::RotatedLatLon {
            south_pole_lat: sp_lat,
            south_pole_lon: sp_lon,
        };

        let (rlon, rlat) = crs.forward(24.9384, 60.1699);
        let (lon, lat) = crs.inverse(rlon, rlat).unwrap();
        assert!((lon - 24.9384).abs() < 0.01, "lon={lon}");
        assert!((lat - 60.1699).abs() < 0.01, "lat={lat}");
    }

    // Identity rotation: south pole at (-90, 0) should be a no-op
    #[test]
    fn rotlatlon_identity() {
        let crs = Crs::RotatedLatLon {
            south_pole_lat: (-90.0_f64).to_radians(),
            south_pole_lon: 0.0_f64.to_radians(),
        };

        let (rlon, rlat) = crs.forward(24.0, 60.0);
        assert!((rlon - 24.0).abs() < 0.01, "rlon={rlon}");
        assert!((rlat - 60.0).abs() < 0.01, "rlat={rlat}");
    }

    #[test]
    fn ecef_known_points() {
        // (0,0,0): on the equator at the prime meridian, ECEF = (a, 0, 0).
        let [x, y, z] = geodetic_to_ecef(0.0, 0.0, 0.0);
        assert!((x - WGS84_A).abs() < 1e-3, "x={x}");
        assert!(y.abs() < 1e-6 && z.abs() < 1e-6, "y={y} z={z}");

        // North pole: x=y=0, z = polar radius b = a(1-f) (+height).
        let b = WGS84_A * (1.0 - WGS84_F);
        let [x, y, z] = geodetic_to_ecef(123.0, 90.0, 0.0);
        assert!(x.abs() < 1e-6 && y.abs() < 1e-6, "x={x} y={y}");
        assert!((z - b).abs() < 1e-3, "z={z} b={b}");

        // 90°E on the equator: ECEF = (0, a, 0); height adds along the radial.
        let [x, y, z] = geodetic_to_ecef(90.0, 0.0, 100.0);
        assert!(x.abs() < 1e-6, "x={x}");
        assert!((y - (WGS84_A + 100.0)).abs() < 1e-3, "y={y}");
        assert!(z.abs() < 1e-6, "z={z}");
    }
}
