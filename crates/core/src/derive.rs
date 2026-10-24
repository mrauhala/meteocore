//! Parameters a collection serves derived from its own: wind speed and
//! direction from u/v components ([`crate::wind`], #897), geopotential
//! height from geopotential ([`crate::geopotential`], #948), and relative
//! humidity and dew point from their ingredients ([`crate::moisture`],
//! #898).
//!
//! One wrapper, [`Derived`], serves every derivation over an engine's
//! `MapEngine` + `EdrEngine`, so the time, run, level and cache-key rules
//! (root `CLAUDE.md`, adding-an-engine step 7) are written once. A
//! derivation contributes two things:
//!
//! - a planner from the engine's per-parameter facts snapshot
//!   ([`crate::wind::WindFacts`], named for the first derivation; it carries
//!   every parameter's GRIB triple, CF `standard_name`, level and served
//!   unit) to [`DerivedParameter`]s, with an [`Outcome`] per candidate for
//!   the load log;
//! - a [`Formula`] that combines its input samples into one value.
//!
//! Engines only report facts; they never compute a derived field. A derived
//! value is computed **after sampling**, from inputs sampled at the output
//! pixel or EDR point with the engine's own interpolation and the same
//! request (bbox, size, time, z, run), never interpolated itself.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use crate::geopotential::{HeightOutcome, HeightPlan};
use crate::moisture::{MoistureOutcome, MoisturePlan, Recipe};
use crate::wind::{DerivedKind, PairOutcome, WindFacts, WindPlan};

#[cfg(test)]
mod tests;
mod wrapper;

pub use wrapper::Derived;

/// Which derivations a collection serves. Each is a per-collection opt-out
/// in config (`derive_wind`, `derive_geopotential_height`,
/// `derive_humidity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Derivations {
    /// Wind speed and direction from u/v components.
    pub wind: bool,
    /// Geopotential height from geopotential.
    pub geopotential_height: bool,
    /// Relative humidity and dew point from their ingredients.
    pub humidity: bool,
}

impl Derivations {
    /// Wind only: what #897 derived.
    pub const WIND: Derivations = Derivations {
        wind: true,
        geopotential_height: false,
        humidity: false,
    };
    /// Every derivation.
    pub const ALL: Derivations = Derivations {
        wind: true,
        geopotential_height: true,
        humidity: true,
    };

    /// Whether any derivation is on.
    pub fn any(self) -> bool {
        self.wind || self.geopotential_height || self.humidity
    }
}

/// How a derived parameter's value follows from its inputs' samples, in
/// [`DerivedParameter::inputs`] order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Formula {
    /// `hypot(u, v)` ([`crate::wind::speed`]); inputs u, v.
    WindSpeed,
    /// Where the wind blows from ([`crate::wind::from_direction`]); inputs
    /// u, v.
    WindDirection,
    /// Φ / g₀ ([`crate::geopotential::height`]); input geopotential.
    GeopotentialHeight,
    /// Relative humidity or dew point ([`Recipe::value`]); inputs
    /// temperature and a dew point, relative humidity or specific humidity,
    /// then, for specific humidity, the level's pressure in Pa, which the
    /// wrapper appends ([`Formula::reads_level_pressure`]).
    Moisture(Recipe),
}

impl Formula {
    /// The value from the input samples, `None` when one is missing.
    #[inline]
    pub fn value(self, inputs: &[Option<f64>]) -> Option<f64> {
        match (self, inputs) {
            (Formula::WindSpeed, &[u, v]) => crate::wind::speed(u, v),
            (Formula::WindDirection, &[u, v]) => crate::wind::from_direction(u, v),
            (Formula::GeopotentialHeight, &[z]) => crate::geopotential::height(z),
            (Formula::Moisture(recipe), samples) => recipe.value(samples),
            _ => None,
        }
    }

    /// The CF standard name of the quantity computed: asserted by
    /// construction.
    pub fn standard_name(self) -> &'static str {
        match self {
            Formula::WindSpeed => crate::wind::WIND_SPEED,
            Formula::WindDirection => crate::wind::WIND_FROM_DIRECTION,
            Formula::GeopotentialHeight => crate::geopotential::GEOPOTENTIAL_HEIGHT,
            Formula::Moisture(recipe) => recipe.quantity().standard_name(),
        }
    }

    /// Whether its last sample is the pressure of the level its inputs are
    /// read at, in Pa, which the wrapper appends: from the request's
    /// isobaric level on a map, from the coverage's `z` in EDR.
    pub fn reads_level_pressure(self) -> bool {
        matches!(self, Formula::Moisture(recipe) if recipe.reads_level_pressure())
    }

    /// Whether the map APIs serve it: a scalar a palette can draw. A
    /// direction needs a cyclic palette and is EDR only.
    pub fn on_map(self) -> bool {
        !matches!(self, Formula::WindDirection)
    }

    /// The derivation it belongs to, for messages: `"wind"`.
    pub fn quantity(self) -> &'static str {
        match self {
            Formula::WindSpeed | Formula::WindDirection => "wind",
            Formula::GeopotentialHeight => "geopotential height",
            Formula::Moisture(recipe) => recipe.quantity().noun(),
        }
    }

    /// What one input is called in messages: `"wind component"`.
    fn input_noun(self) -> &'static str {
        match self {
            Formula::WindSpeed | Formula::WindDirection => "wind component",
            Formula::GeopotentialHeight => "geopotential",
            Formula::Moisture(_) => "humidity input",
        }
    }
}

impl From<DerivedKind> for Formula {
    fn from(kind: DerivedKind) -> Self {
        match kind {
            DerivedKind::Speed => Formula::WindSpeed,
            DerivedKind::Direction => Formula::WindDirection,
        }
    }
}

/// One planned derived parameter.
#[derive(Debug, Clone, PartialEq)]
pub struct DerivedParameter {
    /// Its name, the same on every API.
    pub name: String,
    pub formula: Formula,
    /// The inputs' map names, in the formula's order.
    pub inputs: Vec<String>,
    /// The inputs' EDR names, in the same order.
    pub edr_inputs: Vec<String>,
    pub title: String,
    pub unit: String,
}

impl DerivedParameter {
    /// This parameter's value from its input samples.
    #[inline]
    pub fn value(&self, inputs: &[Option<f64>]) -> Option<f64> {
        self.formula.value(inputs)
    }

    /// The input names quoted for a message: `'10u' and '10v'`.
    fn quoted(names: &[String]) -> String {
        let quoted: Vec<String> = names.iter().map(|n| format!("'{n}'")).collect();
        match quoted.split_last() {
            Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
            _ => quoted.concat(),
        }
    }
}

/// What one derivation candidate gave, for the load log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A u/v pair.
    Wind(PairOutcome),
    /// A geopotential.
    GeopotentialHeight(HeightOutcome),
    /// A temperature's level, with its humidity ingredients.
    Moisture(MoistureOutcome),
}

impl Outcome {
    /// What the outcome is about: one per candidate, so a changed outcome
    /// replaces the earlier one in the log.
    fn key(&self) -> (&'static str, &str, &str) {
        match self {
            Outcome::Wind(pair) => ("wind", &pair.u, &pair.v),
            Outcome::GeopotentialHeight(h) => ("geopotential height", &h.geopotential, ""),
            Outcome::Moisture(m) => ("humidity", &m.temperature, ""),
        }
    }
}

impl fmt::Display for Outcome {
    /// `wind 10u/10v: derived speed '10si' and direction '10wdir'`,
    /// `geopotential height z: derived 'gh'`, `humidity 2t: derived
    /// relative humidity '2r' from '2t' and '2d'; no dew point: the
    /// collection already has '2d'`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Wind(pair) => write!(f, "wind {pair}"),
            Outcome::GeopotentialHeight(height) => write!(f, "geopotential height {height}"),
            Outcome::Moisture(moisture) => write!(f, "humidity {moisture}"),
        }
    }
}

/// Reports one candidate's outcome: `(collection id, outcome)`. Called for
/// every candidate when the wrapper is built, then only when its outcome
/// changes (a refreshed snapshot that now states a frame or a unit, a new
/// run without an input). ds-core has no logging dependency; the server
/// logs.
pub type DerivationLog = Arc<dyn Fn(&str, &Outcome) + Send + Sync>;

/// The parameters to derive for one source snapshot, and every candidate's
/// outcome.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Plan {
    pub derived: Vec<DerivedParameter>,
    pub outcomes: Vec<Outcome>,
}

impl Plan {
    /// Plan every derivation `derive` turns on, wind first, then
    /// geopotential height, then humidity: a name one derivation takes is
    /// taken for the next.
    pub fn build(facts: &WindFacts, derive: Derivations) -> Plan {
        let mut plan = Plan::default();
        if derive.wind {
            let wind = WindPlan::build(facts);
            plan.derived
                .extend(wind.derived.into_iter().map(|d| DerivedParameter {
                    name: d.name,
                    formula: d.kind.into(),
                    inputs: vec![d.u, d.v],
                    edr_inputs: vec![d.edr_u, d.edr_v],
                    title: d.title,
                    unit: d.unit,
                }));
            plan.outcomes
                .extend(wind.outcomes.into_iter().map(Outcome::Wind));
        }
        if derive.geopotential_height {
            let claimed: HashSet<String> = plan.derived.iter().map(|d| d.name.clone()).collect();
            let heights = HeightPlan::build(facts, &claimed);
            plan.derived
                .extend(heights.derived.into_iter().map(|h| DerivedParameter {
                    name: h.name,
                    formula: Formula::GeopotentialHeight,
                    inputs: vec![h.geopotential],
                    edr_inputs: vec![h.edr_geopotential],
                    title: h.title,
                    unit: crate::geopotential::HEIGHT_UNIT.to_string(),
                }));
            plan.outcomes.extend(
                heights
                    .outcomes
                    .into_iter()
                    .map(Outcome::GeopotentialHeight),
            );
        }
        if derive.humidity {
            let claimed: HashSet<String> = plan.derived.iter().map(|d| d.name.clone()).collect();
            let moisture = MoisturePlan::build(facts, &claimed);
            plan.derived
                .extend(moisture.derived.into_iter().map(|m| DerivedParameter {
                    name: m.name,
                    formula: Formula::Moisture(m.recipe),
                    inputs: m.inputs,
                    edr_inputs: m.edr_inputs,
                    title: m.title,
                    unit: m.unit,
                }));
            plan.outcomes
                .extend(moisture.outcomes.into_iter().map(Outcome::Moisture));
        }
        plan
    }

    /// The derived parameter with this exact name.
    pub fn get(&self, name: &str) -> Option<&DerivedParameter> {
        self.derived.iter().find(|d| d.name == name)
    }

    /// The derived parameter with this exact name that the map APIs serve.
    pub fn map_parameter(&self, name: &str) -> Option<&DerivedParameter> {
        self.get(name).filter(|d| d.formula.on_map())
    }

    /// The derived parameter an EDR `parameter-name` selects, matched
    /// case-insensitively like [`crate::edr_engine::select_parameters`].
    pub fn edr_parameter(&self, name: &str) -> Option<&DerivedParameter> {
        self.derived
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(name))
    }
}
