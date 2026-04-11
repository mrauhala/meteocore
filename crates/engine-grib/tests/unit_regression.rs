//! Unit-conversion regression snapshot.
//!
//! The old engine had a hardcoded `params::ecmwf_param_info` table that
//! mapped ECMWF short names to display units. The new implementation reads
//! the WMO `(discipline, category, parameter_number)` triple out of every
//! decoded GRIB2 message and looks up the source unit in `units.rs`, then
//! derives the display unit mechanically.
//!
//! This test locks in the display values produced by the new resolver for
//! the committed ECMWF sample message, guarding against regressions during
//! future unit-table changes. The committed sample is specific humidity at
//! 150 hPa (ECMWF `q`, WMO triple (0, 1, 0), unit `kg kg-1`), which the
//! resolver must classify as `SourceUnit::KgPerKg` → identity conversion.

use engine_grib::reader::decode_message;
use engine_grib::units::{default_display, lookup, SourceUnit};

#[test]
fn ecmwf_specific_humidity_sample_triple_and_unit() {
    let path = std::path::Path::new("../../testdata/ecmwf/sample-message.grib2");
    if !path.exists() {
        eprintln!("Skipping: ECMWF sample GRIB2 not present");
        return;
    }

    let bytes = std::fs::read(path).unwrap();
    let grid = decode_message(&bytes, "q").expect("sample must decode");

    // Triple derived from the decoded message, not from any hardcoded table.
    let (discipline, category, number) = grid.triple;
    assert_eq!(
        (discipline, category, number),
        (0, 1, 0),
        "ECMWF sample-message is specific humidity = (0, 1, 0)"
    );

    // Originating centre should be ECMWF (98).
    assert_eq!(grid.centre, 98);

    // Unit resolver must map (0, 1, 0) to SourceUnit::KgPerKg.
    let info = lookup(grid.centre, discipline, category, number)
        .expect("specific humidity must be in the curated WMO table");
    assert_eq!(info.label, "Specific humidity");
    assert_eq!(info.source_unit, SourceUnit::KgPerKg);

    // Display conversion for kg/kg is identity.
    let display = default_display(info.source_unit);
    assert_eq!(display.display_unit, "kg kg-1");
    assert!((display.scale - 1.0).abs() < 1e-12);
    assert!(display.offset.abs() < 1e-12);
    assert!(!display.has_conversion());

    // A plausible stratospheric specific humidity value (~5e-6 kg/kg) must
    // round-trip unchanged through the identity conversion.
    let raw = 5e-6_f64;
    assert!((display.convert(raw) - raw).abs() < 1e-18);
}

/// Snapshot test: for a fixed set of WMO triples commonly used in ECMWF open
/// data, the new triple-based resolver must produce exactly the same display
/// values as the previous hardcoded `ecmwf_param_info` table did. The
/// expected values here are copied verbatim from the removed table so that
/// any drift is caught immediately.
#[test]
fn ecmwf_unit_conversion_snapshot() {
    // (label, (centre, discipline, category, number), expected_display_unit,
    //  raw_input, expected_output)
    let cases = [
        // 2t: (98, 0, 0, 0) Kelvin → °C offset -273.15
        ("2t (Temperature)", (98, 0, 0, 0), "°C", 293.15, 20.0),
        // msl: (98, 0, 3, 1) Pascal → hPa scale 0.01
        (
            "msl (Pressure reduced to MSL)",
            (98, 0, 3, 1),
            "hPa",
            101325.0,
            1013.25,
        ),
        // 10u / 10v: (98, 0, 2, 2) and (98, 0, 2, 3) m s-1 → identity
        ("u-component of wind", (98, 0, 2, 2), "m s-1", 12.5, 12.5),
        ("v-component of wind", (98, 0, 2, 3), "m s-1", -3.2, -3.2),
        // z: (98, 0, 3, 4) Geopotential m² s⁻² → gpm (divide by g = 9.80665)
        // 9806.65 m²/s² = 1000 gpm
        ("Geopotential", (98, 0, 3, 4), "gpm", 9806.65, 1000.0),
        // Precipitation rate: (98, 0, 1, 7) kg m-2 s-1 → identity
        (
            "Precipitation rate",
            (98, 0, 1, 7),
            "kg m-2 s-1",
            1e-4,
            1e-4,
        ),
    ];

    for (name, (centre, disc, cat, num), expected_unit, raw, expected) in cases {
        let info = lookup(centre, disc, cat, num)
            .unwrap_or_else(|| panic!("{name}: triple ({disc},{cat},{num}) must be in table"));
        let display = default_display(info.source_unit);

        assert_eq!(
            display.display_unit, expected_unit,
            "{name}: display unit mismatch"
        );

        let got = display.convert(raw);
        assert!(
            (got - expected).abs() < 1e-6,
            "{name}: convert({raw}) = {got}, expected {expected}"
        );
    }
}

/// The critical cloud-cover asymmetry trap:
///
///   ECMWF ships `tcc` under local parameter (98, 0, 6, 192) in proportion 0-1.
///   GFS ships `TCDC` under standard parameter (7, 0, 6, 1) in percent 0-100.
///
/// The old engine had a single hardcoded rule "tcc → ×100". That rule would
/// silently corrupt GFS values by multiplying them by 100 again. The new
/// resolver must produce the right conversion for each without any
/// per-provider code paths.
#[test]
fn cloud_cover_asymmetry_regression() {
    // ECMWF local path
    let ecmwf = lookup(98, 0, 6, 192).expect("ECMWF local tcc must be in overlay");
    assert_eq!(ecmwf.source_unit, SourceUnit::Proportion);
    let ecmwf_disp = default_display(ecmwf.source_unit);
    assert_eq!(ecmwf_disp.display_unit, "%");
    // 0.75 proportion → 75 %
    assert!((ecmwf_disp.convert(0.75) - 75.0).abs() < 1e-9);

    // GFS/NCEP standard path
    let gfs = lookup(7, 0, 6, 1).expect("standard total cloud cover must be in table");
    assert_eq!(gfs.source_unit, SourceUnit::Percent);
    let gfs_disp = default_display(gfs.source_unit);
    assert_eq!(gfs_disp.display_unit, "%");
    // 75 percent → 75 (identity, NOT ×100)
    assert!((gfs_disp.convert(75.0) - 75.0).abs() < 1e-9);
}

/// The geopotential-vs-geopotential-height asymmetry:
///
///   ECMWF `z` is (0, 3, 4) Geopotential in m² s⁻².
///   GFS `HGT` is (0, 3, 5) Geopotential height in gpm.
///
/// Both should produce gpm output, but only the Geopotential one divides
/// by gravity.
#[test]
fn geopotential_asymmetry_regression() {
    // Geopotential (m²/s²) — must divide by g
    let geo = lookup(0, 0, 3, 4).expect("Geopotential must be in table");
    assert_eq!(geo.source_unit, SourceUnit::M2PerS2);
    let geo_disp = default_display(geo.source_unit);
    assert_eq!(geo_disp.display_unit, "gpm");
    // 9806.65 m²/s² = 1000 gpm
    assert!((geo_disp.convert(9806.65) - 1000.0).abs() < 0.01);

    // Geopotential height (gpm) — already in gpm, identity
    let ght = lookup(0, 0, 3, 5).expect("Geopotential height must be in table");
    assert_eq!(ght.source_unit, SourceUnit::Gpm);
    let ght_disp = default_display(ght.source_unit);
    assert_eq!(ght_disp.display_unit, "gpm");
    // 1000 gpm → 1000 gpm
    assert!((ght_disp.convert(1000.0) - 1000.0).abs() < 1e-9);
}
