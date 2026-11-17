//! EDR `parameter-name` on GRIB (#1493): every query type and layout
//! matches names by the shared rule (`select_parameters`), case-insensitively
//! and answered under the catalog's spelling, and an unknown name is the
//! shared 400 listing the advertised parameters. The legacy layout's
//! position and trajectory used to answer either with a range of nulls
//! under the name as sent.

use super::*;
use crate::test_support::{message, parameter_message, TestSource};
use ds_core::cube::CubeResolution;
use ds_core::feature::Bbox;

/// One run, steps 0 and 6: `TMP` at 2 m (near-surface) and `RH` at
/// 500 hPa (upper-air, outside the legacy layout's defaults, as the
/// committed ECMWF fixture's `q` at 150 hPa).
fn source() -> TestSource {
    let source = TestSource::new();
    for step in [0, 6] {
        let offset = step as f32;
        source.write(
            &format!("f{step}"),
            &[
                (
                    "TMP",
                    "2 m above ground",
                    message(0, 280.0 + offset, [0, 2, 4, 6], 103, 2),
                ),
                (
                    "RH",
                    "500 mb",
                    parameter_message(1, 1, 0x30, 50.0 + offset, [0, 2, 4, 6], 100, 50_000),
                ),
            ],
            step,
        );
    }
    source
}

const POINT: &str = "POINT(0.5 0.5)";
const PATH: &str = "LINESTRING(0 0.5, 1 0.5)";
const AREA: &str = "POLYGON((0 0,1 0,1 1,0 1,0 0))";

type Ranges = BTreeMap<String, Vec<Option<f64>>>;

/// The ranges `kind` answers for `names`.
fn query(engine: &GribEngine, kind: &str, names: &[&str]) -> Result<Ranges, DataServerError> {
    let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
    let names = Some(names.as_slice());
    let response = match kind {
        "position" => engine.query_position(POINT, None, names, None, None),
        "trajectory" => engine.query_trajectory(PATH, None, names, None, None),
        "area" => engine.query_area(AREA, None, names, None, None),
        "radius" => engine.query_radius(POINT, 100_000.0, None, names, None, None),
        "cube" => engine.query_cube(
            &Bbox::new(0.0, 0.0, 1.0, 1.0).unwrap(),
            None,
            names,
            None,
            CubeResolution::default(),
            None,
        ),
        other => panic!("no query type {other}"),
    }?;
    // A 2-D path over two steps is a coverage per step.
    let results = match response {
        CoverageResponse::Single(result) => vec![result],
        CoverageResponse::Collection(results) => results,
    };
    let mut ranges = Ranges::new();
    for result in results {
        assert_eq!(
            result.parameters.keys().collect::<HashSet<_>>(),
            result.ranges.keys().collect::<HashSet<_>>(),
            "{kind}: every range is described"
        );
        for (name, range) in result.ranges {
            ranges.entry(name).or_default().extend(range.values);
        }
    }
    Ok(ranges)
}

/// The shared rule's 400 for `name` on `engine`: the message the EDR HTML
/// error page recognises, built from `get_parameters` as that page does.
fn assert_unknown(engine: &GribEngine, kind: &str, names: &[&str], name: &str) {
    let expected = format!(
        "Unknown parameter '{name}'; available: {}",
        engine.get_parameters().join(", ")
    );
    match query(engine, kind, names) {
        Err(DataServerError::InvalidParameter(message)) => {
            assert_eq!(message, expected, "{kind} {names:?}")
        }
        other => panic!("{kind} {names:?}: expected a 400, got {other:?}"),
    }
}

/// The `nwp` reproduction of #1493: a case variant answers the advertised
/// parameter under its own name, with its values, and an unknown name is
/// a 400 listing the advertised ones, on every query type of the legacy
/// layout.
#[test]
fn the_legacy_layout_matches_names_by_the_shared_rule() {
    let source = source();
    let engine = GribEngine::new("names", &source.config()).unwrap();
    assert!(engine.family.is_none(), "the legacy layout");
    assert_eq!(engine.get_parameters(), ["TMP", "RH"]);
    for kind in ["position", "trajectory", "area", "radius"] {
        for name in ["TMP", "RH"] {
            let exact = query(&engine, kind, &[name]).unwrap();
            assert_eq!(exact.keys().collect::<Vec<_>>(), [name], "{kind}");
            assert!(exact[name].iter().any(Option::is_some), "{kind} {name}");
            for variant in [
                name.to_lowercase(),
                format!("{}{}", &name[..1], name[1..].to_lowercase()),
            ] {
                assert_eq!(
                    query(&engine, kind, &[&variant]).unwrap(),
                    exact,
                    "{kind}: {variant} answers {name}"
                );
            }
            // A repeat, in any spelling, is one range.
            assert_eq!(
                query(&engine, kind, &[name, &name.to_lowercase()]).unwrap(),
                exact,
                "{kind}"
            );
        }
        // `Ranges` is sorted by name.
        let both = query(&engine, kind, &["tmp", "rh"]).unwrap();
        assert_eq!(both.keys().collect::<Vec<_>>(), ["RH", "TMP"], "{kind}");
        assert_unknown(&engine, kind, &["NOPE"], "NOPE");
        assert_unknown(&engine, kind, &["tmp", "NOPE"], "NOPE");
    }
}

/// The level views take the same rule: a cube on the pressure view, and
/// position, area and trajectory on both views. A name another view
/// serves is unknown here, listing this view's parameters.
#[test]
fn level_views_match_names_by_the_shared_rule() {
    let source = source();
    let mut config = source.config();
    config.level_types = Some(vec![GribLevelType::Single, GribLevelType::Pressure]);
    let owner = GribEngine::new("names", &config).unwrap();
    let views = owner.level_collections();
    let view = |family: GribLevelType| {
        views
            .iter()
            .find(|view| view.family == Some(family))
            .expect("both families have a run")
    };
    let (single, pressure) = (view(GribLevelType::Single), view(GribLevelType::Pressure));
    assert_eq!(single.get_parameters(), ["TMP"]);
    assert_eq!(pressure.get_parameters(), ["RH"]);
    for (engine, name, kinds) in [
        (single, "TMP", &["position", "trajectory", "area"][..]),
        (
            pressure,
            "RH",
            &["position", "trajectory", "area", "cube"][..],
        ),
    ] {
        for &kind in kinds {
            let exact = query(engine, kind, &[name]).unwrap();
            assert_eq!(exact.keys().collect::<Vec<_>>(), [name], "{kind}");
            assert!(exact[name].iter().any(Option::is_some), "{kind} {name}");
            assert_eq!(
                query(engine, kind, &[&name.to_lowercase()]).unwrap(),
                exact,
                "{kind}"
            );
            let other = if name == "TMP" { "rh" } else { "tmp" };
            assert_unknown(engine, kind, &[other], other);
        }
    }
}
