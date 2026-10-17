//! CF conventions metadata shared by NetCDF and Zarr readers.
//!
//! - Time coordinates: `<unit> since <reference>` units decoded to UTC
//!   instants ([`parse_time_units`], [`decode_times`]).
//! - The role of each of a variable's dimensions ([`classify_axis`]).
//! - A CF grid-mapping variable (CF 1.11 Appendix F) resolved to a [`Crs`]
//!   ([`crs_from_grid_mapping`]), the unit of a coordinate variable
//!   ([`coordinate_scale`]), and a field's regular 1-D x/y coordinates
//!   resolved to a north-up [`GeoTransform`] ([`grid`]).
//! - A pressure vertical coordinate, known by its units, and its levels in
//!   hPa ([`PressureUnit`]).
//!
//! Readers own attribute and coordinate access and pass plain values or a
//! lookup closure, so this module stays independent of any NetCDF or Zarr
//! library.

use crate::geo::{Crs, GeoTransform, SweepAxis, WGS84_A, WGS84_F};

mod axis;
mod time;

pub use axis::{classify_axis, AxisRole};
pub use time::{decode_times, is_standard_calendar, parse_duration_seconds, parse_time_units};

/// A grid-mapping attribute value as a reader found it.
#[derive(Debug, Clone, PartialEq)]
pub enum CfAttr {
    Number(f64),
    /// A numeric attribute of more than one element, such as a secant
    /// cone's two `standard_parallel`s. A parameter that takes one number
    /// rejects it rather than reading its first element.
    Numbers(Vec<f64>),
    Text(String),
}

/// Resolve a CF grid mapping to a [`Crs`].
///
/// `attr(name)` returns attribute `name` of the grid-mapping variable.
/// Supported `grid_mapping_name`s: `latitude_longitude`, `geostationary`
/// and `lambert_conformal_conic`. Any other mapping, a missing required
/// parameter, or one this server cannot honour is an error rather than a
/// guess: a wrong earth figure alone moves a geostationary pixel by
/// kilometres. Not honoured: a non-zero geostationary false easting or
/// equatorial origin, a conic `scale_factor_at_projection_origin` other
/// than 1, and a cone on an ellipsoid other than WGS84.
pub fn crs_from_grid_mapping(attr: impl Fn(&str) -> Option<CfAttr>) -> Result<Crs, String> {
    let text = |name: &str| match attr(name) {
        Some(CfAttr::Text(value)) => Ok(Some(value)),
        Some(_) => Err(format!("grid mapping '{name}' must be text")),
        None => Ok(None),
    };
    let numbers = |name: &str| {
        let values = match attr(name) {
            Some(CfAttr::Number(value)) => vec![value],
            Some(CfAttr::Numbers(values)) => values,
            Some(CfAttr::Text(_)) => return Err(format!("grid mapping '{name}' must be numeric")),
            None => return Ok(None),
        };
        if values.is_empty() || values.iter().any(|v| !v.is_finite()) {
            return Err(format!("grid mapping '{name}' must be finite numbers"));
        }
        Ok(Some(values))
    };
    let number = |name: &str| match numbers(name)?.as_deref() {
        None => Ok(None),
        Some(&[value]) => Ok(Some(value)),
        Some(values) => Err(format!(
            "grid mapping '{name}' must be one number, got {} values",
            values.len()
        )),
    };
    let name = text("grid_mapping_name")?.ok_or("grid mapping has no grid_mapping_name")?;
    let mapping = name.trim();
    let required =
        |name: &str| number(name)?.ok_or_else(|| format!("{mapping} grid mapping needs '{name}'"));
    match mapping {
        "latitude_longitude" => Ok(Crs::Wgs84),
        "geostationary" => {
            let zero_or_absent = |name: &str| match number(name)? {
                Some(value) if value != 0.0 => Err(format!(
                    "geostationary grid mapping with non-zero '{name}' ({value}) is not supported"
                )),
                _ => Ok(()),
            };
            zero_or_absent("latitude_of_projection_origin")?;
            zero_or_absent("false_easting")?;
            zero_or_absent("false_northing")?;
            let lon0 = required("longitude_of_projection_origin")?;
            let height = required("perspective_point_height")?;
            if height <= 0.0 {
                return Err(format!(
                    "perspective_point_height must be positive, got {height}"
                ));
            }
            let (semi_major, semi_minor) = earth_figure(&number)?;
            Ok(Crs::Geostationary {
                lon0: lon0.to_radians(),
                height,
                semi_major,
                semi_minor,
                sweep: sweep_axis(text("sweep_angle_axis")?, text("fixed_angle_axis")?)?,
            })
        }
        "lambert_conformal_conic" => {
            let parallels = numbers("standard_parallel")?
                .ok_or("lambert_conformal_conic grid mapping needs 'standard_parallel'")?;
            let (lat1, lat2) = match parallels[..] {
                [tangent] => (tangent, tangent),
                [first, second] => (first, second),
                _ => {
                    return Err(format!(
                        "lambert_conformal_conic takes one or two standard_parallel values, \
                         got {}",
                        parallels.len()
                    ))
                }
            };
            if lat1.abs() >= 90.0 || lat2.abs() >= 90.0 {
                return Err(format!(
                    "standard_parallel {lat1}, {lat2} must lie between the poles"
                ));
            }
            // The cone constant is zero: the cone opens into a cylinder.
            if (lat1 + lat2).abs() < 1e-9 {
                return Err(format!(
                    "standard_parallel {lat1}, {lat2} is symmetric about the equator: a \
                     Mercator, not a cone"
                ));
            }
            let lat0 = required("latitude_of_projection_origin")?;
            if lat0.abs() > 90.0 {
                return Err(format!(
                    "latitude_of_projection_origin {lat0} is not a latitude"
                ));
            }
            let lon0 = required("longitude_of_central_meridian")?;
            // CF gives this mapping no scale factor, and the cone has none
            // to scale: a one-parallel producer's k0 below 1 would be lost.
            if let Some(k0) = number("scale_factor_at_projection_origin")? {
                if k0 != 1.0 {
                    return Err(format!(
                        "lambert_conformal_conic with scale_factor_at_projection_origin {k0} \
                         is not supported, only 1"
                    ));
                }
            }
            let radius = match earth_figure(&number)? {
                (a, b) if a == b => Some(a),
                (a, b) if is_wgs84(a, b) => None,
                (a, b) => {
                    return Err(format!(
                        "lambert_conformal_conic on the ellipsoid a={a}, b={b} is not \
                         supported, only a sphere or WGS84"
                    ))
                }
            };
            Ok(Crs::LambertConformalConic {
                lat1: lat1.to_radians(),
                lat2: lat2.to_radians(),
                lat0: lat0.to_radians(),
                lon0: lon0.to_radians(),
                false_e: number("false_easting")?.unwrap_or(0.0),
                false_n: number("false_northing")?.unwrap_or(0.0),
                radius,
            })
        }
        other => Err(format!("grid mapping '{other}' is not supported")),
    }
}

/// `(semi_major, semi_minor)` in metres from the CF earth-figure
/// attributes: `earth_radius` (a sphere), or `semi_major_axis` with
/// `semi_minor_axis` or `inverse_flattening` (0 meaning a sphere), or
/// `semi_major_axis` alone (a sphere).
fn earth_figure(
    number: &impl Fn(&str) -> Result<Option<f64>, String>,
) -> Result<(f64, f64), String> {
    let positive = |name: &str, value: f64| {
        if value > 0.0 {
            Ok(value)
        } else {
            Err(format!("'{name}' must be positive, got {value}"))
        }
    };
    if let Some(radius) = number("earth_radius")? {
        let radius = positive("earth_radius", radius)?;
        return Ok((radius, radius));
    }
    let a = number("semi_major_axis")?
        .ok_or("grid mapping gives no earth figure (earth_radius or semi_major_axis)")?;
    let a = positive("semi_major_axis", a)?;
    if let Some(b) = number("semi_minor_axis")? {
        return Ok((a, positive("semi_minor_axis", b)?));
    }
    match number("inverse_flattening")? {
        Some(f) if f != 0.0 => Ok((a, a * (1.0 - 1.0 / positive("inverse_flattening", f)?))),
        _ => Ok((a, a)),
    }
}

/// Whether an ellipsoid is WGS84 to within a millimetre in both axes.
/// GRS80, 0.1 mm off in the semi-minor axis, passes.
fn is_wgs84(semi_major: f64, semi_minor: f64) -> bool {
    (semi_major - WGS84_A).abs() < 1e-3 && (semi_minor - WGS84_A * (1.0 - WGS84_F)).abs() < 1e-3
}

/// The sweep axis from CF's `sweep_angle_axis` or its complement
/// `fixed_angle_axis`; one is required and they must agree.
fn sweep_axis(sweep: Option<String>, fixed: Option<String>) -> Result<SweepAxis, String> {
    let axis = |value: &str| match value.trim() {
        "x" => Ok(SweepAxis::X),
        "y" => Ok(SweepAxis::Y),
        other => Err(format!("unknown geostationary axis '{other}'")),
    };
    let other = |axis: SweepAxis| match axis {
        SweepAxis::X => SweepAxis::Y,
        SweepAxis::Y => SweepAxis::X,
    };
    match (sweep, fixed) {
        (Some(sweep), None) => axis(&sweep),
        (None, Some(fixed)) => Ok(other(axis(&fixed)?)),
        (Some(sweep), Some(fixed)) => {
            let sweep = axis(&sweep)?;
            if sweep == other(axis(&fixed)?) {
                Ok(sweep)
            } else {
                Err("sweep_angle_axis and fixed_angle_axis name the same axis".to_string())
            }
        }
        (None, None) => Err("geostationary grid mapping needs sweep_angle_axis".to_string()),
    }
}

/// `crs`'s own coordinate units per unit of an x/y coordinate variable, or
/// `None` for a unit it cannot be in. A projected coordinate is metres. A
/// geostationary coordinate is the scan angle — radians (`rad`, GOES-R) or
/// microradians (Himawari ISatSS) — and the projection works in the angle
/// times the satellite height. A latitude/longitude coordinate is degrees.
pub fn coordinate_scale(units: &str, crs: &Crs) -> Option<f64> {
    let units = units.trim();
    let metres = matches!(units, "m" | "meter" | "meters" | "metre" | "metres");
    match crs {
        Crs::Geostationary { height, .. } => match units {
            "rad" | "radian" | "radians" => Some(*height),
            "microradian" | "microradians" | "urad" | "µrad" => Some(height * 1e-6),
            _ if metres => Some(1.0),
            _ => None,
        },
        Crs::Wgs84 => is_degrees(units).then_some(1.0),
        Crs::RotatedLatLon { .. } => None,
        _ => metres.then_some(1.0),
    }
}

/// CF's spellings of degrees of latitude or longitude: `degrees`,
/// `degrees_east`, `degree_N`, `degreesE`, …
fn is_degrees(units: &str) -> bool {
    let lower = units.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("degrees")
        .or_else(|| lower.strip_prefix("degree"));
    matches!(
        rest,
        Some("" | "_east" | "_north" | "_e" | "_n" | "e" | "n")
    )
}

/// A regular 1-D coordinate axis: `first + i * step` at index `i` in
/// `0..len`, in the coordinate variable's `units`.
#[derive(Debug, Clone, PartialEq)]
pub struct RegularAxis {
    /// The value at index 0.
    pub first: f64,
    /// The spacing; negative when the axis descends.
    pub step: f64,
    pub len: u32,
    /// The coordinate variable's `units`, empty when it has none.
    pub units: String,
}

impl RegularAxis {
    /// The axis that coordinate variable `name`'s `values` (CF packing
    /// applied) describe. The origin and step come from the first and last
    /// values and the count, never from adjacent differences, and every
    /// value must lie within the larger of a thousandth of a step and the
    /// f32 allowance of `first + i * step`. Fewer than two values, a zero
    /// step or an irregular axis is an error.
    ///
    /// Float32 coordinates (MEPS's x/y) are rounded to f32 and can carry
    /// their producer's arithmetic too: x[100] is −810084.06, not −810084.
    /// The allowance is two f32 epsilons of the axis's magnitude, about
    /// 0.3 m at 1.3e6 m and 4e-5° at 180°, capped at a tenth of a step. A
    /// thousandth of a step alone covers MEPS's 2500 m, but not a 0.01° f32
    /// longitude near ±180, whose ulp is 1.5e-5°.
    pub fn from_values(name: &str, values: &[f64], units: &str) -> Result<Self, String> {
        let len =
            u32::try_from(values.len()).map_err(|_| format!("coordinate '{name}' is too long"))?;
        if len < 2 {
            return Err(format!("coordinate '{name}' has fewer than two values"));
        }
        let (first, last) = (values[0], values[values.len() - 1]);
        let step = (last - first) / f64::from(len - 1);
        let f32_noise =
            (2.0 * f64::from(f32::EPSILON) * first.abs().max(last.abs())).min(step.abs() * 0.1);
        let tolerance = (step.abs() * 1e-3).max(f32_noise);
        let regular = step != 0.0
            && step.is_finite()
            && values
                .iter()
                .enumerate()
                .all(|(i, v)| (v - (first + i as f64 * step)).abs() <= tolerance);
        if !regular {
            return Err(format!("coordinate '{name}' is not a regular axis"));
        }
        Ok(Self {
            first,
            step,
            len,
            units: units.to_string(),
        })
    }
}

/// A field's grid, resolved from its CF coordinates by [`grid`].
#[derive(Debug, Clone)]
pub struct CfGrid {
    /// North-up: row 0 is the northernmost.
    pub transform: GeoTransform,
    /// The stored rows run south to north (`y` ascends, so row 0 is the
    /// southern edge, as in MEPS). `transform` starts at the last stored
    /// row, and a reader reverses the rows once when it decodes a field.
    pub flipped: bool,
}

/// The grid of a field on regular 1-D `x`/`y` coordinates in `crs`: the
/// CRS from [`crs_from_grid_mapping`] for a variable with a `grid_mapping`,
/// or [`Crs::Wgs84`] for latitude/longitude axes. Each axis's units go
/// through [`coordinate_scale`]. `x` must ascend; a descending `y` is
/// north-up as stored, and an ascending one is flipped.
pub fn grid(crs: Crs, x: &RegularAxis, y: &RegularAxis) -> Result<CfGrid, String> {
    let scale = |axis: &RegularAxis, name: &str| {
        coordinate_scale(&axis.units, &crs)
            .ok_or_else(|| format!("{name} coordinate has unusable units '{}'", axis.units))
    };
    let (sx, sy) = (scale(x, "x")?, scale(y, "y")?);
    let flipped = y.step > 0.0;
    let (y0, dy) = if flipped {
        (
            y.first + f64::from(y.len.saturating_sub(1)) * y.step,
            -y.step,
        )
    } else {
        (y.first, y.step)
    };
    let transform = GeoTransform::from_cell_centres(
        x.first * sx,
        x.step * sx,
        y0 * sy,
        dy * sy,
        x.len,
        y.len,
        crs,
    )?;
    Ok(CfGrid { transform, flipped })
}

/// The unit of a CF pressure coordinate. CF 1.11 §4.3 identifies a
/// vertical coordinate by units of pressure alone (`positive` is then
/// optional), so these units make an axis a pressure axis, unless it is a
/// parametric coordinate (§4.3.3: `formula_terms`, a hybrid
/// `standard_name`), which may carry pressure units too and which a reader
/// rules out first. Served levels are in hPa
/// ([`crate::vertical::VerticalKind::Pressure`]): MEPS stores hPa, ERA5
/// and many CF writers Pa.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureUnit {
    Pa,
    HPa,
    KPa,
    Bar,
}

impl PressureUnit {
    /// The pressure unit `units` spells, or `None` for any other unit.
    /// UDUNITS symbols are case-sensitive (`Pa`, `hPa`, `kPa`, `mbar`,
    /// `bar`); `mb` is the meteorological millibar, and the spelled-out
    /// names are matched in any case.
    pub fn parse(units: &str) -> Option<Self> {
        let units = units.trim();
        match units {
            "Pa" => return Some(Self::Pa),
            "hPa" | "mbar" | "mb" => return Some(Self::HPa),
            "kPa" => return Some(Self::KPa),
            "bar" => return Some(Self::Bar),
            _ => {}
        }
        match units.to_ascii_lowercase().as_str() {
            "pascal" | "pascals" => Some(Self::Pa),
            "hectopascal" | "hectopascals" | "millibar" | "millibars" => Some(Self::HPa),
            "kilopascal" | "kilopascals" => Some(Self::KPa),
            "bars" => Some(Self::Bar),
            _ => None,
        }
    }

    /// `value`, in this unit, in hPa, to six significant digits. Pa is
    /// divided by 100 rather than multiplied by 0.01, so 85000 Pa is
    /// exactly 850 hPa; the rounding drops what a `float` coordinate adds
    /// in `f64`, so a level is the number a client types: 0.85 bar stored
    /// as `float` is 850 hPa, not 850.0000238, and 0.1 hPa is 0.1, not
    /// 0.10000000149.
    pub fn to_hpa(self, value: f64) -> f64 {
        let hpa = match self {
            Self::Pa => value / 100.0,
            Self::HPa => value,
            Self::KPa => value * 10.0,
            Self::Bar => value * 1000.0,
        };
        round_significant(hpa, LEVEL_DIGITS)
    }
}

/// Significant decimal digits a pressure level keeps
/// ([`PressureUnit::to_hpa`]): the digits a `float` holds exactly (C's
/// `FLT_DIG`), 0.001 hPa at 1000 hPa.
const LEVEL_DIGITS: i32 = 6;

/// `value` rounded to `digits` significant decimal digits; zero and
/// non-finite values as they are.
fn round_significant(value: f64, digits: i32) -> f64 {
    if value == 0.0 || !value.is_finite() {
        return value;
    }
    let shift = digits - 1 - value.abs().log10().floor() as i32;
    if shift >= 0 {
        let scale = 10f64.powi(shift);
        (value * scale).round() / scale
    } else {
        let scale = 10f64.powi(-shift);
        (value / scale).round() * scale
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pressure axis is known by its units, in any CF spelling, and its
    /// levels convert to hPa exactly: 85000 Pa is 850 hPa, not 850.0000001,
    /// and a `float` coordinate's level is its decimal value.
    #[test]
    fn pressure_units_convert_to_hpa_exactly() {
        for (units, unit) in [
            ("Pa", PressureUnit::Pa),
            (" Pa ", PressureUnit::Pa),
            ("pascals", PressureUnit::Pa),
            ("hPa", PressureUnit::HPa),
            ("mbar", PressureUnit::HPa),
            ("mb", PressureUnit::HPa),
            ("millibar", PressureUnit::HPa),
            ("kPa", PressureUnit::KPa),
            ("bar", PressureUnit::Bar),
        ] {
            assert_eq!(PressureUnit::parse(units), Some(unit), "{units}");
        }
        for units in ["m", "K", "1", "Pa s-1", "PA", "hpa", "level", ""] {
            assert_eq!(PressureUnit::parse(units), None, "{units}");
        }
        for (pa, hpa) in [
            (100_000.0, 1000.0),
            (92_500.0, 925.0),
            (85_000.0, 850.0),
            (70_000.0, 700.0),
            (101_325.0, 1013.25),
            (5_000.0, 50.0),
        ] {
            assert_eq!(PressureUnit::Pa.to_hpa(pa), hpa);
        }
        assert_eq!(PressureUnit::HPa.to_hpa(850.0), 850.0);
        assert_eq!(PressureUnit::KPa.to_hpa(85.0), 850.0);
        assert_eq!(PressureUnit::Bar.to_hpa(0.85), 850.0);
        // `float` coordinates, widened to f64 as the reader gives them.
        let float = |v: f32| f64::from(v);
        assert_ne!(float(0.85) * 1000.0, 850.0, "what the rounding is for");
        for (unit, stored, hpa) in [
            (PressureUnit::Bar, 0.85, 850.0),
            (PressureUnit::Bar, 0.925, 925.0),
            (PressureUnit::KPa, 0.1, 1.0),
            (PressureUnit::KPa, 92.5, 925.0),
            (PressureUnit::HPa, 0.1, 0.1),
            (PressureUnit::HPa, 0.01, 0.01),
            (PressureUnit::HPa, 1013.25, 1013.25),
            (PressureUnit::Pa, 1.0, 0.01),
            (PressureUnit::Pa, 85_000.0, 850.0),
        ] {
            assert_eq!(unit.to_hpa(float(stored)), hpa, "{stored} {unit:?}");
        }
    }

    fn attrs<'a>(pairs: &'a [(&'a str, CfAttr)]) -> impl Fn(&str) -> Option<CfAttr> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, v)| v.clone())
        }
    }

    fn n(value: f64) -> CfAttr {
        CfAttr::Number(value)
    }

    fn ns(values: &[f64]) -> CfAttr {
        CfAttr::Numbers(values.to_vec())
    }

    fn t(value: &str) -> CfAttr {
        CfAttr::Text(value.to_string())
    }

    /// `goes_imager_projection` from a GOES-19 ABI L2 file, as `ncdump -h`
    /// prints it.
    fn goes_imager_projection() -> Vec<(&'static str, CfAttr)> {
        vec![
            ("perspective_point_height", n(35786023.0)),
            ("semi_major_axis", n(6378137.0)),
            ("semi_minor_axis", n(6356752.31414)),
            ("inverse_flattening", n(298.2572221)),
            ("latitude_of_projection_origin", n(0.0)),
            ("longitude_of_projection_origin", n(-75.0)),
            ("grid_mapping_name", t("geostationary")),
            ("sweep_angle_axis", t("x")),
        ]
    }

    /// `projection_lambert` from a MEPS file on thredds.met.no.
    fn meps_projection_lambert() -> Vec<(&'static str, CfAttr)> {
        vec![
            ("grid_mapping_name", t("lambert_conformal_conic")),
            ("standard_parallel", ns(&[63.3, 63.3])),
            ("longitude_of_central_meridian", n(15.0)),
            ("latitude_of_projection_origin", n(63.3)),
            ("earth_radius", n(6371000.0)),
            (
                "proj4",
                t("+proj=lcc +lat_0=63.3 +lon_0=15 +lat_1=63.3 +lat_2=63.3 +no_defs +R=6.371e+06"),
            ),
        ]
    }

    fn meps_crs() -> Crs {
        crs_from_grid_mapping(attrs(&meps_projection_lambert())).unwrap()
    }

    /// `pairs` with attribute `name` set to `value`, resolved.
    fn with(
        mut pairs: Vec<(&'static str, CfAttr)>,
        name: &'static str,
        value: CfAttr,
    ) -> Result<Crs, String> {
        pairs.retain(|(key, _)| *key != name);
        pairs.push((name, value));
        crs_from_grid_mapping(attrs(&pairs))
    }

    /// `pairs` without attributes `names`, resolved.
    fn without(mut pairs: Vec<(&'static str, CfAttr)>, names: &[&str]) -> Result<Crs, String> {
        pairs.retain(|(key, _)| !names.contains(key));
        crs_from_grid_mapping(attrs(&pairs))
    }

    /// Asserts `crs` maps each `(lon, lat)` to the PROJ `(x, y)` within
    /// 1 mm, and the PROJ point back.
    fn assert_matches_proj(crs: &Crs, cases: &[(f64, f64, f64, f64)]) {
        for &(lon, lat, x_proj, y_proj) in cases {
            let (x, y) = crs.forward(lon, lat);
            assert!(
                (x - x_proj).abs() < 1e-3 && (y - y_proj).abs() < 1e-3,
                "forward({lon}, {lat}) = ({x}, {y}), PROJ ({x_proj}, {y_proj})"
            );
            let (lon_back, lat_back) = crs.inverse(x_proj, y_proj).unwrap();
            assert!(
                (lon_back - lon).abs() < 1e-7 && (lat_back - lat).abs() < 1e-7,
                "inverse({x_proj}, {y_proj}) = ({lon_back}, {lat_back}), PROJ ({lon}, {lat})"
            );
        }
    }

    #[test]
    fn goes_r_grid_mapping_resolves() {
        let crs = crs_from_grid_mapping(attrs(&goes_imager_projection())).unwrap();
        assert_eq!(
            crs,
            Crs::Geostationary {
                lon0: (-75.0_f64).to_radians(),
                height: 35786023.0,
                semi_major: 6378137.0,
                semi_minor: 6356752.31414,
                sweep: SweepAxis::X,
            }
        );
        // GOES-R x/y are radians; the projection works in angle × height.
        assert_eq!(coordinate_scale("rad", &crs), Some(35786023.0));
        assert_eq!(coordinate_scale("microradian", &crs), Some(35.786023));
        assert_eq!(coordinate_scale("degrees", &crs), None);
    }

    #[test]
    fn earth_figure_and_axis_variants() {
        let mapping = |figure: Vec<(&'static str, CfAttr)>, axis: (&'static str, &str)| {
            let mut pairs = vec![
                ("grid_mapping_name", t("geostationary")),
                ("longitude_of_projection_origin", n(140.7)),
                ("perspective_point_height", n(35785831.0)),
                (axis.0, t(axis.1)),
            ];
            pairs.extend(figure);
            match crs_from_grid_mapping(attrs(&pairs)).unwrap() {
                Crs::Geostationary {
                    semi_major,
                    semi_minor,
                    sweep,
                    ..
                } => (semi_major, semi_minor, sweep),
                other => panic!("{other:?}"),
            }
        };
        // A fixed x axis means a y sweep (Himawari, Meteosat).
        let (a, b, sweep) = mapping(
            vec![
                ("semi_major_axis", n(6378137.0)),
                ("inverse_flattening", n(298.257223563)),
            ],
            ("fixed_angle_axis", "x"),
        );
        assert_eq!((a, sweep), (6378137.0, SweepAxis::Y));
        assert!((b - 6356752.314245).abs() < 1e-5, "b={b}");
        let (a, b, _) = mapping(
            vec![("earth_radius", n(6371000.0))],
            ("sweep_angle_axis", "y"),
        );
        assert_eq!((a, b), (6371000.0, 6371000.0));
        let (a, b, _) = mapping(
            vec![("semi_major_axis", n(6378137.0))],
            ("sweep_angle_axis", "y"),
        );
        assert_eq!((a, b), (6378137.0, 6378137.0));
    }

    #[test]
    fn unusable_mappings_are_errors_not_guesses() {
        let with = |name, value| with(goes_imager_projection(), name, value);
        let without = |names: &[&str]| without(goes_imager_projection(), names);
        assert!(with("false_easting", n(1000.0)).is_err());
        assert!(with("latitude_of_projection_origin", n(10.0)).is_err());
        assert!(with("perspective_point_height", n(-1.0)).is_err());
        assert!(with("perspective_point_height", n(f64::NAN)).is_err());
        assert!(with("sweep_angle_axis", t("z")).is_err());
        assert!(
            with("fixed_angle_axis", t("x")).is_err(),
            "axes must differ"
        );
        assert!(with("grid_mapping_name", t("polar_stereographic")).is_err());
        assert!(with("longitude_of_projection_origin", t("-75")).is_err());
        assert!(without(&["sweep_angle_axis"]).is_err());
        assert!(without(&["perspective_point_height"]).is_err());
        assert!(without(&["semi_major_axis", "semi_minor_axis", "inverse_flattening"]).is_err());
        assert!(without(&["grid_mapping_name"]).is_err());
    }

    /// A multi-element attribute where one number belongs is an error,
    /// never its first element: a reader keeping only the first would
    /// silently move the satellite or the cone.
    #[test]
    fn multi_element_values_are_never_cut_to_their_first() {
        let error = with(
            goes_imager_projection(),
            "longitude_of_projection_origin",
            ns(&[-75.0, -137.0]),
        )
        .unwrap_err();
        assert!(error.contains("must be one number, got 2"), "{error}");
        assert!(with(
            meps_projection_lambert(),
            "longitude_of_central_meridian",
            ns(&[15.0, 16.0])
        )
        .is_err());
        assert!(with(
            meps_projection_lambert(),
            "earth_radius",
            ns(&[6371000.0, 1.0])
        )
        .is_err());
        // A one-element list is that number.
        assert_eq!(
            with(
                goes_imager_projection(),
                "perspective_point_height",
                ns(&[35786023.0])
            ),
            crs_from_grid_mapping(attrs(&goes_imager_projection()))
        );
        assert!(with(
            goes_imager_projection(),
            "perspective_point_height",
            ns(&[])
        )
        .is_err());
        assert!(with(
            goes_imager_projection(),
            "longitude_of_projection_origin",
            ns(&[-75.0, f64::NAN])
        )
        .is_err());
    }

    /// MEPS's `projection_lambert` (two equal parallels on a 6371 km
    /// sphere) pinned to `cs2cs +proj=longlat +R=6371000 +to +proj=lcc
    /// +lat_1=63.3 +lat_2=63.3 +lat_0=63.3 +lon_0=15 +R=6371000 +units=m`
    /// (PROJ 9.8.1).
    #[test]
    fn meps_lambert_conformal_conic_matches_proj() {
        let crs = meps_crs();
        assert_eq!(
            crs,
            Crs::LambertConformalConic {
                lat1: 63.3_f64.to_radians(),
                lat2: 63.3_f64.to_radians(),
                lat0: 63.3_f64.to_radians(),
                lon0: 15.0_f64.to_radians(),
                false_e: 0.0,
                false_n: 0.0,
                radius: Some(6_371_000.0),
            }
        );
        assert_matches_proj(
            &crs,
            &[
                (15.0, 63.3, 0.0, 0.0),
                (25.0, 60.17, 551671.8943, -305112.4955),
                (0.279, 50.32, -1060023.1835, -1332487.2622),
                (40.0, 70.0, 933856.1083, 931177.9751),
            ],
        );
        // One parallel is the same tangent cone.
        assert_eq!(
            with(meps_projection_lambert(), "standard_parallel", n(63.3)),
            Ok(crs.clone())
        );
        // A scale factor of 1 is the cone as defined.
        assert_eq!(
            with(
                meps_projection_lambert(),
                "scale_factor_at_projection_origin",
                n(1.0)
            ),
            Ok(crs.clone())
        );
        assert_eq!(coordinate_scale("m", &crs), Some(1.0));
        assert_eq!(coordinate_scale("km", &crs), None);
    }

    /// A secant cone on WGS84 with a false origin, pinned to `cs2cs
    /// +proj=longlat +datum=WGS84 +to +proj=lcc +lat_1=33 +lat_2=45
    /// +lat_0=40 +lon_0=-97 +x_0=500000 +y_0=-200000 +datum=WGS84 +units=m`
    /// (PROJ 9.8.1). GRS80 gives the same values to 0.1 mm.
    #[test]
    fn secant_lambert_conformal_conic_on_wgs84_matches_proj() {
        let mapping = |figure: Vec<(&'static str, CfAttr)>| {
            let mut pairs = vec![
                ("grid_mapping_name", t("lambert_conformal_conic")),
                ("standard_parallel", ns(&[33.0, 45.0])),
                ("longitude_of_central_meridian", n(-97.0)),
                ("latitude_of_projection_origin", n(40.0)),
                ("false_easting", n(500000.0)),
                ("false_northing", n(-200000.0)),
            ];
            pairs.extend(figure);
            crs_from_grid_mapping(attrs(&pairs))
        };
        let crs = mapping(vec![
            ("semi_major_axis", n(6378137.0)),
            ("inverse_flattening", n(298.257223563)),
        ])
        .unwrap();
        assert!(
            matches!(crs, Crs::LambertConformalConic { radius: None, .. }),
            "{crs:?}"
        );
        assert_matches_proj(
            &crs,
            &[
                (-97.0, 40.0, 500000.0, -200000.0),
                (-122.4, 37.8, -1696467.9736, -133933.5345),
                (-74.0, 40.7, 2413429.8188, 120766.8583),
            ],
        );
        for figure in [
            vec![
                ("semi_major_axis", n(6378137.0)),
                ("semi_minor_axis", n(6356752.314245)),
            ],
            // GRS80.
            vec![
                ("semi_major_axis", n(6378137.0)),
                ("inverse_flattening", n(298.257222101)),
            ],
        ] {
            assert_eq!(mapping(figure), Ok(crs.clone()));
        }
        // Bessel 1841 is another ellipsoid, and only WGS84 is honoured.
        let error = mapping(vec![
            ("semi_major_axis", n(6377397.155)),
            ("inverse_flattening", n(299.1528128)),
        ])
        .unwrap_err();
        assert!(error.contains("only a sphere or WGS84"), "{error}");
        // A sphere given by its semi-major axis alone is a sphere.
        assert!(matches!(
            mapping(vec![("semi_major_axis", n(6371229.0))]),
            Ok(Crs::LambertConformalConic {
                radius: Some(6371229.0),
                ..
            })
        ));
    }

    /// Two distinct parallels on a sphere, in either order, pinned to
    /// `cs2cs +proj=longlat +R=6371000 +to +proj=lcc +lat_1=58.964
    /// +lat_2=69.987 +lat_0=63 +lon_0=15 +R=6371000 +units=m` (PROJ 9.8.1).
    #[test]
    fn secant_lambert_conformal_conic_on_a_sphere_matches_proj() {
        for parallels in [[58.964, 69.987], [69.987, 58.964]] {
            let crs = crs_from_grid_mapping(attrs(&[
                ("grid_mapping_name", t("lambert_conformal_conic")),
                ("standard_parallel", ns(&parallels)),
                ("longitude_of_central_meridian", n(15.0)),
                ("latitude_of_projection_origin", n(63.0)),
                ("earth_radius", n(6371000.0)),
            ]))
            .unwrap();
            assert_matches_proj(&crs, &[(10.75, 59.91, -236426.0450, -334623.5260)]);
        }
    }

    #[test]
    fn unusable_lambert_conformal_conic_mappings_are_errors() {
        let with = |name, value| with(meps_projection_lambert(), name, value);
        let without = |names: &[&str]| without(meps_projection_lambert(), names);
        // A one-parallel k0 other than 1 cannot be honoured.
        let mut one_parallel = meps_projection_lambert();
        one_parallel.retain(|(key, _)| *key != "standard_parallel");
        one_parallel.push(("standard_parallel", n(63.3)));
        let error =
            self::with(one_parallel, "scale_factor_at_projection_origin", n(0.9996)).unwrap_err();
        assert!(
            error.contains("scale_factor_at_projection_origin"),
            "{error}"
        );
        assert!(with("standard_parallel", ns(&[60.0, 63.0, 66.0])).is_err());
        assert!(with("standard_parallel", t("63.3")).is_err());
        assert!(with("standard_parallel", n(90.0)).is_err());
        assert!(with("standard_parallel", ns(&[-91.0, 63.3])).is_err());
        // Symmetric about the equator: a cylinder, not a cone.
        assert!(with("standard_parallel", ns(&[30.0, -30.0])).is_err());
        assert!(with("standard_parallel", n(0.0)).is_err());
        assert!(with("latitude_of_projection_origin", n(95.0)).is_err());
        assert!(with("earth_radius", n(-6371000.0)).is_err());
        assert!(without(&["standard_parallel"]).is_err());
        assert!(without(&["longitude_of_central_meridian"]).is_err());
        assert!(without(&["latitude_of_projection_origin"]).is_err());
        // No earth figure is not a WGS84 guess.
        assert!(without(&["earth_radius"]).is_err());
    }

    #[test]
    fn latitude_longitude_is_crs84() {
        let pairs = [("grid_mapping_name", t("latitude_longitude"))];
        let crs = crs_from_grid_mapping(attrs(&pairs)).unwrap();
        assert_eq!(crs, Crs::Wgs84);
        for units in [
            "degrees_east",
            "degrees_north",
            "degree_N",
            "degreesE",
            "degrees",
        ] {
            assert_eq!(coordinate_scale(units, &crs), Some(1.0), "{units}");
        }
        assert_eq!(coordinate_scale("m", &crs), None);
        assert_eq!(coordinate_scale("degrees_west", &crs), None);
    }

    fn axis(first: f64, step: f64, len: u32, units: &str) -> RegularAxis {
        RegularAxis {
            first,
            step,
            len,
            units: units.to_string(),
        }
    }

    /// MEPS's 949 × 1069 grid at 2500 m: `y` ascends from −1332517.9 m, so
    /// row 0 is the southern edge and the grid is flipped. The PROJ point
    /// (0.279, 50.32) lies in the stored south-west cell, which is the
    /// transform's last row.
    #[test]
    fn an_ascending_y_is_flipped_to_north_up() {
        let x = axis(-1060084.0, 2500.0, 949, "m");
        let y_values: Vec<f64> = (0..1069)
            .map(|i| -1332517.9 + f64::from(i) * 2500.0)
            .collect();
        let y = RegularAxis::from_values("y", &y_values, "m").unwrap();
        assert!((y.step - 2500.0).abs() < 1e-9, "step={}", y.step);
        let grid = grid(meps_crs(), &x, &y).unwrap();
        assert!(grid.flipped);
        let gt = &grid.transform;
        assert_eq!((gt.width, gt.height), (949, 1069));
        assert!((gt.pixel_width - 2500.0).abs() < 1e-9);
        assert!((gt.pixel_height - 2500.0).abs() < 1e-9);
        assert!((gt.origin_x - (-1060084.0 - 1250.0)).abs() < 1e-6);
        assert!((gt.origin_y - (1337482.1 + 1250.0)).abs() < 1e-6);
        assert_eq!(gt.world_to_pixel(0.279, 50.32), Some((0, 1068)));
        // The projection origin's fractional row, counted from the south
        // edge in storage, counts from the north edge in the transform.
        let (_, row) = gt.world_to_pixel_f64(15.0, 63.3);
        let stored_row = (0.0 - (-1332517.9 - 1250.0)) / 2500.0;
        assert!((row - (1069.0 - stored_row)).abs() < 1e-9, "row={row}");
    }

    #[test]
    fn a_descending_y_is_north_up_as_stored() {
        let x = axis(-1000.0, 2500.0, 4, "m");
        let grid_of = |y: RegularAxis| grid(meps_crs(), &x, &y).unwrap();
        let north_up = grid_of(axis(3000.0, -2500.0, 3, "metres"));
        assert!(!north_up.flipped);
        let gt = &north_up.transform;
        assert_eq!(
            (gt.origin_x, gt.origin_y, gt.pixel_width, gt.pixel_height),
            (-2250.0, 4250.0, 2500.0, 2500.0)
        );
        // The same cells stored south-up describe the same transform.
        let south_up = grid_of(axis(-2000.0, 2500.0, 3, "m"));
        assert!(south_up.flipped);
        let flipped = &south_up.transform;
        assert_eq!(
            (
                flipped.origin_x,
                flipped.origin_y,
                flipped.pixel_width,
                flipped.pixel_height
            ),
            (-2250.0, 4250.0, 2500.0, 2500.0)
        );
    }

    #[test]
    fn a_latitude_longitude_grid_is_in_degrees() {
        // A south-up global 0.25° grid.
        let grid = grid(
            Crs::Wgs84,
            &axis(-179.875, 0.25, 1440, "degrees_east"),
            &axis(-89.875, 0.25, 720, "degrees_north"),
        )
        .unwrap();
        assert!(grid.flipped);
        assert_eq!(grid.transform.bbox(), [-180.0, -90.0, 180.0, 90.0]);
        assert_eq!(grid.transform.world_to_pixel(-179.9, 89.9), Some((0, 0)));
    }

    #[test]
    fn unusable_axes_are_errors() {
        let x = axis(-1000.0, 2500.0, 4, "m");
        let y = axis(3000.0, -2500.0, 3, "m");
        // Each axis's own units count: kilometres are not metres.
        let error = grid(meps_crs(), &x, &axis(3000.0, -2500.0, 3, "km")).unwrap_err();
        assert!(error.contains("y coordinate"), "{error}");
        assert!(grid(meps_crs(), &axis(-1000.0, 2500.0, 4, ""), &y).is_err());
        assert!(grid(Crs::Wgs84, &x, &y).is_err(), "metres are not degrees");
        // x must run west to east.
        assert!(grid(meps_crs(), &axis(6500.0, -2500.0, 4, "m"), &y).is_err());
        assert!(grid(meps_crs(), &x, &axis(0.0, 2500.0, 0, "m")).is_err());
    }

    #[test]
    fn regular_axes_from_values() {
        let regular = RegularAxis::from_values("x", &[10.0, 12.5, 15.0, 17.5], "m").unwrap();
        assert_eq!(regular, axis(10.0, 2.5, 4, "m"));
        // f32 rounding noise is regular; the step is the end-to-end mean.
        let noisy: Vec<f64> = (0..1000)
            .map(|i| f64::from((0.1 * f64::from(i)) as f32))
            .collect();
        let regular = RegularAxis::from_values("lon", &noisy, "degrees_east").unwrap();
        assert!((regular.step - 0.1).abs() < 1e-8, "step={}", regular.step);
        let irregular: [&[f64]; 6] = [
            &[1.0],
            &[],
            &[1.0, 1.0, 1.0],
            &[0.0, 1.0, 3.0],
            &[0.0, 1.0, f64::NAN, 3.0],
            &[0.0, f64::INFINITY],
        ];
        for values in irregular {
            assert!(
                RegularAxis::from_values("x", values, "m").is_err(),
                "{values:?}"
            );
        }
    }

    /// A Float32 axis on the lattice `first + i * step`: each value rounded
    /// to f32, then the `down` ones an ulp below that and every other odd
    /// one an ulp above, as a producer's own f32 arithmetic leaves them.
    fn f32_axis(first: f64, step: f64, len: u32, down: &[u32]) -> Vec<f64> {
        (0..len)
            .map(|i| {
                let value = (first + f64::from(i) * step) as f32;
                let value = if down.contains(&i) {
                    value.next_down()
                } else if i % 2 == 1 {
                    value.next_up()
                } else {
                    value
                };
                f64::from(value)
            })
            .collect()
    }

    /// MEPS's x/y are Float32, and the file shows them so at every 100th
    /// index. The axes are regular, and the origin and step come from the
    /// first and last values and the count.
    #[test]
    fn meps_f32_axes_are_regular() {
        let down: Vec<u32> = (1..=8).map(|k| k * 100).collect();
        let x = f32_axis(-1060084.0, 2500.0, 949, &down);
        let y = f32_axis(-1332517.9, 2500.0, 1069, &[500]);
        for (axis, i, file) in [
            (&x, 0, -1060084.0_f32),
            (&x, 100, -810084.06),
            (&x, 200, -560084.06),
            (&x, 900, 1189916.0),
            (&y, 0, -1332517.9),
            (&y, 100, -1082517.9),
            (&y, 500, -82517.91),
            (&y, 600, 167482.1),
        ] {
            assert_eq!(axis[i], f64::from(file), "index {i}");
        }
        let x_axis = RegularAxis::from_values("x", &x, "m").unwrap();
        let y_axis = RegularAxis::from_values("y", &y, "m").unwrap();
        assert!((x_axis.step - 2500.0).abs() < 1e-3, "{x_axis:?}");
        assert!((y_axis.step - 2500.0).abs() < 1e-3, "{y_axis:?}");
        let grid = grid(meps_crs(), &x_axis, &y_axis).unwrap();
        assert!(grid.flipped);
        let gt = &grid.transform;
        assert!((gt.origin_x - (-1060084.0 - 1250.0)).abs() < 0.2, "{gt:?}");
        assert!((gt.origin_y - (1337482.1 + 1250.0)).abs() < 0.2, "{gt:?}");
        // Every 100th value, as a strided subset reads them: a 250 km step.
        let every_100th = |values: &[f64]| values.iter().step_by(100).copied().collect::<Vec<_>>();
        for values in [every_100th(&x), every_100th(&y)] {
            let axis = RegularAxis::from_values("subset", &values, "m").unwrap();
            assert!((axis.step - 250000.0).abs() < 1e-3, "{axis:?}");
        }
        // A 3 m step out of line is irregular, f32 or not.
        let mut glitch = x.clone();
        glitch[474] += 3.0;
        assert!(RegularAxis::from_values("x", &glitch, "m").is_err());
    }

    /// Near ±180° an f32 ulp is 1.5e-5°, so one ulp of noise on a 0.01°
    /// longitude is past a thousandth of its step: the f32 allowance keeps
    /// the axis regular, and a real misplacement is still rejected.
    #[test]
    fn a_fine_f32_axis_near_the_antimeridian_is_regular() {
        let lon = f32_axis(-179.995, 0.01, 36_000, &[]);
        let axis = RegularAxis::from_values("lon", &lon, "degrees_east").unwrap();
        assert!((axis.step - 0.01).abs() < 1e-8, "{axis:?}");
        let mut glitch = lon.clone();
        glitch[18_000] += 0.001;
        assert!(RegularAxis::from_values("lon", &glitch, "degrees_east").is_err());
    }
}
