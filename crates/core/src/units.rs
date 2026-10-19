//! Unit spellings, mechanical display conversions and unit vocabularies,
//! shared by the engines, ds-render's style defaults and the API layers.
//!
//! ## Spelling
//!
//! One unit has many spellings: CF/UDUNITS writes kg m⁻² as `kg m-2`,
//! `kg m**-2` or `kg/m^2`, UCUM as `kg.m-2` or `kg/m2`. [`canonical_unit`]
//! reduces every spelling of a product of powers to one: the factors in the
//! order written, one space apart, each followed by its integer exponent
//! when that is not 1. Every unit-keyed match goes through it:
//! [`display_conversion`], [`qudt_unit`], ds-render's parameter-defaults
//! table and config `[[parameter_defaults]]` unit ranges, the u/v pairing in
//! `crate::wind` ([`same_unit`]), engine-netcdf's per-file units check and
//! the satellite recipes' brightness-temperature check in `crate::config`.
//! The canonical spelling is a matching key and is never served: an engine
//! serves a unit as its source spells it, or as a display conversion below
//! names it.
//!
//! | spellings | canonical |
//! |---|---|
//! | `kg m-2`, `kg m**-2`, `kg m^-2`, `kg.m-2`, `kg/m2`, `kg/m^2`, `kg/m²`, `kg·m⁻²` | `kg m-2` |
//! | `m s-1`, `m s**-1`, `m s^-1`, `m.s-1`, `m/s`, `m·s⁻¹` | `m s-1` |
//! | `kg m-2 s-1`, `kg m**-2 s**-1`, `kg/m2/s`, `kg/(m2 s)` | `kg m-2 s-1` |
//! | `m2 s-2`, `m**2 s**-2`, `m^2/s^2`, `m²/s²` | `m2 s-2` |
//! | `s-1`, `/s`, `1/s` | `s-1` |
//! | `K`, `kelvin`, `degK`, `degree_K` | `K` |
//! | `°C`, `degC`, `deg_C`, `degree_C`, `degree_Celsius`, `degrees_Celsius`, `celsius`, `Cel`, `℃` | `°C` |
//! | `%`, `percent` | `%` |
//! | `Pa`, `1`, `dBZ` and every other lone symbol | unchanged |
//!
//! What is not a product of symbols and integer powers stays as written,
//! trimmed: a scale factor (`0.01`, `10 m`), free text (`code table 0 20 003`,
//! GRIB's `(0 - 1)`), an exponent on a group, and `a/b c`, which readers
//! split differently. A repeated symbol is not combined: `kg/kg` is
//! `kg kg-1`, never `1`. Symbol names are renamed only where the spelling
//! is unambiguous: `C` stays (the coulomb), `deg` stays (the angle).
//! Groups nest at most [`MAX_GROUP_DEPTH`] deep; a deeper spelling stays as
//! written, so a hostile attribute cannot recurse the parser off the stack.
//!
//! ## Display conversions
//!
//! Source units are whatever the format encodes (BUFR Table B, WMO GRIB
//! Code Table 4.2, a CF `units` attribute); clients want the everyday
//! forms. engine-bufr and engine-netcdf convert through
//! [`display_conversion`]. The rules are keyed on the **source unit**,
//! through [`canonical_unit`], never on a parameter name (see the
//! unit-conversion rule in root `CLAUDE.md`), and mirror
//! `crates/engine-grib/src/units.rs::default_display` — GRIB keeps its
//! enum-keyed twin for now because its source units come from code-table
//! lookups. Both tables agree rule for rule on every unit a spelling names:
//! engine-grib's tests map each `SourceUnit` through its WMO spelling, and
//! each spelling `parse_source_unit` reads, to this table. Two GRIB rules
//! have no twin here because no unit string tells them apart: a temperature
//! difference in `K` (dewpoint depression, no offset) and ECMWF's `m` of
//! water equivalent (× 1000 to `mm`). GRIB's code tables assign them from
//! the parameter.
//!
//! | source (canonical) | display | rule |
//! |---|---|---|
//! | `K` | `°C` | − 273.15 |
//! | `Pa` | `hPa` | × 0.01 |
//! | `kg m-2` | `mm` | identity (1 kg m⁻² of water ≈ 1 mm) |
//! | `m2 s-2` | `gpm` | ÷ 9.80665 |
//! | `Proportion`, `proportion`, `(0 - 1)` (WMO's fraction) | `%` | × 100 |
//! | anything else | unchanged | identity |
//!
//! CF's dimensionless `1` has no rule: it names many quantities besides a
//! fraction.
//!
//! ## Vocabularies
//!
//! [`qudt_unit`] names the served unit in the QUDT vocabulary, which the
//! OGC API - EDR Metocean Profile requires for parameter metadata.
//! [`is_ucum`] tells whether a unit QUDT does not know is UCUM, which OGC API
//! - EDR 1.2 then types as [`UCUM_TYPE`].

use std::borrow::Cow;

/// Spellings of one unit symbol, each mapped to its canonical symbol. Whole
/// symbols only, and only spellings that cannot mean anything else.
const SYMBOL_SPELLINGS: &[(&str, &[&str])] = &[
    (
        "K",
        &[
            "kelvin",
            "Kelvin",
            "degK",
            "deg_K",
            "degreeK",
            "degree_K",
            "degreesK",
            "degrees_K",
            "degree_Kelvin",
            "degrees_Kelvin",
        ],
    ),
    (
        "°C",
        &[
            "degC",
            "deg_C",
            "degreeC",
            "degree_C",
            "degreesC",
            "degrees_C",
            "degree_Celsius",
            "degrees_Celsius",
            "degree_celsius",
            "degrees_celsius",
            "celsius",
            "Celsius",
            "Cel",
            "℃",
        ],
    ),
    ("%", &["percent", "Percent"]),
];

/// How deep parenthesised groups may nest in a unit spelling, here and in
/// [`is_ucum`]. Real units nest once at most (`kg/(m2 s)`,
/// `mW m-2 sr-1 (cm-1)-1`); the bound keeps the recursive parsers' stack
/// depth constant for untrusted metadata.
pub const MAX_GROUP_DEPTH: u32 = 4;

/// The canonical spelling of `unit` (see the module docs): `kg/m^2`,
/// `kg m**-2` and `kg.m-2` are all `kg m-2`; `m/s` is `m s-1`; `degC` is
/// `°C`. A unit that is not a product of symbols and integer powers comes
/// back as written, trimmed. Borrows when nothing changes.
pub fn canonical_unit(unit: &str) -> Cow<'_, str> {
    let unit = unit.trim();
    let Some(factors) = factors(unit) else {
        return Cow::Borrowed(unit);
    };
    let mut out = String::with_capacity(unit.len());
    for (symbol, exponent) in factors {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(symbol);
        if exponent != 1 {
            out.push_str(&exponent.to_string());
        }
    }
    if out == unit {
        Cow::Borrowed(unit)
    } else {
        Cow::Owned(out)
    }
}

/// Whether two unit strings spell the same unit ([`canonical_unit`]).
pub fn same_unit(a: &str, b: &str) -> bool {
    canonical_unit(a) == canonical_unit(b)
}

/// `unit` as (symbol, exponent) factors in written order, a divisor's
/// exponent negated. `None` when it is not a product of symbols and
/// integer powers, or is `1` or empty, which stay as they are.
fn factors(unit: &str) -> Option<Vec<(&str, i32)>> {
    if unit.is_empty() || unit == "1" {
        return None;
    }
    let mut p = Spelling {
        s: unit,
        i: 0,
        depth: 0,
    };
    let mut out = Vec::new();
    let sign = if p.eat('/') { -1 } else { 1 };
    p.skip_ws();
    p.operand(sign, &mut out)?;
    // After a division, a further product (`a/b c`, `a/b.c`) is read
    // differently by different conventions: not guessed.
    let mut divided = sign < 0;
    loop {
        let spaced = p.skip_ws();
        match p.peek() {
            None => break,
            Some('/') => {
                p.bump();
                p.skip_ws();
                p.operand(-1, &mut out)?;
                divided = true;
            }
            Some(c) if is_product(c) && !p.rest().starts_with("**") => {
                p.bump();
                p.skip_ws();
                if divided {
                    return None;
                }
                p.operand(1, &mut out)?;
            }
            Some(_) if spaced && !divided => p.operand(1, &mut out)?,
            Some(_) => return None,
        }
    }
    (!out.is_empty()).then_some(out)
}

/// An explicit multiplication sign: UCUM's `.`, `*`, `·`, `⋅`, `×`.
fn is_product(c: char) -> bool {
    matches!(c, '.' | '*' | '·' | '⋅' | '×')
}

/// The value of a superscript digit.
fn superscript_digit(c: char) -> Option<i32> {
    Some(match c {
        '⁰' => 0,
        '¹' => 1,
        '²' => 2,
        '³' => 3,
        '⁴' => 4,
        '⁵' => 5,
        '⁶' => 6,
        '⁷' => 7,
        '⁸' => 8,
        '⁹' => 9,
        _ => return None,
    })
}

/// A character that can be part of a unit symbol: anything but whitespace,
/// digits, signs, grouping, operators and superscripts.
fn is_symbol_char(c: char) -> bool {
    !(c.is_whitespace()
        || c.is_ascii_digit()
        || is_product(c)
        || matches!(c, '/' | '^' | '(' | ')' | '+' | '-' | '⁻' | '⁺')
        || superscript_digit(c).is_some())
}

/// The canonical name of one symbol.
fn canonical_symbol(symbol: &str) -> &str {
    SYMBOL_SPELLINGS
        .iter()
        .find(|(_, spellings)| spellings.contains(&symbol))
        .map_or(symbol, |(canonical, _)| canonical)
}

/// Recursive descent over a unit spelling: `product := operand ((sep |
/// '/') operand)*`, `operand := '(' product ')' | '1' | symbol exponent?`.
struct Spelling<'a> {
    s: &'a str,
    /// Byte offset into `s`.
    i: usize,
    /// Groups open at `i`, at most [`MAX_GROUP_DEPTH`].
    depth: u32,
}

impl<'a> Spelling<'a> {
    fn rest(&self) -> &'a str {
        &self.s[self.i..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self) {
        if let Some(c) = self.peek() {
            self.i += c.len_utf8();
        }
    }

    fn eat(&mut self, c: char) -> bool {
        let hit = self.peek() == Some(c);
        if hit {
            self.bump();
        }
        hit
    }

    /// Skip whitespace; whether there was any.
    fn skip_ws(&mut self) -> bool {
        let start = self.i;
        while self.peek().is_some_and(char::is_whitespace) {
            self.bump();
        }
        self.i > start
    }

    fn digits(&mut self) -> Option<i32> {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.bump();
        }
        self.s[start..self.i].parse().ok()
    }

    /// An ASCII integer with an optional sign.
    fn signed(&mut self) -> Option<i32> {
        let negative = match self.peek() {
            Some('-') => true,
            Some('+') => false,
            _ => return self.digits(),
        };
        self.bump();
        let n = self.digits()?;
        Some(if negative { -n } else { n })
    }

    /// Whether an exponent starts here.
    fn starts_exponent(&self) -> bool {
        self.rest().starts_with("**")
            || self.peek().is_some_and(|c| {
                c.is_ascii_digit()
                    || matches!(c, '^' | '-' | '+' | '⁻' | '⁺')
                    || superscript_digit(c).is_some()
            })
    }

    /// The exponent after a symbol: `**n`, `^n`, `n`, `-n`, `+n` or
    /// superscripts; 1 when there is none. `None` when malformed or zero.
    fn exponent(&mut self) -> Option<i32> {
        let exponent = if self.rest().starts_with("**") {
            self.i += 2;
            self.signed()?
        } else if self.eat('^') {
            self.signed()?
        } else {
            match self.peek() {
                Some(c) if c.is_ascii_digit() || c == '-' || c == '+' => self.signed()?,
                Some(c) if c == '⁻' || c == '⁺' || superscript_digit(c).is_some() => {
                    let negative = self.eat('⁻');
                    if !negative {
                        self.eat('⁺');
                    }
                    let mut n: i32 = 0;
                    let mut any = false;
                    while let Some(d) = self.peek().and_then(superscript_digit) {
                        self.bump();
                        n = n.checked_mul(10)?.checked_add(d)?;
                        any = true;
                    }
                    if !any {
                        return None;
                    }
                    if negative {
                        -n
                    } else {
                        n
                    }
                }
                _ => 1,
            }
        };
        (exponent != 0).then_some(exponent)
    }

    /// One factor or parenthesised product, its exponents times `sign`.
    fn operand(&mut self, sign: i32, out: &mut Vec<(&'a str, i32)>) -> Option<()> {
        if self.eat('(') {
            if self.depth == MAX_GROUP_DEPTH {
                return None;
            }
            self.depth += 1;
            let start = out.len();
            self.skip_ws();
            self.operand(1, out)?;
            loop {
                let spaced = self.skip_ws();
                match self.peek() {
                    Some(')') => break,
                    Some(c) if is_product(c) && !self.rest().starts_with("**") => {
                        self.bump();
                        self.skip_ws();
                        self.operand(1, out)?;
                    }
                    Some(_) if spaced => self.operand(1, out)?,
                    _ => return None,
                }
            }
            self.bump();
            self.depth -= 1;
            // An exponent on a group is not read.
            if self.starts_exponent() {
                return None;
            }
            for factor in &mut out[start..] {
                factor.1 *= sign;
            }
            return Some(());
        }
        if self.peek().is_some_and(|c| c.is_ascii_digit()) {
            // A numeric factor: only the unit `1` (`1/s`), never a scale.
            let start = self.i;
            self.digits()?;
            let one = &self.s[start..self.i] == "1";
            return (one && !self.starts_exponent() && !self.peek().is_some_and(is_symbol_char))
                .then_some(());
        }
        let start = self.i;
        while self.peek().is_some_and(is_symbol_char) {
            self.bump();
        }
        if self.i == start {
            return None;
        }
        let symbol = canonical_symbol(&self.s[start..self.i]);
        let exponent = self.exponent()?.checked_mul(sign)?;
        out.push((symbol, exponent));
        Some(())
    }
}

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

/// The display conversion for a source unit in any spelling
/// ([`canonical_unit`]: `kg/m^2` is `kg m-2`, `kelvin` is `K`; `°C`/`degC`
/// and `hPa` are already display units and pass through). `None` when the
/// unit has no rule — the caller keeps the source unit and the raw value.
/// The rules are the module docs' table, engine-grib's `default_display`
/// twin.
pub fn display_conversion(source_unit: &str) -> Option<DisplayConversion> {
    let c = match canonical_unit(source_unit).as_ref() {
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
        "kg m-2" => DisplayConversion {
            unit: "mm",
            scale: 1.0,
            offset: 0.0,
        },
        "m2 s-2" => DisplayConversion {
            unit: "gpm",
            scale: 1.0 / 9.80665,
            offset: 0.0,
        },
        // WMO Code Table 4.2's words for a fraction in [0, 1].
        "Proportion" | "proportion" | "(0 - 1)" => DisplayConversion {
            unit: "%",
            scale: 100.0,
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
/// and every spelling of the same unit ([`canonical_unit`]: UCUM `m/s`, CF
/// `m s-1`, `m s**-1`, …) map to one entry (identifiers and symbols as
/// published in QUDT 3.5.2). `None` when QUDT has no faithful entry; callers
/// then keep the unit string as it is.
///
/// Deliberately absent: `dBZ` (QUDT's `DeciB_Z` is the acoustic Z-weighted
/// sound level, not radar reflectivity), `gpm`, `deg/km`, the CF
/// dimensionless `1` and BUFR code-table "units".
pub fn qudt_unit(unit: &str) -> Option<QudtUnit> {
    let (id, symbol) = match canonical_unit(unit).as_ref() {
        "K" => ("K", "K"),
        "°C" => ("DEG_C", "°C"),
        "Pa" => ("PA", "Pa"),
        "hPa" => ("HectoPA", "hPa"),
        "Pa s-1" => ("PA-PER-SEC", "Pa/s"),
        "m s-1" => ("M-PER-SEC", "m/s"),
        "km h-1" => ("KiloM-PER-HR", "km/h"),
        "m" => ("M", "m"),
        "km" => ("KiloM", "km"),
        "cm" => ("CentiM", "cm"),
        "mm" => ("MilliM", "mm"),
        "mm h-1" => ("MilliM-PER-HR", "mm/h"),
        "%" => ("PERCENT", "%"),
        "dB" => ("DeciB", "dB"),
        "deg" | "degree" | "degrees" | "°" => ("DEG", "°"),
        "kg m-2" => ("KiloGM-PER-M2", "kg/m²"),
        "kg m-2 s-1" => ("KiloGM-PER-M2-SEC", "kg/(m²·s)"),
        "kg m-3" => ("KiloGM-PER-M3", "kg/m³"),
        "kg kg-1" => ("KiloGM-PER-KiloGM", "kg/kg"),
        "J kg-1" => ("J-PER-KiloGM", "J/kg"),
        "J m-2" => ("J-PER-M2", "J/m²"),
        "W m-2" => ("W-PER-M2", "W/m²"),
        "m2 s-2" => ("M2-PER-SEC2", "m²/s²"),
        "m3 m-3" => ("M3-PER-M3", "m³/m³"),
        "s-1" => ("PER-SEC", "/s"),
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
/// genuine UCUM unit is then sent untyped, never the reverse. Groups nesting
/// deeper than [`MAX_GROUP_DEPTH`] answer `false`.
pub fn is_ucum(unit: &str) -> bool {
    if unit.is_empty() || !unit.is_ascii() {
        return false;
    }
    let mut p = UcumParser {
        s: unit.as_bytes(),
        i: 0,
        depth: 0,
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
    /// Groups open at `i`, at most [`MAX_GROUP_DEPTH`].
    depth: u32,
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
                if self.depth == MAX_GROUP_DEPTH {
                    return false;
                }
                self.depth += 1;
                self.i += 1;
                if !self.term() || self.peek() != Some(b')') {
                    return false;
                }
                self.i += 1;
                self.depth -= 1;
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

    /// The normalisation table (module docs): every spelling of a unit,
    /// CF/UDUNITS, UCUM, WMO or ASCII-art, reaches one canonical spelling.
    #[test]
    fn spellings_share_one_canonical_form() {
        for (canonical, spellings) in [
            (
                "kg m-2",
                &[
                    "kg m-2",
                    "kg m**-2",
                    "kg m^-2",
                    "kg.m-2",
                    "kg*m**-2",
                    "kg/m2",
                    "kg/m^2",
                    "kg / m^2",
                    "kg/m²",
                    "kg·m⁻²",
                    " kg  m-2 ",
                ][..],
            ),
            (
                "m s-1",
                &[
                    "m s-1",
                    "m s**-1",
                    "m s^-1",
                    "m.s-1",
                    "m/s",
                    "m·s⁻¹",
                    "m s⁻¹",
                ][..],
            ),
            (
                "kg m-2 s-1",
                &[
                    "kg m-2 s-1",
                    "kg m**-2 s**-1",
                    "kg.m-2.s-1",
                    "kg/m2/s",
                    "kg/(m2 s)",
                    "kg/(m^2*s)",
                    "kg m-2/s",
                    "kg m⁻² s⁻¹",
                ][..],
            ),
            (
                "m2 s-2",
                &[
                    "m2 s-2",
                    "m**2 s**-2",
                    "m^2/s^2",
                    "m2/s2",
                    "m²/s²",
                    "m+2.s-2",
                ][..],
            ),
            ("s-1", &["s-1", "/s", "1/s", "s^-1", "s**-1"][..]),
            ("Pa s-1", &["Pa s-1", "Pa/s", "Pa.s-1"][..]),
            ("kg kg-1", &["kg kg-1", "kg/kg", "kg kg**-1"][..]),
            ("J kg-1", &["J kg-1", "J/kg", "J kg^-1"][..]),
            ("W m-2", &["W m-2", "W/m2", "W/m²", "W m**-2"][..]),
            ("mm h-1", &["mm h-1", "mm/h", "mm.h-1"][..]),
            ("m3 m-3", &["m3 m-3", "m3/m3", "m³/m³", "m**3 m**-3"][..]),
            ("K", &["K", "kelvin", "Kelvin", "degK", "degree_K"][..]),
            (
                "°C",
                &[
                    "°C",
                    "degC",
                    "deg_C",
                    "degree_C",
                    "degree_Celsius",
                    "degrees_Celsius",
                    "celsius",
                    "Cel",
                    "℃",
                ][..],
            ),
            ("%", &["%", "percent", " % "][..]),
            ("K km-1", &["K/km", "kelvin/km"][..]),
        ] {
            for spelling in spellings {
                assert_eq!(canonical_unit(spelling), canonical, "{spelling:?}");
            }
            assert!(
                spellings.windows(2).all(|w| same_unit(w[0], w[1])),
                "{canonical}"
            );
        }
    }

    /// What is not a product of symbols and integer powers is kept as
    /// written (trimmed): scale factors, free text, group exponents and the
    /// ambiguous `a/b c`. Lone symbols are unchanged, and `C` (the coulomb)
    /// and `deg` (the angle) are not renamed.
    #[test]
    fn units_that_are_not_products_stay_as_written() {
        for unit in [
            "",
            "1",
            "Pa",
            "hPa",
            "dBZ",
            "gpm",
            "C",
            "deg",
            "°",
            "Proportion",
            "Numeric",
            "degrees_north",
            "m of water equivalent",
            "(0 - 1)",
            "0.01",
            "10 m",
            "1e-3",
            "code table 0 20 003",
            "mW m-2 sr-1 (cm-1)-1",
            "kg/m2 s",
            "W/m2.sr",
            "N-m",
            "m s -1",
            "m^",
            "m⁻",
            "m0",
            "H2O",
            "(m",
            "m)",
        ] {
            assert_eq!(canonical_unit(unit), unit.trim(), "{unit:?}");
        }
        assert_eq!(canonical_unit("  dBZ "), "dBZ");
        // `kg/kg` is a mass fraction spelled as one: never reduced to `1`.
        assert_eq!(canonical_unit("kg/kg"), "kg kg-1");
        assert!(!same_unit("kg/kg", "1"));
        assert!(!same_unit("m s-1", "m s-2"));
    }

    /// A canonical spelling comes back borrowed from the input, and so does
    /// a unit that is not a product. The parse itself still allocates for a
    /// product.
    #[test]
    fn canonical_spellings_are_borrowed() {
        for unit in ["kg m-2", "m s-1", "K", "1", "%", "dBZ", "(0 - 1)", " K "] {
            assert!(matches!(canonical_unit(unit), Cow::Borrowed(_)), "{unit:?}");
        }
        assert!(matches!(canonical_unit("m/s"), Cow::Owned(_)));
    }

    /// Groups nest at most `MAX_GROUP_DEPTH` deep in both parsers. A unit
    /// attribute from remote metadata with a hundred thousand `(` stays as
    /// written instead of overflowing the stack, which would abort the
    /// process.
    #[test]
    fn deep_groups_stay_as_written() {
        let open = "(".repeat(100_000);
        let hostile = format!("{open}m{}", ")".repeat(100_000));
        for unit in [open.as_str(), hostile.as_str()] {
            assert_eq!(canonical_unit(unit), unit);
            assert!(!is_ucum(unit));
            assert_eq!(qudt_unit(unit), None);
            assert_eq!(display_conversion(unit), None);
        }
        // Up to the bound, groups still read.
        assert_eq!(canonical_unit("kg/((((m2 s))))"), "kg m-2 s-1");
        assert_eq!(canonical_unit("kg/(((((m2 s)))))"), "kg/(((((m2 s)))))");
        assert!(is_ucum("kg/((((m2.s))))"));
        assert!(!is_ucum("kg/(((((m2.s)))))"));
    }

    /// `display_conversion` reads every spelling of its source units: a
    /// UDUNITS `kg/m^2` is millimetres and `m^2/s^2` geopotential metres,
    /// as `kg m-2` and `m2 s-2` always were.
    #[test]
    fn display_conversion_reads_every_spelling() {
        for (spellings, unit) in [
            (&["K", "kelvin", "degK"][..], "°C"),
            (&["Pa", " Pa "][..], "hPa"),
            (
                &["kg m-2", "kg m**-2", "kg/m^2", "kg/m2", "kg.m-2", "kg/m²"][..],
                "mm",
            ),
            (&["m2 s-2", "m**2 s**-2", "m^2/s^2", "m2/s2"][..], "gpm"),
            (
                &["Proportion", "proportion", "(0 - 1)", " (0 - 1) "][..],
                "%",
            ),
        ] {
            let rule = display_conversion(spellings[0]).unwrap();
            assert_eq!(rule.unit, unit);
            for spelling in spellings {
                assert_eq!(display_conversion(spelling), Some(rule), "{spelling:?}");
            }
        }
        // Display units and unruled units pass through, in any spelling.
        for unit in [
            "degC",
            "degree_Celsius",
            "hPa",
            "m/s",
            "percent",
            "kg m-2 s-1",
        ] {
            assert_eq!(display_conversion(unit), None, "{unit:?}");
        }
    }

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
                &["m s-1", "m/s", "m.s-1", " m s-1 ", "m s**-1", "m s^-1"][..],
                "M-PER-SEC",
                "m/s",
            ),
            (
                &["°C", "degC", "Cel", "degree_Celsius", "℃"][..],
                "DEG_C",
                "°C",
            ),
            (
                &["kg m-2", "kg/m²", "kg.m-2", "kg/m^2", "kg m^-2"][..],
                "KiloGM-PER-M2",
                "kg/m²",
            ),
            (&["deg", "degree", "°"][..], "DEG", "°"),
            (&["K", "kelvin"][..], "K", "K"),
            (&["%", "percent"][..], "PERCENT", "%"),
            (&["s-1", "/s", "1/s", "s^-1"][..], "PER-SEC", "/s"),
            (
                &["kg m-2 s-1", "kg/(m2 s)", "kg/m2/s"][..],
                "KiloGM-PER-M2-SEC",
                "kg/(m²·s)",
            ),
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
        for source in ["K", "Pa", "kg m-2", "Proportion"] {
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
        let proportion = display_conversion("(0 - 1)").unwrap();
        assert_eq!(proportion.unit, "%");
        assert!((proportion.convert(0.75) - 75.0).abs() < 1e-12);
        // Everything else: no rule (the caller keeps the source unit). CF's
        // `1` is not a fraction by definition.
        for u in [
            "m s-1",
            "%",
            "1",
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
