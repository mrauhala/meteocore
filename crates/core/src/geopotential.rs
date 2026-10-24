//! Geopotential height derived from geopotential (#948).
//!
//! Geopotential Φ (m² s⁻²) and geopotential height Z = Φ / g₀ (gpm) are two
//! quantities, not one quantity in two units: WMO GRIB2 numbers them 0/3/4
//! and 0/3/5, CF names them `geopotential` and `geopotential_height`. An
//! engine serves geopotential as its source stores it, and a collection with
//! geopotential but no geopotential height at a level gets the height
//! derived, the way [`crate::wind`] derives wind speed:
//!
//! - the formula: [`height`], Φ / [`STANDARD_GRAVITY`];
//! - the identification: what the source's metadata states, a GRIB triple
//!   or a CF `standard_name` ([`HeightRole`]), never a name alone (`z` is as
//!   often a vertical coordinate), and a unit the source states as m² s⁻²;
//! - the naming: the height is named in the source's own vocabulary
//!   ([`HeightPlan`]): ECMWF `z` → `gh`, wgrib2 `GP` → `HGT`, a CF variable
//!   by inserting `height` after its `geopotential` token
//!   (`geopotential_pl` → `geopotential_height_pl`).
//!
//! Engines report through the per-parameter facts wind derivation reads
//! ([`crate::wind::WindFacts`]); [`crate::derive::Derived`] serves the plan.
//! A single-level collection's surface geopotential gives the surface's
//! geopotential height (terrain height in gpm) by the same rule.
//!
//! **Derive after sampling.** Geopotential is sampled with the engine's
//! normal interpolation and each sample divided by g₀. Division by a
//! constant commutes with linear interpolation, so this is also what an
//! interpolated height field would give; the rule is kept for the same
//! wrapper path every derivation takes.

use std::collections::HashSet;
use std::fmt;

use crate::wind::{ParameterFacts, WindFacts};

/// WMO's standard acceleration of gravity g₀, m s⁻²: one geopotential
/// metre is 9.80665 m² s⁻² of geopotential (WMO-No. 8, the definition of
/// the gpm).
pub const STANDARD_GRAVITY: f64 = 9.80665;

/// CF standard name of geopotential.
pub const GEOPOTENTIAL: &str = "geopotential";
/// CF standard name of a derived geopotential height.
pub const GEOPOTENTIAL_HEIGHT: &str = "geopotential_height";
/// Unit of a derived geopotential height: geopotential metres, as WMO Code
/// Table 4.2 gives 0/3/5 and engine-grib serves it.
pub const HEIGHT_UNIT: &str = "gpm";
/// Unit geopotential must be served in to give a height, in its canonical
/// spelling ([`crate::units::canonical_unit`]).
const GEOPOTENTIAL_UNIT: &str = "m2 s-2";

/// Geopotential height Φ / g₀, in gpm, from geopotential in m² s⁻². `None`
/// when the sample is missing or not finite.
#[inline]
pub fn height(geopotential: Option<f64>) -> Option<f64> {
    geopotential
        .filter(|z| z.is_finite())
        .map(|z| z / STANDARD_GRAVITY)
}

/// The quantity a parameter is, as far as height derivation cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeightRole {
    Geopotential,
    Height,
}

impl HeightRole {
    /// WMO GRIB2 code table 4.2, discipline 0 category 3 (mass).
    pub fn from_grib(triple: (u8, u8, u8)) -> Option<Self> {
        match triple {
            (0, 3, 4) => Some(HeightRole::Geopotential),
            (0, 3, 5) => Some(HeightRole::Height),
            _ => None,
        }
    }

    /// A CF `standard_name`.
    pub fn from_standard_name(name: &str) -> Option<Self> {
        match name {
            GEOPOTENTIAL => Some(HeightRole::Geopotential),
            GEOPOTENTIAL_HEIGHT => Some(HeightRole::Height),
            _ => None,
        }
    }
}

/// The role the source's metadata states. `Err` when the GRIB triple and
/// the CF `standard_name` disagree; `Ok(None)` when neither says.
fn asserted_role(p: &ParameterFacts) -> Result<Option<HeightRole>, ()> {
    let grib = p.grib.map(HeightRole::from_grib);
    let cf = p
        .standard_name
        .as_deref()
        .map(HeightRole::from_standard_name);
    match (grib, cf) {
        (Some(a), Some(b)) if a != b => Err(()),
        (Some(role), _) | (None, Some(role)) => Ok(role),
        (None, None) => Ok(None),
    }
}

/// Whether any of the parameter's metadata calls it geopotential.
fn states_geopotential(p: &ParameterFacts) -> bool {
    p.grib.and_then(HeightRole::from_grib) == Some(HeightRole::Geopotential)
        || p.standard_name
            .as_deref()
            .and_then(HeightRole::from_standard_name)
            == Some(HeightRole::Geopotential)
}

/// Whether the parameter is geopotential height: by its metadata, or by
/// its vocabulary name when it has none. A name is enough to count as an
/// existing height (deriving one more would duplicate it), never to derive.
fn is_height(p: &ParameterFacts) -> bool {
    match asserted_role(p) {
        Ok(Some(role)) => role == HeightRole::Height,
        Ok(None) if p.grib.is_none() && p.standard_name.is_none() => {
            VOCABULARY.iter().any(|&(_, height)| height == p.name)
        }
        _ => false,
    }
}

/// Source vocabularies: a geopotential name and its geopotential height's
/// name. Names are case-sensitive.
const VOCABULARY: &[(&str, &str)] = &[
    // ECMWF GRIB short names (parameter database: z 129, gh 156), also the
    // ECMWF netCDF variable names (ERA5).
    ("z", "gh"),
    // NCEP wgrib2 abbreviations (0/3/4 GP, 0/3/5 HGT).
    ("GP", "HGT"),
];

/// The name of the height derived from geopotential `p`, in its source's
/// vocabulary: a vocabulary name, else, for a CF-identified variable, its
/// own name with `height` after its `geopotential` token
/// (`geopotential_pl` → `geopotential_height_pl`).
fn height_name(p: &ParameterFacts) -> Option<String> {
    if let Some(&(_, height)) = VOCABULARY.iter().find(|&&(z, _)| z == p.name) {
        return Some(height.to_string());
    }
    if p.standard_name.as_deref() != Some(GEOPOTENTIAL) {
        return None;
    }
    let mut tokens: Vec<&str> = p.name.split('_').collect();
    let at = (0..tokens.len())
        .find(|&i| tokens[i] == "geopotential" && tokens.get(i + 1) != Some(&"height"))?;
    tokens.insert(at + 1, "height");
    Some(tokens.join("_"))
}

/// One planned geopotential height.
#[derive(Debug, Clone, PartialEq)]
pub struct HeightParameter {
    /// Its name, the same on every API.
    pub name: String,
    /// The geopotential's map name.
    pub geopotential: String,
    /// The geopotential's EDR name.
    pub edr_geopotential: String,
    pub title: String,
}

/// What one geopotential gave, for the load log: the derived name, or why
/// not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeightOutcome {
    pub geopotential: String,
    pub height: Result<String, String>,
}

impl fmt::Display for HeightOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.height {
            Ok(height) => write!(f, "{}: derived '{height}'", self.geopotential),
            Err(why) => write!(f, "{}: nothing derived: {why}", self.geopotential),
        }
    }
}

/// The heights to derive for one source snapshot, and why each
/// geopotential gave what it gave.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HeightPlan {
    pub derived: Vec<HeightParameter>,
    pub outcomes: Vec<HeightOutcome>,
}

impl HeightPlan {
    /// One height per geopotential the source's metadata identifies (GRIB
    /// 0/3/4, CF `geopotential`) in m² s⁻². A height is skipped when the
    /// collection already has geopotential height at that level (GRIB
    /// 0/3/5, CF `geopotential_height`, or a vocabulary name such as `gh`
    /// on a parameter without metadata), when its name is a parameter's or
    /// one in `claimed` (derived by another derivation), or when the
    /// source's vocabulary has no name for it.
    pub fn build(facts: &WindFacts, claimed: &HashSet<String>) -> HeightPlan {
        let taken: HashSet<&str> = facts
            .parameters
            .iter()
            .flat_map(|p| [p.name.as_str(), p.edr_name()])
            .collect();
        let mut planned: HashSet<String> = HashSet::new();
        let mut plan = HeightPlan::default();
        for z in &facts.parameters {
            let height = match asserted_role(z) {
                Ok(Some(HeightRole::Geopotential)) => Self::plan_one(
                    facts,
                    z,
                    |name| taken.contains(name) || claimed.contains(name),
                    &mut planned,
                ),
                Err(()) if states_geopotential(z) => Err(
                    "its GRIB parameter and CF standard_name disagree on what it is".to_string(),
                ),
                _ => continue,
            };
            if let Ok(d) = &height {
                plan.derived.push(d.clone());
            }
            plan.outcomes.push(HeightOutcome {
                geopotential: z.name.clone(),
                height: height.map(|d| d.name),
            });
        }
        plan
    }

    fn plan_one(
        facts: &WindFacts,
        z: &ParameterFacts,
        taken: impl Fn(&str) -> bool,
        planned: &mut HashSet<String>,
    ) -> Result<HeightParameter, String> {
        if z.unit.trim().is_empty() {
            return Err("its unit is not stated, or not read yet".into());
        }
        if !crate::units::same_unit(&z.unit, GEOPOTENTIAL_UNIT) {
            return Err(format!("its unit is '{}', not m² s⁻²", z.unit));
        }
        if let Some(existing) = facts
            .parameters
            .iter()
            .find(|p| is_height(p) && p.level == z.level)
        {
            return Err(format!("the collection already has '{}'", existing.name));
        }
        let name = height_name(z)
            .ok_or_else(|| "the source's vocabulary has no name for its height".to_string())?;
        if taken(&name) {
            return Err(format!("the name '{name}' is taken by another parameter"));
        }
        if !planned.insert(name.clone()) {
            return Err(format!(
                "'{name}' is already derived from another parameter"
            ));
        }
        let title = match &z.level_label {
            Some(level) => format!("Geopotential height ({level})"),
            None => "Geopotential height".to_string(),
        };
        Ok(HeightParameter {
            name,
            geopotential: z.name.clone(),
            edr_geopotential: z.edr_name().to_string(),
            title,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wind::GridAxes;

    fn facts(parameters: Vec<ParameterFacts>) -> WindFacts {
        WindFacts {
            grid: GridAxes::NorthAligned,
            parameters,
        }
    }

    fn grib(name: &str, triple: (u8, u8, u8), unit: &str, level: Option<&str>) -> ParameterFacts {
        ParameterFacts {
            grib: Some(triple),
            unit: unit.into(),
            level: level.map(String::from),
            level_label: level.map(|_| "surface".into()),
            ..ParameterFacts::new(name)
        }
    }

    fn cf(name: &str, standard_name: Option<&str>, unit: &str) -> ParameterFacts {
        ParameterFacts {
            standard_name: standard_name.map(String::from),
            unit: unit.into(),
            ..ParameterFacts::new(name)
        }
    }

    fn build(parameters: Vec<ParameterFacts>) -> HeightPlan {
        HeightPlan::build(&facts(parameters), &HashSet::new())
    }

    fn outcomes(plan: &HeightPlan) -> Vec<String> {
        plan.outcomes.iter().map(ToString::to_string).collect()
    }

    /// g₀ is the gpm's definition: 9806.65 m² s⁻² is 1000 gpm. ECMWF's
    /// 500 hPa geopotential of 57 600 m² s⁻² is about 5874 gpm.
    #[test]
    fn height_divides_by_standard_gravity() {
        let close = |z: f64, want: f64| (height(Some(z)).unwrap() - want).abs() < 1e-9;
        assert!(close(9806.65, 1000.0));
        assert!(close(57_600.0, 57_600.0 / 9.80665));
        assert!((height(Some(57_600.0)).unwrap() - 5873.6).abs() < 0.1);
        assert_eq!(height(Some(0.0)), Some(0.0));
        assert!(close(-490.3325, -50.0), "below sea level");
        for missing in [None, Some(f64::NAN), Some(f64::INFINITY)] {
            assert_eq!(height(missing), None, "{missing:?}");
        }
    }

    #[test]
    fn roles_come_from_the_grib_triple_and_the_cf_standard_name() {
        assert_eq!(
            HeightRole::from_grib((0, 3, 4)),
            Some(HeightRole::Geopotential)
        );
        assert_eq!(HeightRole::from_grib((0, 3, 5)), Some(HeightRole::Height));
        assert_eq!(HeightRole::from_grib((0, 3, 6)), None);
        assert_eq!(
            HeightRole::from_grib((10, 3, 4)),
            None,
            "not meteorological"
        );
        assert_eq!(
            HeightRole::from_standard_name("geopotential"),
            Some(HeightRole::Geopotential)
        );
        assert_eq!(
            HeightRole::from_standard_name("geopotential_height"),
            Some(HeightRole::Height)
        );
        assert_eq!(HeightRole::from_standard_name("surface_geopotential"), None);
    }

    /// ECMWF pressure levels without `gh` (AIFS publishes both, IFS open
    /// data `gh` only): `z` gives `gh`, its level being the request's.
    #[test]
    fn ecmwf_z_gives_gh() {
        let plan = build(vec![
            grib("z", (0, 3, 4), "m2 s-2", None),
            grib("t", (0, 0, 0), "°C", None),
        ]);
        assert_eq!(
            plan.derived,
            [HeightParameter {
                name: "gh".into(),
                geopotential: "z".into(),
                edr_geopotential: "z".into(),
                title: "Geopotential height".into(),
            }]
        );
        assert_eq!(outcomes(&plan), ["z: derived 'gh'"]);
    }

    /// The surface geopotential of a single-level collection gives the
    /// surface's height, terrain in gpm, by the same rule.
    #[test]
    fn surface_z_gives_the_surface_height() {
        let plan = build(vec![grib("z", (0, 3, 4), "m2 s-2", Some("sfc"))]);
        assert_eq!(plan.derived[0].name, "gh");
        assert_eq!(plan.derived[0].title, "Geopotential height (surface)");
        // wgrib2's abbreviations, NCEP's vocabulary.
        let plan = build(vec![grib("GP", (0, 3, 4), "m2 s-2", Some("sfc"))]);
        assert_eq!(plan.derived[0].name, "HGT");
    }

    /// A height at the same level is not derived again; at another level it
    /// is no duplicate, but its name is taken.
    #[test]
    fn an_existing_height_at_the_level_is_not_derived_again() {
        let plan = build(vec![
            grib("z", (0, 3, 4), "m2 s-2", None),
            grib("gh", (0, 3, 5), "gpm", None),
        ]);
        assert!(plan.derived.is_empty());
        assert_eq!(
            outcomes(&plan),
            ["z: nothing derived: the collection already has 'gh'"]
        );
        // GFS ships HGT natively.
        let plan = build(vec![
            grib("GP", (0, 3, 4), "m2 s-2", None),
            grib("HGT", (0, 3, 5), "gpm", None),
        ]);
        assert!(plan.derived.is_empty());
        // A height by name only, without metadata, still counts.
        let plan = build(vec![
            grib("z", (0, 3, 4), "m2 s-2", None),
            ParameterFacts::new("gh"),
        ]);
        assert!(plan.derived.is_empty());
        let plan = build(vec![
            grib("z", (0, 3, 4), "m2 s-2", Some("sfc")),
            grib("gh", (0, 3, 5), "gpm", Some("pl:500")),
        ]);
        assert!(plan.derived.is_empty());
        assert_eq!(
            outcomes(&plan),
            ["z: nothing derived: the name 'gh' is taken by another parameter"]
        );
        let claimed = HashSet::from(["gh".to_string()]);
        let plan = HeightPlan::build(&facts(vec![grib("z", (0, 3, 4), "m2 s-2", None)]), &claimed);
        assert!(plan.derived.is_empty());
    }

    /// Only metadata identifies geopotential, and only in m² s⁻²: a `z`
    /// without metadata may be a height coordinate, and a geopotential in
    /// another unit (or one not read yet) is not divided.
    #[test]
    fn geopotential_needs_metadata_and_its_unit() {
        assert!(build(vec![ParameterFacts {
            unit: "m2 s-2".into(),
            ..ParameterFacts::new("z")
        }])
        .outcomes
        .is_empty());
        let plan = build(vec![grib("z", (0, 3, 4), "gpm", None)]);
        assert_eq!(
            outcomes(&plan),
            ["z: nothing derived: its unit is 'gpm', not m² s⁻²"]
        );
        let plan = build(vec![grib("z", (0, 3, 4), "", None)]);
        assert_eq!(
            outcomes(&plan),
            ["z: nothing derived: its unit is not stated, or not read yet"]
        );
        let plan = build(vec![cf("geopotential", Some("geopotential"), "m^2/s^2")]);
        assert_eq!(
            outcomes(&plan),
            ["geopotential: derived 'geopotential_height'"]
        );
        let plan = build(vec![ParameterFacts {
            standard_name: Some("geopotential_height".into()),
            ..grib("z", (0, 3, 4), "m2 s-2", None)
        }]);
        assert!(plan.derived.is_empty());
        assert!(outcomes(&plan)[0].contains("disagree"));
        // A triple of another quantity: nothing to say.
        assert!(build(vec![grib("z", (0, 0, 0), "m2 s-2", None)])
            .outcomes
            .is_empty());
    }

    /// CF variables are named in the store's vocabulary: `height` after
    /// the `geopotential` token, as MEPS names its pressure levels.
    #[test]
    fn cf_geopotential_is_named_in_the_store_vocabulary() {
        for (name, height) in [
            ("geopotential_pl", Some("geopotential_height_pl")),
            ("geopotential", Some("geopotential_height")),
            (
                "mean_geopotential_500",
                Some("mean_geopotential_height_500"),
            ),
            // ERA5's netCDF: the ECMWF vocabulary.
            ("z", Some("gh")),
            ("phi", None),
            ("geopotentials", None),
        ] {
            let plan = build(vec![cf(name, Some("geopotential"), "m2 s-2")]);
            assert_eq!(
                plan.derived.first().map(|d| d.name.as_str()),
                height,
                "{name}"
            );
        }
        let plan = build(vec![cf("phi", Some("geopotential"), "m2 s-2")]);
        assert_eq!(
            outcomes(&plan),
            ["phi: nothing derived: the source's vocabulary has no name for its height"]
        );
        // A native CF height at the level wins.
        let plan = build(vec![
            cf("geopotential_pl", Some("geopotential"), "m^2/s^2"),
            cf("geopotential_height_pl", Some("geopotential_height"), "m"),
        ]);
        assert!(plan.derived.is_empty());
    }
}
