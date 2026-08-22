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
        if !point_in_ring(x, y, &self.exterior) {
            return false;
        }
        for hole in &self.holes {
            if point_in_ring(x, y, hole) {
                return false;
            }
        }
        true
    }
}

/// Ray-casting point-in-polygon test for a single ring.
fn point_in_ring(x: f64, y: f64, ring: &[[f64; 2]]) -> bool {
    let n = ring.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = (ring[i][0], ring[i][1]);
        let (xj, yj) = (ring[j][0], ring[j][1]);
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
}
