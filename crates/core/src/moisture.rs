//! Relative humidity and dew point derived from their ingredients (#898).
//!
//! Many NWP collections carry what relative humidity or dew point is made
//! of, but not the parameter itself: ECMWF's single levels have `2t` and
//! `2d` but no `2r`, AIFS's pressure levels `t` and `q` but neither `r` nor
//! a dew point. This module is the one home for turning the ingredients
//! into the missing parameter:
//!
//! - the formulas: [`saturation_vapour_pressure`] and its inverse, and the
//!   four paths ([`relative_humidity_from_dew_point`],
//!   [`relative_humidity_from_specific_humidity`],
//!   [`dew_point_from_relative_humidity`],
//!   [`dew_point_from_specific_humidity`]);
//! - the units: each ingredient's served unit, read from the source's
//!   metadata ([`TemperatureUnit`], [`RelativeHumidityUnit`],
//!   [`SpecificHumidityUnit`]), never from a name; an unknown unit refuses
//!   the derivation;
//! - the pairing: which parameters are the ingredients at one level, and
//!   what the derived parameter is called in the source's own vocabulary
//!   ([`MoisturePlan`]).
//!
//! **Saturation reference.** Every derived relative humidity is relative
//! to liquid water at all temperatures, as WMO-No. 8 defines it, and says
//! so in its title. Dew point is relative to water by definition (frost
//! point is another quantity). One formula serves both directions: Tetens's
//! over water with the constants of the ECMWF IFS ([`TETENS_A1`],
//! [`TETENS_A3`], [`TETENS_A4`]), so a dew point derived from a relative
//! humidity gives that relative humidity back.
//!
//! **Bounds.** A derived relative humidity is clamped to 0–100 % and a
//! derived dew point to at most the temperature: interpolation and rounding
//! put a dew point slightly above its temperature, and NWP specific
//! humidity can dip below zero. A specific humidity or relative humidity of
//! zero or less gives a relative humidity of 0 and no dew point (the
//! logarithm of zero vapour pressure is undefined).
//!
//! **Pressure.** A path from specific humidity needs the air's pressure. It
//! is taken only from an isobaric level's own value, converted from hPa to
//! Pa ([`crate::vertical::VerticalKind::Pressure`]); a single level would
//! need the surface pressure (never mean sea-level pressure, which is
//! reduced), and model levels their hybrid coefficients, neither of which
//! is implemented.
//!
//! Engines report what their source states about each parameter through the
//! shared facts snapshot ([`crate::wind::WindFacts`]);
//! [`crate::derive::Derived`] serves the plan and computes each value after
//! sampling: the ingredients are sampled with the engine's own
//! interpolation at the output pixel or EDR point, then combined. A
//! relative humidity is not linear in its ingredients, so this is what makes
//! a map pixel and an EDR value at the same point agree.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::units::canonical_unit;
use crate::vertical::VerticalKind;
use crate::wind::{ParameterFacts, WindFacts};

#[cfg(test)]
mod tests;

/// CF standard name of a derived relative humidity.
pub const RELATIVE_HUMIDITY: &str = "relative_humidity";
/// CF standard name of a derived dew point.
pub const DEW_POINT_TEMPERATURE: &str = "dew_point_temperature";
/// CF standard name of air temperature, an ingredient.
pub const AIR_TEMPERATURE: &str = "air_temperature";
/// CF standard name of specific humidity, an ingredient.
pub const SPECIFIC_HUMIDITY: &str = "specific_humidity";
/// Unit of a derived relative humidity (QUDT `PERCENT`).
pub const RELATIVE_HUMIDITY_UNIT: &str = "%";
/// Title of a derived relative humidity, before its level: it names the
/// saturation reference, which EDR's `label` and `description` repeat.
/// Short enough that with a level (`… (2 m above ground)`, 47 characters)
/// it fits the Metocean profile's 50-character EDR label uncut.
pub const RELATIVE_HUMIDITY_TITLE: &str = "Relative humidity over water";
/// Title of a derived dew point, before its level.
pub const DEW_POINT_TITLE: &str = "Dew point temperature";

/// Tetens's saturation vapour pressure at [`TRIPLE_POINT`], Pa: `a1` of the
/// ECMWF IFS formula `e_s(T) = a1 · exp(a3 (T − T0) / (T − a4))` (IFS
/// Documentation, Part IV: Physical Processes, the cloud scheme's
/// saturation vapour pressure, constants after Buck, 1981).
pub const TETENS_A1: f64 = 611.21;
/// `a3` of the IFS Tetens formula over liquid water (over ice it is
/// 22.587, which this module never uses).
pub const TETENS_A3: f64 = 17.502;
/// `a4` of the IFS Tetens formula over liquid water, K (over ice −0.7 K).
pub const TETENS_A4: f64 = 32.19;
/// `T0` of the IFS Tetens formula: the triple point of water, K.
pub const TRIPLE_POINT: f64 = 273.16;
/// 0 °C in kelvin.
pub const CELSIUS_ZERO: f64 = 273.15;
/// The ratio of the gas constants of dry air and water vapour, `R_d/R_v`,
/// as WMO-No. 8 (Annex 4.B) gives it: vapour pressure from specific
/// humidity is `e = q·p / (ε + (1 − ε)·q)`.
pub const EPSILON: f64 = 0.621_98;

/// Saturation vapour pressure over liquid water, Pa, at `t` K: the IFS
/// Tetens formula. Within 0.3 % of the Goff–Gratch equation from −30 to
/// +40 °C. `None` for a temperature the formula does not hold at (not
/// finite, or at or below [`TETENS_A4`]).
#[inline]
pub fn saturation_vapour_pressure(t: f64) -> Option<f64> {
    (t.is_finite() && t > TETENS_A4)
        .then(|| TETENS_A1 * (TETENS_A3 * (t - TRIPLE_POINT) / (t - TETENS_A4)).exp())
}

/// The temperature, K, at which `e` Pa saturates over liquid water: the
/// dew point of air holding vapour at `e`, [`saturation_vapour_pressure`]
/// inverted exactly. `None` for `e` ≤ 0 or not finite.
#[inline]
pub fn dew_point_of_vapour_pressure(e: f64) -> Option<f64> {
    if !(e.is_finite() && e > 0.0) {
        return None;
    }
    // ln(e/a1) = a3 (Td − T0) / (Td − a4), solved for Td.
    let l = (e / TETENS_A1).ln();
    let td = (TETENS_A3 * TRIPLE_POINT - l * TETENS_A4) / (TETENS_A3 - l);
    td.is_finite().then_some(td)
}

/// Vapour pressure, Pa, of air at `p` Pa with specific humidity `q` kg/kg:
/// `q·p / (ε + (1 − ε)·q)`.
#[inline]
pub fn vapour_pressure(q: f64, p: f64) -> f64 {
    q * p / (EPSILON + (1.0 - EPSILON) * q)
}

/// Relative humidity over liquid water, %, from temperature `t` and dew
/// point `td`, both K: `100 · e_s(Td) / e_s(T)`, clamped to 0–100.
pub fn relative_humidity_from_dew_point(t: f64, td: f64) -> Option<f64> {
    let e = saturation_vapour_pressure(td)?;
    relative_humidity(t, e)
}

/// Relative humidity over liquid water, %, from temperature `t` K and
/// specific humidity `q` kg/kg at pressure `p` Pa, clamped to 0–100; `q` ≤ 0
/// gives 0.
pub fn relative_humidity_from_specific_humidity(t: f64, q: f64, p: f64) -> Option<f64> {
    if !(q.is_finite() && p.is_finite() && p > 0.0) {
        return None;
    }
    if q <= 0.0 {
        return saturation_vapour_pressure(t).map(|_| 0.0);
    }
    relative_humidity(t, vapour_pressure(q, p))
}

/// Dew point, K, from temperature `t` K and relative humidity `rh` % over
/// liquid water, at most `t`. `None` for `rh` ≤ 0.
pub fn dew_point_from_relative_humidity(t: f64, rh: f64) -> Option<f64> {
    if !(rh.is_finite() && rh > 0.0) {
        return None;
    }
    let e = rh / 100.0 * saturation_vapour_pressure(t)?;
    dew_point_of_vapour_pressure(e).map(|td| td.min(t))
}

/// Dew point, K, from specific humidity `q` kg/kg at pressure `p` Pa, at
/// most the temperature `t` K. `None` for `q` ≤ 0.
pub fn dew_point_from_specific_humidity(t: f64, q: f64, p: f64) -> Option<f64> {
    if !(q.is_finite() && q > 0.0 && p.is_finite() && p > 0.0) {
        return None;
    }
    saturation_vapour_pressure(t)?;
    dew_point_of_vapour_pressure(vapour_pressure(q, p)).map(|td| td.min(t))
}

/// `100 · e / e_s(t)`, clamped to 0–100.
fn relative_humidity(t: f64, e: f64) -> Option<f64> {
    let saturation = saturation_vapour_pressure(t)?;
    let rh = 100.0 * e / saturation;
    rh.is_finite().then(|| rh.clamp(0.0, 100.0))
}

/// The unit a temperature is served in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TemperatureUnit {
    Kelvin,
    Celsius,
}

impl TemperatureUnit {
    /// The unit `unit` spells, in any spelling ([`canonical_unit`]); `None`
    /// for anything else.
    pub fn of(unit: &str) -> Option<Self> {
        match canonical_unit(unit).as_ref() {
            "K" => Some(TemperatureUnit::Kelvin),
            "°C" => Some(TemperatureUnit::Celsius),
            _ => None,
        }
    }

    #[inline]
    fn to_kelvin(self, value: f64) -> f64 {
        match self {
            TemperatureUnit::Kelvin => value,
            TemperatureUnit::Celsius => value + CELSIUS_ZERO,
        }
    }

    #[inline]
    fn kelvin_to_unit(self, kelvin: f64) -> f64 {
        match self {
            TemperatureUnit::Kelvin => kelvin,
            TemperatureUnit::Celsius => kelvin - CELSIUS_ZERO,
        }
    }
}

/// The unit a relative humidity is served in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelativeHumidityUnit {
    /// `%`.
    Percent,
    /// A fraction, 0–1: CF's `1`, as a source serves it unconverted.
    Fraction,
}

impl RelativeHumidityUnit {
    pub fn of(unit: &str) -> Option<Self> {
        match canonical_unit(unit).as_ref() {
            "%" => Some(RelativeHumidityUnit::Percent),
            "1" => Some(RelativeHumidityUnit::Fraction),
            _ => None,
        }
    }

    #[inline]
    fn to_percent(self, value: f64) -> f64 {
        match self {
            RelativeHumidityUnit::Percent => value,
            RelativeHumidityUnit::Fraction => value * 100.0,
        }
    }
}

/// The unit a specific humidity is served in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpecificHumidityUnit {
    /// `kg kg-1`, or CF's `1` for a parameter the metadata states is a
    /// specific humidity: a mass fraction.
    KgPerKg,
    /// `g kg-1`.
    GramPerKg,
}

impl SpecificHumidityUnit {
    pub fn of(unit: &str) -> Option<Self> {
        match canonical_unit(unit).as_ref() {
            "kg kg-1" | "1" => Some(SpecificHumidityUnit::KgPerKg),
            "g kg-1" => Some(SpecificHumidityUnit::GramPerKg),
            _ => None,
        }
    }

    #[inline]
    fn to_kg_per_kg(self, value: f64) -> f64 {
        match self {
            SpecificHumidityUnit::KgPerKg => value,
            SpecificHumidityUnit::GramPerKg => value / 1000.0,
        }
    }
}

/// A derived quantity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Quantity {
    RelativeHumidity,
    DewPoint,
}

impl Quantity {
    /// The CF standard name of the quantity.
    pub fn standard_name(self) -> &'static str {
        match self {
            Quantity::RelativeHumidity => RELATIVE_HUMIDITY,
            Quantity::DewPoint => DEW_POINT_TEMPERATURE,
        }
    }

    /// The quantity in messages: `"relative humidity"`.
    pub fn noun(self) -> &'static str {
        match self {
            Quantity::RelativeHumidity => "relative humidity",
            Quantity::DewPoint => "dew point",
        }
    }
}

/// How one derived humidity follows from its ingredients' samples, in its
/// served units. The samples come in the order of the derived parameter's
/// inputs, then the level's pressure in Pa for a path from specific
/// humidity, which the wrapper appends ([`Recipe::reads_level_pressure`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Recipe {
    /// Relative humidity, %, from temperature and dew point.
    RelativeHumidityFromDewPoint {
        t: TemperatureUnit,
        td: TemperatureUnit,
    },
    /// Relative humidity, %, from temperature and specific humidity at the
    /// level's pressure.
    RelativeHumidityFromSpecificHumidity {
        t: TemperatureUnit,
        q: SpecificHumidityUnit,
    },
    /// Dew point, in the temperature's unit, from temperature and relative
    /// humidity, the relative humidity taken as over liquid water.
    DewPointFromRelativeHumidity {
        t: TemperatureUnit,
        rh: RelativeHumidityUnit,
    },
    /// Dew point, in the temperature's unit, from specific humidity at the
    /// level's pressure, at most the temperature.
    DewPointFromSpecificHumidity {
        t: TemperatureUnit,
        q: SpecificHumidityUnit,
    },
}

impl Recipe {
    /// The value from the samples: inputs `[t, td]`, `[t, rh]`, or
    /// `[t, q, p]` with `p` the level's pressure in Pa. `None` when a sample
    /// is missing, or none is defined (see the module docs' bounds).
    #[inline]
    pub fn value(self, samples: &[Option<f64>]) -> Option<f64> {
        match (self, samples) {
            (Recipe::RelativeHumidityFromDewPoint { t, td }, &[Some(tv), Some(tdv)]) => {
                relative_humidity_from_dew_point(t.to_kelvin(tv), td.to_kelvin(tdv))
            }
            (
                Recipe::RelativeHumidityFromSpecificHumidity { t, q },
                &[Some(tv), Some(qv), Some(p)],
            ) => relative_humidity_from_specific_humidity(t.to_kelvin(tv), q.to_kg_per_kg(qv), p),
            (Recipe::DewPointFromRelativeHumidity { t, rh }, &[Some(tv), Some(rhv)]) => {
                dew_point_from_relative_humidity(t.to_kelvin(tv), rh.to_percent(rhv))
                    .map(|td| t.kelvin_to_unit(td))
            }
            (Recipe::DewPointFromSpecificHumidity { t, q }, &[Some(tv), Some(qv), Some(p)]) => {
                dew_point_from_specific_humidity(t.to_kelvin(tv), q.to_kg_per_kg(qv), p)
                    .map(|td| t.kelvin_to_unit(td))
            }
            _ => None,
        }
    }

    /// What it computes.
    pub fn quantity(self) -> Quantity {
        match self {
            Recipe::RelativeHumidityFromDewPoint { .. }
            | Recipe::RelativeHumidityFromSpecificHumidity { .. } => Quantity::RelativeHumidity,
            Recipe::DewPointFromRelativeHumidity { .. }
            | Recipe::DewPointFromSpecificHumidity { .. } => Quantity::DewPoint,
        }
    }

    /// Whether its last sample is the level's pressure, which the wrapper
    /// reads from the request's isobaric level.
    pub fn reads_level_pressure(self) -> bool {
        matches!(
            self,
            Recipe::RelativeHumidityFromSpecificHumidity { .. }
                | Recipe::DewPointFromSpecificHumidity { .. }
        )
    }
}

/// The quantity a parameter is, as far as humidity derivation cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MoistureRole {
    Temperature,
    DewPoint,
    RelativeHumidity,
    SpecificHumidity,
}

impl MoistureRole {
    /// WMO GRIB2 code table 4.2, discipline 0: temperature 0/0/0, dew point
    /// 0/0/6, specific humidity 0/1/0, relative humidity 0/1/1.
    pub fn from_grib(triple: (u8, u8, u8)) -> Option<Self> {
        match triple {
            (0, 0, 0) => Some(MoistureRole::Temperature),
            (0, 0, 6) => Some(MoistureRole::DewPoint),
            (0, 1, 0) => Some(MoistureRole::SpecificHumidity),
            (0, 1, 1) => Some(MoistureRole::RelativeHumidity),
            _ => None,
        }
    }

    /// A CF `standard_name`.
    pub fn from_standard_name(name: &str) -> Option<Self> {
        match name {
            AIR_TEMPERATURE => Some(MoistureRole::Temperature),
            DEW_POINT_TEMPERATURE => Some(MoistureRole::DewPoint),
            RELATIVE_HUMIDITY => Some(MoistureRole::RelativeHumidity),
            SPECIFIC_HUMIDITY => Some(MoistureRole::SpecificHumidity),
            _ => None,
        }
    }

    /// An FMI newbase parameter number (`NFmiParameterName.h`:
    /// `kFmiTemperature = 4`, `kFmiDewPoint = 10`, `kFmiHumidity = 13`,
    /// relative). QueryData's specific humidity has no number with a fixed
    /// unit, so it is no ingredient.
    pub fn from_fmi_param(id: u32) -> Option<Self> {
        match id {
            4 => Some(MoistureRole::Temperature),
            10 => Some(MoistureRole::DewPoint),
            13 => Some(MoistureRole::RelativeHumidity),
            _ => None,
        }
    }

    fn fmi_param(self) -> Option<u32> {
        match self {
            MoistureRole::Temperature => Some(4),
            MoistureRole::DewPoint => Some(10),
            MoistureRole::RelativeHumidity => Some(13),
            MoistureRole::SpecificHumidity => None,
        }
    }

    fn noun(self) -> &'static str {
        match self {
            MoistureRole::Temperature => "temperature",
            MoistureRole::DewPoint => "dew point",
            MoistureRole::RelativeHumidity => "relative humidity",
            MoistureRole::SpecificHumidity => "specific humidity",
        }
    }
}

/// The role the source's metadata states: every piece it states must
/// agree. `Err` when two disagree, `Ok(None)` when none names a role.
fn asserted_role(p: &ParameterFacts) -> Result<Option<MoistureRole>, ()> {
    let stated = [
        p.grib.map(MoistureRole::from_grib),
        p.standard_name
            .as_deref()
            .map(MoistureRole::from_standard_name),
        p.fmi_param.map(MoistureRole::from_fmi_param),
    ];
    let mut role: Option<Option<MoistureRole>> = None;
    for found in stated.into_iter().flatten() {
        match role {
            None => role = Some(found),
            Some(r) if r == found => {}
            Some(_) => return Err(()),
        }
    }
    Ok(role.flatten())
}

/// The role from metadata, else from the vocabulary name when the
/// parameter has no metadata at all.
fn role_of(p: &ParameterFacts) -> Option<MoistureRole> {
    match asserted_role(p) {
        Ok(Some(role)) => Some(role),
        Ok(None) if p.grib.is_none() && p.standard_name.is_none() && p.fmi_param.is_none() => {
            vocabulary_role(&p.name)
        }
        _ => None,
    }
}

/// One source vocabulary's ingredient names, which are also the names a
/// derived relative humidity or dew point takes in it. Names are
/// case-sensitive.
struct Vocabulary {
    t: &'static str,
    td: &'static str,
    rh: &'static str,
    q: Option<&'static str>,
}

const VOCABULARY: &[Vocabulary] = &[
    // ECMWF GRIB short names at 2 m (parameter database: 2t 167, 2d 168,
    // 2r 260242).
    Vocabulary {
        t: "2t",
        td: "2d",
        rh: "2r",
        q: None,
    },
    // ECMWF GRIB short names at any other level (t 130, q 133, r 157,
    // dpt 3017).
    Vocabulary {
        t: "t",
        td: "dpt",
        rh: "r",
        q: Some("q"),
    },
    // ECMWF netCDF / CF variable names at 2 m (ERA5; cfgrib names 2r `r2`).
    Vocabulary {
        t: "t2m",
        td: "d2m",
        rh: "r2",
        q: None,
    },
    // NCEP wgrib2 abbreviations, at any level.
    Vocabulary {
        t: "TMP",
        td: "DPT",
        rh: "RH",
        q: Some("SPFH"),
    },
    // FMI newbase names (QueryData).
    Vocabulary {
        t: "Temperature",
        td: "DewPoint",
        rh: "Humidity",
        q: None,
    },
];

impl Vocabulary {
    fn name(&self, role: MoistureRole) -> Option<&'static str> {
        match role {
            MoistureRole::Temperature => Some(self.t),
            MoistureRole::DewPoint => Some(self.td),
            MoistureRole::RelativeHumidity => Some(self.rh),
            MoistureRole::SpecificHumidity => self.q,
        }
    }
}

fn vocabulary_role(name: &str) -> Option<MoistureRole> {
    use MoistureRole::*;
    VOCABULARY.iter().find_map(|v| {
        [Temperature, DewPoint, RelativeHumidity, SpecificHumidity]
            .into_iter()
            .find(|&role| v.name(role) == Some(name))
    })
}

/// The name a temperature's partner of `role` has, and a derived one takes,
/// in the temperature's own vocabulary: a vocabulary's name; newbase's for a
/// QueryData temperature (FMI number 4); else, for a CF-identified
/// variable, its own name with its `air_temperature` token phrase swapped
/// (`air_temperature_2m` → `relative_humidity_2m`,
/// `air_temperature_pl` → `dew_point_temperature_pl`).
fn vocabulary_name(t: &ParameterFacts, role: MoistureRole) -> Option<String> {
    if let Some(v) = VOCABULARY.iter().find(|v| v.t == t.name) {
        return v.name(role).map(str::to_string);
    }
    if t.fmi_param == MoistureRole::Temperature.fmi_param() {
        let newbase = VOCABULARY.iter().find(|v| v.t == "Temperature")?;
        return newbase.name(role).map(str::to_string);
    }
    if t.standard_name.as_deref() != Some(AIR_TEMPERATURE) {
        return None;
    }
    let phrase = match role {
        MoistureRole::Temperature => AIR_TEMPERATURE,
        MoistureRole::DewPoint => DEW_POINT_TEMPERATURE,
        MoistureRole::RelativeHumidity => RELATIVE_HUMIDITY,
        MoistureRole::SpecificHumidity => SPECIFIC_HUMIDITY,
    };
    // Whole `_`-separated tokens only: `max_air_temperature` is fine,
    // `air_temperatures` is not.
    let name = t.name.as_str();
    let (at, _) = name.match_indices(AIR_TEMPERATURE).find(|&(at, _)| {
        let end = at + AIR_TEMPERATURE.len();
        (at == 0 || name.as_bytes()[at - 1] == b'_')
            && (end == name.len() || name.as_bytes()[end] == b'_')
    })?;
    Some(format!(
        "{}{phrase}{}",
        &name[..at],
        &name[at + AIR_TEMPERATURE.len()..]
    ))
}

/// Whether two parameters are at one level: the same single level, or both
/// spanning the same kind of vertical axis.
fn same_level(a: &ParameterFacts, b: &ParameterFacts) -> bool {
    a.level == b.level && a.vertical == b.vertical
}

/// One planned derived humidity.
#[derive(Debug, Clone, PartialEq)]
pub struct MoistureParameter {
    /// Its name, the same on every API.
    pub name: String,
    pub recipe: Recipe,
    /// The ingredients' map names, in the recipe's order.
    pub inputs: Vec<String>,
    /// The ingredients' EDR names, in the same order.
    pub edr_inputs: Vec<String>,
    pub title: String,
    /// `%` for a relative humidity; the temperature's unit for a dew point.
    pub unit: String,
}

/// What one temperature's level gave, for the load log: the relative
/// humidity and the dew point derived there, or why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoistureOutcome {
    /// The temperature the level is anchored at.
    pub temperature: String,
    /// The derived name and its ingredients, or why none.
    pub relative_humidity: Result<(String, String), String>,
    pub dew_point: Result<(String, String), String>,
}

impl fmt::Display for MoistureOutcome {
    /// `2t: derived relative humidity '2r' from '2t' and '2d'; no dew point:
    /// the collection already has '2d'`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: ", self.temperature)?;
        if let (Err(rh), Err(td)) = (&self.relative_humidity, &self.dew_point) {
            if rh == td {
                return write!(f, "nothing derived: {rh}");
            }
        }
        part(f, Quantity::RelativeHumidity, &self.relative_humidity)?;
        write!(f, "; ")?;
        part(f, Quantity::DewPoint, &self.dew_point)
    }
}

fn part(
    f: &mut fmt::Formatter<'_>,
    quantity: Quantity,
    result: &Result<(String, String), String>,
) -> fmt::Result {
    let noun = quantity.noun();
    match result {
        Ok((name, from)) => write!(f, "derived {noun} '{name}' from {from}"),
        Err(why) => write!(f, "no {noun}: {why}"),
    }
}

/// The humidities to derive for one source snapshot, and what each
/// temperature's level gave.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MoisturePlan {
    pub derived: Vec<MoistureParameter>,
    pub outcomes: Vec<MoistureOutcome>,
}

/// One temperature and its ingredients at its level.
struct Level<'a> {
    facts: &'a WindFacts,
    t: &'a ParameterFacts,
    td: Option<&'a ParameterFacts>,
    rh: Option<&'a ParameterFacts>,
    q: Option<&'a ParameterFacts>,
}

impl MoisturePlan {
    /// Every temperature (GRIB 0/0/0, CF `air_temperature`, FMI 4, or a
    /// vocabulary name on a parameter without metadata) whose level has a
    /// dew point, relative humidity or specific humidity, found by the
    /// name the temperature's vocabulary gives it (`2t` → `2d`, `TMP` →
    /// `RH`, `air_temperature_pl` → `specific_humidity_pl`) or, in
    /// QueryData, by FMI number. At that level:
    ///
    /// - relative humidity from the dew point, else from the specific
    ///   humidity and an isobaric level's pressure;
    /// - dew point from the specific humidity and an isobaric level's
    ///   pressure, which assumes nothing about the reference a native
    ///   relative humidity is relative to, else from the relative humidity.
    ///
    /// Each is skipped when the collection already has that quantity at the
    /// level (GRIB 0/1/1 or 0/0/6, CF `standard_name`, FMI number, or a
    /// vocabulary name on a parameter without metadata), when its name is a
    /// parameter's or one in `claimed` (derived by another derivation), or
    /// when an ingredient's unit is not stated or not one the formulas
    /// know.
    pub fn build(facts: &WindFacts, claimed: &HashSet<String>) -> MoisturePlan {
        let by_name: HashMap<&str, &ParameterFacts> = facts
            .parameters
            .iter()
            .map(|p| (p.name.as_str(), p))
            .collect();
        let taken: HashSet<&str> = facts
            .parameters
            .iter()
            .flat_map(|p| [p.name.as_str(), p.edr_name()])
            .collect();
        let mut planned: HashSet<String> = HashSet::new();
        let mut plan = MoisturePlan::default();
        for t in &facts.parameters {
            if role_of(t) != Some(MoistureRole::Temperature) {
                continue;
            }
            let partner = |role: MoistureRole| partner(t, role, &by_name, facts);
            let level = Level {
                facts,
                t,
                td: partner(MoistureRole::DewPoint),
                rh: partner(MoistureRole::RelativeHumidity),
                q: partner(MoistureRole::SpecificHumidity),
            };
            if level.td.is_none() && level.rh.is_none() && level.q.is_none() {
                continue;
            }
            let mut result = |quantity: Quantity| {
                let d = level.plan(quantity, |name| {
                    if taken.contains(name) || claimed.contains(name) {
                        Err(format!("the name '{name}' is taken by another parameter"))
                    } else if planned.contains(name) {
                        Err(format!("'{name}' is already derived at another level"))
                    } else {
                        Ok(())
                    }
                })?;
                planned.insert(d.name.clone());
                let from = quoted(&d.inputs, d.recipe.reads_level_pressure());
                let outcome = (d.name.clone(), from);
                plan.derived.push(d);
                Ok(outcome)
            };
            let relative_humidity = result(Quantity::RelativeHumidity);
            let dew_point = result(Quantity::DewPoint);
            plan.outcomes.push(MoistureOutcome {
                temperature: t.name.clone(),
                relative_humidity,
                dew_point,
            });
        }
        plan
    }
}

/// The ingredient of `role` at temperature `t`'s level: by the name `t`'s
/// vocabulary gives it, else, for a QueryData temperature, the one
/// parameter with that FMI number.
fn partner<'a>(
    t: &ParameterFacts,
    role: MoistureRole,
    by_name: &HashMap<&str, &'a ParameterFacts>,
    facts: &'a WindFacts,
) -> Option<&'a ParameterFacts> {
    if let Some(p) = vocabulary_name(t, role).and_then(|name| by_name.get(name.as_str()).copied()) {
        return Some(p);
    }
    if t.fmi_param != MoistureRole::Temperature.fmi_param() {
        return None;
    }
    let number = role.fmi_param()?;
    let mut found = facts
        .parameters
        .iter()
        .filter(|p| p.fmi_param == Some(number));
    match (found.next(), found.next()) {
        (Some(p), None) => Some(p),
        _ => None,
    }
}

/// `'2t' and '2d'`; `'t', 'q' and the level's pressure`.
fn quoted(names: &[String], pressure: bool) -> String {
    let mut parts: Vec<String> = names.iter().map(|n| format!("'{n}'")).collect();
    if pressure {
        parts.push("the level's pressure".into());
    }
    match parts.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
        _ => parts.concat(),
    }
}

/// A path to a quantity: the recipe and its ingredients, or why the
/// ingredient present gives none. `None` when the ingredient is absent.
type Path<'a> = Option<Result<(Recipe, Vec<&'a ParameterFacts>), String>>;

impl<'a> Level<'a> {
    /// The one parameter of `quantity` at this level, or why there is none.
    fn plan(
        &self,
        quantity: Quantity,
        free: impl Fn(&str) -> Result<(), String>,
    ) -> Result<MoistureParameter, String> {
        let role = match quantity {
            Quantity::RelativeHumidity => MoistureRole::RelativeHumidity,
            Quantity::DewPoint => MoistureRole::DewPoint,
        };
        if let Some(existing) = self
            .facts
            .parameters
            .iter()
            .find(|p| role_of(p) == Some(role) && same_level(p, self.t))
        {
            return Err(format!("the collection already has '{}'", existing.name));
        }
        let t_unit = unit(self.t, TemperatureUnit::of, "K or °C")?;
        let (recipe, inputs) = self.recipe(quantity, t_unit)?;
        let name = vocabulary_name(self.t, role)
            .ok_or_else(|| "the source's vocabulary has no name for it".to_string())?;
        free(&name)?;
        let (title, unit) = match quantity {
            Quantity::RelativeHumidity => (RELATIVE_HUMIDITY_TITLE, RELATIVE_HUMIDITY_UNIT),
            Quantity::DewPoint => (DEW_POINT_TITLE, self.t.unit.as_str()),
        };
        let title = match &self.t.level_label {
            Some(level) => format!("{title} ({level})"),
            None => title.to_string(),
        };
        Ok(MoistureParameter {
            name,
            recipe,
            inputs: inputs.iter().map(|p| p.name.clone()).collect(),
            edr_inputs: inputs.iter().map(|p| p.edr_name().to_string()).collect(),
            title,
            unit: unit.to_string(),
        })
    }

    /// The first path to `quantity` the level has, or the reason the first
    /// ingredient present gave none. Relative humidity prefers the dew
    /// point, which needs no pressure; a dew point prefers specific
    /// humidity, which needs no assumption about the saturation reference
    /// of a native relative humidity (ECMWF's is over ice below −23 °C).
    fn recipe(
        &self,
        quantity: Quantity,
        t: TemperatureUnit,
    ) -> Result<(Recipe, Vec<&'a ParameterFacts>), String> {
        let paths = match quantity {
            Quantity::RelativeHumidity => [
                self.via_dew_point(t),
                self.via_specific_humidity(quantity, t),
            ],
            Quantity::DewPoint => [
                self.via_specific_humidity(quantity, t),
                self.via_relative_humidity(t),
            ],
        };
        let mut refusal = None;
        for path in paths.into_iter().flatten() {
            match path {
                Ok(found) => return Ok(found),
                Err(why) => {
                    refusal.get_or_insert(why);
                }
            }
        }
        Err(refusal.unwrap_or_else(|| {
            match quantity {
                Quantity::RelativeHumidity => "no dew point or specific humidity at its level",
                Quantity::DewPoint => "no relative humidity or specific humidity at its level",
            }
            .to_string()
        }))
    }

    fn via_dew_point(&self, t: TemperatureUnit) -> Path<'a> {
        let td = self.td?;
        Some(
            self.ingredient(td, MoistureRole::DewPoint)
                .and_then(|()| unit(td, TemperatureUnit::of, "K or °C"))
                .map(|td_unit| {
                    (
                        Recipe::RelativeHumidityFromDewPoint { t, td: td_unit },
                        vec![self.t, td],
                    )
                }),
        )
    }

    fn via_relative_humidity(&self, t: TemperatureUnit) -> Path<'a> {
        let rh = self.rh?;
        Some(
            self.ingredient(rh, MoistureRole::RelativeHumidity)
                .and_then(|()| unit(rh, RelativeHumidityUnit::of, "% or 1"))
                .map(|rh_unit| {
                    (
                        Recipe::DewPointFromRelativeHumidity { t, rh: rh_unit },
                        vec![self.t, rh],
                    )
                }),
        )
    }

    /// Specific humidity gives a vapour pressure only on an isobaric level,
    /// whose value is the pressure.
    fn via_specific_humidity(&self, quantity: Quantity, t: TemperatureUnit) -> Path<'a> {
        let q = self.q?;
        let pressure = match self.t.vertical {
            Some(VerticalKind::Pressure) => Ok(()),
            Some(VerticalKind::ModelLevel) => Err(format!(
                "'{}' is on model levels, whose pressure needs the hybrid coefficients and the \
                 surface pressure",
                q.name
            )),
            _ => Err(format!(
                "'{}' is not on pressure levels, and a single level's pressure would be the \
                 surface pressure, which is not supported",
                q.name
            )),
        };
        Some(
            self.ingredient(q, MoistureRole::SpecificHumidity)
                .and(pressure)
                .and_then(|()| unit(q, SpecificHumidityUnit::of, "kg kg-1, g kg-1 or 1"))
                .map(|q_unit| {
                    let recipe = match quantity {
                        Quantity::RelativeHumidity => {
                            Recipe::RelativeHumidityFromSpecificHumidity { t, q: q_unit }
                        }
                        Quantity::DewPoint => Recipe::DewPointFromSpecificHumidity { t, q: q_unit },
                    };
                    (recipe, vec![self.t, q])
                }),
        )
    }

    /// Whether `p` is the ingredient `role` at this level.
    fn ingredient(&self, p: &ParameterFacts, role: MoistureRole) -> Result<(), String> {
        if role_of(p) != Some(role) {
            return Err(format!(
                "the source's metadata does not identify '{}' as {}",
                p.name,
                role.noun()
            ));
        }
        if !same_level(p, self.t) {
            return Err(format!(
                "'{}' and '{}' are at different levels",
                self.t.name, p.name
            ));
        }
        Ok(())
    }
}

/// The parameter's unit as `parse` reads it, or why not.
fn unit<U>(
    p: &ParameterFacts,
    parse: impl Fn(&str) -> Option<U>,
    expected: &str,
) -> Result<U, String> {
    if p.unit.trim().is_empty() {
        return Err(format!(
            "the unit of '{}' is not stated, or not read yet",
            p.name
        ));
    }
    parse(&p.unit)
        .ok_or_else(|| format!("the unit of '{}' is '{}', not {expected}", p.name, p.unit))
}
