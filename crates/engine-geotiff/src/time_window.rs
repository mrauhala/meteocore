//! ISO 8601 duration parsing and time window computation.

use chrono::{DateTime, Duration, Utc};
use ds_core::error::DataServerError;

/// Parsed time window: a duration that can be negative (past) or positive (future).
#[derive(Debug, Clone)]
pub struct TimeWindow {
    /// Duration in seconds. Negative = past, positive = future.
    seconds: i64,
}

impl TimeWindow {
    /// Parse an ISO 8601 duration string.
    ///
    /// Supports: `P[nD]T[nH][nM][nS]` with optional leading `-` for past.
    /// Examples: `-PT2H`, `PT30M`, `-P1DT6H`, `PT2H30M`, `-PT2H`
    pub fn parse(s: &str) -> Result<Self, DataServerError> {
        let (negative, rest) = if let Some(r) = s.strip_prefix('-') {
            (true, r)
        } else {
            (false, s)
        };

        let rest = rest.strip_prefix('P').ok_or_else(|| {
            DataServerError::Config(format!(
                "Invalid time_window '{}': must start with 'P' or '-P'",
                s
            ))
        })?;

        let (date_part, time_part) = if let Some(t_pos) = rest.find('T') {
            (&rest[..t_pos], &rest[t_pos + 1..])
        } else {
            (rest, "")
        };

        let mut total_seconds: i64 = 0;

        // Parse date part (only D supported)
        if !date_part.is_empty() {
            total_seconds += parse_component(date_part, 'D', s)? * 86400;
        }

        // Parse time part (H, M, S)
        if !time_part.is_empty() {
            let mut remaining = time_part;
            if let Some(pos) = remaining.find('H') {
                let val: i64 = remaining[..pos].parse().map_err(|_| {
                    DataServerError::Config(format!("Invalid hours in time_window '{}'", s))
                })?;
                total_seconds += val * 3600;
                remaining = &remaining[pos + 1..];
            }
            if let Some(pos) = remaining.find('M') {
                let val: i64 = remaining[..pos].parse().map_err(|_| {
                    DataServerError::Config(format!("Invalid minutes in time_window '{}'", s))
                })?;
                total_seconds += val * 60;
                remaining = &remaining[pos + 1..];
            }
            if let Some(pos) = remaining.find('S') {
                let val: i64 = remaining[..pos].parse().map_err(|_| {
                    DataServerError::Config(format!("Invalid seconds in time_window '{}'", s))
                })?;
                total_seconds += val;
            }
        }

        if total_seconds == 0 {
            return Err(DataServerError::Config(format!(
                "Invalid time_window '{}': zero duration",
                s
            )));
        }

        if negative {
            total_seconds = -total_seconds;
        }

        Ok(TimeWindow {
            seconds: total_seconds,
        })
    }

    /// Compute the (start, end) time range relative to now.
    /// For negative durations: `(now + seconds, now)` (seconds is negative)
    /// For positive durations: `(now, now + seconds)`
    pub fn to_range(&self, now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
        let offset = Duration::seconds(self.seconds);
        if self.seconds < 0 {
            (now + offset, now)
        } else {
            (now, now + offset)
        }
    }

    /// Compute the distinct dates that need prefix expansion.
    /// Based on the actual time range (now ± duration), not a fixed day count.
    /// E.g. at 15:00 UTC with `-PT2H`, only today is needed.
    /// At 01:00 UTC with `-PT2H`, both today and yesterday are needed.
    pub fn scan_dates(&self, now: DateTime<Utc>) -> Vec<chrono::NaiveDate> {
        let (start, end) = self.to_range(now);
        let start_date = start.date_naive();
        let end_date = end.date_naive();

        let mut dates = Vec::new();
        let mut d = start_date;
        while d <= end_date {
            dates.push(d);
            d += chrono::Duration::days(1);
        }
        dates
    }

    /// Maximum number of days this window could span (for config validation / static fallback).
    pub fn max_scan_days(&self) -> u32 {
        let hours = self.seconds.unsigned_abs() / 3600;
        let days = (hours / 24) as u32;
        (days + 2).max(2)
    }
}

fn parse_component(s: &str, suffix: char, original: &str) -> Result<i64, DataServerError> {
    let stripped = s.strip_suffix(suffix).ok_or_else(|| {
        DataServerError::Config(format!(
            "Invalid date component in time_window '{}'",
            original
        ))
    })?;
    stripped.parse().map_err(|_| {
        DataServerError::Config(format!("Invalid number in time_window '{}'", original))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_past_2_hours() {
        let tw = TimeWindow::parse("-PT2H").unwrap();
        assert_eq!(tw.seconds, -7200);
        assert_eq!(tw.max_scan_days(), 2);
    }

    #[test]
    fn parse_future_6_hours() {
        let tw = TimeWindow::parse("PT6H").unwrap();
        assert_eq!(tw.seconds, 21600);
        assert_eq!(tw.max_scan_days(), 2);
    }

    #[test]
    fn parse_past_30_minutes() {
        let tw = TimeWindow::parse("-PT30M").unwrap();
        assert_eq!(tw.seconds, -1800);
        assert_eq!(tw.max_scan_days(), 2);
    }

    #[test]
    fn parse_past_2_days() {
        let tw = TimeWindow::parse("-P2D").unwrap();
        assert_eq!(tw.seconds, -172800);
        assert_eq!(tw.max_scan_days(), 4);
    }

    #[test]
    fn parse_combined() {
        let tw = TimeWindow::parse("-P1DT6H").unwrap();
        assert_eq!(tw.seconds, -(86400 + 21600));
        assert_eq!(tw.max_scan_days(), 3);
    }

    #[test]
    fn parse_hours_and_minutes() {
        let tw = TimeWindow::parse("-PT2H30M").unwrap();
        assert_eq!(tw.seconds, -(7200 + 1800));
    }

    #[test]
    fn range_past() {
        let tw = TimeWindow::parse("-PT2H").unwrap();
        let now = Utc::now();
        let (start, end) = tw.to_range(now);
        assert!(start < end);
        assert_eq!(end, now);
        assert_eq!((end - start).num_seconds(), 7200);
    }

    #[test]
    fn range_future() {
        let tw = TimeWindow::parse("PT6H").unwrap();
        let now = Utc::now();
        let (start, end) = tw.to_range(now);
        assert!(start < end);
        assert_eq!(start, now);
        assert_eq!((end - start).num_seconds(), 21600);
    }

    #[test]
    fn zero_duration_rejected() {
        assert!(TimeWindow::parse("PT0H").is_err());
        assert!(TimeWindow::parse("P0D").is_err());
    }

    #[test]
    fn invalid_format_rejected() {
        assert!(TimeWindow::parse("2H").is_err());
        assert!(TimeWindow::parse("").is_err());
        assert!(TimeWindow::parse("P").is_err());
    }

    #[test]
    fn scan_dates_midday_only_today() {
        use chrono::NaiveDate;
        let tw = TimeWindow::parse("-PT2H").unwrap();
        // 15:00 UTC - 2h = 13:00 UTC, same day
        let now = NaiveDate::from_ymd_opt(2026, 3, 25)
            .unwrap()
            .and_hms_opt(15, 0, 0)
            .unwrap()
            .and_utc();
        let dates = tw.scan_dates(now);
        assert_eq!(dates.len(), 1, "At 15:00, -PT2H should only scan today");
        assert_eq!(dates[0], NaiveDate::from_ymd_opt(2026, 3, 25).unwrap());
    }

    #[test]
    fn scan_dates_early_morning_includes_yesterday() {
        use chrono::NaiveDate;
        let tw = TimeWindow::parse("-PT2H").unwrap();
        // 01:00 UTC - 2h = 23:00 yesterday
        let now = NaiveDate::from_ymd_opt(2026, 3, 25)
            .unwrap()
            .and_hms_opt(1, 0, 0)
            .unwrap()
            .and_utc();
        let dates = tw.scan_dates(now);
        assert_eq!(
            dates.len(),
            2,
            "At 01:00, -PT2H should scan yesterday+today"
        );
        assert_eq!(dates[0], NaiveDate::from_ymd_opt(2026, 3, 24).unwrap());
        assert_eq!(dates[1], NaiveDate::from_ymd_opt(2026, 3, 25).unwrap());
    }

    #[test]
    fn scan_dates_midnight_exact() {
        use chrono::NaiveDate;
        let tw = TimeWindow::parse("-PT2H").unwrap();
        // 00:00 UTC - 2h = 22:00 yesterday
        let now = NaiveDate::from_ymd_opt(2026, 3, 25)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let dates = tw.scan_dates(now);
        assert_eq!(dates.len(), 2);
    }
}
