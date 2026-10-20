//! EDR trajectory and cube queries (#1169) over the committed fixtures:
//! the MEPS surface and pressure-level crops (testdata/meps-netcdf,
//! testdata/meps-netcdf-pl: real MET Norway values on its south-up Lambert
//! grid, two runs each) and the synthetic global grid
//! (testdata/netcdf-global). Every sampled value is checked against what a
//! position query reads at the same point, time, level and run, and the
//! field reads are counted: each field a query names is decoded once,
//! never per sample.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Timelike, Utc};
use ds_core::config::NetcdfConfig;
use ds_core::cube::CubeResolution;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::Bbox;
use ds_core::model::{CoverageResponse, DomainDescription, QueryResult};
use engine_netcdf::NetcdfEngine;

fn testdata(dir: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(dir)
}

/// `[netcdf]` over `dir` plus `extra` TOML lines.
fn config(dir: &Path, extra: &str) -> NetcdfConfig {
    toml::from_str(&format!(
        "data_path = {:?}\n{extra}",
        dir.display().to_string()
    ))
    .unwrap()
}

/// The MEPS surface crop: 10 × 11 cells 250 km apart, runs 06Z (leads
/// 0–3) and 09Z (leads 0–2).
fn meps() -> NetcdfEngine {
    let config = config(
        &testdata("meps-netcdf"),
        r#"filename_template = "meps_sfc_{lead}_%Y%m%dT{run}Z.nc"
           variables = ["air_temperature_2m", "cloud_area_fraction", "x_wind_10m",
                        "y_wind_10m", "air_pressure_at_sea_level"]"#,
    );
    NetcdfEngine::new("meps", &config).unwrap()
}

/// The MEPS pressure-level crop: 1000, 850, 500 and 300 hPa, runs 06Z
/// (leads 0–2) and 09Z (leads 0–1).
fn meps_pl() -> NetcdfEngine {
    let config = config(
        &testdata("meps-netcdf-pl"),
        r#"filename_template = "meps_pl_{lead}_%Y%m%dT{run}Z.nc"
           variables = ["air_temperature_pl", "geopotential_pl"]"#,
    );
    NetcdfEngine::new("meps-pl", &config).unwrap()
}

/// The synthetic 5° global grid: three steps, no run.
fn global() -> NetcdfEngine {
    NetcdfEngine::new("global", &config(&testdata("netcdf-global"), "")).unwrap()
}

fn at(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().into()
}

/// An `M` coordinate: seconds since the Unix epoch.
fn m(s: &str) -> i64 {
    at(s).timestamp()
}

fn names(parameters: &[&str]) -> Vec<String> {
    parameters.iter().map(|p| p.to_string()).collect()
}

fn single(response: CoverageResponse) -> QueryResult {
    match response {
        CoverageResponse::Single(result) => result,
        CoverageResponse::Collection(_) => panic!("expected a single coverage"),
    }
}

fn coverages(response: CoverageResponse) -> Vec<QueryResult> {
    match response {
        CoverageResponse::Single(result) => vec![result],
        CoverageResponse::Collection(results) => results,
    }
}

/// What a position query reads of `parameters` at each of `points`, at the
/// instant `t`, at level `z`, from run `run` (the newest covering run
/// without): one value per parameter and point, every field read once for
/// all the points.
fn positions_at(
    engine: &dyn EdrEngine,
    points: &[(f64, f64)],
    t: DateTime<Utc>,
    parameters: &[&str],
    z: Option<f64>,
    run: Option<DateTime<Utc>>,
) -> Vec<HashMap<String, Option<f64>>> {
    let coords: Vec<String> = points
        .iter()
        .map(|(lon, lat)| format!("POINT({lon} {lat})"))
        .collect();
    let z: Option<Vec<f64>> = z.map(|z| vec![z]);
    let mut read = Vec::new();
    engine
        .query_positions(
            &coords,
            Some((t, t)),
            Some(&names(parameters)),
            z.as_deref(),
            run,
            &mut |response| {
                let result = single(response);
                let DomainDescription::PointSeries { t: times, .. } = &result.domain else {
                    panic!("not a series: {:?}", result.domain)
                };
                assert_eq!(times, &[t]);
                read.push(
                    result
                        .ranges
                        .into_iter()
                        .map(|(name, range)| (name, range.values[0]))
                        .collect(),
                );
                Ok(())
            },
        )
        .unwrap();
    read
}

/// A `Trajectory` domain's `(t, lon, lat)` nodes.
type Nodes = Vec<(DateTime<Utc>, f64, f64)>;

/// A `Trajectory` coverage's nodes and each node's level.
fn nodes(result: &QueryResult) -> (Nodes, Vec<Option<f64>>) {
    let DomainDescription::Trajectory { nodes, node_z, z } = &result.domain else {
        panic!("not a Trajectory: {:?}", result.domain)
    };
    let levels = (0..nodes.len())
        .map(|i| {
            node_z
                .as_ref()
                .map(|z| z.values[i])
                .or_else(|| z.as_ref().map(|z| z.values[0]))
        })
        .collect();
    (nodes.clone(), levels)
}

/// Every value of a `Trajectory` coverage is what a position query reads
/// at its node's point, time and level, in `run`, exactly: both project
/// the point once and sample it as the map draws it. Returns the values
/// checked that are not null.
fn assert_reads_like_position(
    engine: &dyn EdrEngine,
    result: &QueryResult,
    parameters: &[&str],
    run: Option<DateTime<Utc>>,
) -> usize {
    let (nodes, levels) = nodes(result);
    let mut groups: BTreeMap<(DateTime<Utc>, Option<u64>), Vec<usize>> = BTreeMap::new();
    for (i, node) in nodes.iter().enumerate() {
        groups
            .entry((node.0, levels[i].map(f64::to_bits)))
            .or_default()
            .push(i);
    }
    let mut checked = 0;
    for ((t, level), indices) in groups {
        let points: Vec<(f64, f64)> = indices.iter().map(|&i| (nodes[i].1, nodes[i].2)).collect();
        let read = positions_at(
            engine,
            &points,
            t,
            parameters,
            level.map(f64::from_bits),
            run,
        );
        for (k, &i) in indices.iter().enumerate() {
            for &parameter in parameters {
                let got = result.ranges[parameter].values[i];
                assert_eq!(
                    got, read[k][parameter],
                    "{parameter} at node {i} {:?}, level {:?}",
                    nodes[i], levels[i]
                );
                checked += usize::from(got.is_some());
            }
        }
    }
    checked
}

/// A cube's `[t, z, y, x]` (or `[t, y, x]`) values are what a position
/// query reads at each cell centre, step and level, exactly.
fn assert_cube_reads_like_position(
    engine: &dyn EdrEngine,
    result: &QueryResult,
    parameters: &[&str],
    run: Option<DateTime<Utc>>,
) -> usize {
    let DomainDescription::Grid { x, y, t, z } = &result.domain else {
        panic!("not a Grid: {:?}", result.domain)
    };
    let times = t.as_ref().expect("a cube always has a t axis");
    let levels: Vec<Option<f64>> = match z {
        Some(z) => z.values.iter().copied().map(Some).collect(),
        None => vec![None],
    };
    let points: Vec<(f64, f64)> = y
        .iter()
        .flat_map(|&lat| x.iter().map(move |&lon| (lon, lat)))
        .collect();
    let mut checked = 0;
    for (ti, &time) in times.iter().enumerate() {
        for (zi, &level) in levels.iter().enumerate() {
            let read = positions_at(engine, &points, time, parameters, level, run);
            for (cell, read) in read.iter().enumerate() {
                for &parameter in parameters {
                    let offset = (ti * levels.len() + zi) * points.len() + cell;
                    let got = result.ranges[parameter].values[offset];
                    assert_eq!(
                        got, read[parameter],
                        "{parameter} at {time} level {level:?} cell {:?}",
                        points[cell]
                    );
                    checked += usize::from(got.is_some());
                }
            }
        }
    }
    checked
}

// ---------------------------------------------------------------------------
// Trajectory
// ---------------------------------------------------------------------------

/// A 2-D path takes the position query's steps, one coverage each (every
/// step of the newest run without `datetime`), is densified to about one
/// sample per source cell crossed, and reads each field once for all its
/// samples: 3 steps × 2 variables are 6 decodes, whatever the samples.
/// Each value is the position query's at the node.
#[test]
fn a_2d_path_reads_each_field_once_and_samples_like_position() {
    let engine = meps();
    let parameters = ["air_temperature_2m", "cloud_area_fraction"];
    let path = "LINESTRING(5 58, 30 66, 20 70)";
    let response = engine
        .query_trajectory(path, None, Some(&names(&parameters)), None, None)
        .unwrap();
    assert_eq!(
        engine.field_decodes(),
        6,
        "one read per field, not per sample"
    );
    let coverages = coverages(response);
    let hours: Vec<u32> = coverages.iter().map(|c| nodes(c).0[0].0.hour()).collect();
    assert_eq!(hours, [9, 10, 11], "the newest run's steps");
    for coverage in &coverages {
        let (nodes, levels) = nodes(coverage);
        assert!(nodes.len() > 3, "densified: {nodes:?}");
        assert!(nodes.iter().all(|n| n.0 == nodes[0].0), "one step each");
        assert!(levels.iter().all(Option::is_none), "no vertical axis");
        assert_eq!((nodes[0].1, nodes[0].2), (5.0, 58.0));
        assert_eq!(
            (nodes[nodes.len() - 1].1, nodes[nodes.len() - 1].2),
            (20.0, 70.0)
        );
        assert_eq!(coverage.parameters["air_temperature_2m"].unit, "°C");
        assert_eq!(coverage.parameters["cloud_area_fraction"].unit, "%");
        let checked = assert_reads_like_position(&engine, coverage, &parameters, None);
        assert_eq!(checked, 2 * nodes.len(), "every node is on the grid");
    }

    // `datetime` selects steps like position; a window without one is the
    // 404 a datetime list skips.
    let ten = at("2026-10-15T10:00:00Z");
    let one = coverages_of(engine.query_trajectory(path, Some((ten, ten)), None, None, None));
    assert_eq!(one.len(), 1);
    assert!(nodes(&one[0]).0.iter().all(|n| n.0 == ten));
    assert_eq!(
        one[0].ranges.len(),
        5,
        "every parameter without parameter-name"
    );
    let empty = at("2026-10-15T08:30:00Z");
    assert!(matches!(
        engine.query_trajectory(path, Some((empty, empty)), None, None, None),
        Err(DataServerError::LocationNotFound(_))
    ));
    // A pinned run is read, and only it.
    let six = at("2026-10-15T06:00:00Z");
    let pinned = coverages_of(engine.query_trajectory(
        path,
        None,
        Some(&names(&["air_temperature_2m"])),
        None,
        Some(six),
    ));
    let hours: Vec<u32> = pinned.iter().map(|c| nodes(c).0[0].0.hour()).collect();
    assert_eq!(hours, [6, 7, 8, 9]);
    for coverage in &pinned {
        assert_reads_like_position(&engine, coverage, &["air_temperature_2m"], Some(six));
    }
    // A path off the extent is a 404 and reads nothing.
    let decodes = engine.field_decodes();
    assert!(matches!(
        engine.query_trajectory("LINESTRING(-100 10, -90 20)", None, None, None, None),
        Err(DataServerError::LocationNotFound(_))
    ));
    assert_eq!(engine.field_decodes(), decodes);
}

fn coverages_of(response: Result<CoverageResponse, DataServerError>) -> Vec<QueryResult> {
    coverages(response.unwrap())
}

/// An M path reads the run a map of its earliest vertex time draws from,
/// and each sample snaps to that run's nearest step: one coverage, each
/// distinct step read once. A vertex time outside the run, or no run
/// covering the earliest time, is a 400.
#[test]
fn an_m_path_snaps_each_sample_to_a_step_of_the_run_its_first_time_selects() {
    let engine = meps();
    let parameters = ["air_temperature_2m"];
    let path = |from: &str, to: &str| {
        format!(
            "LINESTRING M(5 58 {}, 30 66 {}, 20 70 {})",
            m(from),
            m(&format!("2026-10-15T{to}")),
            m(&format!("2026-10-15T{to}"))
        )
    };

    let newest = single(
        engine
            .query_trajectory(
                &path("2026-10-15T09:00:00Z", "11:00:00Z"),
                None,
                Some(&names(&parameters)),
                None,
                None,
            )
            .unwrap(),
    );
    let (nodes_09z, _) = nodes(&newest);
    let hours: Vec<u32> = nodes_09z.iter().map(|n| n.0.hour()).collect();
    assert_eq!(hours.first(), Some(&9));
    assert_eq!(hours.last(), Some(&11));
    assert!(hours.windows(2).all(|w| w[0] <= w[1]), "{hours:?}");
    assert_eq!(engine.field_decodes(), 3, "each step once: {hours:?}");
    assert_eq!(
        assert_reads_like_position(&engine, &newest, &parameters, None),
        nodes_09z.len()
    );

    // 07Z is only in the 06Z run: its steps, 09Z included, not 09Z's.
    let six = at("2026-10-15T06:00:00Z");
    let older = single(
        engine
            .query_trajectory(
                &path("2026-10-15T07:00:00Z", "09:00:00Z"),
                None,
                Some(&names(&parameters)),
                None,
                None,
            )
            .unwrap(),
    );
    let (nodes_06z, _) = nodes(&older);
    let hours: Vec<u32> = nodes_06z.iter().map(|n| n.0.hour()).collect();
    assert_eq!((hours[0], hours[hours.len() - 1]), (7, 9), "{hours:?}");
    assert_reads_like_position(&engine, &older, &parameters, Some(six));
    let last = nodes_06z.len() - 1;
    let (lon, lat) = (nodes_06z[last].1, nodes_06z[last].2);
    let nine = at("2026-10-15T09:00:00Z");
    let from_09z = positions_at(&engine, &[(lon, lat)], nine, &parameters, None, None);
    assert_ne!(
        older.ranges["air_temperature_2m"].values[last], from_09z[0]["air_temperature_2m"],
        "the 06Z run's 09Z step, not the 09Z run's analysis"
    );

    for (what, coords, run) in [
        (
            "past the 06Z run",
            path("2026-10-15T07:00:00Z", "10:00:00Z"),
            None,
        ),
        (
            "before every run",
            path("2026-10-15T05:00:00Z", "07:00:00Z"),
            None,
        ),
        (
            "outside the pinned run",
            path("2026-10-15T07:00:00Z", "09:00:00Z"),
            Some(nine),
        ),
    ] {
        assert!(
            matches!(
                engine.query_trajectory(&coords, None, None, None, run),
                Err(DataServerError::InvalidParameter(_))
            ),
            "{what}"
        );
    }
}

/// On pressure levels, a Z path's samples snap to the nearest level, each
/// (step, level) read once; a 2-D path reads `z`'s levels (every level
/// without), one coverage per step and level; a ZM path picks both per
/// sample. Values are the position query's at the node's level.
#[test]
fn a_z_path_follows_the_pressure_levels() {
    let engine = meps_pl();
    let parameters = ["air_temperature_pl", "geopotential_pl"];
    let nine = at("2026-10-15T09:00:00Z");
    let ten = at("2026-10-15T10:00:00Z");

    let z_path = single(
        engine
            .query_trajectory(
                "LINESTRING Z(5 58 1000, 30 66 300)",
                Some((nine, nine)),
                Some(&names(&parameters)),
                None,
                None,
            )
            .unwrap(),
    );
    let (nodes_z, levels) = nodes(&z_path);
    let DomainDescription::Trajectory { node_z, z, .. } = &z_path.domain else {
        unreachable!()
    };
    assert!(node_z.is_some() && z.is_none(), "levels ride in the tuples");
    let levels: Vec<f64> = levels.into_iter().map(Option::unwrap).collect();
    assert_eq!((levels[0], levels[levels.len() - 1]), (1000.0, 300.0));
    assert!(
        levels
            .iter()
            .all(|l| [1000.0, 850.0, 500.0, 300.0].contains(l)),
        "snapped to the axis: {levels:?}"
    );
    let mut distinct = levels.clone();
    distinct.dedup();
    assert_eq!(
        engine.field_decodes(),
        2 * distinct.len() as u64,
        "each level once per variable: {levels:?}"
    );
    assert_eq!(
        assert_reads_like_position(&engine, &z_path, &parameters, None),
        2 * nodes_z.len()
    );

    // A 2-D path: `z`'s levels in request order, one coverage per step and
    // level, each with a one-value z axis.
    let per_level = coverages_of(engine.query_trajectory(
        "LINESTRING(5 58, 30 66)",
        Some((nine, ten)),
        Some(&names(&["air_temperature_pl"])),
        Some(&[850.0, 500.0]),
        None,
    ));
    let shape: Vec<(u32, f64)> = per_level
        .iter()
        .map(|c| {
            let (nodes, levels) = nodes(c);
            (nodes[0].0.hour(), levels[0].unwrap())
        })
        .collect();
    assert_eq!(shape, [(9, 850.0), (9, 500.0), (10, 850.0), (10, 500.0)]);
    for coverage in &per_level {
        assert_reads_like_position(&engine, coverage, &["air_temperature_pl"], None);
    }
    let every = coverages_of(engine.query_trajectory(
        "LINESTRING(5 58, 30 66)",
        Some((nine, nine)),
        Some(&names(&["air_temperature_pl"])),
        None,
        None,
    ));
    let levels: Vec<f64> = every.iter().map(|c| nodes(c).1[0].unwrap()).collect();
    assert_eq!(
        levels,
        [1000.0, 850.0, 500.0, 300.0],
        "every level, bottom first"
    );

    // A ZM path: each sample's own step and level.
    let zm = single(
        engine
            .query_trajectory(
                &format!(
                    "LINESTRING ZM(5 58 1000 {}, 30 66 500 {})",
                    m("2026-10-15T09:00:00Z"),
                    m("2026-10-15T10:00:00Z")
                ),
                None,
                Some(&names(&parameters)),
                None,
                None,
            )
            .unwrap(),
    );
    let (nodes_zm, levels) = nodes(&zm);
    assert_eq!(
        (nodes_zm[0].0, levels[0], nodes_zm[nodes_zm.len() - 1].0),
        (nine, Some(1000.0), ten)
    );
    assert_reads_like_position(&engine, &zm, &parameters, None);

    // A level outside the axis, `z` with a Z path, or a `z` naming no level:
    // 400s.
    for (coords, z) in [
        ("LINESTRING Z(5 58 1000, 30 66 200)", None),
        ("LINESTRING Z(5 58 1000, 30 66 300)", Some(&[850.0][..])),
        ("LINESTRING(5 58, 30 66)", Some(&[925.0][..])),
    ] {
        assert!(
            matches!(
                engine.query_trajectory(coords, Some((nine, nine)), None, z, None),
                Err(DataServerError::InvalidParameter(_))
            ),
            "{coords} {z:?}"
        );
    }
}

/// The derived wind speed along a path is the speed of the components the
/// engine reads along it.
#[test]
fn a_derived_wind_speed_along_a_path_is_the_components_speed() {
    use ds_core::wind::DerivedWind;
    let engine = Arc::new(meps());
    let wind = DerivedWind::new(
        "meps",
        engine.clone(),
        Arc::new(|_: &str, _: &ds_core::wind::PairOutcome| {}),
    );
    let ten = at("2026-10-15T10:00:00Z");
    let path = "LINESTRING(5 58, 30 66)";
    let speed = single(
        EdrEngine::query_trajectory(
            &wind,
            path,
            Some((ten, ten)),
            Some(&names(&["wind_speed_10m"])),
            None,
            None,
        )
        .unwrap(),
    );
    let components = single(
        engine
            .query_trajectory(
                path,
                Some((ten, ten)),
                Some(&names(&["x_wind_10m", "y_wind_10m"])),
                None,
                None,
            )
            .unwrap(),
    );
    let speeds = &speed.ranges["wind_speed_10m"].values;
    let (u, v) = (
        &components.ranges["x_wind_10m"].values,
        &components.ranges["y_wind_10m"].values,
    );
    assert_eq!(speeds.len(), u.len());
    for ((s, u), v) in speeds.iter().zip(u).zip(v) {
        assert_eq!(*s, u.zip(*v).map(|(u, v)| u.hypot(v)));
    }
    assert!(speeds.iter().all(Option::is_some));
}

/// The global grid's columns wrap: a path from 170°E to 170°W takes the
/// short way across the antimeridian, every sample on the grid and read
/// as position reads it.
#[test]
fn a_path_crosses_the_antimeridian_on_a_global_grid() {
    let engine = global();
    let t = at("2026-01-01T06:00:00Z");
    let result = single(
        engine
            .query_trajectory(
                "LINESTRING(170 15, -170 15)",
                Some((t, t)),
                Some(&names(&["air_temperature"])),
                None,
                None,
            )
            .unwrap(),
    );
    let (nodes, _) = nodes(&result);
    assert!(nodes.len() >= 4, "{nodes:?}");
    assert!(
        nodes.iter().all(|n| n.1.abs() >= 170.0),
        "the short way, across 180°: {nodes:?}"
    );
    assert_eq!(
        assert_reads_like_position(&engine, &result, &["air_temperature"], None),
        nodes.len()
    );
}

// ---------------------------------------------------------------------------
// Cube
// ---------------------------------------------------------------------------

fn bbox(west: f64, south: f64, east: f64, north: f64) -> Bbox {
    Bbox::new(west, south, east, north).unwrap()
}

const NATIVE: CubeResolution = CubeResolution {
    x: None,
    y: None,
    z: None,
};

/// A cube on pressure levels is a `[t, z, y, x]` Grid: the steps of the
/// `datetime` window, `z`'s levels, and every cell sampled as a position
/// query samples the point, each (step, level) field read once.
#[test]
fn a_pressure_level_cube_samples_each_cell_like_position() {
    let engine = meps_pl();
    let parameters = ["air_temperature_pl", "geopotential_pl"];
    let (nine, ten) = (at("2026-10-15T09:00:00Z"), at("2026-10-15T10:00:00Z"));
    let cube = single(
        engine
            .query_cube(
                &bbox(10.0, 58.0, 25.0, 66.0),
                Some((nine, ten)),
                Some(&names(&parameters)),
                Some(&[850.0, 500.0]),
                NATIVE,
                None,
            )
            .unwrap(),
    );
    assert_eq!(
        engine.field_decodes(),
        2 * 2 * 2,
        "steps × levels × variables"
    );
    let DomainDescription::Grid { x, y, t, z } = &cube.domain else {
        panic!("{:?}", cube.domain)
    };
    assert_eq!(t.as_deref(), Some(&[nine, ten][..]));
    assert_eq!(z.as_ref().unwrap().values, [850.0, 500.0], "z's order");
    assert!(x.windows(2).all(|w| w[0] < w[1]) && y.windows(2).all(|w| w[0] > w[1]));
    assert!(x[0] > 10.0 && x[x.len() - 1] < 25.0 && y[0] < 66.0 && y[y.len() - 1] > 58.0);
    let range = &cube.ranges["air_temperature_pl"];
    assert_eq!(range.shape, [2, 2, y.len(), x.len()]);
    assert_eq!(range.axis_names, ["t", "z", "y", "x"]);
    let values = assert_cube_reads_like_position(&engine, &cube, &parameters, None);
    assert_eq!(
        values,
        2 * 2 * 2 * x.len() * y.len(),
        "every cell on the grid"
    );

    // Without `z` every level, bottom first; without `datetime` the step a
    // map without TIME draws; an instance its own run's.
    let six = at("2026-10-15T06:00:00Z");
    let pinned = single(
        engine
            .query_cube(
                &bbox(10.0, 58.0, 25.0, 66.0),
                None,
                Some(&names(&["air_temperature_pl"])),
                None,
                NATIVE,
                Some(six),
            )
            .unwrap(),
    );
    let DomainDescription::Grid { t, z, .. } = &pinned.domain else {
        unreachable!()
    };
    assert_eq!(t.as_deref(), Some(&[at("2026-10-15T08:00:00Z")][..]));
    assert_eq!(z.as_ref().unwrap().values, [1000.0, 850.0, 500.0, 300.0]);
    assert_cube_reads_like_position(&engine, &pinned, &["air_temperature_pl"], Some(six));
}

/// `resolution-x`/`-y` are evenly spaced positions from edge to edge, both
/// included, sampled like position; `resolution-z` positions from the
/// lowest selected level to the highest take the nearest level's values,
/// and a level no position takes is not read.
#[test]
fn resolution_sets_the_positions_and_reads_only_the_levels_taken() {
    let engine = meps_pl();
    let nine = at("2026-10-15T09:00:00Z");
    let resolution = CubeResolution {
        x: Some(4),
        y: Some(3),
        z: Some(3),
    };
    let cube = single(
        engine
            .query_cube(
                &bbox(10.0, 58.0, 25.0, 66.0),
                Some((nine, nine)),
                Some(&names(&["air_temperature_pl"])),
                None,
                resolution,
                None,
            )
            .unwrap(),
    );
    let DomainDescription::Grid { x, y, z, .. } = &cube.domain else {
        unreachable!()
    };
    assert_eq!(x, &[10.0, 15.0, 20.0, 25.0]);
    assert_eq!(y, &[66.0, 62.0, 58.0]);
    // 300, 650 and 1000 hPa: 650 takes 500's values (150 away, 850 200).
    assert_eq!(z.as_ref().unwrap().values, [300.0, 650.0, 1000.0]);
    assert_eq!(engine.field_decodes(), 3, "850 hPa is not read");
    let values = &cube.ranges["air_temperature_pl"].values;
    let plane = 12;
    let at_500 = positions_at(
        &engine,
        &y.iter()
            .flat_map(|&lat| x.iter().map(move |&lon| (lon, lat)))
            .collect::<Vec<_>>(),
        nine,
        &["air_temperature_pl"],
        Some(500.0),
        None,
    );
    for (cell, read) in at_500.iter().enumerate() {
        assert_eq!(values[plane + cell], read["air_temperature_pl"]);
    }
    for (zi, level) in [(0, 300.0), (2, 1000.0)] {
        let points: Vec<(f64, f64)> = y
            .iter()
            .flat_map(|&lat| x.iter().map(move |&lon| (lon, lat)))
            .collect();
        let read = positions_at(
            &engine,
            &points,
            nine,
            &["air_temperature_pl"],
            Some(level),
            None,
        );
        for (cell, read) in read.iter().enumerate() {
            assert_eq!(values[zi * plane + cell], read["air_temperature_pl"]);
        }
    }
}

/// On a single-level collection a cube is a `[t, y, x]` Grid with no `z`,
/// at native resolution laid out cell for cell as an area over the same
/// bbox; `resolution-z` there is a 400.
#[test]
fn a_surface_cube_is_an_area_with_a_time_axis() {
    let engine = meps();
    let parameters = ["air_temperature_2m", "air_pressure_at_sea_level"];
    let cube = single(
        engine
            .query_cube(
                &bbox(5.0, 56.0, 30.0, 68.0),
                None,
                Some(&names(&parameters)),
                Some(&[850.0]),
                NATIVE,
                None,
            )
            .unwrap(),
    );
    let DomainDescription::Grid { x, y, t, z } = &cube.domain else {
        unreachable!()
    };
    assert!(z.is_none(), "no vertical axis: z ignored");
    assert_eq!(
        t.as_deref(),
        Some(&[at("2026-10-15T11:00:00Z")][..]),
        "the step a map without TIME draws"
    );
    let range = &cube.ranges["air_pressure_at_sea_level"];
    assert_eq!(range.axis_names, ["t", "y", "x"]);
    assert_eq!(range.shape, [1, y.len(), x.len()]);
    assert_eq!(cube.parameters["air_pressure_at_sea_level"].unit, "hPa");
    assert!(x.len() > 1 && y.len() > 1, "{x:?} {y:?}");
    assert_cube_reads_like_position(&engine, &cube, &parameters, None);

    let area = single(
        engine
            .query_area("5,56,30,68", None, Some(&names(&parameters)), None, None)
            .unwrap(),
    );
    let DomainDescription::Grid {
        x: area_x,
        y: area_y,
        ..
    } = &area.domain
    else {
        unreachable!()
    };
    assert_eq!((x, y), (area_x, area_y), "the area's cells");
    for parameter in parameters {
        assert_eq!(cube.ranges[parameter].values, area.ranges[parameter].values);
    }

    let resolution_z = CubeResolution {
        z: Some(2),
        ..NATIVE
    };
    assert!(matches!(
        engine.query_cube(
            &bbox(5.0, 56.0, 30.0, 68.0),
            None,
            None,
            None,
            resolution_z,
            None
        ),
        Err(DataServerError::InvalidParameter(_))
    ));
}

/// A bbox off the grid is a 404 and reads nothing; a response over the
/// value budget is a 400 before any read; a cube across the antimeridian
/// of a global grid wraps its columns.
#[test]
fn a_cube_off_the_grid_or_over_the_budget_reads_nothing() {
    let engine = meps();
    // Outside the extent, and inside it but off the curved Lambert grid,
    // whose western edge lies near 0° at 51°N.
    for bbox in [
        bbox(100.0, 0.0, 110.0, 10.0),
        bbox(-15.0, 50.5, -12.0, 52.0),
    ] {
        assert!(matches!(
            engine.query_cube(&bbox, None, None, None, NATIVE, None),
            Err(DataServerError::LocationNotFound(_))
        ));
    }
    let huge = CubeResolution {
        x: Some(2000),
        y: Some(2000),
        z: None,
    };
    let refused = engine.query_cube(&bbox(5.0, 56.0, 30.0, 68.0), None, None, None, huge, None);
    assert!(
        matches!(&refused, Err(DataServerError::QueryTooLarge(m)) if m.contains("Cube query")),
        "{refused:?}"
    );
    assert_eq!(engine.field_decodes(), 0);

    let global = global();
    let t = at("2026-01-01T12:00:00Z");
    let seam = single(
        global
            .query_cube(
                &bbox(170.0, 10.0, -170.0, 20.0),
                Some((t, t)),
                Some(&names(&["air_temperature"])),
                None,
                NATIVE,
                None,
            )
            .unwrap(),
    );
    let DomainDescription::Grid { x, y, .. } = &seam.domain else {
        unreachable!()
    };
    assert_eq!(
        x,
        &[172.5, 177.5, -177.5, -172.5],
        "the 5° nodes, eastwards"
    );
    assert_eq!(y, &[17.5, 12.5]);
    assert_eq!(
        assert_cube_reads_like_position(&global, &seam, &["air_temperature"], None),
        8
    );
}

/// A query past its deadline is the 504 every EDR query gets, before it
/// reads anything.
#[test]
fn a_query_past_its_deadline_reads_nothing() {
    let engine = meps();
    let _deadline = ds_core::deadline::enter(Some(std::time::Instant::now()));
    assert!(matches!(
        engine.query_trajectory("LINESTRING(5 58, 30 66)", None, None, None, None),
        Err(DataServerError::DeadlineExceeded)
    ));
    assert!(matches!(
        engine.query_cube(&bbox(5.0, 56.0, 30.0, 68.0), None, None, None, NATIVE, None),
        Err(DataServerError::DeadlineExceeded)
    ));
    assert_eq!(engine.field_decodes(), 0);
}
