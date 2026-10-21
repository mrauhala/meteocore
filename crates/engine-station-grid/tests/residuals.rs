//! Leave-one-out residuals (#1206) and the Features collection that serves
//! them, over the repository's real station fixture (`testdata/weather.csv`
//! through the CSV engine) and an in-memory outlier layout. Values are
//! pinned against `tests/reference/loo_reference.py`, an independent stdlib
//! implementation:
//!
//! ```sh
//! python3 crates/engine-station-grid/tests/reference/loo_reference.py
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Duration, TimeZone, Utc};
use ds_core::config::{StationGridConfig, StationGridGeometry};
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::{
    parse_area_coords, Bbox, DatetimeInterval, Feature, FeaturePage, FeatureQuery, Geometry,
    PropertyValue, SortKey,
};
use ds_core::feature_engine::FeatureEngine;
use ds_core::map_engine::MapEngine;
use ds_core::model::{
    CoverageResponse, DomainDescription, LocationSnapshot, NdArray, ParameterDescription,
    QueryResult,
};
use engine_station_grid::residuals::Summary;
use engine_station_grid::StationGridEngine;

fn fixture() -> Arc<dyn EdrEngine> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/weather.csv");
    let store = engine_csv::CsvDataStore::load(path).expect("weather.csv loads");
    Arc::new(engine_csv::CsvEngine::new(store))
}

/// The reference's settings: up to 8 stations within 150 km; IDW power 2,
/// ADW exp(-d / 150 km)^4.
fn config(method: &str, parameters: &[&str], resolution: f64) -> StationGridConfig {
    StationGridConfig {
        source: "weather".into(),
        parameters: parameters.iter().map(|&p| p.into()).collect(),
        elevation_source: None,
        elevation_parameter: None,
        land_mask_source: None,
        land_mask_parameter: None,
        exclude_where: Default::default(),
        grid: StationGridGeometry {
            crs: "EPSG:4326".into(),
            bbox: [19.0, 59.0, 32.0, 71.0],
            resolution,
        },
        method: method.into(),
        power: None,
        correlation_length_km: (method == "adw").then_some(150.0),
        max_neighbours: 8,
        search_radius_km: 150.0,
        max_age: "PT1H".into(),
        cadence: Some("PT1H".into()),
        poll_interval_secs: 60,
        precompute: 1,
    }
}

fn utc(day: u32, hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, day, hour, 0, 0).unwrap()
}

fn instant(t: DateTime<Utc>) -> Option<DatetimeInterval> {
    Some(DatetimeInterval {
        start: Some(t),
        end: Some(t),
    })
}

fn query(datetime: Option<DatetimeInterval>) -> FeatureQuery {
    FeatureQuery {
        datetime,
        limit: 1000,
        ..Default::default()
    }
}

/// `get_features` as a caller with a request deadline: it waits for a grid
/// being computed.
fn features(engine: &StationGridEngine, query: &FeatureQuery) -> FeaturePage {
    let _deadline = ds_core::deadline::enter(Some(Instant::now() + StdDuration::from_secs(60)));
    engine.get_features(query).unwrap()
}

fn float(feature: &Feature, name: &str) -> Option<f64> {
    match feature.properties.get(name) {
        Some(PropertyValue::Float(v)) => Some(*v),
        Some(PropertyValue::Null) => None,
        other => panic!("{name}: {other:?}"),
    }
}

fn text<'a>(feature: &'a Feature, name: &str) -> &'a str {
    feature.properties[name]
        .as_str()
        .expect("a string property")
}

fn point(feature: &Feature) -> (f64, f64) {
    match feature.geometry.as_ref() {
        Geometry::Point { x, y } => (*x, *y),
        other => panic!("a point, not {other:?}"),
    }
}

fn rmse(features: &[Feature]) -> (usize, f64) {
    let residuals: Vec<f64> = features
        .iter()
        .filter_map(|f| float(f, "residual"))
        .collect();
    let sum: f64 = residuals.iter().map(|r| r * r).sum();
    (residuals.len(), (sum / residuals.len() as f64).sqrt())
}

fn largest(features: &[Feature]) -> &Feature {
    features
        .iter()
        .max_by(|a, b| {
            let a = float(a, "residual").unwrap_or(0.0).abs();
            let b = float(b, "residual").unwrap_or(0.0).abs();
            a.total_cmp(&b)
        })
        .expect("features")
}

fn close(got: f64, want: f64, what: &str) {
    assert!((got - want).abs() < 1e-9, "{what}: {got} vs {want}");
}

/// The reference at 2026-01-15T12:00Z: `(method, residuals, rmse,
/// (lon, lat, value, estimate, residual, neighbours) of the largest
/// |residual|)`.
type Worst = (f64, f64, f64, f64, f64, i64);
const FIXTURE: [(&str, usize, f64, Worst); 2] = [
    (
        "idw",
        192,
        0.7605811266459485,
        (
            20.8509,
            69.0428,
            -11.9,
            -7.800276548081752,
            4.099723451918249,
            3,
        ),
    ),
    (
        "adw",
        192,
        0.7580437671169754,
        (
            20.8509,
            69.0428,
            -11.9,
            -7.863451116859636,
            4.036548883140364,
            3,
        ),
    ),
];

/// Every station of the real fixture's slice has a residual, their RMSE is
/// the reference's, and so is the station the others support least, by
/// IDW and ADW. The per-analysis summary behind the gauge agrees.
#[test]
fn fixture_residuals_match_the_independent_reference() {
    let t = utc(15, 12);
    for (method, count, want_rmse, (lon, lat, value, estimate, residual, neighbours)) in FIXTURE {
        let engine = StationGridEngine::new(
            &format!("loo-{method}"),
            "weather",
            fixture(),
            &config(method, &["temperature"], 0.1),
        )
        .unwrap();
        let page = features(&engine, &query(instant(t)));
        assert_eq!(page.number_matched, 192, "{method}");
        let (n, got_rmse) = rmse(&page.features);
        assert_eq!(n, count, "{method}");
        close(got_rmse, want_rmse, method);
        let worst = largest(&page.features);
        assert_eq!(point(worst), (lon, lat), "{method}");
        assert_eq!(float(worst, "value"), Some(value));
        close(float(worst, "estimate").unwrap(), estimate, method);
        close(float(worst, "residual").unwrap(), residual, method);
        assert_eq!(
            worst.properties["neighbours"],
            PropertyValue::Integer(neighbours),
            "{method}"
        );
        assert!(page
            .features
            .iter()
            .all(|f| text(f, "time") == "2026-01-15T12:00:00Z"
                && text(f, "parameter") == "temperature"));

        let summaries = engine.residual_summaries();
        assert_eq!(summaries.len(), 1);
        let (parameter, Some(Summary { count, rmse })) = &summaries[0] else {
            panic!("a summary after an analysis");
        };
        assert_eq!((parameter.as_str(), *count), ("temperature", 192));
        close(rmse.unwrap(), want_rmse, method);
    }
}

/// `tests/reference/loo_reference.py`'s `LATTICE`: a plane field on a
/// jittered 5 × 5 lattice, with 6.0 added at the centre, 25 E 60.5 N.
const LATTICE: [(f64, f64, f64); 25] = [
    (24.019, 59.984, 3.04),
    (24.021, 60.219, 2.69),
    (24.002, 60.499, 2.25),
    (24.012, 60.74, 1.9),
    (23.971, 60.99, 1.49),
    (24.539, 59.974, 3.47),
    (24.476, 60.261, 2.99),
    (24.535, 60.539, 2.62),
    (24.536, 60.747, 2.31),
    (24.505, 61.01, 1.89),
    (24.97, 60.016, 3.75),
    (25.026, 60.288, 3.39),
    (25.0, 60.5, 9.05),
    (24.979, 60.746, 2.66),
    (25.025, 61.019, 2.29),
    (25.461, 59.961, 4.23),
    (25.534, 60.213, 3.91),
    (25.512, 60.48, 3.49),
    (25.507, 60.751, 3.08),
    (25.488, 60.975, 2.73),
    (26.018, 60.017, 4.59),
    (25.977, 60.288, 4.15),
    (26.04, 60.491, 3.9),
    (26.025, 60.72, 3.54),
    (26.013, 61.004, 3.1),
];
const OUTLIER: (f64, f64) = (25.0, 60.5);

/// The reference over `LATTICE`: `(method, outlier estimate, outlier
/// residual, largest |residual| elsewhere, rmse)`.
const OUTLIER_REFERENCE: [(&str, f64, f64, f64, f64); 3] = [
    (
        "idw",
        3.038980247817607,
        -6.011019752182394,
        1.4025432627066698,
        1.33017885447002,
    ),
    (
        "adw",
        3.045419608719366,
        -6.004580391280635,
        1.0404504892497095,
        1.3201191127940488,
    ),
    ("nearest", 3.39, -5.66, 5.66, 1.9762955244598417),
];

/// A station source held in memory: every `LATTICE` station reports once,
/// at `t`.
struct Lattice {
    t: DateTime<Utc>,
}

impl EdrEngine for Lattice {
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

    fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
        HashMap::new()
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((self.t, self.t))
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
        let reports = start <= self.t && self.t <= end;
        Ok(CoverageResponse::Collection(
            LATTICE
                .iter()
                .filter(|(lon, lat, _)| reports && polygon.contains(*lon, *lat))
                .map(|&(x, y, value)| QueryResult {
                    domain: DomainDescription::PointSeries {
                        x,
                        y,
                        t: vec![self.t],
                        z: None,
                    },
                    parameters: HashMap::new(),
                    ranges: HashMap::from([(
                        "t2m".to_string(),
                        NdArray {
                            shape: vec![1],
                            axis_names: vec!["t".into()],
                            values: vec![Some(value)],
                        },
                    )]),
                })
                .collect(),
        ))
    }
}

fn lattice_engine(method: &str) -> StationGridEngine {
    let mut config = config(method, &["t2m"], 0.1);
    config.source = "lattice".into();
    config.grid.bbox = [23.0, 59.0, 27.0, 62.0];
    StationGridEngine::new(
        &format!("lattice-{method}"),
        "lattice",
        Arc::new(Lattice { t: utc(10, 6) }),
        &config,
    )
    .unwrap()
}

/// The issue's acceptance case: in a layout with one outlier, the largest
/// |residual| is at the outlier (by IDW and ADW; the nearest-station method
/// gives the outlier and its nearest neighbour equal and opposite ones),
/// and it sorts first by `residual`. Every value is the reference's.
#[test]
fn an_outlier_has_the_largest_residual() {
    for (method, estimate, residual, elsewhere, want_rmse) in OUTLIER_REFERENCE {
        let engine = lattice_engine(method);
        let page = features(&engine, &query(None));
        assert_eq!(page.number_matched, 25, "{method}");
        let outlier = page
            .features
            .iter()
            .find(|f| point(f) == OUTLIER)
            .expect("the outlier");
        assert_eq!(float(outlier, "value"), Some(9.05));
        close(float(outlier, "estimate").unwrap(), estimate, method);
        close(float(outlier, "residual").unwrap(), residual, method);
        let others: Vec<Feature> = page
            .features
            .iter()
            .filter(|f| point(f) != OUTLIER)
            .cloned()
            .collect();
        close(
            float(largest(&others), "residual").unwrap().abs(),
            elsewhere,
            method,
        );
        if method != "nearest" {
            assert_eq!(point(largest(&page.features)), OUTLIER, "{method}");
        }
        close(rmse(&page.features).1, want_rmse, method);

        let sorted = features(
            &engine,
            &FeatureQuery {
                sortby: vec![SortKey::ascending("residual")],
                limit: 1,
                ..Default::default()
            },
        );
        assert_eq!(point(&sorted.features[0]), OUTLIER, "{method}");
        assert_eq!(sorted.number_matched, 25);
        assert_eq!(sorted.next_offset, Some(1));
    }
}

/// `datetime` selects analysis times, the latest without one; an interval
/// reads every analysis time in it, at most 96 analyses (times ×
/// parameters, a `parameter` filter narrowing them).
#[test]
fn datetime_selects_analysis_times_under_the_cap() {
    let engine = StationGridEngine::new(
        "loo-times",
        "weather",
        fixture(),
        &config("nearest", &["temperature", "wind_speed"], 0.5),
    )
    .unwrap();
    let times = |page: &FeaturePage| {
        let mut times: Vec<String> = page
            .features
            .iter()
            .map(|f| text(f, "time").to_string())
            .collect();
        times.dedup();
        times
    };

    let latest = features(&engine, &query(None));
    assert_eq!(times(&latest), vec!["2026-01-31T23:00:00Z"]);
    let parameters: Vec<&str> = latest
        .features
        .iter()
        .map(|f| text(f, "parameter"))
        .collect();
    assert!(parameters.contains(&"temperature") && parameters.contains(&"wind_speed"));

    let t = utc(15, 12);
    let at_t = features(&engine, &query(instant(t)));
    assert_eq!(times(&at_t), vec!["2026-01-15T12:00:00Z"]);
    // Between analysis times: none.
    let between = features(&engine, &query(instant(t + Duration::minutes(30))));
    assert_eq!((between.number_matched, between.features.len()), (0, 0));

    // An interval: every analysis time in it, ends included, in order.
    let interval = |start, end| {
        query(Some(DatetimeInterval {
            start: Some(start),
            end: Some(end),
        }))
    };
    let mut three = interval(t - Duration::hours(2), t);
    three.property_filters = vec![("parameter".into(), "temperature".into())];
    let page = features(&engine, &three);
    assert_eq!(
        times(&page),
        vec![
            "2026-01-15T10:00:00Z",
            "2026-01-15T11:00:00Z",
            "2026-01-15T12:00:00Z"
        ]
    );
    assert!(page
        .features
        .iter()
        .all(|f| text(f, "parameter") == "temperature"));

    // 49 times × 2 parameters is over the cap, before anything is read;
    // one parameter is not.
    let two_days = interval(t - Duration::hours(48), t);
    let analyses = engine.metrics().0;
    let too_many = engine.get_features(&two_days).unwrap_err();
    assert!(
        matches!(&too_many, DataServerError::QueryTooLarge(m)
            if m.contains("98 analyses (49 analysis times × 2 parameters)")
                && m.contains("the limit is 96")),
        "{too_many}"
    );
    assert_eq!(engine.metrics().0, analyses);
    let mut one_parameter = two_days.clone();
    one_parameter.property_filters = vec![("parameter".into(), "wind_speed".into())];
    one_parameter.limit = 1;
    let page = features(&engine, &one_parameter);
    assert_eq!(page.number_returned, 1);
    assert!(page.number_matched > 49, "{}", page.number_matched);
    assert_eq!(text(&page.features[0], "time"), "2026-01-13T12:00:00Z");
}

/// Paging, `bbox`, property filters and `sortby` follow the Features
/// rules: filters before counting, sorting before paging; ids read back.
#[test]
fn paging_filters_and_ids() {
    let engine = StationGridEngine::new(
        "loo-paging",
        "weather",
        fixture(),
        &config("idw", &["temperature"], 0.5),
    )
    .unwrap();
    let t = utc(15, 12);
    let all = features(&engine, &query(instant(t)));
    assert_eq!(all.number_matched, 192);

    // Pages of the natural order are slices of the whole.
    let mut page_query = query(instant(t));
    page_query.limit = 50;
    page_query.offset = 150;
    let page = features(&engine, &page_query);
    assert_eq!((page.number_matched, page.number_returned), (192, 42));
    assert_eq!(page.next_offset, None);
    let ids = |features: &[Feature]| features.iter().map(|f| f.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(&page.features), ids(&all.features[150..]));
    page_query.offset = 0;
    let first = features(&engine, &page_query);
    assert_eq!(first.next_offset, Some(50));
    assert_eq!(ids(&first.features), ids(&all.features[..50]));

    // Sorted before paging: the second page continues the first.
    let mut sorted = query(instant(t));
    sorted.sortby = vec![SortKey::descending("residual")];
    let whole = features(&engine, &sorted);
    sorted.limit = 10;
    sorted.offset = 10;
    let second = features(&engine, &sorted);
    assert_eq!(ids(&second.features), ids(&whole.features[10..20]));
    let residuals: Vec<f64> = whole
        .features
        .iter()
        .map(|f| float(f, "residual").unwrap())
        .collect();
    assert!(residuals.windows(2).all(|w| w[0] >= w[1]));

    // `bbox` and property filters before counting.
    let bbox = Bbox::new(24.0, 60.0, 26.0, 61.0).unwrap();
    let mut boxed = query(instant(t));
    boxed.bbox = Some(bbox);
    let inside = features(&engine, &boxed);
    let want = all
        .features
        .iter()
        .filter(|f| {
            let (x, y) = point(f);
            bbox.contains(x, y)
        })
        .count();
    assert!(want > 0);
    assert_eq!(inside.number_matched, want);
    let mut filtered = query(instant(t));
    filtered.property_filters = vec![("neighbours".into(), "8".into())];
    let eight = features(&engine, &filtered);
    assert!(eight.number_matched > 0 && eight.number_matched <= 192);
    assert!(eight
        .features
        .iter()
        .all(|f| f.properties["neighbours"] == PropertyValue::Integer(8)));

    // Every id reads back as its feature; ids are unique.
    let mut unique = ids(&all.features);
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 192);
    let one = &all.features[7];
    let _deadline = ds_core::deadline::enter(Some(Instant::now() + StdDuration::from_secs(60)));
    let again = engine.get_feature(&one.id).unwrap();
    assert_eq!(again.id, one.id);
    assert_eq!(again.properties, one.properties);
    for missing in [
        "temperature@2026-01-15T12:00:00Z@1,2".to_string(),
        "temperature@2026-01-15T12:30:00Z@20.8509,69.0428".to_string(),
        "humidity@2026-01-15T12:00:00Z@20.8509,69.0428".to_string(),
        "nonsense".to_string(),
    ] {
        assert!(
            matches!(
                engine.get_feature(&missing),
                Err(DataServerError::FeatureNotFound(_))
            ),
            "{missing}"
        );
    }
    assert_eq!(
        engine.label(one).as_deref(),
        Some(format!("temperature at {}, {}", point(one).1, point(one).0).as_str())
    );
}

/// A request with no deadline (a Features request on a request worker)
/// never waits: a cold analysis is `ResourceExhausted` (503) at once, its
/// computation goes on, and a later request is served from it without
/// computing again.
#[test]
fn a_miss_without_a_deadline_is_busy_then_served() {
    let engine = StationGridEngine::new(
        "loo-cold",
        "weather",
        fixture(),
        &config("idw", &["temperature"], 0.1),
    )
    .unwrap();
    let cold = query(instant(utc(20, 6)));
    let started = Instant::now();
    assert!(matches!(
        engine.get_features(&cold),
        Err(DataServerError::ResourceExhausted)
    ));
    assert!(started.elapsed() < StdDuration::from_secs(1), "no wait");
    let stations = features(&engine, &cold).number_matched;
    assert!(stations > 100, "{stations}");
    assert_eq!(engine.metrics().0, 1, "one computation served both");
    // Cached now: served without a deadline too.
    assert_eq!(engine.get_features(&cold).unwrap().number_matched, stations);
    // Metadata never computes: the count comes from the latest analysis.
    assert_eq!(engine.feature_count(), stations);
    assert_eq!(engine.metrics().0, 1);
    assert!(engine.has_time_dimension());
    assert_eq!(engine.data_version(), engine.content_version());
}
