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
//! 2. [`expand_prefix_for_range`] / [`expand_prefix_pattern`] —
//!    substitute the times a range touches into a strftime prefix
//!    template, yielding one literal prefix per day (or per hour, for a
//!    template naming the hour) to `list`. [`validate_prefix_pattern`]
//!    rejects a template discovery cannot expand, at config load.
//!
//! This module is the shared home for both. `engine-odim` and
//! `engine-geotiff` use it; `engine-grib` formats its run-hour
//! prefixes itself.

use std::fmt::Write as _;

use chrono::format::{Fixed, Item, Numeric, StrftimeItems};
use chrono::{DateTime, Duration, NaiveDate, Timelike, Utc};
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

    /// Upper bound on the UTC days this window can span, for callers that
    /// size a static day-count fallback from it.
    pub fn max_scan_days(&self) -> u32 {
        let days = (self.seconds.unsigned_abs() / 86_400) as u32;
        days + 2
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

/// How finely a prefix template partitions time. Each step is one
/// `list` per poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PrefixStep {
    /// No time specifiers: one fixed prefix.
    Fixed,
    /// Date specifiers only (`%Y/%m/%d`, `%Y/%j`, …): one prefix per day.
    Day,
    /// An hour specifier (`%H`): one prefix per hour.
    Hour,
}

/// The step of a strftime prefix template.
///
/// Every specifier must be one chrono can format from a UTC date-time,
/// and none may be finer than an hour: a minute-partitioned prefix would
/// need a `list` per minute of the window.
pub fn prefix_step(pattern: &str) -> Result<PrefixStep, DataServerError> {
    let invalid =
        |why: &str| DataServerError::Config(format!("Invalid prefix_pattern '{pattern}': {why}"));
    let mut step = PrefixStep::Fixed;
    for item in StrftimeItems::new(pattern) {
        let item_step = match item {
            Item::Literal(_) | Item::OwnedLiteral(_) | Item::Space(_) | Item::OwnedSpace(_) => {
                PrefixStep::Fixed
            }
            Item::Numeric(
                Numeric::Year
                | Numeric::YearDiv100
                | Numeric::YearMod100
                | Numeric::IsoYear
                | Numeric::IsoYearDiv100
                | Numeric::IsoYearMod100
                | Numeric::Quarter
                | Numeric::Month
                | Numeric::Day
                | Numeric::WeekFromSun
                | Numeric::WeekFromMon
                | Numeric::IsoWeek
                | Numeric::NumDaysFromSun
                | Numeric::WeekdayFromMon
                | Numeric::Ordinal,
                _,
            )
            | Item::Fixed(
                Fixed::ShortMonthName
                | Fixed::LongMonthName
                | Fixed::ShortWeekdayName
                | Fixed::LongWeekdayName,
            ) => PrefixStep::Day,
            Item::Numeric(Numeric::Hour | Numeric::Hour12, _)
            | Item::Fixed(Fixed::LowerAmPm | Fixed::UpperAmPm) => PrefixStep::Hour,
            Item::Error => return Err(invalid("unknown strftime specifier")),
            _ => {
                return Err(invalid(
                    "only date and hour specifiers are supported (no minutes, seconds, \
                     time zones or timestamps)",
                ))
            }
        };
        step = step.max(item_step);
    }
    Ok(step)
}

/// Validate a prefix template against the discovery window it runs
/// with, so a template discovery cannot expand fails at config load
/// rather than on the first poll.
///
/// An hourly template needs a `time_window`: without one, discovery
/// falls back to whole days, 24 `list` calls per day on every poll.
pub fn validate_prefix_pattern(
    pattern: &str,
    time_window: Option<&TimeWindow>,
) -> Result<PrefixStep, DataServerError> {
    let step = prefix_step(pattern)?;
    if step == PrefixStep::Hour && time_window.is_none() {
        return Err(DataServerError::Config(format!(
            "prefix_pattern '{pattern}' is partitioned by hour and needs a time_window"
        )));
    }
    Ok(step)
}

/// Expand a strftime prefix template over every day (or hour, for a
/// template naming the hour) that `[start, end]` touches, oldest first.
/// A prefix repeated across steps (a `%Y/%m/` template over several
/// days) is listed once.
///
/// E.g. `"%Y/%j/%H/"` from 22:30 to 00:10 the next day yields three
/// prefixes: hours 22 and 23 of the first day and hour 00 of the next.
/// A template with no time specifiers is a fixed prefix, returned as a
/// single entry.
pub fn expand_prefix_for_range(
    pattern: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<String>, DataServerError> {
    let step = prefix_step(pattern)?;
    let (mut t, stride) = match step {
        PrefixStep::Fixed | PrefixStep::Day => (
            start.date_naive().and_time(Default::default()).and_utc(),
            Duration::days(1),
        ),
        PrefixStep::Hour => (
            start
                .date_naive()
                .and_hms_opt(start.hour(), 0, 0)
                .unwrap_or_default()
                .and_utc(),
            Duration::hours(1),
        ),
    };
    let mut prefixes: Vec<String> = Vec::new();
    loop {
        let mut prefix = String::new();
        write!(prefix, "{}", t.format(pattern))
            .map_err(|_| DataServerError::Config(format!("Invalid prefix_pattern '{pattern}'")))?;
        let prefix = prefix.trim_end_matches('/').to_string();
        if !prefixes.contains(&prefix) {
            prefixes.push(prefix);
        }
        t += stride;
        if step == PrefixStep::Fixed || t > end {
            return Ok(prefixes);
        }
    }
}

/// Expand a prefix template over the most recent `scan_days` UTC days,
/// counting back from today. Fallback for callers without a
/// [`TimeWindow`]; `scan_days` is clamped to at least 1.
pub fn expand_prefix_pattern(
    pattern: &str,
    scan_days: u32,
) -> Result<Vec<String>, DataServerError> {
    let now = Utc::now();
    let first = now.date_naive() - Duration::days(i64::from(scan_days.max(1)) - 1);
    expand_prefix_for_range(pattern, first.and_time(Default::default()).and_utc(), now)
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

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, 0)
            .unwrap()
            .and_utc()
    }

    #[test]
    fn max_scan_days_covers_both_partial_days() {
        assert_eq!(TimeWindow::parse("-PT2H").unwrap().max_scan_days(), 2);
        assert_eq!(TimeWindow::parse("-P1D").unwrap().max_scan_days(), 3);
        assert_eq!(TimeWindow::parse("PT36H").unwrap().max_scan_days(), 3);
    }

    #[test]
    fn expand_static_prefix_is_single_entry() {
        let now = at(2026, 5, 15, 12, 0);
        assert_eq!(
            expand_prefix_for_range("some/fixed/prefix/", now - Duration::days(3), now).unwrap(),
            vec!["some/fixed/prefix"]
        );
        assert_eq!(
            expand_prefix_pattern("some/fixed/prefix", 5).unwrap(),
            vec!["some/fixed/prefix"]
        );
        assert_eq!(
            expand_prefix_for_range("100%%/", now, now).unwrap(),
            vec!["100%"]
        );
    }

    #[test]
    fn expand_dated_prefix_per_day_oldest_first() {
        assert_eq!(
            expand_prefix_for_range(
                "%Y/%m/%d/OPERA/COMP/",
                at(2026, 5, 14, 23, 30),
                at(2026, 5, 15, 0, 10)
            )
            .unwrap(),
            vec!["2026/05/14/OPERA/COMP", "2026/05/15/OPERA/COMP"]
        );
        // A coarser template over several days is listed once.
        assert_eq!(
            expand_prefix_for_range("%Y/%m/", at(2026, 5, 13, 0, 0), at(2026, 5, 15, 0, 0))
                .unwrap(),
            vec!["2026/05"]
        );
    }

    /// GOES-R layout: `<product>/YYYY/DOY/HH/` — the hour is a directory.
    #[test]
    fn expand_hourly_prefix_across_midnight() {
        let tw = TimeWindow::parse("-PT2H").unwrap();
        let (start, end) = tw.to_range(at(2026, 9, 26, 0, 10));
        assert_eq!(
            expand_prefix_for_range("ABI-L2-CMIPF/%Y/%j/%H/", start, end).unwrap(),
            vec![
                "ABI-L2-CMIPF/2026/268/22",
                "ABI-L2-CMIPF/2026/268/23",
                "ABI-L2-CMIPF/2026/269/00",
            ]
        );
    }

    #[test]
    fn expand_fallback_counts_back_whole_days() {
        let prefixes = expand_prefix_pattern("%Y/%m/%d/", 2).unwrap();
        let today = Utc::now().date_naive();
        assert_eq!(
            prefixes,
            vec![
                (today - Duration::days(1)).format("%Y/%m/%d").to_string(),
                today.format("%Y/%m/%d").to_string(),
            ]
        );
        assert_eq!(expand_prefix_pattern("%Y/%m/%d/", 0).unwrap().len(), 1);
    }

    #[test]
    fn prefix_step_classifies_and_rejects() {
        assert_eq!(prefix_step("radar/").unwrap(), PrefixStep::Fixed);
        assert_eq!(prefix_step("%Y/%m/%d/").unwrap(), PrefixStep::Day);
        assert_eq!(prefix_step("%F/%b/").unwrap(), PrefixStep::Day);
        assert_eq!(prefix_step("%Y/%j/%H/").unwrap(), PrefixStep::Hour);
        // Finer than an hour, or not a date-time field at all.
        for pattern in ["%Y/%H%M/", "%T/", "%s/", "%Y/%z/", "%Y/%Q/", "%Y/%.3f/"] {
            assert!(prefix_step(pattern).is_err(), "{pattern}");
        }
    }

    #[test]
    fn hourly_prefix_needs_a_time_window() {
        let tw = TimeWindow::parse("-PT3H").unwrap();
        assert_eq!(
            validate_prefix_pattern("%Y/%j/%H/", Some(&tw)).unwrap(),
            PrefixStep::Hour
        );
        assert!(validate_prefix_pattern("%Y/%j/%H/", None).is_err());
        assert_eq!(
            validate_prefix_pattern("%Y/%m/%d/", None).unwrap(),
            PrefixStep::Day
        );
    }
}
