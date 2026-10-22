//! Parameter names (#1215). A descriptor may end in a short name in
//! parentheses, `2 Metre Temperature (2t)`. The map APIs name the parameter
//! by it, the default included, so the root layer draws the default's own
//! style and WMS GetFeatureInfo asks EDR for `2t`. EDR advertises the full
//! descriptor, takes the short name as an alias of it, and keys the
//! coverage by the name asked for.

use super::*;

const T2M: &str = "2 Metre Temperature (2t)";
const MSL: &str = "Mean Sea Level Pressure (msl)";

fn kenya(wms_parameter: Option<&str>) -> QueryDataEngine {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/ecmwf-kenya");
    QueryDataEngine::new(&dir, "qd", wms_parameter, 30, 4).unwrap()
}

fn single(response: CoverageResponse) -> QueryResult {
    match response {
        CoverageResponse::Single(result) => result,
        CoverageResponse::Collection(_) => panic!("expected one coverage"),
    }
}

fn keys(result: &QueryResult) -> Vec<&str> {
    let mut keys: Vec<&str> = result.ranges.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut described: Vec<&str> = result.parameters.keys().map(String::as_str).collect();
    described.sort_unstable();
    assert_eq!(keys, described, "every range is described under its key");
    keys
}

/// `RasterInfo.parameter` is the default's map name, a name the parameter
/// list holds, however `wms_parameter` names it; EDR's advertised names
/// stay the descriptors.
#[test]
fn the_default_parameter_is_named_as_its_layer() {
    for (wms_parameter, default, unit) in [
        (Some("2t"), "2t", "°C"),
        (Some(T2M), "2t", "°C"),
        // kFmiTemperature's number.
        (Some("4"), "2t", "°C"),
        // The first parameter.
        (None, "msl", "hPa"),
        (Some("rr1h"), "rr1h", "mm"),
    ] {
        let engine = kenya(wms_parameter);
        let info = engine.raster_info();
        assert_eq!(info.parameter, default, "{wms_parameter:?}");
        assert!(info.parameters.iter().any(|p| p.name == default));
        assert_eq!(info.parameter_unit(None), Some(unit), "{wms_parameter:?}");
        assert_eq!(engine.get_parameters(), [MSL, T2M, "rr1h"]);
        let mut described: Vec<String> = engine.get_parameter_descriptions().into_keys().collect();
        described.sort_unstable();
        assert_eq!(described, [T2M, MSL, "rr1h"]);
    }
}

/// Every query type that takes `parameter-name` (position, area, radius
/// and trajectory) takes the short name as an alias of its descriptor, in
/// any case, and keys the coverage by the name asked for, in its canonical
/// spelling: the same values as the descriptor's, described as it is.
#[test]
fn every_query_takes_a_short_name_keyed_as_asked() {
    let engine = kenya(Some("2t"));
    let (first, _) = engine.get_temporal_extent().unwrap();
    let step = Some((first, first));
    type Query<'a> = Box<dyn Fn(&[String]) -> Result<CoverageResponse, DataServerError> + 'a>;
    let queries: [(&str, Query); 4] = [
        (
            "position",
            Box::new(|p| engine.query_position("POINT(36.8 -1.3)", step, Some(p), None, None)),
        ),
        (
            "area",
            Box::new(|p| {
                engine.query_area(
                    "POLYGON((36 -2, 38 -2, 38 0, 36 0, 36 -2))",
                    step,
                    Some(p),
                    None,
                    None,
                )
            }),
        ),
        (
            "radius",
            Box::new(|p| {
                engine.query_radius("POINT(36.8 -1.3)", 80_000.0, step, Some(p), None, None)
            }),
        ),
        (
            "trajectory",
            Box::new(|p| {
                engine.query_trajectory("LINESTRING(35 -2, 37 0, 39 1)", step, Some(p), None, None)
            }),
        ),
    ];
    let names = |names: &[&str]| -> Vec<String> { names.iter().map(|n| n.to_string()).collect() };
    for (query, run) in &queries {
        let by_descriptor = single(run(&names(&[T2M])).unwrap());
        assert_eq!(keys(&by_descriptor), [T2M], "{query}");
        let want = &by_descriptor.ranges[T2M];
        assert!(want.values.iter().any(Option::is_some), "{query}");
        for alias in ["2t", "2T"] {
            let by_alias = single(run(&names(&[alias])).unwrap());
            assert_eq!(keys(&by_alias), ["2t"], "{query} {alias}");
            let got = &by_alias.ranges["2t"];
            assert_eq!(
                (&got.shape, &got.axis_names, &got.values),
                (&want.shape, &want.axis_names, &want.values),
                "{query} {alias}"
            );
            assert_eq!(by_alias.parameters["2t"].label, T2M, "{query}");
            assert_eq!(by_alias.parameters["2t"].unit, "°C", "{query}");
            assert_eq!(
                format!("{:?}", by_alias.domain),
                format!("{:?}", by_descriptor.domain),
                "{query} {alias}"
            );
        }

        // Each name asked for is a range; a repeat of one collapses. A
        // name without a short name is its own alias.
        let all = single(run(&names(&["2t", T2M, "2T", "msl", "RR1H"])).unwrap());
        assert_eq!(keys(&all), [T2M, "2t", "msl", "rr1h"], "{query}");
        assert_eq!(all.ranges["2t"].values, all.ranges[T2M].values, "{query}");

        // Neither a descriptor nor a short name: the shared 400, listing
        // the advertised descriptors. So is an empty list.
        let err = run(&names(&["2t", "t2m"])).unwrap_err();
        assert!(
            matches!(&err, DataServerError::InvalidParameter(m)
                if m.contains("'t2m'") && m.contains(&format!("{MSL}, {T2M}, rr1h"))),
            "{query}: {err}"
        );
        assert!(
            matches!(run(&[]), Err(DataServerError::InvalidParameter(_))),
            "{query}"
        );
    }
}

/// A descriptor matches before a short name, as the map side's
/// `param_index_by_name` matches: a parameter whose descriptor is another's
/// short name is the one that name selects.
#[test]
fn a_descriptor_wins_over_another_parameters_short_name() {
    let fixture = super::wind_tests::Fixture::new(
        super::wind_tests::KENYA,
        // `rr1h` becomes `msl`: the MSLP's short name.
        &[(b"\n353\n4 rr1h\n", b"\n353\n3 msl\n")],
    );
    let engine = QueryDataEngine::new(&fixture.0, "qd", None, 30, 4).unwrap();
    let all = single(
        engine
            .query_position("POINT(36.8 -1.3)", None, None, None, None)
            .unwrap(),
    );
    let one = single(
        engine
            .query_position(
                "POINT(36.8 -1.3)",
                None,
                Some(&["msl".to_string()]),
                None,
                None,
            )
            .unwrap(),
    );
    assert_eq!(keys(&one), ["msl"]);
    assert_eq!(one.ranges["msl"].values, all.ranges["msl"].values);
    assert_eq!(one.parameters["msl"].unit, "mm");
}

/// A descriptor in FMI's ISO-8859-1, `Lämpötila` for the 2 m temperature
/// (#1251), names its parameter like an ASCII one: EDR advertises, selects
/// and keys it, the map lists, defaults to and renders it, and
/// `wms_parameter` names it. The parameter is still its number, with its
/// unit and its values.
#[test]
fn a_latin1_descriptor_names_its_parameter() {
    const LAMPOTILA: &str = "Lämpötila";
    let fixture = super::wind_tests::Fixture::new(
        super::wind_tests::KENYA,
        &[(
            b"\n4\n24 2 Metre Temperature (2t)\n",
            b"\n4\n9 L\xe4mp\xf6tila\n",
        )],
    );
    let point = |engine: &QueryDataEngine, name: &str| {
        single(
            engine
                .query_position(
                    "POINT(36.8 -1.3)",
                    None,
                    Some(&[name.to_string()]),
                    None,
                    None,
                )
                .unwrap(),
        )
    };
    let tile = |engine: &QueryDataEngine, name: &str| -> Vec<Option<f64>> {
        engine
            .get_raster_tile(
                [36.0, -2.0, 38.0, 0.0],
                8,
                8,
                None,
                &OutputCrs::Wgs84,
                Some(name),
                None,
                None,
            )
            .unwrap()
            .values
            .iter_values()
            .collect()
    };
    let original = kenya(Some("2t"));
    let want = point(&original, T2M).ranges[T2M].values.clone();
    let want_tile = tile(&original, "2t");
    assert!(want_tile.iter().all(Option::is_some), "inside the grid");
    for wms_parameter in [LAMPOTILA, "lämpötila", "4"] {
        let engine = QueryDataEngine::new(&fixture.0, "qd", Some(wms_parameter), 30, 4).unwrap();
        assert_eq!(engine.get_parameters(), [MSL, LAMPOTILA, "rr1h"]);
        let info = engine.raster_info();
        assert_eq!(info.parameter, LAMPOTILA, "{wms_parameter}");
        assert_eq!(info.parameter_unit(None), Some("°C"), "{wms_parameter}");
        let names: Vec<&str> = info.parameters.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["msl", LAMPOTILA, "rr1h"]);
        assert_eq!(tile(&engine, LAMPOTILA), want_tile);

        for name in [LAMPOTILA, "lämpötila"] {
            let result = point(&engine, name);
            assert_eq!(keys(&result), [LAMPOTILA], "{name}");
            assert_eq!(result.ranges[LAMPOTILA].values, want, "{name}");
            assert_eq!(result.parameters[LAMPOTILA].label, LAMPOTILA);
            assert_eq!(result.parameters[LAMPOTILA].unit, "°C");
        }
        // An ASCII name beside it is unchanged.
        assert_eq!(keys(&point(&engine, "MSL")), ["msl"]);
    }
}
