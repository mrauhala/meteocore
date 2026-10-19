//! End to end over the repository's real station fixture (`testdata/
//! weather.csv`, 194 FMI stations, hourly, January 2026) through the public
//! CSV engine, plus synthetic sources for the antimeridian, the pole, late
//! observations and the request deadline.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Duration, TimeZone, Utc};
use ds_core::config::{StationGridConfig, StationGridGeometry};
use ds_core::cube::CubeResolution;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::{parse_area_coords, Bbox};
use ds_core::map_engine::{MapEngine, OutputCrs, RasterTile, RasterValues};
use ds_core::model::{
    CoverageResponse, DomainDescription, LocationSnapshot, NdArray, ParameterDescription,
    QueryResult,
};
use engine_station_grid::StationGridEngine;

fn fixture() -> Arc<dyn EdrEngine> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/weather.csv");
    let store = engine_csv::CsvDataStore::load(path).expect("weather.csv loads");
    Arc::new(engine_csv::CsvEngine::new(store))
}

fn config(method: &str) -> StationGridConfig {
    StationGridConfig {
        source: "weather".into(),
        parameters: vec!["temperature".into()],
        grid: StationGridGeometry {
            crs: "EPSG:4326".into(),
            bbox: [19.0, 59.0, 32.0, 71.0],
            resolution: 0.1,
        },
        method: method.into(),
        power: None,
        correlation_length_km: None,
        max_neighbours: 8,
        search_radius_km: 150.0,
        max_age: "PT1H".into(),
        cadence: Some("PT1H".into()),
        poll_interval_secs: 60,
        precompute: 2,
    }
}

fn utc(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, day, hour, minute, 0).unwrap()
}

fn instant(t: DateTime<Utc>) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    Some((t, t))
}

fn single(response: CoverageResponse) -> QueryResult {
    match response {
        CoverageResponse::Single(result) => result,
        CoverageResponse::Collection(_) => panic!("expected one coverage"),
    }
}

fn f32_values(tile: &RasterTile) -> &[f32] {
    match &tile.values {
        RasterValues::F32 { data, .. } => data,
        _ => panic!("station grids render as F32"),
    }
}

fn position(engine: &StationGridEngine, lon: f64, lat: f64, t: DateTime<Utc>) -> Option<f64> {
    let result = single(
        engine
            .query_position(&format!("POINT({lon} {lat})"), instant(t), None, None, None)
            .unwrap(),
    );
    let values = &result.ranges.values().next().unwrap().values;
    assert_eq!(values.len(), 1);
    values[0]
}

/// The independent stdlib Python implementation's estimates at cell centres
/// of the test grid, 2026-01-15T12:00Z: `(lon, lat, idw, adw)`, printed by
/// `tests/reference/idw_adw_reference.py`. The engine stores grids as
/// `f32`, so they agree to 1e-4 °C (an `f32` of a value under 16 is within
/// 1e-6 of it; the rest is summation order).
const REFERENCE: [(f64, f64, f64, f64); 5] = [
    (24.95, 60.25, -5.967775680681713, -5.896184262690398),
    (25.45, 62.15, -6.64421602443666, -6.6290391557712365),
    (27.65, 64.95, -7.389332217712177, -7.325134861601059),
    (22.05, 63.85, -5.599879043971707, -5.590011191976788),
    (28.05, 69.95, -8.458189996147595, -8.57062303770603),
];

#[test]
fn matches_the_independent_reference() {
    let t = utc(15, 12, 0);
    let idw = StationGridEngine::new("t-idw", "weather", fixture(), &config("idw")).unwrap();
    let mut adw_config = config("adw");
    adw_config.correlation_length_km = Some(150.0);
    let adw = StationGridEngine::new("t-adw", "weather", fixture(), &adw_config).unwrap();
    for (lon, lat, want_idw, want_adw) in REFERENCE {
        let got_idw = position(&idw, lon, lat, t).unwrap();
        let got_adw = position(&adw, lon, lat, t).unwrap();
        assert!(
            (got_idw - want_idw).abs() < 1e-4,
            "IDW at {lon},{lat}: {got_idw} vs {want_idw}"
        );
        assert!(
            (got_adw - want_adw).abs() < 1e-4,
            "ADW at {lon},{lat}: {got_adw} vs {want_adw}"
        );
    }
}

#[test]
fn csv_fixture_serves_maps_and_edr() {
    let engine = StationGridEngine::new("t2m", "weather", fixture(), &config("idw")).unwrap();
    assert!(engine.has_data());
    assert!(engine.provenance().contains("'weather'"));

    let info = engine.raster_info();
    assert_eq!(info.times.len(), 31 * 24, "hourly over January");
    assert_eq!(info.times.first(), Some(&utc(1, 0, 0)));
    assert_eq!(info.grid_size, Some([130, 120]));
    assert_eq!(info.parameters.len(), 1);
    assert_eq!(info.parameters[0].unit, "°C");
    assert_eq!(engine.content_version(), 1);

    // Time snaps to the latest analysis not after it, in every resolver.
    let t = utc(15, 12, 0);
    assert_eq!(engine.resolve_time(Some(utc(15, 12, 40)), None), Some(t));
    assert_eq!(engine.resolve_time(None, None), Some(utc(31, 23, 0)));

    // A map over the grid's own bbox at its own size: one pixel per cell,
    // the values EDR answers.
    let tile = engine
        .get_raster_tile(
            [19.0, 59.0, 32.0, 71.0],
            130,
            120,
            Some(utc(15, 12, 40)),
            &OutputCrs::Wgs84,
            Some("temperature"),
            None,
            None,
        )
        .unwrap();
    let pixels = f32_values(&tile);
    for (lon, lat, _, _) in REFERENCE {
        let (col, row) = (((lon - 19.0) / 0.1) as usize, ((71.0 - lat) / 0.1) as usize);
        let edr = position(&engine, lon, lat, t).unwrap();
        assert_eq!(pixels[row * 130 + col], edr as f32, "{lon},{lat}");
    }
    // The grid's south-east corner, east of the Gulf of Finland, has no
    // station within 150 km.
    assert!(pixels[119 * 130 + 129].is_nan());

    // Web Mercator and a projected CRS sample the same grid.
    let mercator = engine
        .get_raster_tile(
            [20.0, 60.0, 30.0, 70.0],
            256,
            256,
            Some(t),
            &OutputCrs::WebMercator,
            None,
            None,
            None,
        )
        .unwrap();
    assert!(
        f32_values(&mercator)
            .iter()
            .filter(|v| v.is_finite())
            .count()
            > 30_000
    );
    let tm35 = ds_core::geo::projected_output_crs("EPSG:3067").unwrap();
    let projected = engine
        .get_raster_tile(
            [20.0, 59.5, 33.0, 70.5],
            256,
            256,
            Some(t),
            &OutputCrs::Projected {
                crs: tm35,
                bbox: [100_000.0, 6_600_000.0, 750_000.0, 7_800_000.0],
            },
            None,
            None,
            None,
        )
        .unwrap();
    let finite: Vec<f32> = f32_values(&projected)
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .collect();
    assert!(finite.len() > 20_000, "{}", finite.len());
    assert!(
        finite.iter().all(|v| (-45.0..10.0).contains(v)),
        "January temperatures"
    );

    // EDR area: a grid at the analysis resolution, with its time axis.
    let area = single(
        engine
            .query_area("24,60,26,62", instant(t), None, None, None)
            .unwrap(),
    );
    let DomainDescription::Grid { x, y, t: times, .. } = &area.domain else {
        panic!("area is a Grid");
    };
    assert_eq!((x.len(), y.len()), (20, 20));
    assert_eq!(times.as_deref(), Some(&[t][..]));
    let values = &area.ranges["temperature"];
    assert_eq!(values.shape, vec![1, 20, 20]);
    let ix = x.iter().position(|v| (v - 24.95).abs() < 1e-9).unwrap();
    let iy = y.iter().position(|v| (v - 60.25).abs() < 1e-9).unwrap();
    assert_eq!(
        values.values[iy * 20 + ix],
        position(&engine, 24.95, 60.25, t)
    );

    // Radius, through the default polygon path.
    let radius = single(
        engine
            .query_radius("POINT(25 62)", 50_000.0, instant(t), None, None, None)
            .unwrap(),
    );
    assert!(radius.ranges["temperature"]
        .values
        .iter()
        .any(Option::is_some));

    // Cube: the native cells inside the bbox, or `resolution` positions.
    let bbox = Bbox::new(24.0, 60.0, 25.0, 61.0).unwrap();
    let cube = single(
        engine
            .query_cube(
                &bbox,
                instant(t),
                None,
                None,
                CubeResolution::default(),
                None,
            )
            .unwrap(),
    );
    let DomainDescription::Grid { x, y, .. } = &cube.domain else {
        panic!("cube is a Grid");
    };
    assert_eq!((x.len(), y.len()), (10, 10));
    assert!((x[0] - 24.05).abs() < 1e-9 && (y[0] - 60.95).abs() < 1e-9);
    let resampled = single(
        engine
            .query_cube(
                &bbox,
                instant(t),
                None,
                None,
                CubeResolution {
                    x: Some(3),
                    y: Some(4),
                    z: None,
                },
                None,
            )
            .unwrap(),
    );
    assert_eq!(resampled.ranges["temperature"].shape, vec![1, 4, 3]);
}

#[test]
fn edr_time_and_parameter_rules() {
    let engine =
        StationGridEngine::new("t2m-rules", "weather", fixture(), &config("nearest")).unwrap();
    // No datetime: the latest analysis.
    let latest = single(
        engine
            .query_position("POINT(25 62)", None, None, None, None)
            .unwrap(),
    );
    let DomainDescription::PointSeries { t, .. } = &latest.domain else {
        panic!("position is a PointSeries");
    };
    assert_eq!(t, &vec![utc(31, 23, 0)]);
    // An instant must be an analysis time.
    assert!(matches!(
        engine.query_position("POINT(25 62)", instant(utc(15, 12, 30)), None, None, None),
        Err(DataServerError::LocationNotFound(_))
    ));
    // An interval reads every analysis time inside it.
    let series = single(
        engine
            .query_position(
                "POINT(25 62)",
                Some((utc(15, 10, 0), utc(15, 12, 0))),
                None,
                None,
                None,
            )
            .unwrap(),
    );
    assert_eq!(series.ranges["temperature"].values.len(), 3);
    // A whole month is more analyses than one query may read.
    assert!(matches!(
        engine.query_position(
            "POINT(25 62)",
            Some((utc(1, 0, 0), utc(31, 23, 0))),
            None,
            None,
            None
        ),
        Err(DataServerError::QueryTooLarge(_))
    ));
    // Unknown parameters and points off the grid.
    let bogus = ["bogus".to_string()];
    assert!(matches!(
        engine.query_position("POINT(25 62)", None, Some(&bogus), None, None),
        Err(DataServerError::InvalidParameter(_))
    ));
    assert!(matches!(
        engine.get_raster_tile(
            [20.0, 60.0, 21.0, 61.0],
            4,
            4,
            None,
            &OutputCrs::Wgs84,
            Some("bogus"),
            None,
            None
        ),
        Err(DataServerError::InvalidParameter(_))
    ));
    assert!(matches!(
        engine.query_position("POINT(10 50)", None, None, None, None),
        Err(DataServerError::LocationNotFound(_))
    ));
}

#[test]
fn rejects_bad_configurations() {
    let fail = |config: StationGridConfig| {
        StationGridEngine::new("bad", "weather", fixture(), &config)
            .err()
            .expect("must fail")
            .to_string()
    };
    let mut c = config("idw");
    c.cadence = None;
    assert!(fail(c).contains("cadence"));
    let mut c = config("idw");
    c.correlation_length_km = Some(100.0);
    assert!(fail(c).contains("adw"));
    let mut c = config("nearest");
    c.power = Some(2.0);
    assert!(fail(c).contains("nearest"));
    assert!(fail(config("kriging")).contains("unknown method"));
    let mut c = config("idw");
    c.parameters = vec!["dew_point".into()];
    assert!(fail(c).contains("dew_point"));
    let mut c = config("idw");
    c.grid.crs = "EPSG:3067".into();
    assert!(fail(c).contains("CRS84"));
    let mut c = config("idw");
    c.max_neighbours = 0;
    assert!(fail(c).contains("max_neighbours"));
    let mut c = config("idw");
    c.grid.resolution = 0.0005;
    assert!(fail(c).contains("cells"));
}

/// One synthetic station: `(lon, lat, [(time, value)])`.
type Obs = (f64, f64, Vec<(DateTime<Utc>, f64)>);

/// A station source held in memory: what a BUFR or PostGIS station
/// collection answers to an area query, editable to play late reports.
struct Synthetic {
    stations: Mutex<Vec<Obs>>,
    /// Slows every area query, as a remote source would.
    delay: StdDuration,
}

impl Synthetic {
    fn new(stations: Vec<Obs>) -> Arc<Self> {
        Arc::new(Self {
            stations: Mutex::new(stations),
            delay: StdDuration::ZERO,
        })
    }
}

impl EdrEngine for Synthetic {
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
        HashMap::from([(
            "t2m".to_string(),
            ParameterDescription {
                label: "air temperature".into(),
                unit: "°C".into(),
                observed_property: "air_temperature".into(),
                standard_name: None,
            },
        )])
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let stations = self.stations.lock().unwrap();
        let times = stations.iter().flat_map(|s| s.2.iter().map(|(t, _)| *t));
        Some((times.clone().min()?, times.max()?))
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
        std::thread::sleep(self.delay);
        let polygon = parse_area_coords(coords)?;
        let (start, end) = datetime.expect("the engine always sends a window");
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
                            },
                        )]),
                    }
                })
                .collect(),
        ))
    }
}

fn synthetic_config(bbox: [f64; 4], resolution: f64, radius_km: f64) -> StationGridConfig {
    StationGridConfig {
        source: "obs".into(),
        parameters: vec!["t2m".into()],
        grid: StationGridGeometry {
            crs: "CRS:84".into(),
            bbox,
            resolution,
        },
        search_radius_km: radius_km,
        ..config("idw")
    }
}

/// The canonical seam box: a grid across the antimeridian, stations either
/// side of it, rendered and queried with `170,10,-170,20`.
#[test]
fn seam_across_the_antimeridian() {
    let t = utc(10, 6, 0);
    let source = Synthetic::new(vec![
        (179.0, 15.0, vec![(t, 10.0)]),
        (-179.0, 15.0, vec![(t, 20.0)]),
    ]);
    let engine = StationGridEngine::new(
        "seam",
        "obs",
        source,
        &synthetic_config([170.0, 10.0, -170.0, 20.0], 0.5, 400.0),
    )
    .unwrap();
    let tile = engine
        .get_raster_tile(
            [170.0, 10.0, -170.0, 20.0],
            40,
            20,
            Some(t),
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
        .unwrap();
    let px = f32_values(&tile);
    // Row of 15.25°N; the columns either side of 180°.
    let (west, east) = (px[9 * 40 + 19], px[9 * 40 + 20]);
    assert!(west > 10.0 && west < 15.0, "{west}");
    assert!(east > 15.0 && east < 20.0, "{east}");
    assert!(
        (west + east - 30.0).abs() < 1e-3,
        "mirror pair: {west} {east}"
    );

    let area = single(
        engine
            .query_area("170,10,-170,20", instant(t), None, None, None)
            .unwrap(),
    );
    let DomainDescription::Grid { x, .. } = &area.domain else {
        panic!("Grid");
    };
    assert_eq!(x.len(), 40);
    assert!(x.contains(&179.75) && x.contains(&-179.75));
    let values = &area.ranges["t2m"].values;
    let row = 9 * 40;
    let at = |lon: f64| values[row + x.iter().position(|v| *v == lon).unwrap()];
    assert_eq!(at(179.75).map(|v| v as f32), Some(west));
    assert_eq!(at(-179.75).map(|v| v as f32), Some(east));
    assert_eq!(
        position(&engine, -179.75, 15.25, t).map(|v| v as f32),
        Some(east)
    );

    // Cube across the seam keeps west-to-east order through 180°.
    let bbox = Bbox::new(179.0, 15.0, -179.0, 16.0).unwrap();
    let cube = single(
        engine
            .query_cube(
                &bbox,
                instant(t),
                None,
                None,
                CubeResolution::default(),
                None,
            )
            .unwrap(),
    );
    let DomainDescription::Grid { x, .. } = &cube.domain else {
        panic!("Grid");
    };
    assert_eq!(x, &vec![179.25, 179.75, -179.75, -179.25]);
}

/// A global grid around the North Pole, drawn in a polar view of EPSG:3035:
/// every pixel near the pole is within reach of a ring of stations at 85°N.
#[test]
fn near_the_pole() {
    let t = utc(10, 6, 0);
    let ring = (0..8)
        .map(|i| (-180.0 + 45.0 * i as f64, 85.0, vec![(t, 5.0)]))
        .collect();
    let engine = StationGridEngine::new(
        "pole",
        "obs",
        Synthetic::new(ring),
        &synthetic_config([-180.0, 60.0, 180.0, 90.0], 0.5, 1000.0),
    )
    .unwrap();
    let laea = ds_core::geo::projected_output_crs("EPSG:3035").unwrap();
    let (px, py) = laea.forward(10.0, 90.0);
    let tile = engine
        .get_raster_tile(
            [-180.0, 80.0, 180.0, 90.0],
            64,
            64,
            Some(t),
            &OutputCrs::Projected {
                crs: laea,
                bbox: [
                    px - 400_000.0,
                    py - 400_000.0,
                    px + 400_000.0,
                    py + 400_000.0,
                ],
            },
            None,
            None,
            None,
        )
        .unwrap();
    assert!(
        f32_values(&tile).iter().all(|v| (v - 5.0).abs() < 1e-5),
        "a constant field over the pole"
    );
    for lon in [0.0, 180.0, -90.0] {
        let v = position(&engine, lon, 89.9, t).unwrap();
        assert!((v - 5.0).abs() < 1e-5, "{lon}: {v}");
    }
}

/// A report arriving after the analysis time was first drawn moves the
/// content version and the drawn value, and recomputes that time's grid
/// alone: the other time's grid stays cached under its own revision.
#[test]
fn late_observations_revise_the_grid() {
    let t = utc(10, 6, 0);
    let earlier = t - Duration::hours(1);
    let source = Synthetic::new(vec![
        (24.0, 60.0, vec![(earlier, 0.0), (t, 1.0)]),
        (26.0, 60.0, vec![(earlier, 2.0), (t, 3.0)]),
    ]);
    let engine = StationGridEngine::new(
        "late",
        "obs",
        source.clone(),
        &synthetic_config([22.0, 58.0, 28.0, 62.0], 0.1, 300.0),
    )
    .unwrap();
    engine.poll_once();
    let analyses = || engine.metrics().0;
    assert_eq!(analyses(), 2, "both latest times computed ahead");
    let version = engine.content_version();
    let before = position(&engine, 24.95, 60.05, t).unwrap();
    let earlier_before = position(&engine, 24.95, 60.05, earlier).unwrap();
    assert_eq!(engine.station_counts(), vec![("t2m".to_string(), Some(2))]);

    // Nothing new: the poll leaves the version and the grids alone.
    engine.poll_once();
    assert_eq!(engine.content_version(), version);
    assert_eq!(analyses(), 2);

    // A third station reports late for the latest analysis time.
    source
        .stations
        .lock()
        .unwrap()
        .push((25.0, 60.0, vec![(t, 10.0)]));
    engine.poll_once();
    assert!(
        engine.content_version() > version,
        "a revision moves the version"
    );
    assert_eq!(analyses(), 3, "only the revised time is computed again");
    let after = position(&engine, 24.95, 60.05, t).unwrap();
    assert!(after > before + 1.0, "{before} → {after}");
    let tile = engine
        .get_raster_tile(
            [24.9, 60.0, 25.0, 60.1],
            1,
            1,
            Some(t),
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
        .unwrap();
    assert_eq!(f32_values(&tile)[0], after as f32);
    assert_eq!(engine.station_counts(), vec![("t2m".to_string(), Some(3))]);
    // The earlier time's grid is still the cached one.
    assert_eq!(
        position(&engine, 24.95, 60.05, earlier),
        Some(earlier_before)
    );
    assert_eq!(analyses(), 3);
}

/// A grid that cannot be ready by the request deadline is a 503
/// (`ResourceExhausted`), never a partial grid; its computation goes on and
/// serves the retry.
#[test]
fn a_miss_past_the_deadline_is_busy_then_served() {
    let t = utc(10, 6, 0);
    let source = Arc::new(Synthetic {
        stations: Mutex::new(vec![(24.0, 60.0, vec![(t, 1.0)])]),
        delay: StdDuration::from_millis(400),
    });
    let engine = StationGridEngine::new(
        "slow",
        "obs",
        source,
        &synthetic_config([22.0, 58.0, 28.0, 62.0], 0.1, 300.0),
    )
    .unwrap();
    let render = || {
        engine.get_raster_tile(
            [23.0, 59.0, 25.0, 61.0],
            8,
            8,
            Some(t),
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
    };
    {
        let _deadline =
            ds_core::deadline::enter(Some(Instant::now() + StdDuration::from_millis(50)));
        assert!(matches!(render(), Err(DataServerError::ResourceExhausted)));
    }
    let started = Instant::now();
    let tile = render().unwrap();
    assert!(f32_values(&tile).iter().all(|v| (v - 1.0).abs() < 1e-6));
    assert!(
        started.elapsed() < StdDuration::from_millis(400),
        "the first computation is reused"
    );
    // The axis ends at the source's latest report.
    assert_eq!(
        engine.resolve_time(Some(t + Duration::hours(5)), None),
        Some(t)
    );
}
