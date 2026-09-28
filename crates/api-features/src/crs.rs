//! OGC API - Features - Part 2: Coordinate Reference Systems by Reference
//! (OGC 18-058): `crs` and `bbox-crs` on `/items` (#685).
//!
//! Every engine holds geometry in CRS84, so this is an API-layer transform:
//! output positions are reprojected per vertex, and a `bbox` given in another
//! CRS becomes a CRS84 box before the engine query. The projection math is
//! `ds_core`'s — `web_mercator` for EPSG:3857, `geo::projected_output_crs`
//! for EPSG:3067 and EPSG:3035 (Critical Rule 4) — this module only picks the
//! transform and the axis order.
//!
//! **Axis order is the CRS's own** (Part 2), in `crs` output and in
//! `bbox-crs` input alike: CRS84 is lon/lat, EPSG:4326 lat/lon, EPSG:3857 and
//! EPSG:3067 easting/northing, EPSG:3035 northing/easting. `cs2cs`, which
//! follows the EPSG axis order, prints the same order.

use ds_core::error::DataServerError;
use ds_core::feature::Bbox;
use ds_core::geo::{self, Crs};
use ds_core::web_mercator;

/// The CRSs `/items` serves, in the order collections advertise them (the
/// first is the default). The same list as Maps' output CRSs, so the shared
/// root's merged `crs` list of a collection served by both is exactly what
/// `/items` accepts.
pub const SUPPORTED: [FeatureCrs; 5] = [
    FeatureCrs::Crs84,
    FeatureCrs::Epsg4326,
    FeatureCrs::Epsg3857,
    FeatureCrs::Epsg3067,
    FeatureCrs::Epsg3035,
];

/// One CRS of [`SUPPORTED`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeatureCrs {
    /// WGS 84 longitude/latitude, the default and storage CRS.
    Crs84,
    /// WGS 84 latitude/longitude.
    Epsg4326,
    /// Web Mercator, easting/northing.
    Epsg3857,
    /// ETRS89 / TM35FIN, easting/northing.
    Epsg3067,
    /// ETRS89-extended / LAEA Europe, northing/easting.
    Epsg3035,
}

/// Edge samples per side when a projected `bbox` becomes a CRS84 box. The
/// edges of a projected rectangle curve in longitude/latitude, so its extremes
/// can lie between the corners.
const BBOX_EDGE_SAMPLES: usize = 64;

impl FeatureCrs {
    /// The CRS URI advertised in `crs` and sent in `Content-Crs`.
    pub fn uri(self) -> &'static str {
        match self {
            FeatureCrs::Crs84 => "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
            FeatureCrs::Epsg4326 => "http://www.opengis.net/def/crs/EPSG/0/4326",
            FeatureCrs::Epsg3857 => "http://www.opengis.net/def/crs/EPSG/0/3857",
            FeatureCrs::Epsg3067 => "http://www.opengis.net/def/crs/EPSG/0/3067",
            FeatureCrs::Epsg3035 => "http://www.opengis.net/def/crs/EPSG/0/3035",
        }
    }

    /// The `AUTHORITY:CODE` form, also accepted as a parameter value.
    fn curie(self) -> &'static str {
        match self {
            FeatureCrs::Crs84 => "OGC:CRS84",
            FeatureCrs::Epsg4326 => "EPSG:4326",
            FeatureCrs::Epsg3857 => "EPSG:3857",
            FeatureCrs::Epsg3067 => "EPSG:3067",
            FeatureCrs::Epsg3035 => "EPSG:3035",
        }
    }

    /// Whether the CRS's first axis is latitude or northing.
    fn northing_first(self) -> bool {
        matches!(self, FeatureCrs::Epsg4326 | FeatureCrs::Epsg3035)
    }

    /// Parse the value of the `name` parameter (`crs` or `bbox-crs`): the
    /// advertised URI (or its `https` form), or the `[AUTHORITY:CODE]` /
    /// `AUTHORITY:CODE` CURIE (`CRS84` alone also names CRS84, as in
    /// collection search). Anything else is a 400 naming the valid URIs.
    pub fn parse(value: &str, name: &str) -> Result<Self, DataServerError> {
        let trimmed = value.trim();
        let curie = trimmed
            .strip_prefix('[')
            .and_then(|v| v.strip_suffix(']'))
            .unwrap_or(trimmed);
        SUPPORTED
            .into_iter()
            .find(|crs| {
                let uri = crs.uri();
                trimmed == uri
                    || uri
                        .strip_prefix("http://")
                        .is_some_and(|rest| trimmed.strip_prefix("https://") == Some(rest))
                    || curie == crs.curie()
                    || (*crs == FeatureCrs::Crs84 && curie == "CRS84")
            })
            .ok_or_else(|| {
                DataServerError::InvalidParameter(format!(
                    "{name} '{value}' is not supported by this collection (valid: {})",
                    supported_uris().join(", ")
                ))
            })
    }

    /// The projection behind a projected CRS.
    fn projection(self) -> Option<Crs> {
        match self {
            FeatureCrs::Epsg3067 | FeatureCrs::Epsg3035 => geo::projected_output_crs(self.curie()),
            _ => None,
        }
    }

    /// The CRS84 box to query for a `bbox` given in this CRS, as its four
    /// horizontal values in request order (lower corner, then upper corner,
    /// each in this CRS's axis order).
    ///
    /// - CRS84 and EPSG:4326 are the same box with the axes named: a west edge
    ///   east of the east edge still crosses the antimeridian (Part 1 §7.15.3).
    /// - EPSG:3857 is separable, so its edges map exactly; longitude wraps, so
    ///   a box past ±180° or with `minx > maxx` crosses the antimeridian.
    /// - EPSG:3067 and EPSG:3035 edges curve in longitude/latitude: the result
    ///   is the envelope of densely sampled edge points, widened to a pole the
    ///   box contains, and crossing the antimeridian when that is narrower.
    ///   The envelope can hold features just outside the projected rectangle
    ///   near its corners; it never misses one inside.
    pub fn bbox_to_crs84(self, values: [f64; 4]) -> Result<Bbox, DataServerError> {
        let invalid = |msg: String| DataServerError::InvalidBbox(msg);
        if !values.iter().all(|v| v.is_finite()) {
            return Err(invalid("bbox coordinates must be finite numbers".into()));
        }
        let [a_min, b_min, a_max, b_max] = values;
        let (min_e, min_n, max_e, max_n) = if self.northing_first() {
            (b_min, a_min, b_max, a_max)
        } else {
            (a_min, b_min, a_max, b_max)
        };
        match self {
            FeatureCrs::Crs84 | FeatureCrs::Epsg4326 => {
                Bbox::new(min_e, min_n, max_e, max_n).map_err(invalid)
            }
            FeatureCrs::Epsg3857 => {
                let (west, east) = (web_mercator::x_to_lon(min_e), web_mercator::x_to_lon(max_e));
                let (west, east) = if min_e <= max_e && east - west >= 360.0 {
                    (-180.0, 180.0)
                } else {
                    (wrap_west(west), geo::wrap_lon(east))
                };
                Bbox::new(
                    west,
                    web_mercator::y_to_lat(min_n),
                    east,
                    web_mercator::y_to_lat(max_n),
                )
                .map_err(invalid)
            }
            FeatureCrs::Epsg3067 | FeatureCrs::Epsg3035 => {
                if min_e > max_e || min_n > max_n {
                    return Err(invalid(format!(
                        "the lower corner must not exceed the upper corner in {}",
                        self.uri()
                    )));
                }
                let projection = self.projection().ok_or_else(|| {
                    invalid(format!("{} has no projection definition", self.uri()))
                })?;
                projected_envelope(&projection, [min_e, min_n, max_e, max_n]).ok_or_else(|| {
                    invalid(format!(
                        "bbox lies outside the area {} can represent",
                        self.uri()
                    ))
                })
            }
        }
    }
}

/// The CRS of one `/items` or `/items/{featureId}` response, prepared once
/// per request: positions are mapped into it, and links repeat it when the
/// request named it.
pub struct ResponseCrs {
    crs: FeatureCrs,
    projection: Option<Crs>,
    requested: bool,
}

impl Default for ResponseCrs {
    fn default() -> Self {
        Self::new(None)
    }
}

impl ResponseCrs {
    /// `requested` is the parsed `crs` parameter; `None` is CRS84.
    pub fn new(requested: Option<FeatureCrs>) -> Self {
        let crs = requested.unwrap_or(FeatureCrs::Crs84);
        Self {
            crs,
            projection: crs.projection(),
            requested: requested.is_some(),
        }
    }

    /// The `Content-Crs` header value (Part 2): the URI in angle brackets.
    pub fn content_crs(&self) -> String {
        format!("<{}>", self.crs.uri())
    }

    /// `crs=<encoded URI>` for links, or empty when the request used the
    /// default: following `next` must not switch CRS mid-way.
    pub fn link_query(&self) -> String {
        if !self.requested {
            return String::new();
        }
        form_urlencoded::Serializer::new(String::new())
            .append_pair("crs", self.crs.uri())
            .finish()
    }

    /// Map a CRS84 position into the response CRS, in its axis order. `None`
    /// when the position has no finite coordinates there (the South Pole in
    /// Web Mercator). CRS84 passes through untouched.
    pub fn position(&self, lon: f64, lat: f64) -> Option<[f64; 2]> {
        let (easting, northing) = match self.crs {
            FeatureCrs::Crs84 => return Some([lon, lat]),
            FeatureCrs::Epsg4326 => (lon, lat),
            FeatureCrs::Epsg3857 => (web_mercator::lon_to_x(lon), web_mercator::lat_to_y(lat)),
            FeatureCrs::Epsg3067 | FeatureCrs::Epsg3035 => {
                self.projection.as_ref()?.forward(lon, lat)
            }
        };
        if !(easting.is_finite() && northing.is_finite()) {
            return None;
        }
        Some(if self.crs.northing_first() {
            [northing, easting]
        } else {
            [easting, northing]
        })
    }
}

/// The advertised URIs, in [`SUPPORTED`] order.
pub fn supported_uris() -> Vec<&'static str> {
    SUPPORTED.iter().map(|crs| crs.uri()).collect()
}

/// A west edge wraps into [-180°, 180°): −180° stays west.
fn wrap_west(lon: f64) -> f64 {
    let w = geo::wrap_lon(lon);
    if w == 180.0 {
        -180.0
    } else {
        w
    }
}

/// Longitude/latitude of a projected point, when it has one.
///
/// LAEA's Newton inverse can settle on a latitude past ±90° with the same
/// sine, which its forward projection treats as that latitude folded back
/// into range. The folded point must then project back onto `(x, y)`: that
/// rejects the unconverged answers for points outside the projection's
/// domain.
fn inverse_point(projection: &Crs, x: f64, y: f64) -> Option<(f64, f64)> {
    let (lon, lat) = projection.inverse(x, y)?;
    let lat = if lat.abs() > 90.0 {
        lat.to_radians().sin().asin().to_degrees()
    } else {
        lat
    };
    let (rx, ry) = projection.forward(lon, lat);
    if !((rx - x).abs() <= 1.0 && (ry - y).abs() <= 1.0) {
        return None;
    }
    Some((geo::wrap_lon(lon), lat))
}

/// CRS84 envelope of the projected box `[min_e, min_n, max_e, max_n]`, or
/// `None` when no edge point inverse-projects.
fn projected_envelope(projection: &Crs, bbox: [f64; 4]) -> Option<Bbox> {
    let [min_e, min_n, max_e, max_n] = bbox;
    let mut points = Vec::with_capacity(4 * (BBOX_EDGE_SAMPLES + 1));
    for i in 0..=BBOX_EDGE_SAMPLES {
        let t = i as f64 / BBOX_EDGE_SAMPLES as f64;
        let e = min_e + t * (max_e - min_e);
        let n = min_n + t * (max_n - min_n);
        for (x, y) in [(e, min_n), (e, max_n), (min_e, n), (max_e, n)] {
            points.extend(inverse_point(projection, x, y));
        }
    }
    if points.is_empty() {
        return None;
    }
    // A pole inside the box is its latitude extreme and spans every
    // longitude, but no edge point reaches it.
    let contains = |lat: f64| {
        let (e, n) = projection.forward(0.0, lat);
        e.is_finite()
            && n.is_finite()
            && (min_e..=max_e).contains(&e)
            && (min_n..=max_n).contains(&n)
    };
    let (north_pole, south_pole) = (contains(90.0), contains(-90.0));
    let mut south = points.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
    let mut north = points.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
    if north_pole {
        north = 90.0;
    }
    if south_pole {
        south = -90.0;
    }
    let (west, east) = if north_pole || south_pole {
        (-180.0, 180.0)
    } else {
        longitude_span(points.iter().map(|p| p.0))
    };
    // Inverse projection can overshoot a pole by rounding; a latitude bound
    // past ±90° is the pole itself.
    Bbox::new(west, south.max(-90.0), east, north.min(90.0)).ok()
}

/// The narrower of the direct longitude interval and the one crossing the
/// antimeridian (`west > east`) covering every sample.
fn longitude_span(lons: impl Iterator<Item = f64> + Clone) -> (f64, f64) {
    let direct = lons
        .clone()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), l| {
            (lo.min(l), hi.max(l))
        });
    // The same samples on [0°, 360°): an interval through 180° is contiguous.
    let shifted = lons.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), l| {
        let l = if l < 0.0 { l + 360.0 } else { l };
        (lo.min(l), hi.max(l))
    });
    if shifted.1 - shifted.0 < direct.1 - direct.0 {
        let back = |l: f64| if l > 180.0 { l - 360.0 } else { l };
        (back(shifted.0), back(shifted.1))
    } else {
        direct
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected coordinates come from PROJ 9 (`cs2cs EPSG:4326 EPSG:<code>`
    // and back), which prints the EPSG axis order — never from this code.
    const HELSINKI: (f64, f64) = (24.9384, 60.1699);

    fn close(actual: [f64; 2], expected: [f64; 2], tolerance: f64) {
        assert!(
            (actual[0] - expected[0]).abs() <= tolerance
                && (actual[1] - expected[1]).abs() <= tolerance,
            "{actual:?} != {expected:?} (±{tolerance})"
        );
    }

    fn bbox_close(actual: &Bbox, expected: [f64; 4], tolerance: f64) {
        let actual_values = [actual.west, actual.south, actual.east, actual.north];
        assert!(
            actual_values
                .iter()
                .zip(expected)
                .all(|(a, e)| (a - e).abs() <= tolerance),
            "{actual_values:?} != {expected:?} (±{tolerance})"
        );
    }

    fn position(crs: FeatureCrs, (lon, lat): (f64, f64)) -> [f64; 2] {
        ResponseCrs::new(Some(crs)).position(lon, lat).unwrap()
    }

    #[test]
    fn parses_uris_and_curies_and_names_the_valid_ones_otherwise() {
        for value in [
            "http://www.opengis.net/def/crs/EPSG/0/3067",
            "https://www.opengis.net/def/crs/EPSG/0/3067",
            "EPSG:3067",
            "[EPSG:3067]",
            " http://www.opengis.net/def/crs/EPSG/0/3067 ",
        ] {
            assert_eq!(
                FeatureCrs::parse(value, "crs").unwrap(),
                FeatureCrs::Epsg3067
            );
        }
        for value in [
            "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
            "OGC:CRS84",
            "[OGC:CRS84]",
            "CRS84",
        ] {
            assert_eq!(FeatureCrs::parse(value, "crs").unwrap(), FeatureCrs::Crs84);
        }
        for value in [
            "http://www.opengis.net/def/crs/EPSG/0/32635",
            "EPSG:3067x",
            "CRS:84",
            "",
        ] {
            let message = FeatureCrs::parse(value, "bbox-crs")
                .unwrap_err()
                .to_string();
            assert!(message.contains("bbox-crs"), "{message}");
            for crs in SUPPORTED {
                assert!(message.contains(crs.uri()), "{message}");
            }
        }
    }

    #[test]
    fn output_positions_follow_each_crs_axis_order() {
        close(
            position(FeatureCrs::Crs84, HELSINKI),
            [24.9384, 60.1699],
            0.0,
        );
        // EPSG:4326 is latitude first.
        close(
            position(FeatureCrs::Epsg4326, HELSINKI),
            [60.1699, 24.9384],
            0.0,
        );
        // cs2cs EPSG:4326 EPSG:3857: easting, northing.
        close(
            position(FeatureCrs::Epsg3857, HELSINKI),
            [2776129.9892, 8437661.7820],
            0.001,
        );
        // cs2cs EPSG:4326 EPSG:3067: easting, northing.
        close(
            position(FeatureCrs::Epsg3067, HELSINKI),
            [385611.3167, 6672118.3802],
            0.001,
        );
        // cs2cs EPSG:4326 EPSG:3035: northing, easting.
        close(
            position(FeatureCrs::Epsg3035, HELSINKI),
            [4206147.9718, 5145297.8805],
            0.001,
        );
    }

    #[test]
    fn a_position_with_no_finite_coordinates_is_none() {
        let mercator = ResponseCrs::new(Some(FeatureCrs::Epsg3857));
        assert_eq!(mercator.position(0.0, -90.0), None);
        assert!(mercator.position(0.0, 89.0).is_some());
    }

    #[test]
    fn output_positions_round_trip_through_bbox_crs() {
        for crs in SUPPORTED {
            let [a, b] = position(crs, HELSINKI);
            let bbox = crs.bbox_to_crs84([a, b, a, b]).unwrap();
            bbox_close(
                &bbox,
                [HELSINKI.0, HELSINKI.1, HELSINKI.0, HELSINKI.1],
                1e-7,
            );
        }
    }

    #[test]
    fn epsg_4326_bbox_is_latitude_first_and_may_cross_the_antimeridian() {
        let bbox = FeatureCrs::Epsg4326
            .bbox_to_crs84([60.0, 20.0, 70.0, 30.0])
            .unwrap();
        bbox_close(&bbox, [20.0, 60.0, 30.0, 70.0], 0.0);
        let crossing = FeatureCrs::Epsg4326
            .bbox_to_crs84([-20.0, 170.0, -10.0, -170.0])
            .unwrap();
        assert!(crossing.crosses_antimeridian());
        bbox_close(&crossing, [170.0, -20.0, -170.0, -10.0], 0.0);
        // Latitude is the first value of each corner: 95 there is a 400,
        // while 95 as a longitude is fine.
        assert!(FeatureCrs::Epsg4326
            .bbox_to_crs84([20.0, 95.0, 30.0, 100.0])
            .is_ok());
        assert!(FeatureCrs::Epsg4326
            .bbox_to_crs84([95.0, 20.0, 100.0, 30.0])
            .is_err());
    }

    #[test]
    fn web_mercator_bbox_maps_exactly_and_wraps_the_antimeridian() {
        // cs2cs EPSG:4326 EPSG:3857 of (-20°, 170°) and (-10°, -170°).
        let (x170, y_20, y_10) = (18924313.4349, -2273030.9270, -1118889.9749);
        let direct = FeatureCrs::Epsg3857
            .bbox_to_crs84([-x170, y_20, x170, y_10])
            .unwrap();
        bbox_close(&direct, [-170.0, -20.0, 170.0, -10.0], 1e-6);
        assert!(!direct.crosses_antimeridian());
        // minx > maxx is a box across the antimeridian …
        let crossing = FeatureCrs::Epsg3857
            .bbox_to_crs84([x170, y_20, -x170, y_10])
            .unwrap();
        bbox_close(&crossing, [170.0, -20.0, -170.0, -10.0], 1e-6);
        assert!(crossing.crosses_antimeridian());
        // … and so is one continuing past the world's east edge.
        let unwrapped = FeatureCrs::Epsg3857
            .bbox_to_crs84([x170, y_20, x170 / 170.0 * 190.0, y_10])
            .unwrap();
        bbox_close(&unwrapped, [170.0, -20.0, -170.0, -10.0], 1e-6);
        // The whole world, however its edges round, is -180…180.
        let edge = web_mercator::lon_to_x(180.0);
        let world = FeatureCrs::Epsg3857
            .bbox_to_crs84([-edge, y_20, edge * (1.0 + 1e-12), y_10])
            .unwrap();
        assert_eq!((world.west, world.east), (-180.0, 180.0));
        assert!(FeatureCrs::Epsg3857
            .bbox_to_crs84([0.0, y_10, 1.0, y_20])
            .is_err());
    }

    #[test]
    fn projected_bbox_is_the_envelope_of_its_curved_edges() {
        // A TM35FIN box across the central meridian (27°E): the top edge
        // bows north between the corners. Dense `cs2cs EPSG:3067 EPSG:4326`
        // edge samples give the envelope; the corners alone would give a
        // north bound of 69.24209720.
        let bbox = FeatureCrs::Epsg3067
            .bbox_to_crs84([200_000.0, 6_600_000.0, 800_000.0, 7_700_000.0])
            .unwrap();
        bbox_close(
            &bbox,
            [19.39871773, 59.43111982, 34.60128227, 69.40928180],
            1e-6,
        );
        // LAEA Europe takes northing first: N 4.0–4.4 Mm, E 4.9–5.3 Mm.
        let bbox = FeatureCrs::Epsg3035
            .bbox_to_crs84([4_000_000.0, 4_900_000.0, 4_400_000.0, 5_300_000.0])
            .unwrap();
        bbox_close(
            &bbox,
            [20.01698217, 58.06147229, 28.58741200, 62.28725091],
            1e-6,
        );
    }

    #[test]
    fn projected_bbox_pinned_against_an_inverse_reference_point() {
        // cs2cs EPSG:3067 EPSG:4326 of (400000, 6700000): 60.423898797°N,
        // 25.183728786°E — the box's north-east corner and its extremes.
        let bbox = FeatureCrs::Epsg3067
            .bbox_to_crs84([300_000.0, 6_600_000.0, 400_000.0, 6_700_000.0])
            .unwrap();
        assert!((bbox.north - 60.423898797).abs() < 1e-7, "{bbox:?}");
        // cs2cs EPSG:3035 EPSG:4326 of N 4200000, E 5100000.
        let point = FeatureCrs::Epsg3035
            .bbox_to_crs84([4_200_000.0, 5_100_000.0, 4_200_000.0, 5_100_000.0])
            .unwrap();
        bbox_close(
            &point,
            [24.116499363, 60.198989865, 24.116499363, 60.198989865],
            1e-7,
        );
    }

    #[test]
    fn far_laea_points_fold_back_into_latitude_range() {
        // The Newton inverse lands on 230.93° here; cs2cs EPSG:3035
        // EPSG:4326 of N 3210000, E 17021000 is 50.929121°S, 174.232157°E.
        let point = FeatureCrs::Epsg3035
            .bbox_to_crs84([3_210_000.0, 17_021_000.0, 3_210_000.0, 17_021_000.0])
            .unwrap();
        bbox_close(
            &point,
            [174.232157, -50.929121, 174.232157, -50.929121],
            1e-5,
        );
    }

    #[test]
    fn projected_bbox_around_a_pole_spans_every_longitude() {
        // cs2cs: the North Pole is N 7369716.2555, E 4321000 in EPSG:3035.
        let bbox = FeatureCrs::Epsg3035
            .bbox_to_crs84([7_300_000.0, 4_200_000.0, 7_400_000.0, 4_400_000.0])
            .unwrap();
        assert_eq!((bbox.west, bbox.east, bbox.north), (-180.0, 180.0, 90.0));
        assert!(bbox.south > 88.0 && bbox.south < 90.0, "{bbox:?}");
    }

    #[test]
    fn projected_bbox_must_be_ordered_and_representable() {
        assert!(FeatureCrs::Epsg3067
            .bbox_to_crs84([400_000.0, 6_600_000.0, 300_000.0, 6_700_000.0])
            .is_err());
        // Farther than the antipode from LAEA Europe's origin: no point of
        // this box inverse-projects.
        let outside = FeatureCrs::Epsg3035
            .bbox_to_crs84([5.0e7, 5.0e7, 6.0e7, 6.0e7])
            .unwrap_err();
        assert!(outside.to_string().contains("outside"), "{outside}");
        assert!(FeatureCrs::Epsg3067
            .bbox_to_crs84([f64::NAN, 0.0, 1.0, 1.0])
            .is_err());
    }

    #[test]
    fn links_repeat_only_a_requested_crs() {
        assert_eq!(ResponseCrs::new(None).link_query(), "");
        assert_eq!(
            ResponseCrs::new(Some(FeatureCrs::Epsg3035)).link_query(),
            "crs=http%3A%2F%2Fwww.opengis.net%2Fdef%2Fcrs%2FEPSG%2F0%2F3035"
        );
        assert_eq!(
            ResponseCrs::new(None).content_crs(),
            "<http://www.opengis.net/def/crs/OGC/1.3/CRS84>"
        );
    }
}
