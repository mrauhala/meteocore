//! CF conventions metadata shared by NetCDF readers.
//!
//! Resolves a CF grid-mapping variable (CF 1.11 Appendix F) to a
//! [`Crs`], and the unit of a geostationary coordinate variable to metres.
//! Readers own attribute access and pass a lookup closure, so this module
//! stays independent of any NetCDF library.

use crate::geo::{Crs, SweepAxis};

/// A grid-mapping attribute value as a reader found it.
#[derive(Debug, Clone, PartialEq)]
pub enum CfAttr {
    Number(f64),
    Text(String),
}

/// Resolve a CF grid mapping to a [`Crs`].
///
/// `attr(name)` returns attribute `name` of the grid-mapping variable.
/// Supported `grid_mapping_name`s: `latitude_longitude` and
/// `geostationary`. Any other mapping, a missing required parameter, or one
/// this server cannot honour (a non-zero false easting, an equatorial
/// origin other than 0°) is an error rather than a guess: a wrong earth
/// figure alone moves a geostationary pixel by kilometres.
pub fn crs_from_grid_mapping(attr: impl Fn(&str) -> Option<CfAttr>) -> Result<Crs, String> {
    let text = |name: &str| match attr(name) {
        Some(CfAttr::Text(value)) => Ok(Some(value)),
        Some(CfAttr::Number(_)) => Err(format!("grid mapping '{name}' must be text")),
        None => Ok(None),
    };
    let number = |name: &str| match attr(name) {
        Some(CfAttr::Number(value)) if value.is_finite() => Ok(Some(value)),
        Some(_) => Err(format!("grid mapping '{name}' must be a finite number")),
        None => Ok(None),
    };
    let name = text("grid_mapping_name")?.ok_or("grid mapping has no grid_mapping_name")?;
    match name.trim() {
        "latitude_longitude" => Ok(Crs::Wgs84),
        "geostationary" => {
            let required = |name: &str| {
                number(name)?.ok_or_else(|| format!("geostationary grid mapping needs '{name}'"))
            };
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

/// Metres per unit of a projected x/y coordinate variable in `crs`, or
/// `None` for a unit it cannot be in. A geostationary coordinate is the
/// scan angle — radians (`rad`, GOES-R) or microradians (Himawari ISatSS) —
/// and the projection works in the angle times the satellite height.
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
        Crs::Wgs84 | Crs::RotatedLatLon { .. } => None,
        _ => metres.then_some(1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let with = |name: &'static str, value: CfAttr| {
            let mut pairs = goes_imager_projection();
            pairs.retain(|(key, _)| *key != name);
            pairs.push((name, value));
            crs_from_grid_mapping(attrs(&pairs))
        };
        let without = |names: &[&str]| {
            let mut pairs = goes_imager_projection();
            pairs.retain(|(key, _)| !names.contains(key));
            crs_from_grid_mapping(attrs(&pairs))
        };
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

    #[test]
    fn latitude_longitude_is_crs84() {
        let pairs = [("grid_mapping_name", t("latitude_longitude"))];
        let crs = crs_from_grid_mapping(attrs(&pairs)).unwrap();
        assert_eq!(crs, Crs::Wgs84);
        assert_eq!(coordinate_scale("degrees_east", &crs), None);
    }
}
