//! A precipitation rate is served in mm/h (#1213): WMO Code Table 4.2
//! gives PRATE (0/1/7) in kg m-2 s-1, and `units::default_display`
//! multiplies by 3600, as `ds_core::units::display_conversion` does for
//! every other engine. A rate accumulated over the step's window is an
//! amount, served in mm (#886).

use super::*;
use crate::test_support::{from_ecmwf, parameter_message, statistically_processed, TestSource};
use crate::units::ACCUMULATION;

/// PRATE at the surface, the same rate at every node: 2.5e-4 kg m-2 s-1,
/// 0.9 mm/h.
fn prate(rate: f32) -> Vec<u8> {
    parameter_message(1, 7, 0x30, rate, [0; 4], 1, 0)
}

/// The map pixel, the EDR position value and the reported unit agree, all
/// in mm/h.
#[test]
fn prate_is_served_in_mm_per_hour() {
    let source = TestSource::new();
    source.write("f000", &[("PRATE", "surface", prate(2.5e-4))], 0);
    let engine = GribEngine::new("gfs", &source.config()).unwrap();
    // The stored rate, as the message packs it (an f32 reference value).
    let stored = f64::from(2.5e-4_f32);
    let served = stored * 3600.0;
    assert!((served - 0.9).abs() < 1e-6, "{served}");

    let info = engine.raster_info_shared();
    assert_eq!(info.parameter_unit(Some("PRATE")), Some("mm/h"));
    assert_eq!(engine.get_parameter_descriptions()["PRATE"].unit, "mm/h");

    let tile = engine
        .get_raster_tile(
            [0.0, 0.0, 1.0, 1.0],
            2,
            2,
            None,
            &OutputCrs::Wgs84,
            Some("PRATE"),
            None,
            None,
        )
        .unwrap();
    let pixels: Vec<f64> = tile.values.iter_values().flatten().collect();
    assert_eq!(pixels.len(), 4);
    for pixel in &pixels {
        assert!((pixel - served).abs() < 1e-6, "pixel {pixel}");
    }

    let names = ["PRATE".to_string()];
    let CoverageResponse::Single(result) = engine
        .query_position("POINT(0.5 0.5)", None, Some(&names), None, None)
        .unwrap()
    else {
        panic!("expected one series")
    };
    assert_eq!(result.parameters["PRATE"].unit, "mm/h");
    let value = result.ranges["PRATE"].values[0].unwrap();
    assert!((value - served).abs() < 1e-9, "position {value}");
    assert!((value - pixels[0]).abs() < 1e-6, "{value} vs {}", pixels[0]);
}

/// Every EDR range is sampled from decoded `f32`s, the display conversion
/// adding no digits, so each carries `f32` precision (#1341): CoverageJSON
/// writes the rate as the `f32` nearest it, `0.90000004`, not as its `f64`
/// `0.9000000427477062`.
#[test]
fn edr_ranges_carry_f32_precision() {
    let source = TestSource::new();
    source.write("f000", &[("PRATE", "surface", prate(2.5e-4))], 0);
    let engine = GribEngine::new("gfs", &source.config()).unwrap();
    let names = ["PRATE".to_string()];
    let ranges = |response: CoverageResponse| -> Vec<NdArray> {
        let coverages = match response {
            CoverageResponse::Single(q) => vec![q],
            CoverageResponse::Collection(v) => v,
        };
        coverages
            .into_iter()
            .flat_map(|q| q.ranges.into_values())
            .collect()
    };
    let position = engine
        .query_position("POINT(0.5 0.5)", None, Some(&names), None, None)
        .unwrap();
    let position = ranges(position);
    let value = position[0].values[0].unwrap();
    assert_eq!(value.to_string(), "0.9000000427477062");
    assert_eq!((value as f32).to_string(), "0.90000004");
    let area = engine
        .query_area(
            "POLYGON((0 0, 1 0, 1 1, 0 1, 0 0))",
            None,
            Some(&names),
            None,
            None,
        )
        .unwrap();
    let trajectory = engine
        .query_trajectory(
            "LINESTRING(0.25 0.25, 0.75 0.75)",
            None,
            Some(&names),
            None,
            None,
        )
        .unwrap();
    for (query, ranges) in [
        ("position", position),
        ("area", ranges(area)),
        ("trajectory", ranges(trajectory)),
    ] {
        assert!(!ranges.is_empty(), "{query}");
        for range in ranges {
            assert_eq!(range.precision, ValuePrecision::F32, "{query}");
        }
    }
}

/// #886: ECMWF `tcw` and `cp` as ECMWF open data encodes them (read from
/// the live 2026-10-24 00Z headers; see `units::tests`), each served with
/// its name and in mm: the reported unit, the map pixel and the EDR value
/// agree. AIFS `cp` is the rate 0/1/37 accumulated over the step's window,
/// an amount: its value is the stored one, not × 3600 as a rate's would be.
#[test]
fn tcw_and_cp_are_named_and_served_in_mm() {
    let message =
        |number: u8, base: f32| from_ecmwf(parameter_message(1, number, 0x30, base, [0; 4], 1, 0));
    // tcw: WMO 0/1/51, template 4.0, from the ground (1) to the nominal top
    // of the atmosphere (8), in both IFS and AIFS.
    let mut tcw = message(51, 25.0);
    tcw[109 + 28] = 8; // Section 4 octet 29: type of second fixed surface
                       // AIFS cp: WMO 0/1/37 in template 4.8, accumulated over 0-6 h.
    let aifs_cp = statistically_processed(message(37, 3.5), 8, ACCUMULATION, 6);
    // IFS cp: ECMWF local 0/1/195, metres of water, accumulated likewise.
    let ifs_cp = statistically_processed(message(195, 0.0035), 8, ACCUMULATION, 6);
    for (name, bytes, label, served) in [
        ("tcw", tcw, "Total column water (surface)", 25.0),
        ("cp", aifs_cp, "Convective precipitation (surface)", 3.5),
        (
            "cp",
            ifs_cp,
            "Convective precipitation (surface)",
            f64::from(0.0035_f32) * 1000.0,
        ),
    ] {
        let source = TestSource::new();
        source.write("f006", &[(name, "surface", bytes)], 6);
        let engine = GribEngine::new("ecmwf", &source.config()).unwrap();

        let info = engine.raster_info_shared();
        assert_eq!(info.parameter_unit(Some(name)), Some("mm"), "{label}");
        let description = &engine.get_parameter_descriptions()[name];
        assert_eq!(
            (description.label.as_str(), description.unit.as_str()),
            (label, "mm")
        );

        let tile = engine
            .get_raster_tile(
                [0.0, 0.0, 1.0, 1.0],
                2,
                2,
                None,
                &OutputCrs::Wgs84,
                Some(name),
                None,
                None,
            )
            .unwrap();
        let pixels: Vec<f64> = tile.values.iter_values().flatten().collect();
        assert_eq!(pixels.len(), 4);
        for pixel in &pixels {
            assert!((pixel - served).abs() < 1e-4, "{label}: pixel {pixel}");
        }

        let names = [name.to_string()];
        let CoverageResponse::Single(result) = engine
            .query_position("POINT(0.5 0.5)", None, Some(&names), None, None)
            .unwrap()
        else {
            panic!("expected one series")
        };
        assert_eq!(result.parameters[name].unit, "mm");
        let value = result.ranges[name].values[0].unwrap();
        assert!((value - served).abs() < 1e-4, "{label}: position {value}");
    }
}
