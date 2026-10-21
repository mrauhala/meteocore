//! Minimal PROJ-string parser for ODIM `/where/projdef` values.
//!
//! ODIM_H5 stores the grid projection as a PROJ.4 string (e.g.
//! `+proj=stere +lat_0=90 +lon_0=0 +lat_ts=60 +R=6371228 +x_0=0 +y_0=0`).
//! This module hand-parses the four projections we encounter across
//! European weather radar producers — `stere`, `tmerc`, `laea`, and
//! `longlat` — and maps each into a [`ds_core::geo::Crs`] variant, on the
//! earth figure the string states (`figure`, #1243).
//!
//! Out of scope (Phase 1 — see [[project_odim_engine_plan]] for the
//! consolidated plan):
//! - Arbitrary PROJ.4 strings beyond the four supported `+proj=` values
//! - Datum shifts (`+towgs84`, `+nadgrids`, a `+datum=`'s own shift): a
//!   grid's longitudes and latitudes are taken as WGS84's, as before
//! - WKT / PROJ-JSON
//!
//! When the input contains an unsupported `+proj=` or a malformed
//! parameter, return an error rather than silently falling back — a
//! quiet projection mismatch would surface as wrong pixel-to-world
//! mappings far downstream.

use std::collections::HashMap;

use ds_core::geo::{Crs, Ellipsoid, WGS84_A};

/// Errors from [`parse`].
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ParseError {
    #[error("PROJ string is missing the required `+proj=` token")]
    MissingProj,
    #[error("unsupported `+proj={0}` (Phase 1 supports stere/tmerc/laea/longlat)")]
    UnsupportedProj(String),
    #[error("PROJ parameter `{param}={value}` is not a valid number")]
    InvalidNumber { param: String, value: String },
    #[error("PROJ parameter `+{0}` is required for `+proj={1}` but missing")]
    MissingParam(&'static str, &'static str),
    #[error("`+ellps={0}` is not an ellipsoid PROJ knows (`proj -le`)")]
    UnknownEllipsoid(String),
    #[error("`+datum={0}` is not a datum PROJ knows")]
    UnknownDatum(String),
    #[error("unsupported earth figure: {0}")]
    UnsupportedFigure(String),
}

/// Parse a PROJ.4 string from an ODIM `/where/projdef` value into a
/// [`Crs`].
///
/// Recognised parameters per projection:
/// - `stere`: `+lat_0`, `+lon_0`, `+k_0`/`+k`, `+lat_ts`, `+x_0`, `+y_0`
/// - `tmerc`: `+lat_0`, `+lon_0`, `+k_0`/`+k`, `+x_0`, `+y_0`
/// - `laea`:  `+lat_0`, `+lon_0`, `+x_0`, `+y_0`
/// - `longlat`: (no parameters needed beyond `+proj=longlat`)
///
/// `stere`, `tmerc` and `laea` are computed on the earth figure the string
/// states (`figure`): a `+R=` sphere, a named ellipsoid or datum, or
/// axes. A grid's `xscale`/`yscale` are metres on that figure. A `longlat`
/// grid takes its coordinates as given and needs no figure. Other
/// parameters (`+units=`, `+towgs84=`, `+no_defs`, …) are ignored.
///
/// For polar stereographic (`+lat_0=±90`) with `+lat_ts` and without an
/// explicit `+k_0`, the scale factor is the one that makes the scale true
/// at `lat_ts` on the figure ([`ds_core::geo::polar_stereographic_k0`]),
/// `(1 + sin|lat_ts|) / 2` on a sphere, as PROJ's `+lat_ts` does. FMI/OPERA
/// polar composites typically state `lat_ts=60`.
pub fn parse(projdef: &str) -> Result<Crs, ParseError> {
    let params = tokenize(projdef);

    let proj = params.get("proj").ok_or(ParseError::MissingProj)?.as_str();

    match proj {
        "longlat" | "latlong" | "lonlat" | "latlon" => Ok(Crs::Wgs84),
        "stere" => parse_stere(&params),
        "tmerc" => parse_tmerc(&params),
        "laea" => parse_laea(&params),
        other => Err(ParseError::UnsupportedProj(other.to_string())),
    }
}

/// Split a PROJ.4 string into `key → value` pairs. Bare flags
/// (`+no_defs`) parse to an empty string. The leading `+` is stripped.
/// Whitespace is the only separator — PROJ doesn't allow quoting and
/// neither do we.
fn tokenize(projdef: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for token in projdef.split_whitespace() {
        let Some(body) = token.strip_prefix('+') else {
            continue;
        };
        match body.split_once('=') {
            Some((k, v)) => {
                out.insert(k.to_string(), v.to_string());
            }
            None => {
                out.insert(body.to_string(), String::new());
            }
        }
    }
    out
}

fn parse_f64(params: &HashMap<String, String>, key: &str) -> Result<Option<f64>, ParseError> {
    match params.get(key) {
        None => Ok(None),
        Some(v) => v
            .parse::<f64>()
            .map(Some)
            .map_err(|_| ParseError::InvalidNumber {
                param: key.to_string(),
                value: v.clone(),
            }),
    }
}

/// `+k_0` is the canonical form; `+k` is the historical shorthand
/// PROJ still accepts. Prefer `+k_0` when both appear.
fn parse_k0(params: &HashMap<String, String>) -> Result<Option<f64>, ParseError> {
    if params.contains_key("k_0") {
        parse_f64(params, "k_0")
    } else {
        parse_f64(params, "k")
    }
}

/// How PROJ states a named ellipsoid's shape after its semi-major axis:
/// the inverse flattening or the semi-minor axis, in metres.
#[derive(Clone, Copy)]
enum Shape {
    Rf(f64),
    B(f64),
}

/// The ellipsoids PROJ's `+ellps=` names, verbatim from `proj -le` (PROJ
/// 9.8.1): name, semi-major axis in metres (`WGS84_A` where PROJ states
/// 6378137.0) and shape. `cs2cs` builds every one with exactly these
/// numbers (pinned in this module's tests).
#[rustfmt::skip]
const NAMED_ELLIPSOIDS: [(&str, f64, Shape); 46] = {
    use Shape::{Rf, B};
    [
        ("MERIT", WGS84_A, Rf(298.257)),             // MERIT 1983
        ("SGS85", 6378136.0, Rf(298.257)),             // Soviet Geodetic System 85
        ("GRS80", WGS84_A, Rf(298.257222101)),       // GRS 1980(IUGG, 1980)
        ("IAU76", 6378140.0, Rf(298.257)),             // IAU 1976
        ("airy", 6377563.396, Rf(299.3249646)),        // Airy 1830
        ("APL4.9", WGS84_A, Rf(298.25)),             // Appl. Physics. 1965
        ("NWL9D", 6378145.0, Rf(298.25)),              // Naval Weapons Lab., 1965
        ("mod_airy", 6377340.189, B(6356034.446)),     // Modified Airy
        ("andrae", 6377104.43, Rf(300.0)),             // Andrae 1876 (Den., Iclnd.)
        ("danish", 6377019.2563, Rf(300.0)),           // Andrae 1876 (Denmark, Iceland)
        ("aust_SA", 6378160.0, Rf(298.25)),            // Australian Natl & S. Amer. 1969
        ("GRS67", 6378160.0, Rf(298.2471674270)),      // GRS 67(IUGG 1967)
        ("GSK2011", 6378136.5, Rf(298.2564151)),       // GSK-2011
        ("bessel", 6377397.155, Rf(299.1528128)),      // Bessel 1841
        ("bess_nam", 6377483.865, Rf(299.1528128)),    // Bessel 1841 (Namibia)
        ("clrk66", 6378206.4, B(6356583.8)),           // Clarke 1866
        ("clrk80", 6378249.145, Rf(293.4663)),         // Clarke 1880 mod.
        ("clrk80ign", 6378249.2, Rf(293.4660212936269)), // Clarke 1880 (IGN).
        ("CPM", 6375738.7, Rf(334.29)),                // Comm. des Poids et Mesures 1799
        ("delmbr", 6376428.0, Rf(311.5)),              // Delambre 1810 (Belgium)
        ("engelis", 6378136.05, Rf(298.2566)),         // Engelis 1985
        ("evrst30", 6377276.345, Rf(300.8017)),        // Everest 1830
        ("evrst48", 6377304.063, Rf(300.8017)),        // Everest 1948
        ("evrst56", 6377301.243, Rf(300.8017)),        // Everest 1956
        ("evrst69", 6377295.664, Rf(300.8017)),        // Everest 1969
        ("evrstSS", 6377298.556, Rf(300.8017)),        // Everest (Sabah & Sarawak)
        ("fschr60", 6378166.0, Rf(298.3)),             // Fischer (Mercury Datum) 1960
        ("fschr60m", 6378155.0, Rf(298.3)),            // Modified Fischer 1960
        ("fschr68", 6378150.0, Rf(298.3)),             // Fischer 1968
        ("helmert", 6378200.0, Rf(298.3)),             // Helmert 1906
        ("hough", 6378270.0, Rf(297.0)),               // Hough
        ("intl", 6378388.0, Rf(297.0)),                // International 1924 (Hayford 1909, 1910)
        ("krass", 6378245.0, Rf(298.3)),               // Krassovsky, 1942
        ("kaula", 6378163.0, Rf(298.24)),              // Kaula 1961
        ("lerch", 6378139.0, Rf(298.257)),             // Lerch 1979
        ("mprts", 6397300.0, Rf(191.0)),               // Maupertius 1738
        ("new_intl", 6378157.5, B(6356772.2)),         // New International 1967
        ("plessis", 6376523.0, B(6355863.0)),          // Plessis 1817 (France)
        ("PZ90", 6378136.0, Rf(298.25784)),            // PZ-90
        ("SEasia", 6378155.0, B(6356773.3205)),        // Southeast Asia
        ("walbeck", 6376896.0, B(6355834.8467)),       // Walbeck
        ("WGS60", 6378165.0, Rf(298.3)),               // WGS 60
        ("WGS66", 6378145.0, Rf(298.25)),              // WGS 66
        ("WGS72", 6378135.0, Rf(298.26)),              // WGS 72
        ("WGS84", WGS84_A, Rf(298.257223563)),       // WGS 84
        ("sphere", 6370997.0, B(6370997.0)),           // Normal Sphere (r=6370997)
    ]
};

/// The ellipsoid PROJ's `+ellps=<name>` names; names are case-sensitive,
/// as in PROJ.
fn named_ellipsoid(name: &str) -> Option<Ellipsoid> {
    let (_, a, shape) = NAMED_ELLIPSOIDS.iter().find(|(n, ..)| *n == name)?;
    Some(match *shape {
        Shape::Rf(rf) => Ellipsoid::from_inverse_flattening(*a, rf),
        Shape::B(b) => Ellipsoid::from_axes(*a, b),
    })
}

/// The ellipsoid of a datum PROJ's `+datum=` names, as PROJ 9.8.1 builds a
/// CRS from it (each checked against `cs2cs`). Only the figure is taken:
/// the datum's shift is not applied (module docs).
fn datum_ellipsoid(name: &str) -> Option<Ellipsoid> {
    match name {
        "WGS84" => named_ellipsoid("WGS84"),
        "GGRS87" | "NAD83" => named_ellipsoid("GRS80"),
        "NAD27" => named_ellipsoid("clrk66"),
        "potsdam" | "hermannskogel" => named_ellipsoid("bessel"),
        "carthage" => named_ellipsoid("clrk80ign"),
        "nzgd49" => named_ellipsoid("intl"),
        "OSGB36" => named_ellipsoid("airy"),
        // EPSG's Airy Modified 1849 (1/f 299.3249646), which PROJ builds
        // this datum on, not `mod_airy`'s semi-minor axis 0.2 mm off it.
        "ire65" => Some(Ellipsoid::from_inverse_flattening(
            6_377_340.189,
            299.324_964_6,
        )),
        _ => None,
    }
}

/// The earth figure a projdef states (#1243), by the rules PROJ 9.8.1
/// builds a CRS from a PROJ string with, each checked against `cs2cs`:
///
/// - `+R=r` is a sphere of radius `r`, whatever else the string states.
/// - `+ellps=` names one of PROJ's ellipsoids ([`NAMED_ELLIPSOIDS`]),
///   `+datum=` one of its datums ([`datum_ellipsoid`]); both may appear
///   when they name the same figure.
/// - `+a=` with a shape, the first of `+rf`, `+f`, `+es`, `+e` and `+b`
///   in that order, is that ellipsoid, a sphere when it is flat; `+a=`
///   alone is a sphere of radius `a`.
/// - A string that states none of these is on WGS 84, PROJ's own default.
///
/// Everything else is an error, never a WGS 84 guess: an unknown name, a
/// shape without `+a`, a prolate or degenerate figure, a spherification
/// (`+R_A`, …), and a name stated together with axes, whose meaning PROJ
/// leaves to the parameter (`+ellps=intl +b=…` takes the `b`, `+ellps=intl
/// +f=…` ignores the `f`).
fn figure(params: &HashMap<String, String>) -> Result<Ellipsoid, ParseError> {
    if let Some(key) = params.keys().find(|k| k.starts_with("R_")) {
        return Err(ParseError::UnsupportedFigure(format!(
            "`+{key}`, a sphere derived from the ellipsoid"
        )));
    }
    let length = |key: &str| -> Result<Option<f64>, ParseError> {
        match parse_f64(params, key)? {
            Some(v) if !(v.is_finite() && v > 0.0) => Err(ParseError::UnsupportedFigure(format!(
                "`+{key}={v}` is not a positive length"
            ))),
            v => Ok(v),
        }
    };
    if let Some(radius) = length("R")? {
        return Ok(Ellipsoid::sphere(radius));
    }

    let ellps = match params.get("ellps") {
        Some(name) => {
            Some(named_ellipsoid(name).ok_or_else(|| ParseError::UnknownEllipsoid(name.clone()))?)
        }
        None => None,
    };
    let datum = match params.get("datum") {
        Some(name) => {
            Some(datum_ellipsoid(name).ok_or_else(|| ParseError::UnknownDatum(name.clone()))?)
        }
        None => None,
    };
    let named = match (ellps, datum) {
        (Some(e), Some(d)) if e != d => {
            return Err(ParseError::UnsupportedFigure(
                "`+ellps=` and `+datum=` name different ellipsoids".into(),
            ))
        }
        (e, d) => e.or(d),
    };

    // PROJ reads the first of these shape parameters it finds.
    const SHAPES: [&str; 5] = ["rf", "f", "es", "e", "b"];
    let shape = SHAPES.into_iter().find(|key| params.contains_key(*key));
    let a = length("a")?;
    match (named, a, shape) {
        (None, None, None) => Ok(Ellipsoid::WGS84),
        (Some(named), None, None) => Ok(named),
        (Some(_), ..) => Err(ParseError::UnsupportedFigure(
            "a named ellipsoid or datum together with `+a`/`+rf`/`+f`/`+es`/`+e`/`+b`".into(),
        )),
        (None, None, Some(key)) => Err(ParseError::UnsupportedFigure(format!(
            "`+{key}` without the semi-major axis `+a`"
        ))),
        (None, Some(a), None) => Ok(Ellipsoid::sphere(a)),
        (None, Some(a), Some(key)) => {
            let v = parse_f64(params, key)?.unwrap_or(f64::NAN);
            // The semi-minor axis each shape gives; `None` is not a sphere
            // or an oblate ellipsoid.
            let b = match key {
                "rf" if v.is_finite() && v > 1.0 => {
                    return Ok(Ellipsoid::from_inverse_flattening(a, v))
                }
                "f" if (0.0..1.0).contains(&v) => Some(a * (1.0 - v)),
                "es" if (0.0..1.0).contains(&v) => Some(a * (1.0 - v).sqrt()),
                "e" if (0.0..1.0).contains(&v) => Some(a * (1.0 - v * v).sqrt()),
                "b" if v > 0.0 && v <= a => Some(v),
                _ => None,
            };
            b.map(|b| Ellipsoid::from_axes(a, b)).ok_or_else(|| {
                ParseError::UnsupportedFigure(format!(
                    "`+a={a} +{key}={v}` is not a sphere or an oblate ellipsoid"
                ))
            })
        }
    }
}

fn parse_stere(params: &HashMap<String, String>) -> Result<Crs, ParseError> {
    let figure = figure(params)?;
    let lat0 = parse_f64(params, "lat_0")?.unwrap_or(0.0);
    let lon0 = parse_f64(params, "lon_0")?.unwrap_or(0.0);
    let false_e = parse_f64(params, "x_0")?.unwrap_or(0.0);
    let false_n = parse_f64(params, "y_0")?.unwrap_or(0.0);

    let k0 = match parse_k0(params)? {
        Some(k) => k,
        None => match parse_f64(params, "lat_ts")? {
            // Derive k0 from lat_ts so the scale at the latitude-of-
            // true-scale is exactly 1. Two cases:
            //
            // Polar (lat_0 = ±90): PROJ's `+proj=stere` uses Snyder
            // eq. 21-39 `ρ = a·m_c·t/t_c` with lat_ts directly, and
            // `ds_core::geo::polar_stereographic_k0` gives the k0 on the
            // figure that makes our `ρ = 2·a·k₀·t/D` match it. On an
            // ellipsoid the spherical shortcut `(1 + sin|lat_ts|)/2` is
            // ~0.012% off, ~200 m on a 3000 km radius — observable in
            // `stereographic_inverse_absolute_polar`.
            //
            // Oblique (|lat_0| < π/2): no comparable case-2 formula
            // applies; ODIM oblique producers (DMI) ship +k=1
            // explicitly rather than relying on lat_ts, so this
            // branch only triggers for hypothetical oblique
            // configs and we keep the general spherical form.
            Some(lat_ts) => {
                let lat0_rad = lat0.to_radians();
                let lat_ts_rad = lat_ts.to_radians();
                if (lat0_rad.abs() - std::f64::consts::FRAC_PI_2).abs() < 1e-10 {
                    ds_core::geo::polar_stereographic_k0(lat_ts_rad, &figure)
                } else {
                    (1.0 + lat0_rad.sin() * lat_ts_rad.sin() + lat0_rad.cos() * lat_ts_rad.cos())
                        / 2.0
                }
            }
            // No +k_0, no +lat_ts → PROJ default is 1.0.
            None => 1.0,
        },
    };

    Ok(Crs::Stereographic {
        lat0: lat0.to_radians(),
        lon0: lon0.to_radians(),
        k0,
        false_e,
        false_n,
        figure,
    })
}

fn parse_tmerc(params: &HashMap<String, String>) -> Result<Crs, ParseError> {
    let figure = figure(params)?;
    let lat0 = parse_f64(params, "lat_0")?.unwrap_or(0.0);
    let lon0 = parse_f64(params, "lon_0")?.unwrap_or(0.0);
    let k0 = parse_k0(params)?.unwrap_or(1.0);
    let false_e = parse_f64(params, "x_0")?.unwrap_or(0.0);
    let false_n = parse_f64(params, "y_0")?.unwrap_or(0.0);

    Ok(Crs::TransverseMercator {
        lat0: lat0.to_radians(),
        lon0: lon0.to_radians(),
        k0,
        false_e,
        false_n,
        figure,
    })
}

fn parse_laea(params: &HashMap<String, String>) -> Result<Crs, ParseError> {
    let figure = figure(params)?;
    let lat0 = parse_f64(params, "lat_0")?.unwrap_or(0.0);
    let lon0 = parse_f64(params, "lon_0")?.unwrap_or(0.0);
    let false_e = parse_f64(params, "x_0")?.unwrap_or(0.0);
    let false_n = parse_f64(params, "y_0")?.unwrap_or(0.0);

    Ok(Crs::LambertAzimuthalEqualArea {
        lat0: lat0.to_radians(),
        lon0: lon0.to_radians(),
        false_e,
        false_n,
        figure,
    })
}

/// The EPSG code a projdef states, when it names the ellipsoid of a
/// registered CRS and gives exactly its parameters (#1237): `+ellps=GRS80`
/// with TM35FIN's (EPSG:3067: `tmerc`, `lat_0=0 lon_0=27 k=0.9996
/// x_0=500000 y_0=0`) or ETRS89-LAEA Europe's (EPSG:3035: `laea`,
/// `lat_0=52 lon_0=10 x_0=4321000 y_0=3210000`).
///
/// Anything else states no code: another or no ellipsoid, a sphere, a
/// datum shift other than none (`+towgs84` of zeros), a unit other than
/// metres, or any parameter beyond these. The code is never inferred from
/// the parameters alone: on another ellipsoid they are another CRS.
pub fn stated_epsg(projdef: &str) -> Option<u32> {
    let params = tokenize(projdef);
    const ALLOWED: [&str; 12] = [
        "proj", "lat_0", "lon_0", "k", "k_0", "x_0", "y_0", "ellps", "towgs84", "units", "no_defs",
        "type",
    ];
    if params.keys().any(|k| !ALLOWED.contains(&k.as_str())) {
        return None;
    }
    let text = |key: &str| params.get(key).map(String::as_str);
    if text("ellps") != Some("GRS80")
        || text("units").is_some_and(|u| u != "m")
        || text("type").is_some_and(|t| t != "crs")
        || text("no_defs").is_some_and(|v| !v.is_empty())
    {
        return None;
    }
    if let Some(shift) = text("towgs84") {
        let zero = shift
            .split(',')
            .all(|v| v.trim().parse::<f64>().is_ok_and(|v| v == 0.0));
        if !zero {
            return None;
        }
    }
    // A missing parameter takes PROJ's default, 0 (1 for the scale).
    let value = |key: &str, default: f64| -> Option<f64> {
        match text(key) {
            None => Some(default),
            Some(v) => v.parse().ok(),
        }
    };
    let k = match (text("k_0"), text("k")) {
        (Some(_), _) => value("k_0", 1.0)?,
        (None, _) => value("k", 1.0)?,
    };
    let is = |key: &str, default: f64, expected: f64| value(key, default) == Some(expected);
    match text("proj")? {
        "tmerc"
            if is("lat_0", 0.0, 0.0)
                && is("lon_0", 0.0, 27.0)
                && k == 0.9996
                && is("x_0", 0.0, 500_000.0)
                && is("y_0", 0.0, 0.0) =>
        {
            Some(3067)
        }
        "laea"
            if text("k").is_none()
                && text("k_0").is_none()
                && is("lat_0", 0.0, 52.0)
                && is("lon_0", 0.0, 10.0)
                && is("x_0", 0.0, 4_321_000.0)
                && is("y_0", 0.0, 3_210_000.0) =>
        {
            Some(3035)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1237: a projdef states an EPSG code only with the registered
    /// ellipsoid and exactly the registered parameters.
    #[test]
    fn stated_epsg_needs_the_ellipsoid_and_every_parameter() {
        let tm35fin = "+proj=tmerc +lat_0=0 +lon_0=27 +k=0.9996 +x_0=500000 +y_0=0 +ellps=GRS80";
        assert_eq!(stated_epsg(tm35fin), Some(3067));
        assert_eq!(
            stated_epsg(&format!(
                "{tm35fin} +towgs84=0,0,0,0,0,0,0 +units=m +no_defs"
            )),
            Some(3067)
        );
        assert_eq!(
            stated_epsg("+proj=laea +lat_0=52 +lon_0=10 +x_0=4321000 +y_0=3210000 +ellps=GRS80"),
            Some(3035)
        );
        for other in [
            // The same parameters on WGS 84, on no stated ellipsoid, on a
            // sphere: not EPSG:3067.
            "+proj=tmerc +lat_0=0 +lon_0=27 +k=0.9996 +x_0=500000 +y_0=0 +ellps=WGS84",
            "+proj=tmerc +lat_0=0 +lon_0=27 +k=0.9996 +x_0=500000 +y_0=0",
            "+proj=tmerc +lat_0=0 +lon_0=27 +k=0.9996 +x_0=500000 +y_0=0 +R=6371228",
            // One parameter off, or missing where the default differs.
            "+proj=tmerc +lat_0=0 +lon_0=27 +k=0.9996 +x_0=500001 +y_0=0 +ellps=GRS80",
            "+proj=tmerc +lat_0=0 +lon_0=27 +x_0=500000 +y_0=0 +ellps=GRS80",
            // A datum shift, another unit, an extra parameter.
            "+proj=tmerc +lat_0=0 +lon_0=27 +k=0.9996 +x_0=500000 +y_0=0 +ellps=GRS80 +towgs84=1,0,0",
            "+proj=tmerc +lat_0=0 +lon_0=27 +k=0.9996 +x_0=500000 +y_0=0 +ellps=GRS80 +units=km",
            "+proj=tmerc +lat_0=0 +lon_0=27 +k=0.9996 +x_0=500000 +y_0=0 +ellps=GRS80 +lat_ts=60",
            // OPERA's LAEA and a stereographic composite.
            "+proj=laea +lat_0=55 +lon_0=10 +x_0=1950000 +y_0=-2100000 +units=m +ellps=WGS84",
            "+proj=stere +lat_0=90 +lon_0=0 +lat_ts=60 +ellps=GRS80",
        ] {
            assert_eq!(stated_epsg(other), None, "{other}");
        }
        // GRS 1980 stated by its axes or by a datum is the figure parse
        // computes on (#1243), but not the named ellipsoid: no code.
        let by_axes = format!(
            "+proj=tmerc +lat_0=0 +lon_0=27 +k=0.9996 +x_0=500000 +y_0=0 +a={WGS84_A} +rf=298.257222101"
        );
        let by_datum = "+proj=tmerc +lat_0=0 +lon_0=27 +k=0.9996 +x_0=500000 +y_0=0 +datum=NAD83";
        for projdef in [tm35fin, &by_axes, by_datum] {
            assert_eq!(figure_of(&parse(projdef).unwrap()), Ellipsoid::GRS80);
        }
        assert_eq!(stated_epsg(&by_axes), None);
        assert_eq!(stated_epsg(by_datum), None);
    }

    /// Asserts two `Crs` values are equal across float-comparison
    /// tolerance. The enum doesn't derive `PartialEq` because it carries
    /// floats, so we destructure each variant and compare individually.
    fn assert_crs_eq(actual: &Crs, expected: &Crs) {
        const EPS: f64 = 1e-9;
        match (actual, expected) {
            (Crs::Wgs84, Crs::Wgs84) => {}
            (
                Crs::Stereographic {
                    lat0: a_lat0,
                    lon0: a_lon0,
                    k0: a_k0,
                    false_e: a_e,
                    false_n: a_n,
                    figure: a_figure,
                },
                Crs::Stereographic {
                    lat0: e_lat0,
                    lon0: e_lon0,
                    k0: e_k0,
                    false_e: e_e,
                    false_n: e_n,
                    figure: e_figure,
                },
            ) => {
                assert_eq!(a_figure, e_figure);
                assert!((a_lat0 - e_lat0).abs() < EPS, "lat0: {a_lat0} vs {e_lat0}");
                assert!((a_lon0 - e_lon0).abs() < EPS, "lon0: {a_lon0} vs {e_lon0}");
                assert!((a_k0 - e_k0).abs() < EPS, "k0: {a_k0} vs {e_k0}");
                assert!((a_e - e_e).abs() < EPS, "false_e: {a_e} vs {e_e}");
                assert!((a_n - e_n).abs() < EPS, "false_n: {a_n} vs {e_n}");
            }
            (
                Crs::TransverseMercator {
                    lat0: a_lat0,
                    lon0: a_lon0,
                    k0: a_k0,
                    false_e: a_e,
                    false_n: a_n,
                    figure: a_figure,
                },
                Crs::TransverseMercator {
                    lat0: e_lat0,
                    lon0: e_lon0,
                    k0: e_k0,
                    false_e: e_e,
                    false_n: e_n,
                    figure: e_figure,
                },
            ) => {
                assert_eq!(a_figure, e_figure);
                assert!((a_lat0 - e_lat0).abs() < EPS);
                assert!((a_lon0 - e_lon0).abs() < EPS);
                assert!((a_k0 - e_k0).abs() < EPS);
                assert!((a_e - e_e).abs() < EPS);
                assert!((a_n - e_n).abs() < EPS);
            }
            (
                Crs::LambertAzimuthalEqualArea {
                    lat0: a_lat0,
                    lon0: a_lon0,
                    false_e: a_e,
                    false_n: a_n,
                    figure: a_figure,
                },
                Crs::LambertAzimuthalEqualArea {
                    lat0: e_lat0,
                    lon0: e_lon0,
                    false_e: e_e,
                    false_n: e_n,
                    figure: e_figure,
                },
            ) => {
                assert_eq!(a_figure, e_figure);
                assert!((a_lat0 - e_lat0).abs() < EPS);
                assert!((a_lon0 - e_lon0).abs() < EPS);
                assert!((a_e - e_e).abs() < EPS);
                assert!((a_n - e_n).abs() < EPS);
            }
            (a, e) => panic!("CRS variant mismatch: {a:?} vs {e:?}"),
        }
    }

    #[test]
    fn longlat_maps_to_wgs84() {
        let crs = parse("+proj=longlat +ellps=WGS84 +no_defs").unwrap();
        assert_crs_eq(&crs, &Crs::Wgs84);
    }

    /// DMI's Denmark national composite uses *oblique* stereographic
    /// with `+lat_0=56 +lat_ts=56`. Under the general spherical
    /// scale-factor formula, when `lat_ts == lat_0` the scale at the
    /// origin is unity, so `k0 = 1.0`. The naive polar-only formula
    /// `(1 + sin|lat_ts|)/2` would give ~0.914 — a 9% error that
    /// expands the projected grid bbox by ~50% and causes ~100% of
    /// output pixels to land outside the source data when sampling.
    #[test]
    fn dmi_oblique_stereographic_lat_ts_equals_lat_0_gives_k0_one() {
        let crs = parse("+proj=stere +ellps=WGS84 +lat_0=56 +lon_0=10.5666 +lat_ts=56").unwrap();
        match crs {
            Crs::Stereographic { k0, .. } => {
                assert!(
                    (k0 - 1.0).abs() < 1e-9,
                    "DMI oblique stereographic with lat_ts=lat_0 must give k0=1, got {k0}"
                );
            }
            other => panic!("expected Stereographic, got {other:?}"),
        }
    }

    /// FMI/OPERA polar stereographic composite — the canonical Phase 1
    /// shape. `+lat_ts=60` converts to the **ellipsoidal** k0 such
    /// that `Crs::Stereographic`'s case-1 forward formula matches
    /// PROJ's case-2 forward formula (`+proj=stere +lat_ts=…` uses
    /// Snyder eq. 21-39). The spherical shortcut `(1 + sin|lat_ts|)/2`
    /// is ~0.012% off and produces ~200 m error on a 3000 km radius;
    /// the absolute-coord test
    /// `core::geo::stereographic_inverse_absolute_polar` would
    /// regress if this conversion drifted.
    ///
    /// Expected value computed offline:
    ///   m_c = cos(60°)/√(1 - e²·sin²(60°))
    ///   t_c = tan(15°) · ((1+e·sin60°)/(1−e·sin60°))^(e/2)
    ///   D   = √((1+e)^(1+e) · (1−e)^(1−e))
    ///   k0  = m_c · D / (2 · t_c)  ≈  0.9330690717363566
    #[test]
    fn fmi_polar_stereographic_with_lat_ts_converts_to_k0() {
        // On WGS84; the historical FMI `+R=6371228` sphere is
        // `a_sphere_radius_is_that_sphere`.
        let crs =
            parse("+proj=stere +lat_0=90 +lon_0=0 +lat_ts=60 +ellps=WGS84 +x_0=0 +y_0=0 +no_defs")
                .unwrap();

        assert_crs_eq(
            &crs,
            &Crs::Stereographic {
                lat0: 90f64.to_radians(),
                lon0: 0.0,
                k0: 0.933_069_071_736_356_6,
                false_e: 0.0,
                false_n: 0.0,
                figure: Ellipsoid::WGS84,
            },
        );
    }

    /// Single-site oblique stereographic. `+k_0=1` is the explicit
    /// form; no `+lat_ts`. False easting/northing carry through.
    #[test]
    fn oblique_stereographic_with_explicit_k0() {
        let crs = parse("+proj=stere +lat_0=56 +lon_0=10.5667 +k_0=1.0 +x_0=0 +y_0=0 +ellps=WGS84")
            .unwrap();

        assert_crs_eq(
            &crs,
            &Crs::Stereographic {
                lat0: 56f64.to_radians(),
                lon0: 10.5667f64.to_radians(),
                k0: 1.0,
                false_e: 0.0,
                false_n: 0.0,
                figure: Ellipsoid::WGS84,
            },
        );
    }

    /// `+k` (no underscore) is PROJ's historical shorthand for `+k_0`.
    /// We accept either; both forms appear in real-world ODIM files.
    #[test]
    fn stere_accepts_legacy_k_shorthand() {
        let crs = parse("+proj=stere +lat_0=90 +lon_0=0 +k=0.933 +x_0=0 +y_0=0").unwrap();
        assert_crs_eq(
            &crs,
            &Crs::Stereographic {
                lat0: 90f64.to_radians(),
                lon0: 0.0,
                k0: 0.933,
                false_e: 0.0,
                false_n: 0.0,
                figure: Ellipsoid::WGS84,
            },
        );
    }

    /// EPSG:3067 TM35FIN — the projection FMI publishes single-site
    /// radar data in, on its GRS 1980 (#1243).
    #[test]
    fn tmerc_epsg3067_tm35fin() {
        let crs = parse("+proj=tmerc +lat_0=0 +lon_0=27 +k=0.9996 +x_0=500000 +y_0=0 +ellps=GRS80")
            .unwrap();

        assert_crs_eq(
            &crs,
            &Crs::TransverseMercator {
                lat0: 0.0,
                lon0: 27f64.to_radians(),
                k0: 0.9996,
                false_e: 500_000.0,
                false_n: 0.0,
                figure: Ellipsoid::GRS80,
            },
        );
    }

    /// EPSG:3035 ETRS89-LAEA — the OPERA pan-European grid, on
    /// its GRS 1980 (#1243).
    #[test]
    fn laea_epsg3035_etrs89() {
        let crs =
            parse("+proj=laea +lat_0=52 +lon_0=10 +x_0=4321000 +y_0=3210000 +ellps=GRS80").unwrap();

        assert_crs_eq(
            &crs,
            &Crs::LambertAzimuthalEqualArea {
                lat0: 52f64.to_radians(),
                lon0: 10f64.to_radians(),
                false_e: 4_321_000.0,
                false_n: 3_210_000.0,
                figure: Ellipsoid::GRS80,
            },
        );
    }

    /// Missing `+proj=` is the most common malformed-input case: a
    /// truncated PROJ string from a partial HDF5 read. Surface it as
    /// a structured error rather than silently defaulting to lon/lat.
    #[test]
    fn missing_proj_token_is_an_error() {
        let err = parse("+lat_0=90 +lon_0=0").unwrap_err();
        assert_eq!(err, ParseError::MissingProj);
    }

    /// Unsupported `+proj=` should name the offender so an operator
    /// can decide whether to add support or fix the file.
    #[test]
    fn unsupported_proj_lists_the_value() {
        let err = parse("+proj=merc +lat_0=0").unwrap_err();
        assert_eq!(err, ParseError::UnsupportedProj("merc".into()));
    }

    /// A non-numeric `+lat_0` is a real-world failure mode for files
    /// written by buggy producers (e.g. ASCII garbage spliced into the
    /// projdef). Refuse with a structured error.
    #[test]
    fn non_numeric_parameter_is_an_error() {
        let err = parse("+proj=stere +lat_0=ninety +lon_0=0").unwrap_err();
        assert_eq!(
            err,
            ParseError::InvalidNumber {
                param: "lat_0".into(),
                value: "ninety".into(),
            }
        );
    }

    /// Bare flags like `+no_defs` mustn't perturb anything. PROJ
    /// strings shipped by real producers always include at least one
    /// such flag.
    #[test]
    fn bare_flags_are_ignored() {
        let crs = parse("+proj=longlat +no_defs +wktext").unwrap();
        assert_crs_eq(&crs, &Crs::Wgs84);
    }

    /// All four `longlat` aliases PROJ recognises. ODIM v2.x doesn't
    /// distinguish between them so neither should we.
    #[test]
    fn all_longlat_aliases_map_to_wgs84() {
        for proj in ["longlat", "latlong", "lonlat", "latlon"] {
            let projdef = format!("+proj={proj}");
            let crs = parse(&projdef).unwrap();
            assert_crs_eq(&crs, &Crs::Wgs84);
        }
    }

    /// The figure of a parsed projected `Crs`.
    fn figure_of(crs: &Crs) -> Ellipsoid {
        match crs {
            Crs::Stereographic { figure, .. }
            | Crs::TransverseMercator { figure, .. }
            | Crs::LambertAzimuthalEqualArea { figure, .. } => *figure,
            other => panic!("no figure on {other:?}"),
        }
    }

    /// A point and where PROJ maps it.
    type ProjPin = ((f64, f64), (f64, f64));

    /// Pins `projdef`'s forward and inverse to PROJ's: `forward` maps
    /// lon/lat to x/y within 1 mm, `inverse` x/y to lon/lat within 1e-9°.
    #[track_caller]
    fn assert_matches_proj(projdef: &str, forward: &[ProjPin], inverse: &[ProjPin]) {
        let crs = parse(projdef).unwrap();
        for &((lon, lat), (x, y)) in forward {
            let (fx, fy) = crs.forward(lon, lat);
            assert!(
                (fx - x).abs() < 1e-3 && (fy - y).abs() < 1e-3,
                "{projdef}: forward {lon},{lat} = {fx},{fy}, PROJ {x},{y}"
            );
        }
        for &((x, y), (lon, lat)) in inverse {
            let (ilon, ilat) = crs.inverse(x, y).unwrap();
            assert!(
                (ilon - lon).abs() < 1e-9 && (ilat - lat).abs() < 1e-9,
                "{projdef}: inverse {x},{y} = {ilon},{ilat}, PROJ {lon},{lat}"
            );
        }
    }

    /// Every `+ellps=` name projects where PROJ puts it (#1243): a
    /// Transverse Mercator 4° from its meridian moves by metres between
    /// these figures. Forward of 20°E 62°N by `cs2cs -f %.4f
    /// +proj=longlat +ellps=<name> +to +proj=tmerc +lon_0=16 +k=0.9996
    /// +x_0=500000 +ellps=<name>`; inverse of 700000 6900000 by the same
    /// with `-I -f %.9f` (PROJ 9.8.1).
    #[test]
    fn every_named_ellipsoid_matches_proj() {
        #[rustfmt::skip]
        const FORWARD: [(&str, f64, f64); 46] = [
            ("MERIT", 709413.9387, 6880638.1951), ("SGS85", 709413.9059, 6880637.1164),
            ("GRS80", 709413.9383, 6880638.2137), ("IAU76", 709414.0372, 6880641.4315),
            ("airy", 709393.1488, 6880108.0950), ("APL4.9", 709413.9516, 6880637.6116),
            ("NWL9D", 709414.2142, 6880646.2419), ("mod_airy", 709385.8204, 6879867.2973),
            ("andrae", 709376.8502, 6879668.6953), ("danish", 709374.0538, 6879576.8092),
            ("aust_SA", 709414.7067, 6880662.4237), ("GRS67", 709414.7119, 6880662.1876),
            ("GSK2011", 709413.9233, 6880637.6070), ("bessel", 709388.0051, 6879914.5003),
            ("bess_nam", 709390.8521, 6880008.0427), ("clrk66", 709422.3129, 6880436.7564),
            ("clrk80", 709426.5745, 6880353.3144), ("clrk80ign", 709426.5769, 6880353.3498),
            ("CPM", 709276.1136, 6880729.6164), ("delmbr", 709334.5185, 6879851.1986),
            ("engelis", 709413.9082, 6880637.1370), ("evrst30", 709381.0416, 6879920.0251),
            ("evrst48", 709381.9517, 6879949.9278), ("evrst56", 709381.8591, 6879946.8855),
            ("evrst69", 709381.6759, 6879940.8668), ("evrstSS", 709381.7709, 6879943.9868),
            ("fschr60", 709414.8118, 6880673.0637), ("fschr60m", 709414.4506, 6880661.1971),
            ("fschr68", 709414.2865, 6880655.8031), ("helmert", 709415.9281, 6880709.7424),
            ("hough", 709420.6268, 6880676.4496), ("intl", 709424.5012, 6880803.7443),
            ("krass", 709417.4056, 6880758.2878), ("kaula", 709414.8236, 6880664.8264),
            ("lerch", 709414.0044, 6880640.3527), ("mprts", 710352.4234, 6887304.5503),
            ("new_intl", 709414.6253, 6880659.6947), ("plessis", 709342.4997, 6879733.2564),
            ("PZ90", 709413.9043, 6880637.1864), ("SEasia", 709414.4506, 6880661.1971),
            ("walbeck", 709365.0017, 6879670.7342), ("WGS60", 709414.7790, 6880671.9849),
            ("WGS66", 709414.2142, 6880646.2419), ("WGS72", 709413.8675, 6880636.2876),
            ("WGS84", 709413.9383, 6880638.2138), ("sphere", 708632.8960, 6897758.5464),
        ];
        #[rustfmt::skip]
        const INVERSE: [(&str, f64, f64); 8] = [
            ("WGS84", 19.842565710, 62.178500106), ("GRS80", 19.842565710, 62.178500108),
            ("intl", 19.842182923, 62.177015390), ("bessel", 19.843867240, 62.184990074),
            ("clrk66", 19.842640883, 62.180304600), ("clrk80", 19.842657426, 62.181051879),
            ("krass", 19.842365231, 62.177423575), ("airy", 19.843552013, 62.183254088),
        ];
        assert_eq!(
            FORWARD.map(|(name, ..)| name),
            NAMED_ELLIPSOIDS.map(|(name, ..)| name)
        );
        let projdef =
            |name: &str| format!("+proj=tmerc +lon_0=16 +k=0.9996 +x_0=500000 +ellps={name}");
        for (name, x, y) in FORWARD {
            assert_matches_proj(&projdef(name), &[((20.0, 62.0), (x, y))], &[]);
        }
        for (name, lon, lat) in INVERSE {
            assert_matches_proj(
                &projdef(name),
                &[],
                &[((700_000.0, 6_900_000.0), (lon, lat))],
            );
        }
        let figure = |name: &str| figure_of(&parse(&projdef(name)).unwrap());
        assert_eq!(figure("WGS84"), Ellipsoid::WGS84);
        assert_eq!(figure("GRS80"), Ellipsoid::GRS80);
        assert_eq!(figure("sphere"), Ellipsoid::sphere(6_370_997.0));
        assert_eq!(
            figure("intl"),
            Ellipsoid::from_inverse_flattening(6_378_388.0, 297.0)
        );
    }

    /// A `+R=` sphere is that sphere (#1243), in every projection: FMI's
    /// historical `+R=6371228` composites project where PROJ puts them,
    /// polar stereographic's `+lat_ts` true on the sphere. `cs2cs -f %.4f
    /// +proj=longlat +R=6371228 +to <projdef>`, inverse with `-I -f %.9f`
    /// (PROJ 9.8.1). `+R` overrules any other figure parameter, as in
    /// PROJ, and a radius equal to WGS 84's semi-major axis is a sphere
    /// too, not WGS 84.
    #[test]
    fn a_sphere_radius_is_that_sphere() {
        assert_matches_proj(
            "+proj=stere +lat_0=90 +lon_0=25 +lat_ts=60 +R=6371228",
            &[
                ((25.0, 60.0), (0.0, -3_185_614.0)),
                ((10.0, 55.0), (-970_195.446_4, -3_620_818.699_4)),
            ],
            &[
                ((500_000.0, -3_000_000.0), (34.462_322_208, 61.300_959_728)),
                (
                    (-1_000_000.0, -2_500_000.0),
                    (3.198_590_514, 64.477_949_040),
                ),
            ],
        );
        assert_matches_proj(
            "+proj=tmerc +lat_0=0 +lon_0=27 +k=1 +x_0=3500000 +R=6371228",
            &[
                ((25.0, 60.0), (3_388_812.388_5, 6_673_615.215_9)),
                ((30.0, 68.0), (3_624_926.444_7, 7_564_558.911_8)),
            ],
            &[
                ((3_400_000.0, 6_700_000.0), (25.188_105_018, 60.240_045_798)),
                ((3_600_000.0, 7_600_000.0), (29.435_725_489, 68.328_228_849)),
            ],
        );
        assert_matches_proj(
            "+proj=laea +lat_0=52 +lon_0=10 +x_0=4321000 +y_0=3210000 +R=6371228",
            &[
                ((25.0, 60.0), (5_149_697.491_3, 4_187_192.394_8)),
                ((-5.0, 40.0), (3_045_647.393_4, 2_004_916.830_1)),
            ],
            &[
                ((5_000_000.0, 4_000_000.0), (21.763_520_409, 58.601_164_131)),
                ((3_000_000.0, 2_000_000.0), (-5.514_322_510, 39.873_803_196)),
            ],
        );
        for projdef in [
            "+proj=stere +lat_0=90 +lon_0=25 +lat_ts=60 +R=6371228",
            "+proj=stere +lat_0=90 +lon_0=25 +lat_ts=60 +R=6371228 +ellps=intl +a=6378388 +rf=297",
        ] {
            let crs = parse(projdef).unwrap();
            assert_eq!(figure_of(&crs), Ellipsoid::sphere(6_371_228.0), "{projdef}");
            match crs {
                Crs::Stereographic { k0, .. } => {
                    let spherical = (1.0 + 60.0_f64.to_radians().sin()) / 2.0;
                    assert!((k0 - spherical).abs() < 1e-15, "{projdef}: k0 {k0}");
                }
                other => panic!("expected Stereographic, got {other:?}"),
            }
        }
        let crs = parse("+proj=stere +lat_0=56 +lon_0=10.5666 +k=1 +R=6378137").unwrap(); // nogeocheck — PROJ-string fixture, the value under test
        assert_eq!(figure_of(&crs), Ellipsoid::sphere(6_378_137.0)); // nogeocheck — the radius under test
    }

    /// SMHI's Swedish composite states Bessel 1841 (#1243): it projects
    /// where PROJ puts it (`cs2cs -f %.4f +proj=longlat +ellps=bessel +to
    /// <projdef>`, inverse with `-I -f %.9f`, PROJ 9.8.1), and the grid it
    /// anchors at its upper-left corner, 458 × 881 cells of 2 km, ends
    /// where the file says. Its lower-right corner attribute, 22.7617°E
    /// 53.7065°N, lies 22 m from the grid's on Bessel and 209 m from it on
    /// WGS84, which drew the composite until now.
    #[test]
    fn smhi_bessel_composite_lands_on_its_corners() {
        const SMHI: &str =
            "+proj=stere +ellps=bessel +lat_0=90 +lon_0=14 +lat_ts=60 +towgs84=0,0,0";
        assert_matches_proj(
            SMHI,
            &[
                ((14.0, 60.0), (0.0, -3_196_709.626_7)),
                ((25.0, 66.0), (484_019.181_9, -2_490_062.825_9)),
                ((5.32396, 69.8074), (-320_603.021_1, -2_101_025.930_1)),
            ],
            &[((500_000.0, -3_000_000.0), (23.462_322_208, 61.398_677_872))],
        );
        assert_eq!(
            figure_of(&parse(SMHI).unwrap()),
            Ellipsoid::from_inverse_flattening(6_377_397.155, 299.152_812_8)
        );
        let lower_right_miss = |projdef: &str| {
            let crs = parse(projdef).unwrap();
            let (x, y) = crs.forward(5.32396, 69.8074);
            let (lon, lat) = crs.inverse(x + 458.0 * 2000.0, y - 881.0 * 2000.0).unwrap();
            ds_core::geo::great_circle_distance_m(lon, lat, 22.7617, 53.7065)
        };
        assert!(lower_right_miss(SMHI) < 25.0);
        assert!(lower_right_miss(&SMHI.replace("bessel", "WGS84")) > 200.0);
    }

    /// `+a=` with a shape is that ellipsoid, `+a=` alone a sphere, as in
    /// PROJ (#1243). KNMI's composite states `+a=6378137 +b=6356752`, a
    /// semi-minor axis 0.3 m short of WGS 84's, pinned to `cs2cs -f %.4f
    /// +proj=longlat +a=6378137 +b=6356752 +to <projdef>` (inverse `-I -f
    /// %.9f`, PROJ 9.8.1); DWD's `+b=6356752.3142451802` is WGS 84's.
    #[test]
    fn axes_state_their_ellipsoid() {
        assert_matches_proj(
            "+proj=stere +lat_0=90 +lon_0=0 +lat_ts=60 +a=6378137 +b=6356752", // nogeocheck — KNMI's projdef
            &[
                ((5.0, 52.0), (357_885.749_6, -4_090_652.836_1)),
                ((10.0, 56.0), (633_294.336_7, -3_591_590.658_4)),
            ],
            &[
                ((0.0, -4_000_000.0), (0.0, 52.917_739_326)),
                ((500_000.0, -3_500_000.0), (8.130_102_354, 56.983_748_263)),
            ],
        );
        let figure = |axes: &str| {
            figure_of(&parse(&format!("+proj=stere +lat_0=90 +lat_ts=60 {axes}")).unwrap())
        };
        let dwd = "+a=6378137 +b=6356752.3142451802 +no_defs"; // nogeocheck — DWD's projdef
        assert_eq!(figure(dwd), Ellipsoid::WGS84);
        let grs80 = format!("+a={} +rf=298.257222101", ds_core::geo::WGS84_A);
        assert_eq!(figure(&grs80), Ellipsoid::GRS80);
        let intl = Ellipsoid::from_inverse_flattening(6_378_388.0, 297.0);
        assert_eq!(figure("+a=6378388 +rf=297"), intl);
        // The other shapes PROJ reads, each International 1924; a `+rf`
        // comes first, as in PROJ, so the `+b` beside it is ignored.
        for shape in [
            "+f=0.003367003367003367",
            "+es=0.006722670022333322",
            "+e=0.08199188997902977",
            "+b=6356911.946127946",
            "+rf=297 +b=6356000",
        ] {
            let f = figure(&format!("+a=6378388 {shape}"));
            assert_eq!(f.semi_major(), 6_378_388.0, "{shape}");
            assert!((f.flattening() - 1.0 / 297.0).abs() < 1e-15, "{shape}");
        }
        for sphere in [
            "+a=6371000",
            "+a=6371000 +b=6371000",
            "+a=6371000 +f=0",
            "+a=6371000 +es=0",
        ] {
            assert_eq!(figure(sphere), Ellipsoid::sphere(6_371_000.0), "{sphere}");
        }
        // No figure at all is WGS 84, PROJ's own default for a CRS.
        assert_eq!(figure(""), Ellipsoid::WGS84);
    }

    /// `+datum=` names its ellipsoid as PROJ builds the CRS (#1243): the
    /// OSGB36 British National Grid on Airy 1830, and the Irish Grid on
    /// EPSG's Airy Modified 1849, not `mod_airy`. `cs2cs -f %.4f
    /// +proj=longlat +datum=<datum> +to <projdef>`, inverse with `-I -f
    /// %.9f` (PROJ 9.8.1). Only the figure is taken, not the datum shift.
    #[test]
    fn datums_state_their_ellipsoid() {
        assert_matches_proj(
            "+proj=tmerc +lat_0=49 +lon_0=-2 +k=0.9996012717 +x_0=400000 +y_0=-100000 +datum=OSGB36",
            &[
                ((-1.0, 52.0), (468_643.108_1, 234_025.789_9)),
                ((-4.0, 57.0), (278_526.066_7, 791_630.595_8)),
            ],
            &[
                ((500_000.0, 300_000.0), (-0.523_737_117, 52.588_236_349)),
                ((300_000.0, 800_000.0), (-3.649_956_741, 57.080_309_898)),
            ],
        );
        assert_matches_proj(
            "+proj=tmerc +lat_0=53.5 +lon_0=-8 +k=1.000035 +x_0=200000 +y_0=250000 +datum=ire65",
            &[
                ((-7.0, 53.0), (267_129.739_5, 194_827.342_5)),
                ((-9.0, 55.0), (136_013.248_9, 417_406.919_9)),
            ],
            &[
                ((250_000.0, 200_000.0), (-7.254_344_906, 53.048_352_582)),
                ((100_000.0, 400_000.0), (-9.556_586_154, 54.837_752_330)),
            ],
        );
        // A Clarke 1866 azimuthal equal-area, the figure of NAD27.
        assert_matches_proj(
            "+proj=laea +lat_0=45 +lon_0=-100 +datum=NAD27",
            &[
                ((-90.0, 40.0), (852_122.908_0, -503_875.772_1)),
                ((-110.0, 50.0), (-715_341.166_3, 600_768.833_4)),
            ],
            &[
                ((500_000.0, -300_000.0), (-93.947_253_149, 42.134_012_016)),
                ((-800_000.0, 600_000.0), (-111.168_169_203, 49.891_995_431)),
            ],
        );
        let figure = |names: &str| figure_of(&parse(&format!("+proj=laea {names}")).unwrap());
        assert_eq!(figure("+datum=NAD27"), figure("+ellps=clrk66"));
        assert_eq!(figure("+datum=potsdam"), figure("+ellps=bessel"));
        assert_eq!(figure("+datum=NAD83"), Ellipsoid::GRS80);
        assert_eq!(figure("+datum=WGS84 +ellps=WGS84"), Ellipsoid::WGS84);
        assert_eq!(
            figure("+datum=potsdam +ellps=bessel"),
            figure("+ellps=bessel")
        );
    }

    /// A figure the parser cannot honour is an error, never WGS 84
    /// (#1243): an unknown name (names are case-sensitive, as in PROJ), a
    /// shape without its axis, a figure that is not a sphere or an oblate
    /// ellipsoid, a spherification, a name beside axes, or two names that
    /// disagree.
    #[test]
    fn a_figure_it_cannot_honour_is_an_error() {
        let parse_figure = |figure: &str| parse(&format!("+proj=stere +lat_0=90 {figure}"));
        assert_eq!(
            parse_figure("+ellps=foo"),
            Err(ParseError::UnknownEllipsoid("foo".into()))
        );
        assert_eq!(
            parse_figure("+ellps=wgs84"),
            Err(ParseError::UnknownEllipsoid("wgs84".into()))
        );
        assert_eq!(
            parse_figure("+datum=foo"),
            Err(ParseError::UnknownDatum("foo".into()))
        );
        for figure in [
            "+rf=297",
            "+b=6356000",
            "+R=0",
            "+R=-6371228",
            "+a=-6378388 +rf=297",
            "+a=6378388 +b=6400000",
            "+a=6378388 +rf=0",
            "+a=6378388 +rf=1",
            "+a=6378388 +f=1",
            "+a=6378388 +es=1",
            "+ellps=GRS80 +R_A",
            "+ellps=intl +b=6356000",
            "+ellps=intl +a=6378000",
            "+datum=WGS84 +rf=297",
            "+ellps=airy +datum=potsdam",
        ] {
            assert!(
                matches!(parse_figure(figure), Err(ParseError::UnsupportedFigure(_))),
                "{figure}: {:?}",
                parse_figure(figure)
            );
        }
        // Every projected `+proj=` checks it; `longlat` needs no figure.
        for proj in ["tmerc", "laea"] {
            assert_eq!(
                parse(&format!("+proj={proj} +ellps=foo")),
                Err(ParseError::UnknownEllipsoid("foo".into()))
            );
        }
        assert_eq!(parse("+proj=longlat +ellps=foo"), Ok(Crs::Wgs84));
        assert_eq!(
            parse_figure("+a=6378388 +rf=abc"),
            Err(ParseError::InvalidNumber {
                param: "rf".into(),
                value: "abc".into()
            })
        );
    }

    /// Defaults: stere with no parameters except `+proj=stere` should
    /// produce a CRS with `lat0=0, lon0=0, k0=1.0, false_e=false_n=0`.
    /// PROJ has the same defaults; surprising a downstream caller with
    /// a different-flavoured "default" would mask bugs.
    #[test]
    fn stere_defaults_match_proj_defaults() {
        let crs = parse("+proj=stere").unwrap();
        assert_crs_eq(
            &crs,
            &Crs::Stereographic {
                lat0: 0.0,
                lon0: 0.0,
                k0: 1.0,
                false_e: 0.0,
                false_n: 0.0,
                figure: Ellipsoid::WGS84,
            },
        );
    }
}
