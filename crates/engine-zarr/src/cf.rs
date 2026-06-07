//! CF-conventions helpers for the Zarr engine.
//!
//! Pure functions, no I/O: time-axis decoding (`<unit> since <reference>`),
//! axis-role classification (latitude / longitude / time), the bad-chunking
//! heuristic, and 1-D coordinate location for bilinear sampling.

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};

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
    /// A dimension we don't geolocate (vertical level, ensemble member,
    /// projected x/y, …). Pinned to index 0 when sampling.
    Other,
}

/// Classify a dimension given its name and, when a CF coordinate variable
/// exists for it, that variable's `standard_name` and `units` attributes.
///
/// Attributes win over the name heuristic: a `standard_name`/`units` that
/// names a projected axis (metres) resolves to [`AxisRole::Other`] so a
/// non-geographic store fails the lat/lon check cleanly rather than being
/// mistaken for degrees.
pub fn classify_axis(dim_name: &str, standard_name: Option<&str>, units: Option<&str>) -> AxisRole {
    if let Some(sn) = standard_name {
        match sn.trim().to_ascii_lowercase().as_str() {
            "time" => return AxisRole::Time,
            "forecast_reference_time" => return AxisRole::Reference,
            "forecast_period" => return AxisRole::Lead,
            "latitude" => return AxisRole::Lat,
            "longitude" => return AxisRole::Lon,
            // Projected / rotated / curvilinear axes are not geographic — keep
            // them out of the lat/lon slots (Phase 4 territory).
            "projection_x_coordinate"
            | "projection_y_coordinate"
            | "grid_latitude"
            | "grid_longitude" => return AxisRole::Other,
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
        if parse_time_units(&ul).is_some() {
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
        "y" if units.is_none() => AxisRole::Lat,
        "x" if units.is_none() => AxisRole::Lon,
        _ => AxisRole::Other,
    }
}

/// Parse a plain duration unit (no "since"), returning seconds-per-unit. Used to
/// decode a forecast **lead** axis (e.g. `seconds`, `hours`). Returns `None` for
/// anything that isn't a recognised duration unit.
pub fn parse_duration_seconds(units: &str) -> Option<f64> {
    match units.trim().to_ascii_lowercase().as_str() {
        "nanoseconds" | "nanosecond" | "ns" => Some(1e-9),
        "microseconds" | "microsecond" | "us" | "µs" => Some(1e-6),
        "milliseconds" | "millisecond" | "ms" => Some(1e-3),
        "seconds" | "second" | "secs" | "sec" | "s" => Some(1.0),
        "minutes" | "minute" | "mins" | "min" => Some(60.0),
        "hours" | "hour" | "hrs" | "hr" | "h" => Some(3600.0),
        "days" | "day" | "d" => Some(86_400.0),
        _ => None,
    }
}

/// Parse a CF time `units` string of the form `"<unit> since <reference>"`
/// into `(seconds_per_unit, reference_epoch)`. Returns `None` if the string
/// is not a recognised time encoding.
pub fn parse_time_units(units: &str) -> Option<(f64, DateTime<Utc>)> {
    let lower = units.trim().to_ascii_lowercase();
    let idx = lower.find(" since ")?;
    let unit_part = lower[..idx].trim();
    // Use the ORIGINAL (case-preserved) slice for the reference datetime so a
    // trailing "Z"/timezone is parsed correctly.
    let ref_part = units.trim()[idx + " since ".len()..].trim();
    let secs = match unit_part {
        "seconds" | "second" | "secs" | "sec" | "s" => 1.0,
        "minutes" | "minute" | "mins" | "min" => 60.0,
        "hours" | "hour" | "hrs" | "hr" | "h" => 3600.0,
        "days" | "day" | "d" => 86_400.0,
        _ => return None,
    };
    let epoch = parse_cf_datetime(ref_part)?;
    Some((secs, epoch))
}

/// Parse a CF reference datetime, tolerating the common spellings: RFC 3339,
/// `YYYY-MM-DD HH:MM:SS[.f]`, the `T`-separated variant, `YYYY-MM-DD HH:MM`,
/// and a bare `YYYY-MM-DD`. A trailing `Z` is treated as UTC.
fn parse_cf_datetime(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    let core = s.trim_end_matches('Z').trim();
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(core, fmt) {
            return Some(Utc.from_utc_datetime(&ndt));
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(core, "%Y-%m-%d") {
        return Some(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0)?));
    }
    None
}

/// True if `calendar` is one this engine decodes exactly. Non-standard
/// calendars (`360_day`, `noleap`, `julian`, …) are approximated with the
/// proleptic Gregorian calendar; callers should warn.
pub fn is_standard_calendar(calendar: Option<&str>) -> bool {
    match calendar {
        None => true,
        Some(c) => matches!(
            c.trim().to_ascii_lowercase().as_str(),
            "standard" | "gregorian" | "proleptic_gregorian" | ""
        ),
    }
}

/// Decode raw time-axis values into UTC instants using a CF `units` string.
///
/// Calendar handling is Gregorian; non-standard calendars are approximated
/// (the caller decides whether to warn — see [`is_standard_calendar`]).
pub fn decode_times(raw: &[f64], units: &str) -> Result<Vec<DateTime<Utc>>, String> {
    let (secs, epoch) =
        parse_time_units(units).ok_or_else(|| format!("unrecognised CF time units '{units}'"))?;
    raw.iter()
        .map(|&v| {
            // A non-finite value (e.g. zarrs returning the Zarr fill for an
            // unwritten time-axis chunk) would `round() as i64`-saturate to 0
            // and silently produce the epoch — fail the build instead.
            if !v.is_finite() {
                return Err(format!("non-finite value in time axis: {v}"));
            }
            Ok(epoch + Duration::milliseconds((v * secs * 1000.0).round() as i64))
        })
        .collect()
}

/// Whether a variable's chunk shape is pathological for point / time-series
/// access: each timestep is stored as a single full-domain spatial chunk, so a
/// one-point time series must decode the entire field for every timestep
/// (e.g. MUR-SST's `time=1, lat=full, lon=full`). Issue #125 asks us to warn
/// on this at startup.
pub fn is_bad_timeseries_chunking(
    time_chunk: u64,
    lat_chunk: u64,
    lon_chunk: u64,
    ny: u64,
    nx: u64,
    n_times: u64,
) -> bool {
    n_times > 1 && time_chunk <= 1 && lat_chunk >= ny && lon_chunk >= nx
}

/// Whether a 1-D axis is monotonic (non-strictly ascending or descending).
/// [`locate`] assumes this; a non-monotonic coordinate axis would otherwise
/// produce silently wrong bilinear interpolation, so callers validate up front.
pub fn is_monotonic(vals: &[f64]) -> bool {
    if vals.len() < 2 {
        return true;
    }
    let ascending = vals[1] >= vals[0];
    vals.windows(2).all(|w| {
        if ascending {
            w[1] >= w[0]
        } else {
            w[1] <= w[0]
        }
    })
}

/// Locate `target` within a monotonic (ascending or descending) 1-D
/// coordinate axis. Returns `(lo, hi, w)` such that the interpolated value is
/// `data[lo] * (1 - w) + data[hi] * w`, with `lo`/`hi` the bracketing indices
/// and `w ∈ [0, 1]` the weight toward `hi`.
///
/// Returns `None` when `target` lies outside the axis range.
///
/// A single-element axis always returns `(0, 0, 0.0)`, snapping *any* in-bounds
/// target to the sole cell regardless of distance. This is intentional: a
/// collapsed/degenerate axis — e.g. a zonal- or meridional-mean field stored
/// with a single lat or lon point — should return its one value for every
/// position along that axis, not nodata.
pub fn locate(axis: &[f64], target: f64) -> Option<(usize, usize, f64)> {
    let n = axis.len();
    if n == 0 || !target.is_finite() {
        return None;
    }
    if n == 1 {
        return Some((0, 0, 0.0));
    }
    // Monotonic axis (ascending or descending) → O(log n) bracket via binary
    // search. Both endpoints are inclusive, matching a fence-post linear scan
    // (a query exactly at the first or last coordinate is in-range).
    let ascending = axis[n - 1] >= axis[0];
    let (min, max) = if ascending {
        (axis[0], axis[n - 1])
    } else {
        (axis[n - 1], axis[0])
    };
    if target < min || target > max {
        return None;
    }
    // `partition_point` returns the first index whose value is >= target
    // (ascending) / <= target (descending). Clamp into a valid bracket: `pos`
    // is 0 only when target == the first element and `n` only when target ==
    // the last — both in range here, so the clamp picks the edge cell.
    let pos = axis.partition_point(|&v| if ascending { v < target } else { v > target });
    let j = pos.clamp(1, n - 1);
    let i = j - 1;
    let (a, b) = (axis[i], axis[j]);
    let w = if (b - a).abs() < f64::EPSILON {
        0.0
    } else {
        (target - a) / (b - a)
    };
    Some((i, j, w))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_units_hours_since() {
        let (secs, epoch) = parse_time_units("hours since 2026-01-01 00:00:00").unwrap();
        assert_eq!(secs, 3600.0);
        assert_eq!(epoch, Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap());
    }

    #[test]
    fn time_units_variants() {
        assert!(parse_time_units("days since 1900-01-01").is_some());
        assert!(parse_time_units("seconds since 1970-01-01T00:00:00Z").is_some());
        assert!(parse_time_units("minutes since 2020-03-04 06:00").is_some());
        assert!(parse_time_units("Kelvin").is_none());
        assert!(parse_time_units("degrees_north").is_none());
    }

    #[test]
    fn decode_times_hours() {
        let t = decode_times(&[0.0, 6.0, 18.0], "hours since 2026-01-01 00:00:00").unwrap();
        assert_eq!(t[0], Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap());
        assert_eq!(t[1], Utc.with_ymd_and_hms(2026, 1, 1, 6, 0, 0).unwrap());
        assert_eq!(t[2], Utc.with_ymd_and_hms(2026, 1, 1, 18, 0, 0).unwrap());
    }

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
        // Projected axes named x/y with metre units must NOT become lon/lat.
        assert_eq!(
            classify_axis("x", Some("projection_x_coordinate"), Some("m")),
            AxisRole::Other
        );
        assert_eq!(classify_axis("x", None, Some("m")), AxisRole::Other);
        // Bare x/y with no units fall back to lon/lat.
        assert_eq!(classify_axis("x", None, None), AxisRole::Lon);
        assert_eq!(classify_axis("y", None, None), AxisRole::Lat);
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

    #[test]
    fn duration_units() {
        assert_eq!(parse_duration_seconds("seconds"), Some(1.0));
        assert_eq!(parse_duration_seconds("hours"), Some(3600.0));
        assert_eq!(parse_duration_seconds("days"), Some(86_400.0));
        // A "<unit> since <ref>" string is a time origin, not a duration.
        assert!(parse_duration_seconds("seconds since 1970-01-01").is_none());
        assert!(parse_duration_seconds("degrees_north").is_none());
    }

    #[test]
    fn locate_ascending_and_descending() {
        let asc = [0.0, 1.0, 2.0, 3.0];
        let (lo, hi, w) = locate(&asc, 1.5).unwrap();
        assert_eq!((lo, hi), (1, 2));
        assert!((w - 0.5).abs() < 1e-9);

        let desc = [60.0, 59.0, 58.0, 57.0];
        let (lo, hi, w) = locate(&desc, 58.25).unwrap();
        assert_eq!((lo, hi), (1, 2)); // between 59 and 58
        assert!((w - 0.75).abs() < 1e-9); // 0.75 of the way from 59 toward 58

        assert!(locate(&asc, -1.0).is_none());
        assert!(locate(&asc, 9.0).is_none());
    }

    #[test]
    fn locate_single_element_snaps_any_target() {
        // A collapsed axis (one point — e.g. a zonal/meridional mean) returns
        // its only cell for any target, near or far. Documented, intentional.
        let one = [50.0];
        assert_eq!(locate(&one, 50.0), Some((0, 0, 0.0)));
        assert_eq!(locate(&one, -120.0), Some((0, 0, 0.0)));
        assert_eq!(locate(&one, 9999.0), Some((0, 0, 0.0)));
        // Non-finite is still rejected.
        assert!(locate(&one, f64::NAN).is_none());
    }

    #[test]
    fn locate_inclusive_endpoints() {
        // Exact endpoints must stay in range (the subtle case a naive
        // partition_point bracket gets wrong at the upper edge).
        let asc = [0.0, 1.0, 2.0, 3.0];
        assert_eq!(locate(&asc, 0.0), Some((0, 1, 0.0))); // first element, w=0
        assert_eq!(locate(&asc, 3.0), Some((2, 3, 1.0))); // last element, w=1

        let desc = [60.0, 59.0, 58.0, 57.0];
        assert_eq!(locate(&desc, 60.0), Some((0, 1, 0.0)));
        assert_eq!(locate(&desc, 57.0), Some((2, 3, 1.0)));
    }

    #[test]
    fn decode_times_rejects_non_finite() {
        assert!(decode_times(&[0.0, f64::NAN], "hours since 2026-01-01").is_err());
        assert!(decode_times(&[0.0, 6.0], "hours since 2026-01-01").is_ok());
    }

    #[test]
    fn is_monotonic_detects_both_directions() {
        assert!(is_monotonic(&[1.0, 2.0, 3.0]));
        assert!(is_monotonic(&[3.0, 2.0, 1.0]));
        assert!(is_monotonic(&[5.0])); // trivially
        assert!(!is_monotonic(&[1.0, 3.0, 2.0]));
    }

    #[test]
    fn bad_chunking_fires_on_full_spatial_chunk() {
        // time=1, lat=full, lon=full → pathological for time series.
        assert!(is_bad_timeseries_chunking(1, 1800, 3600, 1800, 3600, 24));
        // time spans many steps in one chunk, spatial split → fine.
        assert!(!is_bad_timeseries_chunking(24, 100, 100, 1800, 3600, 24));
        // single timestep → not a time-series concern.
        assert!(!is_bad_timeseries_chunking(1, 1800, 3600, 1800, 3600, 1));
    }
}
