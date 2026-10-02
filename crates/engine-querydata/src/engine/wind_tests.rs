//! QueryData u/v components (#897). The format states no u/v frame; FMI
//! newbase's convention makes them relative to the data's own grid, so a
//! pair gives speed and direction on a lat/lon area and speed only on a
//! projected one. No committed fixture carries wind: these tests rename two
//! descriptors of the Kenya (lat/lon) and MEPS (LCC) fixtures to FMI's u
//! and v components (newbase `kFmiWindUMS = 23`, `kFmiWindVMS = 24`). The
//! values are no wind, but the derivation only needs two fields.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use super::*;
use ds_core::map_engine::OutputCrs;
use ds_core::wind::{DerivedWind, OutcomeLog, PairOutcome};

const KENYA: (&str, &str) = (
    "ecmwf-kenya",
    "202604042019_202604040600_ecmwf_kenya_surface.sqd",
);
const MSL: &[u8] = b"\n1\n29 Mean Sea Level Pressure (msl)\n";
const T2M: &[u8] = b"\n4\n24 2 Metre Temperature (2t)\n";

const MEPS: (&str, &str) = ("meps", "20260405T180000Z_meps_northeurope_surface.sqd");
const MEPS_T: &[u8] = b"\n4\n11 Temperature\n";
const MEPS_MSL: &[u8] = b"\n1\n23 Mean Sea Level Pressure\n";

const FMI_U: &[u8] = b"\n23\n7 WindUMS\n";
const FMI_V: &[u8] = b"\n24\n7 WindVMS\n";

/// A copy of a fixture with descriptors replaced, in a fresh directory
/// (removed on drop).
struct Fixture(PathBuf);

impl Fixture {
    fn new((dir, file): (&str, &str), renames: &[(&[u8], &[u8])]) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata")
            .join(dir)
            .join(file);
        let mut bytes = std::fs::read(source).unwrap();
        for (from, to) in renames {
            let at = bytes
                .windows(from.len())
                .position(|w| w == *from)
                .expect("descriptor in the fixture header");
            bytes.splice(at..at + from.len(), to.iter().copied());
        }
        let dir = std::env::temp_dir().join(format!(
            "meteocore-qd-wind-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(file), bytes).unwrap();
        Self(dir)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn wrap(fixture: &Fixture) -> (Arc<QueryDataEngine>, DerivedWind, Vec<String>) {
    let engine = Arc::new(QueryDataEngine::new(&fixture.0, "qd", None, 30, 4).unwrap());
    let logged = Arc::new(Mutex::new(Vec::new()));
    let sink = logged.clone();
    let log: OutcomeLog = Arc::new(move |id: &str, outcome: &PairOutcome| {
        sink.lock().unwrap().push(format!("{id}: {outcome}"));
    });
    let wind = DerivedWind::new("qd", engine.clone(), log);
    let logged = logged.lock().unwrap().clone();
    (engine, wind, logged)
}

fn series(engine: &dyn EdrEngine, point: &str, names: &[&str]) -> QueryResult {
    let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
    match engine
        .query_position(point, None, Some(&names), None, None)
        .unwrap()
    {
        CoverageResponse::Single(result) => result,
        CoverageResponse::Collection(_) => panic!("expected one series"),
    }
}

fn tile(engine: &dyn MapEngine, bbox: [f64; 4], parameter: &str) -> Vec<Option<f64>> {
    engine
        .get_raster_tile(
            bbox,
            8,
            8,
            None,
            &OutputCrs::WebMercator,
            Some(parameter),
            None,
            None,
        )
        .unwrap()
        .values
        .iter_values()
        .collect()
}

fn map_names(wind: &DerivedWind) -> Vec<String> {
    wind.raster_info_shared()
        .parameters
        .iter()
        .map(|p| p.name.clone())
        .collect()
}

/// The speed tile is the hypotenuse of the component tiles.
fn assert_speed_tile(engine: &QueryDataEngine, wind: &DerivedWind, bbox: [f64; 4], speed: &str) {
    let (u, v) = (tile(engine, bbox, "WindUMS"), tile(engine, bbox, "WindVMS"));
    let want: Vec<Option<f64>> = u
        .iter()
        .zip(&v)
        .map(|(u, v)| match (u, v) {
            (Some(u), Some(v)) => Some(f64::from(u.hypot(*v) as f32)),
            _ => None,
        })
        .collect();
    assert!(
        want.iter().all(Option::is_some),
        "the view is inside the grid"
    );
    assert_eq!(tile(wind, bbox, speed), want);
}

/// A lat/lon area: grid-relative is earth-relative, so FMI's
/// `WindUMS`/`WindVMS` give `WindSpeedMS` and `WindDirection`.
#[test]
fn fmi_components_on_lat_lon_give_speed_and_direction() {
    let fixture = Fixture::new(KENYA, &[(MSL, FMI_U), (T2M, FMI_V)]);
    let (engine, wind, logged) = wrap(&fixture);
    assert_eq!(
        logged,
        ["qd: WindUMS/WindVMS: derived speed 'WindSpeedMS' and direction 'WindDirection'"]
    );
    let facts = engine.wind_facts();
    assert_eq!(facts.grid, ds_core::wind::GridAxes::NorthAligned);
    assert_eq!(facts.parameters[0].fmi_param, Some(23));
    assert_eq!(facts.parameters[0].frame, VectorFrame::Grid);
    assert!(Arc::ptr_eq(&facts, &engine.wind_facts()));

    assert_eq!(
        wind.get_parameters(),
        ["WindUMS", "WindVMS", "rr1h", "WindSpeedMS", "WindDirection"]
    );
    // Direction is EDR only.
    assert_eq!(
        map_names(&wind),
        ["WindUMS", "WindVMS", "rr1h", "WindSpeedMS"]
    );

    let point = "POINT(36.8 -1.3)";
    let components = series(engine.as_ref(), point, &["WindUMS", "WindVMS"]);
    let derived = series(&wind, point, &["WindSpeedMS", "WindDirection"]);
    let (u, v) = (
        &components.ranges["WindUMS"].values,
        &components.ranges["WindVMS"].values,
    );
    for (i, (u, v)) in u.iter().zip(v).enumerate() {
        let (u, v) = (u.unwrap(), v.unwrap());
        assert_eq!(derived.ranges["WindSpeedMS"].values[i], Some(u.hypot(v)));
        let from = derived.ranges["WindDirection"].values[i].unwrap();
        assert!((from - (-u).atan2(-v).to_degrees().rem_euclid(360.0)).abs() < 1e-9);
    }
    assert_eq!(
        derived.parameters["WindSpeedMS"].unit, "",
        "QueryData has no units"
    );
    assert_eq!(derived.parameters["WindDirection"].unit, "°");
    assert_speed_tile(&engine, &wind, [35.0, -4.0, 40.0, 3.0], "WindSpeedMS");
}

/// A Lambert conformal conic area: the components are along the grid's
/// axes, which turn away from north across the grid, so speed only.
#[test]
fn fmi_components_on_lcc_give_speed_only() {
    let fixture = Fixture::new(MEPS, &[(MEPS_T, FMI_U), (MEPS_MSL, FMI_V)]);
    let (engine, wind, logged) = wrap(&fixture);
    assert_eq!(
        logged,
        [
            "qd: WindUMS/WindVMS: derived speed 'WindSpeedMS' only; no direction: u/v are \
             grid-relative on a rotated grid and turning them to true north is not implemented"
        ]
    );
    assert_eq!(engine.wind_facts().grid, ds_core::wind::GridAxes::Rotated);
    assert_eq!(wind.get_parameters(), ["WindUMS", "WindVMS", "WindSpeedMS"]);
    assert_eq!(map_names(&wind), ["WindUMS", "WindVMS", "WindSpeedMS"]);
    let point = "POINT(15 62.5)";
    let components = series(engine.as_ref(), point, &["WindUMS", "WindVMS"]);
    let derived = series(&wind, point, &["WindSpeedMS"]);
    let u = components.ranges["WindUMS"].values[0].unwrap();
    let v = components.ranges["WindVMS"].values[0].unwrap();
    assert_eq!(derived.ranges["WindSpeedMS"].values[0], Some(u.hypot(v)));
    assert_speed_tile(&engine, &wind, [12.0, 61.0, 18.0, 64.0], "WindSpeedMS");
}

/// Producer names with ECMWF short names in parentheses: the map names are
/// the short names, EDR's the full descriptors, and the derived names are
/// the ECMWF vocabulary's on both.
#[test]
fn ecmwf_short_names_keep_their_full_edr_names() {
    let fixture = Fixture::new(
        KENYA,
        &[
            (MSL, b"\n23\n31 10 metre U wind component (10u)\n"),
            (T2M, b"\n24\n31 10 metre V wind component (10v)\n"),
        ],
    );
    let (engine, wind, logged) = wrap(&fixture);
    assert_eq!(
        logged,
        ["qd: 10u/10v: derived speed '10si' and direction '10wdir'"]
    );
    let names = wind.get_parameters();
    assert!(names.contains(&"10si".to_string()) && names.contains(&"10wdir".to_string()));
    assert!(map_names(&wind).contains(&"10si".to_string()));
    let components = series(
        engine.as_ref(),
        "POINT(36.8 -1.3)",
        &[
            "10 metre U wind component (10u)",
            "10 metre V wind component (10v)",
        ],
    );
    let derived = series(&wind, "POINT(36.8 -1.3)", &["10si"]);
    assert_eq!(derived.ranges.len(), 1);
    let u = components.ranges["10 metre U wind component (10u)"].values[0].unwrap();
    let v = components.ranges["10 metre V wind component (10v)"].values[0].unwrap();
    assert_eq!(derived.ranges["10si"].values[0], Some(u.hypot(v)));
}
