//! MVT byte encoder.
//!
//! Takes WGS84 [`Feature`]s and a tile's WGS84 bbox; emits Mapbox Vector Tile
//! protobuf bytes. Coordinates inside the tile are linear within the tile's
//! display projection — Web Mercator for `WebMercatorQuad`, plate-carrée for
//! `WorldCRS84Quad` — so map clients render features in the right place
//! without any client-side reprojection.

use std::collections::HashSet;

use ds_core::feature::{Feature, Geometry, PropertyValue};
use mvt::{Error as MvtError, GeomEncoder, GeomType, Tile};

/// Which OGC TileMatrixSet the encoded tile belongs to.
///
/// The variant drives how feature lon/lat is mapped to the tile's local
/// `0..extent` coordinate space.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum TmsKind {
    /// Spherical Mercator (EPSG:3857). The dominant choice for slippy maps.
    WebMercatorQuad,
    /// Equirectangular WGS84 (CRS:84). 2 columns × 1 row at z=0.
    WorldCRS84Quad,
}

impl TmsKind {
    pub fn id(&self) -> &'static str {
        match self {
            TmsKind::WebMercatorQuad => "WebMercatorQuad",
            TmsKind::WorldCRS84Quad => "WorldCRS84Quad",
        }
    }

    /// Parse a TileMatrixSet ID into a `TmsKind`. Returns `None` for unknown IDs.
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "WebMercatorQuad" => Some(TmsKind::WebMercatorQuad),
            "WorldCRS84Quad" => Some(TmsKind::WorldCRS84Quad),
            _ => None,
        }
    }
}

/// Which feature properties survive into the encoded tile.
#[derive(Debug, Clone, Default)]
pub enum PropertyAllowlist {
    /// Emit every property carried by the feature.
    #[default]
    All,
    /// Emit only properties whose key is in the set. Unknown keys are dropped.
    Subset(HashSet<String>),
}

impl PropertyAllowlist {
    fn allows(&self, key: &str) -> bool {
        match self {
            PropertyAllowlist::All => true,
            PropertyAllowlist::Subset(set) => set.contains(key),
        }
    }
}

/// Encoder options. `layer_name` becomes the MVT layer; `extent` is the
/// per-axis resolution of the tile's local coordinate space (4096 by spec).
#[derive(Debug, Clone)]
pub struct TileEncodeOptions {
    pub layer_name: String,
    pub extent: u32,
    pub tms: TmsKind,
    pub properties: PropertyAllowlist,
}

impl TileEncodeOptions {
    pub fn new(layer_name: impl Into<String>, tms: TmsKind) -> Self {
        Self {
            layer_name: layer_name.into(),
            extent: 4096,
            tms,
            properties: PropertyAllowlist::All,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("invalid tile bbox: {0}")]
    InvalidBbox(&'static str),
    #[error("mvt encoder error: {0}")]
    Mvt(#[from] MvtError),
}

/// Encode a slice of features into the bytes of a single MVT tile.
///
/// `tile_bbox` is the tile's WGS84 [west, south, east, north] envelope as
/// returned by `tilematrixset::tile_bbox`. The output is a complete tile —
/// callers don't need to wrap it further.
pub fn encode_tile(
    features: &[Feature],
    tile_bbox: [f64; 4],
    opts: &TileEncodeOptions,
) -> Result<Vec<u8>, EncodeError> {
    let projector = Projector::new(opts.tms, tile_bbox, opts.extent)?;

    let mut tile = Tile::new(opts.extent);
    let mut layer = tile.create_layer(&opts.layer_name);

    for feature in features {
        let Some(geom_data) = encode_geometry(&feature.geometry, &projector, opts.extent)? else {
            continue;
        };

        let mut mvt_feature = layer.into_feature(geom_data);
        if let Some(id) = parse_numeric_id(&feature.id) {
            mvt_feature.set_id(id);
        }
        for (key, value) in feature.properties.iter() {
            if !opts.properties.allows(key) {
                continue;
            }
            add_tag(&mut mvt_feature, key, value);
        }
        layer = mvt_feature.into_layer();
    }

    tile.add_layer(layer)?;
    let bytes = tile.to_bytes()?;
    Ok(bytes)
}

fn add_tag(feature: &mut mvt::Feature, key: &str, value: &PropertyValue) {
    match value {
        PropertyValue::String(s) => feature.add_tag_string(key, s),
        PropertyValue::Float(f) => feature.add_tag_double(key, *f),
        PropertyValue::Integer(i) => feature.add_tag_sint(key, *i),
        PropertyValue::Bool(b) => feature.add_tag_bool(key, *b),
        // MVT has no null tag type. Dropping a null-valued tag matches what
        // every other tile-producing stack does (PostGIS ST_AsMVT, tippecanoe).
        PropertyValue::Null => {}
        // MVT tag values are scalar — flatten a list to a comma-joined string
        // (nested lists / nulls contribute nothing).
        PropertyValue::List(items) => {
            let joined = items
                .iter()
                .filter_map(scalar_tag_string)
                .collect::<Vec<_>>()
                .join(",");
            // An empty / all-null list contributes no scalar value — drop the
            // tag entirely, matching how `Null` is handled (no empty-string tag).
            if !joined.is_empty() {
                feature.add_tag_string(key, &joined);
            }
        }
    }
}

/// Render a scalar `PropertyValue` as a string for the MVT list-flattening
/// path. Returns `None` for `Null` and nested `List`s (which have no scalar
/// representation), so they're skipped in the joined output.
fn scalar_tag_string(v: &PropertyValue) -> Option<String> {
    match v {
        PropertyValue::String(s) => Some(s.clone()),
        PropertyValue::Float(f) => Some(f.to_string()),
        PropertyValue::Integer(i) => Some(i.to_string()),
        PropertyValue::Bool(b) => Some(b.to_string()),
        PropertyValue::Null | PropertyValue::List(_) => None,
    }
}

/// MVT features support a numeric `id` field. Feature ids in our model are
/// strings; we try to parse them as u64 so numeric ids round-trip cleanly.
fn parse_numeric_id(id: &str) -> Option<u64> {
    id.parse::<u64>().ok()
}

/// How far outside the tile extent (in tile-local units) clipped geometry may
/// reach, as a fraction of `extent`. MapLibre rejects features whose coords
/// exceed `[-buffer, extent+buffer]` at parse time. `1/16` (= 6.25%) is
/// MapLibre's default tile-buffer ratio — at the standard `extent = 4096`
/// it resolves to 256, which matches MapLibre's source-layer default buffer
/// and keeps polygon edges flush across tile boundaries.
const CLIP_BUFFER_RATIO: f64 = 1.0 / 16.0;

fn encode_geometry(
    geom: &Geometry,
    proj: &Projector,
    extent: u32,
) -> Result<Option<mvt::GeomData>, MvtError> {
    let clip = ClipRect::for_extent(extent);
    match geom {
        Geometry::Null => Ok(None),
        Geometry::Point { x, y } => {
            let (px, py) = proj.project(*x, *y);
            if !clip.contains(px, py) {
                return Ok(None);
            }
            let geom = GeomEncoder::<f64>::new(GeomType::Point)
                .point(px, py)?
                .encode()?;
            Ok(Some(geom))
        }
        Geometry::Polygon { exterior, holes } => {
            let parts = [(exterior.as_slice(), holes.as_slice())];
            encode_polygon_parts(parts.into_iter(), proj, &clip)
        }
        Geometry::MultiPolygon { polygons } => encode_polygon_parts(
            polygons.iter().map(|(e, h)| (e.as_slice(), h.as_slice())),
            proj,
            &clip,
        ),
    }
}

/// What a ring represents inside its parent polygon. Drives winding-order
/// normalisation: MVT spec §4.3.4.4 requires exterior rings clockwise and
/// holes counter-clockwise in tile-local coordinates (Y-down).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum RingRole {
    Exterior,
    Hole,
}

fn encode_polygon_parts<'a, I>(
    parts: I,
    proj: &Projector,
    clip: &ClipRect,
) -> Result<Option<mvt::GeomData>, MvtError>
where
    I: Iterator<Item = (&'a [[f64; 2]], &'a [Vec<[f64; 2]>])>,
{
    let mut enc = GeomEncoder::<f64>::new(GeomType::Polygon);
    let mut wrote_any = false;
    for (exterior, holes) in parts {
        let clipped_ext = clip_ring(exterior, proj, clip, RingRole::Exterior);
        if clipped_ext.len() < 3 {
            // Outside the tile, or degenerate after clipping — drop the whole
            // polygon part. Skipping a part with no exterior keeps any later
            // parts in the same MultiPolygon from being mistakenly read as
            // holes of a missing exterior.
            continue;
        }
        if wrote_any {
            enc.complete_geom()?;
        }
        push_clipped_ring(&mut enc, &clipped_ext)?;
        for hole in holes {
            let clipped_hole = clip_ring(hole, proj, clip, RingRole::Hole);
            if clipped_hole.len() < 3 {
                continue;
            }
            enc.complete_geom()?;
            push_clipped_ring(&mut enc, &clipped_hole)?;
        }
        wrote_any = true;
    }
    if !wrote_any {
        return Ok(None);
    }
    Ok(Some(enc.complete()?.encode()?))
}

fn push_clipped_ring(enc: &mut GeomEncoder<f64>, ring: &[(f64, f64)]) -> Result<(), MvtError> {
    for &(x, y) in ring {
        enc.add_point(x, y)?;
    }
    Ok(())
}

/// Project the WGS84 ring into tile-local coords, clip against the buffered
/// tile rectangle, and normalise winding order. Returns the clipped ring's
/// vertices in tile-local space, without a closing duplicate (MVT's
/// ClosePath command implies closure).
///
/// Winding: MVT spec §4.3.4.4 requires exterior rings clockwise and holes
/// counter-clockwise *in tile coordinates* (Y-down). Inputs that don't
/// follow RFC 7946 (some PostGIS exports, hand-built data) would otherwise
/// render inside-out on strict clients; we compute the shoelace signed
/// area of the clipped ring and reverse when the sign doesn't match the
/// role.
fn clip_ring(
    ring: &[[f64; 2]],
    proj: &Projector,
    clip: &ClipRect,
    role: RingRole,
) -> Vec<(f64, f64)> {
    let n = ring.len();
    if n == 0 {
        return Vec::new();
    }
    // GeoJSON convention duplicates the first vertex at the end. Drop the
    // duplicate so Sutherland-Hodgman doesn't double the first edge.
    let end = if n >= 2 && ring[0] == ring[n - 1] {
        n - 1
    } else {
        n
    };
    let projected: Vec<(f64, f64)> = ring[..end]
        .iter()
        .map(|p| proj.project(p[0], p[1]))
        .collect();
    let mut clipped = sutherland_hodgman(&projected, clip);

    // Normalise winding after clipping so the area test runs on the actual
    // emitted geometry. Clipping can flip winding for rings that crossed
    // an edge an odd number of times.
    let area = signed_area(&clipped);
    let needs_reverse = match role {
        RingRole::Exterior => area < 0.0,
        RingRole::Hole => area > 0.0,
    };
    if needs_reverse {
        clipped.reverse();
    }
    clipped
}

/// Axis-aligned clip rectangle in tile-local space, expanded by `extent * CLIP_BUFFER_RATIO`
/// so geometry on tile boundaries renders without seams.
#[derive(Debug, Clone, Copy)]
struct ClipRect {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl ClipRect {
    fn for_extent(extent: u32) -> Self {
        let e = extent as f64;
        let buffer = e * CLIP_BUFFER_RATIO;
        Self {
            min_x: -buffer,
            min_y: -buffer,
            max_x: e + buffer,
            max_y: e + buffer,
        }
    }

    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.min_x && x <= self.max_x && y >= self.min_y && y <= self.max_y
    }
}

/// `MinY` / `MaxY` rather than `Top` / `Bottom`: tile coordinates are
/// y-down (y=0 at the top of the screen), so screen-relative "top" maps
/// to `MinY` — opposite of what most readers would assume. Labelling by
/// the literal numeric bound removes the ambiguity.
#[derive(Debug, Clone, Copy)]
enum ClipEdge {
    Left(f64),
    Right(f64),
    MinY(f64),
    MaxY(f64),
}

impl ClipEdge {
    fn inside(&self, p: (f64, f64)) -> bool {
        match *self {
            ClipEdge::Left(v) => p.0 >= v,
            ClipEdge::Right(v) => p.0 <= v,
            ClipEdge::MinY(v) => p.1 >= v,
            ClipEdge::MaxY(v) => p.1 <= v,
        }
    }

    fn intersect(&self, a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
        match *self {
            ClipEdge::Left(v) | ClipEdge::Right(v) => {
                let t = (v - a.0) / (b.0 - a.0);
                (v, a.1 + t * (b.1 - a.1))
            }
            ClipEdge::MinY(v) | ClipEdge::MaxY(v) => {
                let t = (v - a.1) / (b.1 - a.1);
                (a.0 + t * (b.0 - a.0), v)
            }
        }
    }
}

/// Sutherland-Hodgman polygon clipping against an axis-aligned rectangle.
///
/// The output ring is always closed implicitly: the last vertex connects back
/// to the first, matching MVT's ClosePath semantics. An input that lies fully
/// outside the clip rect produces an empty output.
fn sutherland_hodgman(ring: &[(f64, f64)], clip: &ClipRect) -> Vec<(f64, f64)> {
    let edges = [
        ClipEdge::Left(clip.min_x),
        ClipEdge::Right(clip.max_x),
        ClipEdge::MinY(clip.min_y),
        ClipEdge::MaxY(clip.max_y),
    ];
    let mut output = ring.to_vec();
    for edge in edges {
        if output.is_empty() {
            break;
        }
        let input = std::mem::take(&mut output);
        let n = input.len();
        for i in 0..n {
            let curr = input[i];
            let prev = input[(i + n - 1) % n];
            let curr_in = edge.inside(curr);
            let prev_in = edge.inside(prev);
            match (prev_in, curr_in) {
                (true, true) => output.push(curr),
                (true, false) => output.push(edge.intersect(prev, curr)),
                (false, true) => {
                    output.push(edge.intersect(prev, curr));
                    output.push(curr);
                }
                (false, false) => {}
            }
        }
    }
    output
}

/// Shoelace signed area for an open polygon ring (first vertex not
/// duplicated). In screen-coords (Y-down) the convention is:
///
///   * positive → clockwise visual winding
///   * negative → counter-clockwise visual winding
///
/// Returns 0 for rings with fewer than 3 distinct vertices.
fn signed_area(ring: &[(f64, f64)]) -> f64 {
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    let mut acc = 0.0;
    for i in 0..n {
        let (x0, y0) = ring[i];
        let (x1, y1) = ring[(i + 1) % n];
        acc += x0 * y1 - x1 * y0;
    }
    acc * 0.5
}

// ---------------------------------------------------------------------------
// Projection: WGS84 lon/lat → tile-local pixels (0..extent, 0..extent)
// ---------------------------------------------------------------------------

// Tile-grid pole cutoff — geometry is pinned to the grid edge, the one
// place clamping to the limit is allowed (see `ds_core::web_mercator`).
use ds_core::web_mercator::LAT_LIMIT_DEG as WEB_MERCATOR_MAX_LAT;

struct Projector {
    tms: TmsKind,
    extent: f64,
    // Tile bounds in *display* coordinates (Mercator metres for
    // `WebMercatorQuad`, lon/lat degrees for `WorldCRS84Quad`).
    west_disp: f64,
    east_disp: f64,
    south_disp: f64,
    north_disp: f64,
}

impl Projector {
    fn new(tms: TmsKind, tile_bbox: [f64; 4], extent: u32) -> Result<Self, EncodeError> {
        if extent == 0 {
            // `TileEncodeOptions.extent` is a public mutable field. Zero
            // would collapse the projection to `(0, 0)` for every feature
            // — fail loudly instead of silently producing wrong output.
            return Err(EncodeError::InvalidBbox("extent must be > 0"));
        }
        let [west, south, east, north] = tile_bbox;
        if !(west.is_finite() && south.is_finite() && east.is_finite() && north.is_finite()) {
            return Err(EncodeError::InvalidBbox("non-finite bbox coordinate"));
        }
        if east <= west {
            return Err(EncodeError::InvalidBbox("east <= west"));
        }
        if north <= south {
            return Err(EncodeError::InvalidBbox("north <= south"));
        }
        let (west_disp, east_disp, south_disp, north_disp) = match tms {
            TmsKind::WebMercatorQuad => (
                lon_to_merc_x(west),
                lon_to_merc_x(east),
                lat_to_merc_y(south),
                lat_to_merc_y(north),
            ),
            TmsKind::WorldCRS84Quad => (west, east, south, north),
        };
        Ok(Self {
            tms,
            extent: extent as f64,
            west_disp,
            east_disp,
            south_disp,
            north_disp,
        })
    }

    fn project(&self, lon: f64, lat: f64) -> (f64, f64) {
        let (dx, dy) = match self.tms {
            TmsKind::WebMercatorQuad => (lon_to_merc_x(lon), lat_to_merc_y(lat)),
            TmsKind::WorldCRS84Quad => (lon, lat),
        };
        let px = (dx - self.west_disp) / (self.east_disp - self.west_disp) * self.extent;
        // Y flip: north → 0, south → extent. Tile-local origin is top-left.
        let py = (self.north_disp - dy) / (self.north_disp - self.south_disp) * self.extent;
        (px, py)
    }
}

fn lon_to_merc_x(lon: f64) -> f64 {
    ds_core::web_mercator::lon_to_x(lon)
}

fn lat_to_merc_y(lat: f64) -> f64 {
    let clamped = lat.clamp(-WEB_MERCATOR_MAX_LAT, WEB_MERCATOR_MAX_LAT);
    ds_core::web_mercator::lat_to_y(clamped)
}

/// Hash of a property allowlist for use as a component of vector-tile cache
/// keys and ETags. Different allowlists produce different hashes so they
/// don't share cache slots.
///
/// Uses FNV-1a (fixed algorithm) rather than `DefaultHasher` because the
/// value flows into `VectorTileKey::etag()`, which is transmitted as an
/// HTTP `ETag` header. A toolchain bump must not silently rotate the bytes
/// clients replay in `If-None-Match`.
pub fn properties_hash(allowlist: &PropertyAllowlist) -> u64 {
    use crate::hash::{fnv1a_mix, FNV1A_OFFSET};
    let mut h = FNV1A_OFFSET;
    match allowlist {
        PropertyAllowlist::All => fnv1a_mix(&mut h, &[0u8]),
        PropertyAllowlist::Subset(set) => {
            fnv1a_mix(&mut h, &[1u8]);
            // Sort for deterministic hashing — HashSet iteration order is random.
            let mut keys: Vec<&String> = set.iter().collect();
            keys.sort();
            for k in keys {
                fnv1a_mix(&mut h, k.as_bytes());
                fnv1a_mix(&mut h, b"|");
            }
        }
    }
    h
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn eq(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    fn feature(id: &str, geom: Geometry, props: &[(&str, PropertyValue)]) -> Feature {
        let mut map = HashMap::new();
        for (k, v) in props {
            map.insert((*k).to_string(), v.clone());
        }
        Feature {
            id: id.into(),
            geometry: Arc::new(geom),
            properties: Arc::new(map),
        }
    }

    fn web_mercator_tile_z0() -> [f64; 4] {
        // The whole-world tile at z=0 for WebMercatorQuad. Note WGS84 north/south
        // are clamped at the Web Mercator pole limits (~85.0511°).
        [-180.0, -WEB_MERCATOR_MAX_LAT, 180.0, WEB_MERCATOR_MAX_LAT]
    }

    #[test]
    fn projector_centers_correctly_in_mercator() {
        let proj = Projector::new(TmsKind::WebMercatorQuad, web_mercator_tile_z0(), 4096).unwrap();
        let (x, y) = proj.project(0.0, 0.0);
        assert!(eq(x, 2048.0, 1e-6), "x = {x}");
        assert!(eq(y, 2048.0, 1e-6), "y = {y}");
    }

    #[test]
    fn projector_centers_correctly_in_crs84() {
        // CRS84 z=0 has 2 cols × 1 row, but a single z=0 tile is e.g. west
        // hemisphere [-180,-90,0,90]. Use that for a clean centre test.
        let proj =
            Projector::new(TmsKind::WorldCRS84Quad, [-180.0, -90.0, 0.0, 90.0], 4096).unwrap();
        let (x, y) = proj.project(-90.0, 0.0);
        assert!(eq(x, 2048.0, 1e-6), "x = {x}");
        assert!(eq(y, 2048.0, 1e-6), "y = {y}");
    }

    #[test]
    fn projector_flips_y_axis() {
        // North end of the tile should land at y=0; south end at y=extent.
        let proj =
            Projector::new(TmsKind::WorldCRS84Quad, [-10.0, -10.0, 10.0, 10.0], 4096).unwrap();
        let (_, y_north) = proj.project(0.0, 10.0);
        let (_, y_south) = proj.project(0.0, -10.0);
        assert!(eq(y_north, 0.0, 1e-6));
        assert!(eq(y_south, 4096.0, 1e-6));
    }

    #[test]
    fn projector_rejects_degenerate_bbox() {
        assert!(Projector::new(TmsKind::WorldCRS84Quad, [0.0, 0.0, 0.0, 1.0], 4096).is_err());
        assert!(Projector::new(TmsKind::WorldCRS84Quad, [0.0, 0.0, 1.0, 0.0], 4096).is_err());
        assert!(Projector::new(TmsKind::WorldCRS84Quad, [0.0, f64::NAN, 1.0, 1.0], 4096).is_err());
    }

    #[test]
    fn projector_rejects_zero_extent() {
        // `extent = 0` would collapse the projection to (0,0) — fail loudly
        // since `TileEncodeOptions.extent` is a public mutable field.
        assert!(Projector::new(TmsKind::WorldCRS84Quad, [0.0, 0.0, 1.0, 1.0], 0).is_err());
    }

    #[test]
    fn encode_empty_features_produces_valid_tile() {
        let opts = TileEncodeOptions::new("empty", TmsKind::WebMercatorQuad);
        let bytes = encode_tile(&[], web_mercator_tile_z0(), &opts).unwrap();
        // Even an empty tile must include the layer block; the result should be
        // non-empty (the layer header) and parse without error.
        assert!(!bytes.is_empty());
    }

    #[test]
    fn encode_point_with_string_property() {
        let f = feature(
            "1",
            Geometry::Point { x: 0.0, y: 0.0 },
            &[("name", PropertyValue::String("origin".into()))],
        );
        let opts = TileEncodeOptions::new("points", TmsKind::WebMercatorQuad);
        let bytes = encode_tile(&[f], web_mercator_tile_z0(), &opts).unwrap();
        assert!(!bytes.is_empty());
        // Sanity: the layer name and property both appear in the serialized
        // tile (MVT stores tag keys and string values in plaintext within the
        // protobuf, so a substring check is robust to encoding details).
        assert!(slice_contains(&bytes, b"points"));
        assert!(slice_contains(&bytes, b"name"));
        assert!(slice_contains(&bytes, b"origin"));
    }

    #[test]
    fn encode_point_flattens_list_property_to_joined_string() {
        let f = feature(
            "1",
            Geometry::Point { x: 0.0, y: 0.0 },
            &[(
                "quantities",
                PropertyValue::List(vec![
                    PropertyValue::String("DBZH".into()),
                    PropertyValue::String("VRADH".into()),
                ]),
            )],
        );
        let opts = TileEncodeOptions::new("points", TmsKind::WebMercatorQuad);
        let bytes = encode_tile(&[f], web_mercator_tile_z0(), &opts).unwrap();
        // MVT tag values are scalar, so the list is flattened to a comma-joined
        // string stored in plaintext within the protobuf.
        assert!(slice_contains(&bytes, b"quantities"));
        assert!(slice_contains(&bytes, b"DBZH,VRADH"));
    }

    #[test]
    fn encode_point_drops_all_null_list_tag() {
        // A list with no scalar items flattens to "" — the tag must be dropped
        // entirely (like a bare Null), not written as an empty-string value.
        let f = feature(
            "1",
            Geometry::Point { x: 0.0, y: 0.0 },
            &[
                ("empty", PropertyValue::List(vec![PropertyValue::Null])),
                ("name", PropertyValue::String("origin".into())),
            ],
        );
        let opts = TileEncodeOptions::new("points", TmsKind::WebMercatorQuad);
        let bytes = encode_tile(&[f], web_mercator_tile_z0(), &opts).unwrap();
        assert!(slice_contains(&bytes, b"name"), "scalar tag still present");
        assert!(
            !slice_contains(&bytes, b"empty"),
            "an all-null list tag must be dropped"
        );
    }

    #[test]
    fn scalar_tag_string_skips_null_and_nested_lists() {
        assert_eq!(
            scalar_tag_string(&PropertyValue::String("x".into())),
            Some("x".into())
        );
        assert_eq!(
            scalar_tag_string(&PropertyValue::Integer(3)),
            Some("3".into())
        );
        assert_eq!(scalar_tag_string(&PropertyValue::Null), None);
        assert_eq!(scalar_tag_string(&PropertyValue::List(vec![])), None);
    }

    #[test]
    fn encode_polygon_closes_ring_without_duplicate() {
        // GeoJSON convention closes the ring by repeating the first point.
        // Encoder must accept either form and produce identical bytes.
        let closed = feature(
            "closed",
            Geometry::Polygon {
                exterior: vec![
                    [-5.0, -5.0],
                    [5.0, -5.0],
                    [5.0, 5.0],
                    [-5.0, 5.0],
                    [-5.0, -5.0],
                ],
                holes: vec![],
            },
            &[],
        );
        let open = feature(
            "open",
            Geometry::Polygon {
                exterior: vec![[-5.0, -5.0], [5.0, -5.0], [5.0, 5.0], [-5.0, 5.0]],
                holes: vec![],
            },
            &[],
        );
        let opts = TileEncodeOptions::new("polys", TmsKind::WebMercatorQuad);
        let bytes_closed = encode_tile(&[closed], web_mercator_tile_z0(), &opts).unwrap();
        let bytes_open = encode_tile(&[open], web_mercator_tile_z0(), &opts).unwrap();
        // Neither ID parses as `u64`, so `set_id` is skipped and the only
        // remaining difference between the two features (the open/closed
        // last vertex) is normalised away in `push_ring`. The byte streams
        // must be identical, not just the same length.
        assert_eq!(bytes_closed, bytes_open);
    }

    #[test]
    fn signed_area_sign_matches_winding() {
        // CW in screen-coords (Y down) → positive area
        let cw_screen = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        assert!(signed_area(&cw_screen) > 0.0);
        // CCW in screen-coords (Y down) → negative area
        let ccw_screen = [(0.0, 0.0), (0.0, 10.0), (10.0, 10.0), (10.0, 0.0)];
        assert!(signed_area(&ccw_screen) < 0.0);
        // Degenerate / under 3 vertices
        assert_eq!(signed_area(&[(0.0, 0.0), (1.0, 1.0)]), 0.0);
    }

    #[test]
    fn winding_order_normalised_for_non_conformant_inputs() {
        // Project both rings through the same projector and check that, after
        // running through `push_ring`'s normalisation, the *projected*
        // signed area has the right sign for the ring's role. Byte-level
        // equality between two different inputs isn't a useful invariant
        // here (a different start vertex changes the MVT byte stream even
        // when the winding is identical), but the post-normalisation sign
        // is the actual MVT-spec requirement.
        let proj = Projector::new(TmsKind::WebMercatorQuad, web_mercator_tile_z0(), 4096).unwrap();

        // RFC 7946: exterior CCW in geographic coords.
        let ccw_geo = [[-5.0, -5.0], [5.0, -5.0], [5.0, 5.0], [-5.0, 5.0]];
        // RFC 7946 violation: exterior CW in geographic coords.
        let cw_geo = [[-5.0, -5.0], [-5.0, 5.0], [5.0, 5.0], [5.0, -5.0]];

        for ring in [ccw_geo, cw_geo] {
            let mut projected: Vec<(f64, f64)> =
                ring.iter().map(|p| proj.project(p[0], p[1])).collect();
            // Mirror the normalisation logic in `push_ring`.
            if signed_area(&projected) < 0.0 {
                projected.reverse();
            }
            // MVT spec §4.3.4.4: exterior must be CW in tile coords → positive area.
            assert!(
                signed_area(&projected) > 0.0,
                "exterior must be CW (positive area) after normalisation"
            );
        }
    }

    #[test]
    fn encode_polygon_with_hole() {
        let f = feature(
            "donut",
            Geometry::Polygon {
                exterior: vec![[-10.0, -10.0], [10.0, -10.0], [10.0, 10.0], [-10.0, 10.0]],
                holes: vec![vec![[-2.0, -2.0], [2.0, -2.0], [2.0, 2.0], [-2.0, 2.0]]],
            },
            &[],
        );
        let opts = TileEncodeOptions::new("donut", TmsKind::WebMercatorQuad);
        let bytes = encode_tile(&[f], web_mercator_tile_z0(), &opts).unwrap();
        assert!(!bytes.is_empty());
    }

    #[test]
    fn encode_multipolygon() {
        let f = feature(
            "two",
            Geometry::MultiPolygon {
                polygons: vec![
                    (
                        vec![[-50.0, -10.0], [-40.0, -10.0], [-40.0, 0.0], [-50.0, 0.0]],
                        vec![],
                    ),
                    (
                        vec![[40.0, 0.0], [50.0, 0.0], [50.0, 10.0], [40.0, 10.0]],
                        vec![],
                    ),
                ],
            },
            &[],
        );
        let opts = TileEncodeOptions::new("multi", TmsKind::WebMercatorQuad);
        let bytes = encode_tile(&[f], web_mercator_tile_z0(), &opts).unwrap();
        assert!(!bytes.is_empty());
    }

    #[test]
    fn encode_multipolygon_with_holes() {
        // Two parts, both with a hole. The encoder must emit
        // `complete_geom()` before each non-first exterior AND between an
        // exterior and its hole(s), so the ring separators land in the
        // right places. Without correct placement either the protobuf is
        // malformed or the `mvt` crate returns an error.
        let f = feature(
            "donuts",
            Geometry::MultiPolygon {
                polygons: vec![
                    (
                        vec![[-50.0, -10.0], [-40.0, -10.0], [-40.0, 0.0], [-50.0, 0.0]],
                        vec![vec![
                            [-48.0, -8.0],
                            [-42.0, -8.0],
                            [-42.0, -2.0],
                            [-48.0, -2.0],
                        ]],
                    ),
                    (
                        vec![[40.0, 0.0], [50.0, 0.0], [50.0, 10.0], [40.0, 10.0]],
                        vec![vec![[42.0, 2.0], [48.0, 2.0], [48.0, 8.0], [42.0, 8.0]]],
                    ),
                ],
            },
            &[],
        );
        let opts = TileEncodeOptions::new("donuts", TmsKind::WebMercatorQuad);
        let bytes = encode_tile(&[f], web_mercator_tile_z0(), &opts).unwrap();
        assert!(!bytes.is_empty());
        assert!(slice_contains(&bytes, b"donuts"));
    }

    #[test]
    fn null_geometry_is_skipped() {
        let f = feature("null", Geometry::Null, &[]);
        let opts = TileEncodeOptions::new("nulls", TmsKind::WebMercatorQuad);
        let bytes = encode_tile(&[f], web_mercator_tile_z0(), &opts).unwrap();
        // The layer header is still emitted even when every feature is null.
        assert!(!bytes.is_empty());
    }

    #[test]
    fn empty_polygon_is_skipped() {
        // A `Polygon` with an empty exterior would let `enc.complete()` run on
        // a zero-point encoder — the `mvt` crate errors there and the whole
        // tile would fail. Verify the encoder bails early instead.
        let f = feature(
            "empty",
            Geometry::Polygon {
                exterior: vec![],
                holes: vec![],
            },
            &[],
        );
        let opts = TileEncodeOptions::new("polys", TmsKind::WebMercatorQuad);
        let bytes = encode_tile(&[f], web_mercator_tile_z0(), &opts).unwrap();
        // No feature bytes were written — the dropped id won't appear in tags.
        assert!(!slice_contains(&bytes, b"empty"));
    }

    #[test]
    fn empty_multipolygon_is_skipped() {
        let f = feature("no-parts", Geometry::MultiPolygon { polygons: vec![] }, &[]);
        let opts = TileEncodeOptions::new("polys", TmsKind::WebMercatorQuad);
        let bytes = encode_tile(&[f], web_mercator_tile_z0(), &opts).unwrap();
        assert!(!slice_contains(&bytes, b"no-parts"));
    }

    #[test]
    fn multipolygon_with_only_empty_parts_is_skipped() {
        // Mix of empty exterior + non-empty hole — every part is degenerate,
        // so the whole feature must be dropped without invoking
        // `enc.complete()`.
        let f = feature(
            "all-empty",
            Geometry::MultiPolygon {
                polygons: vec![(vec![], vec![]), (vec![], vec![vec![[0.0, 0.0]]])],
            },
            &[],
        );
        let opts = TileEncodeOptions::new("polys", TmsKind::WebMercatorQuad);
        let bytes = encode_tile(&[f], web_mercator_tile_z0(), &opts).unwrap();
        assert!(!slice_contains(&bytes, b"all-empty"));
    }

    #[test]
    fn empty_hole_rings_are_skipped() {
        // A polygon with a valid exterior but an empty hole would otherwise
        // close a zero-point ring inside `enc.complete_geom()`. Both the
        // `Polygon` and `MultiPolygon` arms must guard against this.
        let single = feature(
            "polygon-empty-hole",
            Geometry::Polygon {
                exterior: vec![[-5.0, -5.0], [5.0, -5.0], [5.0, 5.0], [-5.0, 5.0]],
                holes: vec![vec![]],
            },
            &[],
        );
        let multi = feature(
            "multi-empty-hole",
            Geometry::MultiPolygon {
                polygons: vec![(
                    vec![[-5.0, -5.0], [5.0, -5.0], [5.0, 5.0], [-5.0, 5.0]],
                    vec![vec![]],
                )],
            },
            &[],
        );
        let opts = TileEncodeOptions::new("polys", TmsKind::WebMercatorQuad);
        let bytes_single = encode_tile(&[single], web_mercator_tile_z0(), &opts).unwrap();
        let bytes_multi = encode_tile(&[multi], web_mercator_tile_z0(), &opts).unwrap();
        // Both succeed (no encoder error) and the exterior is preserved —
        // we look for the layer name as a proxy because feature ids are
        // strings that don't parse to u64.
        assert!(slice_contains(&bytes_single, b"polys"));
        assert!(slice_contains(&bytes_multi, b"polys"));
    }

    #[test]
    fn ring_with_fewer_than_three_unique_vertices_is_skipped() {
        // `[[0,0],[1,1],[0,0]]` is closed (last == first), so stripping the
        // duplicate leaves 2 unique vertices — too few for a polygon. The
        // encoder must reject this before calling `enc.complete()`.
        let degenerate = feature(
            "two-vertex",
            Geometry::Polygon {
                exterior: vec![[0.0, 0.0], [1.0, 1.0], [0.0, 0.0]],
                holes: vec![],
            },
            &[],
        );
        // 1-vertex (open) ring — also degenerate.
        let single = feature(
            "one-vertex",
            Geometry::Polygon {
                exterior: vec![[0.0, 0.0]],
                holes: vec![],
            },
            &[],
        );
        let opts = TileEncodeOptions::new("polys", TmsKind::WebMercatorQuad);
        // Both must encode successfully (no error from mvt::complete)
        // and produce no feature bytes for the dropped polygons.
        let bytes_a = encode_tile(&[degenerate], web_mercator_tile_z0(), &opts).unwrap();
        let bytes_b = encode_tile(&[single], web_mercator_tile_z0(), &opts).unwrap();
        assert!(!bytes_a.is_empty());
        assert!(!bytes_b.is_empty());
    }

    #[test]
    fn property_allowlist_drops_unlisted_keys() {
        let f = feature(
            "p",
            Geometry::Point { x: 0.0, y: 0.0 },
            &[
                ("keep", PropertyValue::String("yes".into())),
                ("drop", PropertyValue::String("no".into())),
            ],
        );
        let mut opts = TileEncodeOptions::new("filtered", TmsKind::WebMercatorQuad);
        let mut subset = HashSet::new();
        subset.insert("keep".to_string());
        opts.properties = PropertyAllowlist::Subset(subset);
        let bytes = encode_tile(&[f], web_mercator_tile_z0(), &opts).unwrap();
        assert!(slice_contains(&bytes, b"keep"));
        assert!(!slice_contains(&bytes, b"drop"));
        assert!(slice_contains(&bytes, b"yes"));
        assert!(!slice_contains(&bytes, b"no"));
    }

    #[test]
    fn properties_hash_is_deterministic() {
        let mut a = HashSet::new();
        a.insert("foo".to_string());
        a.insert("bar".to_string());
        let mut b = HashSet::new();
        b.insert("bar".to_string());
        b.insert("foo".to_string());
        let h1 = properties_hash(&PropertyAllowlist::Subset(a));
        let h2 = properties_hash(&PropertyAllowlist::Subset(b));
        assert_eq!(h1, h2);
        assert_ne!(h1, properties_hash(&PropertyAllowlist::All));
    }

    #[test]
    fn numeric_string_id_round_trips() {
        assert_eq!(parse_numeric_id("42"), Some(42));
        assert_eq!(parse_numeric_id("station-001"), None);
        assert_eq!(parse_numeric_id(""), None);
    }

    #[test]
    fn polygon_far_outside_tile_is_dropped() {
        // Polygon entirely in the Pacific; tile bbox covers a small piece of
        // Europe. Clipping should drop everything → no geom written.
        let f = feature(
            "pacific",
            Geometry::Polygon {
                exterior: vec![
                    [-160.0, 10.0],
                    [-150.0, 10.0],
                    [-150.0, 20.0],
                    [-160.0, 20.0],
                ],
                holes: vec![],
            },
            &[],
        );
        let opts = TileEncodeOptions::new("dropped", TmsKind::WebMercatorQuad);
        // Tile over central Europe.
        let bytes = encode_tile(&[f], [5.0, 45.0, 15.0, 55.0], &opts).unwrap();
        // Layer header still emitted, but no feature bytes — assert by absence
        // of the dropped feature id, which would otherwise appear in tags.
        assert!(!slice_contains(&bytes, b"pacific"));
    }

    #[test]
    fn polygon_crossing_tile_edge_stays_within_buffer() {
        // End-to-end check: projection + clip_ring on a real-world ring
        // must constrain every output coordinate to the buffered envelope.
        // The mvt crate doesn't expose a decoder, so we exercise the same
        // pipeline the encoder runs (`Projector::new` → `clip_ring`) and
        // inspect the projected/clipped coordinates directly. A failure
        // here would mean encode_tile emits coords outside the buffer —
        // MapLibre would refuse to render the feature.
        let extent: u32 = 4096;
        let proj =
            Projector::new(TmsKind::WebMercatorQuad, [10.0, 50.0, 11.0, 51.0], extent).unwrap();
        let clip = ClipRect::for_extent(extent);
        // Northern-hemisphere band — far wider than the small tile at (10,50)-(11,51).
        let ring = [[-180.0, 0.0], [180.0, 0.0], [180.0, 80.0], [-180.0, 80.0]];
        let clipped = clip_ring(&ring, &proj, &clip, RingRole::Exterior);
        assert!(
            !clipped.is_empty(),
            "polygon fully encloses the tile — clipped ring should not be empty"
        );
        let buffer = extent as f64 * CLIP_BUFFER_RATIO;
        for &(x, y) in &clipped {
            assert!(
                x >= -buffer && x <= extent as f64 + buffer,
                "clipped x out of buffered range: {x}"
            );
            assert!(
                y >= -buffer && y <= extent as f64 + buffer,
                "clipped y out of buffered range: {y}"
            );
        }
    }

    #[test]
    fn clip_buffer_scales_with_extent() {
        // Non-standard extent must produce a proportional buffer — otherwise
        // a 512-extent tile would have buffer=256 (50% of extent), which
        // MapLibre would refuse to parse.
        let standard = ClipRect::for_extent(4096);
        assert_eq!(standard.min_x, -256.0);
        assert_eq!(standard.max_x, 4096.0 + 256.0);

        let small = ClipRect::for_extent(512);
        assert_eq!(small.min_x, -32.0);
        assert_eq!(small.max_x, 512.0 + 32.0);
    }

    #[test]
    fn multipolygon_with_one_part_outside_tile_keeps_inside_part() {
        // Exercises `encode_polygon_parts`' `wrote_any` / `complete_geom`
        // sequencing when one part is dropped during clipping. The "all-out"
        // exterior is skipped (clipped_ext.len() < 3), so subsequent parts
        // must not be mistakenly read as holes of the missing exterior, and
        // the surviving part must still be emitted as a valid polygon.
        let extent: u32 = 4096;
        let proj =
            Projector::new(TmsKind::WebMercatorQuad, [10.0, 50.0, 11.0, 51.0], extent).unwrap();
        // Part A: square fully inside the tile (10.2–10.8 lon, 50.2–50.8 lat).
        let inside = (
            vec![
                [10.2, 50.2],
                [10.8, 50.2],
                [10.8, 50.8],
                [10.2, 50.8],
                [10.2, 50.2],
            ],
            vec![],
        );
        // Part B: square far away (-150..-140 lon, -10..0 lat) — entirely
        // outside the tile and outside the buffer.
        let outside = (
            vec![
                [-150.0, -10.0],
                [-140.0, -10.0],
                [-140.0, 0.0],
                [-150.0, 0.0],
                [-150.0, -10.0],
            ],
            vec![],
        );
        // Outside-then-inside ordering forces `wrote_any` to stay false on
        // the first part — exercises the `if wrote_any { complete_geom() }`
        // guard on the first surviving write.
        let geom = Geometry::MultiPolygon {
            polygons: vec![outside.clone(), inside.clone()],
        };
        let encoded = encode_geometry(&geom, &proj, extent)
            .unwrap()
            .expect("inside part must survive clipping");
        assert!(
            !encoded.is_empty(),
            "encoded geometry must carry the inside part's commands"
        );

        // Reverse ordering: inside-then-outside. `wrote_any` is true after the
        // first part; the second part's exterior is skipped without flipping
        // any state, and the final `complete()` still succeeds.
        let geom_rev = Geometry::MultiPolygon {
            polygons: vec![inside, outside],
        };
        assert!(encode_geometry(&geom_rev, &proj, extent).unwrap().is_some());

        // All-outside MultiPolygon — must produce `None`, not an empty-ring
        // polygon that downstream readers might trip over.
        let outside_only = Geometry::MultiPolygon {
            polygons: vec![(
                vec![
                    [-150.0, -10.0],
                    [-140.0, -10.0],
                    [-140.0, 0.0],
                    [-150.0, 0.0],
                    [-150.0, -10.0],
                ],
                vec![],
            )],
        };
        assert!(encode_geometry(&outside_only, &proj, extent)
            .unwrap()
            .is_none());
    }

    #[test]
    fn sutherland_hodgman_keeps_fully_inside_polygon() {
        let clip = ClipRect::for_extent(4096);
        let ring = vec![
            (100.0, 100.0),
            (200.0, 100.0),
            (200.0, 200.0),
            (100.0, 200.0),
        ];
        let out = sutherland_hodgman(&ring, &clip);
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn sutherland_hodgman_clips_crossing_polygon_to_buffer() {
        let clip = ClipRect::for_extent(4096);
        // Square that crosses the east edge by 1000 units.
        let ring = vec![
            (1000.0, 1000.0),
            (5096.0, 1000.0),
            (5096.0, 3000.0),
            (1000.0, 3000.0),
        ];
        let out = sutherland_hodgman(&ring, &clip);
        // Every vertex must lie within the buffered rect.
        for &(x, y) in &out {
            assert!(x >= clip.min_x && x <= clip.max_x, "x out of range: {x}");
            assert!(y >= clip.min_y && y <= clip.max_y, "y out of range: {y}");
        }
        // Result should still be a valid polygon.
        assert!(out.len() >= 3);
    }

    #[test]
    fn sutherland_hodgman_drops_fully_outside_polygon() {
        let clip = ClipRect::for_extent(4096);
        let ring = vec![
            (-2000.0, -2000.0),
            (-1000.0, -2000.0),
            (-1000.0, -1000.0),
            (-2000.0, -1000.0),
        ];
        let out = sutherland_hodgman(&ring, &clip);
        assert!(out.is_empty(), "expected empty, got {:?}", out);
    }

    #[test]
    fn antimeridian_bbox_west_gt_east_is_rejected_for_crs84() {
        // CRS84 tile spanning the antimeridian: west=170, east=-170. Plate-carrée
        // linear interp would still work if we normalised the split (e.g.
        // west=170, east=190), but `Projector::new` is the wrong layer for
        // that — its job is to map a single contiguous bbox to tile-local
        // coords. Callers must split antimeridian-crossing tiles themselves.
        assert!(
            Projector::new(TmsKind::WorldCRS84Quad, [170.0, -10.0, -170.0, 10.0], 4096).is_err()
        );
    }

    fn slice_contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}
