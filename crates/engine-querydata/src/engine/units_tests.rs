//! Units from FMI parameter numbers (#1211). A `.sqd` file carries no unit
//! string; each parameter's number fixes it (`crate::units`), and the
//! engine reports it wherever a unit is reported: EDR parameter metadata,
//! every query's coverage, and the map parameter list whose units the
//! style defaults and legends read.

use super::wind_tests::{Fixture, KENYA};
use super::*;

fn fixture_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(name)
}

fn map_units(engine: &QueryDataEngine) -> Vec<(String, String)> {
    engine
        .raster_info()
        .parameters
        .into_iter()
        .map(|p| (p.name, p.unit))
        .collect()
}

fn units(result: &QueryResult) -> BTreeMap<&str, &str> {
    result
        .parameters
        .iter()
        .map(|(name, p)| (name.as_str(), p.unit.as_str()))
        .collect()
}

fn single(response: CoverageResponse) -> QueryResult {
    match response {
        CoverageResponse::Single(result) => result,
        CoverageResponse::Collection(_) => panic!("expected one coverage"),
    }
}

/// The Kenya fixture's MSLP (1), 2 m temperature (4) and 1 h precipitation
/// (353): hPa, °C and mm, in the EDR metadata, the position, area and
/// trajectory coverages, and the map parameters.
#[test]
fn kenya_parameters_report_their_newbase_units() {
    let engine =
        QueryDataEngine::new(&fixture_dir("ecmwf-kenya"), "qd", Some("2t"), 30, 4).unwrap();
    let want = BTreeMap::from([
        ("2 Metre Temperature (2t)", "°C"),
        ("Mean Sea Level Pressure (msl)", "hPa"),
        ("rr1h", "mm"),
    ]);
    let descriptions = engine.get_parameter_descriptions();
    let described: BTreeMap<&str, &str> = descriptions
        .iter()
        .map(|(name, p)| (name.as_str(), p.unit.as_str()))
        .collect();
    assert_eq!(described, want);

    let (first, _) = engine.get_temporal_extent().unwrap();
    let step = Some((first, first));
    let position = single(
        engine
            .query_position("POINT(36.8 -1.3)", step, None, None, None)
            .unwrap(),
    );
    assert_eq!(units(&position), want);
    let area = single(
        engine
            .query_area(
                "POLYGON((36 -2, 38 -2, 38 0, 36 0, 36 -2))",
                step,
                None,
                None,
                None,
            )
            .unwrap(),
    );
    assert_eq!(units(&area), want);
    let trajectory = single(
        engine
            .query_trajectory("LINESTRING(35 -2, 37 0, 39 1)", step, None, None, None)
            .unwrap(),
    );
    assert_eq!(units(&trajectory), want);

    assert_eq!(
        map_units(&engine),
        [
            ("msl".to_string(), "hPa".to_string()),
            ("2t".to_string(), "°C".to_string()),
            ("rr1h".to_string(), "mm".to_string()),
        ]
    );
    // The default parameter's, which a legend naming no parameter reads.
    let info = engine.raster_info();
    assert_eq!(info.unit, "°C");
    assert_eq!(info.parameter_unit(None), Some("°C"));
}

/// The MEPS fixture's temperature (4) and MSLP (1), on its LCC grid.
#[test]
fn meps_parameters_report_their_newbase_units() {
    let engine = QueryDataEngine::new(&fixture_dir("meps"), "qd", None, 30, 4).unwrap();
    assert_eq!(
        map_units(&engine),
        [
            ("Temperature".to_string(), "°C".to_string()),
            ("Mean Sea Level Pressure".to_string(), "hPa".to_string()),
        ]
    );
    assert_eq!(engine.raster_info().unit, "°C");
    assert_eq!(
        engine.get_parameter_descriptions()["Mean Sea Level Pressure"].unit,
        "hPa"
    );
}

/// A number without a sourced unit reports none, never one guessed from
/// its name: `GeopHeight` (2), whose stored unit newbase does not fix, and
/// a number newbase does not know.
#[test]
fn an_unsourced_number_reports_no_unit() {
    let fixture = Fixture::new(
        KENYA,
        &[
            (
                b"\n1\n29 Mean Sea Level Pressure (msl)\n",
                b"\n2\n7 GeopHgt\n",
            ),
            (b"\n353\n4 rr1h\n", b"\n65000\n4 rr1h\n"),
        ],
    );
    let engine = QueryDataEngine::new(&fixture.0, "qd", None, 30, 4).unwrap();
    let descriptions = engine.get_parameter_descriptions();
    assert_eq!(descriptions["GeopHgt"].unit, "");
    assert_eq!(descriptions["rr1h"].unit, "");
    assert_eq!(descriptions["2 Metre Temperature (2t)"].unit, "°C");
    assert_eq!(
        map_units(&engine),
        [
            ("GeopHgt".to_string(), String::new()),
            ("2t".to_string(), "°C".to_string()),
            ("rr1h".to_string(), String::new()),
        ]
    );
    // The default parameter, the first, has none either.
    assert_eq!(engine.raster_info().unit, "");
    let position = single(
        engine
            .query_position("POINT(36.8 -1.3)", None, Some(&["rr1h".into()]), None, None)
            .unwrap(),
    );
    assert_eq!(position.parameters["rr1h"].unit, "");
}
