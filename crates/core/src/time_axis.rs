//! Long time axes described as ranges instead of lists (#1006).
//!
//! An archive store can expose every historical forecast run: one had about
//! 7300 runs, which WMS advertised as a 206 810-character `reference_time`
//! `<Dimension>` and EDR as one instance document per run. This module is the
//! one place that decides when an axis is too long to list
//! ([`MAX_LISTED_VALUES`], [`is_long`]): WMS then writes it as ranges, and
//! EDR pages its instances list instead of answering every run at once.
//!
//! - an axis of at most [`MAX_LISTED_VALUES`] values is a plain list, written
//!   exactly as before, so the capabilities of every layer below the
//!   threshold stay byte-identical;
//! - a longer axis is split into segments: each stretch of at least three
//!   values one whole-second step apart becomes a range, every other value
//!   stays a single value. A regular axis is one range, a gap starts a new
//!   one, and an irregular axis stays a list of single values.
//!
//! The description is lossless: expanding the segments in order gives back
//! every value, and nothing else, so a value inside a range is a value of the
//! axis and resolves exactly as it would from the list. [`wms_extent`] writes
//! it in WMS 1.3.0 Annex C notation: `min/max/resolution` for a range,
//! comma-separated with the single values in axis order, e.g.
//! `2021-05-01T00:00:00+00:00/2026-10-09T18:00:00+00:00/PT6H`.

use chrono::{DateTime, Utc};

use crate::datetime::format_iso8601_duration;

/// The longest axis written as a plain list; a longer one is described with
/// ranges.
///
/// 500 RFC 3339 values are about 13 KB of text. Every observation and
/// forecast axis served today stays below it: a day of 5-minute radar frames
/// is 288 values, six hours of 1-minute lightning 361, a forecast's lead
/// times at most a few hundred, and an operational model keeps a handful of
/// runs. What it catches is an archive's run axis, hundreds to thousands of
/// runs (967 and about 7300 in the stores that prompted it), plus any
/// observation axis long enough to dominate a capabilities document.
pub const MAX_LISTED_VALUES: usize = 500;

/// The fewest equally spaced values written as one range. Two values cost
/// about as much either way, so they stay single values.
const MIN_RANGE_LEN: usize = 3;

/// One piece of a time axis: a single value, or a run of equally spaced ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Segment {
    /// A value that starts no range.
    Value(DateTime<Utc>),
    /// `count` values (at least three): `start`, then one `step_seconds`
    /// apart.
    Range {
        start: DateTime<Utc>,
        step_seconds: i64,
        count: usize,
    },
}

impl Segment {
    /// The first value.
    fn start(&self) -> DateTime<Utc> {
        match *self {
            Segment::Value(t) => t,
            Segment::Range { start, .. } => start,
        }
    }

    /// The last value.
    fn end(&self) -> DateTime<Utc> {
        match *self {
            Segment::Value(t) => t,
            Segment::Range {
                start,
                step_seconds,
                count,
            } => start + chrono::Duration::seconds(step_seconds * (count as i64 - 1)),
        }
    }

    /// The step as an ISO 8601 duration (`PT6H`); `None` for a single value.
    fn period(&self) -> Option<String> {
        match *self {
            Segment::Value(_) => None,
            Segment::Range { step_seconds, .. } => format_iso8601_duration(step_seconds),
        }
    }
}

/// Whether `times` is longer than [`MAX_LISTED_VALUES`]: WMS then writes it
/// as ranges, and an EDR run axis that long pages its instances list.
pub fn is_long(times: &[DateTime<Utc>]) -> bool {
    times.len() > MAX_LISTED_VALUES
}

/// Split `times`, an axis in ascending order, into ranges and single values.
///
/// Greedy and in axis order: from each value, the longest stretch of equal
/// whole-second steps becomes a [`Segment::Range`] when it holds at least
/// three values, else the value is a [`Segment::Value`]. A step with a
/// fraction of a second, a repeat or a step backwards never extends a range,
/// so the segments expand to exactly `times`, whatever its order.
fn segments(times: &[DateTime<Utc>]) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < times.len() {
        if let Some(step) = times
            .get(i + 1)
            .and_then(|&next| whole_step(times[i], next))
        {
            let mut j = i + 1;
            while j + 1 < times.len() && whole_step(times[j], times[j + 1]) == Some(step) {
                j += 1;
            }
            let count = j - i + 1;
            if count >= MIN_RANGE_LEN {
                out.push(Segment::Range {
                    start: times[i],
                    step_seconds: step,
                    count,
                });
                i = j + 1;
                continue;
            }
        }
        out.push(Segment::Value(times[i]));
        i += 1;
    }
    out
}

/// The step from `a` to `b` in seconds, when it is a positive whole number of
/// seconds.
fn whole_step(a: DateTime<Utc>, b: DateTime<Utc>) -> Option<i64> {
    let step = b - a;
    (step.subsec_nanos() == 0 && step.num_seconds() > 0).then(|| step.num_seconds())
}

/// The text of a WMS 1.3.0 `<Dimension>` for a time axis (Annex C).
///
/// Up to [`MAX_LISTED_VALUES`] values: the comma-separated list of RFC 3339
/// values, as WMS always wrote it. A longer axis: its segments in axis
/// order, a range as `min/max/resolution` and a single value as itself,
/// comma-separated.
pub fn wms_extent(times: &[DateTime<Utc>]) -> String {
    if !is_long(times) {
        return join_values(times);
    }
    segments(times)
        .iter()
        .map(|segment| match segment.period() {
            Some(period) => format!(
                "{}/{}/{period}",
                segment.start().to_rfc3339(),
                segment.end().to_rfc3339()
            ),
            None => segment.start().to_rfc3339(),
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn join_values(times: &[DateTime<Utc>]) -> String {
    times
        .iter()
        .map(|t| t.to_rfc3339())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datetime::parse_iso8601_duration;
    use chrono::{Duration, TimeZone};

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2021, 5, 1, 0, 0, 0).unwrap()
    }

    /// `n` values from `start`, `step` apart.
    fn regular(start: DateTime<Utc>, step: Duration, n: usize) -> Vec<DateTime<Utc>> {
        (0..n as i32).map(|i| start + step * i).collect()
    }

    /// Expand a WMS Annex C extent back to its values.
    fn expand_wms(extent: &str) -> Vec<DateTime<Utc>> {
        let mut out = Vec::new();
        for item in extent.split(',') {
            let parts: Vec<&str> = item.split('/').collect();
            match parts.as_slice() {
                [value] => out.push(parse(value)),
                [min, max, resolution] => {
                    let (min, max) = (parse(min), parse(max));
                    let step = parse_iso8601_duration(resolution).unwrap();
                    let mut t = min;
                    while t <= max {
                        out.push(t);
                        t += step;
                    }
                }
                _ => panic!("not Annex C: {item}"),
            }
        }
        out
    }

    fn parse(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .unwrap_or_else(|e| panic!("{s}: {e}"))
            .with_timezone(&Utc)
    }

    #[test]
    fn short_axes_keep_the_list_byte_for_byte() {
        // At the threshold an axis is still the old comma-joined list, even
        // when it is perfectly regular.
        let times = regular(t0(), Duration::hours(6), MAX_LISTED_VALUES);
        let list: Vec<String> = times.iter().map(|t| t.to_rfc3339()).collect();
        assert_eq!(wms_extent(&times), list.join(","));
        assert!(!is_long(&times));
        assert_eq!(wms_extent(&[]), "");
    }

    #[test]
    fn a_regular_long_axis_is_one_range() {
        // An archive's run axis: every 6 h for five and a half years.
        let times = regular(t0(), Duration::hours(6), 7952);
        assert_eq!(
            segments(&times),
            [Segment::Range {
                start: t0(),
                step_seconds: 6 * 3600,
                count: 7952
            }]
        );
        assert_eq!(
            wms_extent(&times),
            "2021-05-01T00:00:00+00:00/2026-10-09T18:00:00+00:00/PT6H"
        );
        assert_eq!(expand_wms(&wms_extent(&times)), times);
    }

    #[test]
    fn one_past_the_threshold_switches_to_ranges() {
        let times = regular(t0(), Duration::minutes(5), MAX_LISTED_VALUES + 1);
        assert!(is_long(&times));
        assert_eq!(segments(&times).len(), 1);
        assert!(!wms_extent(&times).contains(','));
    }

    #[test]
    fn gaps_start_new_ranges_and_lone_values_stay_values() {
        // 300 six-hourly runs, two missing runs, 300 more, one stray run
        // 13 h later, then 3-hourly runs from 5 h after it: a gap or a change
        // of cadence starts a new range, and the stray run belongs to none.
        let first = regular(t0(), Duration::hours(6), 300);
        let resume = *first.last().unwrap() + Duration::hours(18);
        let second = regular(resume, Duration::hours(6), 300);
        let stray = *second.last().unwrap() + Duration::hours(13);
        let third = regular(stray + Duration::hours(5), Duration::hours(3), 4);
        let times: Vec<_> = first
            .iter()
            .chain(&second)
            .chain([&stray])
            .chain(&third)
            .copied()
            .collect();
        let segs = segments(&times);
        assert_eq!(
            segs,
            [
                Segment::Range {
                    start: t0(),
                    step_seconds: 6 * 3600,
                    count: 300
                },
                Segment::Range {
                    start: resume,
                    step_seconds: 6 * 3600,
                    count: 300
                },
                Segment::Value(stray),
                Segment::Range {
                    start: third[0],
                    step_seconds: 3 * 3600,
                    count: 4
                },
            ]
        );
        let wms = wms_extent(&times);
        assert_eq!(
            wms,
            format!(
                "2021-05-01T00:00:00+00:00/{}/PT6H,{}/{}/PT6H,{},{}/{}/PT3H",
                first[299].to_rfc3339(),
                resume.to_rfc3339(),
                second[299].to_rfc3339(),
                stray.to_rfc3339(),
                third[0].to_rfc3339(),
                third[3].to_rfc3339()
            )
        );
        assert_eq!(expand_wms(&wms), times);
    }

    #[test]
    fn an_irregular_long_axis_stays_a_list() {
        // Volume scans a few seconds off the nominal cadence: no two steps
        // match, so every value is single and the text is the list.
        let mut t = t0();
        let times: Vec<_> = (0..MAX_LISTED_VALUES as i64 + 50)
            .map(|i| {
                t += Duration::seconds(300 + i % 7);
                t
            })
            .collect();
        assert!(segments(&times)
            .iter()
            .all(|s| matches!(s, Segment::Value(_))));
        assert_eq!(wms_extent(&times), join_values(&times));
    }

    #[test]
    fn two_equal_steps_make_a_range_but_one_does_not() {
        let a = regular(t0(), Duration::hours(1), 2);
        assert_eq!(segments(&a), [Segment::Value(a[0]), Segment::Value(a[1])]);
        let b = regular(t0(), Duration::hours(1), 3);
        assert_eq!(
            segments(&b),
            [Segment::Range {
                start: t0(),
                step_seconds: 3600,
                count: 3
            }]
        );
    }

    #[test]
    fn sub_second_steps_repeats_and_disorder_never_form_ranges() {
        let half = Duration::milliseconds(500);
        let fractional = regular(t0(), half, 4);
        assert!(segments(&fractional)
            .iter()
            .all(|s| matches!(s, Segment::Value(_))));
        let repeated = vec![t0(), t0(), t0()];
        assert_eq!(segments(&repeated).len(), 3);
        let backwards = regular(t0(), Duration::hours(-1), 3);
        assert_eq!(segments(&backwards).len(), 3);
        // Values with a fraction keep it, and whole steps still join them.
        let offset = regular(t0() + half, Duration::seconds(1), 3);
        assert_eq!(segments(&offset)[0].end(), offset[2]);
    }

    #[test]
    fn day_and_compound_periods_round_trip() {
        let mut times = regular(t0(), Duration::days(1), 400);
        let tail = *times.last().unwrap() + Duration::hours(25);
        times.extend(regular(tail, Duration::minutes(90), 200));
        let wms = wms_extent(&times);
        assert_eq!(
            wms,
            format!(
                "2021-05-01T00:00:00+00:00/{}/P1D,{}/{}/PT1H30M",
                times[399].to_rfc3339(),
                tail.to_rfc3339(),
                times[599].to_rfc3339()
            )
        );
        assert_eq!(expand_wms(&wms), times);
    }
}
