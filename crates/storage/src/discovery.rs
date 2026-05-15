//! Time-windowed prefix discovery for date-partitioned object stores.
//!
//! S3/HTTP buckets that hold time-series data (radar composites, NWP
//! runs) almost always partition objects under a date-templated key
//! prefix such as `%Y/%m/%d/OPERA/COMP/`. Two pieces of logic recur
//! across every engine that polls such a bucket:
//!
//! 1. [`TimeWindow`] — parse an ISO 8601 duration (`-PT12H`, `-P2D`)
//!    and turn "now" into the concrete `(start, end)` range and the
//!    set of UTC dates that range touches.
//! 2. [`expand_prefix_for_dates`] / [`expand_prefix_pattern`] —
//!    substitute those dates into a strftime prefix template, yielding
//!    one literal prefix per day to `list`.
//!
//! This module is the shared home for both. `engine-odim` uses it
//! today; `engine-geotiff` and `engine-grib` carry their own copies
//! and are tracked for migration.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use ds_core::error::DataServerError;

/// A signed ISO 8601 duration describing how far back (or forward)
/// from "now" a collection's useful data extends.
#[derive(Debug, Clone)]
pub struct TimeWindow {
    /// Duration in seconds. Negative = into the past, positive = future.
    seconds: i64,
}

impl TimeWindow {
    /// Parse an ISO 8601 duration string.
    ///
    /// Supports `P[nD]T[nH][nM][nS]` with an optional leading `-` for
    /// past-facing windows. Examples: `-PT2H`, `PT30M`, `-P1DT6H`.
    pub fn parse(s: &str) -> Result<Self, DataServerError> {
        let (negative, rest) = match s.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, s),
        };

        let rest = rest.strip_prefix('P').ok_or_else(|| {
            DataServerError::Config(format!(
                "Invalid time_window '{s}': must start with 'P' or '-P'"
            ))
        })?;

        let (date_part, time_part) = match rest.find('T') {
            Some(t) => (&rest[..t], &rest[t + 1..]),
            None => (rest, ""),
        };

        let mut total_seconds: i64 = 0;

        if !date_part.is_empty() {
            total_seconds += parse_component(date_part, 'D', s)? * 86_400;
        }

        if !time_part.is_empty() {
            let mut remaining = time_part;
            if let Some(pos) = remaining.find('H') {
                total_seconds += parse_int(&remaining[..pos], "hours", s)? * 3_600;
                remaining = &remaining[pos + 1..];
            }
            if let Some(pos) = remaining.find('M') {
                total_seconds += parse_int(&remaining[..pos], "minutes", s)? * 60;
                remaining = &remaining[pos + 1..];
            }
            if let Some(pos) = remaining.find('S') {
                total_seconds += parse_int(&remaining[..pos], "seconds", s)?;
                remaining = &remaining[pos + 1..];
            }
            // Reject anything left over — e.g. `PT2H5` (a bare `5`
            // with no unit) or `PT2H30M5SFOO` — rather than silently
            // dropping it and returning a partial duration.
            if !remaining.is_empty() {
                return Err(DataServerError::Config(format!(
                    "Invalid time_window '{s}': unexpected trailing characters '{remaining}'"
                )));
            }
        }

        if total_seconds == 0 {
            return Err(DataServerError::Config(format!(
                "Invalid time_window '{s}': zero duration"
            )));
        }

        if negative {
            total_seconds = -total_seconds;
        }

        Ok(TimeWindow {
            seconds: total_seconds,
        })
    }

    /// Compute the `(start, end)` range relative to `now`.
    ///
    /// Past windows (`seconds < 0`) return `(now + seconds, now)`;
    /// future windows return `(now, now + seconds)`.
    pub fn to_range(&self, now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
        let offset = Duration::seconds(self.seconds);
        if self.seconds < 0 {
            (now + offset, now)
        } else {
            (now, now + offset)
        }
    }

    /// The distinct UTC dates the window spans — one per day inclusive
    /// of both ends. These are the dates that need prefix expansion.
    pub fn scan_dates(&self, now: DateTime<Utc>) -> Vec<NaiveDate> {
        let (start, end) = self.to_range(now);
        let mut dates = Vec::new();
        let mut d = start.date_naive();
        let last = end.date_naive();
        while d <= last {
            dates.push(d);
            d += Duration::days(1);
        }
        dates
    }
}

fn parse_component(s: &str, suffix: char, original: &str) -> Result<i64, DataServerError> {
    let stripped = s.strip_suffix(suffix).ok_or_else(|| {
        DataServerError::Config(format!(
            "Invalid date component in time_window '{original}'"
        ))
    })?;
    parse_int(stripped, "days", original)
}

fn parse_int(s: &str, field: &str, original: &str) -> Result<i64, DataServerError> {
    s.parse().map_err(|_| {
        DataServerError::Config(format!("Invalid {field} in time_window '{original}'"))
    })
}

/// Expand a strftime prefix template for a specific set of dates.
///
/// E.g. `"%Y/%m/%d/OPERA/COMP/"` with `[2026-05-15, 2026-05-14]`
/// yields `["2026/05/15/OPERA/COMP", "2026/05/14/OPERA/COMP"]`.
///
/// A pattern with no `%` is a fixed prefix — returned as a single
/// entry regardless of `dates`.
pub fn expand_prefix_for_dates(pattern: &str, dates: &[NaiveDate]) -> Vec<String> {
    if !pattern.contains('%') {
        return vec![pattern.trim_end_matches('/').to_string()];
    }
    dates
        .iter()
        .map(|date| {
            date.format(pattern)
                .to_string()
                .trim_end_matches('/')
                .to_string()
        })
        .collect()
}

/// Expand a prefix template over the most recent `scan_days` days,
/// counting back from today (UTC). Fallback for callers without a
/// [`TimeWindow`]; `scan_days` is clamped to at least 1.
pub fn expand_prefix_pattern(pattern: &str, scan_days: u32) -> Vec<String> {
    let today = Utc::now().date_naive();
    let dates: Vec<_> = (0..scan_days.max(1))
        .map(|offset| today - Duration::days(offset as i64))
        .collect();
    expand_prefix_for_dates(pattern, &dates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_past_and_future() {
        assert_eq!(TimeWindow::parse("-PT2H").unwrap().seconds, -7_200);
        assert_eq!(TimeWindow::parse("PT6H").unwrap().seconds, 21_600);
        assert_eq!(TimeWindow::parse("-PT30M").unwrap().seconds, -1_800);
        assert_eq!(TimeWindow::parse("-P2D").unwrap().seconds, -172_800);
        assert_eq!(
            TimeWindow::parse("-P1DT6H").unwrap().seconds,
            -(86_400 + 21_600)
        );
        assert_eq!(
            TimeWindow::parse("-PT2H30M").unwrap().seconds,
            -(7_200 + 1_800)
        );
    }

    #[test]
    fn invalid_windows_rejected() {
        assert!(TimeWindow::parse("PT0H").is_err());
        assert!(TimeWindow::parse("2H").is_err());
        assert!(TimeWindow::parse("").is_err());
        assert!(TimeWindow::parse("P").is_err());
        // Trailing characters must not be silently dropped.
        assert!(TimeWindow::parse("PT2H5").is_err());
        assert!(TimeWindow::parse("PT2H30M5SFOO").is_err());
        assert!(TimeWindow::parse("PT2HFOO").is_err());
    }

    #[test]
    fn scan_dates_spans_window() {
        let tw = TimeWindow::parse("-PT2H").unwrap();
        // 15:00 - 2h stays inside one day.
        let midday = NaiveDate::from_ymd_opt(2026, 5, 15)
            .unwrap()
            .and_hms_opt(15, 0, 0)
            .unwrap()
            .and_utc();
        assert_eq!(tw.scan_dates(midday).len(), 1);
        // 01:00 - 2h crosses midnight into the previous day.
        let early = NaiveDate::from_ymd_opt(2026, 5, 15)
            .unwrap()
            .and_hms_opt(1, 0, 0)
            .unwrap()
            .and_utc();
        let dates = tw.scan_dates(early);
        assert_eq!(dates.len(), 2);
        assert_eq!(dates[0], NaiveDate::from_ymd_opt(2026, 5, 14).unwrap());
        assert_eq!(dates[1], NaiveDate::from_ymd_opt(2026, 5, 15).unwrap());
    }

    #[test]
    fn expand_static_prefix_is_single_entry() {
        assert_eq!(
            expand_prefix_for_dates("some/fixed/prefix/", &[]),
            vec!["some/fixed/prefix"]
        );
        assert_eq!(
            expand_prefix_pattern("some/fixed/prefix", 5),
            vec!["some/fixed/prefix"]
        );
    }

    #[test]
    fn expand_dated_prefix() {
        let dates = [
            NaiveDate::from_ymd_opt(2026, 5, 15).unwrap(),
            NaiveDate::from_ymd_opt(2026, 5, 14).unwrap(),
        ];
        assert_eq!(
            expand_prefix_for_dates("%Y/%m/%d/OPERA/COMP/", &dates),
            vec!["2026/05/15/OPERA/COMP", "2026/05/14/OPERA/COMP"]
        );
    }
}
