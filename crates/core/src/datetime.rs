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

    // Reject any sign character anywhere in `rest` up front. ISO 8601 doesn't
    // permit negative components within a positive duration, and without this
    // check `P1DT-2H` parses as 22h because `i64::parse("-2")` succeeds — a
    // config typo would produce a surprising window length, not an error.
    if rest.contains('-') || rest.contains('+') {
        return Err(DataServerError::Config(format!(
            "Invalid ISO 8601 duration '{s}': signed components are not permitted"
        )));
    }

    if !date_part.is_empty() {
        // Mixed week-and-day forms (`P1W2D`) are unsupported. Catch them
        // explicitly here — otherwise the parser falls into the days branch
        // below, fails on `"1W2".parse::<i64>()`, and surfaces a misleading
        // "Invalid days" message that hides the real issue.
        if date_part.contains('W') && date_part.contains('D') {
            return Err(DataServerError::Config(format!(
                "Invalid ISO 8601 duration '{s}': cannot mix 'W' with other date units"
            )));
        }
        // Weeks (`P1W`) — natural unit for meteorological archives.
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
            let v: u64 = remaining[..pos].parse().map_err(|_| {
                DataServerError::Config(format!("Invalid hours in ISO 8601 duration '{s}'"))
            })?;
            total_seconds += (v as i64) * 3600;
            remaining = &remaining[pos + 1..];
        }
        if let Some(pos) = remaining.find('M') {
            let v: u64 = remaining[..pos].parse().map_err(|_| {
                DataServerError::Config(format!("Invalid minutes in ISO 8601 duration '{s}'"))
            })?;
            total_seconds += (v as i64) * 60;
            remaining = &remaining[pos + 1..];
        }
        if let Some(pos) = remaining.find('S') {
            let v: u64 = remaining[..pos].parse().map_err(|_| {
                DataServerError::Config(format!("Invalid seconds in ISO 8601 duration '{s}'"))
            })?;
            total_seconds += v as i64;
            remaining = &remaining[pos + 1..];
        }
        // Anything left in `remaining` is unparsed — a trailing number with
        // no unit (`PT12H30` for `PT12H30M`) would otherwise be silently
        // dropped, returning a shorter window than the operator intended.
        if !remaining.is_empty() {
            return Err(DataServerError::Config(format!(
                "Invalid ISO 8601 duration '{s}': trailing characters '{remaining}' after time \
                 components"
            )));
        }
    }

    if total_seconds <= 0 {
        return Err(DataServerError::Config(format!(
            "Invalid ISO 8601 duration '{s}': zero or negative duration"
        )));
    }

    Ok(Duration::seconds(total_seconds))
}

/// Format a strictly-positive whole-second duration as a canonical ISO 8601
/// string (`300` → `"PT5M"`, `3600` → `"PT1H"`, `86400` → `"P1D"`,
/// `5400` → `"PT1H30M"`, `90000` → `"P1DT1H"`). Returns `None` for zero or
/// negative input. Used to advertise the temporal grid resolution in OGC API
/// Common Part 2 `extent.temporal.grid.resolution`.
pub fn format_iso8601_duration(seconds: i64) -> Option<String> {
    if seconds <= 0 {
        return None;
    }

    let days = seconds / 86_400;
    let mut rem = seconds % 86_400;
    let hours = rem / 3600;
    rem %= 3600;
    let minutes = rem / 60;
    let secs = rem % 60;

    let mut out = String::from("P");
    if days > 0 {
        out.push_str(&format!("{days}D"));
    }
    if hours > 0 || minutes > 0 || secs > 0 {
        out.push('T');
        if hours > 0 {
            out.push_str(&format!("{hours}H"));
        }
        if minutes > 0 {
            out.push_str(&format!("{minutes}M"));
        }
        if secs > 0 {
            out.push_str(&format!("{secs}S"));
        }
    }
    Some(out)
}

/// OGC API Common Part 2 `extent.temporal.grid` descriptor.
///
/// Serialises (via `serde`, the only serialization dependency ds-core carries)
/// to `{ "cellsCount": N, "resolution": "<ISO 8601>" }` for a regular series or
/// `{ "cellsCount": N, "coordinates": [<rfc3339>, …] }` for an irregular one.
/// Defined here — not in the API crates — so the Maps and Tiles
/// `extent.temporal.grid` builders share one definition and the JSON shape
/// can't drift between `/maps/...` and `/tiles/...`. The API crates turn it
/// into JSON with `serde_json::to_value` (ds-core never builds `serde_json::Value`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(untagged)]
pub enum TemporalGrid {
    Regular {
        #[serde(rename = "cellsCount")]
        cells_count: usize,
        resolution: String,
    },
    Irregular {
        #[serde(rename = "cellsCount")]
        cells_count: usize,
        coordinates: Vec<String>,
    },
}

/// Build the [`TemporalGrid`] descriptor for an ascending timestamp series: a
/// single resolution when the step is regular (see [`regular_step_duration`]),
/// else the full coordinates list. Returns `None` for fewer than two
/// timestamps.
pub fn temporal_grid(times: &[DateTime<Utc>]) -> Option<TemporalGrid> {
    if times.len() < 2 {
        return None;
    }
    let cells_count = times.len();
    if let Some(resolution) = regular_step_duration(times) {
        return Some(TemporalGrid::Regular {
            cells_count,
            resolution,
        });
    }
    let coordinates = times.iter().map(|t| t.to_rfc3339()).collect();
    Some(TemporalGrid::Irregular {
        cells_count,
        coordinates,
    })
}

/// Detect a regular sampling step across `times` (assumed ascending) and
/// return it as a canonical ISO 8601 duration. Returns `None` when there are
/// fewer than two timestamps or the gaps vary by more than 2 seconds (an
/// irregular series). The reported step is the rounded mean gap, so ±1 s
/// jitter on any single interval doesn't shift it or flip a regular series to
/// irregular. Shared by the Maps and Tiles `extent.temporal.grid` builders.
///
/// Caveat: the 2-second spread tolerance is absolute, sized for ±1 s clock
/// jitter on the minute-and-coarser cadences every current engine produces.
/// A truly alternating sub-minute series (e.g. gaps `[1, 3, 1, 3]`, spread 2)
/// would be reported as "regular PT2S". If a sub-minute engine is ever added,
/// revisit this threshold (e.g. make it relative to the mean step).
pub fn regular_step_duration(times: &[DateTime<Utc>]) -> Option<String> {
    if times.len() < 2 {
        return None;
    }
    let gaps: Vec<i64> = times
        .windows(2)
        .map(|w| (w[1] - w[0]).num_seconds())
        .collect();
    let min = *gaps.iter().min()?;
    let max = *gaps.iter().max()?;
    if min <= 0 || max - min > 2 {
        return None;
    }
    let count = gaps.len() as i64;
    let avg = (gaps.iter().sum::<i64>() + count / 2) / count;
    format_iso8601_duration(avg)
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
    fn iso8601_duration_rejects_signed_components() {
        // Pre-fix, `P1DT-2H` parsed as 22h because `i64::parse("-2")` succeeds
        // and `86400 - 7200 > 0` passed the zero-check. A config typo would
        // produce a window of surprising length, not an error.
        assert!(parse_iso8601_duration("P1DT-2H").is_err());
        assert!(parse_iso8601_duration("P-1DT2H").is_err());
        assert!(parse_iso8601_duration("PT2H-30M").is_err());
        assert!(parse_iso8601_duration("PT+2H").is_err());
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
        // `P1W2D` mixes weeks with other date units — surface a message that
        // names the actual problem, not "Invalid days" (which is what we
        // got before the explicit guard; an operator reading logs would have
        // started staring at the days value, not the structure).
        let err = parse_iso8601_duration("P1W2D").unwrap_err().to_string();
        assert!(
            err.contains("cannot mix 'W'"),
            "Expected 'cannot mix W' message, got: {err}"
        );
    }

    #[test]
    fn format_iso8601_duration_canonical_forms() {
        assert_eq!(format_iso8601_duration(300).as_deref(), Some("PT5M"));
        assert_eq!(format_iso8601_duration(3600).as_deref(), Some("PT1H"));
        assert_eq!(format_iso8601_duration(86_400).as_deref(), Some("P1D"));
        assert_eq!(format_iso8601_duration(5400).as_deref(), Some("PT1H30M"));
        assert_eq!(format_iso8601_duration(90).as_deref(), Some("PT1M30S"));
        assert_eq!(format_iso8601_duration(90_000).as_deref(), Some("P1DT1H"));
        assert_eq!(format_iso8601_duration(45).as_deref(), Some("PT45S"));
    }

    #[test]
    fn format_iso8601_duration_rejects_non_positive() {
        assert_eq!(format_iso8601_duration(0), None);
        assert_eq!(format_iso8601_duration(-300), None);
    }

    #[test]
    fn regular_step_duration_detects_regular_series() {
        let t = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        // Exactly regular.
        let times = [
            t("2024-01-01T00:00:00Z"),
            t("2024-01-01T00:05:00Z"),
            t("2024-01-01T00:10:00Z"),
        ];
        assert_eq!(regular_step_duration(&times).as_deref(), Some("PT5M"));
        // ±1 s jitter on the first interval is still regular -> mean PT1H.
        let jittered = [
            t("2024-01-01T00:00:00Z"),
            t("2024-01-01T00:59:59Z"),
            t("2024-01-01T02:00:00Z"),
            t("2024-01-01T03:00:00Z"),
        ];
        assert_eq!(regular_step_duration(&jittered).as_deref(), Some("PT1H"));
    }

    #[test]
    fn temporal_grid_picks_regular_or_irregular_variant() {
        let t = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(temporal_grid(&[]), None);
        let regular = [t("2024-01-01T00:00:00Z"), t("2024-01-01T00:05:00Z")];
        assert_eq!(
            temporal_grid(&regular),
            Some(TemporalGrid::Regular {
                cells_count: 2,
                resolution: "PT5M".to_string()
            })
        );
        let irregular = [
            t("2024-01-01T00:00:00Z"),
            t("2024-01-01T01:00:00Z"),
            t("2024-01-01T03:00:00Z"),
        ];
        assert_eq!(
            temporal_grid(&irregular),
            Some(TemporalGrid::Irregular {
                cells_count: 3,
                coordinates: vec![
                    "2024-01-01T00:00:00+00:00".to_string(),
                    "2024-01-01T01:00:00+00:00".to_string(),
                    "2024-01-01T03:00:00+00:00".to_string(),
                ]
            })
        );
    }

    #[test]
    fn regular_step_duration_rejects_irregular_and_short() {
        let t = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(regular_step_duration(&[]), None);
        assert_eq!(regular_step_duration(&[t("2024-01-01T00:00:00Z")]), None);
        // 1 h then 2 h -> irregular.
        let irregular = [
            t("2024-01-01T00:00:00Z"),
            t("2024-01-01T01:00:00Z"),
            t("2024-01-01T03:00:00Z"),
        ];
        assert_eq!(regular_step_duration(&irregular), None);
    }

    #[test]
    fn format_iso8601_duration_roundtrips_through_parser() {
        for secs in [60_i64, 300, 900, 3600, 10_800, 86_400, 90_000] {
            let formatted = format_iso8601_duration(secs).unwrap();
            assert_eq!(
                parse_iso8601_duration(&formatted).unwrap(),
                Duration::seconds(secs),
                "roundtrip failed for {secs}s -> {formatted}"
            );
        }
    }

    #[test]
    fn iso8601_duration_rejects_trailing_digits_without_unit() {
        // Without the trailing-content check, `PT12H30` (operator typo for
        // `PT12H30M`) silently parsed as 12h, dropping the 30. The
        // surrounding behaviour — start with PT, end with a unit suffix —
        // would otherwise hide the typo.
        assert!(parse_iso8601_duration("PT12H30").is_err());
        assert!(parse_iso8601_duration("PT30M5").is_err());
        assert!(parse_iso8601_duration("PT1H2M3").is_err());
        // The valid forms are still accepted.
        assert_eq!(
            parse_iso8601_duration("PT12H30M").unwrap(),
            Duration::seconds(12 * 3600 + 30 * 60)
        );
        assert_eq!(
            parse_iso8601_duration("PT1H2M3S").unwrap(),
            Duration::seconds(3600 + 120 + 3)
        );
    }
}
