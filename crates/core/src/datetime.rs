use chrono::{DateTime, Utc};

use crate::error::DataServerError;

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
}
