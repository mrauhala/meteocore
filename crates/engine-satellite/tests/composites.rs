//! RGB composite layers (#819 phase 4) against the cropped GOES-19 scans in
//! `testdata/goes19-abi`, configured through `[[satellite.composites]]`
//! TOML: a composite's time axis is the scans every band has, it resolves
//! as its bands do, and it is not a band.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::config::SatelliteConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::map_engine::{default_request_time, MapEngine, OutputCrs};
use engine_satellite::SatelliteEngine;

const C13: &str = "OR_ABI-L2-CMIPF-M6C13_G19_s20262681900199_e20262681909519_c20262681909592.nc";
const ACHT: &str = "OR_ABI-L2-ACHTF-M6_G19_s20262681900199_e20262681909507_c20262681912337.nc";

const IR: &str = "ir_10_3";
const CLOUD: &str = "cloud_top_temperature";
/// IR minus cloud top temperature in red, cloud top in green, IR in blue.
const RGB: &str = "ir_cloud";
/// Every channel reads IR.
const GREY: &str = "ir_grey";

fn at(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

/// `fixture` republished as the scan starting at `hhmm` on 2026-09-25, in
/// the nested directory the engine lists recursively.
fn publish(dir: &Path, fixture: &str, hhmm: &str) {
    let nested = dir.join("ABI-L2/2026/268/19");
    std::fs::create_dir_all(&nested).unwrap();
    let from: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/goes19-abi")
        .join(fixture);
    let name = fixture.replace("_s20262681900199_", &format!("_s2026268{hhmm}199_"));
    std::fs::copy(from, nested.join(name)).unwrap();
}

fn config(dir: &Path) -> SatelliteConfig {
    toml::from_str(&format!(
        r#"
        data_path = '{}'

        [[products]]
        parameter = "{IR}"
        title = "IR 10.3 µm brightness temperature"
        unit = "K"
        product = "ABI-L2-CMIPF"
        band = 13
        variable = "CMI"

        [[products]]
        parameter = "{CLOUD}"
        title = "Cloud top temperature"
        unit = "K"
        product = "ABI-L2-ACHTF"
        variable = "TEMP"

        [[composites]]
        name = "{RGB}"
        title = "IR and cloud top"
        red = {{ parameter = "{IR}", minus = "{CLOUD}", min = -10, max = 40 }}
        green = {{ parameter = "{CLOUD}", min = 330.0, max = 180.0 }}
        blue = {{ parameter = "{IR}", min = 330.0, max = 180.0, gamma = 1.5 }}

        [[composites]]
        name = "{GREY}"
        red = {{ parameter = "{IR}", min = 330.0, max = 180.0 }}
        green = {{ parameter = "{IR}", min = 330.0, max = 180.0 }}
        blue = {{ parameter = "{IR}", min = 330.0, max = 180.0 }}
        "#,
        dir.display()
    ))
    .unwrap()
}

/// An engine over the fixtures published at `ir` and `cloud` scan starts
/// (`HHMM`), polled once.
fn engine(id: &str, ir: &[&str], cloud: &[&str]) -> (SatelliteEngine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    for hhmm in ir {
        publish(dir.path(), C13, hhmm);
    }
    for hhmm in cloud {
        publish(dir.path(), ACHT, hhmm);
    }
    let engine = SatelliteEngine::new(id, &config(dir.path())).unwrap();
    engine.poll_once();
    (engine, dir)
}

fn times(engine: &SatelliteEngine, parameter: &str) -> Vec<DateTime<Utc>> {
    engine.parameter_times(parameter).unwrap().to_vec()
}

/// The composites are layers of their own: defined from config, kept out
/// of the parameter lists, each on the scans all its bands have. Their axes
/// come from the snapshot and follow a new scan.
#[test]
fn a_composite_is_a_layer_on_the_scans_every_band_has() {
    // IR has 19:10 and cloud top 19:20, which the other lacks.
    let (engine, dir) = engine("goes19-composites", &["1900", "1910"], &["1900", "1920"]);
    let (t0, t1, t2) = (
        at("2026-09-25T19:00:00Z"),
        at("2026-09-25T19:10:00Z"),
        at("2026-09-25T19:20:00Z"),
    );

    let composites = engine.composites();
    assert!(Arc::ptr_eq(&composites, &engine.composites()));
    let names: Vec<&str> = composites.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, [RGB, GREY]);
    let rgb = &composites[0];
    assert_eq!(rgb.title, "IR and cloud top");
    assert_eq!(rgb.parameters(), [IR, CLOUD]);
    assert_eq!(rgb.channels[0].minus.as_deref(), Some(CLOUD));
    assert_eq!(
        rgb.channels.each_ref().map(|c| (c.min, c.max, c.gamma)),
        [(-10.0, 40.0, 1.0), (330.0, 180.0, 1.0), (330.0, 180.0, 1.5)]
    );
    // A composite without a title is titled by its name.
    assert_eq!(composites[1].title, GREY);
    assert_eq!(composites[1].parameters(), [IR]);

    // Not a parameter of any API until the layers are wired from
    // `composites()`: the parameter lists and EDR are unchanged.
    let info = engine.raster_info();
    let parameters: Vec<&str> = info.parameters.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(parameters, [IR, CLOUD]);
    assert_eq!(engine.get_parameters(), [IR, CLOUD]);
    assert_eq!(engine.get_parameter_available_times(RGB), None);
    assert_eq!(info.times, [t0, t1, t2]);

    // Neither band's extra scan extends the composite: only 19:00 is in
    // both. A one-band composite has its band's axis.
    assert_eq!(times(&engine, IR), [t0, t1]);
    assert_eq!(times(&engine, CLOUD), [t0, t2]);
    assert_eq!(times(&engine, RGB), [t0]);
    assert_eq!(times(&engine, GREY), [t0, t1]);
    assert_eq!(engine.parameter_times("nope"), None);
    // Served from the snapshot, not intersected per call.
    assert!(Arc::ptr_eq(
        &engine.parameter_times(RGB).unwrap(),
        &engine.parameter_times(RGB).unwrap()
    ));

    // Cloud top 19:10 lands: now both bands have it.
    publish(dir.path(), ACHT, "1910");
    engine.poll_once();
    assert_eq!(times(&engine, CLOUD), [t0, t1, t2]);
    assert_eq!(times(&engine, RGB), [t0, t1]);
    assert_eq!(
        default_request_time(&engine, &engine.raster_info(), Some(RGB)),
        Some(t1)
    );
}

/// A composite's name resolves to the scan `resolve_parameters_time`
/// picks for its bands, which is the scan `get_raster_tiles` renders them
/// from; every scan on its axis resolves to itself.
#[test]
fn a_composite_resolves_as_its_bands_do() {
    let (engine, _dir) = engine(
        "goes19-composites-resolve",
        &["1900", "1910"],
        &["1900", "1910", "1920"],
    );
    let (t0, t1) = (at("2026-09-25T19:00:00Z"), at("2026-09-25T19:10:00Z"));
    let composites = engine.composites();
    let bands = composites[0].parameters();
    let resolve = |time| engine.resolve_parameter_time(Some(RGB), time, None);
    for time in [
        None,
        Some(at("2026-09-25T18:00:00Z")),
        Some(t0),
        Some(at("2026-09-25T19:05:00Z")),
        Some(t1),
        Some(at("2026-09-25T19:15:00Z")),
        Some(at("2026-09-25T19:20:00Z")),
        Some(at("2026-09-25T20:00:00Z")),
    ] {
        assert_eq!(
            resolve(time),
            engine.resolve_parameters_time(&bands, time, None),
            "{time:?}"
        );
    }
    // 19:20 is cloud top only: the composite snaps to 19:10, its latest.
    assert_eq!(resolve(None), Some(t1));
    assert_eq!(resolve(Some(at("2026-09-25T19:20:00Z"))), Some(t1));
    assert_eq!(resolve(Some(at("2026-09-25T19:05:00Z"))), Some(t0));
    assert_eq!(resolve(Some(at("2026-09-25T18:00:00Z"))), Some(t0));
    let axis = times(&engine, RGB);
    assert_eq!(axis, [t0, t1]);
    for time in &axis {
        assert_eq!(resolve(Some(*time)), Some(*time));
    }
    // The bands keep their own axes; `resolve_time` is still the first
    // product's.
    assert_eq!(
        engine.resolve_parameter_time(Some(CLOUD), None, None),
        Some(at("2026-09-25T19:20:00Z"))
    );
    assert_eq!(engine.resolve_time(None, None), Some(t1));

    // The resolved scan renders every band, as the API layer will ask.
    let extent = engine.raster_info().spatial_extent.unwrap();
    let render = |time| {
        engine
            .get_raster_tiles(extent, 32, 32, time, &OutputCrs::Wgs84, &bands, None, None)
            .unwrap()
    };
    let pinned = render(resolve(None));
    assert_eq!(pinned.len(), 2);
    assert!(pinned.iter().all(|tile| !tile.is_empty()));
    for (a, b) in pinned.iter().zip(render(None)) {
        assert!(a.values.iter_values().eq(b.values.iter_values()));
    }
}

/// Bands that share no scan give the composite an empty axis and no time.
#[test]
fn a_composite_without_a_shared_scan_is_empty() {
    let (engine, _dir) = engine("goes19-composites-disjoint", &["1900"], &["1920"]);
    assert!(times(&engine, RGB).is_empty());
    assert_eq!(times(&engine, GREY), [at("2026-09-25T19:00:00Z")]);
    assert_eq!(engine.resolve_parameter_time(Some(RGB), None, None), None);
    assert_eq!(
        engine.resolve_parameter_time(Some(RGB), Some(at("2026-09-25T19:20:00Z")), None),
        None
    );
}

/// A composite has no values of its own: rendering or querying it as a
/// band fails with a message that says so and names its bands.
#[test]
fn a_composite_is_not_a_band() {
    let (engine, _dir) = engine("goes19-composites-band", &["1900"], &["1900"]);
    let extent = engine.raster_info().spatial_extent.unwrap();
    let is_composite_error = |result: Result<(), DataServerError>| match result {
        Err(DataServerError::InvalidParameter(message)) => {
            assert!(
                message.contains("'ir_cloud' is an RGB composite"),
                "{message}"
            );
            assert!(message.contains(&format!("{IR}, {CLOUD}")), "{message}");
        }
        other => panic!("expected a composite error, got {other:?}"),
    };
    is_composite_error(
        engine
            .get_raster_tile(
                extent,
                16,
                16,
                None,
                &OutputCrs::Wgs84,
                Some(RGB),
                None,
                None,
            )
            .map(drop),
    );
    is_composite_error(
        engine
            .get_raster_tiles(extent, 16, 16, None, &OutputCrs::Wgs84, &[RGB], None, None)
            .map(drop),
    );
    is_composite_error(
        engine
            .query_position(
                "POINT(-75 0)",
                None,
                Some(&[RGB.to_string()][..]),
                None,
                None,
            )
            .map(drop),
    );
    // Any other unknown name is still just not served.
    match engine.get_raster_tile(
        extent,
        16,
        16,
        None,
        &OutputCrs::Wgs84,
        Some("nope"),
        None,
        None,
    ) {
        Err(DataServerError::InvalidParameter(message)) => {
            assert!(message.contains("'nope' is not served"), "{message}")
        }
        other => panic!(
            "expected an unknown-parameter error, got {:?}",
            other.map(drop)
        ),
    }
}

/// The Airmass example in the README and `collections.d/goes19-fd.toml`
/// validates and builds: four ABI bands, two differences, one band read
/// twice.
#[test]
fn the_documented_airmass_example_builds() {
    let dir = tempfile::tempdir().unwrap();
    let product = |parameter: &str, band: u8| {
        format!(
            "[[products]]\nparameter = \"{parameter}\"\ntitle = \"{parameter}\"\nunit = \"K\"\n\
             product = \"ABI-L2-CMIPF\"\nband = {band}\nvariable = \"CMI\"\n"
        )
    };
    let config: SatelliteConfig = toml::from_str(&format!(
        r#"
        data_path = '{}'
        {}{}{}{}
        [[composites]]
        name = "airmass"
        title = "Airmass RGB"
        red = {{ parameter = "wv_6_2", minus = "wv_7_3", min = -25.0, max = 0.0 }}
        green = {{ parameter = "ir_9_6", minus = "ir_10_3", min = -40.0, max = 5.0 }}
        blue = {{ parameter = "wv_6_2", min = 243.0, max = 208.0 }}
        "#,
        dir.path().display(),
        product("wv_6_2", 8),
        product("wv_7_3", 10),
        product("ir_9_6", 12),
        product("ir_10_3", 13),
    ))
    .unwrap();
    let engine = SatelliteEngine::new("goes19-airmass", &config).unwrap();
    let composites = engine.composites();
    assert_eq!(
        composites[0].parameters(),
        ["wv_6_2", "wv_7_3", "ir_9_6", "ir_10_3"]
    );
    // Nothing is polled yet: an empty axis, and no time to resolve.
    assert!(times(&engine, "airmass").is_empty());
    assert_eq!(
        engine.resolve_parameter_time(Some("airmass"), None, None),
        None
    );
}
