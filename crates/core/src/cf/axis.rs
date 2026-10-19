//! The role a dimension plays in a CF data variable, from its coordinate
//! variable's attributes or, failing those, its name.

use super::time::parse_time_units;

/// The role a dimension plays in a CF-conventions data variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxisRole {
    /// Valid time (a plain CF time axis).
    Time,
    /// Forecast **reference time** (model run / init / analysis time).
    Reference,
    /// Forecast **lead** / period (a duration offset from the reference time).
    Lead,
    Lat,
    Lon,
    /// Projected x (`projection_x_coordinate`, or the geostationary scan
    /// angle `projection_x_angular_coordinate`), placed by the variable's
    /// `grid_mapping`.
    ProjectionX,
    /// Projected y, as [`AxisRole::ProjectionX`].
    ProjectionY,
    /// Ensemble member (`standard_name = realization`, or a dimension named
    /// `ensemble_member`).
    Ensemble,
    /// A dimension no role covers (a vertical level, a rotated-pole axis, …).
    Other,
}

/// Classify a dimension given its name and, when a CF coordinate variable
/// exists for it, that variable's `standard_name` and `units` attributes.
///
/// Attributes win over the name heuristic: a `standard_name` naming a
/// projected axis resolves to a projection role, and metre `units` alone to
/// [`AxisRole::Other`], so a non-geographic grid is never mistaken for
/// degrees. A bare `x`/`y` with no units falls back to longitude/latitude.
pub fn classify_axis(dim_name: &str, standard_name: Option<&str>, units: Option<&str>) -> AxisRole {
    if let Some(sn) = standard_name {
        match sn.trim().to_ascii_lowercase().as_str() {
            "time" => return AxisRole::Time,
            "forecast_reference_time" => return AxisRole::Reference,
            "forecast_period" => return AxisRole::Lead,
            "latitude" => return AxisRole::Lat,
            "longitude" => return AxisRole::Lon,
            "projection_x_coordinate" | "projection_x_angular_coordinate" => {
                return AxisRole::ProjectionX
            }
            "projection_y_coordinate" | "projection_y_angular_coordinate" => {
                return AxisRole::ProjectionY
            }
            "realization" => return AxisRole::Ensemble,
            // Rotated-pole axes are not geographic and have no role yet.
            "grid_latitude" | "grid_longitude" => return AxisRole::Other,
            _ => {}
        }
    }
    if let Some(u) = units {
        let ul = u.trim().to_ascii_lowercase();
        if ul.starts_with("degrees_north")
            || ul == "degree_north"
            || ul == "degrees_n"
            || ul == "degreen"
        {
            return AxisRole::Lat;
        }
        if ul.starts_with("degrees_east")
            || ul == "degree_east"
            || ul == "degrees_e"
            || ul == "degreee"
        {
            return AxisRole::Lon;
        }
        if parse_time_units(u).is_some() {
            return AxisRole::Time;
        }
    }
    // Name fallback. `x`/`y` are accepted as lon/lat only when no units are
    // present, to avoid reading projected metres as degrees.
    match dim_name.trim().to_ascii_lowercase().as_str() {
        "time" | "t" | "valid_time" | "forecast_time" => AxisRole::Time,
        "init_time" | "reference_time" | "forecast_reference_time" | "analysis_time" => {
            AxisRole::Reference
        }
        "lead_time" | "forecast_period" | "step" | "prediction_timedelta" => AxisRole::Lead,
        "latitude" | "lat" | "nav_lat" => AxisRole::Lat,
        "longitude" | "lon" | "long" | "nav_lon" => AxisRole::Lon,
        "ensemble_member" => AxisRole::Ensemble,
        "y" if units.is_none() => AxisRole::Lat,
        "x" if units.is_none() => AxisRole::Lon,
        _ => AxisRole::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_by_attributes_then_name() {
        assert_eq!(
            classify_axis("nlat", Some("latitude"), Some("degrees_north")),
            AxisRole::Lat
        );
        assert_eq!(
            classify_axis("anything", None, Some("degrees_east")),
            AxisRole::Lon
        );
        assert_eq!(
            classify_axis("time", None, Some("hours since 2020-01-01")),
            AxisRole::Time
        );
        // Metre units alone are not degrees, nor a projection role.
        assert_eq!(classify_axis("x", None, Some("m")), AxisRole::Other);
        // Bare x/y with no units fall back to lon/lat.
        assert_eq!(classify_axis("x", None, None), AxisRole::Lon);
        assert_eq!(classify_axis("y", None, None), AxisRole::Lat);
        // Rotated-pole axes stay out of the lat/lon slots.
        assert_eq!(
            classify_axis("rlat", Some("grid_latitude"), Some("degrees")),
            AxisRole::Other
        );
    }

    /// MEPS's x/y, and GOES-R's scan angles under either CF name.
    #[test]
    fn projected_axes_by_standard_name() {
        assert_eq!(
            classify_axis("x", Some("projection_x_coordinate"), Some("m")),
            AxisRole::ProjectionX
        );
        assert_eq!(
            classify_axis("y", Some("projection_y_coordinate"), Some("m")),
            AxisRole::ProjectionY
        );
        assert_eq!(
            classify_axis("x", Some("projection_x_angular_coordinate"), Some("rad")),
            AxisRole::ProjectionX
        );
        assert_eq!(
            classify_axis("y", Some("projection_y_angular_coordinate"), Some("rad")),
            AxisRole::ProjectionY
        );
    }

    #[test]
    fn ensemble_axis_by_standard_name_or_name() {
        assert_eq!(
            classify_axis("member", Some("realization"), None),
            AxisRole::Ensemble
        );
        assert_eq!(
            classify_axis("ensemble_member", None, None),
            AxisRole::Ensemble
        );
        assert_eq!(classify_axis("member", None, None), AxisRole::Other);
    }

    /// MET Norway's spaced offset makes `time` a time axis by its units.
    #[test]
    fn time_axis_by_units_with_a_spaced_offset() {
        assert_eq!(
            classify_axis(
                "valid",
                None,
                Some("seconds since 1970-01-01 00:00:00 +00:00")
            ),
            AxisRole::Time
        );
    }

    #[test]
    fn classify_forecast_axes() {
        assert_eq!(
            classify_axis(
                "init_time",
                Some("forecast_reference_time"),
                Some("seconds since 1970")
            ),
            AxisRole::Reference
        );
        assert_eq!(
            classify_axis("lead_time", Some("forecast_period"), Some("seconds")),
            AxisRole::Lead
        );
        // Plain valid time stays Time.
        assert_eq!(
            classify_axis("valid_time", Some("time"), Some("seconds since 1970")),
            AxisRole::Time
        );
        // Name fallbacks (no standard_name).
        assert_eq!(classify_axis("init_time", None, None), AxisRole::Reference);
        assert_eq!(
            classify_axis("lead_time", None, Some("hours")),
            AxisRole::Lead
        );
    }
}
