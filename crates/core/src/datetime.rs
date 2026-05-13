use chrono::{DateTime, Duration, Utc};

use crate::error::DataServerError;

/// Parse an ISO 8601 positive duration (e.g. `"PT12H"`, `"P1D"`, `"P1DT6H"`)
/// into a `chrono::Duration`. Rejects zero-length and signed durations —
/// callers that need signed offsets should layer on top of this.
pub fn parse_iso8601_duration(s: &str) -> Result<Duration, DataServerError> {
    let rest = s.strip_prefix('P').ok_or_else(|| {
        DataServerError::Config(format!(
            "Invalid ISO 8601 duration '{s}': must start with 'P'"
        ))
    })?;

    let (date_part, time_part) = if let Some(t_pos) = rest.find('T') {
        (&rest[..t_pos], &rest[t_pos + 1..])
    } else {
        (rest, "")
    };

    let mut total_seconds: i64 = 0;

    if !date_part.is_empty() {
        // Weeks (`P1W`) — natural unit for meteorological archives; reject the
        // ambiguous combination `P1W2D` which mixes weeks with other date units.
        if let Some(stripped) = date_part.strip_suffix('W') {
            let weeks: i64 = stripped.parse().map_err(|_| {
                DataServerError::Config(format!("Invalid weeks in ISO 8601 duration '{s}'"))
            })?;
            total_seconds += weeks * 7 * 86_400;
        } else {
            let stripped = date_part.strip_suffix('D').ok_or_else(|| {
                DataServerError::Config(format!(
                    "Invalid date component in ISO 8601 duration '{s}': supported units are 'D' \
                     (days) and 'W' (weeks)"
                ))
            })?;
            let days: i64 = stripped.parse().map_err(|_| {
                DataServerError::Config(format!("Invalid days in ISO 8601 duration '{s}'"))
            })?;
            total_seconds += days * 86_400;
        }
    }

    // `PnDT` with no time components is not valid ISO 8601 — the `T` separator
    // must be followed by at least one of H/M/S. Reject explicitly so config
    // typos surface at load rather than silently parsing as `PnD`.
    if rest.contains('T') && time_part.is_empty() {
        return Err(DataServerError::Config(format!(
            "Invalid ISO 8601 duration '{s}': 'T' separator present but no time components follow"
        )));
    }

    if !time_part.is_empty() {
        let mut remaining = time_part;
        if let Some(pos) = remaining.find('H') {
            let v: i64 = remaining[..pos].parse().map_err(|_| {
                DataServerError::Config(format!("Invalid hours in ISO 8601 duration '{s}'"))
            })?;
            total_seconds += v * 3600;
            remaining = &remaining[pos + 1..];
        }
        if let Some(pos) = remaining.find('M') {
            let v: i64 = remaining[..pos].parse().map_err(|_| {
                DataServerError::Config(format!("Invalid minutes in ISO 8601 duration '{s}'"))
            })?;
            total_seconds += v * 60;
            remaining = &remaining[pos + 1..];
        }
        if let Some(pos) = remaining.find('S') {
            let v: i64 = remaining[..pos].parse().map_err(|_| {
                DataServerError::Config(format!("Invalid seconds in ISO 8601 duration '{s}'"))
            })?;
            total_seconds += v;
        }
    }

    if total_seconds <= 0 {
        return Err(DataServerError::Config(format!(
            "Invalid ISO 8601 duration '{s}': zero or negative duration"
        )));
    }

    Ok(Duration::seconds(total_seconds))
}

/// Parses an OGC datetime interval string like "2024-01-01T00:00:00Z/2024-01-01T06:00:00Z"
/// or a single instant "2024-01-01T00:00:00Z" (treated as a zero-width interval).
/// Also supports open intervals with ".." (e.g., "../2024-01-01T06:00:00Z").
pub fn parse_datetime_interval(
    input: &str,
) -> Result<(DateTime<Utc>, DateTime<Utc>), DataServerError> {
    if let Some((start_str, end_str)) = input.split_once('/') {
        let start = parse_bound(start_str, false)?;
        let end = parse_bound(end_str, true)?;
        Ok((start, end))
    } else {
        let instant = parse_single(input)?;
        Ok((instant, instant))
    }
}

fn parse_bound(s: &str, is_end: bool) -> Result<DateTime<Utc>, DataServerError> {
    if s == ".." {
        if is_end {
            Ok(DateTime::<Utc>::MAX_UTC)
        } else {
            Ok(DateTime::<Utc>::MIN_UTC)
        }
    } else {
        parse_single(s)
    }
}

fn parse_single(s: &str) -> Result<DateTime<Utc>, DataServerError> {
    s.parse::<DateTime<Utc>>()
        .map_err(|e| DataServerError::InvalidDatetime(format!("{s}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_interval() {
        let (start, end) =
            parse_datetime_interval("2024-01-01T00:00:00Z/2024-01-01T06:00:00Z").unwrap();
        assert_eq!(start.to_rfc3339(), "2024-01-01T00:00:00+00:00");
        assert_eq!(end.to_rfc3339(), "2024-01-01T06:00:00+00:00");
    }

    #[test]
    fn test_parse_instant() {
        let (start, end) = parse_datetime_interval("2024-01-01T03:00:00Z").unwrap();
        assert_eq!(start, end);
    }

    #[test]
    fn test_parse_open_start() {
        let (start, end) = parse_datetime_interval("../2024-01-01T06:00:00Z").unwrap();
        assert_eq!(start, DateTime::<Utc>::MIN_UTC);
        assert_eq!(end.to_rfc3339(), "2024-01-01T06:00:00+00:00");
    }

    #[test]
    fn iso8601_duration_basic() {
        assert_eq!(
            parse_iso8601_duration("PT12H").unwrap(),
            Duration::seconds(12 * 3600)
        );
        assert_eq!(
            parse_iso8601_duration("P1D").unwrap(),
            Duration::seconds(86_400)
        );
        assert_eq!(
            parse_iso8601_duration("P1DT6H").unwrap(),
            Duration::seconds(86_400 + 6 * 3600)
        );
        assert_eq!(
            parse_iso8601_duration("PT30M").unwrap(),
            Duration::seconds(1800)
        );
    }

    #[test]
    fn iso8601_duration_rejects_bad_input() {
        assert!(parse_iso8601_duration("").is_err());
        assert!(parse_iso8601_duration("12H").is_err());
        assert!(parse_iso8601_duration("PT0H").is_err());
        assert!(parse_iso8601_duration("P0D").is_err());
        assert!(parse_iso8601_duration("-PT2H").is_err());
        // Empty T segment is technically invalid; surface the typo.
        assert!(parse_iso8601_duration("P1DT").is_err());
        assert!(parse_iso8601_duration("PT").is_err());
    }

    #[test]
    fn iso8601_duration_supports_weeks() {
        assert_eq!(
            parse_iso8601_duration("P1W").unwrap(),
            Duration::seconds(7 * 86_400)
        );
        assert_eq!(
            parse_iso8601_duration("P2W").unwrap(),
            Duration::seconds(14 * 86_400)
        );
        // `P1W2D` mixes weeks with other date units — not supported.
        assert!(parse_iso8601_duration("P1W2D").is_err());
    }
}
