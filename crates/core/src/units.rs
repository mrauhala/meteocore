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

#[cfg(test)]
mod tests {
    use super::*;

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
