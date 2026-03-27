#![no_main]

use engine_geotiff::fuzz_exports::{Crs, GeoTransform};
use libfuzzer_sys::fuzz_target;

// Fuzz CRS forward/inverse transforms and GeoTransform methods
// with arbitrary floating-point inputs. Ensures no panics, no
// infinite loops, and no NaN/Inf propagation past safety checks.
fuzz_target!(|data: &[u8]| {
    if data.len() < 48 {
        return;
    }

    // Extract test values from fuzzer input
    let lon = f64::from_le_bytes(data[0..8].try_into().unwrap());
    let lat = f64::from_le_bytes(data[8..16].try_into().unwrap());
    let x = f64::from_le_bytes(data[16..24].try_into().unwrap());
    let y = f64::from_le_bytes(data[24..32].try_into().unwrap());
    let param1 = f64::from_le_bytes(data[32..40].try_into().unwrap());
    let param2 = f64::from_le_bytes(data[40..48].try_into().unwrap());

    // Skip non-finite inputs that would trivially propagate
    if !lon.is_finite() || !lat.is_finite() || !x.is_finite() || !y.is_finite() {
        return;
    }

    // Test all CRS variants
    let crs_variants: Vec<Crs> = vec![
        Crs::Wgs84,
        Crs::TransverseMercator {
            lat0: 0.0,
            lon0: param1.clamp(-std::f64::consts::PI, std::f64::consts::PI),
            k0: param2.clamp(0.5, 1.5),
            false_e: 500_000.0,
            false_n: 0.0,
        },
        Crs::LambertAzimuthalEqualArea {
            lat0: param1.clamp(-1.4, 1.4), // ~±80°
            lon0: param2.clamp(-std::f64::consts::PI, std::f64::consts::PI),
            false_e: 0.0,
            false_n: 0.0,
        },
        Crs::LambertConformalConic {
            lat1: 0.8,
            lat2: 1.2,
            lat0: 0.0,
            lon0: param1.clamp(-std::f64::consts::PI, std::f64::consts::PI),
            false_e: 0.0,
            false_n: 0.0,
        },
    ];

    for crs in &crs_variants {
        // Forward transform should not panic
        let _ = crs.forward(lon, lat);

        // Inverse transform should not panic, should return None or finite values
        if let Some((rlon, rlat)) = crs.inverse(x, y) {
            assert!(rlon.is_finite(), "inverse produced non-finite lon");
            assert!(rlat.is_finite(), "inverse produced non-finite lat");
        }
    }

    // Test GeoTransform methods with fuzzed pixel dimensions
    let width = ((param1.abs() as u32) % 10000).max(1);
    let height = ((param2.abs() as u32) % 10000).max(1);

    let gt = GeoTransform {
        origin_x: x,
        origin_y: y,
        pixel_width: if param1.abs() > 1e-10 { param1.abs() } else { 1.0 },
        pixel_height: if param2.abs() > 1e-10 { param2.abs() } else { 1.0 },
        width,
        height,
        crs: Crs::Wgs84,
    };

    // These should never panic
    let _ = gt.world_to_pixel(lon, lat);
    let _ = gt.bbox();
    let _ = gt.pixel_to_world(0, 0);
    let _ = gt.bbox_to_pixels(lon, lat, lon + 1.0, lat + 1.0);
});
