//! CF time coordinates (CF 1.11 §4.4): `<unit> since <reference>` units
//! decoded to UTC instants, and the plain duration units of a forecast
//! period axis.

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};

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
///
/// The reference is RFC 3339, or a date with an optional `T`- or
/// space-separated time of day and an optional UTC offset: `Z`, `UTC`,
/// `GMT`, or `±HH[:MM]` / `±HHMM`, attached or after a space. MET Norway
/// writes `seconds since 1970-01-01 00:00:00 +00:00`.
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

/// Parse a CF reference datetime (see [`parse_time_units`]): the local
/// date and time, less its UTC offset.
fn parse_cf_datetime(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    let (local, offset) = split_utc_offset(s)?;
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(local, fmt) {
            return Some(Utc.from_utc_datetime(&(ndt - offset)));
        }
    }
    let midnight = NaiveDate::parse_from_str(local, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)?;
    Some(Utc.from_utc_datetime(&(midnight - offset)))
}

/// Split a reference datetime into its local part and UTC offset. No
/// offset is UTC; a malformed one is `None` rather than a guess.
fn split_utc_offset(s: &str) -> Option<(&str, Duration)> {
    let upper = s.to_ascii_uppercase();
    for zone in ["UTC", "GMT", "Z"] {
        if upper.ends_with(zone) {
            return Some((s[..s.len() - zone.len()].trim_end(), Duration::zero()));
        }
    }
    // A numeric offset follows the time of day, which holds no sign; the
    // date's own hyphens all come before the first `T` or space.
    let Some(time) = s.find(['T', ' ']) else {
        return Some((s, Duration::zero()));
    };
    match s[time..].find(['+', '-']) {
        None => Some((s, Duration::zero())),
        Some(at) => {
            let at = time + at;
            Some((s[..at].trim_end(), parse_utc_offset(&s[at..])?))
        }
    }
}

/// `±H`, `±HH`, `±H:MM`, `±HH:MM` or `±HHMM` as a duration east of UTC.
fn parse_utc_offset(s: &str) -> Option<Duration> {
    let (sign, body) = match s.split_at_checked(1)? {
        ("+", body) => (1, body),
        ("-", body) => (-1, body),
        _ => return None,
    };
    let (hours, minutes) = match body.split_once(':') {
        Some(parts) => parts,
        None if body.len() == 4 => body.split_at(2),
        None => (body, "0"),
    };
    let field = |digits: &str, max: i64| {
        if !(1..=2).contains(&digits.len()) || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse::<i64>().ok().filter(|&v| v <= max)
    };
    let minutes = field(hours, 23)? * 60 + field(minutes, 59)?;
    Some(Duration::minutes(sign * minutes))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn epoch(units: &str) -> DateTime<Utc> {
        parse_time_units(units)
            .unwrap_or_else(|| panic!("'{units}' did not parse"))
            .1
    }

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

    /// MET Norway's spelling (MEPS `time` and `forecast_reference_time`):
    /// the space before the offset is what the RFC 3339 parse and the plain
    /// formats both reject.
    #[test]
    fn time_units_with_a_spaced_utc_offset_or_zone() {
        let unix = Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap();
        for units in [
            "seconds since 1970-01-01 00:00:00 +00:00",
            "seconds since 1970-01-01 00:00:00+00:00",
            "seconds since 1970-01-01 00:00:00 +0000",
            "seconds since 1970-01-01 00:00:00 -00:00",
            "seconds since 1970-01-01 00:00:00 UTC",
            "seconds since 1970-01-01 00:00:00 utc",
            "seconds since 1970-01-01 00:00:00 GMT",
            "seconds since 1970-01-01 00:00:00Z",
            "seconds since 1970-01-01 00:00:00 Z",
            "seconds since 1970-01-01T00:00:00Z",
            "seconds since 1970-01-01 00:00:00.0 UTC",
            "seconds since 1970-01-01 +00:00",
            "seconds since 1970-01-01",
        ] {
            assert_eq!(epoch(units), unix, "{units}");
        }
        let t = decode_times(
            &[1_781_676_000.0, 1_781_679_600.0],
            "seconds since 1970-01-01 00:00:00 +00:00",
        )
        .unwrap();
        assert_eq!(
            t,
            [
                Utc.with_ymd_and_hms(2026, 6, 17, 6, 0, 0).unwrap(),
                Utc.with_ymd_and_hms(2026, 6, 17, 7, 0, 0).unwrap(),
            ]
        );
    }

    #[test]
    fn a_non_zero_offset_moves_the_epoch_to_utc() {
        // CF 1.11 §4.4's own example: local 15:15:42.5 at UTC-6.
        assert_eq!(
            epoch("seconds since 1992-10-8 15:15:42.5 -6:00"),
            Utc.with_ymd_and_hms(1992, 10, 8, 21, 15, 42).unwrap() + Duration::milliseconds(500)
        );
        let local = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(
            epoch("hours since 2026-01-01 00:00:00 +02:00"),
            local - Duration::hours(2)
        );
        assert_eq!(
            epoch("hours since 2026-01-01 00:00 +0530"),
            local - Duration::minutes(330)
        );
        assert_eq!(
            epoch("hours since 2026-01-01T00:00:00 -03"),
            local + Duration::hours(3)
        );
    }

    #[test]
    fn a_malformed_offset_is_rejected_not_ignored() {
        for units in [
            "seconds since 1970-01-01 00:00:00 +25:00",
            "seconds since 1970-01-01 00:00:00 +00:60",
            "seconds since 1970-01-01 00:00:00 +ab",
            "seconds since 1970-01-01 00:00:00 +",
            "seconds since 1970-01-01 00:00:00 +123",
            "seconds since 1970-01-01 00:00:00 00:00",
        ] {
            assert!(parse_time_units(units).is_none(), "{units}");
        }
    }

    #[test]
    fn decode_times_hours() {
        let t = decode_times(&[0.0, 6.0, 18.0], "hours since 2026-01-01 00:00:00").unwrap();
        assert_eq!(t[0], Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap());
        assert_eq!(t[1], Utc.with_ymd_and_hms(2026, 1, 1, 6, 0, 0).unwrap());
        assert_eq!(t[2], Utc.with_ymd_and_hms(2026, 1, 1, 18, 0, 0).unwrap());
    }

    #[test]
    fn decode_times_rejects_non_finite() {
        assert!(decode_times(&[0.0, f64::NAN], "hours since 2026-01-01").is_err());
        assert!(decode_times(&[0.0, 6.0], "hours since 2026-01-01").is_ok());
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
    fn standard_calendars() {
        assert!(is_standard_calendar(None));
        assert!(is_standard_calendar(Some("proleptic_gregorian")));
        assert!(!is_standard_calendar(Some("360_day")));
    }
}
