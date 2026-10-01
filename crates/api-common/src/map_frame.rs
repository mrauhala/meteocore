//! Map frames for OGC API - Maps Part 1 (OGC 20-058): the CRSs a map is
//! requested and rendered in, the area a map covers in its CRS, the
//! `Content-Bbox` header that reports it (`/req/core/map-response`), and the
//! Scaling class' scale-denominator ↔ resolution conversion. Shared so every
//! render route sizes and reports a map the same way.
//!
//! The projection math is ds-core's: `web_mercator` for EPSG:3857 and
//! `geo::projected_output_crs` for EPSG:3067/3035 (root CLAUDE.md Critical
//! Rule 4). This module only picks the transform and the axis order. Axis
//! order is the CRS's own wherever coordinates are spelled out — `bbox`,
//! `center` and `Content-Bbox` alike: CRS84 is longitude/latitude, EPSG:4326
//! latitude/longitude, EPSG:3857 and EPSG:3067 easting/northing, EPSG:3035
//! northing/easting (as in api-features' Part 2 support).

use ds_core::geo::{self, Crs};
use ds_core::map_engine::OutputCrs;
use ds_core::web_mercator;

/// A CRS a map can be requested and rendered in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MapCrs {
    /// WGS 84 longitude/latitude.
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

/// The display resolution the Scaling class assumes, 0.28 mm per pixel, in
/// metres.
pub const STANDARD_PIXEL_SIZE_M: f64 = 0.000_28;

/// The longer side, in pixels, of a map whose request sets neither `width`
/// nor `height` (`/rec/core/map-op`: "a reasonable width and height";
/// `/rec/scaling/dimensions`: "around 1000 x 1000 pixels").
pub const DEFAULT_MAP_SIZE: u32 = 1024;

/// Output-pixel cap of one map or map tile: `width × height` of the
/// returned image.
///
/// It bounds the output only, so passing it guarantees neither of the budgets
/// behind it ("Pixel budgets" in the root CLAUDE.md, #120): engine-geotiff's
/// `reader::MAX_MAP_PIXELS` has the same value but counts native *source*
/// pixels, and render admission charges 32 B per output pixel against
/// `MC_RENDER_MEMORY_MB`, whose 1024 MiB default admits at most 33 554 432.
///
/// Enforced by [`whole_pixels`] on the given or derived size: HTTP 400
/// "width * height (N) exceeds maximum of 64000000". [`MAX_MAP_DIMENSION`]²
/// equals this cap, so the per-side check always fires first today; this
/// one only guards a future per-side increase.
pub const MAX_MAP_PIXELS: u64 = 64_000_000;

/// Output-pixel cap per side (width or height). 8000 chosen so 8000 × 8000
/// equals [`MAX_MAP_PIXELS`] — a square at the per-dim cap doesn't trip the
/// pixel cap with a confusing second error. Tripping it is HTTP 400
/// "width and height must not exceed 8000".
pub const MAX_MAP_DIMENSION: u32 = 8000;

/// `width` or `height`: a positive integer up to [`MAX_MAP_DIMENSION`]
/// (`/req/scaling/width-definition` C, `height-definition` C). `None` when
/// the parameter is absent; the error is the 400 message.
pub fn parse_dimension(name: &str, value: Option<&str>) -> Result<Option<u32>, String> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let n = raw
        .parse::<u32>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("{name} '{raw}' must be a positive integer"))?;
    if n > MAX_MAP_DIMENSION {
        return Err(format!(
            "width and height must not exceed {MAX_MAP_DIMENSION}"
        ));
    }
    Ok(Some(n))
}

/// `scale-denominator`: a positive number
/// (`/req/scaling/scale-denominator-definition`). `None` when absent; the
/// error is the 400 message.
pub fn parse_scale_denominator(value: Option<&str>) -> Result<Option<f64>, String> {
    value
        .map(|raw| {
            raw.parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v > 0.0)
                .ok_or_else(|| format!("scale-denominator '{raw}' must be a positive number"))
        })
        .transpose()
}

/// Round a size to whole pixels and hold it to [`MAX_MAP_DIMENSION`] and
/// [`MAX_MAP_PIXELS`]: `cause` names what set it, for the 400 message when a
/// derived size is too large.
pub fn whole_pixels((width, height): (f64, f64), cause: &str) -> Result<(u32, u32), String> {
    let (width, height) = (width.round().max(1.0), height.round().max(1.0));
    if !(width.is_finite() && height.is_finite()) {
        return Err(format!("{cause} gives no finite map size"));
    }
    if width > f64::from(MAX_MAP_DIMENSION) || height > f64::from(MAX_MAP_DIMENSION) {
        return Err(format!(
            "width and height must not exceed {MAX_MAP_DIMENSION}: {cause} gives a \
             {width}x{height} map"
        ));
    }
    let (width, height) = (width as u32, height as u32);
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_MAP_PIXELS {
        return Err(format!(
            "width * height ({pixels}) exceeds maximum of {MAX_MAP_PIXELS}"
        ));
    }
    Ok((width, height))
}

impl MapCrs {
    /// Every CRS, in the order collections advertise them.
    pub const ALL: [MapCrs; 5] = [
        MapCrs::Crs84,
        MapCrs::Epsg4326,
        MapCrs::Epsg3857,
        MapCrs::Epsg3067,
        MapCrs::Epsg3035,
    ];

    /// The short identifier: the form the map routes have always accepted,
    /// and the cache key's.
    pub fn code(self) -> &'static str {
        match self {
            MapCrs::Crs84 => "CRS:84",
            MapCrs::Epsg4326 => "EPSG:4326",
            MapCrs::Epsg3857 => "EPSG:3857",
            MapCrs::Epsg3067 => "EPSG:3067",
            MapCrs::Epsg3035 => "EPSG:3035",
        }
    }

    /// The URI collections advertise and `Content-Crs` names.
    pub fn uri(self) -> &'static str {
        match self {
            MapCrs::Crs84 => "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
            MapCrs::Epsg4326 => "http://www.opengis.net/def/crs/EPSG/0/4326",
            MapCrs::Epsg3857 => "http://www.opengis.net/def/crs/EPSG/0/3857",
            MapCrs::Epsg3067 => "http://www.opengis.net/def/crs/EPSG/0/3067",
            MapCrs::Epsg3035 => "http://www.opengis.net/def/crs/EPSG/0/3035",
        }
    }

    fn curie(self) -> &'static str {
        match self {
            MapCrs::Crs84 => "OGC:CRS84",
            other => other.code(),
        }
    }

    /// Parse a CRS parameter value: the short identifier, the URI (or its
    /// `https` form), or the `[AUTHORITY:CODE]` safe CURIE, also without its
    /// brackets (`/per/spatial-subsetting/crs-curie`). `CRS84` alone names
    /// CRS84.
    pub fn parse(value: &str) -> Option<Self> {
        let trimmed = value.trim();
        let curie = trimmed
            .strip_prefix('[')
            .and_then(|v| v.strip_suffix(']'))
            .unwrap_or(trimmed);
        Self::ALL.into_iter().find(|crs| {
            let uri = crs.uri();
            trimmed == crs.code()
                || trimmed == uri
                || uri
                    .strip_prefix("http://")
                    .is_some_and(|rest| trimmed.strip_prefix("https://") == Some(rest))
                || curie == crs.curie()
                || (*crs == MapCrs::Crs84 && curie == "CRS84")
        })
    }

    /// The map CRS of an engine's `RasterInfo.native_crs` label, when it has
    /// a URI and is one of these.
    pub fn from_storage_label(label: &str) -> Option<Self> {
        geo::native_crs_uri(label).and_then(Self::parse)
    }

    /// The URIs of [`Self::ALL`], for messages and collection metadata.
    pub fn uris() -> Vec<&'static str> {
        Self::ALL.into_iter().map(Self::uri).collect()
    }

    /// Longitude/latitude rather than projected metres.
    pub fn is_geographic(self) -> bool {
        matches!(self, MapCrs::Crs84 | MapCrs::Epsg4326)
    }

    /// The first axis is latitude or northing.
    pub fn northing_first(self) -> bool {
        matches!(self, MapCrs::Epsg4326 | MapCrs::Epsg3035)
    }

    /// The `subset` abbreviations of the plane's x and y axes
    /// (`/req/spatial-subsetting/subset-definition`): `Lon`/`Lat` for a
    /// geographic CRS, `E`/`N` for a projected one.
    pub fn axis_abbreviations(self) -> (&'static str, &'static str) {
        if self.is_geographic() {
            ("Lon", "Lat")
        } else {
            ("E", "N")
        }
    }

    /// Two coordinates in this CRS's own axis order as plane `(x, y)`:
    /// easting or longitude first.
    pub fn to_plane(self, first: f64, second: f64) -> (f64, f64) {
        if self.northing_first() {
            (second, first)
        } else {
            (first, second)
        }
    }

    fn projection(self) -> Option<Crs> {
        geo::projected_output_crs(self.code())
    }

    /// Plane coordinates of a WGS 84 `(lon, lat)`. Web Mercator's northing
    /// is infinite at the poles.
    pub fn forward(self, lon: f64, lat: f64) -> (f64, f64) {
        match self {
            MapCrs::Crs84 | MapCrs::Epsg4326 => (lon, lat),
            MapCrs::Epsg3857 => (web_mercator::lon_to_x(lon), web_mercator::lat_to_y(lat)),
            MapCrs::Epsg3067 | MapCrs::Epsg3035 => match self.projection() {
                Some(crs) => crs.forward(lon, lat),
                None => (f64::NAN, f64::NAN),
            },
        }
    }

    /// WGS 84 `(lon, lat)` of plane coordinates, `None` outside the
    /// projection's valid area.
    pub fn inverse(self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (lon, lat) = match self {
            MapCrs::Crs84 | MapCrs::Epsg4326 => (x, y),
            MapCrs::Epsg3857 => (web_mercator::x_to_lon(x), web_mercator::y_to_lat(y)),
            MapCrs::Epsg3067 | MapCrs::Epsg3035 => self.projection()?.inverse(x, y)?,
        };
        (lon.is_finite() && lat.is_finite()).then_some((lon, lat))
    }
}

/// The area a map covers, in the CRS it is rendered in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    crs: MapCrs,
    /// `[west, south, east, north]` in longitude/latitude degrees for CRS84,
    /// EPSG:4326 and EPSG:3857 — the engines' viewport form, where a box
    /// crossing the antimeridian is unwrapped to `east > 180` — and
    /// `[min_e, min_n, max_e, max_n]` in metres for EPSG:3067 and EPSG:3035.
    rect: [f64; 4],
}

/// Why a box has no frame in the requested CRS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// The box lies entirely outside a projection's valid area.
    OutsideDomain,
}

impl Frame {
    /// The frame of a plane box of `crs`: `[min_x, min_y, max_x, max_y]`,
    /// longitude/latitude for a geographic CRS, metres otherwise.
    pub fn from_plane(crs: MapCrs, plane: [f64; 4]) -> Self {
        let rect = match crs {
            MapCrs::Epsg3857 => {
                let [x0, y0, x1, y1] = plane;
                [
                    web_mercator::x_to_lon(x0),
                    web_mercator::y_to_lat(y0),
                    web_mercator::x_to_lon(x1),
                    web_mercator::y_to_lat(y1),
                ]
            }
            _ => plane,
        };
        Self { crs, rect }
    }

    /// The frame, rendered in `to`, of a plane box of `from`. A geographic
    /// box crossing the antimeridian comes unwrapped (`max_x > 180`).
    ///
    /// Between different CRSs the frame is the box's envelope: a projected
    /// box becomes the longitude/latitude envelope of its edges, and that the
    /// projected envelope of its edges in `to` (`ds_core::geo`).
    pub fn from_box(from: MapCrs, plane: [f64; 4], to: MapCrs) -> Result<Self, FrameError> {
        if from == to || (from.is_geographic() && to.is_geographic()) {
            return Ok(Self::from_plane(to, plane));
        }
        let lonlat = match from {
            MapCrs::Crs84 | MapCrs::Epsg4326 => plane,
            MapCrs::Epsg3857 => Self::from_plane(from, plane).rect,
            MapCrs::Epsg3067 | MapCrs::Epsg3035 => {
                let crs = from.projection().ok_or(FrameError::OutsideDomain)?;
                geo::wgs84_envelope(&crs, plane).ok_or(FrameError::OutsideDomain)?
            }
        };
        Ok(match to {
            MapCrs::Crs84 | MapCrs::Epsg4326 | MapCrs::Epsg3857 => Self {
                crs: to,
                rect: lonlat,
            },
            MapCrs::Epsg3067 | MapCrs::Epsg3035 => {
                let crs = to.projection().ok_or(FrameError::OutsideDomain)?;
                Self {
                    crs: to,
                    rect: geo::projected_envelope(&crs, lonlat),
                }
            }
        })
    }

    /// The CRS the map is rendered in.
    pub fn crs(&self) -> MapCrs {
        self.crs
    }

    /// The frame as the engines take it: longitude/latitude degrees for
    /// CRS84, EPSG:4326 and EPSG:3857, metres for EPSG:3067 and EPSG:3035.
    pub fn rect(&self) -> [f64; 4] {
        self.rect
    }

    /// `[min_x, min_y, max_x, max_y]` in the CRS's plane: degrees for a
    /// geographic CRS, metres for the others (EPSG:3857 included).
    pub fn plane(&self) -> [f64; 4] {
        match self.crs {
            MapCrs::Epsg3857 => {
                let [w, s, e, n] = self.rect;
                [
                    web_mercator::lon_to_x(w),
                    web_mercator::lat_to_y(s),
                    web_mercator::lon_to_x(e),
                    web_mercator::lat_to_y(n),
                ]
            }
            _ => self.rect,
        }
    }

    /// The plane centre `(x, y)`.
    pub fn center(&self) -> (f64, f64) {
        let [x0, y0, x1, y1] = self.plane();
        ((x0 + x1) / 2.0, (y0 + y1) / 2.0)
    }

    /// Plane width over plane height: the width-to-height ratio of a map of
    /// square pixels over this frame.
    pub fn aspect(&self) -> f64 {
        let [x0, y0, x1, y1] = self.plane();
        (x1 - x0) / (y1 - y0)
    }

    /// The `get_raster_tile` viewport and output CRS: for a projected CRS the
    /// longitude/latitude envelope the engine reads, with the metres frame
    /// carried in [`OutputCrs::Projected`]. `None` when the projected frame is
    /// entirely outside its CRS's valid area.
    pub fn render_target(&self) -> Option<([f64; 4], OutputCrs)> {
        match self.crs {
            MapCrs::Crs84 | MapCrs::Epsg4326 => Some((self.rect, OutputCrs::Wgs84)),
            MapCrs::Epsg3857 => Some((self.rect, OutputCrs::WebMercator)),
            MapCrs::Epsg3067 | MapCrs::Epsg3035 => {
                let crs = self.crs.projection()?;
                let read = geo::wgs84_envelope(&crs, self.rect)?;
                Some((
                    read,
                    OutputCrs::Projected {
                        crs,
                        bbox: self.rect,
                    },
                ))
            }
        }
    }

    /// The `Content-Bbox` header value (`/req/core/map-response`): the lower
    /// and upper corners in the CRS's plane, in its own axis order. A
    /// geographic frame unwrapped past ±180° reads as the antimeridian
    /// crossing it is (`170,10,-170,20`); one spanning 360° or more keeps its
    /// longitudes. `None` when a coordinate is not finite, or for a Web
    /// Mercator frame reaching a pole, which has no northing.
    pub fn content_bbox(&self) -> Option<String> {
        if self.crs == MapCrs::Epsg3857 && !(self.rect[1] > -90.0 && self.rect[3] < 90.0) {
            return None;
        }
        let [mut x0, y0, mut x1, y1] = self.plane();
        if self.crs.is_geographic() && x1 - x0 < 360.0 && (x0 < -180.0 || x1 > 180.0) {
            x0 = wrap_longitude(x0);
            x1 = wrap_longitude(x1);
        }
        let values = if self.crs.northing_first() {
            [y0, x0, y1, x1]
        } else {
            [x0, y0, x1, y1]
        };
        values
            .iter()
            .all(|v| v.is_finite())
            // `+ 0.0` turns -0 into 0.
            .then(|| values.map(|v| (v + 0.0).to_string()).join(","))
    }
}

/// A longitude in `[-180, 180)`.
fn wrap_longitude(lon: f64) -> f64 {
    (lon + 180.0).rem_euclid(360.0) - 180.0
}

/// The plane units one pixel spans along x and y at plane point `(x, y)` of
/// `crs`, for a map at 1:`scale_denominator` on the standard 0.28 mm pixel
/// (`/req/scaling/scale-denominator-definition`).
///
/// A pixel spans `scale_denominator × 0.28 mm` on the ground, measured at
/// that point — physical metres, not CRS units: one Web Mercator metre is
/// `cos(latitude)` ground metres, one degree of longitude `cos(latitude)`
/// times a degree of latitude. `None` where the local scale degenerates (a
/// geographic pole) or the point is outside the projection.
pub fn resolution_at(crs: MapCrs, x: f64, y: f64, scale_denominator: f64) -> Option<(f64, f64)> {
    let ground = scale_denominator * STANDARD_PIXEL_SIZE_M;
    let (kx, ky) = ground_metres_per_unit(crs, x, y)?;
    let (rx, ry) = (ground / kx, ground / ky);
    (rx.is_finite() && ry.is_finite() && rx > 0.0 && ry > 0.0).then_some((rx, ry))
}

/// Ground metres per plane unit along x and y at `(x, y)`, from the
/// great-circle length of a short step either side of the point.
fn ground_metres_per_unit(crs: MapCrs, x: f64, y: f64) -> Option<(f64, f64)> {
    let step = if crs.is_geographic() { 1e-4 } else { 1.0 };
    let ground = |(x0, y0): (f64, f64), (x1, y1): (f64, f64)| {
        let (lon0, lat0) = crs.inverse(x0, y0)?;
        let (lon1, lat1) = crs.inverse(x1, y1)?;
        Some(geo::great_circle_distance_m(lon0, lat0, lon1, lat1) / (2.0 * step))
    };
    let kx = ground((x - step, y), (x + step, y))?;
    let ky = ground((x, y - step), (x, y + step))?;
    // Under a millimetre of ground per unit is a degenerate point (a pole).
    (kx > 1e-3 && ky > 1e-3 && kx.is_finite() && ky.is_finite()).then_some((kx, ky))
}

/// Width and height in pixels (unrounded) of a map over a frame of `aspect`
/// ([`Frame::aspect`]) when the request may omit either: an omitted side
/// keeps the pixels square, and with both omitted the longer side is
/// [`DEFAULT_MAP_SIZE`] (`/rec/core/map-op`).
pub fn size_for_aspect(aspect: f64, width: Option<u32>, height: Option<u32>) -> (f64, f64) {
    let default = f64::from(DEFAULT_MAP_SIZE);
    match (width, height) {
        (Some(w), Some(h)) => (f64::from(w), f64::from(h)),
        (Some(w), None) => (f64::from(w), f64::from(w) / aspect),
        (None, Some(h)) => (f64::from(h) * aspect, f64::from(h)),
        (None, None) if aspect >= 1.0 => (default, default / aspect),
        (None, None) => (default * aspect, default),
    }
}

/// Width and height in pixels (unrounded) of a map over `frame` at `(rx, ry)`
/// plane units per pixel.
pub fn size_for_resolution(frame: &Frame, rx: f64, ry: f64) -> (f64, f64) {
    let [x0, y0, x1, y1] = frame.plane();
    ((x1 - x0) / rx, (y1 - y0) / ry)
}

/// The frame of a `width × height` map centred on plane point `(x, y)` of
/// `crs` at `(rx, ry)` plane units per pixel.
pub fn frame_around(
    crs: MapCrs,
    (x, y): (f64, f64),
    (rx, ry): (f64, f64),
    width: u32,
    height: u32,
) -> Frame {
    let (half_w, half_h) = (f64::from(width) * rx / 2.0, f64::from(height) * ry / 2.0);
    Frame::from_plane(crs, [x - half_w, y - half_h, x + half_w, y + half_h])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_codes_uris_and_curies() {
        for (value, crs) in [
            ("CRS:84", MapCrs::Crs84),
            (
                "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
                MapCrs::Crs84,
            ),
            (
                "https://www.opengis.net/def/crs/OGC/1.3/CRS84",
                MapCrs::Crs84,
            ),
            ("[OGC:CRS84]", MapCrs::Crs84),
            ("OGC:CRS84", MapCrs::Crs84),
            ("CRS84", MapCrs::Crs84),
            ("EPSG:4326", MapCrs::Epsg4326),
            ("[EPSG:3857]", MapCrs::Epsg3857),
            (
                "http://www.opengis.net/def/crs/EPSG/0/3067",
                MapCrs::Epsg3067,
            ),
            (
                "https://www.opengis.net/def/crs/EPSG/0/3035",
                MapCrs::Epsg3035,
            ),
        ] {
            assert_eq!(MapCrs::parse(value), Some(crs), "{value}");
        }
        for value in [
            "EPSG:9999",
            "",
            "[EPSG:4326",
            "http://www.opengis.net/def/crs/EPSG/0/4258",
        ] {
            assert_eq!(MapCrs::parse(value), None, "{value}");
        }
        assert_eq!(
            MapCrs::from_storage_label("EPSG:3067"),
            Some(MapCrs::Epsg3067)
        );
        assert_eq!(MapCrs::from_storage_label("CRS:84"), Some(MapCrs::Crs84));
        assert_eq!(MapCrs::from_storage_label("TM"), None);
    }

    #[test]
    fn content_bbox_follows_the_axis_order() {
        let frame = |crs| Frame::from_box(MapCrs::Crs84, [10.0, 55.0, 30.0, 70.0], crs).unwrap();
        assert_eq!(frame(MapCrs::Crs84).content_bbox().unwrap(), "10,55,30,70");
        assert_eq!(
            frame(MapCrs::Epsg4326).content_bbox().unwrap(),
            "55,10,70,30"
        );
        let mercator = frame(MapCrs::Epsg3857).content_bbox().unwrap();
        let values: Vec<f64> = mercator.split(',').map(|v| v.parse().unwrap()).collect();
        assert!(
            (values[0] - web_mercator::lon_to_x(10.0)).abs() < 1e-6,
            "{mercator}"
        );
        assert!(
            (values[3] - web_mercator::lat_to_y(70.0)).abs() < 1e-6,
            "{mercator}"
        );
        // EPSG:3035 is northing first: its first value is the minimum
        // northing, millions of metres north of the false origin.
        let laea = frame(MapCrs::Epsg3035);
        let [min_e, min_n, max_e, max_n] = laea.plane();
        assert_eq!(
            laea.content_bbox().unwrap(),
            format!("{min_n},{min_e},{max_n},{max_e}")
        );
        // A Web Mercator frame reaching a pole has no finite box.
        let polar =
            Frame::from_box(MapCrs::Crs84, [0.0, 0.0, 10.0, 90.0], MapCrs::Epsg3857).unwrap();
        assert_eq!(polar.content_bbox(), None);
    }

    /// The seam test box: the unwrapped frame reports as the crossing box.
    #[test]
    fn content_bbox_reports_an_antimeridian_crossing() {
        for rect in [[170.0, 10.0, 190.0, 20.0], [-190.0, 10.0, -170.0, 20.0]] {
            let frame = Frame::from_plane(MapCrs::Crs84, rect);
            assert_eq!(frame.content_bbox().unwrap(), "170,10,-170,20", "{rect:?}");
        }
        let frame = Frame::from_plane(MapCrs::Epsg4326, [170.0, 10.0, 190.0, 20.0]);
        assert_eq!(frame.content_bbox().unwrap(), "10,170,20,-170");
        // Wider than the world: the longitudes stay as rendered.
        let frame = Frame::from_plane(MapCrs::Crs84, [-200.0, -10.0, 200.0, 10.0]);
        assert_eq!(frame.content_bbox().unwrap(), "-200,-10,200,10");
        let frame = Frame::from_plane(MapCrs::Crs84, [-180.0, -90.0, 180.0, 90.0]);
        assert_eq!(frame.content_bbox().unwrap(), "-180,-90,180,90");
    }

    #[test]
    fn projected_boxes_reach_other_crss_through_their_envelopes() {
        // A TM35FIN box over southern Finland in CRS84: degrees around 25°E.
        let frame = Frame::from_box(
            MapCrs::Epsg3067,
            [300_000.0, 6_700_000.0, 500_000.0, 6_900_000.0],
            MapCrs::Crs84,
        )
        .unwrap();
        let [w, s, e, n] = frame.rect();
        assert!(
            w > 21.0 && e < 27.5 && s > 60.0 && n < 62.5,
            "{:?}",
            frame.rect()
        );
        // The same box to its own CRS is unchanged, and renders projected.
        let same = Frame::from_box(
            MapCrs::Epsg3067,
            [300_000.0, 6_700_000.0, 500_000.0, 6_900_000.0],
            MapCrs::Epsg3067,
        )
        .unwrap();
        assert_eq!(
            same.rect(),
            [300_000.0, 6_700_000.0, 500_000.0, 6_900_000.0]
        );
        assert!(matches!(
            same.render_target(),
            Some((_, OutputCrs::Projected { .. }))
        ));
        // EPSG:3857 metres are exact in degrees.
        let x = web_mercator::lon_to_x(20.0);
        let y = web_mercator::lat_to_y(60.0);
        let mercator = Frame::from_box(MapCrs::Epsg3857, [0.0, 0.0, x, y], MapCrs::Crs84).unwrap();
        let [w, s, e, n] = mercator.rect();
        assert!(
            w.abs() < 1e-9 && s.abs() < 1e-9 && (e - 20.0).abs() < 1e-9 && (n - 60.0).abs() < 1e-9
        );
    }

    #[test]
    fn resolution_follows_the_local_ground_scale() {
        // 1:1 000 000 on 0.28 mm pixels is 280 m per pixel on the ground.
        let (rx, ry) = resolution_at(MapCrs::Crs84, 0.0, 0.0, 1_000_000.0).unwrap();
        let metres_per_degree = geo::great_circle_distance_m(0.0, 0.0, 0.0, 1.0);
        assert!((ry * metres_per_degree - 280.0).abs() < 1e-3, "{ry}");
        assert!((rx - ry).abs() / ry < 1e-6, "{rx} {ry}");
        // At 60°N a degree of longitude is half as long: twice the degrees.
        let (rx, ry) = resolution_at(MapCrs::Crs84, 25.0, 60.0, 1_000_000.0).unwrap();
        assert!((rx / ry - 2.0).abs() < 1e-3, "{rx} {ry}");
        // Web Mercator metres stretch by 1/cos(latitude).
        let y = web_mercator::lat_to_y(60.0);
        let (rx, ry) = resolution_at(MapCrs::Epsg3857, 0.0, y, 1_000_000.0).unwrap();
        assert!(
            (rx / 560.0 - 1.0).abs() < 1e-2 && (ry / 560.0 - 1.0).abs() < 1e-2,
            "{rx} {ry}"
        );
        // TM35FIN is close to true scale on its central meridian.
        let (rx, _) = resolution_at(MapCrs::Epsg3067, 500_000.0, 6_700_000.0, 1_000_000.0).unwrap();
        assert!((rx / 280.0 - 1.0).abs() < 1e-2, "{rx}");
        // The pole of a geographic CRS has no longitude scale.
        assert_eq!(resolution_at(MapCrs::Crs84, 0.0, 90.0, 1_000_000.0), None);
    }

    #[test]
    fn sizes_keep_square_pixels() {
        assert_eq!(size_for_aspect(2.0, None, None), (1024.0, 512.0));
        assert_eq!(size_for_aspect(0.5, None, None), (512.0, 1024.0));
        assert_eq!(size_for_aspect(2.0, Some(300), None), (300.0, 150.0));
        assert_eq!(size_for_aspect(2.0, None, Some(300)), (600.0, 300.0));
        assert_eq!(size_for_aspect(2.0, Some(10), Some(20)), (10.0, 20.0));
        let frame = Frame::from_plane(MapCrs::Crs84, [0.0, 0.0, 2.0, 1.0]);
        assert_eq!(size_for_resolution(&frame, 0.01, 0.01), (200.0, 100.0));
        let around = frame_around(MapCrs::Crs84, (10.0, 50.0), (0.1, 0.05), 20, 40);
        assert_eq!(around.rect(), [9.0, 49.0, 11.0, 51.0]);
    }

    /// The size rules Maps and map tiles share: positive integer sides,
    /// positive scales, and the output caps on every given or derived size.
    #[test]
    fn sizes_are_validated_against_the_caps() {
        assert_eq!(parse_dimension("width", None), Ok(None));
        assert_eq!(parse_dimension("width", Some("8000")), Ok(Some(8000)));
        for bad in ["0", "-1", "1.5", "x"] {
            assert_eq!(
                parse_dimension("height", Some(bad)),
                Err(format!("height '{bad}' must be a positive integer"))
            );
        }
        assert_eq!(
            parse_dimension("width", Some("8001")),
            Err("width and height must not exceed 8000".to_string())
        );
        assert_eq!(parse_scale_denominator(Some("1e6")), Ok(Some(1e6)));
        assert_eq!(parse_scale_denominator(None), Ok(None));
        for bad in ["0", "-5", "inf", "NaN", "x"] {
            assert!(parse_scale_denominator(Some(bad)).is_err(), "{bad}");
        }
        assert_eq!(whole_pixels((255.6, 0.2), "it"), Ok((256, 1)));
        assert_eq!(
            whole_pixels((8000.4, 10.0), "it"),
            Ok((MAX_MAP_DIMENSION, 10))
        );
        let err = whole_pixels((8001.0, 10.0), "scale-denominator over this tile").unwrap_err();
        assert_eq!(
            err,
            "width and height must not exceed 8000: scale-denominator over this tile gives a \
             8001x10 map"
        );
        assert!(whole_pixels((f64::INFINITY, 1.0), "it").is_err());
    }
}
