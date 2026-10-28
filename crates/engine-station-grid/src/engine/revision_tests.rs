//! Revisions and the time axis (#1206): a revision found by a request's own
//! computation, late reports for times older than the `precompute` window,
//! and analysis times from the source's own observation instants.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, TimeZone, Utc};
use ds_core::config::{StationGridConfig, StationGridGeometry};
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::parse_area_coords;
use ds_core::map_engine::MapEngine;
use ds_core::model::{
    CoverageResponse, DomainDescription, LocationSnapshot, NdArray, QueryResult, ValuePrecision,
};

use super::{StationGridEngine, RECHECK_PER_CYCLE};
use crate::compute::CACHE;

/// One station: `(lon, lat, [(time, value)])`.
type Obs = (f64, f64, Vec<(DateTime<Utc>, f64)>);

/// An in-memory station source that logs every slice read (the end of its
/// window) and lists its instants when given some.
struct Source {
    stations: Mutex<Vec<Obs>>,
    instants: Mutex<Option<Arc<[DateTime<Utc>]>>>,
    reads: Mutex<Vec<DateTime<Utc>>>,
}

impl Source {
    fn new(stations: Vec<Obs>) -> Arc<Self> {
        Arc::new(Self {
            stations: Mutex::new(stations),
            instants: Mutex::new(None),
            reads: Mutex::new(Vec::new()),
        })
    }

    /// A late report: `value` at `time` from a new station at `(lon, lat)`.
    fn report(&self, lon: f64, lat: f64, time: DateTime<Utc>, value: f64) {
        self.stations
            .lock()
            .unwrap()
            .push((lon, lat, vec![(time, value)]));
    }

    /// The times read since the last call.
    fn take_reads(&self) -> Vec<DateTime<Utc>> {
        std::mem::take(&mut *self.reads.lock().unwrap())
    }
}

impl EdrEngine for Source {
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        Ok(LocationSnapshot::empty())
    }

    fn query_location(
        &self,
        id: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Err(DataServerError::LocationNotFound(id.into()))
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["t2m".into()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let stations = self.stations.lock().unwrap();
        let times = stations.iter().flat_map(|s| s.2.iter().map(|(t, _)| *t));
        Some((times.clone().min()?, times.max()?))
    }

    fn observation_instants(&self) -> Option<Arc<[DateTime<Utc>]>> {
        self.instants.lock().unwrap().clone()
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        None
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["area".into()]
    }

    fn serves_station_series(&self) -> bool {
        true
    }

    fn query_area(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let polygon = parse_area_coords(coords)?;
        let (start, end) = datetime.expect("the engine always sends a window");
        self.reads.lock().unwrap().push(end);
        let stations = self.stations.lock().unwrap();
        Ok(CoverageResponse::Collection(
            stations
                .iter()
                .filter(|(lon, lat, _)| polygon.contains(*lon, *lat))
                .map(|(lon, lat, obs)| {
                    let obs: Vec<_> = obs
                        .iter()
                        .filter(|(t, _)| start <= *t && *t <= end)
                        .collect();
                    QueryResult {
                        domain: DomainDescription::PointSeries {
                            x: *lon,
                            y: *lat,
                            t: obs.iter().map(|(t, _)| *t).collect(),
                            z: None,
                        },
                        parameters: HashMap::new(),
                        ranges: HashMap::from([(
                            "t2m".to_string(),
                            NdArray {
                                shape: vec![obs.len()],
                                axis_names: vec!["t".into()],
                                values: obs.iter().map(|(_, v)| Some(*v)).collect(),
                                precision: ValuePrecision::F64,
                            },
                        )]),
                    }
                })
                .collect(),
        ))
    }
}

/// Hour `h` of 2026-01-10.
fn hour(h: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 10, 0, 0, 0).unwrap() + Duration::hours(h)
}

/// Two stations reporting every hour from 00 to `last`.
fn hourly(last: i64) -> Arc<Source> {
    let series = |base: f64| (0..=last).map(|h| (hour(h), base + h as f64)).collect();
    Source::new(vec![(24.0, 60.0, series(0.0)), (26.0, 60.0, series(10.0))])
}

fn config(precompute: usize) -> StationGridConfig {
    StationGridConfig {
        source: "obs".into(),
        parameters: vec!["t2m".into()],
        elevation_source: None,
        elevation_parameter: None,
        land_mask_source: None,
        land_mask_parameter: None,
        exclude_where: Default::default(),
        grid: StationGridGeometry {
            crs: "CRS:84".into(),
            bbox: [22.0, 58.0, 28.0, 62.0],
            resolution: 0.1,
        },
        method: "idw".into(),
        power: None,
        correlation_length_km: None,
        max_neighbours: 8,
        search_radius_km: 300.0,
        max_age: "PT1H".into(),
        cadence: Some("PT1H".into()),
        poll_interval_secs: 60,
        precompute,
    }
}

fn engine(source: &Arc<Source>, config: &StationGridConfig) -> StationGridEngine {
    let source: Arc<dyn EdrEngine> = source.clone();
    StationGridEngine::new("revisions", "obs", source, config).unwrap()
}

/// The analysed value at the cell holding 24.95°E 60.05°N.
fn value_at(engine: &StationGridEngine, t: DateTime<Utc>) -> f64 {
    let response = engine
        .query_position("POINT(24.95 60.05)", Some((t, t)), None, None, None)
        .unwrap();
    let CoverageResponse::Single(result) = response else {
        panic!("one coverage");
    };
    result.ranges["t2m"].values[0].unwrap()
}

fn analyses(engine: &StationGridEngine) -> u64 {
    engine.metrics().0
}

/// A request whose own computation finds the input revised keeps the grid
/// under the new revision, so the next request is a cache hit, not a
/// second computation (the #1205 review).
#[test]
fn a_revision_found_by_a_request_is_cached_under_it() {
    let source = hourly(3);
    let engine = engine(&source, &config(0));
    let t = hour(2);
    let before = value_at(&engine, t);
    assert_eq!(analyses(&engine), 1);
    let version = engine.content_version();

    // A late report for `t` while its grid is out of the cache: only the
    // next request's computation can find the revision.
    source.report(25.0, 60.0, t, 40.0);
    CACHE.remove(&engine.inner.key(0, t));
    let after = value_at(&engine, t);
    assert_eq!(analyses(&engine), 2);
    assert!(after > before + 1.0, "{before} → {after}");
    assert!(engine.content_version() > version);

    // Served from the cache under the new revision.
    assert_eq!(value_at(&engine, t), after);
    assert_eq!(analyses(&engine), 2, "the second request hits the cache");
}

/// A late report for a time older than the `precompute` window whose grid
/// is cached is found by the poll, without a request reading the source:
/// the revision moves the content version and the new grid is computed
/// ahead. An older time without a cached grid is not read.
#[test]
fn the_poll_rechecks_older_cached_times() {
    let source = hourly(11);
    let engine = engine(&source, &config(1));
    engine.poll_once();
    assert_eq!(source.take_reads(), [hour(11)]);
    assert_eq!(analyses(&engine), 1);
    let (old, older) = (hour(5), hour(9));
    let stale = value_at(&engine, old);
    value_at(&engine, older);
    assert_eq!(analyses(&engine), 3);
    let version = engine.content_version();

    // Nothing new: the latest time and the two cached older ones are read
    // again, and nothing moves.
    source.take_reads();
    engine.poll_once();
    assert_eq!(source.take_reads(), [hour(11), older, old]);
    assert_eq!(engine.content_version(), version);
    assert_eq!(analyses(&engine), 3);

    // Late reports for a cached older time and for an uncached one.
    source.report(25.0, 60.0, old, 40.0);
    source.report(25.0, 60.0, hour(3), 40.0);
    engine.poll_once();
    let reads = source.take_reads();
    assert!(
        reads.contains(&old) && !reads.contains(&hour(3)),
        "{reads:?}"
    );
    assert!(engine.content_version() > version);
    assert_eq!(analyses(&engine), 4, "the revised grid is computed ahead");
    let fresh = value_at(&engine, old);
    assert!(fresh > stale + 1.0, "{stale} → {fresh}");
    assert_eq!(analyses(&engine), 4, "and served from the cache");
    assert!(source.take_reads().is_empty(), "no request read the source");
}

/// The poll re-reads at most `RECHECK_PER_CYCLE` older cached times a
/// cycle, newest first and round-robin, so every one is re-read within
/// ⌈cached / RECHECK_PER_CYCLE⌉ cycles; uncached times are never read.
#[test]
fn rechecks_are_bounded_and_round_robin() {
    let source = hourly(23);
    let engine = engine(&source, &config(1));
    // Twenty older analyses cached by one EDR query: hours 2 to 21.
    let cached: BTreeSet<DateTime<Utc>> = (2..=21).map(hour).collect();
    engine
        .query_position(
            "POINT(24.95 60.05)",
            Some((hour(2), hour(21))),
            None,
            None,
            None,
        )
        .unwrap();
    assert_eq!(analyses(&engine), 20);
    source.take_reads();

    let rounds = cached.len().div_ceil(RECHECK_PER_CYCLE);
    let mut rechecked = BTreeSet::new();
    for round in 0..rounds {
        engine.poll_once();
        let reads = source.take_reads();
        assert_eq!(reads[0], hour(23), "the precompute window first");
        assert_eq!(reads.len(), 1 + RECHECK_PER_CYCLE, "round {round}");
        if round == 0 {
            let newest: Vec<_> = (14..=21).rev().map(hour).collect();
            assert_eq!(reads[1..], newest[..], "newest first");
        }
        rechecked.extend(reads[1..].iter().copied());
    }
    assert_eq!(rechecked, cached, "each cached older time, and no other");
    assert_eq!(analyses(&engine), 21, "only the latest time was computed");
    assert_eq!(engine.content_version(), 1);
}

/// Without a `cadence` the analysis times are the source's own instants:
/// the CSV fixture's hours, giving the same grids as `cadence = "PT1H"`,
/// and an irregular source's every instant, followed as it grows. A source
/// that lists none still needs a cadence.
#[test]
fn without_a_cadence_the_axis_is_the_source_instants() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/weather.csv");
    let csv: Arc<dyn EdrEngine> = Arc::new(engine_csv::CsvEngine::new(
        engine_csv::CsvDataStore::load(path).unwrap(),
    ));
    let mut grid = config(0);
    grid.parameters = vec!["temperature".into()];
    grid.grid.bbox = [19.0, 59.0, 32.0, 71.0];
    grid.search_radius_km = 150.0;
    let cadenced = StationGridEngine::new("csv-cadence", "weather", csv.clone(), &grid).unwrap();
    grid.cadence = None;
    let instants = StationGridEngine::new("csv-instants", "weather", csv, &grid).unwrap();
    let times = instants.raster_info().times;
    assert_eq!(times.len(), 31 * 24);
    assert_eq!(times, cadenced.raster_info().times);
    let t = Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
    let at = |e: &StationGridEngine| {
        let CoverageResponse::Single(r) = e
            .query_position("POINT(24.95 60.25)", Some((t, t)), None, None, None)
            .unwrap()
        else {
            panic!("one coverage");
        };
        r.ranges["temperature"].values[0]
    };
    assert_eq!(at(&instants), at(&cadenced));

    // An irregular source: an off-hour report is an analysis time.
    let source = hourly(2);
    let off_hour = hour(1) + Duration::minutes(20);
    *source.instants.lock().unwrap() = Some(Arc::from([hour(0), hour(1), off_hour]));
    let mut irregular = config(0);
    irregular.cadence = None;
    let engine = engine(&source, &irregular);
    assert_eq!(engine.raster_info().times, [hour(0), hour(1), off_hour]);
    assert_eq!(
        engine.resolve_time(Some(off_hour + Duration::minutes(5)), None),
        Some(off_hour)
    );
    *source.instants.lock().unwrap() = Some(Arc::from([hour(0), hour(1), off_hour, hour(2)]));
    engine.poll_once();
    assert_eq!(engine.resolve_time(None, None), Some(hour(2)));

    // A source that lists no instants needs a cadence.
    let silent = hourly(2);
    let error = StationGridEngine::new("silent", "obs", silent, &irregular)
        .err()
        .expect("no cadence, no instants")
        .to_string();
    assert!(error.contains("cadence"), "{error}");
}
