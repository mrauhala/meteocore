//! The `subset` query parameter of OGC API - Maps Part 1 (OGC 20-058:
//! `/req/spatial-subsetting/subset-definition` and
//! `/req/datetime/subset-definition`) and the time selections `subset` and
//! `datetime` express. Shared so every render route parses one grammar.
//!
//! The grammar is the standard's ABNF:
//!
//! ```text
//! SubsetSpec       = "subset" "=" axisName "(" intervalOrSingle ")"
//! intervalOrSingle = interval / single
//! interval         = low ":" high
//! single           = number / text / "*"
//! ```
//!
//! `axisName` is a letter A–Z of either case followed by any number of
//! letters and digits, `number` an integer or floating-point number, and
//! `text` double-quoted ASCII such as an RFC 3339 time. One value may hold
//! several comma-separated expressions, and `subset` may repeat:
//! `subset=Lat(-90:90)&subset=Lon(0:10)` is `subset=Lat(-90:90),Lon(0:10)`.
//!
//! The axes a route accepts are the caller's ([`by_axis`]): Maps takes
//! `Lon`/`Lat` or `E`/`N` and `time`, while OGC API - Tiles names its time
//! axis `datetime`. An axis outside that list is an error, never ignored.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Utc};

/// One value of a subset expression.
#[derive(Debug, Clone, PartialEq)]
pub enum SubsetValue {
    /// An integer or floating-point number.
    Number(f64),
    /// Double-quoted text, without its quotes.
    Text(String),
    /// `*`: the axis minimum as `low`, its maximum as `high` or as a single
    /// value.
    Star,
}

/// The `intervalOrSingle` of a subset expression.
#[derive(Debug, Clone, PartialEq)]
pub enum SubsetRange {
    /// `axis(value)`, a slice.
    Single(SubsetValue),
    /// `axis(low:high)`, a trim.
    Interval(SubsetValue, SubsetValue),
}

/// One subset expression, `axis(intervalOrSingle)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Subset {
    /// The axis name as the request spelled it.
    pub axis: String,
    pub range: SubsetRange,
}

/// Every value of the query parameter `name` in `raw_query`, in request
/// order. `subset` may repeat, which a struct-shaped query extractor cannot
/// represent.
pub fn query_values(raw_query: Option<&str>, name: &str) -> Vec<String> {
    raw_query
        .map(|query| {
            form_urlencoded::parse(query.as_bytes())
                .filter(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Parse every `subset` value into its expressions, in request order. The
/// error describes the first malformed expression.
pub fn parse<S: AsRef<str>>(values: &[S]) -> Result<Vec<Subset>, String> {
    let mut subsets = Vec::new();
    for value in values {
        for expression in split_expressions(value.as_ref())? {
            subsets.push(parse_expression(expression)?);
        }
    }
    Ok(subsets)
}

/// Split one `subset` value at the commas that separate expressions: those
/// outside parentheses and quotes.
fn split_expressions(value: &str) -> Result<Vec<&str>, String> {
    let malformed = || format!("Invalid subset '{value}': unbalanced parentheses or quotes");
    let mut expressions = Vec::new();
    let (mut depth, mut quoted, mut start) = (0usize, false, 0usize);
    for (i, c) in value.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth = depth.checked_sub(1).ok_or_else(malformed)?,
            ',' if !quoted && depth == 0 => {
                expressions.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if depth != 0 || quoted {
        return Err(malformed());
    }
    expressions.push(&value[start..]);
    Ok(expressions)
}

fn parse_expression(expression: &str) -> Result<Subset, String> {
    let expression = expression.trim();
    let invalid = |why: &str| format!("Invalid subset '{expression}': {why}");
    let (axis, rest) = expression
        .split_once('(')
        .ok_or_else(|| invalid("expected axis(low:high) or axis(value)"))?;
    let axis = axis.trim();
    let mut chars = axis.chars();
    let valid_axis = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric());
    if !valid_axis {
        return Err(invalid(
            "the axis name must start with a letter followed by letters and digits",
        ));
    }
    let inner = rest
        .trim_end()
        .strip_suffix(')')
        .ok_or_else(|| invalid("expected axis(low:high) or axis(value)"))?;
    let parts = split_unquoted(inner, ':');
    let range = match parts.as_slice() {
        [single] => SubsetRange::Single(parse_value(single, expression)?),
        [low, high] => SubsetRange::Interval(
            parse_value(low, expression)?,
            parse_value(high, expression)?,
        ),
        _ => return Err(invalid("expected one value or low:high")),
    };
    Ok(Subset {
        axis: axis.to_string(),
        range,
    })
}

/// Split at `separator` outside double quotes.
fn split_unquoted(s: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut quoted, mut start) = (false, 0usize);
    for (i, c) in s.char_indices() {
        if c == '"' {
            quoted = !quoted;
        } else if c == separator && !quoted {
            parts.push(&s[start..i]);
            start = i + c.len_utf8();
        }
    }
    parts.push(&s[start..]);
    parts
}

fn parse_value(raw: &str, expression: &str) -> Result<SubsetValue, String> {
    let value = raw.trim();
    if value == "*" {
        return Ok(SubsetValue::Star);
    }
    if let Some(text) = value.strip_prefix('"') {
        return match text.strip_suffix('"') {
            Some(text) if !text.contains('"') => Ok(SubsetValue::Text(text.to_string())),
            _ => Err(format!(
                "Invalid subset '{expression}': unbalanced quotes in '{value}'"
            )),
        };
    }
    value
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .map(SubsetValue::Number)
        .ok_or_else(|| {
            format!(
                "Invalid subset '{expression}': '{value}' is not a number, '*' or \
                 double-quoted text"
            )
        })
}

/// An axis a route accepts in `subset`: the name it reports, and every name
/// that selects it. Names match without regard to ASCII case.
#[derive(Debug, Clone, Copy)]
pub struct Axis<'a> {
    pub name: &'a str,
    pub aliases: &'a [&'a str],
}

impl Axis<'_> {
    fn matches(&self, requested: &str) -> bool {
        self.name.eq_ignore_ascii_case(requested)
            || self
                .aliases
                .iter()
                .any(|a| a.eq_ignore_ascii_case(requested))
    }
}

/// Assign each subset to one of `axes`, keyed by the axis' reported name.
/// An axis outside `axes`, or one named twice (under any of its names), is
/// an error naming the valid axes.
pub fn by_axis<'a>(
    subsets: Vec<Subset>,
    axes: &[Axis<'a>],
) -> Result<BTreeMap<&'a str, SubsetRange>, String> {
    let mut ranges = BTreeMap::new();
    for subset in subsets {
        let Some(axis) = axes.iter().find(|a| a.matches(&subset.axis)) else {
            let names: Vec<&str> = axes.iter().map(|a| a.name).collect();
            return Err(format!(
                "subset axis '{}' is not supported; valid axes: {}",
                subset.axis,
                names.join(", ")
            ));
        };
        if ranges.insert(axis.name, subset.range).is_some() {
            return Err(format!(
                "subset names the axis '{}' more than once",
                axis.name
            ));
        }
    }
    Ok(ranges)
}

/// The time a request selects, from `datetime` or a time `subset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeSelection {
    /// One instant. A route snaps it to an available time as it does a
    /// `datetime` instant (OGC 20-058 `/per/datetime/closest`).
    Instant(DateTime<Utc>),
    /// Every instant from `start` to `end`, both inclusive; `None` is
    /// unbounded on that side.
    Range {
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
    },
}

impl TimeSelection {
    /// The selection of a time subset (`/req/datetime/subset-definition`):
    /// double-quoted RFC 3339 instants or the partial forms `yyyy`,
    /// `yyyy-mm`, `yyyy-mm-dd`, `yyyy-mm-ddThhZ` and `yyyy-mm-ddThh:mmZ`, and
    /// `*` for the earliest (`low`) or latest (`high`, or a single value)
    /// available time.
    ///
    /// A partial single value selects its whole period, and a partial bound
    /// reaches to its period's start (`low`) or end (`high`).
    pub fn from_subset(range: &SubsetRange) -> Result<Self, String> {
        match range {
            SubsetRange::Single(SubsetValue::Star) => Ok(Self::Range {
                start: None,
                end: None,
            }),
            SubsetRange::Single(value) => match subset_time(value)? {
                TimeText::Instant(t) => Ok(Self::Instant(t)),
                TimeText::Period(start, end) => Ok(Self::Range {
                    start: Some(start),
                    end: Some(end),
                }),
            },
            SubsetRange::Interval(low, high) => {
                let start = match low {
                    SubsetValue::Star => None,
                    value => Some(subset_time(value)?.start()),
                };
                let end = match high {
                    SubsetValue::Star => None,
                    value => Some(subset_time(value)?.end()),
                };
                Self::range(start, end)
            }
        }
    }

    /// The selection of a `datetime` value (`/req/datetime/datetime-definition`):
    /// an instant, or an interval `start/end` whose open side is `..` or
    /// empty. Instants are RFC 3339; the forms [`parse_instant`] accepts
    /// besides are a MeteoCore leniency.
    pub fn from_datetime(value: &str) -> Result<Self, String> {
        let Some((start, end)) = value.split_once('/') else {
            return parse_instant(value).map(Self::Instant);
        };
        let bound = |s: &str| match s.trim() {
            "" | ".." => Ok(None),
            instant => parse_instant(instant).map(Some),
        };
        Self::range(bound(start)?, bound(end)?)
    }

    fn range(start: Option<DateTime<Utc>>, end: Option<DateTime<Utc>>) -> Result<Self, String> {
        if let (Some(s), Some(e)) = (start, end) {
            if s > e {
                return Err(format!(
                    "time interval start {} is after its end {}",
                    rfc3339(s),
                    rfc3339(e)
                ));
            }
        }
        Ok(Self::Range { start, end })
    }

    /// The latest of `times` this selection contains: for a range, the
    /// latest time inside it, `None` when it holds none; for an instant, the
    /// instant itself when listed.
    pub fn latest_in(&self, times: &[DateTime<Utc>]) -> Option<DateTime<Utc>> {
        let (start, end) = match *self {
            Self::Instant(t) => (Some(t), Some(t)),
            Self::Range { start, end } => (start, end),
        };
        times
            .iter()
            .copied()
            .filter(|t| start.is_none_or(|s| *t >= s) && end.is_none_or(|e| *t <= e))
            .max()
    }
}

/// A time value of a subset: an instant, or a partial date or time naming a
/// period `[start, end]` (end inclusive, one nanosecond before the next).
enum TimeText {
    Instant(DateTime<Utc>),
    Period(DateTime<Utc>, DateTime<Utc>),
}

impl TimeText {
    fn start(&self) -> DateTime<Utc> {
        match *self {
            Self::Instant(t) | Self::Period(t, _) => t,
        }
    }

    fn end(&self) -> DateTime<Utc> {
        match *self {
            Self::Instant(t) | Self::Period(_, t) => t,
        }
    }
}

fn subset_time(value: &SubsetValue) -> Result<TimeText, String> {
    let SubsetValue::Text(text) = value else {
        return Err(
            "time subset values are double-quoted RFC 3339 times or '*', \
             e.g. time(\"2026-10-01T12:00:00Z\")"
                .to_string(),
        );
    };
    parse_time_text(text).ok_or_else(|| {
        format!(
            "time subset value \"{text}\" is not an RFC 3339 time or one of the \
             forms yyyy, yyyy-mm, yyyy-mm-dd, yyyy-mm-ddThhZ, yyyy-mm-ddThh:mmZ"
        )
    })
}

fn parse_time_text(text: &str) -> Option<TimeText> {
    let text = text.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(text) {
        return Some(TimeText::Instant(t.with_timezone(&Utc)));
    }
    let period = |start: NaiveDateTime, next: NaiveDateTime| {
        Some(TimeText::Period(
            start.and_utc(),
            next.and_utc() - Duration::nanoseconds(1),
        ))
    };
    // Only the ASCII forms; this also keeps every slice below on a char
    // boundary.
    if !text.is_ascii() {
        return None;
    }
    let number = |s: &str, n: usize| {
        (s.len() == n && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<u32>().ok())
            .flatten()
    };
    let date = |s: &str| {
        let (year, rest) = s.split_once('-')?;
        let (month, day) = rest.split_once('-')?;
        NaiveDate::from_ymd_opt(number(year, 4)? as i32, number(month, 2)?, number(day, 2)?)
    };
    // `Thh` or `Thh:mm` followed by `Z`: (hour, minute).
    let clock = |s: &str| {
        let s = s.strip_prefix(['T', 't'])?.strip_suffix(['Z', 'z'])?;
        match s.split_once(':') {
            Some((hour, minute)) => Some((number(hour, 2)?, Some(number(minute, 2)?))),
            None => Some((number(s, 2)?, None)),
        }
    };
    match text.len() {
        // yyyy
        4 => {
            let year = number(text, 4)? as i32;
            period(
                NaiveDate::from_ymd_opt(year, 1, 1)?.and_hms_opt(0, 0, 0)?,
                NaiveDate::from_ymd_opt(year + 1, 1, 1)?.and_hms_opt(0, 0, 0)?,
            )
        }
        // yyyy-mm
        7 => {
            let (year, month) = text.split_once('-')?;
            let (year, month) = (number(year, 4)? as i32, number(month, 2)?);
            let start = NaiveDate::from_ymd_opt(year, month, 1)?;
            let next = if month == 12 {
                NaiveDate::from_ymd_opt(year + 1, 1, 1)?
            } else {
                NaiveDate::from_ymd_opt(year, month + 1, 1)?
            };
            period(start.and_hms_opt(0, 0, 0)?, next.and_hms_opt(0, 0, 0)?)
        }
        // yyyy-mm-dd
        10 => {
            let start = date(text)?.and_hms_opt(0, 0, 0)?;
            period(start, start + Duration::days(1))
        }
        // yyyy-mm-ddThhZ and yyyy-mm-ddThh:mmZ
        14 | 17 => {
            let (day, time) = text.split_at(10);
            match clock(time)? {
                (hour, None) => {
                    let start = date(day)?.and_hms_opt(hour, 0, 0)?;
                    period(start, start + Duration::hours(1))
                }
                (hour, Some(minute)) => {
                    let start = date(day)?.and_hms_opt(hour, minute, 0)?;
                    period(start, start + Duration::minutes(1))
                }
            }
        }
        _ => None,
    }
}

/// Parse a `datetime` instant: RFC 3339, or (a MeteoCore leniency) the same
/// with no offset, read as UTC, with or without seconds.
pub fn parse_instant(s: &str) -> Result<DateTime<Utc>, String> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M:%SZ", "%Y-%m-%dT%H:%M"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, format) {
            return Ok(dt.and_utc());
        }
    }
    Err(format!("Cannot parse datetime '{s}' as ISO 8601"))
}

/// An instant as RFC 3339 in UTC (`Z`), with fractional seconds only when
/// present: the form of `Content-Datetime` (`/req/core/map-response`).
pub fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn one(value: &str) -> Subset {
        let mut subsets = parse(&[value]).unwrap();
        assert_eq!(subsets.len(), 1, "{value}");
        subsets.remove(0)
    }

    #[test]
    fn parses_intervals_singles_stars_and_text() {
        assert_eq!(
            one("Lat(-90:90.5)"),
            Subset {
                axis: "Lat".into(),
                range: SubsetRange::Interval(SubsetValue::Number(-90.0), SubsetValue::Number(90.5)),
            }
        );
        assert_eq!(
            one(" Lon ( * : 1e2 ) ").range,
            SubsetRange::Interval(SubsetValue::Star, SubsetValue::Number(100.0))
        );
        assert_eq!(
            one(r#"time("2018-02-12T23:20:52Z")"#).range,
            SubsetRange::Single(SubsetValue::Text("2018-02-12T23:20:52Z".into()))
        );
        assert_eq!(
            one(r#"time("2018-02-12T00:00:00Z":*)"#).range,
            SubsetRange::Interval(
                SubsetValue::Text("2018-02-12T00:00:00Z".into()),
                SubsetValue::Star
            )
        );
        assert_eq!(one("h2(*)").range, SubsetRange::Single(SubsetValue::Star));
    }

    /// Comma-separated expressions and repeated parameters are one list
    /// (the standard's footnote example).
    #[test]
    fn several_expressions_and_repeated_parameters_are_one_list() {
        let joined = parse(&[r#"Lat(-90:90),Lon(-180:180),time("2018-02-12T23:20:52Z")"#]).unwrap();
        let repeated = parse(&[
            "Lat(-90:90)",
            "Lon(-180:180)",
            r#"time("2018-02-12T23:20:52Z")"#,
        ])
        .unwrap();
        assert_eq!(joined, repeated);
        assert_eq!(joined.len(), 3);
    }

    #[test]
    fn query_values_collects_every_repeat() {
        assert_eq!(
            query_values(Some("subset=Lat(1:2)&bbox=1&subset=Lon(3%3A4)"), "subset"),
            ["Lat(1:2)", "Lon(3:4)"]
        );
        assert!(query_values(None, "subset").is_empty());
    }

    #[test]
    fn malformed_expressions_are_errors() {
        for bad in [
            "",
            "Lat",
            "Lat(1:2",
            "Lat1:2)",
            "1Lat(1:2)",
            "La-t(1:2)",
            "Lat(1:2:3)",
            "Lat(a:2)",
            "Lat(NaN:2)",
            r#"time("2018)"#,
            r#"time("a"b")"#,
            "Lat(1:2),",
            "Lat(1:2))",
            "Lat(1:2)x",
        ] {
            assert!(parse(&[bad]).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn axes_match_aliases_without_case_and_reject_the_rest() {
        let axes = [
            Axis {
                name: "Lat",
                aliases: &["Latitude"],
            },
            Axis {
                name: "time",
                aliases: &["t"],
            },
        ];
        let ranges = by_axis(parse(&["LATITUDE(1:2),T(*)"]).unwrap(), &axes).unwrap();
        assert_eq!(ranges.keys().copied().collect::<Vec<_>>(), ["Lat", "time"]);
        let err = by_axis(parse(&["foo(1)"]).unwrap(), &axes).unwrap_err();
        assert_eq!(
            err,
            "subset axis 'foo' is not supported; valid axes: Lat, time"
        );
        let err = by_axis(parse(&["Lat(1:2),latitude(3:4)"]).unwrap(), &axes).unwrap_err();
        assert!(err.contains("more than once"), "{err}");
    }

    fn time(value: &str) -> Result<TimeSelection, String> {
        TimeSelection::from_subset(&one(value).range)
    }

    #[test]
    fn time_subsets_select_instants_periods_and_open_ranges() {
        assert_eq!(
            time(r#"time("2018-02-12T23:20:52Z")"#).unwrap(),
            TimeSelection::Instant(at("2018-02-12T23:20:52Z"))
        );
        assert_eq!(
            time("time(*)").unwrap(),
            TimeSelection::Range {
                start: None,
                end: None
            }
        );
        let range = |s: &str, e: &str| TimeSelection::Range {
            start: Some(at(s)),
            end: Some(at(e)),
        };
        assert_eq!(
            time(r#"time("2018")"#).unwrap(),
            range("2018-01-01T00:00:00Z", "2018-12-31T23:59:59.999999999Z")
        );
        assert_eq!(
            time(r#"time("2018-12")"#).unwrap(),
            range("2018-12-01T00:00:00Z", "2018-12-31T23:59:59.999999999Z")
        );
        assert_eq!(
            time(r#"time("2018-02-28")"#).unwrap(),
            range("2018-02-28T00:00:00Z", "2018-02-28T23:59:59.999999999Z")
        );
        assert_eq!(
            time(r#"time("2018-02-28T23Z")"#).unwrap(),
            range("2018-02-28T23:00:00Z", "2018-02-28T23:59:59.999999999Z")
        );
        assert_eq!(
            time(r#"time("2018-02-28T23:59Z")"#).unwrap(),
            range("2018-02-28T23:59:00Z", "2018-02-28T23:59:59.999999999Z")
        );
        // Partial bounds reach their period's start and end.
        assert_eq!(
            time(r#"time("2018-02":"2018-03")"#).unwrap(),
            range("2018-02-01T00:00:00Z", "2018-03-31T23:59:59.999999999Z")
        );
        assert_eq!(
            time(r#"time(*:"2018-03-18T12:31:12Z")"#).unwrap(),
            TimeSelection::Range {
                start: None,
                end: Some(at("2018-03-18T12:31:12Z"))
            }
        );
        for bad in [
            "time(2018)",
            r#"time("yesterday")"#,
            r#"time("2018-13")"#,
            r#"time("2018-02-30")"#,
            r#"time("2018-02-28T24Z")"#,
            r#"time("2018-03":"2018-02")"#,
        ] {
            assert!(time(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn datetime_parses_instants_and_intervals() {
        assert_eq!(
            TimeSelection::from_datetime("2018-02-12T23:20:50Z").unwrap(),
            TimeSelection::Instant(at("2018-02-12T23:20:50Z"))
        );
        assert_eq!(
            TimeSelection::from_datetime("2018-02-12T00:00:00Z/2018-03-18T12:31:12Z").unwrap(),
            TimeSelection::Range {
                start: Some(at("2018-02-12T00:00:00Z")),
                end: Some(at("2018-03-18T12:31:12Z"))
            }
        );
        for open in ["2018-02-12T00:00:00Z/..", "2018-02-12T00:00:00Z/"] {
            assert_eq!(
                TimeSelection::from_datetime(open).unwrap(),
                TimeSelection::Range {
                    start: Some(at("2018-02-12T00:00:00Z")),
                    end: None
                },
                "{open}"
            );
        }
        for open in ["../2018-03-18T12:31:12Z", "/2018-03-18T12:31:12Z"] {
            assert_eq!(
                TimeSelection::from_datetime(open).unwrap(),
                TimeSelection::Range {
                    start: None,
                    end: Some(at("2018-03-18T12:31:12Z"))
                },
                "{open}"
            );
        }
        assert!(TimeSelection::from_datetime("2018-03-18T00:00:00Z/2018-02-12T00:00:00Z").is_err());
        assert!(TimeSelection::from_datetime("not-a-time").is_err());
        assert!(TimeSelection::from_datetime("a/b/c").is_err());
    }

    #[test]
    fn latest_in_picks_the_latest_contained_time() {
        let times = [
            at("2024-01-01T00:00:00Z"),
            at("2024-01-01T01:00:00Z"),
            at("2024-01-01T02:00:00Z"),
        ];
        let range = |s: Option<&str>, e: Option<&str>| TimeSelection::Range {
            start: s.map(at),
            end: e.map(at),
        };
        assert_eq!(range(None, None).latest_in(&times), Some(times[2]));
        assert_eq!(
            range(None, Some("2024-01-01T01:30:00Z")).latest_in(&times),
            Some(times[1])
        );
        assert_eq!(
            range(Some("2024-01-01T00:10:00Z"), Some("2024-01-01T00:50:00Z")).latest_in(&times),
            None
        );
        assert_eq!(
            TimeSelection::Instant(times[0]).latest_in(&times),
            Some(times[0])
        );
        assert_eq!(range(None, None).latest_in(&[]), None);
    }

    #[test]
    fn rfc3339_is_utc_with_seconds() {
        assert_eq!(
            rfc3339(at("2024-01-01T01:00:00+02:00")),
            "2023-12-31T23:00:00Z"
        );
        assert_eq!(
            rfc3339(at("2024-01-01T01:00:00.5Z")),
            "2024-01-01T01:00:00.500Z"
        );
    }
}
