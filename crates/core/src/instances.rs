//! Model-run / forecast-reference-time support shared across engines (issue
//! #337).
//!
//! Forecast datasets have **two** time axes: the **model run** (a.k.a. forecast
//! *reference time* / init time / analysis time) and the **valid time** (run +
//! lead). OGC API - EDR exposes each run as an **instance**; OGC WMS exposes the
//! run as a custom `reference_time` dimension. This module is the single home
//! for the pieces every forecast engine needs, so GRIB, QueryData (and later
//! Zarr) select runs, build instance lists, and encode/decode instance ids
//! **identically** — no per-engine duplication.
//!
//! The contract for a forecast engine:
//! - store runs in a `BTreeMap<DateTime<Utc>, _>` keyed by reference time
//!   (ascending — so `iter().next_back()` is the latest run),
//! - resolve an incoming `reference_time: Option<DateTime<Utc>>` with
//!   [`select_run`] (`None` ⇒ latest),
//! - build [`EdrEngine::get_instances`](crate::edr_engine::EdrEngine::get_instances)
//!   with [`build_instances`].
//!
//! The API layer (`api-edr`) parses the `{instanceId}` path segment to a
//! reference time with [`parse_instance_id`] and formats instance ids with
//! [`format_instance_id`]; engines never touch the string form.

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDateTime, Utc};

/// One forecast model run, surfaced as an OGC API - EDR *instance*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunInfo {
    /// The model run / forecast reference time (init / analysis time, UTC).
    pub reference_time: DateTime<Utc>,
    /// The valid times produced by this run (`reference_time + lead`), ascending.
    pub valid_times: Vec<DateTime<Utc>>,
}

impl RunInfo {
    /// The instance id used in EDR URLs (the canonical compact UTC stamp, e.g.
    /// `20260607T0600Z`). Round-trips through [`parse_instance_id`].
    pub fn instance_id(&self) -> String {
        format_instance_id(self.reference_time)
    }

    /// This run's valid-time extent `(first, last)`, or `None` when the run has
    /// no valid times.
    pub fn temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        match (self.valid_times.first(), self.valid_times.last()) {
            (Some(&a), Some(&b)) => Some((a, b)),
            _ => None,
        }
    }
}

/// The canonical EDR instance id for a model run: a compact UTC stamp
/// `%Y%m%dT%H%MZ` (e.g. `20260607T0600Z`). URL- and filesystem-safe (no
/// colons), and stable across engines.
pub fn format_instance_id(reference_time: DateTime<Utc>) -> String {
    reference_time.format("%Y%m%dT%H%MZ").to_string()
}

/// Parse an EDR instance id back to a run reference time.
///
/// Accepts the canonical compact form emitted by [`format_instance_id`]
/// (`20260607T0600Z`, and the seconds variant `20260607T060000Z`) as well as
/// full RFC 3339 (`2026-06-07T06:00:00Z`), so clients that echo a run's
/// reference time verbatim still resolve. Returns `None` for anything else.
pub fn parse_instance_id(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%MZ") {
        return Some(dt.and_utc());
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%SZ") {
        return Some(dt.and_utc());
    }
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Select the run to serve from a reference-time-keyed map.
///
/// `None` ⇒ the latest run (largest key). `Some(rt)` ⇒ the run with exactly
/// that reference time, or `None` if absent (the API layer maps that to a 404).
/// This is the one selection rule every forecast engine uses, so "default to
/// latest" and "pick an instance" behave the same everywhere.
pub fn select_run<T>(
    runs: &BTreeMap<DateTime<Utc>, T>,
    reference_time: Option<DateTime<Utc>>,
) -> Option<(&DateTime<Utc>, &T)> {
    match reference_time {
        Some(rt) => runs.get_key_value(&rt),
        None => runs.iter().next_back(),
    }
}

/// Build the [`RunInfo`] list (ascending by reference time, latest last) from a
/// reference-time-keyed run map, deriving each run's valid times via `valid_times`.
///
/// Engines differ only in how they enumerate a run's valid times (GRIB:
/// `reference_time + step`; QueryData: the file's time axis), so they pass that
/// as a closure and share everything else.
pub fn build_instances<T>(
    runs: &BTreeMap<DateTime<Utc>, T>,
    valid_times: impl Fn(&DateTime<Utc>, &T) -> Vec<DateTime<Utc>>,
) -> Vec<RunInfo> {
    runs.iter()
        .map(|(rt, run)| RunInfo {
            reference_time: *rt,
            valid_times: valid_times(rt, run),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn dt(y: i32, mo: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, 0, 0).unwrap()
    }

    #[test]
    fn instance_id_round_trips_compact() {
        let rt = dt(2026, 6, 7, 6);
        let id = format_instance_id(rt);
        assert_eq!(id, "20260607T0600Z");
        assert_eq!(parse_instance_id(&id), Some(rt));
    }

    #[test]
    fn parse_accepts_rfc3339_and_seconds_variant() {
        let rt = dt(2026, 6, 7, 6);
        assert_eq!(parse_instance_id("2026-06-07T06:00:00Z"), Some(rt));
        assert_eq!(parse_instance_id("20260607T060000Z"), Some(rt));
    }

    #[test]
    fn parse_rejects_garbage() {
        assert_eq!(parse_instance_id("latest"), None);
        assert_eq!(parse_instance_id(""), None);
        assert_eq!(parse_instance_id("2026-06-07"), None);
    }

    #[test]
    fn select_run_none_is_latest() {
        let mut runs = BTreeMap::new();
        runs.insert(dt(2026, 6, 7, 0), "00Z");
        runs.insert(dt(2026, 6, 7, 12), "12Z");
        runs.insert(dt(2026, 6, 7, 6), "06Z");
        assert_eq!(select_run(&runs, None), Some((&dt(2026, 6, 7, 12), &"12Z")));
    }

    #[test]
    fn select_run_some_picks_exact_run() {
        let mut runs = BTreeMap::new();
        runs.insert(dt(2026, 6, 7, 0), "00Z");
        runs.insert(dt(2026, 6, 7, 12), "12Z");
        assert_eq!(
            select_run(&runs, Some(dt(2026, 6, 7, 0))),
            Some((&dt(2026, 6, 7, 0), &"00Z"))
        );
        // A run that isn't present resolves to None (→ 404 at the API layer).
        assert_eq!(select_run(&runs, Some(dt(2026, 6, 7, 6))), None);
    }

    #[test]
    fn select_run_empty_is_none() {
        let runs: BTreeMap<DateTime<Utc>, &str> = BTreeMap::new();
        assert_eq!(select_run(&runs, None), None);
        assert_eq!(select_run(&runs, Some(dt(2026, 6, 7, 0))), None);
    }

    #[test]
    fn build_instances_orders_and_derives_valid_times() {
        let mut runs = BTreeMap::new();
        runs.insert(dt(2026, 6, 7, 12), 2u32); // 2 leads
        runs.insert(dt(2026, 6, 7, 0), 3u32); // 3 leads
        let instances = build_instances(&runs, |rt, &n| {
            (0..n as i64)
                .map(|i| *rt + chrono::Duration::hours(i))
                .collect()
        });
        assert_eq!(instances.len(), 2);
        // Ascending by reference time, latest last.
        assert_eq!(instances[0].reference_time, dt(2026, 6, 7, 0));
        assert_eq!(instances[1].reference_time, dt(2026, 6, 7, 12));
        assert_eq!(instances[0].valid_times.len(), 3);
        assert_eq!(
            instances[1].temporal_extent(),
            Some((dt(2026, 6, 7, 12), dt(2026, 6, 7, 13)))
        );
        assert_eq!(instances[1].instance_id(), "20260607T1200Z");
    }
}
