//! WMO GRIB2 parameter → unit resolver.
//!
//! Identifies parameters by the (center, discipline, category, number) triple
//! encoded in every GRIB2 message, not by producer-specific short names. Source
//! units come from WMO Code Table 4.2; display conversions are mechanical by
//! source-unit class.
//!
//! The standard WMO table covers parameter numbers 0-191. Numbers 192-254 are
//! reserved for originating-center local extensions; those are looked up in a
//! per-center overlay first. ECMWF (center 98) is the only local overlay
//! currently populated.
//!
//! Nothing imports this module yet — the `#[allow(dead_code)]` attributes keep
//! the isolated build clean until the engine is wired up.

#![allow(dead_code)]

/// A canonical source unit as encoded in the GRIB2 message.
///
/// The variant names reflect the WMO unit string that appears in Code Table
/// 4.2. Anything we do not explicitly recognise lands in `Raw`, which carries
/// the original string for display without automatic conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceUnit {
    /// "K"
    Kelvin,
    /// "C" (rare in WMO tables, but included for completeness)
    Celsius,
    /// "Pa"
    Pascal,
    /// "hPa"
    Hectopascal,
    /// "kg m-2" — precipitation depth; 1 kg m-2 ≈ 1 mm of water.
    KgPerM2,
    /// "kg m-2 s-1" — precipitation rate.
    KgPerM2PerS,
    /// "m s-1"
    MetresPerSecond,
    /// "m"
    Metres,
    /// "m" of water equivalent — ECMWF convention for precipitation depth,
    /// snow depth, snowfall. Auto-displayed in mm (× 1000).
    MetresOfWater,
    /// "mm"
    Millimetres,
    /// "m2 s-2" — geopotential.
    M2PerS2,
    /// "gpm" — geopotential metres.
    Gpm,
    /// "Proportion" or "(0 - 1)" — unitless fraction in [0, 1].
    Proportion,
    /// "%"
    Percent,
    /// "J kg-1" — specific energy (CAPE, CIN).
    JoulesPerKg,
    /// "J m-2" — areal energy density.
    JoulesPerM2,
    /// "W m-2" — areal power density (radiation flux).
    WattsPerM2,
    /// "kg kg-1" — mixing ratio.
    KgPerKg,
    /// "s-1" — inverse seconds (vorticity, divergence).
    InversePerSec,
    /// "Numeric" or empty — dimensionless scalar.
    Dimensionless,
    /// Anything else — carry the original unit string for display but skip
    /// automatic conversion.
    Raw(&'static str),
}

/// Parse a WMO unit string (from Code Table 4.2 `UnitComments_en`) into a
/// [`SourceUnit`]. Whitespace is trimmed but no other normalisation is
/// performed. Anything unrecognised becomes [`SourceUnit::Raw`] carrying the
/// input verbatim — but because we only call this with `&'static str` inputs
/// from the table below, the `Raw` variant can hold a static string safely.
///
/// Returns [`SourceUnit::Raw`] for unknown inputs.
pub fn parse_source_unit(s: &'static str) -> SourceUnit {
    match s.trim() {
        "K" => SourceUnit::Kelvin,
        "C" | "°C" => SourceUnit::Celsius,
        "Pa" => SourceUnit::Pascal,
        "hPa" => SourceUnit::Hectopascal,
        "kg m-2" | "kg/m2" | "kg/m²" => SourceUnit::KgPerM2,
        "kg m-2 s-1" | "kg/(m2 s)" | "kg m⁻² s⁻¹" => SourceUnit::KgPerM2PerS,
        "m s-1" | "m/s" => SourceUnit::MetresPerSecond,
        "m" => SourceUnit::Metres,
        "mm" => SourceUnit::Millimetres,
        "m2 s-2" | "m² s⁻²" => SourceUnit::M2PerS2,
        "gpm" => SourceUnit::Gpm,
        "Proportion" | "(0 - 1)" | "proportion" => SourceUnit::Proportion,
        "%" => SourceUnit::Percent,
        "J kg-1" | "J/kg" => SourceUnit::JoulesPerKg,
        "J m-2" | "J/m2" => SourceUnit::JoulesPerM2,
        "W m-2" | "W/m2" => SourceUnit::WattsPerM2,
        "kg kg-1" | "kg/kg" => SourceUnit::KgPerKg,
        "s-1" | "/s" | "1/s" => SourceUnit::InversePerSec,
        "Numeric" | "" => SourceUnit::Dimensionless,
        other => SourceUnit::Raw(other),
    }
}

/// Description of a parameter after resolution via [`lookup`].
#[derive(Debug, Clone)]
pub struct ParamInfo {
    /// Human-readable label from the WMO table (or local overlay).
    pub label: &'static str,
    /// Source unit as encoded in the GRIB2 message.
    pub source_unit: SourceUnit,
}

/// Look up a parameter by its WMO triple (plus originating center for local
/// extensions in the 192-254 range).
///
/// Lookup order:
/// 1. If `number >= 192`, try the per-center local overlay first.
/// 2. Otherwise (or on miss), fall back to the standard WMO table.
///
/// Returns `None` if the triple is not in the curated subset.
pub fn lookup(center: u16, discipline: u8, category: u8, number: u8) -> Option<ParamInfo> {
    if number >= 192 {
        if let Some(info) = local_lookup(center, discipline, category, number) {
            return Some(info);
        }
    }
    standard_lookup(discipline, category, number)
}

/// Standard WMO Code Table 4.2 entries (discipline, category, number).
///
/// Curated subset — not exhaustive. Labels and units come verbatim from the
/// WMO CSV tables at <https://github.com/wmo-im/GRIB2>. Source units are
/// pre-classified into [`SourceUnit`] so call sites do not have to re-parse.
fn standard_lookup(discipline: u8, category: u8, number: u8) -> Option<ParamInfo> {
    let (label, source_unit) = match (discipline, category, number) {
        // ---- Discipline 0: Meteorological products ----

        // Category 0: temperature
        (0, 0, 0) => ("Temperature", SourceUnit::Kelvin),
        (0, 0, 2) => ("Potential temperature", SourceUnit::Kelvin),
        (0, 0, 4) => ("Maximum temperature", SourceUnit::Kelvin),
        (0, 0, 5) => ("Minimum temperature", SourceUnit::Kelvin),
        (0, 0, 6) => ("Dewpoint temperature", SourceUnit::Kelvin),
        (0, 0, 7) => ("Dewpoint depression (or deficit)", SourceUnit::Kelvin),
        (0, 0, 17) => ("Skin temperature", SourceUnit::Kelvin),

        // Category 1: moisture
        (0, 1, 0) => ("Specific humidity", SourceUnit::KgPerKg),
        (0, 1, 1) => ("Relative humidity", SourceUnit::Percent),
        (0, 1, 7) => ("Precipitation rate", SourceUnit::KgPerM2PerS),
        // Total precipitation is an accumulation; the engine drops aggregates
        // in v1, but shipping the metadata costs nothing.
        (0, 1, 8) => ("Total precipitation", SourceUnit::KgPerM2),
        (0, 1, 11) => ("Snow depth", SourceUnit::Metres),
        (0, 1, 13) => (
            "Water equivalent of accumulated snow depth",
            SourceUnit::KgPerM2,
        ),
        (0, 1, 52) => ("Total precipitation rate", SourceUnit::KgPerM2PerS),
        (0, 1, 64) => ("Total column integrated water vapour", SourceUnit::KgPerM2),

        // Category 2: momentum
        (0, 2, 2) => ("u-component of wind", SourceUnit::MetresPerSecond),
        (0, 2, 3) => ("v-component of wind", SourceUnit::MetresPerSecond),
        (0, 2, 8) => ("Vertical velocity (pressure)", SourceUnit::Raw("Pa s-1")),
        (0, 2, 9) => ("Vertical velocity (geometric)", SourceUnit::MetresPerSecond),
        (0, 2, 10) => ("Absolute vorticity", SourceUnit::InversePerSec),
        (0, 2, 12) => ("Relative vorticity", SourceUnit::InversePerSec),
        (0, 2, 13) => ("Relative divergence", SourceUnit::InversePerSec),
        // WMO calls this "Wind speed (gust)" — there is no separate bare
        // "Wind speed" entry at parameter 22 in category 2.
        (0, 2, 22) => ("Wind speed (gust)", SourceUnit::MetresPerSecond),
        // NOTE: parameters 27/28 in WMO Code Table 4.2 are u/v storm motion,
        // not u/v component of gust. Shipping them under the WMO label; the
        // engine can still render them, just with the correct description.
        (0, 2, 27) => ("u-component storm motion", SourceUnit::MetresPerSecond),
        (0, 2, 28) => ("v-component storm motion", SourceUnit::MetresPerSecond),

        // Category 3: mass
        (0, 3, 0) => ("Pressure", SourceUnit::Pascal),
        (0, 3, 1) => ("Pressure reduced to MSL", SourceUnit::Pascal),
        (0, 3, 3) => (
            "ICAO Standard Atmosphere Reference Height",
            SourceUnit::Metres,
        ),
        (0, 3, 4) => ("Geopotential", SourceUnit::M2PerS2),
        (0, 3, 5) => ("Geopotential height", SourceUnit::Gpm),
        // WMO has "Geopotential height anomaly" at parameter 9 in category 3;
        // there is no "Geometric height" in that slot. Shipping the WMO entry.
        (0, 3, 9) => ("Geopotential height anomaly", SourceUnit::Gpm),

        // Category 4: short-wave radiation
        (0, 4, 7) => ("Downward short-wave radiation flux", SourceUnit::WattsPerM2),
        (0, 4, 8) => ("Upward short-wave radiation flux", SourceUnit::WattsPerM2),

        // Category 5: long-wave radiation
        (0, 5, 3) => ("Downward long-wave radiation flux", SourceUnit::WattsPerM2),
        (0, 5, 4) => ("Upward long-wave radiation flux", SourceUnit::WattsPerM2),

        // Category 6: cloud
        (0, 6, 1) => ("Total cloud cover", SourceUnit::Percent),
        (0, 6, 3) => ("Low cloud cover", SourceUnit::Percent),
        (0, 6, 4) => ("Medium cloud cover", SourceUnit::Percent),
        (0, 6, 5) => ("High cloud cover", SourceUnit::Percent),
        (0, 6, 6) => ("Cloud water", SourceUnit::KgPerM2),

        // Category 7: thermodynamic stability indices
        (0, 7, 6) => (
            "Convective available potential energy",
            SourceUnit::JoulesPerKg,
        ),
        (0, 7, 7) => ("Convective inhibition", SourceUnit::JoulesPerKg),

        // Category 14: trace gases
        (0, 14, 0) => ("Total ozone", SourceUnit::Raw("DU")),

        // Category 19: physical atmospheric properties
        (0, 19, 0) => ("Visibility", SourceUnit::Metres),

        // ---- Discipline 2: Land surface products ----

        // Category 0: vegetation/biomass
        (2, 0, 0) => ("Land cover (0 = sea, 1 = land)", SourceUnit::Proportion),
        // WMO marks (2, 0, 2) as Deprecated but centres still emit it.
        (2, 0, 2) => ("Soil temperature", SourceUnit::Kelvin),
        // NOTE: (2, 0, 5) in WMO is "Water runoff", not volumetric soil
        // moisture. The modern operational soil-moisture parameter is
        // (2, 0, 25) "Volumetric soil moisture" (m3 m-3). Shipping both.
        (2, 0, 5) => ("Water runoff", SourceUnit::KgPerM2),
        (2, 0, 25) => ("Volumetric soil moisture", SourceUnit::Raw("m3 m-3")),

        // ---- Discipline 10: Oceanographic products ----

        // Category 3: surface properties
        (10, 3, 0) => ("Water temperature", SourceUnit::Kelvin),
        (10, 3, 1) => ("Deviation of sea level from mean", SourceUnit::Metres),

        _ => return None,
    };

    Some(ParamInfo { label, source_unit })
}

/// ECMWF (center 98) local-table overrides for parameter numbers in the
/// 192-254 range.
///
/// ECMWF ships cloud cover and the land-sea mask as unitless proportions in
/// [0, 1] under these local parameter numbers. The engine must distinguish
/// these from the standard WMO cloud-cover entries (which are already in %)
/// to avoid double-scaling on display.
fn local_lookup(center: u16, discipline: u8, category: u8, number: u8) -> Option<ParamInfo> {
    let (label, source_unit) = match (center, discipline, category, number) {
        // ECMWF (centre 98) local extensions — cloud cover as proportion.
        (98, 0, 6, 192) => ("Total cloud cover", SourceUnit::Proportion),
        (98, 0, 6, 193) => ("Low cloud cover", SourceUnit::Proportion),
        (98, 0, 6, 194) => ("Medium cloud cover", SourceUnit::Proportion),
        (98, 0, 6, 195) => ("High cloud cover", SourceUnit::Proportion),

        // ECMWF local precipitation and snow fields. ECMWF reports these
        // in metres of water equivalent; we auto-display in mm.
        (98, 0, 1, 193) => ("Total precipitation", SourceUnit::MetresOfWater),
        (98, 0, 1, 198) => ("Snowfall (water equivalent)", SourceUnit::MetresOfWater),
        (98, 0, 1, 254) => ("Snow depth", SourceUnit::MetresOfWater),

        (98, 2, 0, 192) => ("Land-sea mask", SourceUnit::Proportion),
        _ => return None,
    };

    Some(ParamInfo { label, source_unit })
}

/// Display conversion result: `display = raw * scale + offset`.
#[derive(Debug, Clone, Copy)]
pub struct DisplayConversion {
    /// Unit string suitable for display (e.g. `"°C"`, `"hPa"`, `"mm"`).
    pub display_unit: &'static str,
    /// Multiplicative scale factor.
    pub scale: f64,
    /// Additive offset applied after scaling.
    pub offset: f64,
}

impl DisplayConversion {
    /// Apply the conversion: `display = raw * scale + offset`.
    #[inline]
    pub fn convert(&self, raw: f64) -> f64 {
        raw * self.scale + self.offset
    }

    /// True if this is a non-identity conversion.
    pub fn has_conversion(&self) -> bool {
        (self.scale - 1.0).abs() > 1e-12 || self.offset.abs() > 1e-12
    }
}

/// Compute the default display conversion for a given source unit.
///
/// Mechanical rules:
/// - `Kelvin` → `°C` (scale 1, offset -273.15)
/// - `Pascal` → `hPa` (scale 0.01, offset 0)
/// - `KgPerM2` → `mm` (scale 1, offset 0) — water-depth equivalent
/// - `M2PerS2` → `gpm` (scale 1/9.80665, offset 0)
/// - `Proportion` → `%` (scale 100, offset 0)
/// - Everything else → identity with a canonical source-unit string.
pub fn default_display(source: SourceUnit) -> DisplayConversion {
    match source {
        SourceUnit::Kelvin => DisplayConversion {
            display_unit: "°C",
            scale: 1.0,
            offset: -273.15,
        },
        SourceUnit::Celsius => DisplayConversion {
            display_unit: "°C",
            scale: 1.0,
            offset: 0.0,
        },
        SourceUnit::Pascal => DisplayConversion {
            display_unit: "hPa",
            scale: 0.01,
            offset: 0.0,
        },
        SourceUnit::Hectopascal => DisplayConversion {
            display_unit: "hPa",
            scale: 1.0,
            offset: 0.0,
        },
        // 1 kg m-2 of water ≈ 1 mm depth. Display as mm without scaling.
        SourceUnit::KgPerM2 => DisplayConversion {
            display_unit: "mm",
            scale: 1.0,
            offset: 0.0,
        },
        SourceUnit::KgPerM2PerS => DisplayConversion {
            display_unit: "kg m-2 s-1",
            scale: 1.0,
            offset: 0.0,
        },
        SourceUnit::MetresPerSecond => DisplayConversion {
            display_unit: "m s-1",
            scale: 1.0,
            offset: 0.0,
        },
        SourceUnit::Metres => DisplayConversion {
            display_unit: "m",
            scale: 1.0,
            offset: 0.0,
        },
        // ECMWF encodes precipitation/snow fields in metres of water
        // equivalent; auto-display in millimetres so WMS colormap ranges
        // are in the same units as the WMO kg m-2 convention used elsewhere.
        SourceUnit::MetresOfWater => DisplayConversion {
            display_unit: "mm",
            scale: 1000.0,
            offset: 0.0,
        },
        SourceUnit::Millimetres => DisplayConversion {
            display_unit: "mm",
            scale: 1.0,
            offset: 0.0,
        },
        // Geopotential → geopotential metres: divide by g₀ = 9.80665 m s-2.
        SourceUnit::M2PerS2 => DisplayConversion {
            display_unit: "gpm",
            scale: 1.0 / 9.80665,
            offset: 0.0,
        },
        SourceUnit::Gpm => DisplayConversion {
            display_unit: "gpm",
            scale: 1.0,
            offset: 0.0,
        },
        SourceUnit::Proportion => DisplayConversion {
            display_unit: "%",
            scale: 100.0,
            offset: 0.0,
        },
        SourceUnit::Percent => DisplayConversion {
            display_unit: "%",
            scale: 1.0,
            offset: 0.0,
        },
        SourceUnit::JoulesPerKg => DisplayConversion {
            display_unit: "J kg-1",
            scale: 1.0,
            offset: 0.0,
        },
        SourceUnit::JoulesPerM2 => DisplayConversion {
            display_unit: "J m-2",
            scale: 1.0,
            offset: 0.0,
        },
        SourceUnit::WattsPerM2 => DisplayConversion {
            display_unit: "W m-2",
            scale: 1.0,
            offset: 0.0,
        },
        SourceUnit::KgPerKg => DisplayConversion {
            display_unit: "kg kg-1",
            scale: 1.0,
            offset: 0.0,
        },
        SourceUnit::InversePerSec => DisplayConversion {
            display_unit: "s-1",
            scale: 1.0,
            offset: 0.0,
        },
        SourceUnit::Dimensionless => DisplayConversion {
            display_unit: "",
            scale: 1.0,
            offset: 0.0,
        },
        SourceUnit::Raw(s) => DisplayConversion {
            display_unit: s,
            scale: 1.0,
            offset: 0.0,
        },
    }
}

/// Build a short, human-readable level qualifier from a GRIB2
/// [Code Table 4.5](https://www.nco.ncep.noaa.gov/pmb/docs/grib2/grib2_doc/grib2_table4-5.shtml)
/// `(surface_type, scaled_value)` pair. Returns `None` when the surface type
/// carries no disambiguating information (e.g., type 255 = missing).
///
/// Examples:
/// - `(1, None)` → `Some("surface")`
/// - `(100, Some(50000))` → `Some("500 hPa")`
/// - `(101, None)` → `Some("mean sea level")`
/// - `(103, Some(2))` → `Some("2 m above ground")`
/// - `(106, Some(0.1))` → `Some("0.1 m below ground")`
/// - `(200, None)` → `Some("entire atmosphere")`
///
/// The function is intentionally forgiving: unknown surface types return
/// `Some("surface type {n}")` so they at least disambiguate; `255` returns
/// `None` so labels are not cluttered when no surface info is available.
pub fn format_level_qualifier(surface_type: u8, value: Option<f64>) -> Option<String> {
    // 255 = Missing (per WMO Code Table 4.5). Nothing to say.
    if surface_type == 255 {
        return None;
    }

    // Helpers for formatting numeric levels in a way that round-trips
    // integers nicely (no trailing ".0") while still allowing floats.
    fn fmt_num(v: f64) -> String {
        if v.fract() == 0.0 && v.abs() < 1e15 {
            format!("{}", v as i64)
        } else {
            format!("{v}")
        }
    }

    let q = match (surface_type, value) {
        (1, _) => "surface".to_string(),
        (2, _) => "cloud base".to_string(),
        (3, _) => "cloud top".to_string(),
        (4, _) => "0°C isotherm".to_string(),
        (5, _) => "LCL (adiabatic condensation level)".to_string(),
        (6, _) => "max wind level".to_string(),
        (7, _) => "tropopause".to_string(),
        (8, _) => "top of atmosphere".to_string(),
        (9, _) => "sea bottom".to_string(),
        (10, _) => "entire atmosphere".to_string(),
        (11, Some(v)) => format!("{} m below cumulonimbus base", fmt_num(v)),
        (20, Some(v)) => format!("{} K isothermal", fmt_num(v)),
        // Isobaric surface → value is in Pa, show in hPa for readability.
        (100, Some(pa)) => format!("{} hPa", fmt_num(pa / 100.0)),
        (100, None) => "pressure level".to_string(),
        (101, _) => "mean sea level".to_string(),
        (102, Some(v)) => format!("{} m above MSL", fmt_num(v)),
        (102, None) => "above MSL".to_string(),
        (103, Some(v)) => format!("{} m above ground", fmt_num(v)),
        (103, None) => "above ground".to_string(),
        (104, Some(v)) => format!("sigma {}", fmt_num(v)),
        (104, None) => "sigma level".to_string(),
        (105, Some(v)) => format!("hybrid level {}", fmt_num(v)),
        (105, None) => "hybrid level".to_string(),
        (106, Some(v)) => format!("{} m below ground", fmt_num(v)),
        (106, None) => "below ground".to_string(),
        (107, Some(v)) => format!("{} K isentropic", fmt_num(v)),
        (107, None) => "isentropic level".to_string(),
        (108, Some(v)) => format!("{} Pa above ground", fmt_num(v)),
        (108, None) => "pressure level above ground".to_string(),
        (109, Some(v)) => format!("{} K m² kg⁻¹ s⁻¹ PV surface", fmt_num(v)),
        (109, None) => "potential vorticity surface".to_string(),
        (160, Some(v)) => format!("{} m below sea level", fmt_num(v)),
        (160, None) => "below sea level".to_string(),
        (200, _) => "entire atmosphere".to_string(),
        (201, _) => "entire ocean".to_string(),
        (204, _) => "highest tropospheric freezing level".to_string(),
        (220, _) => "planetary boundary layer".to_string(),
        (234, _) => "bottom of wet bulb zero".to_string(),
        (235, Some(v)) => format!("{} K ocean isothermal", fmt_num(v)),
        (242, _) => "convective cloud bottom".to_string(),
        (243, _) => "convective cloud top".to_string(),
        (244, _) => "convective cloud layer".to_string(),
        (245, _) => "lowest level of wet bulb zero".to_string(),
        (246, _) => "maximum equivalent potential temperature level".to_string(),
        (247, _) => "equilibrium level".to_string(),
        (248, _) => "shallow convective cloud bottom".to_string(),
        (249, _) => "shallow convective cloud top".to_string(),
        (251, _) => "deep convective cloud bottom".to_string(),
        (252, _) => "deep convective cloud top".to_string(),
        // Fallback: disambiguate by the raw code so users can still tell
        // different levels apart even if we don't have a name for the type.
        (n, Some(v)) => format!("surface type {n} @ {}", fmt_num(v)),
        (n, None) => format!("surface type {n}"),
    };

    Some(q)
}

/// Compose a final display label from a base WMO parameter label and an
/// optional level qualifier. If the base label already contains the
/// qualifier (case-insensitive substring match), the qualifier is not
/// appended, so "Pressure reduced to MSL" + "mean sea level" stays just
/// "Pressure reduced to MSL".
pub fn compose_label(base: &str, qualifier: Option<&str>) -> String {
    match qualifier {
        None => base.to_string(),
        Some(q) => {
            let base_lower = base.to_ascii_lowercase();
            let q_lower = q.to_ascii_lowercase();
            // Skip composition if the qualifier is already embedded in the
            // base label. Check both the full qualifier and a short form
            // (e.g. "MSL" in "Pressure reduced to MSL").
            if base_lower.contains(&q_lower) {
                return base.to_string();
            }
            if q_lower == "mean sea level" && base_lower.contains("msl") {
                return base.to_string();
            }
            format!("{base} ({q})")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- lookup() ----

    #[test]
    fn lookup_temperature() {
        let info = lookup(0, 0, 0, 0).expect("temperature must resolve");
        assert_eq!(info.label, "Temperature");
        assert_eq!(info.source_unit, SourceUnit::Kelvin);
    }

    #[test]
    fn lookup_pressure_reduced_msl_center_agnostic() {
        // Number < 192 so the center argument is irrelevant. NCEP (7), WMO (0)
        // and ECMWF (98) must all resolve to the same standard entry.
        for center in [0u16, 7, 98] {
            let info = lookup(center, 0, 3, 1).expect("MSLP must resolve");
            assert_eq!(info.label, "Pressure reduced to MSL");
            assert_eq!(info.source_unit, SourceUnit::Pascal);
        }
    }

    #[test]
    fn lookup_gfs_tcdc_is_percent() {
        // GFS ships total cloud cover under the standard WMO triple with unit %.
        let info = lookup(7, 0, 6, 1).expect("GFS TCDC must resolve");
        assert_eq!(info.label, "Total cloud cover");
        assert_eq!(info.source_unit, SourceUnit::Percent);
    }

    #[test]
    fn lookup_ecmwf_tcc_is_proportion() {
        // ECMWF ships tcc under a local parameter number with unit proportion.
        let info = lookup(98, 0, 6, 192).expect("ECMWF tcc must resolve");
        assert_eq!(info.label, "Total cloud cover");
        assert_eq!(info.source_unit, SourceUnit::Proportion);
    }

    #[test]
    fn lookup_total_precipitation() {
        let info = lookup(0, 0, 1, 8).expect("total precipitation must resolve");
        assert_eq!(info.label, "Total precipitation");
        assert_eq!(info.source_unit, SourceUnit::KgPerM2);
    }

    #[test]
    fn lookup_geopotential() {
        let info = lookup(0, 0, 3, 4).expect("geopotential must resolve");
        assert_eq!(info.label, "Geopotential");
        assert_eq!(info.source_unit, SourceUnit::M2PerS2);
    }

    #[test]
    fn lookup_geopotential_height() {
        let info = lookup(0, 0, 3, 5).expect("geopotential height must resolve");
        assert_eq!(info.label, "Geopotential height");
        assert_eq!(info.source_unit, SourceUnit::Gpm);
    }

    #[test]
    fn lookup_u_component_of_wind() {
        let info = lookup(0, 0, 2, 2).expect("u-component must resolve");
        assert_eq!(info.label, "u-component of wind");
        assert_eq!(info.source_unit, SourceUnit::MetresPerSecond);
    }

    #[test]
    fn lookup_unknown_returns_none() {
        assert!(lookup(0, 99, 99, 99).is_none());
    }

    #[test]
    fn local_overlay_falls_through_to_standard() {
        // An ECMWF local number (>=192) that we have not mapped locally must
        // fall back to the standard table — which is also a miss here, so
        // the overall lookup returns None.
        assert!(lookup(98, 0, 99, 200).is_none());
    }

    // ---- default_display() ----

    #[test]
    fn display_kelvin_to_celsius() {
        let d = default_display(SourceUnit::Kelvin);
        assert_eq!(d.display_unit, "°C");
        assert_eq!(d.scale, 1.0);
        assert_eq!(d.offset, -273.15);
        assert!(d.has_conversion());
        assert!((d.convert(273.15) - 0.0).abs() < 1e-10);
        assert!((d.convert(293.15) - 20.0).abs() < 1e-10);
    }

    #[test]
    fn display_pascal_to_hpa() {
        let d = default_display(SourceUnit::Pascal);
        assert_eq!(d.display_unit, "hPa");
        assert_eq!(d.scale, 0.01);
        assert_eq!(d.offset, 0.0);
        assert!((d.convert(101325.0) - 1013.25).abs() < 1e-10);
    }

    #[test]
    fn display_kg_per_m2_is_mm_identity() {
        let d = default_display(SourceUnit::KgPerM2);
        assert_eq!(d.display_unit, "mm");
        assert_eq!(d.scale, 1.0);
        assert_eq!(d.offset, 0.0);
        assert!(!d.has_conversion());
        assert!((d.convert(5.0) - 5.0).abs() < 1e-12);
    }

    #[test]
    fn display_geopotential_to_gpm() {
        let d = default_display(SourceUnit::M2PerS2);
        assert_eq!(d.display_unit, "gpm");
        // 1 / 9.80665 ≈ 0.1019716
        assert!((d.scale - 0.101_971_6).abs() < 1e-6);
        assert_eq!(d.offset, 0.0);
        assert!((d.convert(9806.65) - 1000.0).abs() < 0.01);
    }

    #[test]
    fn display_proportion_to_percent() {
        let d = default_display(SourceUnit::Proportion);
        assert_eq!(d.display_unit, "%");
        assert_eq!(d.scale, 100.0);
        assert_eq!(d.offset, 0.0);
        assert!((d.convert(0.75) - 75.0).abs() < 1e-10);
    }

    #[test]
    fn display_m_s_is_identity() {
        let d = default_display(SourceUnit::MetresPerSecond);
        assert_eq!(d.display_unit, "m s-1");
        assert_eq!(d.scale, 1.0);
        assert_eq!(d.offset, 0.0);
        assert!(!d.has_conversion());
    }

    // ---- Cloud cover regression ----

    #[test]
    fn gfs_cloud_cover_value_is_not_double_scaled() {
        // GFS reports 75% cloud cover as 75.0. Resolving via the standard
        // table must produce an identity display conversion so the number
        // stays 75.0 (not 7500.0).
        let info = lookup(7, 0, 6, 1).expect("GFS TCDC must resolve");
        let display = default_display(info.source_unit);
        assert_eq!(display.display_unit, "%");
        assert!(!display.has_conversion());
        assert!((display.convert(75.0) - 75.0).abs() < 1e-12);
    }

    #[test]
    fn ecmwf_cloud_cover_value_is_scaled_to_percent() {
        // ECMWF reports 75% cloud cover as 0.75 under local parameter 192.
        // The resolver must apply the Proportion → Percent conversion.
        let info = lookup(98, 0, 6, 192).expect("ECMWF tcc must resolve");
        let display = default_display(info.source_unit);
        assert_eq!(display.display_unit, "%");
        assert!(display.has_conversion());
        assert!((display.convert(0.75) - 75.0).abs() < 1e-10);
    }

    // ---- parse_source_unit() sanity ----

    #[test]
    fn parse_known_units() {
        assert_eq!(parse_source_unit("K"), SourceUnit::Kelvin);
        assert_eq!(parse_source_unit("Pa"), SourceUnit::Pascal);
        assert_eq!(parse_source_unit("kg m-2"), SourceUnit::KgPerM2);
        assert_eq!(parse_source_unit("kg m-2 s-1"), SourceUnit::KgPerM2PerS);
        assert_eq!(parse_source_unit("m s-1"), SourceUnit::MetresPerSecond);
        assert_eq!(parse_source_unit("gpm"), SourceUnit::Gpm);
        assert_eq!(parse_source_unit("Proportion"), SourceUnit::Proportion);
        assert_eq!(parse_source_unit("%"), SourceUnit::Percent);
        assert_eq!(parse_source_unit("Numeric"), SourceUnit::Dimensionless);
    }

    #[test]
    fn parse_unknown_unit_is_raw() {
        let u = parse_source_unit("furlongs per fortnight");
        match u {
            SourceUnit::Raw(s) => assert_eq!(s, "furlongs per fortnight"),
            other => panic!("expected Raw, got {other:?}"),
        }
    }

    // ---- format_level_qualifier() ----

    #[test]
    fn qualifier_surface() {
        assert_eq!(format_level_qualifier(1, None), Some("surface".to_string()));
    }

    #[test]
    fn qualifier_msl() {
        assert_eq!(
            format_level_qualifier(101, None),
            Some("mean sea level".to_string())
        );
    }

    #[test]
    fn qualifier_hag_2m_integer_formatted() {
        assert_eq!(
            format_level_qualifier(103, Some(2.0)),
            Some("2 m above ground".to_string())
        );
    }

    #[test]
    fn qualifier_hag_10m() {
        assert_eq!(
            format_level_qualifier(103, Some(10.0)),
            Some("10 m above ground".to_string())
        );
    }

    #[test]
    fn qualifier_pressure_level_converts_pa_to_hpa() {
        // 50000 Pa = 500 hPa
        assert_eq!(
            format_level_qualifier(100, Some(50000.0)),
            Some("500 hPa".to_string())
        );
    }

    #[test]
    fn qualifier_soil_depth_fractional() {
        assert_eq!(
            format_level_qualifier(106, Some(0.1)),
            Some("0.1 m below ground".to_string())
        );
    }

    #[test]
    fn qualifier_missing_surface_type_is_none() {
        assert_eq!(format_level_qualifier(255, None), None);
    }

    #[test]
    fn qualifier_unknown_surface_type_is_disambiguated() {
        // Unknown type 230 (as of this writing) — still disambiguated with
        // the raw code so different levels remain distinguishable.
        let q = format_level_qualifier(230, None).unwrap();
        assert!(q.contains("230"));
    }

    // ---- compose_label() ----

    #[test]
    fn compose_label_appends_qualifier() {
        assert_eq!(
            compose_label("Pressure", Some("surface")),
            "Pressure (surface)"
        );
        assert_eq!(
            compose_label("Pressure", Some("mean sea level")),
            "Pressure (mean sea level)"
        );
        assert_eq!(
            compose_label("Temperature", Some("2 m above ground")),
            "Temperature (2 m above ground)"
        );
    }

    #[test]
    fn compose_label_skips_when_qualifier_already_in_base() {
        // GFS PRMSL has base label "Pressure reduced to MSL" — no need to
        // append "(mean sea level)".
        assert_eq!(
            compose_label("Pressure reduced to MSL", Some("mean sea level")),
            "Pressure reduced to MSL"
        );
        // And the case-insensitive exact-substring fallback works too.
        assert_eq!(
            compose_label("Total cloud cover", Some("entire atmosphere")),
            "Total cloud cover (entire atmosphere)"
        );
    }

    #[test]
    fn compose_label_no_qualifier() {
        assert_eq!(compose_label("Temperature", None), "Temperature");
    }

    /// The user-reported regression: ECMWF ships `msl` and `sp` under the
    /// same WMO triple (0, 3, 0) but distinguishes them via Code Table 4.5
    /// surface types. Without a level-aware label, both end up called
    /// "Pressure". With the fix they are distinct.
    #[test]
    fn ecmwf_pressure_disambiguation() {
        // Both resolve to the same WMO parameter…
        let sp_info = lookup(98, 0, 3, 0).unwrap();
        let msl_info = lookup(98, 0, 3, 0).unwrap();
        assert_eq!(sp_info.label, msl_info.label);
        assert_eq!(sp_info.label, "Pressure");

        // …but their Table 4.5 surface types are different.
        let sp_label = compose_label(sp_info.label, format_level_qualifier(1, None).as_deref());
        let msl_label = compose_label(msl_info.label, format_level_qualifier(101, None).as_deref());

        assert_eq!(sp_label, "Pressure (surface)");
        assert_eq!(msl_label, "Pressure (mean sea level)");
        assert_ne!(sp_label, msl_label, "sp and msl must be distinguishable");
    }
}
