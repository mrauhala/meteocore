//! In-memory, time-windowed observation store.
//!
//! One `StationSeries` per station id; one dense `f32` row per report time
//! (`NaN` = missing), columns fixed by the `ParameterTable`. Bounded by
//! `retention` × cadence × `max_stations` (a global SYNOP subscription is
//! ~20 k stations × 24 hourly rows × ~22 columns ≈ 50 MB). Ingest happens
//! under a write lock per report — microseconds — and readers (request
//! handlers) take the read lock for one station lookup or one area scan.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};

use crate::decode::ObsReport;
use crate::params::ParameterTable;

#[derive(Debug, Clone, PartialEq)]
pub struct StationInfo {
    pub id: Arc<str>,
    pub name: Option<String>,
    pub lat: f64,
    pub lon: f64,
    pub elevation: Option<f64>,
    pub first_report: DateTime<Utc>,
    pub last_report: DateTime<Utc>,
    /// Receipt time of the newest report (eviction order under `max_stations`).
    pub last_seen: DateTime<Utc>,
}

#[derive(Debug)]
pub struct StationSeries {
    pub info: StationInfo,
    pub rows: BTreeMap<DateTime<Utc>, Box<[f32]>>,
}

/// What `ingest` did with a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ingest {
    /// New station or new report time.
    Inserted,
    /// Same station + time already held — row replaced (update / correction).
    Replaced,
    /// Report time outside the accepted window.
    OutOfWindow,
}

#[derive(Debug)]
pub struct ObsStore {
    stations: HashMap<Arc<str>, StationSeries>,
    retention: Duration,
    max_stations: usize,
    /// Total rows held (kept incrementally; a full count is O(stations)).
    rows: usize,
}

/// Reports newer than `now + FUTURE_SLACK` are rejected (clock skew / bad
/// encodings); older than `now − retention − PAST_SLACK` likewise.
const FUTURE_SLACK: Duration = Duration::hours(1);
const PAST_SLACK: Duration = Duration::hours(1);

impl ObsStore {
    pub fn new(retention: Duration, max_stations: usize) -> Self {
        ObsStore {
            stations: HashMap::new(),
            retention,
            max_stations: max_stations.max(1),
            rows: 0,
        }
    }

    pub fn station_count(&self) -> usize {
        self.stations.len()
    }

    pub fn row_count(&self) -> usize {
        self.rows
    }

    pub fn get(&self, id: &str) -> Option<&StationSeries> {
        self.stations.get(id)
    }

    pub fn stations(&self) -> impl Iterator<Item = &StationSeries> {
        self.stations.values()
    }

    /// Add a report. `now` is the receipt time. Station metadata (name,
    /// position, elevation) follows the newest report.
    pub fn ingest(
        &mut self,
        report: &ObsReport,
        table: &ParameterTable,
        now: DateTime<Utc>,
    ) -> Ingest {
        if report.time > now + FUTURE_SLACK || report.time < now - self.retention - PAST_SLACK {
            return Ingest::OutOfWindow;
        }
        let row = table.row(report);
        let id: Arc<str> = Arc::from(report.station_id.as_str());
        let entry = self
            .stations
            .entry(id.clone())
            .or_insert_with(|| StationSeries {
                info: StationInfo {
                    id,
                    name: report.name.clone(),
                    lat: report.lat,
                    lon: report.lon,
                    elevation: report.elevation,
                    first_report: report.time,
                    last_report: report.time,
                    last_seen: now,
                },
                rows: BTreeMap::new(),
            });
        let info = &mut entry.info;
        info.last_seen = now;
        if report.time >= info.last_report {
            info.last_report = report.time;
            info.lat = report.lat;
            info.lon = report.lon;
            if report.name.is_some() {
                info.name = report.name.clone();
            }
            if report.elevation.is_some() {
                info.elevation = report.elevation;
            }
        }
        if report.time < info.first_report {
            info.first_report = report.time;
        }
        match entry.rows.insert(report.time, row) {
            Some(_) => Ingest::Replaced,
            None => {
                self.rows += 1;
                Ingest::Inserted
            }
        }
    }

    /// Drop rows older than `retention`, stations left with no rows, and —
    /// beyond `max_stations` — the least recently reporting stations.
    /// Returns `(rows_dropped, stations_dropped)`.
    pub fn prune(&mut self, now: DateTime<Utc>) -> (usize, usize) {
        let cutoff = now - self.retention;
        let mut rows_dropped = 0usize;
        for s in self.stations.values_mut() {
            let keep = s.rows.split_off(&cutoff);
            rows_dropped += s.rows.len();
            s.rows = keep;
            if let Some((&t, _)) = s.rows.iter().next() {
                s.info.first_report = t;
            }
        }
        let before = self.stations.len();
        self.stations.retain(|_, s| !s.rows.is_empty());
        if self.stations.len() > self.max_stations {
            let mut by_age: Vec<(DateTime<Utc>, Arc<str>)> = self
                .stations
                .values()
                .map(|s| (s.info.last_seen, s.info.id.clone()))
                .collect();
            by_age.sort();
            let excess = self.stations.len() - self.max_stations;
            for (_, id) in by_age.into_iter().take(excess) {
                if let Some(s) = self.stations.remove(&id) {
                    rows_dropped += s.rows.len();
                }
            }
        }
        self.rows = self.rows.saturating_sub(rows_dropped);
        (rows_dropped, before - self.stations.len())
    }

    /// Remove one report (WIS2 `rel=deletion`). Returns whether it existed.
    pub fn remove(&mut self, station_id: &str, time: DateTime<Utc>) -> bool {
        let Some(s) = self.stations.get_mut(station_id) else {
            return false;
        };
        let removed = s.rows.remove(&time).is_some();
        if removed {
            self.rows -= 1;
            if s.rows.is_empty() {
                self.stations.remove(station_id);
            } else {
                if let Some((&t, _)) = s.rows.iter().next() {
                    s.info.first_report = t;
                }
                if let Some((&t, _)) = s.rows.iter().next_back() {
                    s.info.last_report = t;
                }
            }
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::Element;
    use chrono::TimeZone;
    use tinybufr::XY;

    fn t(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 12, h, 0, 0).unwrap()
    }

    fn report(id: &str, h: u32, temp: f64) -> ObsReport {
        ObsReport {
            station_id: id.into(),
            name: Some(format!("S{id}")),
            lat: 60.0,
            lon: 25.0,
            elevation: Some(10.0),
            time: t(h),
            elements: vec![Element {
                xy: XY { x: 12, y: 101 },
                value: temp,
                period_hours: None,
            }],
        }
    }

    #[test]
    fn ingest_replace_window_and_prune() {
        let table = ParameterTable::build(true, &[]);
        let mut s = ObsStore::new(Duration::hours(24), 10);
        let now = t(12);
        assert_eq!(
            s.ingest(&report("A", 8, 280.0), &table, now),
            Ingest::Inserted
        );
        assert_eq!(
            s.ingest(&report("A", 8, 281.0), &table, now),
            Ingest::Replaced
        );
        assert_eq!(
            s.ingest(&report("A", 9, 282.0), &table, now),
            Ingest::Inserted
        );
        assert_eq!(
            s.ingest(&report("B", 9, 270.0), &table, now),
            Ingest::Inserted
        );
        assert_eq!(s.row_count(), 3);
        assert_eq!(s.station_count(), 2);
        let a = s.get("A").unwrap();
        assert_eq!(a.rows[&t(8)][0], 281.0);
        assert_eq!(a.info.first_report, t(8));
        assert_eq!(a.info.last_report, t(9));

        // Future / too old.
        assert_eq!(
            s.ingest(&report("C", 14, 1.0), &table, now),
            Ingest::OutOfWindow
        );
        let old = Utc.with_ymd_and_hms(2026, 9, 11, 9, 0, 0).unwrap();
        let mut r = report("C", 0, 1.0);
        r.time = old;
        assert_eq!(s.ingest(&r, &table, now), Ingest::OutOfWindow);

        // Prune at +24 h from the 08Z row: it goes, 09Z rows stay.
        let later = Utc.with_ymd_and_hms(2026, 9, 13, 8, 30, 0).unwrap();
        let (rows, stations) = s.prune(later);
        assert_eq!((rows, stations), (1, 0));
        assert_eq!(s.get("A").unwrap().info.first_report, t(9));
        // Another hour and everything is gone.
        let (rows, stations) = s.prune(later + Duration::hours(1));
        assert_eq!((rows, stations), (2, 2));
        assert_eq!(s.row_count(), 0);
    }

    #[test]
    fn max_stations_evicts_least_recently_seen() {
        let table = ParameterTable::build(true, &[]);
        let mut s = ObsStore::new(Duration::hours(24), 2);
        s.ingest(&report("A", 8, 1.0), &table, t(8));
        s.ingest(&report("B", 8, 1.0), &table, t(9));
        s.ingest(&report("C", 8, 1.0), &table, t(10));
        s.prune(t(11));
        assert!(s.get("A").is_none());
        assert!(s.get("B").is_some() && s.get("C").is_some());
    }

    #[test]
    fn remove_single_report() {
        let table = ParameterTable::build(true, &[]);
        let mut s = ObsStore::new(Duration::hours(24), 10);
        s.ingest(&report("A", 8, 1.0), &table, t(9));
        s.ingest(&report("A", 9, 1.0), &table, t(9));
        assert!(s.remove("A", t(9)));
        assert_eq!(s.get("A").unwrap().info.last_report, t(8));
        assert!(!s.remove("A", t(9)));
        assert!(s.remove("A", t(8)));
        assert!(s.get("A").is_none());
        assert_eq!(s.row_count(), 0);
    }
}
