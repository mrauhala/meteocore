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
        let Some(geom_data) = encode_geometry(&feature.geometry, &projector)? else {
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
    }
}

/// MVT features support a numeric `id` field. Feature ids in our model are
/// strings; we try to parse them as u64 so numeric ids round-trip cleanly.
fn parse_numeric_id(id: &str) -> Option<u64> {
    id.parse::<u64>().ok()
}

fn encode_geometry(geom: &Geometry, proj: &Projector) -> Result<Option<mvt::GeomData>, MvtError> {
    match geom {
        Geometry::Null => Ok(None),
        Geometry::Point { x, y } => {
            let (px, py) = proj.project(*x, *y);
            let geom = GeomEncoder::<f64>::new(GeomType::Point)
                .point(px, py)?
                .encode()?;
            Ok(Some(geom))
        }
        Geometry::Polygon { exterior, holes } => {
            // A ring with fewer than three unique vertices can't form a
            // valid polygon (MVT §4.3.4.4 requires ≥3). Letting it through
            // would emit a zero/one/two-point ring inside `enc.complete()`,
            // which the `mvt` crate either errors on or stores as
            // degenerate protobuf. Skip the feature.
            if ring_unique_count(exterior) < 3 {
                return Ok(None);
            }
            let mut enc = GeomEncoder::<f64>::new(GeomType::Polygon);
            push_ring(&mut enc, exterior, proj, RingRole::Exterior)?;
            for hole in holes {
                if ring_unique_count(hole) < 3 {
                    continue;
                }
                enc.complete_geom()?;
                push_ring(&mut enc, hole, proj, RingRole::Hole)?;
            }
            Ok(Some(enc.complete()?.encode()?))
        }
        Geometry::MultiPolygon { polygons } => {
            // A `MultiPolygon` with zero valid parts hits the same trap.
            if polygons.is_empty() {
                return Ok(None);
            }
            let mut enc = GeomEncoder::<f64>::new(GeomType::Polygon);
            let mut first = true;
            for (exterior, holes) in polygons {
                if ring_unique_count(exterior) < 3 {
                    continue;
                }
                if !first {
                    enc.complete_geom()?;
                }
                push_ring(&mut enc, exterior, proj, RingRole::Exterior)?;
                for hole in holes {
                    if ring_unique_count(hole) < 3 {
                        continue;
                    }
                    enc.complete_geom()?;
                    push_ring(&mut enc, hole, proj, RingRole::Hole)?;
                }
                first = false;
            }
            // All parts had degenerate exteriors → no points emitted.
            if first {
                return Ok(None);
            }
            Ok(Some(enc.complete()?.encode()?))
        }
    }
}

/// How many distinct vertices a ring contributes, ignoring the GeoJSON
/// convention of repeating the first vertex at the end. A ring with fewer
/// than 3 distinct vertices can't form a valid polygon.
fn ring_unique_count(ring: &[[f64; 2]]) -> usize {
    let n = ring.len();
    if n >= 2 && ring[0] == ring[n - 1] {
        n - 1
    } else {
        n
    }
}

/// What a ring represents inside its parent polygon. Used by `push_ring`
/// to drive winding-order normalisation: MVT spec §4.3.4.4 requires
/// exterior rings clockwise and holes counter-clockwise in tile-local
/// coordinates (which have a flipped Y compared to RFC 7946 geographic).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum RingRole {
    Exterior,
    Hole,
}

fn push_ring(
    enc: &mut GeomEncoder<f64>,
    ring: &[[f64; 2]],
    proj: &Projector,
    role: RingRole,
) -> Result<(), MvtError> {
    // GeoJSON convention duplicates the first vertex at the end to close the
    // ring; MVT's ClosePath command implies closure, so emit the duplicate
    // only if the caller didn't supply one.
    let n = ring.len();
    let end = if n >= 2 && ring[0] == ring[n - 1] {
        n - 1
    } else {
        n
    };

    // Project once into tile-local coords so we can inspect winding before
    // emitting. Y is already flipped (north → 0, south → extent) inside
    // `Projector::project`.
    let mut projected: Vec<(f64, f64)> = ring[..end]
        .iter()
        .map(|p| proj.project(p[0], p[1]))
        .collect();

    // MVT spec §4.3.4.4: exterior rings must be clockwise, holes
    // counter-clockwise *in tile coordinates* (Y-down). Computing the
    // shoelace signed area in tile-local space, positive area corresponds
    // to clockwise visual winding (because Y is flipped relative to the
    // standard math convention). Reverse when the sign doesn't match the
    // role — this makes the encoder robust to RFC-7946-violating inputs
    // (some PostGIS exports, hand-edited GeoJSON) without burdening the
    // engine layer with normalisation.
    let area = signed_area(&projected);
    let needs_reverse = match role {
        RingRole::Exterior => area < 0.0,
        RingRole::Hole => area > 0.0,
    };
    if needs_reverse {
        projected.reverse();
    }

    for (px, py) in projected {
        enc.add_point(px, py)?;
    }
    Ok(())
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

const EARTH_RADIUS_M: f64 = 6_378_137.0;
/// Maximum |lat| representable in Web Mercator before y_m → ±∞.
const WEB_MERCATOR_MAX_LAT: f64 = 85.051_128_779_806_59;

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
    lon.to_radians() * EARTH_RADIUS_M
}

fn lat_to_merc_y(lat: f64) -> f64 {
    let clamped = lat.clamp(-WEB_MERCATOR_MAX_LAT, WEB_MERCATOR_MAX_LAT);
    clamped.to_radians().tan().asinh() * EARTH_RADIUS_M
}

/// Hash of a property allowlist for use in in-process cache keys.
/// Different allowlists produce different hashes so they don't share cache
/// slots.
///
/// **Toolchain-ephemeral.** The implementation uses
/// `std::collections::hash_map::DefaultHasher`, which uses a fixed seed
/// (unlike `RandomState`) but whose algorithm is explicitly unspecified
/// across Rust releases. The same binary will produce the same hash for
/// the same input on every run, but a toolchain upgrade — or a binary
/// compiled with a different Rust version — can change the output without
/// warning. Do not persist this value, transmit it over the wire, or
/// compare it across process boundaries built with different compilers;
/// switch to a fixed-algorithm hasher (e.g. FNV) if stability is required.
pub fn properties_hash(allowlist: &PropertyAllowlist) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    match allowlist {
        PropertyAllowlist::All => 0u8.hash(&mut hasher),
        PropertyAllowlist::Subset(set) => {
            1u8.hash(&mut hasher);
            // Sort for deterministic hashing — HashSet iteration order is random.
            let mut keys: Vec<&String> = set.iter().collect();
            keys.sort();
            for k in keys {
                k.hash(&mut hasher);
            }
        }
    }
    hasher.finish()
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
    fn ring_unique_count_strips_closing_duplicate() {
        // GeoJSON convention closes a ring by repeating the first vertex.
        assert_eq!(
            ring_unique_count(&[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [0.0, 0.0]]),
            3
        );
        // Open ring — no duplicate to strip.
        assert_eq!(ring_unique_count(&[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]), 3);
        // 1-vertex ring is degenerate.
        assert_eq!(ring_unique_count(&[[0.0, 0.0]]), 1);
        // 2-vertex "closed" ring → 1 unique vertex after stripping.
        assert_eq!(ring_unique_count(&[[0.0, 0.0], [0.0, 0.0]]), 1);
        // Empty.
        assert_eq!(ring_unique_count(&[]), 0);
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
