//! Time-windowed file discovery for date-partitioned object stores and
//! directories.
//!
//! S3/HTTP buckets that hold time-series data (radar composites, NWP
//! runs) almost always partition objects under a date-templated key
//! prefix such as `%Y/%m/%d/OPERA/COMP/`, and name each file after the
//! time it holds. Three pieces of logic recur across every engine that
//! polls such a source:
//!
//! 1. [`TimeWindow`] — parse an ISO 8601 duration (`-PT12H`, `-P2D`)
//!    and turn "now" into the concrete `(start, end)` range and the
//!    set of UTC dates that range touches.
//! 2. [`expand_prefix_for_range`] / [`expand_prefix_pattern`] —
//!    substitute the times a range touches into a strftime prefix
//!    template, yielding one literal prefix per day (or per hour, for a
//!    template naming the hour) to `list`. [`validate_prefix_pattern`]
//!    rejects a template discovery cannot expand, at config load.
//! 3. [`FilenameMatcher`] — recognise a collection's files by name and
//!    read the timestamp each name encodes, from a strftime filename
//!    template or an explicit regex (#817).
//! 4. [`scan_remote`] / [`scan_local`] — the catalog scan: list the
//!    prefixes, at most [`MAX_CONCURRENT_LISTS`] at a time, or read the
//!    directory, then skip hidden names (#1009), exclude, match, window,
//!    dedup and cap into `(key, timestamp)` entries (#817). [`list_prefixes`] is its bounded
//!    concurrent LIST, for an engine that recognises files without a
//!    matcher.
//!
//! This module is the shared home for all four. `engine-odim` and
//! `engine-geotiff` use it; `engine-grib` expands its model-run
//! prefixes, a strftime date plus a `{run}` hour, with
//! [`expand_run_prefixes`].

use std::fmt::Write as _;
use std::fs::Metadata;
use std::path::{Path, PathBuf};

use chrono::format::{Fixed, Item, Numeric, StrftimeItems};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Timelike, Utc};
use ds_core::error::DataServerError;
use ds_core::temp_files;
use object_store::path::Path as ObjectPath;
use object_store::ObjectMeta;
use regex::Regex;

use crate::DataStore;

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

/// Longest `time_window` an hourly prefix template may run with. Each hour
/// is one `list` per poll, at most [`MAX_CONCURRENT_LISTS`] in flight, so
/// this caps a poll at 26 calls. A longer window belongs on a day-level
/// template: listing a day prefix is recursive and covers all its hours.
pub const MAX_HOURLY_PREFIX_WINDOW_HOURS: i64 = 24;

/// Validate a prefix template against the discovery window it runs
/// with, so a template discovery cannot expand fails at config load
/// rather than on the first poll.
///
/// An hourly template needs a `time_window` of at most
/// [`MAX_HOURLY_PREFIX_WINDOW_HOURS`]: without one, discovery falls back
/// to whole days, and a long one multiplies the `list` calls per poll.
pub fn validate_prefix_pattern(
    pattern: &str,
    time_window: Option<&TimeWindow>,
) -> Result<PrefixStep, DataServerError> {
    let step = prefix_step(pattern)?;
    if step == PrefixStep::Hour {
        match time_window {
            None => {
                return Err(DataServerError::Config(format!(
                    "prefix_pattern '{pattern}' is partitioned by hour and needs a time_window"
                )))
            }
            Some(tw) if tw.seconds.abs() > MAX_HOURLY_PREFIX_WINDOW_HOURS * 3_600 => {
                return Err(DataServerError::Config(format!(
                    "prefix_pattern '{pattern}' is partitioned by hour, which lists one \
                     prefix per hour on every poll; its time_window may span at most \
                     {MAX_HOURLY_PREFIX_WINDOW_HOURS} hours. Use a day-level prefix \
                     (e.g. '%Y/%j/') for a longer window"
                )))
            }
            Some(_) => {}
        }
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

// ---------------------------------------------------------------------------
// Model-run prefixes: `{run}` (engine-grib, #817)
// ---------------------------------------------------------------------------

/// The placeholder a prefix template names a model run's hour with, as in
/// ECMWF's `%Y%m%d/{run}z/ifs/0p25/oper/`. Each run hour is substituted as
/// two digits (`00`, `06`, …). The date part stays strftime: to strftime,
/// `{run}` is literal text.
pub const RUN_PLACEHOLDER: &str = "{run}";

/// The prefix one model run's files are listed under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPrefix {
    /// The run's reference time: its day at its run hour, or the day's
    /// 00 UTC for a template without [`RUN_PLACEHOLDER`].
    pub reference_time: DateTime<Utc>,
    pub prefix: String,
}

/// Validate a model-run prefix template and the run hours substituted into
/// it, at config load.
///
/// On top of the strftime rules of [`prefix_step`]:
///
/// - No hour specifier. The run hour comes from [`RUN_PLACEHOLDER`]; a
///   `%H` would name the hour of the poll, not of the run.
/// - No `{` or `}` outside [`RUN_PLACEHOLDER`]. A misspelt placeholder
///   such as `{RUN}` or `{run` would otherwise be listed literally and
///   match nothing.
/// - With the placeholder, at least one run hour, each 0–23.
///
/// `run_hours` is not used by a template without the placeholder, so it is
/// not checked then.
pub fn validate_run_prefix_pattern(
    pattern: &str,
    run_hours: &[u32],
) -> Result<PrefixStep, DataServerError> {
    let invalid =
        |why: &str| DataServerError::Config(format!("Invalid prefix_pattern '{pattern}': {why}"));
    let step = prefix_step(pattern)?;
    if step == PrefixStep::Hour {
        return Err(invalid(&format!(
            "the model run hour goes in {RUN_PLACEHOLDER}, not in a strftime hour specifier"
        )));
    }
    if pattern.replace(RUN_PLACEHOLDER, "").contains(['{', '}']) {
        return Err(invalid(&format!(
            "'{{' and '}}' are only allowed as the {RUN_PLACEHOLDER} placeholder"
        )));
    }
    if pattern.contains(RUN_PLACEHOLDER) {
        if run_hours.is_empty() {
            return Err(invalid(&format!(
                "{RUN_PLACEHOLDER} needs at least one run hour"
            )));
        }
        if let Some(hour) = run_hours.iter().find(|&&h| h > 23) {
            return Err(invalid(&format!(
                "run hour {hour} is not an hour of the day (0-23)"
            )));
        }
    }
    Ok(step)
}

/// Expand a model-run prefix template over every day `[start, end]`
/// touches, one prefix per run hour, newest run first.
///
/// Each day's prefix comes from [`expand_prefix_for_range`], so it is
/// formatted and trimmed exactly like every other expanded prefix; then
/// [`RUN_PLACEHOLDER`] is substituted with each run hour. A run whose
/// reference time is after `end` is skipped: with `end` = now, it cannot
/// have been published yet. A template without the placeholder is one
/// prefix per day, ordered by the day's 00 UTC, and ignores `run_hours`.
///
/// A prefix repeated across runs, such as a template with no day specifier,
/// is listed once, under its newest run.
///
/// E.g. `%Y%m%d/{run}z/ifs/0p25/oper/` with run hours 0 and 12, from
/// 00:00 on 5 April to 09:00 on 6 April, yields `20260406/00z/ifs/0p25/oper`,
/// `20260405/12z/ifs/0p25/oper` and `20260405/00z/ifs/0p25/oper`.
pub fn expand_run_prefixes(
    pattern: &str,
    run_hours: &[u32],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<RunPrefix>, DataServerError> {
    validate_run_prefix_pattern(pattern, run_hours)?;
    let has_run = pattern.contains(RUN_PLACEHOLDER);
    let hours = if has_run { run_hours } else { &[0][..] };
    let mut runs = Vec::new();
    let mut day = start.date_naive();
    while day <= end.date_naive() {
        let midnight = day.and_time(chrono::NaiveTime::MIN).and_utc();
        // One entry: a single instant of a template no finer than a day.
        for day_prefix in expand_prefix_for_range(pattern, midnight, midnight)? {
            for &hour in hours {
                let reference_time = midnight + Duration::hours(i64::from(hour));
                if reference_time > end {
                    continue;
                }
                let prefix = if has_run {
                    day_prefix.replace(RUN_PLACEHOLDER, &format!("{hour:02}"))
                } else {
                    day_prefix.clone()
                };
                runs.push(RunPrefix {
                    reference_time,
                    prefix,
                });
            }
        }
        let Some(next) = day.succ_opt() else { break };
        day = next;
    }
    // Newest first; a repeated prefix keeps its newest run.
    runs.sort_by_key(|run| std::cmp::Reverse(run.reference_time));
    let mut seen = std::collections::HashSet::new();
    runs.retain(|run| seen.insert(run.prefix.clone()));
    Ok(runs)
}

#[cfg(test)]
mod run_prefix_tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, 0)
            .unwrap()
            .and_utc()
    }

    fn expand(
        pattern: &str,
        run_hours: &[u32],
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Vec<(String, String)> {
        expand_run_prefixes(pattern, run_hours, start, end)
            .unwrap()
            .into_iter()
            .map(|run| {
                (
                    run.reference_time.format("%m-%dT%H").to_string(),
                    run.prefix,
                )
            })
            .collect()
    }

    fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
        expected
            .iter()
            .map(|(t, p)| (t.to_string(), p.to_string()))
            .collect()
    }

    const IFS: &str = "%Y%m%d/{run}z/ifs/0p25/oper/";

    #[test]
    fn runs_newest_first_skipping_future_ones() {
        assert_eq!(
            expand(
                IFS,
                &[0, 6, 12, 18],
                at(2026, 4, 5, 0, 0),
                at(2026, 4, 6, 15, 0)
            ),
            pairs(&[
                ("04-06T12", "20260406/12z/ifs/0p25/oper"),
                ("04-06T06", "20260406/06z/ifs/0p25/oper"),
                ("04-06T00", "20260406/00z/ifs/0p25/oper"),
                ("04-05T18", "20260405/18z/ifs/0p25/oper"),
                ("04-05T12", "20260405/12z/ifs/0p25/oper"),
                ("04-05T06", "20260405/06z/ifs/0p25/oper"),
                ("04-05T00", "20260405/00z/ifs/0p25/oper"),
            ])
        );
        // Run hours may be listed in any order.
        assert_eq!(
            expand(IFS, &[12, 0], at(2026, 4, 5, 0, 0), at(2026, 4, 6, 9, 0)),
            pairs(&[
                ("04-06T00", "20260406/00z/ifs/0p25/oper"),
                ("04-05T12", "20260405/12z/ifs/0p25/oper"),
                ("04-05T00", "20260405/00z/ifs/0p25/oper"),
            ])
        );
    }

    /// A run is listed from its reference time on, inclusive, including
    /// the 00 UTC run at exactly midnight.
    #[test]
    fn run_at_end_is_included() {
        let end = at(2026, 4, 6, 0, 0);
        assert_eq!(
            expand(IFS, &[0, 18], at(2026, 4, 5, 0, 0), end)[0],
            (
                "04-06T00".to_string(),
                "20260406/00z/ifs/0p25/oper".to_string()
            )
        );
        assert_eq!(
            expand(IFS, &[6], at(2026, 4, 6, 0, 0), end),
            Vec::<(String, String)>::new()
        );
    }

    /// The date part follows each run's own day across month, year and
    /// leap-day boundaries.
    #[test]
    fn date_part_crosses_month_and_year() {
        let gfs = "gfs.%Y%m%d/{run}/atmos/";
        assert_eq!(
            expand(gfs, &[0, 18], at(2028, 2, 29, 0, 0), at(2028, 3, 1, 1, 0)),
            pairs(&[
                ("03-01T00", "gfs.20280301/00/atmos"),
                ("02-29T18", "gfs.20280229/18/atmos"),
                ("02-29T00", "gfs.20280229/00/atmos"),
            ])
        );
        assert_eq!(
            expand(gfs, &[12], at(2026, 12, 31, 0, 0), at(2027, 1, 1, 23, 0)),
            pairs(&[
                ("01-01T12", "gfs.20270101/12/atmos"),
                ("12-31T12", "gfs.20261231/12/atmos"),
            ])
        );
    }

    /// Without `{run}`, a template is one prefix per day under the day's
    /// 00 UTC, and the run hours are not used.
    #[test]
    fn template_without_run_is_one_prefix_per_day() {
        assert_eq!(
            expand(
                "%Y%m%d/00z/ifs/0p25/oper/",
                &[0, 6, 12, 18],
                at(2026, 4, 5, 0, 0),
                at(2026, 4, 6, 15, 0)
            ),
            pairs(&[
                ("04-06T00", "20260406/00z/ifs/0p25/oper"),
                ("04-05T00", "20260405/00z/ifs/0p25/oper"),
            ])
        );
        assert_eq!(
            expand(
                "%Y%m%d/00z/",
                &[],
                at(2026, 4, 6, 0, 0),
                at(2026, 4, 6, 1, 0)
            ),
            pairs(&[("04-06T00", "20260406/00z")])
        );
    }

    /// A prefix that does not change from day to day, or a run hour given
    /// twice, is listed once, under its newest run.
    #[test]
    fn repeated_prefix_is_listed_once() {
        let (start, end) = (at(2026, 4, 5, 0, 0), at(2026, 4, 6, 15, 0));
        assert_eq!(
            expand("latest/{run}/", &[0, 12], start, end),
            pairs(&[("04-06T12", "latest/12"), ("04-06T00", "latest/00")])
        );
        assert_eq!(
            expand("static/", &[0], start, end),
            pairs(&[("04-06T00", "static")])
        );
        assert_eq!(
            expand("%Y%m/{run}/", &[6, 6], start, end),
            pairs(&[("04-06T06", "202604/06")])
        );
    }

    #[test]
    fn empty_or_reversed_range_is_empty() {
        let t = at(2026, 4, 6, 12, 0);
        assert!(expand_run_prefixes(IFS, &[0], t, t - Duration::days(2))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn run_templates_validate() {
        for ok in [
            IFS,
            "%Y%m%d/{run}z/aifs-single/0p25/oper/",
            "gfs.%Y%m%d/{run}/atmos/",
            "%Y%m%d/00z/ifs/0p25/oper/",
            "%Y/%m/%d/{run}/{run}/",
            "static/",
        ] {
            validate_run_prefix_pattern(ok, &[0, 6, 12, 18])
                .unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        // Unknown and sub-hour specifiers, as for every prefix template.
        for bad in ["%Y%m%d/%!/", "%Y%m%d/%M/{run}/"] {
            assert!(validate_run_prefix_pattern(bad, &[0]).is_err(), "{bad}");
        }
        // The run hour is `{run}`, never `%H`.
        let err = validate_run_prefix_pattern("%Y%m%d/%H/", &[0]).unwrap_err();
        assert!(err.to_string().contains("{run}"), "{err}");
        // A misspelt placeholder would be listed literally.
        for bad in ["%Y%m%d/{RUN}z/", "%Y%m%d/{run/", "%Y%m%d/run}/", "{{run}}/"] {
            assert!(validate_run_prefix_pattern(bad, &[0]).is_err(), "{bad}");
        }
        // Run hours are checked only when `{run}` uses them.
        assert!(validate_run_prefix_pattern(IFS, &[]).is_err());
        assert!(validate_run_prefix_pattern(IFS, &[0, 24]).is_err());
        assert!(validate_run_prefix_pattern("%Y%m%d/00z/", &[]).is_ok());
        assert!(validate_run_prefix_pattern("%Y%m%d/00z/", &[24]).is_ok());
        // Expansion applies the same rules.
        let t = at(2026, 4, 6, 12, 0);
        assert!(expand_run_prefixes(IFS, &[24], t, t).is_err());
        assert!(expand_run_prefixes("%Y%m%d/%H/", &[0], t, t).is_err());
    }
}

/// The strftime specifiers a filename template may use, each with the
/// fixed-width digits it matches.
const TEMPLATE_CODES: [(char, &str); 7] = [
    ('Y', r"\d{4}"),
    ('m', r"\d{2}"),
    ('d', r"\d{2}"),
    ('H', r"\d{2}"),
    ('M', r"\d{2}"),
    ('S', r"\d{2}"),
    ('j', r"\d{3}"),
];

/// Literals that stay inside the timestamp capture when another code
/// follows them, like the `T` in `%Y%m%dT%H%M`. A `Z` straight after the
/// last code also stays inside, as the UTC marker.
const TIMESTAMP_SEPARATORS: [char; 5] = ['T', '-', ':', '_', 'Z'];

/// Why a filename template or pattern cannot build a [`FilenameMatcher`].
#[derive(Debug, thiserror::Error)]
pub enum FilenameError {
    #[error("filename_template `{template}` contains no strftime codes — at least one of %Y/%m/%d/%H/%M/%S/%j is required")]
    NoStrftimeCodes { template: String },
    #[error("filename_template `{template}` contains unknown strftime code `{code}`")]
    UnknownCode { template: String, code: String },
    #[error(
        "filename_template `{template}` has non-contiguous strftime codes (more than one block of date/time codes separated by literal text — e.g. `%Y_STATION_%H%M.h5`). \
         The template parser expects all strftime codes to form a single block. Use the explicit `filename_pattern` + `timestamp_format` config form for split layouts."
    )]
    SplitTimestamp { template: String },
    #[error("invalid regex `{pattern}`: {source}")]
    InvalidRegex {
        pattern: String,
        #[source]
        source: regex::Error,
    },
    #[error("filename_pattern `{pattern}` is missing the required `(?P<timestamp>…)` named capture group")]
    NoTimestampCapture { pattern: String },
}

/// Recognises a collection's data files by name and reads the timestamp
/// each name encodes. Build it once when the engine is constructed and
/// reuse it on every poll.
///
/// It is built one of two ways:
///
/// - [`from_template`](Self::from_template): a strftime template such as
///   `radar_%Y%m%dT%H%MZ.tif`. The template is anchored `^…$`, so only a
///   whole basename matches. A partial upload (`….tif.tmp`, `….tif.part`)
///   or a longer name that merely contains a match never does.
/// - [`from_pattern`](Self::from_pattern): an explicit regex with a
///   `timestamp` named capture plus the chrono format of that capture, for
///   layouts a template cannot express. The regex is used as written; one
///   that is not anchored `^…$` is logged at WARN, since it admits partial
///   uploads.
///
/// Match basenames, not object keys or paths: a template describes a file
/// name.
#[derive(Debug, Clone)]
pub struct FilenameMatcher {
    regex: Regex,
    timestamp_format: String,
}

impl FilenameMatcher {
    /// Build a matcher from a strftime filename template.
    ///
    /// The codes `%Y %m %d %H %M %S %j` become fixed-width digit runs, and
    /// the block they form becomes the `timestamp` capture. Separators
    /// between codes (`T - : _`) and a trailing `Z` stay in the capture, so
    /// they round-trip through the chrono format. The codes must form one
    /// contiguous block: `%Y_STATION_%H%M.h5` is an error, which the
    /// explicit [`from_pattern`](Self::from_pattern) form can express.
    pub fn from_template(template: &str) -> Result<Self, FilenameError> {
        let (pattern, timestamp_format) = expand_template(template)?;
        let regex = Regex::new(&pattern)
            .map_err(|source| FilenameError::InvalidRegex { pattern, source })?;
        Ok(Self {
            regex,
            timestamp_format,
        })
    }

    /// Build a matcher from an explicit regex with a `timestamp` named
    /// capture, and the chrono format that parses the captured text.
    ///
    /// **The pattern is not auto-anchored.** An unanchored pattern matches
    /// any substring of a filename, including a partial upload such as
    /// `radar.h5.tmp`, which a scan would then serve as a valid, possibly
    /// half-written, timestep. Such a pattern is accepted but logged at
    /// WARN; include `^` and `$` unless that is deliberate.
    pub fn from_pattern(pattern: &str, timestamp_format: &str) -> Result<Self, FilenameError> {
        let regex = Regex::new(pattern).map_err(|source| FilenameError::InvalidRegex {
            pattern: pattern.to_string(),
            source,
        })?;
        if !regex
            .capture_names()
            .flatten()
            .any(|name| name == "timestamp")
        {
            return Err(FilenameError::NoTimestampCapture {
                pattern: pattern.to_string(),
            });
        }
        if !pattern.starts_with('^') || !pattern.ends_with('$') {
            tracing::warn!(
                "filename_pattern `{pattern}` is not fully anchored (`^...$`) — \
                 partial-upload markers like `.tmp` / `.part` may match and be served as \
                 valid catalog entries. Add `^` and `$` to your pattern unless this is \
                 intentional."
            );
        }
        Ok(Self {
            regex,
            timestamp_format: timestamp_format.to_string(),
        })
    }

    /// The timestamp `filename` encodes.
    ///
    /// `None` when the name is not one of this collection's files.
    /// `Some(Err(text))` when it matches but the captured `text` is not a
    /// valid time under the format (a month 13, say), which a scan may want
    /// to log rather than skip silently.
    pub fn match_timestamp<'h>(&self, filename: &'h str) -> Option<Result<DateTime<Utc>, &'h str>> {
        let stamp = self.regex.captures(filename)?.name("timestamp")?.as_str();
        Some(
            NaiveDateTime::parse_from_str(stamp, &self.timestamp_format)
                .map(|t| t.and_utc())
                .map_err(|_| stamp),
        )
    }

    /// The timestamp `filename` encodes, or `None` when it is not one of
    /// this collection's files or its timestamp does not parse.
    pub fn parse_timestamp(&self, filename: &str) -> Option<DateTime<Utc>> {
        self.match_timestamp(filename)?.ok()
    }

    /// The regex filenames are matched against.
    pub fn pattern(&self) -> &str {
        self.regex.as_str()
    }

    /// The chrono format of the `timestamp` capture.
    pub fn timestamp_format(&self) -> &str {
        &self.timestamp_format
    }
}

/// The regex a strftime template code stands for, when it is one.
fn template_code(code: char) -> Option<&'static str> {
    TEMPLATE_CODES
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, digits)| *digits)
}

/// Invert a strftime filename template into an anchored regex with a
/// `timestamp` capture, plus the chrono format of that capture.
///
/// E.g. `OPERA@%Y%m%dT%H%M@0@ACRR.tiff` becomes
/// `^OPERA@(?P<timestamp>\d{4}\d{2}\d{2}T\d{2}\d{2})@0@ACRR\.tiff$` and
/// `%Y%m%dT%H%M`.
fn expand_template(template: &str) -> Result<(String, String), FilenameError> {
    #[derive(PartialEq)]
    enum Region {
        Before,
        Inside,
        After,
    }

    let chars: Vec<char> = template.chars().collect();
    let code_at = |i: usize| {
        chars.get(i) == Some(&'%') && chars.get(i + 1).and_then(|&c| template_code(c)).is_some()
    };
    let mut regex = String::from("^");
    let mut format = String::new();
    let mut region = Region::Before;
    let mut literal = [0u8; 4];
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];
        if ch == '%' && i + 1 < chars.len() {
            let code = chars[i + 1];
            let digits = template_code(code).ok_or_else(|| FilenameError::UnknownCode {
                template: template.to_string(),
                code: format!("%{code}"),
            })?;
            match region {
                Region::Before => regex.push_str("(?P<timestamp>"),
                Region::Inside => {}
                // A second block would be a second `timestamp` group, which
                // the regex crate rejects with an opaque message.
                Region::After => {
                    return Err(FilenameError::SplitTimestamp {
                        template: template.to_string(),
                    })
                }
            }
            region = Region::Inside;
            format.push('%');
            format.push(code);
            regex.push_str(digits);
            i += 2;
            continue;
        }
        if region == Region::Inside {
            if TIMESTAMP_SEPARATORS.contains(&ch) && code_at(i + 1) {
                format.push(ch);
                regex.push_str(&regex::escape(ch.encode_utf8(&mut literal)));
                i += 1;
                continue;
            }
            // The timestamp ends here; a `Z` right after it is its UTC
            // marker and stays inside.
            if ch == 'Z' {
                format.push('Z');
                regex.push_str("Z)");
                region = Region::After;
                i += 1;
                continue;
            }
            regex.push(')');
            region = Region::After;
        }
        regex.push_str(&regex::escape(ch.encode_utf8(&mut literal)));
        i += 1;
    }

    match region {
        Region::Before => {
            return Err(FilenameError::NoStrftimeCodes {
                template: template.to_string(),
            })
        }
        Region::Inside => regex.push(')'),
        Region::After => {}
    }
    // Anchored: the whole basename must be the template, so a partial
    // upload (`….tif.tmp`, `….h5.part`) or a longer name that merely
    // contains a match is never read.
    regex.push('$');
    Ok((regex, format))
}

// ---------------------------------------------------------------------------
// Catalog scan (#817): list, match, window, dedup and cap, once for every
// engine that discovers one file per timestep.
// ---------------------------------------------------------------------------

/// Most prefix LISTs one remote scan keeps in flight.
///
/// A window expands to one prefix per day, or per hour for an hourly
/// template: up to 26 for a 24 h window. [`list_prefixes`] lists them
/// concurrently on the store's runtime rather than one blocking `list`
/// after another (Critical Rule 9), but never all at once, so a poll does
/// not burst a bucket's request rate: 26 hourly prefixes take four rounds.
pub const MAX_CONCURRENT_LISTS: usize = 8;

/// Basenames longer than this are skipped without being matched.
pub const MAX_FILENAME_LEN: usize = 255;

/// What a catalog scan keeps. [`ScanSpec::new`] sets no limits; set the
/// optional ones with struct-update syntax.
///
/// A scan keeps a file when its basename is not hidden
/// ([`ds_core::temp_files::is_hidden`]), not `exclude`d, matches the
/// [`FilenameMatcher`] and has a timestamp inside `time_filter`. It then
/// returns the kept files oldest first, one per timestamp, capped to the
/// newest `max_files` timestamps. Of files that share a timestamp, the
/// greatest key or path wins, and each file dropped for it is logged at
/// WARN. A hidden or excluded file is dropped before any of that, so it
/// never wins a timestamp or takes a `max_files` slot.
///
/// A hidden name is skipped whatever `exclude` holds: it is the temporary
/// name of a file written and then renamed into place, which an unanchored
/// `filename_pattern` would otherwise match mid-write (#1009).
#[derive(Debug, Clone, Copy)]
pub struct ScanSpec<'a> {
    /// Recognises the collection's files and reads their timestamps. A
    /// name that matches but holds no valid time is logged at WARN.
    pub matcher: &'a FilenameMatcher,
    /// Basenames to skip before matching, in [`is_excluded`]'s pattern
    /// forms: `*.tmp`, `.*`, or an exact name.
    pub exclude: &'a [String],
    /// Keep only files timestamped inside this inclusive `(start, end)`.
    pub time_filter: Option<(DateTime<Utc>, DateTime<Utc>)>,
    /// Keep only the newest N timestamps, counted after deduplication.
    pub max_files: Option<usize>,
    /// [`scan_remote`] only: skip an otherwise kept object larger than this,
    /// logged at WARN.
    pub max_size: Option<u64>,
    /// [`scan_local`] only: whether a symlink to a regular file is a file.
    pub symlinks: Symlinks,
    /// Prefix of the scan's log lines, normally the collection id.
    pub label: &'a str,
}

/// Whether `filename` matches one of `patterns`, the forms a collection's
/// `exclude_patterns` take:
///
/// - `*.ext` matches a name ending in `.ext`, e.g. `*.part`.
/// - A pattern starting with `.` matches every hidden name, e.g. `.*`.
/// - Anything else matches that exact name.
pub fn is_excluded(filename: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        if let Some(ext) = pattern.strip_prefix('*').filter(|e| e.starts_with('.')) {
            filename.ends_with(ext)
        } else if pattern.starts_with('.') {
            filename.starts_with('.')
        } else {
            filename == pattern
        }
    })
}

/// Whether [`scan_local`] catalogues a symlink to a regular file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Symlinks {
    /// Regular files only. An engine that detects a replaced file by the
    /// directory entry's own size, mtime and inode needs this.
    Skip,
    /// A symlink to a regular file counts as that file, with the target's
    /// metadata. A symlink to a directory is still skipped.
    Follow,
}

impl<'a> ScanSpec<'a> {
    /// A scan with no exclusions, time filter, cap or size limit, regular
    /// files only.
    pub fn new(matcher: &'a FilenameMatcher, label: &'a str) -> Self {
        Self {
            matcher,
            exclude: &[],
            time_filter: None,
            max_files: None,
            max_size: None,
            symlinks: Symlinks::Skip,
            label,
        }
    }

    /// The timestamp of a file this scan keeps by name, or `None`.
    fn timestamp(&self, name: &str) -> Option<DateTime<Utc>> {
        if name.len() > MAX_FILENAME_LEN
            || temp_files::is_hidden(name)
            || is_excluded(name, self.exclude)
        {
            return None;
        }
        let time = match self.matcher.match_timestamp(name)? {
            Ok(time) => time,
            Err(stamp) => {
                tracing::warn!(
                    "[{}] Cannot parse timestamp '{stamp}' from file '{name}'",
                    self.label
                );
                return None;
            }
        };
        match self.time_filter {
            Some((start, end)) if time < start || time > end => None,
            _ => Some(time),
        }
    }
}

/// A file [`scan_local`] kept.
#[derive(Debug, Clone)]
pub struct LocalFile {
    pub path: PathBuf,
    pub time: DateTime<Utc>,
    /// The regular file's metadata: the directory entry's own, or a
    /// followed symlink's target's.
    pub metadata: std::fs::Metadata,
}

/// An object [`scan_remote`] kept.
#[derive(Debug, Clone)]
pub struct RemoteFile {
    pub object: ObjectMeta,
    pub time: DateTime<Utc>,
}

impl RemoteFile {
    /// The full object key.
    pub fn key(&self) -> &str {
        self.object.location.as_ref()
    }
}

/// What [`scan_remote`] found.
#[derive(Debug)]
pub struct RemoteScan {
    /// The kept objects, oldest first, one per timestamp.
    pub entries: Vec<RemoteFile>,
    /// One report per prefix, in the order given.
    pub prefixes: Vec<PrefixReport>,
}

/// How one prefix of a [`RemoteScan`] went.
#[derive(Debug)]
pub struct PrefixReport {
    pub prefix: ObjectPath,
    /// How many objects the LIST returned, or why it failed.
    pub listed: Result<usize, DataServerError>,
    /// How many of [`RemoteScan::entries`] came from this prefix.
    pub kept: usize,
}

impl RemoteScan {
    /// The prefixes whose LIST failed, formatted `'prefix': error` and
    /// joined with `; `, and how many there were. `None` when every LIST
    /// succeeded. Whether a failure fails the scan is the engine's call.
    pub fn failures(&self) -> Option<(usize, String)> {
        let failed: Vec<String> = self
            .prefixes
            .iter()
            .filter_map(|report| {
                let e = report.listed.as_ref().err()?;
                Some(format!("'{}': {e}", report.prefix))
            })
            .collect();
        (!failed.is_empty()).then(|| (failed.len(), failed.join("; ")))
    }
}

/// List every prefix, at most [`MAX_CONCURRENT_LISTS`] at a time, on one
/// bridge call ([`DataStore::list_many`]). Returns one result per prefix,
/// in the order given: a prefix that fails to list carries its error and
/// does not stop the others. The outer `Err` is a runtime-bridge failure.
///
/// Call it where [`DataStore::list`] may be called: from the background
/// poll runtime, never from a request handler (Critical Rule 7). An engine
/// that recognises its files without a [`FilenameMatcher`] lists through
/// this; the others use [`scan_remote`].
#[allow(clippy::type_complexity)]
pub fn list_prefixes(
    store: &DataStore,
    prefixes: &[ObjectPath],
) -> Result<Vec<Result<Vec<ObjectMeta>, DataServerError>>, DataServerError> {
    store.list_many(prefixes, MAX_CONCURRENT_LISTS)
}

/// Scan an object store's prefixes for a collection's files.
///
/// The prefixes are listed concurrently ([`list_prefixes`]). Each object's
/// basename goes through `spec` (see [`ScanSpec`]), and objects over
/// `spec.max_size` are skipped. A prefix that fails to list is reported in
/// [`RemoteScan::prefixes`] and does not stop the scan; the outer `Err` is a
/// runtime-bridge failure only.
pub fn scan_remote(
    store: &DataStore,
    prefixes: &[ObjectPath],
    spec: &ScanSpec<'_>,
) -> Result<RemoteScan, DataServerError> {
    let listings = list_prefixes(store, prefixes)?;
    let mut found = Vec::new();
    let mut reports = Vec::with_capacity(prefixes.len());
    for (origin, (prefix, listed)) in prefixes.iter().zip(listings).enumerate() {
        let listed = listed.map(|objects| {
            let count = objects.len();
            for object in objects {
                let key = object.location.as_ref();
                let name = key.rsplit('/').next().unwrap_or(key);
                let Some(time) = spec.timestamp(name) else {
                    continue;
                };
                if spec.max_size.is_some_and(|max| object.size > max) {
                    tracing::warn!(
                        "[{}] skipping oversized remote object `{key}` ({} bytes)",
                        spec.label,
                        object.size
                    );
                    continue;
                }
                found.push(Found {
                    time,
                    key: key.to_string(),
                    origin,
                    item: object,
                });
            }
            count
        });
        reports.push(PrefixReport {
            prefix: prefix.clone(),
            listed,
            kept: 0,
        });
    }
    let found = finish(found, spec);
    for file in &found {
        reports[file.origin].kept += 1;
    }
    Ok(RemoteScan {
        entries: found
            .into_iter()
            .map(|file| RemoteFile {
                object: file.item,
                time: file.time,
            })
            .collect(),
        prefixes: reports,
    })
}

/// Scan one local directory, non-recursively, for a collection's files.
///
/// Returns the kept files as [`ScanSpec`] describes. Directories, names
/// that are not UTF-8 and, unless `spec.symlinks` follows them, symlinks
/// are skipped. The only error is the directory itself being unreadable;
/// an entry that cannot be read is logged and skipped.
pub fn scan_local(dir: &Path, spec: &ScanSpec<'_>) -> std::io::Result<Vec<LocalFile>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                tracing::warn!(
                    "[{}] failed to read entry in `{}`: {e}",
                    spec.label,
                    dir.display()
                );
                continue;
            }
        };
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Some(time) = spec.timestamp(&name) else {
            continue;
        };
        let Some(metadata) = regular_file_metadata(&entry, spec.symlinks) else {
            continue;
        };
        let path = entry.path();
        found.push(Found {
            time,
            key: path.to_string_lossy().into_owned(),
            origin: 0,
            item: (path, metadata),
        });
    }
    Ok(finish(found, spec)
        .into_iter()
        .map(|file| {
            let (path, metadata) = file.item;
            LocalFile {
                path,
                time: file.time,
                metadata,
            }
        })
        .collect())
}

/// The metadata of the regular file `entry` names, or `None` to skip it.
fn regular_file_metadata(entry: &std::fs::DirEntry, symlinks: Symlinks) -> Option<Metadata> {
    let file_type = entry.file_type().ok()?;
    if file_type.is_file() {
        return entry.metadata().ok();
    }
    if file_type.is_symlink() && symlinks == Symlinks::Follow {
        return std::fs::metadata(entry.path())
            .ok()
            .filter(Metadata::is_file);
    }
    None
}

/// A file a scan matched, before [`finish`].
struct Found<T> {
    time: DateTime<Utc>,
    /// The object key or path: the dedup tie-break.
    key: String,
    /// Index of the prefix it was listed under.
    origin: usize,
    item: T,
}

/// Sort `found` oldest first, keep one file per timestamp and cap to the
/// newest `spec.max_files`. Of files that share a timestamp the greatest
/// key wins, so the choice does not depend on listing order.
fn finish<T>(mut found: Vec<Found<T>>, spec: &ScanSpec<'_>) -> Vec<Found<T>> {
    // Within a timestamp, the greatest key sorts first, and `dedup_by`
    // keeps the first of each run.
    found.sort_by(|a, b| a.time.cmp(&b.time).then_with(|| b.key.cmp(&a.key)));
    found.dedup_by(|dropped, kept| {
        let duplicate = dropped.time == kept.time;
        if duplicate {
            tracing::warn!(
                "[{}] Duplicate timestamp {}: using {}, replacing {}",
                spec.label,
                kept.time,
                kept.key,
                dropped.key
            );
        }
        duplicate
    });
    if let Some(max) = spec.max_files {
        let excess = found.len().saturating_sub(max);
        found.drain(..excess);
    }
    found
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

    /// Each hour is one `list` per poll: an hourly template's window
    /// is capped, while a day-level template may run with a long one.
    #[test]
    fn hourly_prefix_window_is_bounded() {
        let window = |s: &str| TimeWindow::parse(s).unwrap();
        assert!(validate_prefix_pattern("%Y/%j/%H/", Some(&window("-PT24H"))).is_ok());
        assert!(validate_prefix_pattern("%Y/%j/%H/", Some(&window("PT24H"))).is_ok());
        assert!(validate_prefix_pattern("%Y/%j/%H/", Some(&window("-PT25H"))).is_err());
        assert!(validate_prefix_pattern("%Y/%j/%H/", Some(&window("-P30D"))).is_err());
        assert!(validate_prefix_pattern("%Y/%j/", Some(&window("-P30D"))).is_ok());
    }

    fn template(t: &str) -> FilenameMatcher {
        FilenameMatcher::from_template(t).unwrap()
    }

    fn time(s: &str) -> Option<DateTime<Utc>> {
        Some(s.parse().unwrap())
    }

    /// The layouts engine-geotiff and engine-odim are configured with today.
    #[test]
    fn template_reads_each_production_layout() {
        let cases = [
            // OPERA: `@` is not a separator, so it closes the timestamp.
            (
                "OPERA@%Y%m%dT%H%M@0@ACRR.tiff",
                "%Y%m%dT%H%M",
                "OPERA@20260324T2040@0@ACRR.tiff",
                "2026-03-24T20:40:00Z",
            ),
            // A trailing `Z` is the UTC marker, inside the timestamp.
            (
                "radar_%Y%m%dT%H%MZ.tif",
                "%Y%m%dT%H%MZ",
                "radar_20260324T2315Z.tif",
                "2026-03-24T23:15:00Z",
            ),
            // FMI: the timestamp leads the name.
            (
                "%Y%m%d%H%M_composite_cappi_600_dbzh_finrad_qc.tif",
                "%Y%m%d%H%M",
                "202603251955_composite_cappi_600_dbzh_finrad_qc.tif",
                "2026-03-25T19:55:00Z",
            ),
            (
                "data_%Y-%m-%dT%H:%M:%S.tif",
                "%Y-%m-%dT%H:%M:%S",
                "data_2026-03-25T19:30:05.tif",
                "2026-03-25T19:30:05Z",
            ),
            // DMI: `_` between codes stays in the timestamp.
            (
                "comp_%Y_%m_%d_%H%M.h5",
                "%Y_%m_%d_%H%M",
                "comp_2025_07_14_1530.h5",
                "2025-07-14T15:30:00Z",
            ),
            (
                "%Y%j%H%M.nc",
                "%Y%j%H%M",
                "20262681900.nc",
                "2026-09-25T19:00:00Z",
            ),
        ];
        for (tpl, format, name, expected) in cases {
            let m = template(tpl);
            assert_eq!(m.timestamp_format(), format, "{tpl}");
            assert_eq!(m.parse_timestamp(name), time(expected), "{tpl}");
        }
        assert_eq!(
            template("OPERA@%Y%m%dT%H%M@0@ACRR.tiff").pattern(),
            r"^OPERA@(?P<timestamp>\d{4}\d{2}\d{2}T\d{2}\d{2})@0@ACRR\.tiff$"
        );
    }

    /// Partial uploads and names that merely contain a match are not the
    /// template: it is anchored `^…$` (#817).
    #[test]
    fn template_matches_the_whole_name_only() {
        let m = template("radar_%Y%m%dT%H%MZ.tif");
        assert!(m.parse_timestamp("radar_20260324T2315Z.tif").is_some());
        for other in [
            "radar_20260324T2315Z.tif.tmp",
            "radar_20260324T2315Z.tif.part",
            "old_radar_20260324T2315Z.tif",
            "radar_20260324T2315Z_tif",
            "README.md",
        ] {
            assert_eq!(m.match_timestamp(other), None, "{other}");
        }
    }

    /// A name that has the template's shape but no valid time is reported,
    /// so a scan can log it; `parse_timestamp` skips it.
    #[test]
    fn invalid_timestamp_is_distinguished_from_no_match() {
        let m = template("radar_%Y%m%dT%H%MZ.tif");
        assert_eq!(
            m.match_timestamp("radar_20261324T2315Z.tif"),
            Some(Err("20261324T2315Z"))
        );
        assert_eq!(m.parse_timestamp("radar_20261324T2315Z.tif"), None);
    }

    /// Literals are matched as characters, not bytes, and never as regex
    /// syntax.
    #[test]
    fn template_literals_are_escaped_characters() {
        let m = template("tutka_%Y%m%d%H%Mä.h5");
        assert_eq!(
            m.parse_timestamp("tutka_202607141530ä.h5"),
            time("2026-07-14T15:30:00Z")
        );
        let m = template("radar+(%Y%m%d%H%M).tif");
        assert!(m.parse_timestamp("radar+(202607141530).tif").is_some());
        assert_eq!(m.parse_timestamp("radarr202607141530xtif"), None);
    }

    #[test]
    fn template_errors() {
        assert!(matches!(
            FilenameMatcher::from_template("radar.h5"),
            Err(FilenameError::NoStrftimeCodes { .. })
        ));
        match FilenameMatcher::from_template("radar_%X.h5") {
            Err(FilenameError::UnknownCode { code, .. }) => assert_eq!(code, "%X"),
            other => panic!("expected UnknownCode, got {other:?}"),
        }
        // Two blocks would be two `timestamp` groups.
        match FilenameMatcher::from_template("%Y_STATION_%H%M.h5") {
            Err(e @ FilenameError::SplitTimestamp { .. }) => {
                assert!(e.to_string().contains("filename_pattern"), "{e}");
            }
            other => panic!("expected SplitTimestamp, got {other:?}"),
        }
    }

    #[test]
    fn explicit_pattern_uses_its_capture_and_format() {
        let m = FilenameMatcher::from_pattern(r"^comp-(?P<timestamp>\d{12})\.h5$", "%Y%m%d%H%M")
            .unwrap();
        assert_eq!(m.pattern(), r"^comp-(?P<timestamp>\d{12})\.h5$");
        assert_eq!(
            m.parse_timestamp("comp-202507141530.h5"),
            time("2025-07-14T15:30:00Z")
        );
        assert_eq!(m.parse_timestamp("comp-202507141530.h5.tmp"), None);
        // The `(?<name>…)` capture syntax works too.
        let m = FilenameMatcher::from_pattern(r"^comp-(?<timestamp>\d{12})\.h5$", "%Y%m%d%H%M")
            .unwrap();
        assert!(m.parse_timestamp("comp-202507141530.h5").is_some());
    }

    /// An explicit pattern is used as written: without anchors it matches a
    /// partial upload, which is why building one logs a WARN.
    #[test]
    fn explicit_pattern_is_not_auto_anchored() {
        let m =
            FilenameMatcher::from_pattern(r"comp-(?P<timestamp>\d{12})\.h5", "%Y%m%d%H%M").unwrap();
        assert_eq!(
            m.parse_timestamp("comp-202507141530.h5.tmp"),
            time("2025-07-14T15:30:00Z")
        );
    }

    #[test]
    fn explicit_pattern_errors() {
        for no_capture in [r"^comp-(\d+)\.h5$", r"^comp-(?P<time>\d{12})\.h5$"] {
            assert!(
                matches!(
                    FilenameMatcher::from_pattern(no_capture, "%Y%m%d%H%M"),
                    Err(FilenameError::NoTimestampCapture { .. })
                ),
                "{no_capture}"
            );
        }
        assert!(matches!(
            FilenameMatcher::from_pattern(r"^comp-(?P<timestamp>\d{12}\.h5$", "%Y%m%d%H%M"),
            Err(FilenameError::InvalidRegex { .. })
        ));
    }
}

#[cfg(test)]
mod scan_tests {
    use super::*;
    use crate::test_store::ListProbe;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn opera() -> FilenameMatcher {
        FilenameMatcher::from_template("OPERA@%Y%m%dT%H%M@0@DBZH.h5").unwrap()
    }

    fn opera_name(hhmm: &str) -> String {
        format!("OPERA@20260515T{hhmm}@0@DBZH.h5")
    }

    fn local_names(files: &[LocalFile]) -> Vec<String> {
        files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_str().unwrap().to_string())
            .collect()
    }

    fn remote_keys(scan: &RemoteScan) -> Vec<&str> {
        scan.entries.iter().map(RemoteFile::key).collect()
    }

    /// Oldest first, window inclusive at both ends and applied before the
    /// cap, which keeps the newest. Directories, unrelated names and
    /// partial uploads next to a finished file are never kept.
    #[test]
    fn local_scan_sorts_windows_and_caps() {
        let dir = tempfile::tempdir().unwrap();
        for hhmm in ["0010", "0000", "0020", "0005", "0015"] {
            std::fs::write(dir.path().join(opera_name(hhmm)), b"x").unwrap();
        }
        for other in ["README.md", "OPERA@20260515T0025@0@DBZH.h5.tmp"] {
            std::fs::write(dir.path().join(other), b"x").unwrap();
        }
        std::fs::write(dir.path().join(opera_name("0030") + ".part"), b"x").unwrap();
        std::fs::create_dir(dir.path().join(opera_name("0035"))).unwrap();
        let matcher = opera();

        let all = scan_local(dir.path(), &ScanSpec::new(&matcher, "t")).unwrap();
        assert_eq!(
            local_names(&all),
            ["0000", "0005", "0010", "0015", "0020"].map(opera_name)
        );
        assert_eq!(all[0].time, at("2026-05-15T00:00:00Z"));
        assert_eq!(all[0].metadata.len(), 1);

        let window = Some((at("2026-05-15T00:05:00Z"), at("2026-05-15T00:15:00Z")));
        let windowed = ScanSpec {
            time_filter: window,
            ..ScanSpec::new(&matcher, "t")
        };
        assert_eq!(
            local_names(&scan_local(dir.path(), &windowed).unwrap()),
            ["0005", "0010", "0015"].map(opera_name)
        );
        let capped = ScanSpec {
            max_files: Some(2),
            ..windowed
        };
        assert_eq!(
            local_names(&scan_local(dir.path(), &capped).unwrap()),
            ["0010", "0015"].map(opera_name)
        );
    }

    /// Files sharing a timestamp collapse to the greatest path whatever the
    /// directory order, and the cap counts timestamps, not files.
    #[test]
    fn local_scan_keeps_the_greatest_path_per_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "202605150000_b.h5",
            "202605150000_a.h5",
            "202605150000_c.h5",
            "202605150005_a.h5",
        ] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let matcher =
            FilenameMatcher::from_pattern(r"^(?P<timestamp>\d{12})_[a-z]\.h5$", "%Y%m%d%H%M")
                .unwrap();
        let spec = ScanSpec {
            max_files: Some(2),
            ..ScanSpec::new(&matcher, "t")
        };
        assert_eq!(
            local_names(&scan_local(dir.path(), &spec).unwrap()),
            ["202605150000_c.h5", "202605150005_a.h5"]
        );
    }

    /// A symlink to a file is catalogued, with its target's metadata, only
    /// when the spec follows symlinks. A symlink to a directory never is.
    #[cfg(unix)]
    #[test]
    fn local_scan_follows_symlinks_only_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(opera_name("0000")), b"x").unwrap();
        let target = elsewhere.path().join("volume.h5");
        std::fs::write(&target, b"xyz").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join(opera_name("0005"))).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), dir.path().join(opera_name("0010"))).unwrap();
        let matcher = opera();

        let skip = scan_local(dir.path(), &ScanSpec::new(&matcher, "t")).unwrap();
        assert_eq!(local_names(&skip), [opera_name("0000")]);

        let follow = ScanSpec {
            symlinks: Symlinks::Follow,
            ..ScanSpec::new(&matcher, "t")
        };
        let followed = scan_local(dir.path(), &follow).unwrap();
        assert_eq!(local_names(&followed), ["0000", "0005"].map(opera_name));
        assert_eq!(followed[1].metadata.len(), 3, "the target's metadata");
    }

    #[test]
    fn local_scan_of_a_missing_directory_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let matcher = opera();
        assert!(scan_local(&dir.path().join("missing"), &ScanSpec::new(&matcher, "t")).is_err());
    }

    #[test]
    fn exclude_pattern_forms() {
        let patterns = |p: &[&str]| p.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        let defaults = patterns(&["*.tmp", "*.part"]);
        assert!(is_excluded("data.tmp", &defaults));
        assert!(is_excluded("radar.tif.part", &defaults));
        assert!(!is_excluded("radar_20240101T0000Z.tif", &defaults));
        assert!(!is_excluded("radar.part.tif", &defaults));
        assert!(is_excluded(".hidden", &patterns(&[".*"])));
        assert!(!is_excluded("visible", &patterns(&[".*"])));
        assert!(is_excluded("LOCK", &patterns(&["LOCK"])));
        assert!(!is_excluded("LOCKED", &patterns(&["LOCK"])));
        assert!(!is_excluded("anything", &[]));
    }

    /// An unanchored explicit pattern matches a partial upload too. The
    /// default exclusions must drop it before the dedup, where it would
    /// beat its finished file as the greater name, and before the cap,
    /// where it would take a slot (#817 review).
    const UNANCHORED: &str = r"radar_(?P<timestamp>\d{8}T\d{4}Z)\.tif";
    const PARTIAL_LAYOUT: [&str; 4] = [
        "radar_20260324T2310Z.tif",
        "radar_20260324T2315Z.tif",
        "radar_20260324T2315Z.tif.part",
        "radar_20260324T2320Z.tif.tmp",
    ];

    fn default_excludes() -> Vec<String> {
        vec!["*.tmp".to_string(), "*.part".to_string()]
    }

    #[test]
    fn local_scan_excludes_before_dedup_and_cap() {
        let dir = tempfile::tempdir().unwrap();
        for name in PARTIAL_LAYOUT {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let matcher = FilenameMatcher::from_pattern(UNANCHORED, "%Y%m%dT%H%MZ").unwrap();
        let exclude = default_excludes();
        let spec = ScanSpec {
            exclude: &exclude,
            max_files: Some(2),
            ..ScanSpec::new(&matcher, "t")
        };
        assert_eq!(
            local_names(&scan_local(dir.path(), &spec).unwrap()),
            ["radar_20260324T2310Z.tif", "radar_20260324T2315Z.tif"]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_scan_excludes_before_dedup_and_cap() {
        let keys = PARTIAL_LAYOUT.map(|name| format!("d/{name}"));
        let probe = ListProbe::default()
            .with_objects(&keys.iter().map(String::as_str).collect::<Vec<_>>())
            .await;
        let store = DataStore::new(Arc::new(probe));
        let matcher = FilenameMatcher::from_pattern(UNANCHORED, "%Y%m%dT%H%MZ").unwrap();
        let exclude = default_excludes();
        let spec = ScanSpec {
            exclude: &exclude,
            max_files: Some(2),
            ..ScanSpec::new(&matcher, "t")
        };
        let scan = scan_remote(&store, &[ObjectPath::from("d")], &spec).unwrap();
        assert_eq!(
            remote_keys(&scan),
            ["d/radar_20260324T2310Z.tif", "d/radar_20260324T2315Z.tif"]
        );
        assert_eq!(scan.prefixes[0].listed.as_ref().ok(), Some(&4));
    }

    /// A publisher writing `.name` and renaming it into place: the hidden
    /// file holds the newest timestamp, which an unanchored pattern
    /// matches. It is skipped with no `exclude` at all, so it neither
    /// becomes the newest entry nor takes the `max_files` slot (#1009).
    const HIDDEN_LAYOUT: [&str; 3] = [
        "radar_20260324T2310Z.tif",
        "radar_20260324T2315Z.tif",
        ".radar_20260324T2320Z.tif",
    ];

    #[test]
    fn local_scan_skips_hidden_names_without_excludes() {
        let dir = tempfile::tempdir().unwrap();
        for name in HIDDEN_LAYOUT {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let matcher = FilenameMatcher::from_pattern(UNANCHORED, "%Y%m%dT%H%MZ").unwrap();
        let spec = ScanSpec {
            max_files: Some(1),
            ..ScanSpec::new(&matcher, "t")
        };
        assert_eq!(
            local_names(&scan_local(dir.path(), &spec).unwrap()),
            ["radar_20260324T2315Z.tif"]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_scan_skips_hidden_names_without_excludes() {
        let keys = HIDDEN_LAYOUT.map(|name| format!("d/{name}"));
        let probe = ListProbe::default()
            .with_objects(&keys.iter().map(String::as_str).collect::<Vec<_>>())
            .await;
        let store = DataStore::new(Arc::new(probe));
        let matcher = FilenameMatcher::from_pattern(UNANCHORED, "%Y%m%dT%H%MZ").unwrap();
        let spec = ScanSpec {
            max_files: Some(1),
            ..ScanSpec::new(&matcher, "t")
        };
        let scan = scan_remote(&store, &[ObjectPath::from("d")], &spec).unwrap();
        assert_eq!(remote_keys(&scan), ["d/radar_20260324T2315Z.tif"]);
    }

    /// A name over `MAX_FILENAME_LEN` is skipped even when it matches.
    #[test]
    fn overlong_names_are_skipped_unmatched() {
        let prefix = "a".repeat(MAX_FILENAME_LEN);
        let matcher = FilenameMatcher::from_template(&format!("{prefix}%Y%m%d%H%M.h5")).unwrap();
        let name = format!("{prefix}202605150000.h5");
        assert!(matcher.parse_timestamp(&name).is_some());
        assert_eq!(ScanSpec::new(&matcher, "t").timestamp(&name), None);
    }

    /// The remote scan merges its prefixes and applies the same window,
    /// dedup and cap. The greatest key wins a timestamp two prefixes share,
    /// and oversized and partially uploaded objects are skipped.
    #[tokio::test(flavor = "multi_thread")]
    async fn remote_scan_merges_prefixes_dedups_windows_and_caps() {
        let probe = ListProbe::default()
            .with_objects(&[
                &format!("d1/{}", opera_name("0000")),
                &format!("d1/{}", opera_name("0005")),
                &format!("d1/{}.tmp", opera_name("0010")),
                "d1/README.md",
                &format!("d2/{}", opera_name("0005")),
                &format!("d2/{}", opera_name("0010")),
                &format!("d2/{}.part", opera_name("0015")),
            ])
            .await
            .with_object_of_size(&format!("d2/{}", opera_name("0020")), 100)
            .await;
        let store = DataStore::new(Arc::new(probe));
        let prefixes = [ObjectPath::from("d1"), ObjectPath::from("d2")];
        let matcher = opera();
        let spec = ScanSpec {
            time_filter: Some((at("2026-05-15T00:05:00Z"), at("2026-05-15T00:20:00Z"))),
            max_size: Some(10),
            ..ScanSpec::new(&matcher, "t")
        };

        let scan = scan_remote(&store, &prefixes, &spec).unwrap();
        assert_eq!(
            remote_keys(&scan),
            [
                format!("d2/{}", opera_name("0005")),
                format!("d2/{}", opera_name("0010")),
            ]
        );
        assert_eq!(scan.entries[0].time, at("2026-05-15T00:05:00Z"));
        let reports: Vec<_> = scan
            .prefixes
            .iter()
            .map(|r| (r.prefix.to_string(), *r.listed.as_ref().unwrap(), r.kept))
            .collect();
        assert_eq!(
            reports,
            [("d1".to_string(), 4, 0), ("d2".to_string(), 4, 2)]
        );
        assert!(scan.failures().is_none());

        let capped = ScanSpec {
            max_files: Some(1),
            ..spec
        };
        let scan = scan_remote(&store, &prefixes, &capped).unwrap();
        assert_eq!(remote_keys(&scan), [format!("d2/{}", opera_name("0010"))]);
    }

    /// A prefix that fails to list is reported and the others still count.
    #[tokio::test(flavor = "multi_thread")]
    async fn remote_scan_reports_a_failed_prefix_and_keeps_the_rest() {
        let probe = ListProbe {
            failing: ["d1".to_string()].into(),
            ..ListProbe::default()
        }
        .with_objects(&[
            &format!("d1/{}", opera_name("0000")),
            &format!("d2/{}", opera_name("0005")),
        ])
        .await;
        let store = DataStore::new(Arc::new(probe));
        let matcher = opera();
        let scan = scan_remote(
            &store,
            &[ObjectPath::from("d1"), ObjectPath::from("d2")],
            &ScanSpec::new(&matcher, "t"),
        )
        .unwrap();
        assert_eq!(remote_keys(&scan), [format!("d2/{}", opera_name("0005"))]);
        assert!(scan.prefixes[0].listed.is_err());
        let (count, summary) = scan.failures().unwrap();
        assert_eq!(count, 1);
        assert!(
            summary.starts_with("'d1': ") && summary.contains("partition unavailable"),
            "{summary}"
        );
    }

    /// Hourly prefixes over a 24 h window are listed concurrently, never
    /// more than `MAX_CONCURRENT_LISTS` at a time, each exactly once
    /// (Critical Rule 9, #817).
    #[tokio::test(flavor = "multi_thread")]
    async fn remote_scan_lists_prefixes_concurrently_up_to_the_bound() {
        let (start, end) = (at("2026-09-25T00:00:00Z"), at("2026-09-25T23:59:00Z"));
        let prefixes = expand_prefix_for_range("ABI/%Y/%j/%H/", start, end).unwrap();
        assert_eq!(prefixes.len(), 24);
        let keys: Vec<String> = prefixes
            .iter()
            .enumerate()
            .map(|(hour, p)| format!("{p}/OR_2026268{hour:02}00.nc"))
            .collect();
        let probe = ListProbe {
            delay: std::time::Duration::from_millis(20),
            ..ListProbe::default()
        }
        .with_objects(&keys.iter().map(String::as_str).collect::<Vec<_>>())
        .await;
        let (peak, listed) = (probe.peak.clone(), probe.listed.clone());
        let store = DataStore::new(Arc::new(probe));
        let matcher = FilenameMatcher::from_template("OR_%Y%j%H%M.nc").unwrap();
        let prefixes: Vec<ObjectPath> = prefixes
            .iter()
            .map(|p| ObjectPath::from(p.as_str()))
            .collect();

        let scan = scan_remote(&store, &prefixes, &ScanSpec::new(&matcher, "t")).unwrap();
        assert_eq!(remote_keys(&scan), keys);
        assert_eq!(peak.load(Ordering::SeqCst), MAX_CONCURRENT_LISTS);
        let mut listed = listed.lock().unwrap().clone();
        listed.sort();
        let mut expected: Vec<String> = prefixes.iter().map(ObjectPath::to_string).collect();
        expected.sort();
        assert_eq!(listed, expected, "each prefix listed exactly once");
    }
}
