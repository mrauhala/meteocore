//! Which frames a window holds, and what changes from one poll to the next
//! (#1379). Pure functions over frame times, no I/O.
//!
//! A window of length `L` ends at the source's latest frame `T` and holds
//! every frame `t` with `T - L <= t < T`. A frame counts for its spacing to
//! the next frame, capped at the source's cadence: a missed frame leaves its
//! interval uncovered instead of stretching its predecessor over it. The
//! window's coverage is the time its frames count for over `L`, so on an
//! even cadence it is the share of the window's frames present.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

/// The frames a window holds: frame time → the seconds it counts for.
pub type Ledger = BTreeMap<DateTime<Utc>, u32>;

/// The frames of the window of `length` ending at the last of `times`
/// (ascending, no repeats), each with the seconds it counts for: its
/// spacing to the next frame, at most `cap`.
pub fn target(times: &[DateTime<Utc>], length: Duration, cap: Duration) -> Ledger {
    let Some(&end) = times.last() else {
        return Ledger::new();
    };
    let start = end - length;
    let cap = cap.num_seconds().max(1);
    let first = times.partition_point(|t| *t < start);
    times[first..]
        .windows(2)
        .filter_map(|pair| {
            let secs = (pair[1] - pair[0]).num_seconds().clamp(0, cap);
            (secs > 0).then_some((pair[0], secs as u32))
        })
        .collect()
}

/// The share of a window of `length` its frames count for, in `[0, 1]`.
pub fn coverage(ledger: &Ledger, length: Duration) -> f64 {
    let covered: u64 = ledger.values().map(|&secs| u64::from(secs)).sum();
    let length = length.num_seconds().max(1) as f64;
    (covered as f64 / length).min(1.0)
}

/// Whether the source holds the history the window ending at the last of
/// `times` needs: back to the window's start, and `margin` before it. With
/// a margin of one frame, the frame the next poll takes out is still there
/// to be read; a source that keeps only the window drops each frame while
/// it is still in the window, where it could not be taken out.
pub fn holds(times: &[DateTime<Utc>], length: Duration, margin: Duration) -> bool {
    match (times.first(), times.last()) {
        (Some(&first), Some(&last)) => first + margin <= last - length,
        _ => false,
    }
}

/// How a ledger differs from its target.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Diff {
    /// Frames to take out, with the seconds they were added with.
    pub removed: Vec<(DateTime<Utc>, u32)>,
    /// Frames to put in, with the seconds they count for.
    pub added: Vec<(DateTime<Utc>, u32)>,
    /// Frames in both whose seconds changed (a late frame arrived after
    /// one, or a gap closed): `(time, old, new)`.
    pub retimed: Vec<(DateTime<Utc>, u32, u32)>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.added.is_empty() && self.retimed.is_empty()
    }
}

/// What turns `ledger` into `target`.
pub fn diff(ledger: &Ledger, target: &Ledger) -> Diff {
    let mut out = Diff::default();
    for (&t, &secs) in ledger {
        match target.get(&t) {
            None => out.removed.push((t, secs)),
            Some(&new) if new != secs => out.retimed.push((t, secs, new)),
            Some(_) => {}
        }
    }
    for (&t, &secs) in target {
        if !ledger.contains_key(&t) {
            out.added.push((t, secs));
        }
    }
    out
}

/// The source's frame spacing: the median of its newest gaps
/// ([`ds_core::health::expected_interval`], the estimator `/metrics`
/// uses), kept at `current` unless it moved by more than a quarter, so the
/// jitter of irregular scan times does not retime every frame each poll.
pub fn cadence(times: &[DateTime<Utc>], current: Option<Duration>) -> Option<Duration> {
    let estimate = ds_core::health::expected_interval(times.iter().rev().copied());
    match (current, estimate) {
        (Some(current), Some(estimate)) => {
            let moved = (estimate - current).num_milliseconds().abs();
            Some(if moved * 4 > current.num_milliseconds() {
                estimate
            } else {
                current
            })
        }
        (current, estimate) => estimate.or(current),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(minute: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap() + Duration::minutes(minute)
    }

    fn times(minutes: &[i64]) -> Vec<DateTime<Utc>> {
        minutes.iter().map(|&m| at(m)).collect()
    }

    /// A window ending at T holds the frames in [T − L, T), each for its
    /// spacing to the next.
    #[test]
    fn a_window_holds_the_frames_before_its_end() {
        let t = times(&[0, 5, 10, 15, 20, 25, 30]);
        let ledger = target(&t, Duration::minutes(15), Duration::minutes(5));
        assert_eq!(
            ledger.into_iter().collect::<Vec<_>>(),
            vec![(at(15), 300), (at(20), 300), (at(25), 300)]
        );
        let full = target(&t, Duration::minutes(15), Duration::minutes(5));
        assert_eq!(coverage(&full, Duration::minutes(15)), 1.0);
    }

    /// A missed frame leaves its interval uncovered: its predecessor counts
    /// for one cadence, not the whole gap.
    #[test]
    fn a_gap_is_capped_at_the_cadence() {
        let t = times(&[0, 5, 10, 20, 25, 30]);
        let ledger = target(&t, Duration::minutes(30), Duration::minutes(5));
        assert_eq!(ledger[&at(10)], 300);
        assert_eq!(ledger.len(), 5);
        let cov = coverage(&ledger, Duration::minutes(30));
        assert!((cov - 25.0 / 30.0).abs() < 1e-12, "{cov}");
    }

    #[test]
    fn a_late_frame_retimes_its_predecessor() {
        let before = target(
            &times(&[0, 5, 15, 20]),
            Duration::minutes(20),
            Duration::minutes(10),
        );
        let after = target(
            &times(&[0, 5, 10, 15, 20]),
            Duration::minutes(20),
            Duration::minutes(10),
        );
        let d = diff(&before, &after);
        assert_eq!(d.added, vec![(at(10), 300)]);
        assert_eq!(d.retimed, vec![(at(5), 600, 300)]);
        assert!(d.removed.is_empty());
        assert!(diff(&after, &after).is_empty());
    }

    /// The window's history, and with a margin one frame before it.
    #[test]
    fn holding_the_window_and_a_margin() {
        let cap = Duration::minutes(5);
        let hour = Duration::hours(1);
        let t: Vec<_> = (0..=13).map(|i| at(i * 5)).collect();
        assert!(holds(&t, hour, cap));
        assert!(!holds(&t[1..], hour, cap));
        assert!(holds(&t[1..], hour, Duration::zero()));
        assert!(!holds(&t[2..], hour, Duration::zero()));
        assert!(!holds(&[], hour, cap));
    }

    #[test]
    fn cadence_follows_the_median_and_ignores_jitter() {
        let five = Duration::minutes(5);
        assert_eq!(cadence(&times(&[0, 5, 10, 20, 25]), None), Some(five));
        assert_eq!(cadence(&times(&[0]), None), None);
        assert_eq!(cadence(&times(&[0]), Some(five)), Some(five));
        // 5 min 30 s is jitter around 5 min; 10 min is a new cadence.
        let jitter: Vec<_> = (0..6).map(|i| at(0) + Duration::seconds(330 * i)).collect();
        assert_eq!(cadence(&jitter, Some(five)), Some(five));
        assert_eq!(
            cadence(&times(&[0, 10, 20, 30]), Some(five)),
            Some(Duration::minutes(10))
        );
    }
}
