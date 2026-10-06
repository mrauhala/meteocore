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
//! [`is_ucum`] tells whether a unit QUDT does not know is UCUM, which OGC API
//! - EDR 1.2 then types as [`UCUM_TYPE`].

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

/// `unit.symbol.type` of a symbol in UCUM's case-sensitive syntax, as OGC API
/// - EDR 1.2 `/req/edr/rc-parameters` G spells it.
pub const UCUM_TYPE: &str = "https://www.opengis.net/def/uom/UCUM/";

/// UCUM prefixes (case-sensitive column), applicable to metric atoms only.
const UCUM_PREFIXES: &[&str] = &[
    "Y", "Z", "E", "P", "T", "G", "M", "k", "h", "da", "d", "c", "m", "u", "n", "p", "f", "a", "z",
    "y",
];

/// UCUM metric atoms (prefixes allowed): the SI units and the metric units a
/// meteorological source plausibly names.
const UCUM_METRIC_ATOMS: &[&str] = &[
    "m", "s", "g", "rad", "K", "C", "cd", "mol", "sr", "Hz", "N", "Pa", "J", "W", "A", "V", "F",
    "Ohm", "S", "Wb", "Cel", "T", "H", "lm", "lx", "Bq", "Gy", "Sv", "l", "L", "ar", "t", "bar",
    "u", "eV", "pc", "[g]", "[c]", "[ly]", "gf", "Gal", "dyn", "erg", "P", "St", "cal", "B",
    "B[SPL]", "B[V]", "B[mV]", "B[uV]", "B[W]", "B[kW]", "Np", "bit", "By", "Bd", "Ci", "R", "mho",
    "m[Hg]", "m[H2O]",
];

/// UCUM non-metric atoms (no prefix).
const UCUM_ATOMS: &[&str] = &[
    "%",
    "[pi]",
    "[ppth]",
    "[ppm]",
    "[ppb]",
    "[pptr]",
    "gon",
    "deg",
    "'",
    "''",
    "min",
    "h",
    "d",
    "a",
    "a_t",
    "a_j",
    "a_g",
    "wk",
    "mo",
    "mo_s",
    "mo_j",
    "mo_g",
    "atm",
    "[in_i]",
    "[ft_i]",
    "[yd_i]",
    "[mi_i]",
    "[nmi_i]",
    "[kn_i]",
    "[lb_av]",
    "[oz_av]",
    "[psi]",
    "[degF]",
    "[degR]",
    "[hp]",
    "[Btu]",
    "[in_i'Hg]",
    "[in_i'H2O]",
];

/// Whether `unit` is a well-formed UCUM expression (case-sensitive syntax)
/// over the atoms MeteoCore knows: `m/s`, `deg/km`, `kg.m-2`, `1`, `%`,
/// `mm[Hg]`, `/s`. CF/udunits spellings with spaces (`m s-1`), symbols UCUM
/// has no atom for (`dBZ`, `gpm`), anything non-ASCII (`°C`) and free text
/// (BUFR code tables) are not. An unknown atom answers `false`: an unlisted
/// genuine UCUM unit is then sent untyped, never the reverse.
pub fn is_ucum(unit: &str) -> bool {
    if unit.is_empty() || !unit.is_ascii() {
        return false;
    }
    let mut p = UcumParser {
        s: unit.as_bytes(),
        i: 0,
    };
    if p.peek() == Some(b'/') {
        p.i += 1;
    }
    p.term() && p.i == p.s.len()
}

/// Recursive descent over UCUM's grammar: `term := component (('.'|'/')
/// component)*`, `component := '(' term ')' | annotation | factor |
/// simple-unit exponent? annotation?`.
struct UcumParser<'a> {
    s: &'a [u8],
    i: usize,
}

impl UcumParser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn term(&mut self) -> bool {
        if !self.component() {
            return false;
        }
        while matches!(self.peek(), Some(b'.' | b'/')) {
            self.i += 1;
            if !self.component() {
                return false;
            }
        }
        true
    }

    fn digits(&mut self) -> bool {
        let start = self.i;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.i += 1;
        }
        self.i > start
    }

    /// At `{`: through the matching `}`, with no `{` inside.
    fn annotation(&mut self) -> bool {
        let rest = &self.s[self.i + 1..];
        match rest.iter().position(|&b| b == b'}') {
            Some(end) if !rest[..end].contains(&b'{') => {
                self.i += end + 2;
                true
            }
            _ => false,
        }
    }

    fn component(&mut self) -> bool {
        match self.peek() {
            Some(b'(') => {
                self.i += 1;
                if !self.term() || self.peek() != Some(b')') {
                    return false;
                }
                self.i += 1;
                true
            }
            Some(b'{') => self.annotation(),
            Some(b) if b.is_ascii_digit() => self.digits(),
            Some(_) => {
                let start = self.i;
                let mut depth = 0u32;
                while let Some(b) = self.peek() {
                    match b {
                        b'[' => depth += 1,
                        b']' if depth > 0 => depth -= 1,
                        _ if depth > 0 => {}
                        b'.' | b'/' | b'(' | b')' | b'{' | b'}' | b'+' | b'-' | b' ' => break,
                        _ if b.is_ascii_digit() => break,
                        _ => {}
                    }
                    self.i += 1;
                }
                if depth > 0 || !simple_unit(&self.s[start..self.i]) {
                    return false;
                }
                // Optional exponent: a sign needs digits after it.
                if matches!(self.peek(), Some(b'+' | b'-')) {
                    self.i += 1;
                    if !self.digits() {
                        return false;
                    }
                } else {
                    self.digits();
                }
                if self.peek() == Some(b'{') {
                    return self.annotation();
                }
                true
            }
            None => false,
        }
    }
}

/// An atom, or a prefix followed by a metric atom.
fn simple_unit(symbol: &[u8]) -> bool {
    let Ok(symbol) = std::str::from_utf8(symbol) else {
        return false;
    };
    if UCUM_ATOMS.contains(&symbol) || UCUM_METRIC_ATOMS.contains(&symbol) {
        return true;
    }
    UCUM_PREFIXES.iter().any(|prefix| {
        symbol
            .strip_prefix(prefix)
            .is_some_and(|atom| UCUM_METRIC_ATOMS.contains(&atom))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ucum_expressions_are_recognised() {
        for unit in [
            "m/s",
            "m.s-1",
            "deg/km",
            "deg",
            "1",
            "%",
            "K",
            "Cel",
            "hPa",
            "kg.m-2",
            "kg/(m2.s)",
            "/s",
            "s-1",
            "mm/h",
            "mm[Hg]",
            "[ppm]",
            "dB",
            "kg{dry}",
            "{count}",
            "km",
            "ms",
            "cd",
            "W.m-2.sr-1",
            "m2.s-2",
            "10.m",
            "m+2",
        ] {
            assert!(is_ucum(unit), "{unit} is UCUM");
        }
    }

    #[test]
    fn non_ucum_symbols_are_not_typed_as_ucum() {
        for unit in [
            "dBZ",
            "gpm",
            "code table 0 20 003",
            "m s-1",
            "kg m-2",
            "°C",
            "°",
            "",
            "m/",
            "(m",
            "m-",
            "kg{dry",
            "{a{b}",
            "[ppm",
            "dbz",
            "knots",
            "mW m-2 sr-1 (cm-1)-1",
            "degrees",
        ] {
            assert!(!is_ucum(unit), "{unit} is not UCUM");
        }
    }

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
