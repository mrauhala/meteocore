//! A precipitation rate is served in mm/h (#1213): WMO Code Table 4.2
//! gives PRATE (0/1/7) in kg m-2 s-1, and `units::default_display`
//! multiplies by 3600, as `ds_core::units::display_conversion` does for
//! every other engine.

use super::*;
use crate::test_support::{parameter_message, TestSource};

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
