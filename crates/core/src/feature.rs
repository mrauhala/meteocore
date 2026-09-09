use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::error::DataServerError;

/// GeoJSON-style geometry.
#[derive(Debug, Clone)]
pub enum Geometry {
    Point {
        x: f64,
        y: f64,
    },
    Polygon {
        /// Exterior ring as [lon, lat] coordinate pairs.
        exterior: Vec<[f64; 2]>,
        /// Interior rings (holes), each as [lon, lat] coordinate pairs.
        holes: Vec<Vec<[f64; 2]>>,
    },
    MultiPolygon {
        /// Each polygon is (exterior ring, holes).
        #[allow(clippy::type_complexity)]
        polygons: Vec<(Vec<[f64; 2]>, Vec<Vec<[f64; 2]>>)>,
    },
    /// Null geometry for features without spatial location (RFC 7946 §3.2).
    Null,
}

impl Geometry {
    /// Compute the bounding box [west, south, east, north] of this geometry.
    /// Returns None for null geometries.
    pub fn bbox(&self) -> Option<[f64; 4]> {
        match self {
            Geometry::Point { x, y } => Some([*x, *y, *x, *y]),
            Geometry::Polygon { exterior, .. } => Some(ring_bbox(exterior)),
            Geometry::MultiPolygon { polygons } => {
                let mut bbox = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
                for (ext, _) in polygons {
                    let b = ring_bbox(ext);
                    bbox[0] = bbox[0].min(b[0]);
                    bbox[1] = bbox[1].min(b[1]);
                    bbox[2] = bbox[2].max(b[2]);
                    bbox[3] = bbox[3].max(b[3]);
                }
                Some(bbox)
            }
            Geometry::Null => None,
        }
    }

    /// Compute the centroid (lon, lat) of this geometry.
    /// Returns None for null geometries.
    pub fn centroid(&self) -> Option<(f64, f64)> {
        match self {
            Geometry::Point { x, y } => Some((*x, *y)),
            Geometry::Polygon { exterior, .. } => Some(ring_centroid(exterior)),
            Geometry::MultiPolygon { polygons } => {
                // Area-weighted centroid across all polygons
                let mut total_area = 0.0_f64;
                let mut cx = 0.0_f64;
                let mut cy = 0.0_f64;
                for (ext, _) in polygons {
                    let area = ring_signed_area(ext).abs();
                    let (px, py) = ring_centroid(ext);
                    total_area += area;
                    cx += px * area;
                    cy += py * area;
                }
                if total_area > 0.0 {
                    Some((cx / total_area, cy / total_area))
                } else {
                    // Degenerate: average of first points
                    let n = polygons.len() as f64;
                    let sx: f64 = polygons.iter().map(|(ext, _)| ext[0][0]).sum();
                    let sy: f64 = polygons.iter().map(|(ext, _)| ext[0][1]).sum();
                    Some((sx / n, sy / n))
                }
            }
            Geometry::Null => None,
        }
    }

    /// Whether a point (lon, lat) lies inside this geometry.
    ///
    /// Ray-casting on exterior rings with holes excluded — the same
    /// [`point_in_ring`] primitive [`QueryPolygon::contains`] uses, so
    /// feature geometry and WKT query geometry can never disagree about what
    /// "inside" means. A `Point` geometry contains nothing (an exact float
    /// match would be meaningless); `Null` contains nothing.
    ///
    /// Callers testing many points against many geometries should prefilter
    /// on [`Geometry::bbox`] — this walks every ring on every call.
    pub fn contains(&self, x: f64, y: f64) -> bool {
        let in_polygon = |exterior: &Vec<[f64; 2]>, holes: &Vec<Vec<[f64; 2]>>| {
            point_in_ring(x, y, exterior) && !holes.iter().any(|h| point_in_ring(x, y, h))
        };
        match self {
            Geometry::Polygon { exterior, holes } => in_polygon(exterior, holes),
            Geometry::MultiPolygon { polygons } => polygons
                .iter()
                .any(|(exterior, holes)| in_polygon(exterior, holes)),
            Geometry::Point { .. } | Geometry::Null => false,
        }
    }
}

fn ring_bbox(ring: &[[f64; 2]]) -> [f64; 4] {
    let mut bbox = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    for &[x, y] in ring {
        bbox[0] = bbox[0].min(x);
        bbox[1] = bbox[1].min(y);
        bbox[2] = bbox[2].max(x);
        bbox[3] = bbox[3].max(y);
    }
    bbox
}

fn ring_signed_area(ring: &[[f64; 2]]) -> f64 {
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    let mut area = 0.0;
    for i in 0..n {
        let j = (i + 1) % n;
        area += ring[i][0] * ring[j][1];
        area -= ring[j][0] * ring[i][1];
    }
    area / 2.0
}

fn ring_centroid(ring: &[[f64; 2]]) -> (f64, f64) {
    let n = ring.len();
    if n == 0 {
        return (0.0, 0.0);
    }
    let area = ring_signed_area(ring);
    if area.abs() < f64::EPSILON {
        // Degenerate polygon: use simple average
        let sx: f64 = ring.iter().map(|c| c[0]).sum();
        let sy: f64 = ring.iter().map(|c| c[1]).sum();
        return (sx / n as f64, sy / n as f64);
    }
    let mut cx = 0.0;
    let mut cy = 0.0;
    for i in 0..n {
        let j = (i + 1) % n;
        let cross = ring[i][0] * ring[j][1] - ring[j][0] * ring[i][1];
        cx += (ring[i][0] + ring[j][0]) * cross;
        cy += (ring[i][1] + ring[j][1]) * cross;
    }
    let factor = 1.0 / (6.0 * area);
    (cx * factor, cy * factor)
}

/// Maximum number of vertices allowed in a WKT polygon query.
const MAX_WKT_VERTICES: usize = 10_000;

/// Maximum byte length of a WKT coords string.
const MAX_WKT_LENGTH: usize = 10_240;

/// A parsed polygon for area queries: exterior ring, optional holes, and precomputed bbox.
#[derive(Debug, Clone)]
pub struct QueryPolygon {
    pub exterior: Vec<[f64; 2]>,
    pub holes: Vec<Vec<[f64; 2]>>,
    pub bbox: Bbox,
}

impl QueryPolygon {
    /// Test whether a point (lon, lat) lies inside this polygon.
    /// Uses ray-casting for the exterior ring, then excludes holes.
    pub fn contains(&self, x: f64, y: f64) -> bool {
        if !self.bbox.contains(x, y) {
            return false;
        }
        // An antimeridian-crossing polygon (bbox west > east, from the
        // `west,south,east,north` form) is tested in a 0..360 longitude
        // frame, where its ring is an ordinary planar shape.
        let wrap = self.bbox.crosses_antimeridian();
        let norm = |lon: f64| if wrap && lon < 0.0 { lon + 360.0 } else { lon };
        let x = norm(x);
        if !point_in_ring_by(x, y, &self.exterior, norm) {
            return false;
        }
        for hole in &self.holes {
            if point_in_ring_by(x, y, hole, norm) {
                return false;
            }
        }
        true
    }
}

/// Per-dimension cap of a gridded engine's EDR area grid (cells per axis);
/// a wider bbox is *coarsened* to this, never refused.
pub const MAX_AREA_DIM: usize = 256;
/// Total value budget of one gridded area response across timesteps ×
/// cells × parameters (≈ 8 MB of CoverageJSON). One home for every engine
/// so the budget cannot drift per engine (#672 review); GRIB / GeoTIFF /
/// ODIM / PostGIS still carry older local limits — consolidating them is a
/// follow-up.
pub const MAX_AREA_VALUES: usize = 1_000_000;

/// Enforce [`MAX_AREA_VALUES`] for a `timesteps × ny × nx × parameters`
/// response, with the message every engine returns.
pub fn check_area_budget(
    timesteps: usize,
    ny: usize,
    nx: usize,
    parameters: usize,
) -> Result<(), DataServerError> {
    let total = timesteps
        .saturating_mul(ny)
        .saturating_mul(nx)
        .saturating_mul(parameters);
    if total > MAX_AREA_VALUES {
        return Err(DataServerError::QueryTooLarge(format!(
            "Area query would return {total} values ({timesteps} timesteps × {ny} × {nx} cells × \
             {parameters} parameters); the limit is {MAX_AREA_VALUES} — narrow the datetime \
             window, the polygon or the parameters"
        )));
    }
    Ok(())
}

/// Cell-centre axes of a regular CRS84 grid over an area query's polygon
/// bbox, at (roughly) a source's native resolution. The shared shape of a
/// gridded engine's EDR *area* / *radius* result (#671): a CoverageJSON
/// `Grid` must be rectangular, so the domain is the bbox and the engine
/// masks cells outside the polygon to null via [`QueryPolygon::cell_mask`].
///
/// `x` ascends west→east, `y` descends north→south (index 0 = north),
/// matching raster row order and the `[t, y, x]` NdArray layout. An
/// antimeridian-crossing bbox (`west > east`, see [`Bbox`]) spans the seam:
/// `x` keeps ascending through +180 and is wrapped into `(-180, 180]`, so
/// the values are not monotonic in that one case. Each dimension is
/// clamped to `[1, max_dim]`, so a bbox much wider than the source
/// resolution allows is *coarsened*, never refused — the total-value
/// budget is [`check_area_budget`].
#[derive(Debug, Clone, PartialEq)]
pub struct AreaGridAxes {
    pub x: Vec<f64>,
    pub y: Vec<f64>,
}

impl AreaGridAxes {
    /// `(nx, ny)`.
    pub fn dims(&self) -> (usize, usize) {
        (self.x.len(), self.y.len())
    }

    /// Row-major cell index of `(ix, iy)` — `iy * nx + ix`, the layout of
    /// [`QueryPolygon::cell_mask`] and of a `[y, x]` NdArray.
    pub fn index(&self, ix: usize, iy: usize) -> usize {
        iy * self.x.len() + ix
    }
}

impl QueryPolygon {
    /// See [`AreaGridAxes`]. `res_lon_deg` / `res_lat_deg` are the source's
    /// cell sizes in degrees (non-positive or non-finite values are treated
    /// as "one cell").
    pub fn sample_grid(&self, res_lon_deg: f64, res_lat_deg: f64, max_dim: usize) -> AreaGridAxes {
        let max_dim = max_dim.max(1);
        let Bbox {
            west,
            south,
            east,
            north,
        } = self.bbox;
        let lon_span = if self.bbox.crosses_antimeridian() {
            east + 360.0 - west
        } else {
            east - west
        };
        let cells = |span: f64, res: f64| -> usize {
            if !(res.is_finite() && res > 0.0) {
                return 1;
            }
            ((span / res).ceil() as usize).clamp(1, max_dim)
        };
        let nx = cells(lon_span, res_lon_deg);
        let ny = cells(north - south, res_lat_deg);
        let cell_w = lon_span / nx as f64;
        let cell_h = (north - south) / ny as f64;
        AreaGridAxes {
            x: (0..nx)
                .map(|ix| {
                    let lon = west + (ix as f64 + 0.5) * cell_w;
                    if lon > 180.0 {
                        lon - 360.0
                    } else {
                        lon
                    }
                })
                .collect(),
            y: (0..ny)
                .map(|iy| north - (iy as f64 + 0.5) * cell_h)
                .collect(),
        }
    }

    /// Which cells of `axes` an area query should fill: row-major
    /// (`iy * nx + ix`), `true` where the cell centre is inside the polygon.
    /// When no centre is inside — a sliver, an L, or a ring smaller than one
    /// native cell whose bbox collapsed to a single cell whose centre the
    /// shape misses — the cells containing a polygon vertex are used
    /// instead, so a small-but-real shape still returns its data instead of
    /// a false "no cell inside" 404.
    pub fn cell_mask(&self, axes: &AreaGridAxes) -> Vec<bool> {
        let (nx, ny) = axes.dims();
        let mut mask: Vec<bool> = axes
            .y
            .iter()
            .flat_map(|&y| axes.x.iter().map(move |&x| (x, y)))
            .map(|(x, y)| self.contains(x, y))
            .collect();
        if mask.iter().any(|&m| m) || nx == 0 || ny == 0 {
            return mask;
        }
        // Fallback: mark the cell whose extent contains each vertex. Cells
        // are `cell_w × cell_h` around their centres.
        let cell_w = if nx > 1 {
            (axes.x[1] - axes.x[0]).rem_euclid(360.0)
        } else {
            self.bbox.east - self.bbox.west
                + if self.bbox.crosses_antimeridian() {
                    360.0
                } else {
                    0.0
                }
        };
        let cell_h = if ny > 1 {
            axes.y[0] - axes.y[1]
        } else {
            self.bbox.north - self.bbox.south
        };
        for &[vx, vy] in self.exterior.iter().chain(self.holes.iter().flatten()) {
            let dx = (vx - (axes.x[0] - cell_w / 2.0)).rem_euclid(360.0);
            let ix = ((dx / cell_w) as usize).min(nx - 1);
            let dy = (axes.y[0] + cell_h / 2.0) - vy;
            if dy < 0.0 {
                continue;
            }
            let iy = ((dy / cell_h) as usize).min(ny - 1);
            mask[axes.index(ix, iy)] = true;
        }
        mask
    }
}

/// Ray-casting point-in-polygon test for a single ring.
fn point_in_ring(x: f64, y: f64, ring: &[[f64; 2]]) -> bool {
    point_in_ring_by(x, y, ring, |lon| lon)
}

/// [`point_in_ring`] with the ring's longitudes passed through `norm`
/// (the caller normalises `x` the same way) — how an antimeridian-crossing
/// ring is tested in a 0..360 frame without allocating a shifted copy.
fn point_in_ring_by(x: f64, y: f64, ring: &[[f64; 2]], norm: impl Fn(f64) -> f64) -> bool {
    let n = ring.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = (norm(ring[i][0]), ring[i][1]);
        let (xj, yj) = (norm(ring[j][0]), ring[j][1]);
        if ((yi > y) != (yj > y)) && (x < (xj - xi) * (y - yi) / (yj - yi) + xi) {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Parse a WKT ring string (comma-separated `lon lat` pairs) into coordinate pairs.
fn parse_wkt_ring(ring_str: &str) -> Result<Vec<[f64; 2]>, DataServerError> {
    let points: Vec<&str> = ring_str.split(',').collect();
    if points.len() < 3 {
        return Err(DataServerError::InvalidParameter(
            "Polygon ring must have at least 3 coordinate pairs".into(),
        ));
    }
    if points.len() > MAX_WKT_VERTICES {
        return Err(DataServerError::InvalidParameter(format!(
            "Polygon ring has {} vertices, maximum is {}",
            points.len(),
            MAX_WKT_VERTICES
        )));
    }
    let mut coords = Vec::with_capacity(points.len());
    for point in &points {
        let parts: Vec<&str> = point.split_whitespace().collect();
        if parts.len() != 2 {
            return Err(DataServerError::InvalidParameter(format!(
                "Invalid coordinate pair: '{}'",
                point.trim()
            )));
        }
        let lon: f64 = parts[0].parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid longitude: {}", parts[0]))
        })?;
        let lat: f64 = parts[1].parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid latitude: {}", parts[1]))
        })?;
        if !lon.is_finite() || !lat.is_finite() {
            return Err(DataServerError::InvalidParameter(
                "Coordinates must be finite numbers".into(),
            ));
        }
        if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
            return Err(DataServerError::InvalidParameter(format!(
                "Coordinates out of range: lon={lon}, lat={lat}"
            )));
        }
        coords.push([lon, lat]);
    }
    Ok(coords)
}

/// Parse EDR area query coordinates into a polygon.
///
/// Accepts:
/// - `POLYGON((lon1 lat1, lon2 lat2, ...))` — single ring
/// - `POLYGON((exterior), (hole1), (hole2), ...)` — with holes
/// - `west,south,east,north` — bbox format (converted to rectangular polygon)
pub fn parse_area_coords(coords: &str) -> Result<QueryPolygon, DataServerError> {
    if coords.len() > MAX_WKT_LENGTH {
        return Err(DataServerError::InvalidParameter(format!(
            "Coordinates string too long ({} bytes, max {})",
            coords.len(),
            MAX_WKT_LENGTH
        )));
    }

    let trimmed = coords.trim().to_uppercase();

    // Try WKT POLYGON format
    if let Some(inner) = trimmed
        .strip_prefix("POLYGON((")
        .or_else(|| trimmed.strip_prefix("POLYGON (("))
        .and_then(|s| s.strip_suffix("))"))
    {
        // Re-parse from original (not uppercased) to preserve numeric precision
        let original_trimmed = coords.trim();
        let original_inner = original_trimmed
            .get("POLYGON((".len()..original_trimmed.len() - "))".len())
            .or_else(|| {
                original_trimmed.get("POLYGON ((".len()..original_trimmed.len() - "))".len())
            })
            .unwrap_or(inner);
        let original_rings: Vec<&str> = original_inner.split("),(").collect();

        let exterior = parse_wkt_ring(original_rings[0])?;

        let mut holes = Vec::new();
        for ring_str in original_rings.iter().skip(1) {
            holes.push(parse_wkt_ring(ring_str)?);
        }

        let bb = ring_bbox(&exterior);
        let bbox = Bbox::new(bb[0], bb[1], bb[2], bb[3]).map_err(DataServerError::InvalidBbox)?;

        return Ok(QueryPolygon {
            exterior,
            holes,
            bbox,
        });
    }

    // Try simple bbox format: west,south,east,north
    let parts: Vec<&str> = coords.trim().split(',').collect();
    if parts.len() == 4 {
        let west: f64 = parts[0].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid west: {}", parts[0]))
        })?;
        let south: f64 = parts[1].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid south: {}", parts[1]))
        })?;
        let east: f64 = parts[2].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid east: {}", parts[2]))
        })?;
        let north: f64 = parts[3].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid north: {}", parts[3]))
        })?;
        let bbox = Bbox::new(west, south, east, north).map_err(DataServerError::InvalidBbox)?;
        let exterior = vec![
            [west, south],
            [east, south],
            [east, north],
            [west, north],
            [west, south],
        ];
        return Ok(QueryPolygon {
            exterior,
            holes: Vec::new(),
            bbox,
        });
    }

    Err(DataServerError::InvalidParameter(
        "Expected coords as POLYGON((lon1 lat1, lon2 lat2, ...)) or west,south,east,north".into(),
    ))
}

/// Parse an EDR position-query `coords` value.
///
/// **Returns `(lat, lon)` — latitude first**, matching the
/// position-query callers in `engine-odim` and `engine-geotiff`. Note
/// that `engine-grib`'s local `parse_coords` returns `(lon, lat)`; a
/// future migration of that engine onto this shared parser must
/// account for the swapped order.
///
/// Accepts WKT `POINT(lon lat)` (a leading space before `(` is
/// tolerated for PROJ-style input) and the bare `lon,lat` shorthand.
/// Longitude/latitude must be finite and within `±180` / `±90`, so a
/// transposed `lat,lon` pair fails loudly rather than querying a
/// nonsense location.
pub fn parse_point_coords(coords: &str) -> Result<(f64, f64), DataServerError> {
    let trimmed = coords.trim();

    let pair = if let Some(inner) = trimmed
        .strip_prefix("POINT(")
        .or_else(|| trimmed.strip_prefix("POINT ("))
        .and_then(|s| s.strip_suffix(')'))
    {
        inner.split_whitespace().collect::<Vec<_>>()
    } else {
        trimmed.split(',').map(str::trim).collect::<Vec<_>>()
    };

    if pair.len() != 2 {
        return Err(DataServerError::InvalidParameter(
            "Expected POINT(lon lat) or lon,lat format".into(),
        ));
    }
    let lon: f64 = pair[0].parse().map_err(|_| {
        DataServerError::InvalidParameter(format!("Invalid longitude: {}", pair[0]))
    })?;
    let lat: f64 = pair[1]
        .parse()
        .map_err(|_| DataServerError::InvalidParameter(format!("Invalid latitude: {}", pair[1])))?;
    if !lon.is_finite() || !lat.is_finite() {
        return Err(DataServerError::InvalidParameter(
            "Coordinates must be finite numbers".into(),
        ));
    }
    if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
        return Err(DataServerError::InvalidParameter(format!(
            "Coordinates out of range: lon={lon}, lat={lat}"
        )));
    }
    Ok((lat, lon))
}

/// Number of vertices of the polygon [`radius_polygon_wkt`] builds. 64
/// keeps the chord sag under 0.13 % of the radius (`1 - cos(π/64)`), well
/// below any engine's sampling resolution.
pub const RADIUS_POLYGON_VERTICES: usize = 64;

/// Build the WKT `POLYGON` approximating a geodesic circle of `radius_m`
/// metres around `(lon, lat)` — the shared translation of an EDR *radius*
/// query into the *area* query every engine already answers, so the two
/// query types cannot disagree about what "within" means.
///
/// Vertices are placed with [`crate::geo::destination_point`] (the same
/// spherical geodesy the radar engines use) at equal bearings, clockwise
/// from north, ring closed. Rejects a non-finite or non-positive radius,
/// a circle that would contain a pole, and one that would cross the
/// antimeridian: the resulting ring would fold over in lon/lat space and
/// no area engine handles a bbox that wraps (#667).
pub fn radius_polygon_wkt(lon: f64, lat: f64, radius_m: f64) -> Result<String, DataServerError> {
    if !radius_m.is_finite() || radius_m <= 0.0 {
        return Err(DataServerError::InvalidParameter(
            "Radius must be a finite, positive distance".into(),
        ));
    }
    if !lon.is_finite() || !lat.is_finite() || lon.abs() > 180.0 || lat.abs() > 90.0 {
        return Err(DataServerError::InvalidParameter(
            "Centre must be a finite lon/lat within ±180 / ±90".into(),
        ));
    }
    // Angular radius in degrees of latitude; a circle reaching a pole has
    // no single-ring lon/lat representation. The margin keeps every vertex
    // at least ~1 km from the pole, where longitude is ill-conditioned and
    // neighbouring vertices would otherwise get arbitrary longitudes that
    // the bbox-span check below cannot detect.
    const POLE_MARGIN_DEG: f64 = 0.01;
    let ang_deg = (radius_m / crate::geo::EARTH_RADIUS_M).to_degrees();
    if lat.abs() + ang_deg >= 90.0 - POLE_MARGIN_DEG {
        return Err(DataServerError::InvalidParameter(
            "Radius circle would contain a pole; use an area query instead".into(),
        ));
    }
    let ring: Vec<[f64; 2]> = (0..RADIUS_POLYGON_VERTICES)
        .map(|i| {
            let bearing = 360.0 * i as f64 / RADIUS_POLYGON_VERTICES as f64;
            let (x, y) = crate::geo::destination_point(lon, lat, radius_m, bearing);
            [x, y]
        })
        .collect();
    let bb = ring_bbox(&ring);
    if bb[2] - bb[0] > 180.0 {
        return Err(DataServerError::InvalidParameter(
            "Radius circle would cross the antimeridian; use an area query instead".into(),
        ));
    }
    let mut wkt = String::with_capacity(32 + 24 * (RADIUS_POLYGON_VERTICES + 1));
    wkt.push_str("POLYGON((");
    for (i, [x, y]) in ring.iter().chain(std::iter::once(&ring[0])).enumerate() {
        if i > 0 {
            wkt.push_str(", ");
        }
        wkt.push_str(&format!("{x} {y}"));
    }
    wkt.push_str("))");
    Ok(wkt)
}

/// Parse a WKT `LINESTRING(lon lat, lon lat, ...)` into a `Vec<(lon, lat)>`
/// (engine-friendly order — note `parse_point_coords` returns `(lat, lon)`
/// for legacy reasons; cross-section paths always carry `(lon, lat)` here).
///
/// A leading space before `(` is tolerated. `LINESTRINGZ` / `LINESTRINGM` /
/// `LINESTRINGZM` are rejected — per-node z and time are deferred to a
/// follow-up. At least two distinct nodes are required (a single node is
/// not a path; the position query covers that case).
pub fn parse_linestring_coords(coords: &str) -> Result<Vec<(f64, f64)>, DataServerError> {
    // Bound the input before any parsing so a 10 MB payload can't
    // allocate one `(f64, f64)` per comma before `TRAJECTORY_MAX_NODES`
    // (engine-odim) ever clamps the resampled path. Same limit as
    // `parse_area_coords`. Flagged by claude-review on PR #275.
    if coords.len() > MAX_WKT_LENGTH {
        return Err(DataServerError::InvalidParameter(format!(
            "LINESTRING geometry exceeds maximum length of {MAX_WKT_LENGTH} bytes"
        )));
    }
    let trimmed = coords.trim();

    // Reject Z/M variants explicitly so the error message points at the
    // dimensional mismatch rather than failing as "not a number".
    //
    // Compare bytes (not `&str` slices) — axum percent-decodes query
    // params into UTF-8, so a payload like `coords=LINESTRING%C3%A9(...)`
    // would arrive with a multibyte char straddling byte index 11 and a
    // `&str` slice at that index would panic (`byte index … is not a
    // char boundary`), crashing the handler with an unhandled 500.
    for variant in ["LINESTRINGZM", "LINESTRINGZ", "LINESTRINGM"] {
        if trimmed.len() >= variant.len()
            && trimmed.as_bytes()[..variant.len()].eq_ignore_ascii_case(variant.as_bytes())
        {
            return Err(DataServerError::InvalidParameter(format!(
                "{variant} is not supported — pass a plain 2-D LINESTRING(lon lat, lon lat, …)"
            )));
        }
    }

    let inner = strip_wkt_prefix(trimmed, "LINESTRING").ok_or_else(|| {
        DataServerError::InvalidParameter(
            "Expected WKT LINESTRING(lon lat, lon lat, …) geometry".into(),
        )
    })?;

    let mut nodes = Vec::new();
    for part in inner.split(',') {
        let tokens: Vec<&str> = part.split_whitespace().collect();
        if tokens.len() != 2 {
            return Err(DataServerError::InvalidParameter(format!(
                "LINESTRING node '{}' is not 'lon lat'",
                part.trim()
            )));
        }
        let lon: f64 = tokens[0].parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid longitude: {}", tokens[0]))
        })?;
        let lat: f64 = tokens[1].parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid latitude: {}", tokens[1]))
        })?;
        if !lon.is_finite() || !lat.is_finite() {
            return Err(DataServerError::InvalidParameter(
                "LINESTRING coordinates must be finite".into(),
            ));
        }
        if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
            return Err(DataServerError::InvalidParameter(format!(
                "LINESTRING node out of range: lon={lon}, lat={lat}"
            )));
        }
        nodes.push((lon, lat));
    }

    if nodes.len() < 2 {
        return Err(DataServerError::InvalidParameter(
            "LINESTRING must have at least two nodes — use POINT(...) for a single location".into(),
        ));
    }
    // A LINESTRING whose nodes are all the same point produces a
    // zero-length path. Downstream `resample_path` would return two
    // identical composite tuples, and the rendered CoverageJSON `Section`
    // domain would carry duplicate `[t,x,y]` values — semantically
    // meaningless and prone to surprise clients. Reject early so the
    // caller can fix the request rather than receive a degenerate
    // coverage. (A LINESTRING with *some* repeated vertices and a
    // non-zero total length is still accepted.)
    let (lon0, lat0) = nodes[0];
    if nodes.iter().all(|&(lon, lat)| lon == lon0 && lat == lat0) {
        return Err(DataServerError::InvalidParameter(
            "LINESTRING nodes must not all be identical — use POINT(...) for a single location"
                .into(),
        ));
    }

    Ok(nodes)
}

/// Strip a leading `KEYWORD(` / `KEYWORD (` and the matching trailing `)`.
/// Case-insensitive on the keyword. Returns `None` when the input does not
/// match the wrapper shape.
fn strip_wkt_prefix<'a>(s: &'a str, keyword: &str) -> Option<&'a str> {
    if s.len() < keyword.len() + 2 {
        return None;
    }
    // Byte-level compare so a multibyte char straddling `keyword.len()`
    // doesn't panic (see the matching note in `parse_linestring_coords`).
    // `s.get(..keyword.len())` would also work; bytes are clearer here.
    if !s.as_bytes()[..keyword.len()].eq_ignore_ascii_case(keyword.as_bytes()) {
        return None;
    }
    // `keyword.len()` is the same byte count as
    // `s.as_bytes()[..keyword.len()]`, and the case-insensitive ASCII
    // match above proves those bytes are ASCII (a UTF-8 leading byte
    // never matches an ASCII keyword byte), so this str slice is on a
    // char boundary by construction.
    let after = s[keyword.len()..].trim_start();
    after
        .strip_prefix('(')
        .and_then(|inner| inner.strip_suffix(')'))
        .map(str::trim)
}

/// A typed property value. Keeps ds-core free of serde_json.
#[derive(Debug, Clone, PartialEq)]
pub enum PropertyValue {
    String(String),
    Float(f64),
    Integer(i64),
    Bool(bool),
    Null,
    /// An ordered, **flat** list of scalar values (e.g. a radar site's measured
    /// quantities or sweep elevation angles). Elements are expected to be
    /// scalars — engines do not nest `List`s, and the Features JSON serializer
    /// (which recurses) and the MVT tag encoder (which flattens to a joined
    /// string) both rely on shallow, engine-constructed nesting rather than a
    /// runtime depth guard. There is no path from untrusted input to a
    /// `PropertyValue`, so depth is bounded by construction.
    List(Vec<PropertyValue>),
}

impl PropertyValue {
    /// The string value, or `None` for any other variant.
    ///
    /// Deliberately strict — it does NOT stringify numbers. An engine reading
    /// a config-named property from a foreign collection wants a wrong
    /// property name to produce nothing (visible, diagnosable) rather than a
    /// plausible-looking rendering of the wrong field.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            PropertyValue::String(s) => Some(s),
            _ => None,
        }
    }

    /// The numeric value. Accepts both `Float` and `Integer`, since which one
    /// a JSON property decodes to depends on whether the source wrote a
    /// decimal point.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            PropertyValue::Float(v) => Some(*v),
            PropertyValue::Integer(v) => Some(*v as f64),
            _ => None,
        }
    }
}

/// A single feature with geometry and properties.
/// Geometry and properties are wrapped in `Arc` for cheap cloning
/// (pagination returns owned features, so clone cost matters at scale).
#[derive(Debug, Clone)]
pub struct Feature {
    pub id: String,
    pub geometry: Arc<Geometry>,
    pub properties: Arc<HashMap<String, PropertyValue>>,
}

/// A page of features with pagination metadata.
#[derive(Debug, Clone)]
pub struct FeaturePage {
    pub features: Vec<Feature>,
    pub number_matched: usize,
    pub number_returned: usize,
    pub next_offset: Option<usize>,
}

/// Bounding box: west, south, east, north.
/// Supports antimeridian-crossing bboxes where west > east (OGC API Features §7.15.3).
#[derive(Debug, Clone, Copy)]
pub struct Bbox {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
}

impl Bbox {
    pub fn new(west: f64, south: f64, east: f64, north: f64) -> Result<Self, String> {
        for v in [west, south, east, north] {
            if v.is_nan() || v.is_infinite() {
                return Err("bbox coordinates must be finite numbers".into());
            }
        }
        if !(-180.0..=180.0).contains(&west) || !(-180.0..=180.0).contains(&east) {
            return Err("bbox longitude out of range (-180..180)".into());
        }
        if south < -90.0 || north > 90.0 {
            return Err("bbox latitude out of range (-90..90)".into());
        }
        if south > north {
            return Err("bbox south must be <= north".into());
        }
        // Note: west > east is valid — it indicates an antimeridian-crossing bbox.
        Ok(Self {
            west,
            south,
            east,
            north,
        })
    }

    /// Whether this bbox crosses the antimeridian (west > east).
    pub fn crosses_antimeridian(&self) -> bool {
        self.west > self.east
    }

    /// Check if a point falls within this bbox.
    pub fn contains(&self, x: f64, y: f64) -> bool {
        let lon_ok = if self.crosses_antimeridian() {
            x >= self.west || x <= self.east
        } else {
            x >= self.west && x <= self.east
        };
        lon_ok && y >= self.south && y <= self.north
    }

    /// Check if this bbox intersects another bbox (as [west, south, east, north]).
    pub fn intersects_bbox(&self, other: &[f64; 4]) -> bool {
        let other_crosses = other[0] > other[2]; // other west > other east
        let lon_ok = match (self.crosses_antimeridian(), other_crosses) {
            (false, false) => {
                // Neither crosses: standard overlap test
                self.west <= other[2] && self.east >= other[0]
            }
            (true, false) => {
                // Self crosses, other doesn't: intersects unless other is entirely in the gap
                !(other[2] < self.west && other[0] > self.east)
            }
            (false, true) => {
                // Other crosses, self doesn't: intersects unless self is entirely in the gap
                !(self.east < other[0] && self.west > other[2])
            }
            (true, true) => {
                // Both cross: always intersects (they share the antimeridian region)
                true
            }
        };
        lon_ok && self.south <= other[3] && self.north >= other[1]
    }
}

/// A datetime interval with optional open bounds.
#[derive(Debug, Clone)]
pub struct DatetimeInterval {
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
}

/// Query parameters for feature retrieval.
#[derive(Debug, Clone)]
pub struct FeatureQuery {
    pub bbox: Option<Bbox>,
    pub limit: usize,
    pub offset: usize,
    pub datetime: Option<DatetimeInterval>,
    /// Sort terms in precedence order, empty for the engine's natural order.
    ///
    /// Engines that advertise sortables via [`crate::feature_engine::
    /// FeatureEngine::sortables`] MUST apply this BEFORE `offset`/`limit` —
    /// sorting a page after slicing it silently returns the wrong rows.
    /// [`sort_features`] does it correctly; call that rather than hand-rolling.
    pub sortby: Vec<SortKey>,
}

/// Sort direction for one [`SortKey`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    Ascending,
    Descending,
}

/// One sort term: a feature property and a direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortKey {
    pub property: String,
    pub direction: SortDirection,
}

impl SortKey {
    pub fn ascending(property: impl Into<String>) -> Self {
        Self {
            property: property.into(),
            direction: SortDirection::Ascending,
        }
    }

    pub fn descending(property: impl Into<String>) -> Self {
        Self {
            property: property.into(),
            direction: SortDirection::Descending,
        }
    }
}

/// Where a value type sorts relative to other types, so a mixed-type property
/// still yields a total order instead of an inconsistent comparator (which
/// would make `sort_by` misbehave rather than merely look odd).
fn type_rank(v: &PropertyValue) -> u8 {
    match v {
        PropertyValue::Bool(_) => 0,
        PropertyValue::Integer(_) | PropertyValue::Float(_) => 1,
        PropertyValue::String(_) => 2,
        PropertyValue::List(_) => 3,
        PropertyValue::Null => 4,
    }
}

/// Compare two present, non-null property values.
fn compare_present(a: &PropertyValue, b: &PropertyValue) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        // Integer and Float are one numeric domain: which one a value decodes
        // to depends on whether the source wrote a decimal point, and that
        // must not affect ordering.
        (PropertyValue::Integer(_) | PropertyValue::Float(_), _)
            if matches!(b, PropertyValue::Integer(_) | PropertyValue::Float(_)) =>
        {
            match (a.as_f64(), b.as_f64()) {
                // NaN can't participate in an ordering; treat as equal so the
                // comparator stays consistent and the id tie-break decides.
                (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
                _ => Ordering::Equal,
            }
        }
        (PropertyValue::String(x), PropertyValue::String(y)) => x.cmp(y),
        (PropertyValue::Bool(x), PropertyValue::Bool(y)) => x.cmp(y),
        // Lexicographic, not by length: same-length lists with different
        // contents would otherwise compare Equal and fall through to the
        // tie-break, which reads as "sorting silently did nothing" — the
        // failure this module is trying to eliminate. No engine advertises a
        // List sortable today, but this is the shared helper they all reuse.
        (PropertyValue::List(x), PropertyValue::List(y)) => x
            .iter()
            .zip(y.iter())
            .map(|(a, b)| compare_present(a, b))
            .find(|o| *o != Ordering::Equal)
            .unwrap_or_else(|| x.len().cmp(&y.len())),
        _ => type_rank(a).cmp(&type_rank(b)),
    }
}

/// Sort features by `sortby`, in place.
///
/// Two behaviours worth knowing, both deliberate:
///
/// - **Missing and null values sort last in BOTH directions.** A descending
///   sort on `significance` must not put the cells that have no score at the
///   top; "unknown" is not "highest". This means the direction flip applies
///   only to values that are actually present.
/// - **Ties break on feature id**, so paging over a sorted result set is
///   stable — without it, two equal-scoring features can swap places between
///   requests and a client paging through them would skip one and see the
///   other twice.
pub fn sort_features(features: &mut [Feature], sortby: &[SortKey]) {
    use std::cmp::Ordering;
    if sortby.is_empty() {
        return;
    }
    features.sort_by(|a, b| {
        for key in sortby {
            let av = a.properties.get(&key.property);
            let bv = b.properties.get(&key.property);
            let a_absent = matches!(av, None | Some(PropertyValue::Null));
            let b_absent = matches!(bv, None | Some(PropertyValue::Null));
            let ord = match (a_absent, b_absent) {
                (true, true) => Ordering::Equal,
                // Not reversed for descending: absent always sinks.
                (true, false) => return Ordering::Greater,
                (false, true) => return Ordering::Less,
                (false, false) => {
                    let o = compare_present(av.expect("present"), bv.expect("present"));
                    match key.direction {
                        SortDirection::Ascending => o,
                        SortDirection::Descending => o.reverse(),
                    }
                }
            };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        a.id.cmp(&b.id)
    });
}

impl Default for FeatureQuery {
    fn default() -> Self {
        Self {
            bbox: None,
            limit: 100,
            offset: 0,
            datetime: None,
            sortby: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bbox_valid() {
        let bbox = Bbox::new(24.0, 60.0, 25.0, 61.0).unwrap();
        assert!(bbox.contains(24.5, 60.5));
        assert!(!bbox.contains(23.0, 60.5));
    }

    #[test]
    fn bbox_rejects_nan() {
        assert!(Bbox::new(f64::NAN, 60.0, 25.0, 61.0).is_err());
    }

    #[test]
    fn bbox_rejects_infinity() {
        assert!(Bbox::new(f64::INFINITY, 60.0, 25.0, 61.0).is_err());
    }

    #[test]
    fn bbox_antimeridian_crossing() {
        // west > east = antimeridian-crossing bbox (e.g., Russia to Alaska)
        let bbox = Bbox::new(170.0, -10.0, -170.0, 10.0).unwrap();
        assert!(bbox.crosses_antimeridian());
        assert!(bbox.contains(175.0, 0.0)); // east of antimeridian
        assert!(bbox.contains(-175.0, 0.0)); // west of antimeridian
        assert!(!bbox.contains(0.0, 0.0)); // in the gap
    }

    #[test]
    fn bbox_antimeridian_intersects() {
        let bbox = Bbox::new(170.0, -10.0, -170.0, 10.0).unwrap();
        // Feature bbox near antimeridian should intersect
        assert!(bbox.intersects_bbox(&[175.0, -5.0, 179.0, 5.0]));
        assert!(bbox.intersects_bbox(&[-179.0, -5.0, -175.0, 5.0]));
        // Feature bbox in the gap should not
        assert!(!bbox.intersects_bbox(&[0.0, -5.0, 10.0, 5.0]));
    }

    #[test]
    fn bbox_rejects_reversed_lat() {
        assert!(Bbox::new(24.0, 61.0, 25.0, 60.0).is_err());
    }

    #[test]
    fn bbox_rejects_out_of_range() {
        assert!(Bbox::new(-200.0, 60.0, 25.0, 61.0).is_err());
        assert!(Bbox::new(24.0, -100.0, 25.0, 61.0).is_err());
    }

    #[test]
    fn null_geometry_bbox_is_none() {
        assert!(Geometry::Null.bbox().is_none());
    }

    #[test]
    fn null_geometry_centroid_is_none() {
        assert!(Geometry::Null.centroid().is_none());
    }

    #[test]
    fn point_geometry_bbox() {
        let g = Geometry::Point { x: 24.0, y: 60.0 };
        assert_eq!(g.bbox(), Some([24.0, 60.0, 24.0, 60.0]));
    }

    // --- Point-in-polygon tests ---

    #[test]
    fn point_in_simple_square() {
        let ring = vec![
            [0.0, 0.0],
            [10.0, 0.0],
            [10.0, 10.0],
            [0.0, 10.0],
            [0.0, 0.0],
        ];
        assert!(point_in_ring(5.0, 5.0, &ring));
        assert!(!point_in_ring(15.0, 5.0, &ring));
        assert!(!point_in_ring(5.0, -1.0, &ring));
    }

    #[test]
    fn point_in_triangle() {
        let ring = vec![[0.0, 0.0], [10.0, 0.0], [5.0, 10.0], [0.0, 0.0]];
        assert!(point_in_ring(5.0, 3.0, &ring));
        assert!(!point_in_ring(1.0, 9.0, &ring)); // outside the triangle
    }

    #[test]
    fn query_polygon_with_hole() {
        let poly = QueryPolygon {
            exterior: vec![
                [0.0, 0.0],
                [20.0, 0.0],
                [20.0, 20.0],
                [0.0, 20.0],
                [0.0, 0.0],
            ],
            holes: vec![vec![
                [5.0, 5.0],
                [15.0, 5.0],
                [15.0, 15.0],
                [5.0, 15.0],
                [5.0, 5.0],
            ]],
            bbox: Bbox::new(0.0, 0.0, 20.0, 20.0).unwrap(),
        };
        assert!(poly.contains(2.0, 2.0)); // inside exterior, outside hole
        assert!(!poly.contains(10.0, 10.0)); // inside hole
        assert!(!poly.contains(25.0, 10.0)); // outside bbox
    }

    // --- WKT parsing tests ---

    #[test]
    fn parse_polygon_wkt() {
        let poly = parse_area_coords("POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))").unwrap();
        assert_eq!(poly.exterior.len(), 5);
        assert!(poly.holes.is_empty());
        assert!(poly.contains(5.0, 5.0));
        assert!(!poly.contains(15.0, 5.0));
    }

    #[test]
    fn parse_polygon_with_hole_wkt() {
        let poly = parse_area_coords(
            "POLYGON((0 0, 20 0, 20 20, 0 20, 0 0),(5 5, 15 5, 15 15, 5 15, 5 5))",
        )
        .unwrap();
        assert_eq!(poly.exterior.len(), 5);
        assert_eq!(poly.holes.len(), 1);
        assert!(poly.contains(2.0, 2.0));
        assert!(!poly.contains(10.0, 10.0)); // in the hole
    }

    #[test]
    fn parse_bbox_format() {
        let poly = parse_area_coords("0,0,10,10").unwrap();
        assert_eq!(poly.exterior.len(), 5); // rectangular polygon
        assert!(poly.contains(5.0, 5.0));
        assert!(!poly.contains(15.0, 5.0));
    }

    #[test]
    fn parse_rejects_too_long() {
        let long_str = "x".repeat(MAX_WKT_LENGTH + 1);
        assert!(parse_area_coords(&long_str).is_err());
    }

    #[test]
    fn parse_rejects_too_few_points() {
        assert!(parse_area_coords("POLYGON((0 0, 1 1))").is_err());
    }

    #[test]
    fn parse_rejects_out_of_range_coords() {
        assert!(parse_area_coords("POLYGON((0 0, 200 0, 200 10, 0 10, 0 0))").is_err());
    }

    #[test]
    fn parse_point_coords_accepts_wkt_and_bare_pair() {
        // WKT, with and without the PROJ-style space before `(`.
        assert_eq!(
            parse_point_coords("POINT(10.5 56.0)").unwrap(),
            (56.0, 10.5)
        );
        assert_eq!(
            parse_point_coords("POINT (10.5 56.0)").unwrap(),
            (56.0, 10.5)
        );
        // Bare `lon,lat` shorthand, surrounding whitespace tolerated.
        assert_eq!(parse_point_coords("10.5, 56.0").unwrap(), (56.0, 10.5));
        assert_eq!(parse_point_coords("  -3.2,48.7 ").unwrap(), (48.7, -3.2));
    }

    #[test]
    fn parse_point_coords_rejects_malformed_and_out_of_range() {
        assert!(parse_point_coords("POINT(10.5)").is_err());
        assert!(parse_point_coords("10.5").is_err());
        assert!(parse_point_coords("a,b").is_err());
        assert!(parse_point_coords("10.5,56.0,3").is_err());
        // Out-of-range so a transposed lat,lon pair fails loudly.
        assert!(parse_point_coords("200.0, 10.0").is_err());
        assert!(parse_point_coords("POINT(10 91)").is_err());
        // Non-finite.
        assert!(parse_point_coords("NaN, 10.0").is_err());
        assert!(parse_point_coords("inf, 10.0").is_err());
    }

    #[test]
    fn parse_linestring_accepts_two_or_more_nodes() {
        let nodes = parse_linestring_coords("LINESTRING(10 50, 11 51)").unwrap();
        assert_eq!(nodes, vec![(10.0, 50.0), (11.0, 51.0)]);

        let nodes =
            parse_linestring_coords("linestring ( 24.94 60.17 , 25.5 60.5 , 26.0 61.0 )").unwrap();
        assert_eq!(nodes.len(), 3);
        assert!((nodes[2].0 - 26.0).abs() < 1e-9);
    }

    #[test]
    fn parse_linestring_rejects_z_and_m_variants() {
        for s in [
            "LINESTRINGZ(10 50 0, 11 51 100)",
            "LINESTRINGM(10 50 0, 11 51 1)",
            "LINESTRINGZM(10 50 0 1, 11 51 100 2)",
        ] {
            let err = parse_linestring_coords(s).unwrap_err();
            assert!(
                format!("{err:?}").contains("not supported"),
                "expected Z/M-rejected error, got {err:?}"
            );
        }
    }

    #[test]
    fn parse_linestring_rejects_single_node_and_malformed() {
        assert!(parse_linestring_coords("LINESTRING(10 50)").is_err());
        assert!(parse_linestring_coords("LINESTRING(10, 50)").is_err());
        assert!(parse_linestring_coords("POINT(10 50)").is_err());
        assert!(parse_linestring_coords("LINESTRING(200 50, 11 51)").is_err());
        assert!(parse_linestring_coords("LINESTRING(NaN 50, 11 51)").is_err());
    }

    /// A multibyte UTF-8 character straddling the prefix length byte must
    /// not panic. Caught by claude-review on PR #275 — `&str[..N]` is
    /// byte-indexed, so a `LINESTRINGé(...)` input would crash the
    /// handler with `byte index … is not a char boundary`.
    #[test]
    fn parse_linestring_handles_multibyte_prefix_collision() {
        // `é` is 2 bytes, landing at byte index 10 — exactly where the
        // length check would slice a `LINESTRINGZ` prefix.
        let res = parse_linestring_coords("LINESTRINGé(10 50, 11 51)");
        assert!(res.is_err(), "non-LINESTRING prefix must error, not panic");
        // And the prefix check itself must not crash on this input.
        assert!(parse_linestring_coords("LINESTRING\u{1F600}(0 0, 1 1)").is_err());
    }

    /// `LINESTRING(24 60, 24 60)` is a zero-length path — every along-path
    /// node maps to the same `(t, lon, lat)` tuple, producing a degenerate
    /// `Section` coverage. Reject early so the caller knows.
    /// A LINESTRING payload longer than `MAX_WKT_LENGTH` is rejected
    /// without allocating one `(f64, f64)` per comma. Mirrors
    /// `parse_rejects_too_long` for polygons; caught by claude-review.
    #[test]
    fn parse_linestring_rejects_too_long_payload() {
        // ~30 KB of valid LINESTRING content. Length-cap fires before
        // any per-node parsing — i.e. this completes instantly.
        let mut s = String::from("LINESTRING(");
        for i in 0..5_000 {
            if i > 0 {
                s.push(',');
            }
            s.push_str("0 0");
        }
        s.push(')');
        assert!(s.len() > MAX_WKT_LENGTH);
        assert!(parse_linestring_coords(&s).is_err());
    }

    #[test]
    fn parse_linestring_rejects_all_identical_nodes() {
        let err = parse_linestring_coords("LINESTRING(24 60, 24 60)").unwrap_err();
        assert!(format!("{err:?}").contains("identical"));
        // Three identical nodes — same outcome.
        assert!(parse_linestring_coords("LINESTRING(1 2, 1 2, 1 2)").is_err());
        // A LINESTRING with *some* duplicates but a non-zero length is
        // accepted — the resample step handles the geometry fine.
        assert!(parse_linestring_coords("LINESTRING(1 2, 1 2, 2 3)").is_ok());
    }

    /// Square (0,0)-(10,10) with a square hole (4,4)-(6,6).
    fn holed() -> Geometry {
        Geometry::Polygon {
            exterior: vec![
                [0.0, 0.0],
                [10.0, 0.0],
                [10.0, 10.0],
                [0.0, 10.0],
                [0.0, 0.0],
            ],
            holes: vec![vec![
                [4.0, 4.0],
                [6.0, 4.0],
                [6.0, 6.0],
                [4.0, 6.0],
                [4.0, 4.0],
            ]],
        }
    }

    #[test]
    fn geometry_contains_respects_holes() {
        let g = holed();
        assert!(g.contains(1.0, 1.0), "inside the exterior");
        assert!(
            !g.contains(5.0, 5.0),
            "inside the hole is outside the polygon"
        );
        assert!(!g.contains(20.0, 20.0), "outside entirely");
    }

    #[test]
    fn geometry_contains_handles_multipolygon_and_degenerate_cases() {
        let multi = Geometry::MultiPolygon {
            polygons: vec![
                (
                    vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0], [0.0, 0.0]],
                    vec![],
                ),
                (
                    vec![[5.0, 5.0], [6.0, 5.0], [6.0, 6.0], [5.0, 6.0], [5.0, 5.0]],
                    vec![],
                ),
            ],
        };
        assert!(multi.contains(0.5, 0.5));
        assert!(multi.contains(5.5, 5.5), "any member polygon counts");
        assert!(!multi.contains(3.0, 3.0), "the gap between them is outside");

        // A Point contains nothing: an exact float match would be meaningless.
        assert!(!Geometry::Point { x: 1.0, y: 2.0 }.contains(1.0, 2.0));
        assert!(!Geometry::Null.contains(0.0, 0.0));
    }

    #[test]
    fn geometry_contains_agrees_with_query_polygon() {
        // The two containment paths must never disagree about "inside".
        let g = holed();
        let Geometry::Polygon { exterior, holes } = g.clone() else {
            unreachable!()
        };
        let bbox = Bbox::new(0.0, 0.0, 10.0, 10.0).unwrap();
        let q = QueryPolygon {
            exterior,
            holes,
            bbox,
        };
        for (x, y) in [(1.0, 1.0), (5.0, 5.0), (9.9, 9.9), (-1.0, 5.0), (4.5, 5.5)] {
            assert_eq!(
                g.contains(x, y),
                q.contains(x, y),
                "disagreement at ({x}, {y})"
            );
        }
    }

    #[test]
    fn property_accessors_are_strict_about_types() {
        assert_eq!(
            PropertyValue::String("Vantaa".into()).as_str(),
            Some("Vantaa")
        );
        assert_eq!(
            PropertyValue::Integer(5).as_str(),
            None,
            "no stringifying numbers"
        );
        assert_eq!(PropertyValue::Null.as_str(), None);

        // Both numeric variants read as f64: which one a JSON property
        // decodes to depends on whether the source wrote a decimal point.
        assert_eq!(PropertyValue::Integer(694_392).as_f64(), Some(694_392.0));
        assert_eq!(PropertyValue::Float(1.5).as_f64(), Some(1.5));
        assert_eq!(
            PropertyValue::String("5".into()).as_f64(),
            None,
            "no parsing strings"
        );
        assert_eq!(PropertyValue::Bool(true).as_f64(), None);
    }

    fn feat(id: &str, props: Vec<(&str, PropertyValue)>) -> Feature {
        Feature {
            id: id.into(),
            geometry: Arc::new(Geometry::Null),
            properties: Arc::new(props.into_iter().map(|(k, v)| (k.to_string(), v)).collect()),
        }
    }

    fn ids(fs: &[Feature]) -> Vec<&str> {
        fs.iter().map(|f| f.id.as_str()).collect()
    }

    #[test]
    fn sorts_ascending_and_descending() {
        let mut fs = vec![
            feat("b", vec![("score", PropertyValue::Float(0.5))]),
            feat("a", vec![("score", PropertyValue::Float(0.9))]),
            feat("c", vec![("score", PropertyValue::Float(0.1))]),
        ];
        sort_features(&mut fs, &[SortKey::ascending("score")]);
        assert_eq!(ids(&fs), ["c", "b", "a"]);
        sort_features(&mut fs, &[SortKey::descending("score")]);
        assert_eq!(ids(&fs), ["a", "b", "c"]);
    }

    #[test]
    fn missing_and_null_sort_last_in_both_directions() {
        // The operational case: "top cells by significance, descending" must
        // not surface the ones with no score. Unknown is not highest.
        let mut fs = vec![
            feat("null", vec![("score", PropertyValue::Null)]),
            feat("high", vec![("score", PropertyValue::Float(0.9))]),
            feat("absent", vec![]),
            feat("low", vec![("score", PropertyValue::Float(0.1))]),
        ];
        sort_features(&mut fs, &[SortKey::descending("score")]);
        assert_eq!(ids(&fs)[..2], ["high", "low"]);
        assert!(ids(&fs)[2..].contains(&"null") && ids(&fs)[2..].contains(&"absent"));

        sort_features(&mut fs, &[SortKey::ascending("score")]);
        assert_eq!(ids(&fs)[..2], ["low", "high"]);
        assert!(ids(&fs)[2..].contains(&"null") && ids(&fs)[2..].contains(&"absent"));
    }

    #[test]
    fn integer_and_float_share_one_numeric_order() {
        // Which variant a JSON property decodes to depends on whether the
        // source wrote a decimal point; it must not affect ordering.
        let mut fs = vec![
            feat("f", vec![("n", PropertyValue::Float(2.5))]),
            feat("i", vec![("n", PropertyValue::Integer(2))]),
            feat("g", vec![("n", PropertyValue::Float(10.0))]),
        ];
        sort_features(&mut fs, &[SortKey::ascending("n")]);
        assert_eq!(ids(&fs), ["i", "f", "g"]);
    }

    #[test]
    fn ties_break_on_id_so_paging_is_stable() {
        let mut fs = vec![
            feat("c", vec![("n", PropertyValue::Integer(1))]),
            feat("a", vec![("n", PropertyValue::Integer(1))]),
            feat("b", vec![("n", PropertyValue::Integer(1))]),
        ];
        sort_features(&mut fs, &[SortKey::descending("n")]);
        assert_eq!(
            ids(&fs),
            ["a", "b", "c"],
            "equal keys must order by id, not arbitrarily"
        );
        // Idempotent: re-sorting cannot reshuffle, or paging skips/duplicates.
        let first = ids(&fs).join(",");
        sort_features(&mut fs, &[SortKey::descending("n")]);
        assert_eq!(ids(&fs).join(","), first);
    }

    #[test]
    fn multi_key_precedence_is_left_to_right() {
        let mut fs = vec![
            feat(
                "x",
                vec![
                    ("a", PropertyValue::Integer(1)),
                    ("b", PropertyValue::Integer(2)),
                ],
            ),
            feat(
                "y",
                vec![
                    ("a", PropertyValue::Integer(1)),
                    ("b", PropertyValue::Integer(1)),
                ],
            ),
            feat(
                "z",
                vec![
                    ("a", PropertyValue::Integer(0)),
                    ("b", PropertyValue::Integer(9)),
                ],
            ),
        ];
        sort_features(
            &mut fs,
            &[SortKey::ascending("a"), SortKey::descending("b")],
        );
        assert_eq!(ids(&fs), ["z", "x", "y"]);
    }

    #[test]
    fn mixed_types_yield_a_total_order_not_a_panic() {
        // An inconsistent comparator makes sort_by misbehave, so every pair
        // must compare deterministically even when types differ.
        let mut fs = vec![
            feat("s", vec![("v", PropertyValue::String("a".into()))]),
            feat("n", vec![("v", PropertyValue::Integer(1))]),
            feat("b", vec![("v", PropertyValue::Bool(true))]),
            feat("l", vec![("v", PropertyValue::List(vec![]))]),
        ];
        sort_features(&mut fs, &[SortKey::ascending("v")]);
        assert_eq!(ids(&fs), ["b", "n", "s", "l"]);
    }

    #[test]
    fn nan_does_not_destabilize_the_comparator() {
        let mut fs = vec![
            feat("nan", vec![("n", PropertyValue::Float(f64::NAN))]),
            feat("one", vec![("n", PropertyValue::Float(1.0))]),
        ];
        sort_features(&mut fs, &[SortKey::ascending("n")]);
        assert_eq!(fs.len(), 2);
    }

    #[test]
    fn lists_compare_lexicographically_not_by_length() {
        // Same length, different contents must not collapse to Equal — that
        // reads as "sorting did nothing".
        let mut fs = vec![
            feat(
                "b",
                vec![(
                    "l",
                    PropertyValue::List(vec![PropertyValue::Integer(1), PropertyValue::Integer(9)]),
                )],
            ),
            feat(
                "a",
                vec![(
                    "l",
                    PropertyValue::List(vec![PropertyValue::Integer(1), PropertyValue::Integer(2)]),
                )],
            ),
        ];
        sort_features(&mut fs, &[SortKey::ascending("l")]);
        assert_eq!(ids(&fs), ["a", "b"]);
        // A shorter prefix still orders before its extension.
        let mut fs = vec![
            feat(
                "long",
                vec![(
                    "l",
                    PropertyValue::List(vec![PropertyValue::Integer(1), PropertyValue::Integer(1)]),
                )],
            ),
            feat(
                "short",
                vec![("l", PropertyValue::List(vec![PropertyValue::Integer(1)]))],
            ),
        ];
        sort_features(&mut fs, &[SortKey::ascending("l")]);
        assert_eq!(ids(&fs), ["short", "long"]);
    }

    #[test]
    fn empty_sortby_leaves_order_untouched() {
        let mut fs = vec![
            feat("c", vec![("n", PropertyValue::Integer(1))]),
            feat("a", vec![("n", PropertyValue::Integer(2))]),
        ];
        sort_features(&mut fs, &[]);
        assert_eq!(ids(&fs), ["c", "a"]);
    }

    #[test]
    fn radius_polygon_contains_inside_and_excludes_outside() {
        let wkt = radius_polygon_wkt(24.9384, 60.1699, 10_000.0).unwrap();
        let poly = parse_area_coords(&wkt).unwrap();
        assert_eq!(poly.exterior.len(), RADIUS_POLYGON_VERTICES + 1);
        // Every vertex sits on the circle (spherical distance == radius).
        for [x, y] in &poly.exterior {
            let d = crate::geo::great_circle_distance_m(24.9384, 60.1699, *x, *y);
            assert!((d - 10_000.0).abs() < 1.0, "vertex at {d} m");
        }
        // Centre and a point at 0.9 r along an off-axis bearing are inside;
        // 1.1 r is outside — in every direction, not just along the axes.
        assert!(poly.contains(24.9384, 60.1699));
        for bearing in [17.0, 100.0, 203.0, 311.0] {
            let (xi, yi) = crate::geo::destination_point(24.9384, 60.1699, 9_000.0, bearing);
            let (xo, yo) = crate::geo::destination_point(24.9384, 60.1699, 11_000.0, bearing);
            assert!(poly.contains(xi, yi), "0.9 r at {bearing}° must be inside");
            assert!(
                !poly.contains(xo, yo),
                "1.1 r at {bearing}° must be outside"
            );
        }
    }

    #[test]
    fn antimeridian_polygon_contains_points_on_both_sides_of_the_seam() {
        let poly = parse_area_coords("170,10,-170,20").unwrap();
        assert!(poly.contains(175.0, 15.0));
        assert!(poly.contains(-175.0, 15.0));
        assert!(poly.contains(180.0, 15.0));
        assert!(!poly.contains(0.0, 15.0));
        assert!(!poly.contains(160.0, 15.0));
        assert!(!poly.contains(175.0, 25.0));
    }

    #[test]
    fn sample_grid_crosses_the_antimeridian() {
        let poly = parse_area_coords("170,10,-170,20").unwrap();
        assert!(poly.bbox.crosses_antimeridian());
        let axes = poly.sample_grid(5.0, 5.0, 256);
        assert_eq!(axes.x, vec![172.5, 177.5, -177.5, -172.5]);
        assert_eq!(axes.y, vec![17.5, 12.5]);
        let mask = poly.cell_mask(&axes);
        assert!(mask.iter().all(|&m| m), "every centre lies in the bbox");
        // Coarsened to one cell: its centre is the seam itself.
        let one = poly.sample_grid(0.0, 0.0, 256);
        assert_eq!(one.x, vec![180.0]);
        assert!(poly.cell_mask(&one)[0]);
    }

    #[test]
    fn cell_mask_falls_back_to_vertex_cells_for_sub_cell_shapes() {
        // A ring (square with a square hole) whose bbox is one 1° cell and
        // whose bbox centre (10.5, 50.5) is inside the hole.
        let poly = parse_area_coords(
            "POLYGON((10 50, 11 50, 11 51, 10 51, 10 50),(10.2 50.2, 10.8 50.2, 10.8 50.8, 10.2 50.8, 10.2 50.2))",
        )
        .unwrap();
        let axes = poly.sample_grid(1.0, 1.0, 256);
        assert_eq!(axes.dims(), (1, 1));
        assert!(!poly.contains(axes.x[0], axes.y[0]));
        assert_eq!(poly.cell_mask(&axes), vec![true]);
        // At native resolution the centre test decides and no fallback fires.
        let fine = poly.sample_grid(0.1, 0.1, 256);
        let mask = poly.cell_mask(&fine);
        assert!(mask.iter().any(|&m| m) && !mask.iter().all(|&m| m));
    }

    #[test]
    fn area_budget_rejects_over_limit() {
        assert!(check_area_budget(4, 256, 256, 3).is_ok());
        assert!(matches!(
            check_area_budget(4, 256, 256, 4),
            Err(DataServerError::QueryTooLarge(_))
        ));
        assert!(
            check_area_budget(usize::MAX, 2, 2, 2).is_err(),
            "no overflow"
        );
    }

    #[test]
    fn sample_grid_axes_match_resolution_and_orientation() {
        let poly = parse_area_coords("POLYGON((10 50, 12 50, 12 51, 10 51, 10 50))").unwrap();
        let axes = poly.sample_grid(0.5, 0.25, 256);
        assert_eq!(axes.x, vec![10.25, 10.75, 11.25, 11.75]);
        assert_eq!(axes.y, vec![50.875, 50.625, 50.375, 50.125]);
        // Coarsened, never refused, when the bbox exceeds max_dim cells.
        let coarse = poly.sample_grid(0.001, 0.001, 4);
        assert_eq!((coarse.x.len(), coarse.y.len()), (4, 4));
        // Degenerate resolution → one cell at the bbox centre.
        let one = poly.sample_grid(0.0, f64::NAN, 256);
        assert_eq!((one.x, one.y), (vec![11.0], vec![50.5]));
    }

    #[test]
    fn radius_polygon_rejects_degenerate_input() {
        assert!(radius_polygon_wkt(25.0, 60.0, 0.0).is_err());
        assert!(radius_polygon_wkt(25.0, 60.0, -5.0).is_err());
        assert!(radius_polygon_wkt(25.0, 60.0, f64::NAN).is_err());
        assert!(radius_polygon_wkt(200.0, 60.0, 1000.0).is_err());
        // Contains the pole.
        assert!(radius_polygon_wkt(25.0, 89.5, 100_000.0).is_err());
        // Just short of the pole (inside the ~1 km margin) is rejected too;
        // 89.5° + 0.495° = 89.995°.
        assert!(radius_polygon_wkt(25.0, 89.5, 55_050.0).is_err());
        // Comfortably short of it is accepted and the ring stays sane.
        let wkt = radius_polygon_wkt(25.0, 89.5, 50_000.0).unwrap();
        let poly = parse_area_coords(&wkt).unwrap();
        assert!(poly.contains(25.0, 89.5));
        // Crosses the antimeridian.
        assert!(radius_polygon_wkt(179.9, 0.0, 50_000.0).is_err());
        // Same circle away from the seam is fine.
        assert!(radius_polygon_wkt(170.0, 0.0, 50_000.0).is_ok());
    }
}
