//! Mechanical display-unit conversions shared by the observation engines.
//!
//! Source units are whatever the format encodes (BUFR Table B, WMO GRIB
//! Code Table 4.2); clients want the everyday forms. The rules are keyed
//! on the **source unit string**, never on a parameter name (see the
//! unit-conversion rule in root `CLAUDE.md`), and mirror
//! `crates/engine-grib/src/units.rs::default_display` — GRIB keeps its
//! enum-keyed twin for now because its source units come from code-table
//! lookups; both tables must agree rule for rule.
//!
//! | source | display | rule |
//! |---|---|---|
//! | `K` | `°C` | − 273.15 |
//! | `Pa` | `hPa` | × 0.01 |
//! | `kg m-2` | `mm` | identity (1 kg m⁻² of water ≈ 1 mm) |
//! | `m2 s-2` | `gpm` | ÷ 9.80665 |
//! | anything else | unchanged | identity |
//!
//! [`qudt_unit`] names the served unit in the QUDT vocabulary, which the
//! OGC API - EDR Metocean Profile requires for parameter metadata.

/// `display = source * scale + offset`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayConversion {
    /// Unit string of the converted value (`"°C"`, `"hPa"`, `"mm"`, …).
    pub unit: &'static str,
    pub scale: f64,
    pub offset: f64,
}

impl DisplayConversion {
    #[inline]
    pub fn convert(&self, source: f64) -> f64 {
        source * self.scale + self.offset
    }

    /// True for a non-identity rule.
    pub fn has_conversion(&self) -> bool {
        (self.scale - 1.0).abs() > 1e-12 || self.offset.abs() > 1e-12
    }
}

/// The display conversion for a source unit as spelled by the format
/// (BUFR Table B / GRIB Code Table 4.2 spellings; `"°C"`/`"degC"` and
/// `"hPa"` are already display units and pass through). `None` when the
/// unit has no rule — the caller keeps the source unit and the raw value.
pub fn display_conversion(source_unit: &str) -> Option<DisplayConversion> {
    let c = match source_unit.trim() {
        "K" => DisplayConversion {
            unit: "°C",
            scale: 1.0,
            offset: -273.15,
        },
        "Pa" => DisplayConversion {
            unit: "hPa",
            scale: 0.01,
            offset: 0.0,
        },
        "kg m-2" | "kg m**-2" => DisplayConversion {
            unit: "mm",
            scale: 1.0,
            offset: 0.0,
        },
        "m2 s-2" | "m**2 s**-2" => DisplayConversion {
            unit: "gpm",
            scale: 1.0 / 9.80665,
            offset: 0.0,
        },
        _ => return None,
    };
    Some(c)
}

/// Namespace of QUDT unit identifiers, in the form the OGC API - EDR
/// Metocean Profile names for `unit.symbol.type` (Requirement 7E).
pub const QUDT_UNIT_BASE: &str = "https://qudt.org/vocab/unit/";

/// A unit in the QUDT vocabulary: its identifier under [`QUDT_UNIT_BASE`]
/// and its `qudt:symbol`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QudtUnit {
    /// Local name of the identifier, e.g. `"M-PER-SEC"`.
    pub id: &'static str,
    /// The unit's `qudt:symbol`, e.g. `"m/s"`.
    pub symbol: &'static str,
}

impl QudtUnit {
    /// The full identifier, e.g. `https://qudt.org/vocab/unit/M-PER-SEC`.
    pub fn uri(&self) -> String {
        format!("{QUDT_UNIT_BASE}{}", self.id)
    }
}

/// The QUDT unit for a unit string as the engines emit it: display units
/// and the UCUM, CF/udunits and WMO spellings of the same unit map to one
/// entry (identifiers and symbols as published in QUDT 3.5.2). `None` when
/// QUDT has no faithful entry; callers then keep the unit string as it is.
///
/// Deliberately absent: `dBZ` (QUDT's `DeciB_Z` is the acoustic Z-weighted
/// sound level, not radar reflectivity), `gpm`, `deg/km`, the CF
/// dimensionless `1` and BUFR code-table "units".
pub fn qudt_unit(unit: &str) -> Option<QudtUnit> {
    let (id, symbol) = match unit.trim() {
        "K" => ("K", "K"),
        "°C" | "degC" | "Cel" | "℃" => ("DEG_C", "°C"),
        "Pa" => ("PA", "Pa"),
        "hPa" => ("HectoPA", "hPa"),
        "Pa s-1" | "Pa/s" | "Pa.s-1" => ("PA-PER-SEC", "Pa/s"),
        "m s-1" | "m/s" | "m.s-1" | "m s**-1" => ("M-PER-SEC", "m/s"),
        "km/h" | "km h-1" | "km.h-1" => ("KiloM-PER-HR", "km/h"),
        "m" => ("M", "m"),
        "km" => ("KiloM", "km"),
        "cm" => ("CentiM", "cm"),
        "mm" => ("MilliM", "mm"),
        "mm/h" | "mm h-1" | "mm.h-1" => ("MilliM-PER-HR", "mm/h"),
        "%" => ("PERCENT", "%"),
        "dB" => ("DeciB", "dB"),
        "deg" | "degree" | "degrees" | "°" => ("DEG", "°"),
        "kg m-2" | "kg m**-2" | "kg.m-2" | "kg/m2" | "kg/m²" => ("KiloGM-PER-M2", "kg/m²"),
        "kg m-2 s-1" | "kg m**-2 s**-1" | "kg.m-2.s-1" | "kg/(m2 s)" => {
            ("KiloGM-PER-M2-SEC", "kg/(m²·s)")
        }
        "kg m-3" | "kg.m-3" | "kg/m3" | "kg/m³" => ("KiloGM-PER-M3", "kg/m³"),
        "kg kg-1" | "kg.kg-1" | "kg/kg" => ("KiloGM-PER-KiloGM", "kg/kg"),
        "J kg-1" | "J.kg-1" | "J/kg" => ("J-PER-KiloGM", "J/kg"),
        "J m-2" | "J.m-2" | "J/m2" | "J/m²" => ("J-PER-M2", "J/m²"),
        "W m-2" | "W.m-2" | "W/m2" | "W/m²" => ("W-PER-M2", "W/m²"),
        "m2 s-2" | "m**2 s**-2" | "m2.s-2" => ("M2-PER-SEC2", "m²/s²"),
        "m3 m-3" | "m3.m-3" | "m3/m3" | "m³/m³" => ("M3-PER-M3", "m³/m³"),
        "s-1" | "/s" | "1/s" => ("PER-SEC", "/s"),
        "s" => ("SEC", "s"),
        "min" => ("MIN", "min"),
        "h" => ("HR", "h"),
        "DU" => ("DU", "DU"),
        "kA" => ("KiloA", "kA"),
        _ => return None,
    };
    Some(QudtUnit { id, symbol })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_spellings_share_one_qudt_entry() {
        for (spellings, id, symbol) in [
            (
                &["m s-1", "m/s", "m.s-1", " m s-1 "][..],
                "M-PER-SEC",
                "m/s",
            ),
            (&["°C", "degC", "Cel"][..], "DEG_C", "°C"),
            (&["kg m-2", "kg/m²", "kg.m-2"][..], "KiloGM-PER-M2", "kg/m²"),
            (&["deg", "degree", "°"][..], "DEG", "°"),
        ] {
            for unit in spellings {
                assert_eq!(qudt_unit(unit), Some(QudtUnit { id, symbol }), "{unit}");
            }
        }
        assert_eq!(
            qudt_unit("hPa").unwrap().uri(),
            "https://qudt.org/vocab/unit/HectoPA"
        );
    }

    #[test]
    fn units_without_a_faithful_qudt_entry_are_not_mapped() {
        // `C` is the coulomb in UCUM; the rest have no QUDT entry that means
        // what the engines serve.
        for unit in ["dBZ", "gpm", "deg/km", "1", "C", "", "code table 0 20 003"] {
            assert_eq!(qudt_unit(unit), None, "{unit}");
        }
    }

    #[test]
    fn every_mechanical_display_unit_but_gpm_has_a_qudt_entry() {
        for source in ["K", "Pa", "kg m-2"] {
            let display = display_conversion(source).unwrap().unit;
            assert!(qudt_unit(display).is_some(), "{display}");
        }
    }

    #[test]
    fn mechanical_rules() {
        let k = display_conversion("K").unwrap();
        assert_eq!(k.unit, "°C");
        assert!((k.convert(290.12) - 16.97).abs() < 1e-9);
        let pa = display_conversion("Pa").unwrap();
        assert_eq!(pa.unit, "hPa");
        assert!((pa.convert(101_530.0) - 1015.3).abs() < 1e-9);
        let mm = display_conversion("kg m-2").unwrap();
        assert_eq!((mm.unit, mm.has_conversion()), ("mm", false));
        assert_eq!(display_conversion("m2 s-2").unwrap().unit, "gpm");
        // Everything else: no rule (the caller keeps the source unit).
        for u in [
            "m s-1",
            "%",
            "deg",
            "m",
            "°C",
            "hPa",
            "code table 0 20 003",
            "",
        ] {
            assert!(display_conversion(u).is_none(), "{u}");
        }
        assert!(k.has_conversion() && pa.has_conversion());
    }
}
