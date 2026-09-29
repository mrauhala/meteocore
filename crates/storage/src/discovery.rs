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
//!
//! This module is the shared home for all three. `engine-odim` and
//! `engine-geotiff` use it; `engine-grib` expands its model-run
//! prefixes, a strftime date plus a `{run}` hour, with
//! [`expand_run_prefixes`].

use std::fmt::Write as _;

use chrono::format::{Fixed, Item, Numeric, StrftimeItems};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Timelike, Utc};
use ds_core::error::DataServerError;
use regex::Regex;

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
/// is one `list` per poll, issued sequentially (Critical Rule 9), so this
/// caps a poll at 26 calls. A longer window belongs on a day-level
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

    /// Each hour is a sequential `list` per poll: an hourly template's window
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
