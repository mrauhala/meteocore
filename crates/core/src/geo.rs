use std::f64::consts::{FRAC_PI_2, PI};

use crate::feature::Geometry;
use crate::map_engine::OutputCrs;

/// WGS84 semi-major axis in metres. Public so engines never hardcode the
/// radius themselves (`scripts/check_geo_safety.sh` flags the literal);
/// numerically equal to `web_mercator::EARTH_RADIUS` (the 3857 sphere).
pub const WGS84_A: f64 = 6_378_137.0;

/// OGC:CRS84 as WKT: WGS 84 with longitude as the first axis, in ISO
/// 19162:2019 (WKT2) form with an explicit axis `ORDER`. EDR advertises it
/// in every data query's `crs_details`. The EDR standard's own examples pair
/// `CRS84` with EPSG:4326's WKT, whose axes are latitude first, so this is
/// not copied from them. PROJ 9.8 identifies it:
/// `projinfo '<wkt>' --identify` → `OGC:CRS84: 100 %`.
pub const CRS84_WKT: &str = r#"GEOGCRS["WGS 84 (CRS84)",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]],CS[ellipsoidal,2],AXIS["geodetic longitude (Lon)",east,ORDER[1],ANGLEUNIT["degree",0.0174532925199433]],AXIS["geodetic latitude (Lat)",north,ORDER[2],ANGLEUNIT["degree",0.0174532925199433]],ID["OGC","CRS84"]]"#;
pub(crate) const WGS84_F: f64 = 1.0 / 298.257223563; // flattening
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

/// Mean Earth radius (metres) for spherical great-circle math — the value the
/// radar beam model uses, so geometry derived here aligns with the volume
/// engines (e.g. the cylindrical voxel grid → isosurface mapping).
pub const EARTH_RADIUS_M: f64 = 6_371_000.0;

/// 4/3 of the mean Earth radius, in metres — the effective radius used to
/// model standard atmospheric refraction of a radar beam.
pub const FOUR_THIRDS_EARTH_M: f64 = 4.0 / 3.0 * EARTH_RADIUS_M;

/// Forward beam map: `(slant_range_m, elevation_angle_deg)` →
/// `(ground_distance_m, height_above_antenna_m)` under the 4/3-Earth model.
///
/// `h = sqrt(r² + R'² + 2·r·R'·sin(el)) − R'`,
/// `s = R' · atan(r·cos(el) / (r·sin(el) + R'))`, with `R' = 4/3 · R_earth`.
///
/// The ONE home for this formula: engine-odim's voxel sampler and the
/// nowcast's per-cell beam geometry (#642) both call it.
pub fn slant_to_ground_height(slant_range_m: f64, elangle_deg: f64) -> (f64, f64) {
    let r = slant_range_m;
    let el = elangle_deg.to_radians();
    let rp = FOUR_THIRDS_EARTH_M;
    let h = (r * r + rp * rp + 2.0 * r * rp * el.sin()).sqrt() - rp;
    let s = rp * (r * el.cos() / (r * el.sin() + rp)).atan();
    (s, h)
}

/// Height of a beam's centre above the antenna at `ground_distance_m` along
/// a sweep at `elangle_deg`, metres, under the 4/3-Earth model.
///
/// The angle is deliberately NOT clamped to `>= 0`: real networks run a
/// slightly negative lowest tilt from terrain-elevated sites, and a
/// negative-tilt floor correctly dips below antenna level before
/// effective-Earth curvature lifts it.
pub fn beam_height_at_ground(elangle_deg: f64, ground_distance_m: f64) -> f64 {
    let cos_el = elangle_deg.to_radians().cos().max(1e-3);
    let r = ground_distance_m / cos_el;
    let (_, h) = slant_to_ground_height(r, elangle_deg);
    h
}

/// Wrap a longitude into (−180, 180] — the one formula for it (Critical Rule 4
/// spirit: the GRIB seam-frame axis, `destination_point` and the nowcast impact
/// join all need it).
pub fn wrap_lon(lon_deg: f64) -> f64 {
    let w = (lon_deg + 180.0).rem_euclid(360.0) - 180.0;
    if w == -180.0 {
        180.0
    } else {
        w
    }
}

/// Great-circle distance between two WGS84 points, metres (haversine on the
/// mean Earth radius). Good to ~0.3% — fine for range and coverage tests.
///
/// Same formula, same clamp, as engine-odim's hot-loop
/// `ground_distance_bearing_from` (which keeps its origin trig hoisted for
/// per-pixel sampling and cannot call this). engine-odim pins the two equal
/// to the metre, so a change here without one there fails a test rather
/// than drifting (the Critical Rule 4 lesson).
pub fn great_circle_distance_m(lon1: f64, lat1: f64, lon2: f64, lat2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = (lat2 - lat1).to_radians();
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    // Rounding can push `a` a hair past 1.0; the clamp keeps asin finite.
    2.0 * EARTH_RADIUS_M * a.sqrt().clamp(0.0, 1.0).asin()
}

/// Great-circle **destination point** on a sphere: starting at
/// (`lon_deg`, `lat_deg`), travel `distance_m` along the initial compass
/// `bearing_deg` (0° = north, increasing clockwise). Returns
/// `(lon_deg, lat_deg)`, with longitude normalised into `(−180, 180]`.
///
/// Shared geodesy so every consumer that turns a (range, azimuth) polar sample
/// into geographic coordinates — the radar volume engines, the 3D Tiles
/// isosurface encoder — uses one formula (and the same [`EARTH_RADIUS_M`]).
pub fn destination_point(
    lon_deg: f64,
    lat_deg: f64,
    distance_m: f64,
    bearing_deg: f64,
) -> (f64, f64) {
    let ang = distance_m / EARTH_RADIUS_M;
    let lat0 = lat_deg.to_radians();
    let lon0 = lon_deg.to_radians();
    let brg = bearing_deg.to_radians();
    let (sin_ang, cos_ang) = ang.sin_cos();
    let (sin_lat0, cos_lat0) = lat0.sin_cos();
    let sin_lat = sin_lat0 * cos_ang + cos_lat0 * sin_ang * brg.cos();
    let lat = sin_lat.asin();
    let y = brg.sin() * sin_ang * cos_lat0;
    let x = cos_ang - sin_lat0 * sin_lat;
    // Normalise longitude into (−180, 180]: a path starting near ±180° can
    // otherwise return a lon outside that range.
    let lon = (lon0 + y.atan2(x)).to_degrees();
    let lon = (lon + 540.0).rem_euclid(360.0) - 180.0;
    (lon, lat.to_degrees())
}

/// The point a fraction `f` (0 = start, 1 = end) of the way along the
/// **great circle** from (`lon0`, `lat0`) to (`lon1`, `lat1`), degrees, with
/// longitude wrapped into (−180, 180] by [`wrap_lon`]. The short way round,
/// so a segment from 170° to −170° crosses the antimeridian rather than
/// the Greenwich meridian — the path [`destination_point`] walks too.
///
/// Returns `None` when the endpoints are (nearly) antipodal: every great
/// circle through them is equally short, so no single path is defined.
/// Coincident endpoints return the start point.
pub fn intermediate_point(
    lon0: f64,
    lat0: f64,
    lon1: f64,
    lat1: f64,
    f: f64,
) -> Option<(f64, f64)> {
    let unit = |lon: f64, lat: f64| {
        let (sin_lat, cos_lat) = lat.to_radians().sin_cos();
        let (sin_lon, cos_lon) = lon.to_radians().sin_cos();
        [cos_lat * cos_lon, cos_lat * sin_lon, sin_lat]
    };
    let (a, b) = (unit(lon0, lat0), unit(lon1, lat1));
    let dot = (a[0] * b[0] + a[1] * b[1] + a[2] * b[2]).clamp(-1.0, 1.0);
    let d = dot.acos();
    let sin_d = d.sin();
    if sin_d < 1e-12 {
        // Coincident (d ≈ 0) or antipodal (d ≈ π).
        return (d < 1.0).then_some((wrap_lon(lon0), lat0));
    }
    // Spherical linear interpolation of the two unit vectors.
    let wa = ((1.0 - f) * d).sin() / sin_d;
    let wb = (f * d).sin() / sin_d;
    let p = [
        wa * a[0] + wb * b[0],
        wa * a[1] + wb * b[1],
        wa * a[2] + wb * b[2],
    ];
    let lat = p[2].atan2(p[0].hypot(p[1])).to_degrees();
    let lon = p[1].atan2(p[0]).to_degrees();
    Some((wrap_lon(lon), lat))
}

/// Coordinate reference system.
///
/// Stores projection parameters and provides forward/inverse transforms
/// between WGS84 geographic coordinates and projected coordinates.
#[derive(Debug, Clone, PartialEq)]
pub enum Crs {
    /// WGS84 geographic (EPSG:4326 / CRS84). Coordinates are (lon, lat) in degrees.
    Wgs84,
    /// Transverse Mercator (e.g., EPSG:3067 TM35FIN, UTM) on the WGS84
    /// ellipsoid, by Krüger's series to sixth order (#1130).
    ///
    /// Its valid area, where the inverse answers, is a square in projected
    /// metres around the point where the central meridian crosses the
    /// equator, of half-side k₀ times the quarter meridian (k₀ ·
    /// 10 001 965.729 m): up to the poles' northings, and as far east and
    /// west. The inverse is `None` past them (#1113). On the Earth that is
    /// the hemisphere within 90° of the central meridian, except near the
    /// equator, where it ends 66.29° from the meridian; from 23.41° of
    /// latitude to the poles it reaches the meridians 90° out. Inside it,
    /// the series matches the exact projection to under a millimetre
    /// (`TM_MAX_ETA`). Past a pole lies the far side of the Earth, and
    /// past the eastings the series runs away from the projection.
    /// The forward transform answers everywhere: exact inside the area,
    /// and past it unbounded near the equator 90° from the meridian.
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
    /// Lambert Conformal Conic with 2 standard parallels; equal
    /// parallels are the tangent (1SP, scale 1) cone.
    LambertConformalConic {
        lat1: f64,    // first standard parallel (radians)
        lat2: f64,    // second standard parallel (radians)
        lat0: f64,    // latitude of false origin (radians)
        lon0: f64,    // longitude of false origin (radians)
        false_e: f64, // false easting (meters)
        false_n: f64, // false northing (meters)
        /// Earth model: `None` is the WGS84 ellipsoid, `Some(r)` a sphere of
        /// radius `r` metres (PROJ `+R=r`), such as QueryData's 6371220 m.
        /// Longitudes and latitudes go onto the sphere unchanged, as in PROJ
        /// and in the producers that define grids this way.
        radius: Option<f64>,
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
    /// Geostationary satellite view (PROJ `+proj=geos`, CF `geostationary`),
    /// e.g. GOES-R ABI, Himawari AHI, MTG FCI.
    ///
    /// Projected coordinates are metres: the instrument's scan angle
    /// (radians) times `height`. GOES-R publishes the angles themselves as
    /// x/y (units `rad`), Himawari ISatSS in microradians. Only the Earth
    /// disk the satellite sees has a projection: `forward` returns NaN for a
    /// point on the far side of the Earth, and `inverse` returns `None` off
    /// the disk.
    Geostationary {
        lon0: f64,       // sub-satellite longitude (radians)
        height: f64,     // perspective point height above the ellipsoid (metres)
        semi_major: f64, // ellipsoid semi-major axis (metres)
        semi_minor: f64, // ellipsoid semi-minor axis (metres); equal for a sphere
        sweep: SweepAxis,
    },
    /// Spherical Web Mercator (EPSG:3857) as a **source** grid's CRS, e.g.
    /// NOAA's GMGSI global satellite mosaic. The math is
    /// [`crate::web_mercator`]; Web Mercator as an *output* CRS is
    /// [`OutputCrs::WebMercator`](crate::map_engine::OutputCrs::WebMercator).
    ///
    /// `forward` keeps longitude unwrapped: x is linear in longitude for
    /// any input, so a longitude past ±180° (a client wrapping the world)
    /// lands a turn (2π·R) further along x rather than jumping back. A grid
    /// that spans the globe wraps its columns itself. `inverse` returns
    /// longitude in [-180, 180]. At the poles the northing is unbounded:
    /// `forward` is ±∞ (or a huge finite value) there.
    WebMercator,
}

/// The axis a geostationary imager sweeps, which fixes the order of its
/// two scan angles (PROJ `+sweep`). GOES-R scans x (`sweep=x`); Himawari,
/// Meteosat and GK2A scan y (`sweep=y`, the PROJ default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepAxis {
    X,
    Y,
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
/// `"EPSG:4326"` arm is forward-looking — no current engine emits it (WGS84
/// grids are tagged `"CRS:84"`). `"EPSG:3857"` is the label of a
/// [`Crs::WebMercator`] source grid (engine-satellite's GMGSI mosaic).
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

/// Normalize an advertised WGS84 extent `[west, south, east, north]` to the
/// CRS84 domain, or `None` when it describes no area.
///
/// Engines derive extents from grid cell edges or projected samples, which can
/// fall just outside the domain: a global 0.25° grid's cell edges reach
/// ±90.125° latitude and −180.125° longitude. An extent whose accumulation
/// never saw a valid point is the inverted `[MAX, MAX, −MAX, −MAX]` sentinel.
/// Extents and collection search must not advertise either.
///
/// - Non-finite values, or `south > north`: `None`.
/// - Latitude is clamped to ±90 (a grid cell reaching past a pole).
/// - A box spanning ≥ 360° of longitude becomes −180…180.
/// - Other longitudes are clamped to ±180; a `west > east` box inside the
///   domain is an antimeridian crossing and is kept as is.
///
/// Advertised extents only: request bboxes (WMS/Maps viewports) must never be
/// clamped (Critical Rule 4).
pub fn crs84_extent(bbox: [f64; 4]) -> Option<[f64; 4]> {
    let [w, s, e, n] = bbox;
    if !bbox.iter().all(|v| v.is_finite()) || s > n {
        return None;
    }
    let (s, n) = (s.clamp(-90.0, 90.0), n.clamp(-90.0, 90.0));
    let (w, e) = if w <= e && e - w >= 360.0 {
        (-180.0, 180.0)
    } else {
        (w.clamp(-180.0, 180.0), e.clamp(-180.0, 180.0))
    };
    Some([w, s, e, n])
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
/// Two kinds of place in the bbox have no edge sample. A pole inside it is
/// its latitude extreme, where every meridian of the valid area meets: the
/// envelope runs to the pole and spans the longitudes the inverse gives
/// there, LAEA's central meridian ± 180° ([-170°, 190°] for EPSG:3035), or
/// [-180°, 180°] for another CRS. And Transverse Mercator's area ends at
/// the poles' northings and at eastings k₀ times a quarter meridian from
/// its central meridian ([`Crs::TransverseMercator`]), so a TM box's
/// envelope takes [`area_edge_points`] too: where those edges enter and
/// leave the bbox, and the pole or equator between. It then reaches the
/// area's edge inside the bbox, at the longitudes its inverse gives,
/// unwrapped.
///
/// Returns `None` when **every** sampled point fails to inverse-project and
/// no TM area edge runs inside the bbox. Ask [`outside_projection_domain`]
/// first: for a bbox it keeps, `None` means the edge samples miss a valid
/// area that reaches into the bbox. That is an EPSG:3035 box wider than the
/// LAEA disk, or a tall one whose edges cross the disk only between
/// samples. The edges then give no read window, and the caller must not
/// fall back to a global extent: a global read window on a planet-scale
/// GRIB/COG source would decode the whole dataset for one request.
///
/// The edge samples skip what lies past the valid area. Where the LAEA
/// disk's rim cuts an edge of the bbox between two samples, the envelope
/// can stop short of the disk by up to a sample's spacing along that edge.
pub fn wgs84_envelope(crs: &Crs, bbox: [f64; 4]) -> Option<[f64; 4]> {
    let mut envelope = edge_envelope(bbox, |x, y| crs.inverse(x, y));
    for (lon, lat) in area_edge_points(crs, bbox) {
        let [w, s, e, n] = envelope.unwrap_or([lon, lat, lon, lat]);
        envelope = Some([w.min(lon), s.min(lat), e.max(lon), n.max(lat)]);
    }
    let [mut west, mut south, mut east, mut north] = envelope?;
    for pole in [90.0, -90.0] {
        if holds_pole(crs, bbox, pole) {
            south = south.min(pole);
            north = north.max(pole);
            if let Some(lons) = pole_longitudes(crs) {
                (west, east) = lons;
            }
        }
    }
    Some([west, south, east, north])
}

/// Whether the projected `bbox` holds the pole at latitude `pole` (±90°),
/// edges included. A geographic bbox's pole is a whole edge, which its
/// samples already reach: never.
///
/// The pole is projected from the central meridian, where TM's and LAEA's
/// forward transforms put it exactly. From another meridian it lands a
/// rounding error off, EPSG:3067's at easting 499 999.999 999 999 8 from
/// 0°, and misses a bbox whose west edge is the pole's easting.
pub fn holds_pole(crs: &Crs, bbox: [f64; 4], pole: f64) -> bool {
    let lon = match *crs {
        Crs::Wgs84 => return false,
        Crs::TransverseMercator { lon0, .. } | Crs::LambertAzimuthalEqualArea { lon0, .. } => {
            lon0.to_degrees()
        }
        _ => 0.0,
    };
    let (x, y) = crs.forward(lon, pole);
    (bbox[0]..=bbox[2]).contains(&x) && (bbox[1]..=bbox[3]).contains(&y)
}

/// The longitudes `crs`'s inverse gives around a pole, where every meridian
/// meets: LAEA's central meridian ± 180°, [-180°, 180°] for a CRS other
/// than Transverse Mercator. For TM `None`: its area's north and south
/// edges run through the pole, along the meridians 90° from the central
/// one, and [`area_edge_points`] already takes where they run.
fn pole_longitudes(crs: &Crs) -> Option<(f64, f64)> {
    match *crs {
        Crs::TransverseMercator { .. } => None,
        Crs::LambertAzimuthalEqualArea { lon0, .. } => {
            let lon0 = lon0.to_degrees();
            Some((lon0 - 180.0, lon0 + 180.0))
        }
        _ => Some((-180.0, 180.0)),
    }
}

/// The places of a Transverse Mercator projection's valid area inside the
/// projected `bbox` that samples along its edges miss: where the area's
/// own edges run inside the bbox (#1113, #1130). In projected metres the
/// area is a square ([`Crs::TransverseMercator`]): the poles' northings
/// north and south, which the meridians 90° from the central one follow
/// down from the pole, and eastings k₀ times a quarter meridian either side
/// of it. Along each of its edges longitude and latitude each run one way
/// from the equator or the pole, so the ends of its stretch inside the
/// bbox, with the equator or the pole between them, are its extremes
/// there. With the edge samples they give the area's extremes inside the
/// bbox. The samples alone stop short of the area's edge by up to their
/// spacing, and find nothing in a box whose edges all miss the area.
/// Longitudes are the central meridian's plus the offset, unwrapped, as
/// the inverse gives them. Empty for any other CRS.
pub fn area_edge_points(crs: &Crs, bbox: [f64; 4]) -> Vec<(f64, f64)> {
    let Crs::TransverseMercator {
        lat0,
        lon0,
        k0,
        false_e,
        false_n,
    } = *crs
    else {
        return Vec::new();
    };
    let ka = k0 * TM_A;
    let [min_e, min_n, max_e, max_n] = bbox;
    let (eta_lo, eta_hi) = ((min_e - false_e) / ka, (max_e - false_e) / ka);
    let xi0 = tm_xi0(lat0);
    let (xi_lo, xi_hi) = ((min_n - false_n) / ka + xi0, (max_n - false_n) / ka + xi0);
    let mut points = Vec::new();
    // The edge at `fixed` (an η for the east and west edges, a ξ for the
    // north and south ones) when the bbox reaches it, over the bbox's span
    // `(lo, hi)` of the other coordinate cut to the area's `limit`.
    let mut edge =
        |fixed: f64, reached: bool, (lo, hi): (f64, f64), limit: f64, east_west: bool| {
            let (lo, hi) = (lo.max(-limit), hi.min(limit));
            if !reached || lo > hi {
                return;
            }
            let mid = (lo < 0.0 && 0.0 < hi).then_some(0.0);
            for t in [Some(lo), Some(hi), mid].into_iter().flatten() {
                let (xi, eta) = if east_west { (t, fixed) } else { (fixed, t) };
                let (lat, lam) = tm_lat_lam(xi, eta);
                points.push((lon0.to_degrees() + lam.to_degrees(), lat.to_degrees()));
            }
        };
    for eta in [-TM_MAX_ETA, TM_MAX_ETA] {
        let reached = (eta_lo..=eta_hi).contains(&eta);
        edge(eta, reached, (xi_lo, xi_hi), FRAC_PI_2, true);
    }
    for xi in [-FRAC_PI_2, FRAC_PI_2] {
        let reached = (xi_lo..=xi_hi).contains(&xi);
        edge(xi, reached, (eta_lo, eta_hi), TM_MAX_ETA, false);
    }
    points
}

/// Whether the projected bbox `[min_e, min_n, max_e, max_n]` lies wholly
/// outside `crs`'s valid area, where no point maps to the Earth. A map of it
/// is empty content (WMS 1.3.0 §7.3.3.6), with nothing to read.
///
/// For the projected output CRSs ([`projected_output_crs`]) that area is
/// symmetric about the axes through a centre and narrows away from it: a
/// point no farther from the centre than an inside one, along each axis, is
/// inside too.
/// - LAEA: the ellipse `ρ ≤ 2·Rq` around the false origin. Past it the
///   inverse is NaN.
/// - Transverse Mercator: where its inverse answers (#1113, #1130),
///   centred on `(false_e, N_eq)`, `N_eq` the equator's northing. That is
///   the square of eastings and northings within k₀ times a quarter
///   meridian of the centre ([`Crs::TransverseMercator`]). Past a pole lies
///   the far side of the Earth; farther east or west, the series is no
///   longer exact, and next to TM's singular points on the equator it has
///   no finite result. The inverse is `None` for all of them.
///
/// So the bbox meets the area exactly when its point nearest the centre
/// does, the centre clamped into the bbox: one test. Unlike the edge samples
/// of [`wgs84_envelope`], it also sees an area the bbox encloses. `false`
/// for every other variant, none of which is an output CRS.
pub fn outside_projection_domain(crs: &Crs, bbox: [f64; 4]) -> bool {
    let (centre_e, centre_n) = match *crs {
        Crs::LambertAzimuthalEqualArea {
            false_e, false_n, ..
        } => (false_e, false_n),
        Crs::TransverseMercator {
            lat0,
            k0,
            false_e,
            false_n,
            ..
        } => (false_e, tm_equator_northing(lat0, k0, false_n)),
        _ => return false,
    };
    let [min_e, min_n, max_e, max_n] = bbox;
    // `max` then `min`, not `clamp`, which panics on an inverted range.
    let e = centre_e.max(min_e).min(max_e);
    let n = centre_n.max(min_n).min(max_n);
    !maps_to_earth(crs, e, n)
}

/// Whether the projected point `(e, n)` lies in `crs`'s valid area: it
/// inverse-projects. Both output CRSs' inverses are `None` past it.
fn maps_to_earth(crs: &Crs, e: f64, n: f64) -> bool {
    crs.inverse(e, n).is_some()
}

/// The northing of a Transverse Mercator projection's equator: the centre
/// line of its valid area, which reaches `k0` times a quarter meridian
/// either side of it.
fn tm_equator_northing(lat0: f64, k0: f64, false_n: f64) -> f64 {
    false_n - k0 * TM_A * tm_xi0(lat0)
}

/// Projected envelope `[min_e, min_n, max_e, max_n]` (in `crs`'s metres) of a
/// WGS84 bbox `[west, south, east, north]`, found by forward-projecting points
/// sampled along the four edges.
///
/// The inverse of [`wgs84_envelope`]: used by OGC API Maps, where the request
/// `bbox` is in CRS:84 but the output `crs` is projected — the projected map
/// frame must cover the requested geographic box. `Crs::forward` is total for
/// every output CRS (Geostationary and WebMercator, the partial variants, are
/// source CRSs only), so this always returns `Some`; the `unwrap_or` is a
/// defensive identity fallback.
///
/// Transverse Mercator's eastings run off without bound toward its singular
/// points, on the equator 90° either side of the central meridian. A box
/// holding one has no easting extreme on its edges, which can lie as close
/// to the meridian as the far side of the Earth does: the edges of a global
/// box map within 3 100 km of it in EPSG:3067. The envelope then reaches
/// the valid area's edge on that side, where it crosses the equator, so it
/// holds the area the box does (#1130). An edge passing within a few
/// degrees of a singular point maps without bound itself, where the series
/// diverges, and so does the envelope.
pub fn projected_envelope(crs: &Crs, bbox: [f64; 4]) -> [f64; 4] {
    let envelope = edge_envelope(bbox, |lon, lat| Some(crs.forward(lon, lat))).unwrap_or(bbox);
    let Crs::TransverseMercator {
        lat0,
        lon0,
        k0,
        false_e,
        false_n,
    } = *crs
    else {
        return envelope;
    };
    let [west, south, east, north] = bbox;
    let [mut min_e, mut min_n, mut max_e, mut max_n] = envelope;
    if south <= 0.0 && 0.0 <= north {
        let n_eq = tm_equator_northing(lat0, k0, false_n);
        for side in [-1.0, 1.0] {
            // The singular point's longitude, the first turn of it at or
            // east of the box's west edge.
            let lon = lon0.to_degrees() + side * 90.0;
            let lon = lon + 360.0 * ((west - lon) / 360.0).ceil();
            if lon <= east {
                let e = false_e + side * k0 * TM_A * TM_MAX_ETA;
                (min_e, max_e) = (min_e.min(e), max_e.max(e));
                (min_n, max_n) = (min_n.min(n_eq), max_n.max(n_eq));
            }
        }
    }
    [min_e, min_n, max_e, max_n]
}

impl Crs {
    /// Forward-transform WGS84 (lon_deg, lat_deg) to projected (easting, northing).
    /// For Wgs84, returns (lon, lat) unchanged.
    /// For RotatedLatLon, returns rotated (lon, lat) in degrees.
    /// For Geostationary, returns `(NaN, NaN)` for a point the satellite
    /// cannot see; for WebMercator the northing is unbounded at the poles.
    /// Every other variant is total. Callers mapping many points
    /// (`ProjectionGrid`, envelopes) must skip non-finite results.
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
                radius,
            } => Lcc::new(*lat1, *lat2, *lat0, *lon0, *false_e, *false_n, *radius)
                .forward(lat_deg.to_radians(), lon_deg.to_radians()),
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
            Crs::Geostationary { .. } => {
                let geos = Geos::from_crs(self).expect("Geostationary variant");
                geos.forward(lat_deg.to_radians(), lon_deg.to_radians())
                    .unwrap_or((f64::NAN, f64::NAN))
            }
            Crs::WebMercator => (
                crate::web_mercator::lon_to_x(lon_deg),
                crate::web_mercator::lat_to_y(lat_deg),
            ),
        }
    }

    /// Inverse-transform projected (easting, northing) to WGS84 (lon_deg, lat_deg).
    /// For Wgs84, returns (x, y) unchanged.
    /// For RotatedLatLon, input is rotated (lon, lat) in degrees.
    /// Returns `None` if the projection math produces NaN/Inf (e.g., near poles).
    /// Transverse Mercator's is `None` outside its valid area, past a pole
    /// or too far east or west for its series ([`Crs::TransverseMercator`],
    /// #1113); its longitude is the central meridian's plus the offset, not
    /// wrapped into [-180°, 180°].
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
                let (lat, lon) = tm_inverse(x, y, *lat0, *lon0, *k0, *false_e, *false_n)?;
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
                radius,
            } => {
                let (lat, lon) =
                    Lcc::new(*lat1, *lat2, *lat0, *lon0, *false_e, *false_n, *radius).inverse(x, y);
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
            Crs::Geostationary { .. } => {
                let geos = Geos::from_crs(self).expect("Geostationary variant");
                let (lat, lon) = geos.inverse(x, y)?;
                (wrap_lon(lon.to_degrees()), lat.to_degrees())
            }
            Crs::WebMercator => (
                wrap_lon(crate::web_mercator::x_to_lon(x)),
                crate::web_mercator::y_to_lat(y),
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
    /// Create from the coordinates of the first pixel's **centre** and the
    /// step between pixel centres — the form NetCDF coordinate variables
    /// take (CF `x`/`y`). Rows must run north to south (`dy < 0`), as for
    /// every other `GeoTransform`: a south-up source is flipped on read.
    ///
    /// For a geostationary source the coordinates are metres, i.e. the scan
    /// angles in radians times the satellite height.
    pub fn from_cell_centres(
        x0: f64,
        dx: f64,
        y0: f64,
        dy: f64,
        width: u32,
        height: u32,
        crs: Crs,
    ) -> Result<Self, String> {
        if !(x0.is_finite() && dx.is_finite() && y0.is_finite() && dy.is_finite()) {
            return Err("non-finite grid coordinates".to_string());
        }
        if dx <= 0.0 || dy >= 0.0 {
            return Err(format!(
                "grid must run west to east and north to south (dx > 0, dy < 0), \
                 got dx={dx}, dy={dy}"
            ));
        }
        if width == 0 || height == 0 {
            return Err("empty grid".to_string());
        }
        Ok(GeoTransform {
            origin_x: x0 - 0.5 * dx,
            origin_y: y0 - 0.5 * dy,
            pixel_width: dx,
            pixel_height: -dy,
            width,
            height,
            crs,
        })
    }

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
    ///
    /// A geostationary raster's edges may lie wholly in space (a full disk),
    /// so its box also covers where the edges meet the limb and the disk's
    /// extreme points, and is `west > east` when it crosses the antimeridian.
    /// Returns the `[MAX, MAX, MIN, MIN]` sentinel when nothing
    /// reprojects; [`crs84_extent`] turns that into `None`.
    pub fn bbox(&self) -> [f64; 4] {
        if let Some(geos) = Geos::from_crs(&self.crs) {
            return self.geostationary_bbox(&geos);
        }
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

    /// [`bbox`](Self::bbox) reaching every pole the grid holds, at every
    /// longitude. A polar stereographic or azimuthal grid's edges circle
    /// its pole and stay short of it, so the edge samples alone miss the
    /// pole and the latitudes around it. The sentinel stays the sentinel.
    pub fn bbox_with_poles(&self) -> [f64; 4] {
        let [mut w, mut s, mut e, mut n] = self.bbox();
        if s > n {
            return [w, s, e, n];
        }
        for pole in [90.0, -90.0] {
            if self.cells_hold_pole(pole) {
                (w, e) = (-180.0, 180.0);
                s = s.min(pole);
                n = n.max(pole);
            }
        }
        [w, s, e, n]
    }

    /// Whether the grid's forward transform puts the pole at latitude
    /// `pole` (±90°) inside its cells.
    pub fn cells_hold_pole(&self, pole: f64) -> bool {
        let (col, row) = self.world_to_pixel_f64(0.0, pole);
        (0.0..f64::from(self.width)).contains(&col) && (0.0..f64::from(self.height)).contains(&row)
    }

    /// [`bbox`](Self::bbox) of a geostationary raster.
    ///
    /// Longitude and latitude have no extremum inside the visible part of
    /// the raster (the projection is a local diffeomorphism there), so the
    /// extremes lie on its boundary: the raster edges where they are on the
    /// disk, and the stretch of limb inside the raster. Along the limb,
    /// latitude and longitude each have one maximum and one minimum — the
    /// disk's four extreme points — so the extremes on a stretch of limb are
    /// at its ends (where it meets a raster edge) or at one of those points.
    fn geostationary_bbox(&self, geos: &Geos) -> [f64; 4] {
        let x_min = self.origin_x;
        let x_max = self.origin_x + self.width as f64 * self.pixel_width;
        let y_max = self.origin_y;
        let y_min = self.origin_y - self.height as f64 * self.pixel_height;
        let mut extent = SeamExtent::new(geos.lon0.to_degrees());
        let inverse = |x: f64, y: f64| self.crs.inverse(x, y);
        let corners = [
            (x_min, y_max),
            (x_max, y_max),
            (x_max, y_min),
            (x_min, y_min),
        ];
        for k in 0..4 {
            let (a, b) = (corners[k], corners[(k + 1) % 4]);
            visit_edge_with_boundaries(
                64,
                |t| (a.0 + t * (b.0 - a.0), a.1 + t * (b.1 - a.1)),
                inverse,
                |(lon, lat)| extent.add(lon, lat),
            );
        }
        for (lat, lon) in geos.limb_extremes() {
            if let Some((x, y)) = geos.forward(lat, lon) {
                if (x_min..=x_max).contains(&x) && (y_min..=y_max).contains(&y) {
                    extent.add(lon.to_degrees(), lat.to_degrees());
                }
            }
        }
        extent.finish()
    }

    /// [`bbox_to_pixels`](Self::bbox_to_pixels) of a geostationary raster:
    /// the projected envelope of the part of the bbox the satellite sees.
    ///
    /// Its extremes lie on the bbox edges where visible, or on the stretch
    /// of limb inside the bbox. Projected x and y are linear across the
    /// disk, whose outline is convex, so a stretch of limb reaches its x/y
    /// extremes at its ends or at one of the disk's four extreme points.
    /// The bbox may cross the antimeridian (`west > east`).
    fn geostationary_envelope(
        &self,
        geos: &Geos,
        west: f64,
        south: f64,
        east: f64,
        north: f64,
    ) -> Option<[f64; 4]> {
        let span = if east >= west {
            east - west
        } else {
            east + 360.0 - west
        };
        let mut env = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
        let mut add = |(x, y): (f64, f64)| {
            env = [env[0].min(x), env[1].min(y), env[2].max(x), env[3].max(y)];
        };
        let forward = |lon: f64, lat: f64| {
            let (x, y) = self.crs.forward(lon, lat);
            (x.is_finite() && y.is_finite()).then_some((x, y))
        };
        let corners = [
            (west, south),
            (west + span, south),
            (west + span, north),
            (west, north),
        ];
        for k in 0..4 {
            let (a, b) = (corners[k], corners[(k + 1) % 4]);
            visit_edge_with_boundaries(
                64,
                |t| (a.0 + t * (b.0 - a.0), a.1 + t * (b.1 - a.1)),
                forward,
                &mut add,
            );
        }
        for (lat, lon) in geos.limb_extremes() {
            let (lon, lat) = (lon.to_degrees(), lat.to_degrees());
            let inside = (south..=north).contains(&lat) && (lon - west).rem_euclid(360.0) <= span;
            if inside {
                if let Some(xy) = geos.forward(lat.to_radians(), lon.to_radians()) {
                    add(xy);
                }
            }
        }
        (env[0] <= env[2]).then_some(env)
    }

    /// Convert pixel coordinate to WGS84 (lon, lat) at pixel center.
    /// Returns (0, 0) if reprojection fails (degenerate edge case, or a
    /// geostationary pixel in space).
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
        if let Some(geos) = Geos::from_crs(&self.crs) {
            let [min_x, min_y, max_x, max_y] =
                self.geostationary_envelope(&geos, west, south, east, north)?;
            return self.envelope_to_pixels(min_x, min_y, max_x, max_y);
        }
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

        self.envelope_to_pixels(min_x, min_y, max_x, max_y)
    }

    /// Pixel window `(col_start, row_start, col_end, row_end)` covering a
    /// source-CRS envelope, clamped to the raster; `None` when empty.
    fn envelope_to_pixels(
        &self,
        min_x: f64,
        min_y: f64,
        max_x: f64,
        max_y: f64,
    ) -> Option<(u32, u32, u32, u32)> {
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
// Transverse Mercator — used by EPSG:3067 (TM35FIN) and UTM zones.
// Krüger's series to sixth order in the third flattening n, in the form of
// C. F. F. Karney, "Transverse Mercator with an accuracy of a few
// nanometers", J. Geodesy 85 (2011) 475–485: τ′ = tan χ of the conformal
// latitude, the spherical TM ζ′ = ξ′ + iη′ of (χ, λ), the series
// ζ = ζ′ + Σ αⱼ sin 2jζ′ and its reverse ζ′ = ζ − Σ βⱼ sin 2jζ, and
// Newton's method for τ of τ′. PROJ's `tmerc` sums the same series (its
// `+approx` selects Snyder's older one).
// ============================================================================

/// WGS84's third flattening n = f / (2 − f), the series' small parameter.
const TM_N: f64 = WGS84_F / (2.0 - WGS84_F);

/// The rectifying radius A, to n⁶: a quarter meridian is A·π/2,
/// 10 001 965.729 m.
const TM_A: f64 = WGS84_A / (1.0 + TM_N)
    * (1.0 + TM_N * TM_N * (1.0 / 4.0 + TM_N * TM_N * (1.0 / 64.0 + TM_N * TM_N / 256.0)));

/// Krüger's αⱼ, j = 1…6, to n⁶ (Karney 2011).
const TM_ALPHA: [f64; 6] = [
    TM_N * (1.0 / 2.0
        + TM_N
            * (-2.0 / 3.0
                + TM_N
                    * (5.0 / 16.0
                        + TM_N
                            * (41.0 / 180.0 + TM_N * (-127.0 / 288.0 + TM_N * 7891.0 / 37800.0))))),
    TM_N * TM_N
        * (13.0 / 48.0
            + TM_N
                * (-3.0 / 5.0
                    + TM_N
                        * (557.0 / 1440.0
                            + TM_N * (281.0 / 630.0 - TM_N * 1983433.0 / 1935360.0)))),
    TM_N * TM_N
        * TM_N
        * (61.0 / 240.0
            + TM_N * (-103.0 / 140.0 + TM_N * (15061.0 / 26880.0 + TM_N * 167603.0 / 181440.0))),
    TM_N * TM_N
        * TM_N
        * TM_N
        * (49561.0 / 161280.0 + TM_N * (-179.0 / 168.0 + TM_N * 6601661.0 / 7257600.0)),
    TM_N * TM_N * TM_N * TM_N * TM_N * (34729.0 / 80640.0 - TM_N * 3418889.0 / 1995840.0),
    TM_N * TM_N * TM_N * TM_N * TM_N * TM_N * 212378941.0 / 319334400.0,
];

/// Krüger's βⱼ, j = 1…6, to n⁶ (Karney 2011).
const TM_BETA: [f64; 6] = [
    TM_N * (1.0 / 2.0
        + TM_N
            * (-2.0 / 3.0
                + TM_N
                    * (37.0 / 96.0
                        + TM_N
                            * (-1.0 / 360.0
                                + TM_N * (-81.0 / 512.0 + TM_N * 96199.0 / 604800.0))))),
    TM_N * TM_N
        * (1.0 / 48.0
            + TM_N
                * (1.0 / 15.0
                    + TM_N
                        * (-437.0 / 1440.0
                            + TM_N * (46.0 / 105.0 - TM_N * 1118711.0 / 3870720.0)))),
    TM_N * TM_N
        * TM_N
        * (17.0 / 480.0
            + TM_N * (-37.0 / 840.0 + TM_N * (-209.0 / 4480.0 + TM_N * 5569.0 / 90720.0))),
    TM_N * TM_N
        * TM_N
        * TM_N
        * (4397.0 / 161280.0 + TM_N * (-11.0 / 504.0 - TM_N * 830251.0 / 7257600.0)),
    TM_N * TM_N * TM_N * TM_N * TM_N * (4583.0 / 161280.0 - TM_N * 108847.0 / 3991680.0),
    TM_N * TM_N * TM_N * TM_N * TM_N * TM_N * 20648693.0 / 638668800.0,
];

/// The normalized easting η = (E − E₀) / (k₀A) where a Transverse Mercator
/// projection's valid area ends, east and west of its central meridian
/// (#1130). The poles' northings, ξ = ±π/2, end it north and south, so the
/// area is a square of half-side k₀ times the quarter meridian.
///
/// The series' error grows with η, each omitted term as e^(2jη). Against
/// the exact projection, found by integrating the meridian arc along a
/// complex isometric latitude, it is under 0.6 mm forward and 7 µm inverse
/// anywhere on this edge, and 1 mm forward at η = 1.65, 1 m at 2.13 and
/// 140 m at 2.50: 68°, 76° and 80° from the meridian on the equator. PROJ's
/// `tmerc` answers to about 80.5° there.
const TM_MAX_ETA: f64 = FRAC_PI_2;

/// How far past a pole's northing, in ξ, a point still counts as the pole:
/// a few rounding errors, the pole projected and inverted again.
const TM_POLE_SLACK: f64 = 4.0 * f64::EPSILON;

/// τ′ = tan χ, the conformal latitude's tangent, of τ = tan φ.
fn tm_taup(tau: f64) -> f64 {
    let tau1 = tau.hypot(1.0);
    let sigma = tm_sigma(tau / tau1);
    sigma.hypot(1.0) * tau - sigma * tau1
}

/// σ = sinh(e · atanh(e · sin φ)), of `sin φ`: τ′ = τ √(1 + σ²) − σ √(1 + τ²).
fn tm_sigma(sin_lat: f64) -> f64 {
    let e = WGS84_E2.sqrt();
    (e * (e * sin_lat).atanh()).sinh()
}

/// τ = tan φ of τ′ = tan χ, by Newton's method from τ′ / (1 − e²). It
/// converges quadratically, so a step under √ε/10 leaves an error under ε:
/// one or two steps.
fn tm_tau(taup: f64) -> f64 {
    let e2m = 1.0 - WGS84_E2;
    let tol = f64::EPSILON.sqrt() / 10.0 * taup.abs().max(1.0);
    let mut tau = taup / e2m;
    for _ in 0..5 {
        let taupa = tm_taup(tau);
        // dτ′/dτ = (1 − e²) √(1 + τ′²) √(1 + τ²) / (1 + (1 − e²) τ²).
        let step =
            (taup - taupa) * (1.0 + e2m * tau * tau) / (e2m * tau.hypot(1.0) * taupa.hypot(1.0));
        tau += step;
        if step.abs() < tol || !step.is_finite() {
            break;
        }
    }
    tau
}

/// `Σ cⱼ sin 2jζ`, j = 1…6, of the complex `ζ = ξ + iη`, as its real and
/// imaginary parts, by Clenshaw's recurrence, from `[sin 2ξ, cos 2ξ,
/// sinh 2η, cosh 2η]`.
fn tm_sine_series(c: &[f64; 6], [sin, cos, sinh, cosh]: [f64; 4]) -> (f64, f64) {
    // 2 cos 2ζ, and sin 2ζ.
    let (ar, ai) = (2.0 * cos * cosh, -2.0 * sin * sinh);
    let (sr, si) = (sin * cosh, cos * sinh);
    // b_k = c_k + 2 cos 2ζ · b_(k+1) − b_(k+2), down from k = 6.
    let (mut b1, mut b2) = ((0.0, 0.0), (0.0, 0.0));
    for &ck in c.iter().rev() {
        let b = (
            ck + ar * b1.0 - ai * b1.1 - b2.0,
            ar * b1.1 + ai * b1.0 - b2.1,
        );
        (b1, b2) = (b, b1);
    }
    // The sum is sin 2ζ · b_1.
    (sr * b1.0 - si * b1.1, sr * b1.1 + si * b1.0)
}

/// The normalized coordinates `(ξ, η)`, northing and easting from the
/// equator on the central meridian over k₀A, of latitude `lat` and
/// longitude `lam` from the central meridian, in radians. Exact to the
/// series' error inside the valid area ([`TM_MAX_ETA`]); past it the
/// series diverges, without bound near the equator 90° from the meridian.
fn tm_xi_eta(lat: f64, lam: f64) -> (f64, f64) {
    let (sin_lat, cos_lat) = lat.sin_cos();
    let sigma = tm_sigma(sin_lat);
    // τ′ over cos φ, as `tm_taup` of tan φ.
    let taup = (sin_lat * sigma.hypot(1.0) - sigma) / cos_lat;
    let (sin_lam, cos_lam) = lam.sin_cos();
    // ξ′ = atan2(τ′, cos λ) and sinh η′ = sin λ / r, r² = τ′² + cos² λ, so
    // the series' double angles follow from τ′ and λ alone.
    let r2 = taup * taup + cos_lam * cos_lam;
    let xip = taup.atan2(cos_lam);
    let etap = (sin_lam / r2.sqrt()).asinh();
    let double = [
        2.0 * taup * cos_lam / r2,
        (cos_lam * cos_lam - taup * taup) / r2,
        2.0 * sin_lam * taup.hypot(1.0) / r2,
        (1.0 + taup * taup + sin_lam * sin_lam) / r2,
    ];
    let (dxi, deta) = tm_sine_series(&TM_ALPHA, double);
    (xip + dxi, etap + deta)
}

/// Latitude and longitude from the central meridian, radians, of the
/// normalized coordinates `(ξ, η)`. Unchecked: [`tm_inverse`] keeps it to
/// the valid area.
fn tm_lat_lam(xi: f64, eta: f64) -> (f64, f64) {
    let (sin, cos) = (2.0 * xi).sin_cos();
    let sinh = (2.0 * eta).sinh();
    let (dxi, deta) = tm_sine_series(&TM_BETA, [sin, cos, sinh, sinh.hypot(1.0)]);
    let (sin_xip, cos_xip) = (xi - dxi).sin_cos();
    let sinh_etap = (eta - deta).sinh();
    let r = sinh_etap.hypot(cos_xip);
    let lam = sinh_etap.atan2(cos_xip);
    let lat = if r > 0.0 {
        tm_tau(sin_xip / r).atan()
    } else {
        FRAC_PI_2.copysign(xi)
    };
    (lat, lam)
}

/// ξ of the latitude of natural origin `lat0`, on the central meridian.
fn tm_xi0(lat0: f64) -> f64 {
    if lat0 == 0.0 {
        0.0
    } else {
        tm_xi_eta(lat0, 0.0).0
    }
}

fn tm_forward(
    lat: f64,
    lon: f64,
    lat0: f64,
    lon0: f64,
    k0: f64,
    false_e: f64,
    false_n: f64,
) -> (f64, f64) {
    let (xi, eta) = tm_xi_eta(lat, lon - lon0);
    let ka = k0 * TM_A;
    (false_e + ka * eta, false_n + ka * (xi - tm_xi0(lat0)))
}

/// `(lat, lon)` in radians of the projected point `(x, y)`, `None` outside
/// the valid area ([`TM_MAX_ETA`]): past a pole, where the far side of the
/// Earth lies, or so far east or west that the series is no longer exact
/// (#1113, #1130). The longitude is the central meridian's plus the
/// offset, unwrapped.
fn tm_inverse(
    x: f64,
    y: f64,
    lat0: f64,
    lon0: f64,
    k0: f64,
    false_e: f64,
    false_n: f64,
) -> Option<(f64, f64)> {
    let ka = k0 * TM_A;
    let xi = (y - false_n) / ka + tm_xi0(lat0);
    let eta = (x - false_e) / ka;
    // `!(a <= b)` also rejects NaN.
    if !(xi.abs() <= FRAC_PI_2 + TM_POLE_SLACK && eta.abs() <= TM_MAX_ETA) {
        return None;
    }
    let (lat, lam) = tm_lat_lam(xi.clamp(-FRAC_PI_2, FRAC_PI_2), eta);
    Some((lat, lon0 + lam))
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
    let qp = q_authalic(PI / 2.0, e);

    let beta = (q_val / qp).clamp(-1.0, 1.0).asin();

    let rq = WGS84_A * (qp / 2.0).sqrt();
    let (beta0, dd) = laea_origin(lat0, e, qp, rq);

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

/// Closed-form inverse, Snyder eqs. 24-26 to 24-30 (oblique ellipsoid) with
/// the authalic-latitude inverse (3-18). Every point of the projection's disk
/// resolves to its own `(lat, lon)`, including points beyond the pole; the
/// longitude comes back within ±180° of `lon0`, so it is continuous except
/// across the antimeridian of `lon0`. Outside the disk (`ρ > 2·Rq`) it is
/// NaN.
///
/// This replaced a Newton iteration on [`laea_forward`], which folds at the
/// pole: past it the iteration settled on latitudes above 90° or never
/// converged, blanking and scrambling every polar EPSG:3035 view (#323).
fn laea_inverse(x: f64, y: f64, lat0: f64, lon0: f64, false_e: f64, false_n: f64) -> (f64, f64) {
    let e2 = WGS84_E2;
    let e = e2.sqrt();
    let qp = q_authalic(PI / 2.0, e);
    let rq = WGS84_A * (qp / 2.0).sqrt();
    let (beta0, dd) = laea_origin(lat0, e, qp, rq);

    let (x, y) = (x - false_e, y - false_n);
    let rho = ((x / dd).powi(2) + (dd * y).powi(2)).sqrt();
    if rho < 1e-9 {
        return (lat0, lon0);
    }
    let ce = 2.0 * (rho / (2.0 * rq)).asin();
    let (sin_ce, cos_ce) = ce.sin_cos();
    let q = qp * (cos_ce * beta0.sin() + dd * y * sin_ce * beta0.cos() / rho);
    let lon = lon0
        + (x * sin_ce).atan2(dd * rho * beta0.cos() * cos_ce - dd * dd * y * beta0.sin() * sin_ce);
    (authalic_inverse(q, e), lon)
}

/// The origin's authalic latitude `β₀` and Snyder's `D` (eq. 24-20), how
/// much the oblique aspect stretches x, and shrinks y, at the origin, for
/// the origin latitude `lat0`, eccentricity `e`, `qp` and the authalic
/// radius `rq`. A polar origin (EASE-Grid 2.0, EPSG:6931/6932) is the
/// limit, `β₀ = ±90°` and `D = 1`: there `D`'s factors `cos φ₀` and
/// `cos β₀` both vanish, and the ratio of their rounded values is 0.45 %
/// off it, which put a point 2300 km from the pole 10 km out of place.
fn laea_origin(lat0: f64, e: f64, qp: f64, rq: f64) -> (f64, f64) {
    if FRAC_PI_2 - lat0.abs() < 1e-10 {
        return (FRAC_PI_2.copysign(lat0), 1.0);
    }
    let beta0 = (q_authalic(lat0, e) / qp).clamp(-1.0, 1.0).asin();
    let m0 = lat0.cos() / (1.0 - WGS84_E2 * lat0.sin().powi(2)).sqrt();
    (beta0, WGS84_A * m0 / (rq * beta0.cos()))
}

/// Snyder's `q` (eq. 3-12), odd in `lat` to the bit: computed as written
/// for a southern latitude, the logarithm's quotient rounds differently,
/// `q(−90°)/q(90°)` misses −1, and `asin` turns that ulp into 20 cm at the
/// south pole.
fn q_authalic(lat: f64, e: f64) -> f64 {
    let sin_lat = lat.abs().sin();
    let e_sin = e * sin_lat;
    let q = (1.0 - e * e)
        * (sin_lat / (1.0 - e_sin * e_sin)
            - (1.0 / (2.0 * e)) * ((1.0 - e_sin) / (1.0 + e_sin)).ln());
    q.copysign(lat)
}

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

/// A Lambert Conformal Conic's constants on its Earth model, derived once
/// per call and shared by forward and inverse.
struct Lcc {
    /// Semi-major axis, or the sphere's radius (metres).
    a: f64,
    /// Eccentricity; 0 on a sphere, where Snyder's ellipsoidal formulas
    /// reduce to the spherical ones.
    e: f64,
    /// Cone constant.
    n: f64,
    f: f64,
    rho0: f64,
    lon0: f64,
    false_e: f64,
    false_n: f64,
}

impl Lcc {
    /// The cone constant `n`, `F` and `ρ0` (Snyder eqs. 15-8 to 15-10).
    ///
    /// A tangent cone — one standard parallel, `lat1 == lat2`, as in the
    /// MEPS/MetCoOp QueryData and GRIB grids — makes eq. 15-8 `0/0`, and the
    /// NaN it produced projected every point to NaN. Its limit is
    /// `n = sin φ1` (Snyder p. 108), which is also PROJ's choice below the
    /// same `1e-10` rad threshold.
    fn new(
        lat1: f64,
        lat2: f64,
        lat0: f64,
        lon0: f64,
        false_e: f64,
        false_n: f64,
        radius: Option<f64>,
    ) -> Self {
        let (a, e2) = radius.map_or((WGS84_A, WGS84_E2), |r| (r, 0.0));
        let e = e2.sqrt();
        let m1 = lcc_m(lat1, e2);
        let t1 = lcc_t(lat1, e);
        let n = if (lat1 - lat2).abs() < 1e-10 {
            lat1.sin()
        } else {
            (m1.ln() - lcc_m(lat2, e2).ln()) / (t1.ln() - lcc_t(lat2, e).ln())
        };
        let f = m1 / (n * t1.powf(n));
        let rho0 = a * f * lcc_t(lat0, e).powf(n);
        Self {
            a,
            e,
            n,
            f,
            rho0,
            lon0,
            false_e,
            false_n,
        }
    }

    /// `(lat, lon)` in radians to `(x, y)` metres.
    fn forward(&self, lat: f64, lon: f64) -> (f64, f64) {
        let rho = self.a * self.f * lcc_t(lat, self.e).powf(self.n);
        let theta = self.n * (lon - self.lon0);
        (
            self.false_e + rho * theta.sin(),
            self.false_n + self.rho0 - rho * theta.cos(),
        )
    }

    /// `(x, y)` metres to `(lat, lon)` in radians.
    fn inverse(&self, x: f64, y: f64) -> (f64, f64) {
        let (n, e) = (self.n, self.e);
        let xp = x - self.false_e;
        let yp = self.rho0 - (y - self.false_n);

        let rho = (xp * xp + yp * yp).sqrt().copysign(n);
        let t = (rho / (self.a * self.f)).powf(1.0 / n);
        let theta = xp.atan2(yp);

        let lon = theta / n + self.lon0;

        // Iterative inverse (exact after one step on a sphere, e = 0).
        let mut lat = PI / 2.0 - 2.0 * t.atan();
        for _ in 0..10 {
            let e_sin = e * lat.sin();
            let new_lat =
                PI / 2.0 - 2.0 * (t * ((1.0 - e_sin) / (1.0 + e_sin)).powf(e / 2.0)).atan();
            if (new_lat - lat).abs() < 1e-12 {
                break;
            }
            lat = new_lat;
        }

        (lat, lon)
    }
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

/// The scale factor at the pole, `k₀` of a polar [`Crs::Stereographic`],
/// of a projection on WGS84 whose scale is true at latitude `lat_ts`
/// (radians): the standard parallel of EPSG method 9829, PROJ's `+lat_ts`
/// and CF's `standard_parallel`. `lat_ts` lies on the pole's side of the
/// equator, measured toward that pole: a south-polar projection's −71° is
/// passed as 71°.
///
/// PROJ's `+proj=stere +lat_ts` uses Snyder eq. 21-39, `ρ = a·m_c·t/t_c`,
/// and [`Crs::Stereographic`]'s polar form is eq. 21-33, `ρ = 2·a·k₀·t/D`,
/// so they agree for
///
/// ```text
/// k₀  = m_c · D / (2 · t_c)
/// m_c = cos(lat_ts) / √(1 − e²·sin²(lat_ts))
/// t_c = tan(π/4 − lat_ts/2) · ((1 + e·sin lat_ts)/(1 − e·sin lat_ts))^(e/2)
/// D   = √((1+e)^(1+e) · (1−e)^(1−e))
/// ```
///
/// (Snyder, "Map Projections — A Working Manual", USGS PP 1395, 1987). The
/// spherical shortcut `(1 + sin lat_ts)/2` is 0.012 % off at 60°, some
/// 200 m at 3000 km from the pole. Scale true at the pole itself is
/// `k₀ = 1`, the limit as `t_c → 0`.
pub fn polar_stereographic_k0(lat_ts: f64) -> f64 {
    if FRAC_PI_2 - lat_ts < 1e-10 {
        return 1.0;
    }
    let e2 = WGS84_E2;
    let e = e2.sqrt();
    let sin_lat_ts = lat_ts.sin();
    let cos_lat_ts = lat_ts.cos();
    let m_c = cos_lat_ts / (1.0 - e2 * sin_lat_ts * sin_lat_ts).sqrt();
    let t_c = (std::f64::consts::FRAC_PI_4 - lat_ts / 2.0).tan()
        * ((1.0 + e * sin_lat_ts) / (1.0 - e * sin_lat_ts)).powf(e / 2.0);
    let d = ((1.0 + e).powf(1.0 + e) * (1.0 - e).powf(1.0 - e)).sqrt();
    m_c * d / (2.0 * t_c)
}

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

// ============================================================================
// Geostationary satellite view — GOES-R ABI, Himawari AHI, MTG FCI, GK2A AMI
// A port of PROJ's `geos` (src/projections/geos.cpp): the ellipsoidal
// formulas, which reduce to the spherical ones when semi_minor == semi_major.
// ============================================================================

/// Derived constants of a [`Crs::Geostationary`], in PROJ's notation.
/// Lengths are in units of the semi-major axis.
struct Geos {
    lon0: f64,
    semi_major: f64,
    /// Satellite height above the ellipsoid, `h / a`.
    radius_g_1: f64,
    /// Satellite distance from the Earth's centre, `1 + h / a`.
    radius_g: f64,
    /// `radius_g² − 1`.
    c: f64,
    /// `b / a`, and its square and inverse square.
    radius_p: f64,
    radius_p2: f64,
    radius_p_inv2: f64,
    sweep: SweepAxis,
}

impl Geos {
    fn from_crs(crs: &Crs) -> Option<Self> {
        let Crs::Geostationary {
            lon0,
            height,
            semi_major,
            semi_minor,
            sweep,
        } = *crs
        else {
            return None;
        };
        let radius_g_1 = height / semi_major;
        let radius_g = 1.0 + radius_g_1;
        let radius_p = semi_minor / semi_major;
        Some(Geos {
            lon0,
            semi_major,
            radius_g_1,
            radius_g,
            c: radius_g * radius_g - 1.0,
            radius_p,
            radius_p2: radius_p * radius_p,
            radius_p_inv2: 1.0 / (radius_p * radius_p),
            sweep,
        })
    }

    /// Satellite→surface vector for geodetic `(lat, lon)` (radians), and
    /// whether the satellite sees that point (PROJ's visibility test).
    fn view_vector(&self, lat: f64, lon: f64) -> ((f64, f64, f64), bool) {
        let lam = lon - self.lon0;
        // Geocentric latitude, then the point's distance from the centre.
        let phi = (self.radius_p2 * lat.tan()).atan();
        let r = self.radius_p / (self.radius_p * phi.cos()).hypot(phi.sin());
        let vx = r * lam.cos() * phi.cos();
        let vy = r * lam.sin() * phi.cos();
        let vz = r * phi.sin();
        let visible = (self.radius_g - vx) * vx - vy * vy - vz * vz * self.radius_p_inv2 >= 0.0;
        ((vx, vy, vz), visible)
    }

    /// Geodetic `(lat, lon)` (radians) to projected metres, or `None` when
    /// the point is on the far side of the Earth.
    fn forward(&self, lat: f64, lon: f64) -> Option<(f64, f64)> {
        let ((vx, vy, vz), visible) = self.view_vector(lat, lon);
        if !visible {
            return None;
        }
        let tmp = self.radius_g - vx;
        let (x, y) = match self.sweep {
            SweepAxis::X => ((vy / vz.hypot(tmp)).atan(), (vz / tmp).atan()),
            SweepAxis::Y => ((vy / tmp).atan(), (vz / vy.hypot(tmp)).atan()),
        };
        let scale = self.radius_g_1 * self.semi_major;
        Some((x * scale, y * scale))
    }

    /// Projected metres to geodetic `(lat, lon)` (radians, lon unwrapped),
    /// or `None` off the Earth disk.
    fn inverse(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let scale = self.radius_g_1 * self.semi_major;
        let (ax, ay) = (x / scale, y / scale);
        let vx = -1.0;
        let (vy, vz) = match self.sweep {
            SweepAxis::X => {
                let vz = ay.tan();
                (ax.tan() * 1.0_f64.hypot(vz), vz)
            }
            SweepAxis::Y => {
                let vy = ax.tan();
                (vy, ay.tan() * 1.0_f64.hypot(vy))
            }
        };
        // Intersect the view ray with the ellipsoid: a quadratic in the ray
        // length whose determinant is negative when the ray misses the Earth.
        let vz_p = vz / self.radius_p;
        let a = vy * vy + vz_p * vz_p + vx * vx;
        let b = 2.0 * self.radius_g * vx;
        let det = b * b - 4.0 * a * self.c;
        if det.is_nan() || det < 0.0 {
            return None;
        }
        let k = (-b - det.sqrt()) / (2.0 * a);
        let (vx, vy, vz) = (self.radius_g + k * vx, vy * k, vz * k);
        let lam = vy.atan2(vx);
        let phi = (vz * lam.cos() / vx).atan();
        let phi = (self.radius_p_inv2 * phi.tan()).atan();
        Some((phi, lam + self.lon0))
    }

    /// The four extreme points of the visible disk, as geodetic `(lat, lon)`
    /// radians just inside the limb: northernmost and southernmost (on the
    /// sub-satellite meridian), easternmost and westernmost (on the
    /// equator). The disk is symmetric about both planes, and each extreme
    /// is found by bisecting the visibility test.
    fn limb_extremes(&self) -> [(f64, f64); 4] {
        let edge = |point: &dyn Fn(f64) -> (f64, f64)| {
            let (mut inside, mut outside) = (0.0, std::f64::consts::FRAC_PI_2);
            for _ in 0..60 {
                let mid = 0.5 * (inside + outside);
                let (lat, lon) = point(mid);
                if self.view_vector(lat, lon).1 {
                    inside = mid;
                } else {
                    outside = mid;
                }
            }
            point(inside)
        };
        [
            edge(&|t| (t, self.lon0)),
            edge(&|t| (-t, self.lon0)),
            edge(&|t| (0.0, self.lon0 + t)),
            edge(&|t| (0.0, self.lon0 - t)),
        ]
    }
}

/// Longitude/latitude extent accumulated relative to a reference meridian,
/// so a region straddling the antimeridian yields `west > east` rather than
/// a spurious −180…180 box (the geostationary disks of GOES-West, Himawari
/// and GK2A all cross it).
struct SeamExtent {
    lon_ref: f64,
    min_rel: f64,
    max_rel: f64,
    min_lat: f64,
    max_lat: f64,
}

impl SeamExtent {
    fn new(lon_ref: f64) -> Self {
        SeamExtent {
            lon_ref,
            min_rel: f64::MAX,
            max_rel: f64::MIN,
            min_lat: f64::MAX,
            max_lat: f64::MIN,
        }
    }

    fn add(&mut self, lon: f64, lat: f64) {
        let rel = wrap_lon(lon - self.lon_ref);
        self.min_rel = self.min_rel.min(rel);
        self.max_rel = self.max_rel.max(rel);
        self.min_lat = self.min_lat.min(lat);
        self.max_lat = self.max_lat.max(lat);
    }

    /// `[west, south, east, north]`, or the `[MAX, MAX, MIN, MIN]` sentinel
    /// when nothing was added (as [`GeoTransform::bbox`] documents).
    fn finish(&self) -> [f64; 4] {
        if self.min_rel > self.max_rel {
            return [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
        }
        if self.max_rel - self.min_rel >= 360.0 {
            return [-180.0, self.min_lat, 180.0, self.max_lat];
        }
        [
            wrap_lon(self.lon_ref + self.min_rel),
            self.min_lat,
            wrap_lon(self.lon_ref + self.max_rel),
            self.max_lat,
        ]
    }
}

/// Bisect the segment from `inside` (mapping to `Some`) to `outside`
/// (mapping to `None`) and return the mapped value nearest the boundary on
/// the inside. Used to find where a raster edge or bbox edge crosses the
/// limb of a geostationary disk.
fn bisect_boundary<T>(
    inside: (f64, f64),
    outside: (f64, f64),
    map: impl Fn(f64, f64) -> Option<T>,
) -> Option<T> {
    let (mut a, mut b) = (inside, outside);
    for _ in 0..40 {
        let mid = (0.5 * (a.0 + b.0), 0.5 * (a.1 + b.1));
        if map(mid.0, mid.1).is_some() {
            a = mid;
        } else {
            b = mid;
        }
    }
    map(a.0, a.1)
}

/// Sample `n` segments along the path `edge(t)`, `t ∈ [0, 1]`,
/// passing every point that maps to `Some` to `visit` — plus, where
/// consecutive samples straddle a domain boundary, the boundary point found
/// by bisection. The inverse and forward geostationary transforms are both
/// partial, so edge sampling alone would miss where an edge meets the limb.
fn visit_edge_with_boundaries<T>(
    n: usize,
    edge: impl Fn(f64) -> (f64, f64),
    map: impl Fn(f64, f64) -> Option<T>,
    mut visit: impl FnMut(T),
) {
    let mut prev: Option<((f64, f64), bool)> = None;
    for i in 0..=n {
        let p = edge(i as f64 / n as f64);
        let mapped = map(p.0, p.1);
        let ok = mapped.is_some();
        if let Some(value) = mapped {
            visit(value);
        }
        if let Some((q, q_ok)) = prev {
            if q_ok != ok {
                let (inside, outside) = if ok { (p, q) } else { (q, p) };
                if let Some(value) = bisect_boundary(inside, outside, &map) {
                    visit(value);
                }
            }
        }
        prev = Some((p, ok));
    }
}

// ---------------------------------------------------------------------------
// Vector geometry → output pixel space (for ds-render's polygon fill, #397)
// ---------------------------------------------------------------------------

/// A single polygon projected into output **pixel** space: an exterior ring
/// plus interior rings (holes), each a list of `[x, y]` pixel coordinates.
/// Feeds directly into `ds_render::rasterize::fill_polygon`.
#[derive(Debug, Clone, PartialEq)]
pub struct PixelPolygon {
    /// Exterior ring, pixel coordinates.
    pub exterior: Vec<[f64; 2]>,
    /// Interior rings (holes), pixel coordinates.
    pub holes: Vec<Vec<[f64; 2]>>,
}

/// A [`Geometry`] projected into output pixel space for one render request —
/// the bridge between [`OutputCrs`] projection (here, in `ds-core`) and the
/// pure scanline fill (`ds_render::rasterize::fill_polygon`), keeping CRS math
/// in one place and the fill math in another.
///
/// **Pixel convention** — top-left origin: `x` increases east (`x = 0` at the
/// west/left edge, `x = width` at the east/right edge), `y` increases south
/// (`y = 0` at the north/top edge, `y = height` at the bottom). A vertex
/// off-tile keeps its projected pixel value (the fill clips to the tile) and a
/// vertex outside a projection's valid domain can be non-finite (the
/// `Projected` forward may not converge) — `fill_polygon` skips edges with a
/// non-finite endpoint, so vertex topology is preserved as-is here. Non-polygon
/// geometries (`Point`, `Null`) yield no polygons.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PixelGeometry {
    /// One entry per polygon (a `Polygon` yields one; a `MultiPolygon`, one
    /// per member).
    pub polygons: Vec<PixelPolygon>,
}

/// Project a [`Geometry`]'s polygon rings into output **pixel** space for a
/// render request, so they can be filled by `ds_render::rasterize::fill_polygon`.
///
/// Only the polygonal parts contribute: `Polygon` → one [`PixelPolygon`],
/// `MultiPolygon` → one per member, `Point`/`Null` → none. Each ring vertex
/// `[lon, lat]` (WGS84 degrees) is mapped through [`OutputCrs::world_to_fraction`]
/// (projecting **vertices only**, never per output pixel — #203) and scaled by
/// `width`/`height`.
///
/// `wgs84_bbox` is the request's WGS84 bounding box `[west, south, east, north]`
/// (used by the `Wgs84`/`WebMercator` variants; the `Projected` variant carries
/// its own projected extents and ignores it — the name matches the downstream
/// [`OutputCrs::world_to_fraction`] parameter and flags the WGS84-only
/// semantics at the call site). See [`PixelGeometry`] for the pixel coordinate
/// convention and out-of-domain handling.
///
/// **Antimeridian (v1 limitation):** vertices are projected independently with
/// no ±180° seam handling, so a polygon ring that crosses the antimeridian — or
/// a request bbox spanning it — is not split and will render incorrectly (a
/// ring vertex at +179° and one at −179° map to opposite edges of the tile,
/// filling the wrong span between them). Callers must pre-split such polygons.
/// Antimeridian splitting is a deferred follow-up (#397); small alert/SIGMET
/// polygons away from ±180° are unaffected.
pub fn geometry_to_pixels(
    geom: &Geometry,
    wgs84_bbox: [f64; 4],
    width: u32,
    height: u32,
    output_crs: &OutputCrs,
) -> PixelGeometry {
    let (w, h) = (width as f64, height as f64);
    let project_ring = |ring: &[[f64; 2]]| -> Vec<[f64; 2]> {
        ring.iter()
            .map(|&[lon, lat]| {
                let (fx, fy) = output_crs.world_to_fraction(wgs84_bbox, lon, lat);
                [fx * w, fy * h]
            })
            .collect()
    };
    let project_polygon = |exterior: &[[f64; 2]], holes: &[Vec<[f64; 2]>]| PixelPolygon {
        exterior: project_ring(exterior),
        holes: holes.iter().map(|ring| project_ring(ring)).collect(),
    };
    let polygons = match geom {
        Geometry::Polygon { exterior, holes } => vec![project_polygon(exterior, holes)],
        Geometry::MultiPolygon { polygons } => polygons
            .iter()
            .map(|(exterior, holes)| project_polygon(exterior, holes))
            .collect(),
        Geometry::Point { .. } | Geometry::Null => Vec::new(),
    };
    PixelGeometry { polygons }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crs84_extent_normalizes_advertised_bounds() {
        // Global 0.25° grid cell edges reach past the domain.
        assert_eq!(
            crs84_extent([-180.125, -90.125, 179.875, 90.125]),
            Some([-180.0, -90.0, 180.0, 90.0])
        );
        // The empty-accumulator sentinel and non-finite values describe no area.
        assert_eq!(crs84_extent([f64::MAX, f64::MAX, f64::MIN, f64::MIN]), None);
        assert_eq!(crs84_extent([f64::NAN, 0.0, 1.0, 1.0]), None);
        // In-domain boxes pass through, including an antimeridian crossing.
        assert_eq!(
            crs84_extent([19.0, 59.0, 32.0, 70.0]),
            Some([19.0, 59.0, 32.0, 70.0])
        );
        assert_eq!(
            crs84_extent([170.0, 10.0, -170.0, 20.0]),
            Some([170.0, 10.0, -170.0, 20.0])
        );
        // A partly overhanging box is clamped, not dropped.
        assert_eq!(
            crs84_extent([-180.5, -10.0, -170.0, 91.0]),
            Some([-180.0, -10.0, -170.0, 90.0])
        );
    }

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
    fn destination_point_matches_analytic_references() {
        // Pin against exact analytic cases (not a self-roundtrip): on a sphere,
        // a meridian/equator path has a closed form independent of this code.
        let ten_deg = EARTH_RADIUS_M * (10.0_f64).to_radians();
        // Due north from the prime-meridian origin → straight up the meridian:
        // lon unchanged, lat = arc angle.
        let (lon, lat) = destination_point(0.0, 0.0, ten_deg, 0.0);
        assert!(lon.abs() < 1e-9, "north keeps lon 0, got {lon}");
        assert!(
            (lat - 10.0).abs() < 1e-9,
            "north 10° arc → lat 10, got {lat}"
        );
        // Due east along the equator → lat stays 0, lon = arc angle.
        let (lon, lat) = destination_point(0.0, 0.0, ten_deg, 90.0);
        assert!(lat.abs() < 1e-9, "east on equator keeps lat 0, got {lat}");
        assert!(
            (lon - 10.0).abs() < 1e-9,
            "east 10° arc → lon 10, got {lon}"
        );
        // Eastbound across the antimeridian wraps into (−180, 180]: 179 + 10 → −171.
        let (lon, _lat) = destination_point(179.0, 0.0, ten_deg, 90.0);
        assert!((lon - (-171.0)).abs() < 1e-9, "wrapped past +180 → {lon}");
    }

    #[test]
    fn intermediate_point_follows_the_short_great_circle() {
        let close = |(lon, lat): (f64, f64), want: (f64, f64)| {
            assert!(
                (lon - want.0).abs() < 1e-9 && (lat - want.1).abs() < 1e-9,
                "got ({lon}, {lat}), want {want:?}"
            );
        };
        // Endpoints are exact; a meridian and the equator are closed forms.
        close(
            intermediate_point(10.0, 20.0, 30.0, 40.0, 0.0).unwrap(),
            (10.0, 20.0),
        );
        close(
            intermediate_point(10.0, 20.0, 30.0, 40.0, 1.0).unwrap(),
            (30.0, 40.0),
        );
        close(
            intermediate_point(0.0, 0.0, 0.0, 10.0, 0.5).unwrap(),
            (0.0, 5.0),
        );
        close(
            intermediate_point(0.0, 0.0, 20.0, 0.0, 0.25).unwrap(),
            (5.0, 0.0),
        );
        // The antimeridian seam (170,10 → −170,20): the short way crosses
        // ±180, never the Greenwich meridian, and wraps into (−180, 180].
        let (lon, lat) = intermediate_point(170.0, 10.0, -170.0, 20.0, 0.5).unwrap();
        assert!(lon.abs() > 175.0, "midpoint crosses the seam, got {lon}");
        assert!((14.0..16.0).contains(&lat), "lat {lat}");
        close(
            intermediate_point(179.0, 0.0, -179.0, 0.0, 0.75).unwrap(),
            (-179.5, 0.0),
        );
        // Off the equator a great circle bulges poleward of the rhumb line,
        // and its midpoint is equidistant from both ends.
        let mid = intermediate_point(0.0, 60.0, 90.0, 60.0, 0.5).unwrap();
        assert!(mid.1 > 60.0, "poleward bulge, got {mid:?}");
        let (d0, d1) = (
            great_circle_distance_m(0.0, 60.0, mid.0, mid.1),
            great_circle_distance_m(90.0, 60.0, mid.0, mid.1),
        );
        assert!((d0 - d1).abs() < 1e-3, "{d0} vs {d1}");
        // Antipodal endpoints define no single path; coincident ones do.
        assert!(intermediate_point(0.0, 0.0, 180.0, 0.0, 0.5).is_none());
        close(
            intermediate_point(5.0, 5.0, 5.0, 5.0, 0.5).unwrap(),
            (5.0, 5.0),
        );
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
        // transforms, the envelope is None — never a fabricated or
        // global-extent box that would trigger a whole-dataset read. The
        // caller tells a box wholly outside the projection (the empty map)
        // from one enclosing what its edges miss (a 400) with
        // `outside_projection_domain`; see its test for real CRSs.
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

    /// `outside_projection_domain` says whether any point of a box maps to
    /// the Earth, checked here against a 41 × 41 grid of points. It also sees
    /// a valid area the box encloses, where the edge samples of
    /// `wgs84_envelope` find nothing (#1089). TM's area is the square
    /// around its equator's point on the central meridian of half-side k₀
    /// times a quarter meridian, 9 997 964.943 m for EPSG:3067 (#1113,
    /// #1130).
    #[test]
    fn outside_projection_domain_matches_the_inverse() {
        let laea = projected_output_crs("EPSG:3035").unwrap();
        let tm = projected_output_crs("EPSG:3067").unwrap();
        let meets = |crs: &Crs, [x0, y0, x1, y1]: [f64; 4]| {
            let at = |lo: f64, hi: f64, k: u32| lo + (hi - lo) * f64::from(k) / 40.0;
            (0..=40).any(|i| (0..=40).any(|j| maps_to_earth(crs, at(x0, x1, i), at(y0, y1, j))))
        };
        // (crs, box, wholly outside, has an envelope). The LAEA disk is
        // centred at (4 321 000, 3 210 000), its radius about 12 742 km.
        for (crs, bbox, outside, envelope) in [
            // Europe.
            (&laea, [4.0e6, 3.0e6, 4.5e6, 3.5e6], false, true),
            // Across the rim, east of the centre.
            (&laea, [1.6e7, 2.2e6, 1.8e7, 4.2e6], false, true),
            // Past the rim: east, west, north, south.
            (&laea, [1.8e7, 2.2e6, 1.9e7, 4.2e6], true, false),
            (&laea, [1.71e7, 3.0e6, 1.72e7, 3.1e6], true, false),
            (&laea, [-1.5e7, 2.2e6, -1.0e7, 4.2e6], true, false),
            (&laea, [3.0e6, 1.7e7, 5.0e6, 1.8e7], true, false),
            (&laea, [3.0e6, -1.2e7, 5.0e6, -1.0e7], true, false),
            // Past it diagonally, though each axis range overlaps the disk's.
            (&laea, [1.4e7, 1.2e7, 1.5e7, 1.3e7], true, false),
            // Wider than the disk: the edges miss it, the box encloses it.
            (&laea, [-3.0e7, -3.0e7, 3.0e7, 3.0e7], false, false),
            // Tall: its west edge crosses the disk only for northings of
            // about 1.9e6 to 4.5e6, between the edge samples at 0 and 5e6.
            (&laea, [1.7e7, -5.0e7, 1.8e7, 5.0e7], false, false),
            // Finland.
            (&tm, [-1.2e5, 6.3e6, 8.8e5, 7.9e6], false, true),
            // The #1110 meta-tile, whose north-east corner, 62° east of the
            // meridian, the Snyder series could not place (#1113).
            (
                &tm,
                [1_448_154.7, 7_240_773.4, 2_896_309.4, 8_688_928.1],
                false,
                true,
            ),
            // Across the North Pole.
            (&tm, [0.0, 9.0e6, 1.0e6, 1.1e7], false, true),
            // On the equator, 44° to 48° east.
            (&tm, [6.0e6, -1.0e5, 6.6e6, 1.0e5], false, true),
            // Past the North Pole, on the far side of the Earth, and on past
            // the far side's equator.
            (&tm, [0.0, 1.5e7, 1.0e6, 1.6e7], true, false),
            (&tm, [0.0, 1.9e7, 1.0e6, 2.1e7], true, false),
            (&tm, [0.0, 2.1e7, 1.0e6, 2.2e7], true, false),
            (&tm, [0.0, -2.2e7, 1.0e6, -2.1e7], true, false),
            (&tm, [-5.0e7, 2.1e7, 5.0e7, 2.2e7], true, false),
            // At Finland's northings, 59° to 77° east and 49° to 63° west
            // of the meridian, past the Snyder series' 50° (#1113).
            (&tm, [5.0e6, 6.5e6, 6.0e6, 7.0e6], false, true),
            (&tm, [-5.0e6, 6.5e6, -4.0e6, 7.0e6], false, true),
            // Across the area's east and west edges, 10 497 964.943 and
            // -9 497 964.943, and past them, where the series is no longer
            // exact.
            (&tm, [1.0e7, -1.0e6, 1.1e7, 1.0e6], false, true),
            (&tm, [-1.0e7, 6.5e6, -9.0e6, 7.0e6], false, true),
            (&tm, [1.05e7, 6.5e6, 1.1e7, 7.0e6], true, false),
            (&tm, [-1.0e7, -1.0e6, -9.5e6, 1.0e6], true, false),
            // Eastings so far out that the TM inverse has no finite result,
            // each side.
            (&tm, [1.0e20, 6.5e6, 2.0e20, 7.0e6], true, false),
            (&tm, [-2.0e20, 6.5e6, -1.0e20, 7.0e6], true, false),
            // Wider than the area: the edges miss it, the box encloses it,
            // and both poles, which bound it.
            (&tm, [-3.0e7, -3.0e7, 3.0e7, 3.0e7], false, true),
        ] {
            assert_eq!(outside_projection_domain(crs, bbox), outside, "{bbox:?}");
            assert_eq!(meets(crs, bbox), !outside, "{bbox:?}");
            assert_eq!(wgs84_envelope(crs, bbox).is_some(), envelope, "{bbox:?}");
        }
        // The area's north and south edge, a pole's northing: against the
        // WGS84 quarter meridian of 10 001 965.729 m, EPSG:3067's poles lie
        // 0.9996 · 10 001 965.729 = 9 997 964.943 m from the equator.
        assert_eq!(tm_equator_northing(0.0, 0.9996, 0.0), 0.0);
        for (north, outside) in [(9_997_000.0, false), (9_999_000.0, true)] {
            for bbox in [[0.0, north, 1.0e6, 2.1e7], [0.0, -2.1e7, 1.0e6, -north]] {
                assert_eq!(outside_projection_domain(&tm, bbox), outside, "{bbox:?}");
            }
        }
        // No other variant is an output CRS: never known to be outside.
        assert!(!outside_projection_domain(
            &Crs::Wgs84,
            [1.0e300, 1.0e300, 1.5e300, 1.5e300]
        ));
        // An inverted box does not panic.
        outside_projection_domain(&laea, [1.0, 1.0, 0.0, 0.0]);
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

    /// EPSG:3067's Transverse Mercator both ways against PROJ 9.8.1's
    /// `tmerc`, which sums the same Krüger series (#1130). Forward
    /// `cs2cs -f %.6f +proj=longlat +datum=WGS84 +to +proj=tmerc +lat_0=0
    /// +lon_0=27 +k=0.9996 +x_0=500000 +y_0=0 +datum=WGS84`, inverse
    /// `cs2cs -f %.12f +proj=tmerc +lat_0=0 +lon_0=27 +k=0.9996 +x_0=500000
    /// +y_0=0 +datum=WGS84 +to +proj=longlat +datum=WGS84`. EPSG:3067 itself
    /// is on GRS80, whose flattening moves these points by 0.1 mm at most.
    /// Places 0°, 30°, 50°, 70° and 85° from the meridian, a tenth of a
    /// degree from each pole, 90° out, and the area's corner; `None` past
    /// the area's edge on the equator, which PROJ answers out to about
    /// 80.5°, and past each pole.
    #[test]
    fn tm35fin_matches_proj_both_ways_out_to_the_edge_of_its_area() {
        let tm = projected_output_crs("EPSG:3067").unwrap();
        // (longitude, latitude, PROJ easting, northing)
        for (lon, lat, e, n) in [
            (27.0, 60.0, 500_000.0, 6_651_411.190_363),
            (57.0, 0.0, 4_003_410.936_147, 0.0),
            (57.0, 45.0, 2_859_847.360_033, 5_440_824.092_18),
            (77.0, 0.0, 6_952_810.991_415, 0.0),
            // West Iceland and Cape Farewell, past the old bound at 23°W
            // (#1113).
            (-23.0, 64.0, -1_733_433.793_648, 8_055_898.753_958),
            (-43.0, 60.0, -2_758_677.999_706, 8_752_133.641_064),
            (97.0, 30.0, 7_757_639.034_713, 6_598_258.963_854),
            (112.0, 60.0, 3_994_363.401_504, 9_676_700.451_656),
            (-58.0, 75.0, -1_187_060.909_154, 9_848_636.264_365),
            (77.0, 89.9, 508_552.833_401, 9_990_788.260_025),
            (-43.0, -89.9, 489_508.393_51, -9_994_146.308_61),
            // 90° out lies on the poles' northing.
            (117.0, 45.0, 6_124_969.706_223, 9_997_964.943_021),
            // 66° east on the equator, inside the edge at 66.29°.
            (93.0, 0.0, 10_416_469.103_711, 0.0),
        ] {
            let (x, y) = tm.forward(lon, lat);
            assert!(
                (x - e).abs() < 1e-6 && (y - n).abs() < 1e-6,
                "forward({lon}, {lat}) = ({x}, {y}), PROJ ({e}, {n})"
            );
            // The two truncated series undo each other to 0.2 mm at the
            // area's edge, as in PROJ: the inverse table below pins its
            // inverse of the last point.
            let (back_lon, back_lat) = tm.inverse(e, n).unwrap();
            let off = great_circle_distance_m(back_lon, back_lat, lon, lat);
            assert!(
                off < 2e-4,
                "inverse({e}, {n}) = ({back_lon}, {back_lat}), ({lon}, {lat}): {off} m"
            );
        }
        // (easting, northing, PROJ longitude, latitude)
        for (e, n, lon, lat) in [
            (
                385_611.316_7,
                6_672_118.380_3,
                24.938_400_000_260,
                60.169_899_999_779,
            ),
            (
                -9_000_000.0,
                3_000_000.0,
                -39.944_935_355_950,
                11.177_476_006_966,
            ),
            (
                10_000_000.0,
                1_000_000.0,
                91.733_854_525_468,
                3.829_538_675_150,
            ),
            (10_400_000.0, 0.0, 92.940_774_462_260, 0.0),
            (10_416_469.103_711, 0.0, 92.999_999_998_890, 0.0),
            (
                4_000_000.0,
                9_500_000.0,
                109.295_272_735_154,
                59.780_888_391_314,
            ),
            (
                -2_000_000.0,
                -8_000_000.0,
                -25.526_215_498_894,
                -62.022_753_081_320,
            ),
            // The #1110 meta-tile's north-east corner, 62° east, which the
            // Snyder series could not place (#1113).
            (
                2_896_309.4,
                8_688_928.1,
                89.076_769_352_368,
                66.084_369_520_925,
            ),
            (500_000.0, 9_997_000.0, 27.0, 89.991_357_375_245),
            (
                6_000_000.0,
                9_997_964.943,
                116.999_999_999_806,
                45.802_232_611_787,
            ),
            // The area's north-west corner, a fiftieth of a millimetre in.
            (
                -9_497_964.0,
                9_997_964.943,
                -62.999_999_999_917,
                23.407_095_063_655,
            ),
        ] {
            let (got_lon, got_lat) = tm.inverse(e, n).unwrap();
            let off = great_circle_distance_m(got_lon, got_lat, lon, lat);
            assert!(
                off < 1e-5,
                "inverse({e}, {n}) = ({got_lon}, {got_lat}), PROJ ({lon}, {lat}): {off} m"
            );
        }
        // The poles, projected and inverted again: a rounding error past
        // their northing still names them.
        for pole in [90.0, -90.0] {
            let (x, y) = tm.forward(27.0, pole);
            let (lon, lat) = tm.inverse(x, y).unwrap();
            assert!(lon == 27.0 && lat == pole, "({x}, {y}): {lon}, {lat}");
        }
        for (e, n) in [
            // On the equator 67° west and 66.5° and 70° east, past the
            // edge at 66.29°: PROJ's forward of 40°W, 93.5°E and 97°E.
            (-9_700_752.241_974, 0.0),
            (10_557_119.801_428, 0.0),
            (11_634_054.513_902, 0.0),
            // A kilometre past each pole, on the far meridian.
            (500_000.0, 9_999_000.0),
            (500_000.0, -9_999_000.0),
            // So far east that the series has no finite answer.
            (1.0e20, 6_500_000.0),
            (f64::NAN, 6_500_000.0),
        ] {
            assert_eq!(tm.inverse(e, n), None, "({e}, {n})");
        }
    }

    /// Transverse Mercator measures its area from the central meridian
    /// unwrapped, so a meridian near the antimeridian answers across it:
    /// UTM zone 60N (177°E) at the corners of the seam box 170,10,-170,20,
    /// 70° either side and 85° east at high latitude. References are PROJ
    /// 9.8.1, `cs2cs -f %.6f +proj=longlat +datum=WGS84 +to +proj=utm
    /// +zone=60 +datum=WGS84` forward and `cs2cs -f %.12f +proj=utm
    /// +zone=60 +datum=WGS84 +to +proj=longlat +datum=WGS84` inverse. The
    /// inverse's longitude is 177° plus the offset, unwrapped, so the seam
    /// box's envelope runs on past 180°.
    #[test]
    fn tm_area_follows_a_meridian_near_the_antimeridian() {
        let utm60 = Crs::TransverseMercator {
            lat0: 0.0,
            lon0: 177.0_f64.to_radians(),
            k0: 0.9996,
            false_e: 500_000.0,
            false_n: 0.0,
        };
        // (longitude, latitude, PROJ easting, northing)
        for (lon, lat, e, n) in [
            (170.0, 10.0, -268_980.132_218, 1_113_599.929_721),
            (-170.0, 10.0, 1_936_441.770_924, 1_134_078.785_879),
            (170.0, 20.0, -233_644.415_888, 2_226_862.893_669),
            (-170.0, 20.0, 1_868_929.815_994, 2_265_245.961_019),
            (-113.0, 60.0, 3_758_677.999_706, 8_752_133.641_064),
            (107.0, 60.0, -2_758_677.999_706, 8_752_133.641_064),
            (-98.0, 70.0, 2_769_394.615_151, 9_795_195.992_305),
        ] {
            let (x, y) = utm60.forward(lon, lat);
            assert!(
                (x - e).abs() < 1e-6 && (y - n).abs() < 1e-6,
                "forward({lon}, {lat}) = ({x}, {y}), PROJ ({e}, {n})"
            );
        }
        // (easting, northing, PROJ longitude, latitude)
        for (e, n, lon, lat) in [
            (
                -268_980.13,
                1_113_599.93,
                170.000_000_020_029,
                10.000_000_002_927,
            ),
            (
                1_936_441.77,
                1_134_078.79,
                -170.000_000_006_741,
                10.000_000_036_636,
            ),
            (5.0e6, 7.0e6, -123.630_924_146_791, 45.102_686_586_664),
            (-4.0e6, 7.0e6, 117.630_924_146_791, 45.102_686_586_664),
            (2.0e6, -9.0e6, -126.288_196_476_995, -74.005_333_893_791),
        ] {
            let (got_lon, got_lat) = utm60.inverse(e, n).unwrap();
            assert!((got_lon - 177.0).abs() < 90.0, "unwrapped: {got_lon}");
            let off = great_circle_distance_m(got_lon, got_lat, lon, lat);
            assert!(
                off < 1e-5,
                "inverse({e}, {n}) = ({got_lon}, {got_lat}), PROJ ({lon}, {lat}): {off} m"
            );
        }
        // Past the area's east and west edges, k₀ times a quarter meridian
        // from 500 000 m.
        for e in [10_498_000.0, -9_498_000.0] {
            assert_eq!(utm60.inverse(e, 7.0e6), None, "{e}");
        }
        let seam = projected_envelope(&utm60, [170.0, 10.0, 190.0, 20.0]);
        let [w, s, e, n] = wgs84_envelope(&utm60, seam).unwrap();
        assert!(
            (165.0..170.0).contains(&w) && (190.0..195.0).contains(&e),
            "lon {w}..{e}"
        );
        assert!(
            (5.0..10.0).contains(&s) && (20.0..25.0).contains(&n),
            "lat {s}..{n}"
        );
    }

    /// A CRS84 box holding a singular point of Transverse Mercator, on the
    /// equator 90° from its meridian, has no easting extreme on its edges:
    /// a global box's edges are the poles and the far meridian, which
    /// EPSG:3067 maps within 3 100 km of its own. Its envelope reaches the
    /// valid area's east and west edges on the equator, 500 000 ± k₀ times
    /// a quarter meridian, as a box across the antimeridian holding 63°W
    /// does on its side. A box short of both keeps its edges' envelope
    /// (#1130). An edge within a few degrees of a singular point maps past
    /// where the series converges, without bound: these boxes keep clear.
    #[test]
    fn tm_projected_envelope_reaches_the_area_around_its_singular_points() {
        let tm = projected_output_crs("EPSG:3067").unwrap();
        let half = 0.9996 * TM_A * FRAC_PI_2;
        let near = |a: f64, b: f64| (a - b).abs() < 1e-6;
        let edges = edge_envelope([-180.0, -90.0, 180.0, 90.0], |lon, lat| {
            Some(tm.forward(lon, lat))
        })
        .unwrap();
        assert!(edges[0] > -3.0e6 && edges[2] < 4.0e6, "{edges:?}");
        let [w, s, e, n] = projected_envelope(&tm, [-180.0, -90.0, 180.0, 90.0]);
        assert!(
            near(w, 500_000.0 - half) && near(e, 500_000.0 + half) && s < -half && n > half,
            "{w}, {s}, {e}, {n}"
        );
        // 63°W only, as 297°E, 40° from the box's sides and 30° from its
        // top and bottom, whose edges stay over a million metres short of
        // the area's west edge.
        let around = [257.0, -30.0, 337.0, 30.0];
        let [edge_w, ..] = edge_envelope(around, |lon, lat| Some(tm.forward(lon, lat))).unwrap();
        assert!(edge_w > 500_000.0 - half + 1.0e6, "{edge_w}");
        let [w, _, e, _] = projected_envelope(&tm, around);
        assert!(near(w, 500_000.0 - half) && e < 500_000.0, "{w}, {e}");
        // Short of both: Africa and Asia, 10°S to 10°N.
        let inside = [-20.0, -10.0, 90.0, 10.0];
        assert_eq!(
            projected_envelope(&tm, inside),
            edge_envelope(inside, |lon, lat| Some(tm.forward(lon, lat))).unwrap()
        );
    }

    /// Krüger's sixth-order series against the exact Transverse Mercator,
    /// which no series truncates (#1130). Conformal, and true to scale on
    /// the central meridian, it is `northing + i·easting = k₀·F(ψ + iλ)`,
    /// `F` the meridian arc `M` as an analytic function of the isometric
    /// latitude `ψ`. Along `ψ + it` that is `M(φ) + i ∫₀^λ ν cos φ dt`, at
    /// the complex latitude `φ` of each `ψ + it`, found by Newton's method.
    /// Simpson's rule on 4 000 steps is good to 1e-7 m there, 1e-6 m at 76°
    /// on the equator. Just inside the area's east edge, from the equator
    /// to 23° north, the series is within 0.6 mm of it forward and 10 µm
    /// inverse; 76° east on the equator, past the edge, it is half a metre
    /// off and more.
    #[test]
    fn tm_series_is_exact_inside_its_area() {
        let tm = projected_output_crs("EPSG:3067").unwrap();
        let (k0, lon0) = (0.9996, 27.0_f64.to_radians());
        // The exact (easting, northing) of (lon, lat), degrees.
        let exact = |lon: f64, lat: f64| {
            let (phi, lam) = (lat.to_radians(), lon.to_radians() - lon0);
            let simpson = |f: &mut dyn FnMut(f64) -> Cx, hi: f64| {
                let steps = 4000;
                let h = hi / f64::from(steps);
                let mut sum = Cx(0.0, 0.0);
                for k in 0..=steps {
                    let w = match k {
                        0 => 1.0,
                        _ if k == steps => 1.0,
                        _ if k % 2 == 1 => 4.0,
                        _ => 2.0,
                    };
                    sum = sum + f(h * f64::from(k)) * w;
                }
                sum * (h / 3.0)
            };
            let e2 = WGS84_E2;
            let e = e2.sqrt();
            let one = Cx(1.0, 0.0);
            let psi_of = |phi: Cx| {
                let s = phi.sin();
                s.atanh() - (s * e).atanh() * e
            };
            let arc = simpson(
                &mut |t| {
                    Cx(
                        WGS84_A * (1.0 - e2) / (1.0 - e2 * t.sin().powi(2)).powf(1.5),
                        0.0,
                    )
                },
                phi,
            )
            .0;
            let psi = psi_of(Cx(phi, 0.0)).0;
            // Continued along the path from the real latitude.
            let mut p = Cx(phi, 0.0);
            let g = simpson(
                &mut |t| {
                    let w = Cx(psi, t);
                    for _ in 0..30 {
                        let s = p.sin();
                        let dpsi = Cx(1.0 - e2, 0.0) / ((one - s * s * e2) * p.cos());
                        let step = (psi_of(p) - w) / dpsi;
                        p = p - step;
                        if step.abs() < 1e-15 {
                            break;
                        }
                    }
                    let s = p.sin();
                    p.cos() * WGS84_A / (one - s * s * e2).sqrt()
                },
                lam,
            );
            (500_000.0 + k0 * g.0, k0 * (arc - g.1))
        };
        let edge = 500_000.0 + k0 * TM_A * (TM_MAX_ETA - 1e-6);
        for xi in [0.0, 0.1, 0.2, 0.3, 0.4] {
            let (lon, lat) = tm.inverse(edge, k0 * TM_A * xi).unwrap();
            let (x, y) = exact(lon, lat);
            let (fx, fy) = tm.forward(lon, lat);
            assert!(
                (fx - x).hypot(fy - y) < 6e-4,
                "({lon}, {lat}): ({fx}, {fy}), exact ({x}, {y})"
            );
            let (ilon, ilat) = tm.inverse(x, y).unwrap();
            let off = great_circle_distance_m(ilon, ilat, lon, lat);
            assert!(off < 1e-5, "({lon}, {lat}): inverse {off} m off");
        }
        let (x, y) = exact(103.0, 0.0);
        let (fx, fy) = tm.forward(103.0, 0.0);
        assert!(
            (fx - x).hypot(fy - y) > 0.5,
            "({fx}, {fy}), exact ({x}, {y})"
        );
    }

    /// A complex number for [`tm_series_is_exact_inside_its_area`]'s exact
    /// projection, with the few functions it takes.
    #[derive(Clone, Copy, Debug)]
    struct Cx(f64, f64);

    impl std::ops::Add for Cx {
        type Output = Cx;
        fn add(self, o: Cx) -> Cx {
            Cx(self.0 + o.0, self.1 + o.1)
        }
    }

    impl std::ops::Sub for Cx {
        type Output = Cx;
        fn sub(self, o: Cx) -> Cx {
            Cx(self.0 - o.0, self.1 - o.1)
        }
    }

    impl std::ops::Mul for Cx {
        type Output = Cx;
        fn mul(self, o: Cx) -> Cx {
            Cx(self.0 * o.0 - self.1 * o.1, self.0 * o.1 + self.1 * o.0)
        }
    }

    impl std::ops::Mul<f64> for Cx {
        type Output = Cx;
        fn mul(self, k: f64) -> Cx {
            Cx(self.0 * k, self.1 * k)
        }
    }

    impl std::ops::Div for Cx {
        type Output = Cx;
        fn div(self, o: Cx) -> Cx {
            let d = o.0 * o.0 + o.1 * o.1;
            Cx(
                (self.0 * o.0 + self.1 * o.1) / d,
                (self.1 * o.0 - self.0 * o.1) / d,
            )
        }
    }

    impl Cx {
        fn abs(self) -> f64 {
            self.0.hypot(self.1)
        }
        fn sin(self) -> Cx {
            Cx(self.0.sin() * self.1.cosh(), self.0.cos() * self.1.sinh())
        }
        fn cos(self) -> Cx {
            Cx(self.0.cos() * self.1.cosh(), -self.0.sin() * self.1.sinh())
        }
        fn ln(self) -> Cx {
            Cx(self.abs().ln(), self.1.atan2(self.0))
        }
        fn sqrt(self) -> Cx {
            let (r, half) = (self.abs().sqrt(), self.1.atan2(self.0) / 2.0);
            Cx(r * half.cos(), r * half.sin())
        }
        fn atanh(self) -> Cx {
            let one = Cx(1.0, 0.0);
            ((one + self) / (one - self)).ln() * 0.5
        }
    }

    /// A pole inside a projected box is its latitude extreme, and no edge
    /// sample reaches it: the read window runs to the pole and spans the
    /// area's longitudes, for EPSG:3067 those within 90° of 27°E (#1113,
    /// #1130). EPSG:3067's North Pole lies at
    /// (500 000, 9 997 964.943), EPSG:3035's at northing 7 369 716.255,
    /// easting 4 321 000: PROJ 9.8.1, `cs2cs -d 3 EPSG:4326 EPSG:3067` of
    /// `90 27` and `cs2cs -d 3 EPSG:4326 EPSG:3035` of `90 10`.
    #[test]
    fn wgs84_envelope_reaches_a_pole_the_box_holds() {
        let tm = projected_output_crs("EPSG:3067").unwrap();
        let laea = projected_output_crs("EPSG:3035").unwrap();
        let pole_n = 9_997_964.943;
        for (crs, lon, (e, n)) in [
            (&tm, 0.0, (500_000.0, pole_n)),
            (&tm, 27.0, (500_000.0, pole_n)),
            (&laea, 0.0, (4_321_000.0, 7_369_716.255)),
        ] {
            let (x, y) = crs.forward(lon, 90.0);
            assert!((x - e).abs() < 0.01 && (y - n).abs() < 0.01, "({x}, {y})");
        }
        let near = |a: f64, b: f64, tol: f64| (a - b).abs() < tol;
        // EPSG:3067 boxes, each with its lowest latitude, PROJ's at a
        // bottom corner (`cs2cs -d 9 EPSG:3067 EPSG:4326`).
        for (bbox, south) in [
            // From Germany over Svalbard and the Arctic Ocean, past the
            // pole: (-1 000 000, 6 000 000) is 52.059 130°N 4.976 770°E.
            ([-1.0e6, 6.0e6, 2.0e6, 1.05e7], 52.059_130),
            // The level-9 meta-tile holding the pole: (1 448 155,
            // 8 688 928) is 75.572 844°N 63.206 699°E.
            ([0.0, 8_688_928.0, 1_448_155.0, 10_137_083.0], 75.572_844),
        ] {
            let [w, s, e, n] = wgs84_envelope(&tm, bbox).unwrap();
            assert_eq!(n, 90.0, "{bbox:?}");
            assert!(
                near(w, -63.0, 1e-9) && near(e, 117.0, 1e-9),
                "{bbox:?}: {w}..{e}"
            );
            assert!(near(s, south, 0.01), "{bbox:?}: south {s}");
        }
        // Its bottom edge 5 km short of the pole. The area's north edge,
        // the pole's northing, runs through the box on the meridians 90°
        // either side of 27°E, and leaves it through its sides, 5 km above
        // its bottom corners: PROJ's (-1 030 000, 9 992 964.943) is
        // 76.422 576°N 62.814 532°W, (1 970 000, 9 992 964.943) 76.945 806°N
        // 116.806 821°E. Every place in the box, found on a fine grid, lies
        // inside the envelope, and the lowest of them at its south edge.
        let bbox = [-1_030_000.0, pole_n - 5_000.0, 1_970_000.0, pole_n + 1.0e6];
        let [w, s, e, n] = wgs84_envelope(&tm, bbox).unwrap();
        assert_eq!(n, 90.0);
        assert!(near(w, -63.0, 1e-9) && near(e, 117.0, 1e-9), "{w}..{e}");
        let mut lowest = f64::MAX;
        for i in 0..=400 {
            for j in 0..=400 {
                let x = bbox[0] + (bbox[2] - bbox[0]) * f64::from(i) / 400.0;
                let y = bbox[1] + 10_000.0 * f64::from(j) / 400.0;
                if let Some((lon, lat)) = tm.inverse(x, y) {
                    assert!(
                        (w..=e).contains(&lon) && lat >= s,
                        "({x}, {y}): {lon}, {lat}"
                    );
                    lowest = lowest.min(lat);
                }
            }
        }
        assert!(
            near(s, 76.422_576, 1e-6) && near(lowest, s, 1e-9),
            "{s}, {lowest}"
        );
        // EPSG:3035: around its North Pole, every longitude its inverse
        // gives, 10°E ± 180°. A store on 0°..360° then reads its 180°-190°
        // sector too, where the view's nodes reach.
        let [w, _, e, n] = wgs84_envelope(&laea, [3.0e6, 6.0e6, 5.5e6, 8.5e6]).unwrap();
        assert!(
            near(w, -170.0, 1e-9) && near(e, 190.0, 1e-9) && n == 90.0,
            "{w}..{e}, {n}"
        );
        // Boxes short of a pole keep their edges' envelope.
        let [_, _, _, n] = wgs84_envelope(&tm, [0.0, 9.0e6, 1.0e6, 9.99e6]).unwrap();
        assert!(n < 90.0, "{n}");
        // A box whose west edge is the pole's easting, 500 000 m, holds it.
        // Projected from 0°E, not the central meridian, the pole lands a
        // rounding error west of that edge, and the box missed it.
        assert!(tm.forward(0.0, 90.0).0 < 500_000.0);
        let bbox = [500_000.0, 9.0e6, 1.0e6, 1.05e7];
        assert!(holds_pole(&tm, bbox, 90.0));
        assert!(holds_pole(&laea, [4_321_000.0, 7.0e6, 5.0e6, 8.0e6], 90.0));
        let [w, s, e, n] = wgs84_envelope(&tm, bbox).unwrap();
        // East of the central meridian, up to the area's edge 90° out: PROJ's
        // corner (1 000 000, 9 000 000) is 80.011 663 590°N.
        assert!(
            n == 90.0 && near(w, 27.0, 1e-6) && near(e, 117.0, 1e-9) && near(s, 80.011_664, 0.01),
            "{w}, {s}, {e}, {n}"
        );
    }

    /// Where an edge of EPSG:3067's area crosses a box, it is the box's
    /// longitude or latitude extreme there, which edge samples 150 km or
    /// 50 km apart stop short of (#1113, #1130). The envelope takes where
    /// it enters and leaves the box, and the equator between. Its bounds
    /// are PROJ 9.8.1's, `cs2cs -f %.9f +proj=tmerc +lat_0=0 +lon_0=27
    /// +k=0.9996 +x_0=500000 +y_0=0 +datum=WGS84 +to +proj=longlat
    /// +datum=WGS84`. Across the east edge, easting 10 497 964.943: the
    /// box's west edge on the equator, (9 000 000, 0), is 87.355 016 295°E,
    /// its corners (9 000 000, -2 000 000) 8.744 819 763°S and (9 000 000,
    /// 3 000 000) 12.906 601 967°N, and the area's edge at its top,
    /// (10 497 964.943, 3 000 000), 95.638 859 216°E. Across the north
    /// edge, the pole's northing 9 997 964.943, which 90° out runs along
    /// 117°E: (2 000 000, 9 500 000) is 98.808 317 361°E, (4 000 000,
    /// 9 500 000) 59.780 888 391°N and (2 000 000, 9 997 964.943) 76.684
    /// 126 655°N. Every place in either box lies in its envelope.
    #[test]
    fn wgs84_envelope_reaches_the_tm_area_edge_between_edge_samples() {
        let tm = projected_output_crs("EPSG:3067").unwrap();
        let near = |a: f64, b: f64, tol: f64| (a - b).abs() < tol;
        for (bbox, [west, south, east, north]) in [
            (
                [9.0e6, -2.0e6, 1.2e7, 3.0e6],
                [
                    87.355_016_295,
                    -8.744_819_763,
                    95.638_859_216,
                    12.906_601_967,
                ],
            ),
            (
                [2.0e6, 9.5e6, 4.0e6, 1.05e7],
                [98.808_317_361, 59.780_888_391, 117.0, 76.684_126_655],
            ),
        ] {
            let [w, s, e, n] = wgs84_envelope(&tm, bbox).unwrap();
            assert!(
                near(w, west, 1e-8)
                    && near(s, south, 1e-8)
                    && near(e, east, 1e-8)
                    && near(n, north, 1e-8),
                "{bbox:?}: {w}, {s}, {e}, {n}"
            );
            let [.., edge_e, _] = edge_envelope(bbox, |x, y| tm.inverse(x, y)).unwrap();
            assert!(edge_e < east - 0.1, "{bbox:?}: the edges reach {edge_e}");
            let (mut low, mut high) = (f64::MAX, f64::MIN);
            for i in 0..=300 {
                for j in 0..=300 {
                    let x = bbox[0] + (bbox[2] - bbox[0]) * f64::from(i) / 300.0;
                    let y = bbox[1] + (bbox[3] - bbox[1]) * f64::from(j) / 300.0;
                    if let Some((lon, lat)) = tm.inverse(x, y) {
                        assert!(
                            (w..=e).contains(&lon) && (s..=n).contains(&lat),
                            "{bbox:?}: ({x}, {y}) at {lon}, {lat}"
                        );
                        (low, high) = (low.min(lon), high.max(lon));
                    }
                }
            }
            assert!(
                near(low, w, 0.2) && near(high, e, 0.2),
                "{bbox:?}: {low}..{high}"
            );
        }
        // The north edge is the second box's latitude extreme too.
        let [.., edge_n] =
            edge_envelope([2.0e6, 9.5e6, 4.0e6, 1.05e7], |x, y| tm.inverse(x, y)).unwrap();
        assert!(edge_n < 76.684_126_655 - 0.005, "the edges reach {edge_n}");
        // A box wholly inside the area, and one wholly past it, have no
        // area edge in them.
        assert!(area_edge_points(&tm, [-1.2e5, 6.3e6, 8.8e5, 7.9e6]).is_empty());
        assert!(area_edge_points(&tm, [1.1e7, 6.5e6, 1.2e7, 7.0e6]).is_empty());
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
            radius: None,
        };
        // Oslo roundtrip
        let (e, n) = crs.forward(10.75, 59.91);
        let (lon, lat) = crs.inverse(e, n).unwrap();
        assert!((lon - 10.75).abs() < 0.001, "lon={lon}");
        assert!((lat - 59.91).abs() < 0.001, "lat={lat}");
    }

    /// Asserts `crs` maps each `(lon, lat)` to the PROJ `(x, y)` and back.
    fn assert_matches_proj(crs: &Crs, cases: &[(f64, f64, f64, f64)]) {
        for &(lon, lat, x_proj, y_proj) in cases {
            let (x, y) = crs.forward(lon, lat);
            assert!(
                (x - x_proj).abs() < 0.01 && (y - y_proj).abs() < 0.01,
                "forward({lon}, {lat}) = ({x}, {y}), PROJ ({x_proj}, {y_proj})"
            );
            let (lon_back, lat_back) = crs.inverse(x_proj, y_proj).unwrap();
            assert!(
                (lon_back - lon).abs() < 1e-7 && (lat_back - lat).abs() < 1e-7,
                "inverse({x_proj}, {y_proj}) = ({lon_back}, {lat_back}), PROJ ({lon}, {lat})"
            );
        }
    }

    /// Secant LCC pinned to `cs2cs +proj=longlat +datum=WGS84 +to
    /// +proj=lcc +lat_1=58.964 +lat_2=69.987 +lat_0=0 +lon_0=0
    /// +datum=WGS84 +units=m` (PROJ 9.x), not only to itself.
    #[test]
    fn lcc_secant_matches_proj() {
        let crs = Crs::LambertConformalConic {
            lat1: 58.964_f64.to_radians(),
            lat2: 69.987_f64.to_radians(),
            lat0: 0.0,
            lon0: 0.0,
            false_e: 0.0,
            false_n: 0.0,
            radius: None,
        };
        assert_matches_proj(&crs, &[(10.75, 59.91, 597771.1689, 8061381.0468)]);
    }

    /// Tangent LCC (one standard parallel, the MEPS/MetCoOp grid) pinned
    /// to `cs2cs +proj=longlat +datum=WGS84 +to +proj=lcc +lat_1=63.3
    /// +lat_2=63.3 +lat_0=63.3 +lon_0=15 +datum=WGS84 +units=m` (PROJ 9.x).
    /// The secant cone constant is 0/0 here, and the NaN it produced
    /// blanked every MEPS map and its advertised extent.
    #[test]
    fn lcc_tangent_matches_proj() {
        let crs = Crs::LambertConformalConic {
            lat1: 63.3_f64.to_radians(),
            lat2: 63.3_f64.to_radians(),
            lat0: 63.3_f64.to_radians(),
            lon0: 15.0_f64.to_radians(),
            false_e: 0.0,
            false_n: 0.0,
            radius: None,
        };
        assert_matches_proj(
            &crs,
            &[
                (15.0, 63.3, 0.0, 0.0),
                (25.0, 60.17, 553685.1338, -305725.8263),
                (0.279, 50.32, -1063266.1496, -1334173.1109),
                (40.0, 70.0, 937671.0198, 934085.7304),
            ],
        );
    }

    /// The tangent cone on QueryData's 6371220 m sphere, pinned to
    /// `cs2cs +proj=longlat +R=6371220 +to +proj=lcc +lat_1=63.3
    /// +lat_2=63.3 +lat_0=63.3 +lon_0=15 +R=6371220 +units=m` (PROJ 9.x).
    /// (0.279, 50.32) is the full MEPS grid's first point, which its QueryData
    /// world rect places at (-1060059.79, -1332533.27).
    #[test]
    fn lcc_tangent_sphere_matches_proj() {
        let crs = Crs::LambertConformalConic {
            lat1: 63.3_f64.to_radians(),
            lat2: 63.3_f64.to_radians(),
            lat0: 63.3_f64.to_radians(),
            lon0: 15.0_f64.to_radians(),
            false_e: 0.0,
            false_n: 0.0,
            radius: Some(6_371_220.0),
        };
        assert_matches_proj(
            &crs,
            &[
                (15.0, 63.3, 0.0, 0.0),
                (25.0, 60.17, 551690.9444, -305123.0314),
                (0.279, 50.32, -1060059.7877, -1332533.2749),
                (40.0, 70.0, 933888.3557, 931210.1301),
            ],
        );
    }

    /// Secant counterpart of `lcc_tangent_sphere_matches_proj`: `cs2cs
    /// +proj=lcc +lat_1=58.964 +lat_2=69.987 +lat_0=0 +lon_0=0 +R=6371220`.
    #[test]
    fn lcc_secant_sphere_matches_proj() {
        let crs = Crs::LambertConformalConic {
            lat1: 58.964_f64.to_radians(),
            lat2: 69.987_f64.to_radians(),
            lat0: 0.0,
            lon0: 0.0,
            false_e: 0.0,
            false_n: 0.0,
            radius: Some(6_371_220.0),
        };
        assert_matches_proj(&crs, &[(10.75, 59.91, 595623.4605, 8092895.0783)]);
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

    /// The EPSG:3035 inverse across the North Pole and the antimeridian,
    /// pinned to `cs2cs +proj=laea +lat_0=52 +lon_0=10 +x_0=4321000
    /// +y_0=3210000 +datum=WGS84 +units=m +to +proj=longlat +datum=WGS84`
    /// (PROJ 9.8). The old Newton inverse settled beyond the pole on
    /// latitudes above 90° or never converged: every Arctic view of a
    /// global field rendered blank wedges and scrambled data (#323).
    /// Longitudes compare modulo a turn — the inverse returns them within
    /// ±180° of 10°E, PROJ within ±180° of Greenwich.
    #[test]
    fn laea_3035_inverse_matches_proj_across_the_pole() {
        let crs = projected_output_crs("EPSG:3035").unwrap();
        let cases = [
            // Ireland, west of Greenwich.
            (3_036_306.0, 3_606_514.0, -9.859444076, 53.963967699),
            // Arabia, far south-east of the centre.
            (9_000_000.0, 1_000_000.0, 55.324361180, 19.442161595),
            // Within a degree of the pole, then beyond it.
            (4_400_000.0, 7_400_000.0, 123.422114951, 89.272405714),
            (4_600_000.0, 7_500_000.0, 128.251176664, 87.330881556),
            // On the longitude cut (170°W, straight up from the pole).
            (4_321_000.0, 8_407_544.0, -170.0, 80.000002880),
            // The antimeridian, then Alaska and Kamchatka either side of it.
            (4_755_500.0, 9_368_548.0, -179.999992499, 69.999998778),
            (2_000_000.0, 9_500_000.0, -131.219110202, 60.483853425),
            (7_000_000.0, 10_000_000.0, 154.078024408, 54.167651717),
        ];
        for (x, y, lon_proj, lat_proj) in cases {
            let (lon, lat) = crs.inverse(x, y).expect("inside the projection's disk");
            let dlon = (lon - lon_proj + 180.0).rem_euclid(360.0) - 180.0;
            assert!(
                dlon.abs() < 1e-7 && (lat - lat_proj).abs() < 1e-7,
                "inverse({x}, {y}) = ({lon}, {lat}), PROJ ({lon_proj}, {lat_proj})"
            );
            // The forward agrees with PROJ too, so the pair is anchored.
            let (fx, fy) = crs.forward(lon_proj, lat_proj);
            assert!(
                (fx - x).abs() < 0.01 && (fy - y).abs() < 0.01,
                "forward({lon_proj}, {lat_proj}) = ({fx}, {fy}), PROJ ({x}, {y})"
            );
        }
        // Past the far side of the disk (twice the authalic radius from
        // the centre) there is no inverse, rather than an invented one.
        assert_eq!(crs.inverse(4_321_000.0, 3_210_000.0 + 13_000_000.0), None);
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

    /// `polar_stereographic_k0` gives the `k0` above for 60°, which the
    /// spherical shortcut misses, and 1 for scale true at the pole, the
    /// limit its formula approaches and cannot evaluate (`t_c = 0`).
    #[test]
    fn polar_stereographic_k0_from_the_standard_parallel() {
        assert_eq!(
            polar_stereographic_k0(60.0_f64.to_radians()),
            0.933_069_071_736_356_6
        );
        assert!((polar_stereographic_k0(60.0_f64.to_radians()) - 0.933_012_7).abs() > 5e-5);
        assert_eq!(polar_stereographic_k0(FRAC_PI_2), 1.0);
        let near_pole = polar_stereographic_k0(89.999_f64.to_radians());
        assert!((near_pole - 1.0).abs() < 1e-9, "{near_pole}");
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

    // -- geometry_to_pixels (#397) -----------------------------------------

    fn approx(a: [f64; 2], b: [f64; 2], eps: f64) -> bool {
        (a[0] - b[0]).abs() < eps && (a[1] - b[1]).abs() < eps
    }

    #[test]
    fn geometry_to_pixels_wgs84_pins_corners_and_centre() {
        // bbox [west, south, east, north] = [0, 0, 10, 10], 100×100 px.
        // Linear lon/lat → pixel: NW corner → (0,0), SE corner → (100,100),
        // centre → (50,50). y is top-down (north at y=0).
        let bbox = [0.0, 0.0, 10.0, 10.0];
        let geom = Geometry::Polygon {
            exterior: vec![
                [0.0, 10.0],  // NW
                [10.0, 10.0], // NE
                [10.0, 0.0],  // SE
                [0.0, 0.0],   // SW
                [5.0, 5.0],   // centre
            ],
            holes: vec![],
        };
        let px = geometry_to_pixels(&geom, bbox, 100, 100, &OutputCrs::Wgs84);
        assert_eq!(px.polygons.len(), 1);
        let ring = &px.polygons[0].exterior;
        assert!(approx(ring[0], [0.0, 0.0], 1e-9), "NW {:?}", ring[0]);
        assert!(approx(ring[1], [100.0, 0.0], 1e-9), "NE {:?}", ring[1]);
        assert!(approx(ring[2], [100.0, 100.0], 1e-9), "SE {:?}", ring[2]);
        assert!(approx(ring[3], [0.0, 100.0], 1e-9), "SW {:?}", ring[3]);
        assert!(approx(ring[4], [50.0, 50.0], 1e-9), "centre {:?}", ring[4]);
    }

    #[test]
    fn geometry_to_pixels_web_mercator_bows_y() {
        // Same bbox; x stays linear in lon, but y follows Mercator: northern
        // latitudes occupy more Mercator-Y space, so the geographic lat=5°
        // midpoint is pushed *below* the geometric middle (pixel y > 50).
        let bbox = [0.0, 0.0, 10.0, 10.0];
        let geom = Geometry::Polygon {
            exterior: vec![[0.0, 10.0], [10.0, 0.0], [5.0, 5.0]],
            holes: vec![],
        };
        let px = geometry_to_pixels(&geom, bbox, 100, 100, &OutputCrs::WebMercator);
        let ring = &px.polygons[0].exterior;
        // Corners pin exactly (the bbox edges are fixed points of the mapping).
        assert!(approx(ring[0], [0.0, 0.0], 1e-9), "NW {:?}", ring[0]);
        assert!(approx(ring[1], [100.0, 100.0], 1e-9), "SE {:?}", ring[1]);
        // Centre: pin y against an independently-computed Mercator fraction.
        let merc = |lat: f64| {
            6_378_137.0_f64 * ((std::f64::consts::FRAC_PI_4 + lat.to_radians() / 2.0).tan()).ln()
        };
        let (my_n, my_s, my_m) = (merc(10.0), merc(0.0), merc(5.0));
        let expected_y = (my_n - my_m) / (my_n - my_s) * 100.0;
        assert!(expected_y > 50.0, "Mercator midpoint below centre");
        assert!(
            (ring[2][0] - 50.0).abs() < 1e-9 && (ring[2][1] - expected_y).abs() < 1e-6,
            "centre {:?} vs expected y {expected_y}",
            ring[2]
        );
    }

    #[test]
    fn geometry_to_pixels_projected_epsg3067() {
        // EPSG:3067 (TM35FIN). Output bbox in projected metres straddling the
        // central meridian (27°E ↔ easting 500 000). A polygon vertex on the
        // central meridian must land at fx = 0.5 — an independent pin that the
        // projection is actually applied (not the metres treated as degrees).
        let crs = projected_output_crs("EPSG:3067").unwrap();
        let bbox_proj = [400_000.0, 6_600_000.0, 600_000.0, 6_900_000.0];
        let out = OutputCrs::Projected {
            crs: crs.clone(),
            bbox: bbox_proj,
        };
        let geom = Geometry::Polygon {
            exterior: vec![[27.0, 62.0], [28.0, 62.0], [27.0, 61.0]],
            holes: vec![],
        };
        // The WGS84 `bbox` arg is ignored by the Projected variant.
        let px = geometry_to_pixels(&geom, [0.0; 4], 200, 300, &out);
        let ring = &px.polygons[0].exterior;
        // Vertex 0 is on λ0=27° → easting exactly 500 000 → fx=0.5 → x=100.
        assert!((ring[0][0] - 100.0).abs() < 1e-3, "λ0 x {:?}", ring[0]);
        // Cross-check the full mapping against forward() + the fraction math.
        for (i, &[lon, lat]) in geom_exterior(&geom).iter().enumerate() {
            let (e, n) = crs.forward(lon, lat);
            let fx = (e - bbox_proj[0]) / (bbox_proj[2] - bbox_proj[0]);
            let fy = (bbox_proj[3] - n) / (bbox_proj[3] - bbox_proj[1]);
            assert!(
                approx(ring[i], [fx * 200.0, fy * 300.0], 1e-6),
                "vertex {i} {:?}",
                ring[i]
            );
        }
    }

    #[test]
    fn geometry_to_pixels_multipolygon_and_holes() {
        let bbox = [0.0, 0.0, 10.0, 10.0];
        let geom = Geometry::MultiPolygon {
            polygons: vec![
                (
                    vec![[0.0, 10.0], [2.0, 10.0], [0.0, 8.0]],
                    vec![vec![[0.5, 9.5], [1.0, 9.5], [0.5, 9.0]]],
                ),
                (vec![[8.0, 2.0], [10.0, 2.0], [8.0, 0.0]], vec![]),
            ],
        };
        let px = geometry_to_pixels(&geom, bbox, 100, 100, &OutputCrs::Wgs84);
        assert_eq!(px.polygons.len(), 2);
        assert_eq!(px.polygons[0].holes.len(), 1);
        // First hole vertex [0.5, 9.5] → (5, 5).
        assert!(approx(px.polygons[0].holes[0][0], [5.0, 5.0], 1e-9));
        assert!(px.polygons[1].holes.is_empty());
    }

    #[test]
    fn geometry_to_pixels_point_and_null_yield_nothing() {
        let bbox = [0.0, 0.0, 10.0, 10.0];
        for geom in [Geometry::Point { x: 5.0, y: 5.0 }, Geometry::Null] {
            let px = geometry_to_pixels(&geom, bbox, 100, 100, &OutputCrs::Wgs84);
            assert!(px.polygons.is_empty());
        }
    }

    fn geom_exterior(geom: &Geometry) -> &[[f64; 2]] {
        match geom {
            Geometry::Polygon { exterior, .. } => exterior,
            _ => panic!("not a polygon"),
        }
    }

    #[test]
    fn beam_height_rises_with_range_and_elevation() {
        // 0.5° at 100 km: ~0.87 km geometric + ~0.59 km curvature ≈ 1.46 km
        // (Doviak & Zrnić). Bracket generously; pin monotonicity exactly.
        let h = beam_height_at_ground(0.5, 100_000.0);
        assert!((1_300.0..1_600.0).contains(&h), "got {h}");
        assert!(beam_height_at_ground(0.5, 200_000.0) > h);
        assert!(beam_height_at_ground(1.5, 100_000.0) > h);
        assert!(beam_height_at_ground(0.5, 0.0).abs() < 1e-6);
        // A negative tilt dips below the antenna before curvature lifts it.
        assert!(beam_height_at_ground(-0.2, 10_000.0) < 0.0);
    }

    #[test]
    fn great_circle_distance_matches_known_pairs() {
        // Helsinki → Vihti radar ≈ 45 km; one degree of latitude ≈ 111.2 km.
        let d = great_circle_distance_m(24.94, 60.17, 24.50, 60.56);
        assert!((44_000.0..50_000.0).contains(&d), "got {d}");
        let one_deg = great_circle_distance_m(25.0, 60.0, 25.0, 61.0);
        assert!((one_deg - 111_195.0).abs() < 200.0, "got {one_deg}");
        assert_eq!(great_circle_distance_m(25.0, 60.0, 25.0, 60.0), 0.0);
    }

    // --- Web Mercator source grid -----------------------------------------

    /// `Crs::WebMercator` against PROJ 9.8.1:
    /// `cs2cs -d 6 EPSG:4326 EPSG:3857` (input `lat lon`). The first two are
    /// GMGSI pixel centres (the global mosaic's first and last pixel).
    #[test]
    fn web_mercator_matches_cs2cs() {
        let crs = Crs::WebMercator;
        for (lon, lat, x, y) in [
            (179.9996, 72.71541, 20_037_463.814_993, 12_015_991.631_465),
            (179.9004, -72.73677, 20_026_420.921_506, -12_023_999.255_654),
            (24.94, 60.17, 2_776_308.100_384, 8_437_684.160_973),
            (151.2, -33.9, 16_831_507.007_943, -4_015_382.360_073),
            (0.0, 0.0, 0.0, 0.0),
        ] {
            let (fx, fy) = crs.forward(lon, lat);
            assert!(
                (fx - x).abs() < 1e-3 && (fy - y).abs() < 1e-3,
                "forward({lon}, {lat}) = ({fx}, {fy}), cs2cs ({x}, {y})"
            );
            let (ilon, ilat) = crs.inverse(x, y).unwrap();
            assert!(
                (ilon - lon).abs() < 1e-9 && (ilat - lat).abs() < 1e-9,
                "inverse({x}, {y}) = ({ilon}, {ilat})"
            );
        }
        // `cs2cs -d 9 EPSG:3857 EPSG:4326`: an easting past the world's
        // edge comes back wrapped into [-180, 180].
        let (lon, lat) = crs.inverse(21_000_000.0, 5_000_000.0).unwrap();
        assert!((lon - -171.353_790_335).abs() < 1e-8, "{lon}");
        assert!((lat - 40.916_274_471).abs() < 1e-8, "{lat}");
    }

    /// Forward keeps longitude unwrapped: past 180° the easting keeps
    /// growing a turn along, so a render across the seam stays continuous.
    #[test]
    fn web_mercator_forward_is_unwrapped() {
        let crs = Crs::WebMercator;
        let turn = crs.forward(360.0, 0.0).0;
        let (a, _) = crs.forward(-170.0, 10.0);
        let (b, _) = crs.forward(190.0, 10.0);
        assert!((b - a - turn).abs() < 1e-6, "{a} {b} {turn}");
        assert!(crs.forward(179.0, 0.0).0 < crs.forward(181.0, 0.0).0);
    }

    // --- Geostationary -----------------------------------------------------

    /// GOES-East as the GOES-R L2 files describe it (`goes_imager_projection`).
    fn goes_east() -> Crs {
        Crs::Geostationary {
            lon0: (-75.0_f64).to_radians(),
            height: 35_786_023.0,
            semi_major: 6_378_137.0,
            semi_minor: 6_356_752.314_14,
            sweep: SweepAxis::X,
        }
    }

    fn goes_west() -> Crs {
        Crs::Geostationary {
            lon0: (-137.2_f64).to_radians(),
            height: 35_786_023.0,
            semi_major: 6_378_137.0,
            semi_minor: 6_356_752.314_14,
            sweep: SweepAxis::X,
        }
    }

    /// The GOES-R ABI full disk: 5424² pixels of 56 µrad, first centre at
    /// ±0.151844 rad (the packed x/y of an L2 file decoded).
    fn goes_full_disk(crs: Crs) -> GeoTransform {
        let h = 35_786_023.0;
        GeoTransform::from_cell_centres(
            -0.151844 * h,
            5.6e-5 * h,
            0.151844 * h,
            -5.6e-5 * h,
            5424,
            5424,
            crs,
        )
        .unwrap()
    }

    /// Pinned to `cs2cs +proj=longlat +a=6378137 +b=6356752.31414 +to
    /// +proj=geos +h=35786023 +lon_0=-75 +sweep=x +a=6378137
    /// +b=6356752.31414 +units=m` (PROJ 9.8.1), not only to itself.
    #[test]
    fn geostationary_sweep_x_matches_proj() {
        assert_matches_proj(
            &goes_east(),
            &[
                (-75.0, 0.0, 0.0, 0.0),
                (-100.0, 30.0, -2239253.387825, 3046275.703528),
                (-50.0, -40.0, 1947935.148980, -3852931.246132),
                (-120.0, 60.0, -2006328.159192, 4902017.718814),
                (-75.0, 80.0, 0.0, 5414758.281994),
                (-10.0, 5.0, 5182984.148697, 500500.611319),
            ],
        );
    }

    /// The navigation example of the GOES-R Product User Guide (vol. 3,
    /// §4.2.8): scan angles (−0.024052, 0.095340) rad from GOES-East are
    /// 33.846162°N 84.690932°W. An absolute reference independent of PROJ.
    #[test]
    fn geostationary_matches_goes_r_pug_example() {
        let h = 35_786_023.0;
        let (lon, lat) = goes_east().inverse(-0.024052 * h, 0.095340 * h).unwrap();
        assert!((lon + 84.690932).abs() < 1e-6, "lon={lon}");
        assert!((lat - 33.846162).abs() < 1e-6, "lat={lat}");
    }

    /// GOES-West sees across the antimeridian: `cs2cs … +proj=geos
    /// +h=35786023 +lon_0=-137.2 +sweep=x …` (PROJ 9.8.1).
    #[test]
    fn geostationary_across_the_antimeridian_matches_proj() {
        assert_matches_proj(
            &goes_west(),
            &[
                (170.0, 10.0, -4639119.965821, 1025975.668246),
                (-170.0, 20.0, -3117676.383867, 2087186.829455),
                (160.0, -30.0, -4402709.137024, -2856257.029850),
                (-150.0, 45.0, -941726.684592, 4233040.952387),
            ],
        );
    }

    /// Himawari scans y: `cs2cs +proj=longlat +a=6378137 +b=6356752.3 +to
    /// +proj=geos +h=35785831 +lon_0=140.7 +sweep=y +a=6378137 +b=6356752.3
    /// +units=m`; and a sphere, `… +R=6371000 … +lon_0=0 +sweep=y`.
    #[test]
    fn geostationary_sweep_y_and_sphere_match_proj() {
        let himawari = Crs::Geostationary {
            lon0: 140.7_f64.to_radians(),
            height: 35_785_831.0,
            semi_major: 6_378_137.0,
            semi_minor: 6_356_752.3,
            sweep: SweepAxis::Y,
        };
        assert_matches_proj(
            &himawari,
            &[
                (140.7, 0.0, 0.0, 0.0),
                (139.7, 35.7, -87578.480720, 3569861.951564),
                (-170.0, 10.0, 4453902.286646, 1026018.881534),
                (100.0, -20.0, -3706108.055965, -2048812.510060),
                (175.0, -40.0, 2583698.871098, -3803707.437491),
            ],
        );
        let sphere = Crs::Geostationary {
            lon0: 0.0,
            height: 35_785_831.0,
            semi_major: 6_371_000.0,
            semi_minor: 6_371_000.0,
            sweep: SweepAxis::Y,
        };
        assert_matches_proj(
            &sphere,
            &[
                (10.0, 50.0, 667435.597628, 4555599.059598),
                (-30.0, -20.0, -2891038.319347, -2099802.151926),
            ],
        );
    }

    /// PROJ rejects both (`*`): a point behind the Earth has no projection,
    /// and a scan angle past the limb sees no Earth.
    #[test]
    fn geostationary_is_undefined_off_the_disk() {
        let crs = goes_east();
        for (lon, lat) in [(100.0, 0.0), (-75.0, 85.0), (105.0, 30.0)] {
            let (x, y) = crs.forward(lon, lat);
            assert!(
                x.is_nan() && y.is_nan(),
                "forward({lon}, {lat}) = ({x}, {y})"
            );
        }
        assert_eq!(crs.inverse(0.0, 5_500_000.0), None);
        assert_eq!(crs.inverse(5_500_000.0, 0.0), None);
        assert_eq!(crs.inverse(f64::NAN, 0.0), None);
    }

    /// A full disk's edges are all space, so its extent comes from the limb:
    /// the equator's edge is `acos(a / (a + h))` = 81.2995° from the
    /// sub-satellite point, and the northern edge is at 81.3282°N (the
    /// latitude where `cs2cs` stops projecting on the −75° meridian).
    #[test]
    fn geostationary_full_disk_extent_is_the_limb() {
        let [w, s, e, n] = goes_full_disk(goes_east()).bbox();
        assert!((w - (-75.0 - 81.2995)).abs() < 1e-3, "west={w}");
        assert!((e - (-75.0 + 81.2995)).abs() < 1e-3, "east={e}");
        assert!((n - 81.3282).abs() < 1e-3, "north={n}");
        assert!((s + 81.3282).abs() < 1e-3, "south={s}");
    }

    /// GOES-West's disk crosses the antimeridian: its extent is `west > east`
    /// (141.5°E to 55.9°W), which `crs84_extent` keeps as a crossing — not a
    /// −180…180 box.
    #[test]
    fn geostationary_full_disk_extent_crosses_the_antimeridian() {
        let bbox = goes_full_disk(goes_west()).bbox();
        let [w, _, e, _] = crs84_extent(bbox).unwrap();
        assert!((w - (-137.2 - 81.2995 + 360.0)).abs() < 1e-3, "west={w}");
        assert!((e - (-137.2 + 81.2995)).abs() < 1e-3, "east={e}");
        assert!(w > e);
    }

    /// A window cut across the limb (the NE edge of the disk, as in the
    /// C13 fixture) has part of each edge in space. Its extent must contain
    /// every on-disk pixel centre, and exceed their extremes only by about
    /// a pixel, which near the limb covers a large ground distance.
    #[test]
    fn geostationary_window_across_the_limb_covers_its_pixels() {
        let h = 35_786_023.0;
        let (row0, col0, rows, cols) = (720, 4480, 240, 320);
        let centre = |i: u32| -0.151844 + 5.6e-5 * i as f64;
        let gt = GeoTransform::from_cell_centres(
            centre(col0) * h,
            5.6e-5 * h,
            -centre(row0) * h,
            -5.6e-5 * h,
            cols,
            rows,
            goes_east(),
        )
        .unwrap();
        let [w, s, e, n] = gt.bbox();
        let mut seen = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
        let mut on_disk = 0;
        for r in 0..rows {
            for c in 0..cols {
                let x = gt.origin_x + (c as f64 + 0.5) * gt.pixel_width;
                let y = gt.origin_y - (r as f64 + 0.5) * gt.pixel_height;
                if let Some((lon, lat)) = gt.crs.inverse(x, y) {
                    on_disk += 1;
                    seen = [
                        seen[0].min(lon),
                        seen[1].min(lat),
                        seen[2].max(lon),
                        seen[3].max(lat),
                    ];
                }
            }
        }
        assert!(
            on_disk > 0 && on_disk < rows * cols,
            "window must cross the limb"
        );
        assert!(w <= seen[0] && s <= seen[1] && e >= seen[2] && n >= seen[3]);
        assert!(
            seen[0] - w < 1.0 && seen[1] - s < 1.0,
            "{:?} vs {seen:?}",
            [w, s, e, n]
        );
        assert!(
            e - seen[2] < 1.0 && n - seen[3] < 1.0,
            "{:?} vs {seen:?}",
            [w, s, e, n]
        );
    }

    /// A whole-world request sees the whole disk even though every bbox
    /// edge (the poles, the far meridian) is invisible; an area behind the
    /// Earth reads nothing; and the canonical seam box reads from GOES-West,
    /// which sees it, but not from GOES-East.
    #[test]
    fn geostationary_bbox_to_pixels_covers_the_visible_part() {
        let east = goes_full_disk(goes_east());
        // The disk is flattened: the equator's limb falls in the outermost
        // columns, but the rows above the pole's limb are all space.
        let on_disk_rows: Vec<u32> = (0..5424)
            .filter(|&r| {
                let y = east.origin_y - (r as f64 + 0.5) * east.pixel_height;
                east.crs.inverse(0.0, y).is_some()
            })
            .collect();
        let (first, last) = (on_disk_rows[0], *on_disk_rows.last().unwrap());
        let (c0, r0, c1, r1) = east.bbox_to_pixels(-180.0, -90.0, 180.0, 90.0).unwrap();
        assert_eq!((c0, c1), (0, 5424));
        assert!(
            r0 <= first && r0 + 1 >= first,
            "r0={r0}, first on-disk row {first}"
        );
        assert!(
            r1 > last && r1 <= last + 2,
            "r1={r1}, last on-disk row {last}"
        );
        assert_eq!(east.bbox_to_pixels(100.0, -10.0, 120.0, 10.0), None);
        assert_eq!(east.bbox_to_pixels(170.0, 10.0, -170.0, 20.0), None);

        let west = goes_full_disk(goes_west());
        let (c0, r0, c1, r1) = west.bbox_to_pixels(170.0, 10.0, -170.0, 20.0).unwrap();
        // Every visible point of the box maps inside the window.
        for (lon, lat) in [(170.0, 10.0), (-170.0, 20.0), (180.0, 15.0), (175.0, 19.0)] {
            let (c, r) = west.world_to_pixel(lon, lat).unwrap();
            assert!(
                (c0..c1).contains(&c) && (r0..r1).contains(&r),
                "({lon}, {lat})"
            );
        }
    }

    /// A box reaching past the limb gets a window up to the disk edge, not
    /// only as far as its visible corners.
    #[test]
    fn geostationary_bbox_to_pixels_reaches_the_limb() {
        let gt = goes_full_disk(goes_east());
        // Up to 20°E: past the eastern limb (6.3°E) on the equator.
        let (_, _, col_end, _) = gt.bbox_to_pixels(-20.0, -5.0, 20.0, 5.0).unwrap();
        assert!(col_end >= 5423, "col_end={col_end}");
    }

    #[test]
    fn from_cell_centres_places_the_origin_on_the_pixel_edge() {
        let gt = GeoTransform::from_cell_centres(10.5, 1.0, 20.5, -1.0, 4, 3, Crs::Wgs84).unwrap();
        assert_eq!((gt.origin_x, gt.origin_y), (10.0, 21.0));
        assert_eq!(gt.world_to_pixel(10.5, 20.5), Some((0, 0)));
        assert_eq!(gt.bbox(), [10.0, 18.0, 14.0, 21.0]);
        // South-up and zero-step grids are rejected rather than mislocated.
        assert!(GeoTransform::from_cell_centres(0.0, 1.0, 0.0, 1.0, 4, 3, Crs::Wgs84).is_err());
        assert!(GeoTransform::from_cell_centres(0.0, 0.0, 0.0, -1.0, 4, 3, Crs::Wgs84).is_err());
    }
}
