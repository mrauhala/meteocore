//! The grid a raster collection's samples lie on, in its own CRS (#1237).
//!
//! An engine reports the [`NativeGrid`] it reads in
//! [`RasterInfo::native_grid`](crate::map_engine::RasterInfo::native_grid),
//! built once per load or poll: the CRS, the cell counts, the cell size
//! and the outer corner, in the CRS's own units. Nothing is guessed:
//!
//! - the CRS is the [`Crs`] the engine computes the grid's coordinates with;
//! - the authority code (EPSG) is the one the **source** states, such as a
//!   GeoTIFF's `ProjectedCSTypeGeoKey`, or an ODIM projdef whose ellipsoid
//!   and every parameter match a registered definition. It is never derived
//!   from the [`Crs`] alone: TM35FIN parameters on the WGS 84 ellipsoid are
//!   not EPSG:3067, which is on GRS 1980 and the ETRS89 datum.
//!
//! The collection description carries it in two forms
//! ([`crate::ogc_extent::build_extent`]): the standard Common Part 2
//! `storageCrsBbox` and `grid` when the source's code is the `storageCrs`
//! the collection already advertises, and the `meteocore:nativeGrid`
//! extension member for every grid that is not lat/lon, whose `crs` is the
//! source's authority URI or else [`crs_wkt2`].

use std::sync::Arc;

use crate::geo::{Crs, GeoTransform, SweepAxis, CRS84_WKT, WGS84_A, WGS84_F};

/// The OGC URI of an EPSG CRS code.
pub fn epsg_uri(code: u32) -> String {
    format!("http://www.opengis.net/def/crs/EPSG/0/{code}")
}

/// A regular grid of `size` cells in its CRS: the cells' outer edges span
/// `origin .. origin + size × spacing` along each axis.
///
/// Axes are in grid order: x (columns, easting or longitude) first, then
/// y (rows, northing or latitude), whatever axis order the CRS's
/// registered definition gives; `origin` is the minimum x and minimum y
/// outer corner and `spacing` is positive, whichever way the source stores
/// its rows. Units are the CRS's: metres for a projected CRS (a
/// geostationary view's are scan angle times satellite height, as PROJ
/// defines them), degrees for lat/lon and rotated lat/lon.
///
/// Built through [`NativeGrid::new`], which checks the geometry and builds
/// the CRS reference once at load or poll, so no request rebuilds the WKT2
/// (Critical Rule 10).
#[derive(Debug, Clone, PartialEq)]
pub struct NativeGrid {
    crs: Crs,
    epsg: Option<u32>,
    size: [u32; 2],
    origin: [f64; 2],
    spacing: [f64; 2],
    /// [`Self::crs_ref`]: the authority URI, else the WKT2 of `crs`.
    crs_ref: Arc<str>,
}

impl NativeGrid {
    /// A grid of `size` `[nx, ny]` cells of `spacing` `[dx, dy]` (both
    /// positive) whose outer corner of minimum x and y is `origin`.
    /// `epsg` is the code the source states for `crs`, if any. `None` when
    /// a count is zero or a value is not finite and positive where it must
    /// be.
    pub fn new(
        crs: Crs,
        epsg: Option<u32>,
        size: [u32; 2],
        origin: [f64; 2],
        spacing: [f64; 2],
    ) -> Option<Self> {
        let valid = size.iter().all(|&n| n > 0)
            && origin.iter().all(|v| v.is_finite())
            && spacing.iter().all(|v| v.is_finite() && *v > 0.0);
        if !valid {
            return None;
        }
        let crs_ref: Arc<str> = match epsg {
            Some(code) => epsg_uri(code).into(),
            None => crs_wkt2(&crs).into(),
        };
        Some(Self {
            crs,
            epsg,
            size,
            origin,
            spacing,
            crs_ref,
        })
    }

    /// The grid of a [`GeoTransform`]: its top-left outer corner, positive
    /// pixel size and cell counts, in its CRS. `epsg` as for [`Self::new`].
    pub fn from_geo_transform(gt: &GeoTransform, epsg: Option<u32>) -> Option<Self> {
        Self::new(
            gt.crs.clone(),
            epsg,
            [gt.width, gt.height],
            [
                gt.origin_x,
                gt.origin_y - gt.pixel_height * f64::from(gt.height),
            ],
            [gt.pixel_width, gt.pixel_height],
        )
    }

    /// The grid whose cells' outer edges span `bbox` `[min_x, min_y, max_x,
    /// max_y]`, in `size` cells. `None` for an empty or inverted box.
    pub fn from_edges(crs: Crs, epsg: Option<u32>, size: [u32; 2], bbox: [f64; 4]) -> Option<Self> {
        let [x0, y0, x1, y1] = bbox;
        let [nx, ny] = size;
        if nx == 0 || ny == 0 {
            return None;
        }
        Self::new(
            crs,
            epsg,
            size,
            [x0, y0],
            [(x1 - x0) / f64::from(nx), (y1 - y0) / f64::from(ny)],
        )
    }

    /// The CRS the grid's coordinates are computed in.
    pub fn crs(&self) -> &Crs {
        &self.crs
    }

    /// The EPSG code the source states for the CRS, if any.
    pub fn epsg(&self) -> Option<u32> {
        self.epsg
    }

    /// `[nx, ny]`: columns, rows.
    pub fn size(&self) -> [u32; 2] {
        self.size
    }

    /// `[dx, dy]`: the cell size along x and y, positive, in [`Self::unit`].
    pub fn spacing(&self) -> [f64; 2] {
        self.spacing
    }

    /// The cells' outer edges `[min_x, min_y, max_x, max_y]`, in grid order.
    pub fn bbox(&self) -> [f64; 4] {
        let [x0, y0] = self.origin;
        let [dx, dy] = self.spacing;
        let [nx, ny] = self.size;
        [x0, y0, x0 + dx * f64::from(nx), y0 + dy * f64::from(ny)]
    }

    /// The axis unit: `deg` for lat/lon and rotated lat/lon, else `m`.
    pub fn unit(&self) -> &'static str {
        match self.crs {
            Crs::Wgs84 | Crs::RotatedLatLon { .. } => "deg",
            _ => "m",
        }
    }

    /// Whether the grid is a lat/lon grid of WGS 84 longitude and latitude.
    pub fn is_lon_lat(&self) -> bool {
        matches!(self.crs, Crs::Wgs84)
    }

    /// The CRS as the collection description gives it: the OGC URI of the
    /// source's EPSG code, else the ISO 19162:2019 (WKT2) text of
    /// [`Self::crs`] ([`crs_wkt2`]).
    pub fn crs_ref(&self) -> &str {
        &self.crs_ref
    }

    /// The OGC URI of the source's EPSG code, if it states one.
    pub fn authority_uri(&self) -> Option<&str> {
        self.epsg.map(|_| &*self.crs_ref)
    }
}

/// A registered CRS that MeteoCore advertises as a `storageCrs`, with the
/// facts a description needs: the EPSG name (PROJ 9.8's database), its
/// axis unit (`deg` or `m`) and whether its first axis is northing (or
/// latitude).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisteredCrs {
    pub uri: &'static str,
    pub name: &'static str,
    pub unit: &'static str,
    pub northing_first: bool,
}

/// The registered CRSs MeteoCore advertises as `storageCrs`
/// ([`crate::geo::native_crs_uri`]). Axis orders from `projinfo
/// EPSG:<code>`: EPSG:3035 lists northing before easting, EPSG:4326
/// latitude before longitude.
const REGISTERED: [RegisteredCrs; 5] = [
    RegisteredCrs {
        uri: "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
        name: "WGS 84 (CRS84)",
        unit: "deg",
        northing_first: false,
    },
    RegisteredCrs {
        uri: "http://www.opengis.net/def/crs/EPSG/0/4326",
        name: "WGS 84",
        unit: "deg",
        northing_first: true,
    },
    RegisteredCrs {
        uri: "http://www.opengis.net/def/crs/EPSG/0/3857",
        name: "WGS 84 / Pseudo-Mercator",
        unit: "m",
        northing_first: false,
    },
    RegisteredCrs {
        uri: "http://www.opengis.net/def/crs/EPSG/0/3067",
        name: "EUREF-FIN / TM35FIN(E,N)",
        unit: "m",
        northing_first: false,
    },
    RegisteredCrs {
        uri: "http://www.opengis.net/def/crs/EPSG/0/3035",
        name: "ETRS89-extended / LAEA Europe",
        unit: "m",
        northing_first: true,
    },
];

/// The registered CRS a URI names, when it is one MeteoCore advertises.
pub fn registered_crs(uri: &str) -> Option<&'static RegisteredCrs> {
    REGISTERED.iter().find(|r| r.uri == uri)
}

/// The name of a WKT CRS: the first quoted string, e.g. `Lambert
/// conformal conic` for `PROJCRS["Lambert conformal conic",…`.
pub fn wkt_name(wkt: &str) -> Option<&str> {
    let start = wkt.find('"')? + 1;
    let len = wkt[start..].find('"')?;
    let name = &wkt[start..start + len];
    (!name.trim().is_empty()).then_some(name)
}

const DEGREE: &str = r#"ANGLEUNIT["degree",0.0174532925199433]"#;
const METRE: &str = r#"LENGTHUNIT["metre",1]"#;
const UNITY: &str = r#"SCALEUNIT["unity",1]"#;

/// A number as WKT gives it: at most 12 decimals, no trailing zeros.
fn num(v: f64) -> String {
    let s = format!("{v:.12}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" {
        "0".to_owned()
    } else {
        s.to_owned()
    }
}

/// An angle in radians, as WKT degrees.
fn deg(radians: f64) -> String {
    num(radians.to_degrees())
}

fn angle(name: &str, radians: f64, epsg: u32) -> String {
    format!(
        r#"PARAMETER["{name}",{},{DEGREE},ID["EPSG",{epsg}]]"#,
        deg(radians)
    )
}

fn length(name: &str, metres: f64, epsg: u32) -> String {
    format!(
        r#"PARAMETER["{name}",{},{METRE},ID["EPSG",{epsg}]]"#,
        num(metres)
    )
}

fn scale(name: &str, k: f64, epsg: u32) -> String {
    format!(
        r#"PARAMETER["{name}",{},{UNITY},ID["EPSG",{epsg}]]"#,
        num(k)
    )
}

/// The geographic CRS a projection is based on: the WGS 84 ellipsoid with
/// no datum stated (MeteoCore computes these projections on it), a sphere,
/// or a geostationary view's own ellipsoid.
fn base_geogcrs(semi_major: f64, semi_minor: f64) -> String {
    let primem = format!(r#"PRIMEM["Greenwich",0,{DEGREE}]"#);
    if semi_major == WGS84_A && (semi_minor - WGS84_A * (1.0 - WGS84_F)).abs() < 1e-6 {
        return format!(
            r#"BASEGEOGCRS["Unknown based on WGS 84 ellipsoid",DATUM["Unknown based on WGS 84 ellipsoid",ELLIPSOID["WGS 84",6378137,298.257223563,{METRE}]],{primem}]"#
        );
    }
    if semi_major == semi_minor {
        let r = num(semi_major);
        return format!(
            r#"BASEGEOGCRS["Unknown based on a sphere",DATUM["Unknown based on a sphere of radius {r} m",ELLIPSOID["Sphere",{r},0,{METRE}]],{primem}]"#
        );
    }
    let inverse_flattening = num(semi_major / (semi_major - semi_minor));
    format!(
        r#"BASEGEOGCRS["Unknown based on an ellipsoid",DATUM["Unknown based on an ellipsoid",ELLIPSOID["Unknown",{},{inverse_flattening},{METRE}]],{primem}]"#,
        num(semi_major)
    )
}

const WGS84_B: f64 = WGS84_A * (1.0 - WGS84_F);

/// A projected CRS of easting then northing, in metres.
fn projcrs(name: &str, base: &str, method: &str, parameters: &[String]) -> String {
    format!(
        r#"PROJCRS["{name}",{base},CONVERSION["{name}",{method},{}],CS[Cartesian,2],AXIS["easting (X)",east,ORDER[1],{METRE}],AXIS["northing (Y)",north,ORDER[2],{METRE}]]"#,
        parameters.join(",")
    )
}

/// The ISO 19162:2019 (WKT2) text of a [`Crs`], as MeteoCore computes it:
///
/// - Transverse Mercator, Lambert azimuthal equal area, stereographic and
///   Web Mercator on the WGS 84 ellipsoid (the [`Crs`] carries no other),
///   with no datum stated;
/// - Lambert conformal conic on WGS 84 or on its sphere;
/// - a geostationary view on its own ellipsoid, sweeping x or y;
/// - rotated lat/lon by the GRIB convention (the southern pole, no axis
///   rotation) on WGS 84;
/// - lat/lon as OGC CRS84 ([`CRS84_WKT`]).
///
/// The names describe the projection; the methods and parameters are
/// EPSG's (or PROJ's, for the oblique stereographic, geostationary and
/// pole-rotation methods EPSG does not define). Each form is pinned in the
/// tests below against what `projinfo` reads back from it.
pub fn crs_wkt2(crs: &Crs) -> String {
    let wgs84 = || base_geogcrs(WGS84_A, WGS84_B);
    match *crs {
        Crs::Wgs84 => CRS84_WKT.to_owned(),
        Crs::TransverseMercator {
            lat0,
            lon0,
            k0,
            false_e,
            false_n,
        } => projcrs(
            "Transverse Mercator",
            &wgs84(),
            r#"METHOD["Transverse Mercator",ID["EPSG",9807]]"#,
            &[
                angle("Latitude of natural origin", lat0, 8801),
                angle("Longitude of natural origin", lon0, 8802),
                scale("Scale factor at natural origin", k0, 8805),
                length("False easting", false_e, 8806),
                length("False northing", false_n, 8807),
            ],
        ),
        Crs::LambertAzimuthalEqualArea {
            lat0,
            lon0,
            false_e,
            false_n,
        } => projcrs(
            "Lambert azimuthal equal area",
            &wgs84(),
            r#"METHOD["Lambert Azimuthal Equal Area",ID["EPSG",9820]]"#,
            &[
                angle("Latitude of natural origin", lat0, 8801),
                angle("Longitude of natural origin", lon0, 8802),
                length("False easting", false_e, 8806),
                length("False northing", false_n, 8807),
            ],
        ),
        Crs::LambertConformalConic {
            lat1,
            lat2,
            lat0,
            lon0,
            false_e,
            false_n,
            radius,
        } => projcrs(
            "Lambert conformal conic",
            &match radius {
                Some(r) => base_geogcrs(r, r),
                None => wgs84(),
            },
            r#"METHOD["Lambert Conic Conformal (2SP)",ID["EPSG",9802]]"#,
            &[
                angle("Latitude of false origin", lat0, 8821),
                angle("Longitude of false origin", lon0, 8822),
                angle("Latitude of 1st standard parallel", lat1, 8823),
                angle("Latitude of 2nd standard parallel", lat2, 8824),
                length("Easting at false origin", false_e, 8826),
                length("Northing at false origin", false_n, 8827),
            ],
        ),
        Crs::Stereographic {
            lat0,
            lon0,
            k0,
            false_e,
            false_n,
        } => {
            let parameters = [
                angle("Latitude of natural origin", lat0, 8801),
                angle("Longitude of natural origin", lon0, 8802),
                scale("Scale factor at natural origin", k0, 8805),
                length("False easting", false_e, 8806),
                length("False northing", false_n, 8807),
            ];
            // The polar aspect is EPSG's variant A; the oblique one is
            // PROJ's `stere`, not EPSG's double stereographic (9809).
            if (lat0.abs() - std::f64::consts::FRAC_PI_2).abs() < 1e-10 {
                projcrs(
                    "Polar stereographic",
                    &wgs84(),
                    r#"METHOD["Polar Stereographic (variant A)",ID["EPSG",9810]]"#,
                    &parameters,
                )
            } else {
                projcrs(
                    "Stereographic",
                    &wgs84(),
                    r#"METHOD["Stereographic"]"#,
                    &parameters,
                )
            }
        }
        Crs::RotatedLatLon {
            south_pole_lat,
            south_pole_lon,
        } => format!(
            r#"GEOGCRS["Rotated pole",{},DERIVINGCONVERSION["Pole rotation (GRIB convention)",METHOD["Pole rotation (GRIB convention)"],PARAMETER["Latitude of the southern pole (GRIB convention)",{},{DEGREE}],PARAMETER["Longitude of the southern pole (GRIB convention)",{},{DEGREE}],PARAMETER["Axis rotation (GRIB convention)",0,{DEGREE}]],CS[ellipsoidal,2],AXIS["longitude",east,ORDER[1],{DEGREE}],AXIS["latitude",north,ORDER[2],{DEGREE}]]"#,
            wgs84(),
            deg(south_pole_lat),
            deg(south_pole_lon)
        ),
        Crs::Geostationary {
            lon0,
            height,
            semi_major,
            semi_minor,
            sweep,
        } => projcrs(
            "Geostationary satellite view",
            &base_geogcrs(semi_major, semi_minor),
            match sweep {
                SweepAxis::X => r#"METHOD["Geostationary Satellite (Sweep X)"]"#,
                SweepAxis::Y => r#"METHOD["Geostationary Satellite (Sweep Y)"]"#,
            },
            &[
                angle("Longitude of natural origin", lon0, 8802),
                format!(r#"PARAMETER["Satellite Height",{},{METRE}]"#, num(height)),
                length("False easting", 0.0, 8806),
                length("False northing", 0.0, 8807),
            ],
        ),
        Crs::WebMercator => projcrs(
            "Web Mercator",
            &wgs84(),
            r#"METHOD["Popular Visualisation Pseudo Mercator",ID["EPSG",1024]]"#,
            &[
                angle("Latitude of natural origin", 0.0, 8801),
                angle("Longitude of natural origin", 0.0, 8802),
                length("False easting", 0.0, 8806),
                length("False northing", 0.0, 8807),
            ],
        ),
    }
}

#[cfg(test)]
mod tests;
