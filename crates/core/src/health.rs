//! Live (post-boot) health signal an engine can expose.
//!
//! `/health` reports each collection's boot status (loaded / degraded /
//! failed). An engine with a long-lived upstream — a database, a WIS2 broker
//! session — additionally reports whether that upstream is currently
//! reachable so the status flips at runtime. Engines return `None` from their
//! `live_health()` until they have actually probed the upstream, so a
//! boot-degraded collection keeps its boot status instead of briefly flashing
//! `ready` from an optimistic seed.
//!
//! [`expected_interval`] is the spacing a collection's data arrives at, the
//! measure its data age is judged against (#1375).

use chrono::{DateTime, Duration, Utc};

/// Runtime health of one collection's upstream connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveStatus {
    Ready,
    /// Serving from the last good snapshot; `reason` is a short operator-facing
    /// phrase (`"broker disconnected"`) — never an error string with hosts or
    /// credentials in it.
    Degraded {
        reason: &'static str,
    },
    /// Serving, but the engine's in-memory store is still refilling from a
    /// push feed that only republishes what it holds now and then (#1000):
    /// it started empty, or was restored from a snapshot older than the
    /// warm-up, so whatever the feed published meanwhile is missing.
    /// Reported as degraded, with how much has arrived, until the engine's
    /// warm-up period is over. `items` names what is counted (`"alerts"`,
    /// `"reports"`).
    WarmingUp {
        received: u64,
        items: &'static str,
        cause: WarmupCause,
    },
}

/// Why a [`LiveStatus::WarmingUp`] engine is warming up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WarmupCause {
    /// Nothing to restore: no state store, no snapshot, or an unusable one.
    #[default]
    ColdStart,
    /// A snapshot was restored, but the server was down longer than the
    /// warm-up: the restored items are served, the gap refills.
    LongOutage,
}

impl LiveStatus {
    pub fn is_ready(self) -> bool {
        matches!(self, LiveStatus::Ready)
    }

    /// The operator-facing `/health` message of a non-ready status.
    pub fn degraded_reason(self) -> Option<String> {
        match self {
            LiveStatus::Ready => None,
            LiveStatus::Degraded { reason } => Some(reason.to_string()),
            LiveStatus::WarmingUp {
                received,
                items,
                cause,
            } => {
                let after = match cause {
                    WarmupCause::ColdStart => "cold start",
                    WarmupCause::LongOutage => "a long outage",
                };
                Some(format!(
                    "warming up after {after}: {received} {items} received"
                ))
            }
        }
    }
}

/// How many of a timeline's newest gaps [`expected_interval`] takes the
/// median of: the newest 25 minutes of a 5-minute radar. A model keeping
/// QueryData's and NetCDF's default four runs has three.
pub const EXPECTED_INTERVAL_GAPS: usize = 5;

/// The spacing at which a collection's data arrives (#1375), from the
/// newest timesteps of the timeline its data age is measured on, newest
/// first: the median of the gaps between the newest
/// [`EXPECTED_INTERVAL_GAPS`] + 1, the larger middle gap of an even count.
/// A missing frame or a late run widens one gap and leaves the median, and
/// the larger middle gap never makes an alert on the data age stricter.
/// `None` below two timesteps. Equal or ascending neighbours, which a
/// timeline sorted newest first does not have, are not gaps.
pub fn expected_interval(
    newest_first: impl IntoIterator<Item = DateTime<Utc>>,
) -> Option<Duration> {
    let newest: Vec<DateTime<Utc>> = newest_first
        .into_iter()
        .take(EXPECTED_INTERVAL_GAPS + 1)
        .collect();
    let mut gaps: Vec<Duration> = newest
        .windows(2)
        .map(|pair| pair[0] - pair[1])
        .filter(|gap| *gap > Duration::zero())
        .collect();
    gaps.sort_unstable();
    gaps.get(gaps.len() / 2).copied()
}

#[cfg(test)]
mod tests {
    use super::{expected_interval, LiveStatus, WarmupCause};
    use chrono::{DateTime, Duration, TimeZone, Utc};

    /// `minutes` after midnight, newest first.
    fn timeline(minutes: &[i64]) -> Vec<DateTime<Utc>> {
        let midnight = Utc.with_ymd_and_hms(2026, 10, 31, 0, 0, 0).unwrap();
        let mut times: Vec<_> = minutes
            .iter()
            .map(|&m| midnight + Duration::minutes(m))
            .collect();
        times.sort_unstable_by(|a, b| b.cmp(a));
        times
    }

    /// #1375: the median spacing of the newest timesteps, so one missing
    /// frame or a late run does not move it, and only the newest count.
    #[test]
    fn expected_interval_is_the_median_of_the_newest_gaps() {
        let interval = |minutes: &[i64]| expected_interval(timeline(minutes));
        // A 5-minute radar, one frame missing.
        assert_eq!(
            interval(&[0, 5, 10, 20, 25, 30, 35]),
            Some(Duration::minutes(5))
        );
        // A 6-hourly model's four runs, one of them missing.
        assert_eq!(interval(&[0, 360, 720, 1440]), Some(Duration::hours(6)));
        // Only the newest six timesteps: an hourly past is ignored.
        assert_eq!(
            interval(&[0, 60, 120, 180, 190, 200, 210, 220, 230]),
            Some(Duration::minutes(10))
        );
        // An even count takes the larger middle gap.
        assert_eq!(interval(&[0, 5, 15]), Some(Duration::minutes(10)));
        assert_eq!(interval(&[0, 5, 10, 15, 30]), Some(Duration::minutes(5)));
        // One timestep has no spacing, nor has none.
        assert_eq!(interval(&[0]), None);
        assert_eq!(interval(&[]), None);
        // Ascending input has no gaps.
        let mut ascending = timeline(&[0, 5, 10]);
        ascending.reverse();
        assert_eq!(expected_interval(ascending), None);
    }

    #[test]
    fn degraded_reason_names_the_warm_up_count() {
        assert_eq!(LiveStatus::Ready.degraded_reason(), None);
        assert_eq!(
            LiveStatus::Degraded {
                reason: "broker down"
            }
            .degraded_reason(),
            Some("broker down".into())
        );
        let warming = |cause| LiveStatus::WarmingUp {
            received: 42,
            items: "alerts",
            cause,
        };
        assert!(!warming(WarmupCause::ColdStart).is_ready());
        assert_eq!(
            warming(WarmupCause::ColdStart).degraded_reason(),
            Some("warming up after cold start: 42 alerts received".into())
        );
        assert_eq!(
            warming(WarmupCause::LongOutage).degraded_reason(),
            Some("warming up after a long outage: 42 alerts received".into())
        );
    }
}
