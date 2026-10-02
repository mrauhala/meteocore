//! Wind speed and direction derived from u/v components (#897).
//!
//! Many NWP sources publish wind only as its two components. This module is
//! the one home for turning a u/v pair into speed and direction:
//!
//! - the formulas: [`speed`] and [`from_direction`];
//! - the frame rules: which way a source's components point
//!   ([`VectorFrame`]) and how its grid's axes lie on the ground
//!   ([`GridAxes`]) decide what may be derived ([`derivable`]);
//! - the pairing: which parameters form a u/v pair, and what the derived
//!   parameters are called in the source's own vocabulary ([`WindPlan`]).
//!
//! Engines only report what their source asserts about each parameter
//! ([`WindSource`] → [`WindFacts`]): the GRIB2 resolution-and-component flag,
//! a CF `standard_name`, an FMI parameter number. [`DerivedWind`] wraps an
//! engine's `MapEngine` + `EdrEngine` and serves the planned parameters.
//!
//! **Derive after sampling.** The components are sampled with the engine's
//! normal interpolation, at the output pixel or EDR point, and combined per
//! sample. A derived field is never interpolated: 359° and 1° would average
//! to 180°.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, LazyLock};

use crate::geo::Crs;

mod derived;
#[cfg(test)]
mod tests;

pub use derived::{DerivedWind, OutcomeLog};

/// CF standard name of a derived speed.
pub const WIND_SPEED: &str = "wind_speed";
/// CF standard name of a derived direction: where the wind blows **from**.
pub const WIND_FROM_DIRECTION: &str = "wind_from_direction";
/// Unit of a derived direction, degrees clockwise from north (QUDT `DEG`).
pub const DIRECTION_UNIT: &str = "°";

/// Wind speed `hypot(u, v)`, in the components' unit. `None` when either
/// component is missing or not finite.
#[inline]
pub fn speed(u: Option<f64>, v: Option<f64>) -> Option<f64> {
    let (u, v) = finite_pair(u, v)?;
    Some(u.hypot(v))
}

/// The direction the wind blows **from**, in degrees clockwise from north
/// in `[0, 360)`: `atan2(-u, -v)`. A northerly (`v < 0`) is 0°, an easterly
/// (`u < 0`) 90°. Earth-relative components only: grid-relative ones must
/// be turned to true north first. `None` when either component is missing
/// or not finite, and for calm (`u = v = 0`), which has no direction.
#[inline]
pub fn from_direction(u: Option<f64>, v: Option<f64>) -> Option<f64> {
    let (u, v) = finite_pair(u, v)?;
    if u == 0.0 && v == 0.0 {
        return None;
    }
    let degrees = (-u).atan2(-v).to_degrees().rem_euclid(360.0);
    // `rem_euclid` rounds a tiny negative angle up to exactly 360.
    Some(if degrees >= 360.0 { 0.0 } else { degrees })
}

fn finite_pair(u: Option<f64>, v: Option<f64>) -> Option<(f64, f64)> {
    match (u, v) {
        (Some(u), Some(v)) if u.is_finite() && v.is_finite() => Some((u, v)),
        _ => None,
    }
}

/// Which directions a source's u and v components are measured along, as
/// the source states it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum VectorFrame {
    /// Along east and north: GRIB2 flag table 3.3 bit 5 = 0 (GRIB1 table 7
    /// likewise), CF `eastward_wind` / `northward_wind`.
    Earth,
    /// Along the grid's increasing x and y: GRIB2 bit 5 = 1, CF `x_wind` /
    /// `y_wind` and their aliases `grid_eastward_wind` /
    /// `grid_northward_wind`.
    Grid,
    /// The source does not say, or its metadata has not been read yet.
    #[default]
    Unknown,
}

impl VectorFrame {
    /// Bit 5 of GRIB2 flag table 3.3 (resolution and component flags,
    /// Section 3) and of GRIB1 code table 7 (GDS octet 17). WMO numbers
    /// bits from the most significant, so bit 5 is `0x08`.
    pub const GRIB_GRID_RELATIVE: u8 = 0x08;

    /// The frame a GRIB resolution-and-component flags octet states.
    pub fn from_grib_flags(flags: u8) -> Self {
        if flags & Self::GRIB_GRID_RELATIVE != 0 {
            VectorFrame::Grid
        } else {
            VectorFrame::Earth
        }
    }

    /// The frame a CF `standard_name` states; [`VectorFrame::Unknown`] for
    /// anything that is not a horizontal wind component.
    pub fn from_standard_name(name: &str) -> Self {
        match name {
            "eastward_wind" | "northward_wind" => VectorFrame::Earth,
            "x_wind" | "y_wind" | "grid_eastward_wind" | "grid_northward_wind" => VectorFrame::Grid,
            _ => VectorFrame::Unknown,
        }
    }
}

/// How a source grid's x and y axes lie on the ground, which decides what
/// grid-relative components can give.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GridAxes {
    /// x points east and y north everywhere: a regular, unrotated lat/lon
    /// grid, or Mercator. The convergence angle is zero, so grid- and
    /// earth-relative components are the same thing.
    NorthAligned,
    /// The axes are orthogonal on the ground but turned from north by a
    /// convergence angle that varies across the grid: conformal projections
    /// (Lambert conformal conic, polar stereographic, transverse Mercator)
    /// and rotated lat/lon. Turning a vector keeps its length, so speed is
    /// right in either frame; direction needs the rotation.
    Rotated,
    /// Axes that are not orthogonal on the ground (equal-area and other
    /// non-conformal projections), or an unknown grid: the hypotenuse of
    /// grid-relative components is not the wind speed.
    Skewed,
}

impl GridAxes {
    /// The axes of a grid laid out in `crs`.
    pub fn of(crs: &Crs) -> Self {
        match crs {
            Crs::Wgs84 | Crs::WebMercator => GridAxes::NorthAligned,
            Crs::TransverseMercator { .. }
            | Crs::LambertConformalConic { .. }
            | Crs::Stereographic { .. }
            | Crs::RotatedLatLon { .. } => GridAxes::Rotated,
            Crs::LambertAzimuthalEqualArea { .. } | Crs::Geostationary { .. } => GridAxes::Skewed,
        }
    }
}

/// What a u/v pair can give ([`derivable`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Derivable {
    SpeedAndDirection,
    /// Speed only, with the reason there is no direction.
    SpeedOnly(&'static str),
    /// Nothing, with the reason.
    Nothing(&'static str),
}

/// The frame rules of #897, in one place:
///
/// 1. An unknown frame gives no direction. Speed is still right when the
///    grid's axes are orthogonal on the ground, since turning a vector keeps
///    its length.
/// 2. Components in different frames give nothing.
/// 3. Earth-relative components give speed and direction.
/// 4. Grid-relative components must be turned to true north by the grid's
///    convergence angle before a direction. On a north-aligned grid the
///    angle is zero. Rotation on other grids is not implemented yet, so
///    they give speed only, and a non-conformal grid gives nothing.
pub fn derivable(u: VectorFrame, v: VectorFrame, grid: GridAxes) -> Derivable {
    use VectorFrame::{Earth, Grid, Unknown};
    match (u, v) {
        (Earth, Earth) => Derivable::SpeedAndDirection,
        (Grid, Grid) => match grid {
            GridAxes::NorthAligned => Derivable::SpeedAndDirection,
            GridAxes::Rotated => Derivable::SpeedOnly(
                "u/v are grid-relative on a rotated grid and turning them to true north \
                 is not implemented",
            ),
            GridAxes::Skewed => {
                Derivable::Nothing("u/v are grid-relative on a grid that is not conformal")
            }
        },
        (Earth, Grid) | (Grid, Earth) => {
            Derivable::Nothing("u and v are relative to different frames")
        }
        (Unknown, _) | (_, Unknown) => match grid {
            GridAxes::NorthAligned | GridAxes::Rotated => Derivable::SpeedOnly(
                "whether u/v are earth- or grid-relative is unknown (not stated by the \
                 source, or not read yet)",
            ),
            GridAxes::Skewed => Derivable::Nothing(
                "whether u/v are earth- or grid-relative is unknown, and the grid is not \
                 conformal",
            ),
        },
    }
}

/// The quantity a parameter is, as far as wind derivation cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindRole {
    /// The eastward (or grid x) component.
    U,
    /// The northward (or grid y) component.
    V,
    Speed,
    /// Direction the wind blows from.
    Direction,
}

impl WindRole {
    /// WMO GRIB2 code table 4.2, discipline 0 category 2 (momentum).
    pub fn from_grib(triple: (u8, u8, u8)) -> Option<Self> {
        match triple {
            (0, 2, 0) => Some(WindRole::Direction),
            (0, 2, 1) => Some(WindRole::Speed),
            (0, 2, 2) => Some(WindRole::U),
            (0, 2, 3) => Some(WindRole::V),
            _ => None,
        }
    }

    /// A CF `standard_name`.
    pub fn from_standard_name(name: &str) -> Option<Self> {
        match name {
            "eastward_wind" | "x_wind" | "grid_eastward_wind" => Some(WindRole::U),
            "northward_wind" | "y_wind" | "grid_northward_wind" => Some(WindRole::V),
            WIND_SPEED => Some(WindRole::Speed),
            WIND_FROM_DIRECTION => Some(WindRole::Direction),
            _ => None,
        }
    }

    /// An FMI newbase parameter number (`NFmiParameterName.h`:
    /// `kFmiWindDirection = 20`, `kFmiWindSpeedMS`, `kFmiWindVectorMS`,
    /// `kFmiWindUMS`, `kFmiWindVMS`).
    pub fn from_fmi_param(id: u32) -> Option<Self> {
        match id {
            20 => Some(WindRole::Direction),
            21 => Some(WindRole::Speed),
            23 => Some(WindRole::U),
            24 => Some(WindRole::V),
            _ => None,
        }
    }
}

/// What a source asserts about one of its parameters. Engines fill in what
/// their format states and leave the rest at the [`ParameterFacts::new`]
/// defaults; nothing here is guessed from a name.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParameterFacts {
    /// The parameter's name on the map APIs (`RasterInfo::parameters`), and
    /// in EDR unless [`Self::edr_name`] says otherwise.
    pub name: String,
    /// Its EDR name, when that differs from `name`.
    pub edr_name: Option<String>,
    /// WMO GRIB `(discipline, category, number)`, when the source has one.
    pub grib: Option<(u8, u8, u8)>,
    /// CF `standard_name` attribute, when the source has one.
    pub standard_name: Option<String>,
    /// FMI newbase parameter number (QueryData).
    pub fmi_param: Option<u32>,
    /// For a vector component, the frame the source states.
    pub frame: VectorFrame,
    /// Identity of the level the parameter is at, when its level is part of
    /// the parameter (a single-level `10u`). A pair must share it. `None`
    /// for a parameter that spans the collection's vertical axis, or a
    /// source without levels.
    pub level: Option<String>,
    /// Human-readable level, appended to derived titles
    /// (`"10 m above ground"`).
    pub level_label: Option<String>,
    /// Unit of the served values; empty when unknown.
    pub unit: String,
}

impl ParameterFacts {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// The parameter's EDR name.
    pub fn edr_name(&self) -> &str {
        self.edr_name.as_deref().unwrap_or(&self.name)
    }

    /// The role the source's own metadata gives the parameter. `Err` when
    /// two pieces of metadata disagree.
    fn asserted_role(&self) -> Result<Option<WindRole>, ()> {
        let roles = [
            self.grib.and_then(WindRole::from_grib),
            self.standard_name
                .as_deref()
                .and_then(WindRole::from_standard_name),
            self.fmi_param.and_then(WindRole::from_fmi_param),
        ];
        let stated = [
            self.grib.is_some(),
            self.standard_name.is_some(),
            self.fmi_param.is_some(),
        ];
        let mut role = None;
        for (found, stated) in roles.into_iter().zip(stated) {
            if !stated {
                continue;
            }
            match (role, found) {
                (None, found) => role = Some(found),
                (Some(a), b) if a == b => {}
                _ => return Err(()),
            }
        }
        Ok(role.flatten())
    }

    /// The role from metadata, else from the vocabulary name.
    fn role(&self) -> Option<WindRole> {
        match self.asserted_role() {
            Ok(Some(role)) => Some(role),
            Ok(None) if self.has_metadata() => None,
            Ok(None) => vocabulary_role(&self.name),
            Err(()) => None,
        }
    }

    fn has_metadata(&self) -> bool {
        self.grib.is_some() || self.standard_name.is_some() || self.fmi_param.is_some()
    }
}

/// Everything a source asserts that wind derivation needs: one snapshot,
/// rebuilt at load and poll refresh (Critical Rule 10), never per request.
#[derive(Debug, Clone, PartialEq)]
pub struct WindFacts {
    pub grid: GridAxes,
    pub parameters: Vec<ParameterFacts>,
}

impl WindFacts {
    /// One shared empty snapshot: a source with nothing to report.
    pub fn none() -> Arc<WindFacts> {
        static NONE: LazyLock<Arc<WindFacts>> = LazyLock::new(|| {
            Arc::new(WindFacts {
                grid: GridAxes::Skewed,
                parameters: Vec::new(),
            })
        });
        NONE.clone()
    }
}

/// An engine that can say what its source asserts about its wind
/// components. Implemented by the engines [`DerivedWind`] wraps.
pub trait WindSource: Send + Sync {
    /// The current snapshot. **O(1)**: an `Arc` clone of a snapshot built
    /// when the engine loads or refreshes its metadata. [`DerivedWind`]
    /// rebuilds its plan only when the returned `Arc` changes.
    fn wind_facts(&self) -> Arc<WindFacts>;
}

/// One source vocabulary's u/v pair and the names its speed and direction
/// have in the same vocabulary.
struct Vocabulary {
    u: &'static str,
    v: &'static str,
    speed: &'static str,
    direction: &'static str,
}

/// The source vocabularies. Names are case-sensitive.
const VOCABULARY: &[Vocabulary] = &[
    // ECMWF GRIB short names (parameter database: 10si 207, 10wdir 260260,
    // 100si 228249, ws 10, wdir 3031).
    vocabulary("10u", "10v", "10si", "10wdir"),
    // `100wdir` is MeteoCore's name, not an ECMWF short name: ECMWF's
    // parameter database has no 100 m wind direction. It follows `10wdir`.
    vocabulary("100u", "100v", "100si", "100wdir"),
    vocabulary("u", "v", "ws", "wdir"),
    // ECMWF netCDF / CF variable names (ERA5).
    vocabulary("u10", "v10", "si10", "wdir10"),
    vocabulary("u100", "v100", "si100", "wdir100"),
    // NCEP wgrib2 abbreviations, at any level.
    vocabulary("UGRD", "VGRD", "WIND", "WDIR"),
    // FMI newbase names (QueryData).
    vocabulary("WindUMS", "WindVMS", "WindSpeedMS", "WindDirection"),
];

const fn vocabulary(
    u: &'static str,
    v: &'static str,
    speed: &'static str,
    direction: &'static str,
) -> Vocabulary {
    Vocabulary {
        u,
        v,
        speed,
        direction,
    }
}

fn vocabulary_role(name: &str) -> Option<WindRole> {
    VOCABULARY.iter().find_map(|w| {
        if name == w.u {
            Some(WindRole::U)
        } else if name == w.v {
            Some(WindRole::V)
        } else if name == w.speed {
            Some(WindRole::Speed)
        } else if name == w.direction {
            Some(WindRole::Direction)
        } else {
            None
        }
    })
}

/// The v partner and the derived names of a CF-identified u component
/// whose name is in no vocabulary: the store's own naming, one of
/// - a component phrase: `10m_u_component_of_wind` → `10m_wind_speed`,
///   `x_wind_10m` → `wind_speed_10m`;
/// - one `_`-separated `u` token: `wind_u_10m` → `wind_speed_10m`.
///
/// Direction is `…wind_direction…` or `…direction…` likewise.
fn cf_names(u: &str) -> Option<(String, String, String)> {
    const PHRASES: [(&str, &str); 4] = [
        ("u_component_of_wind", "v_component_of_wind"),
        ("grid_eastward_wind", "grid_northward_wind"),
        ("eastward_wind", "northward_wind"),
        ("x_wind", "y_wind"),
    ];
    for (u_phrase, v_phrase) in PHRASES {
        // Whole `_`-separated tokens only: `max_wind_u` holds no `x_wind`.
        let aligned = u.match_indices(u_phrase).find(|&(at, _)| {
            let end = at + u_phrase.len();
            (at == 0 || u.as_bytes()[at - 1] == b'_')
                && (end == u.len() || u.as_bytes()[end] == b'_')
        });
        if let Some((at, _)) = aligned {
            let swap = |with: &str| format!("{}{with}{}", &u[..at], &u[at + u_phrase.len()..]);
            return Some((swap(v_phrase), swap("wind_speed"), swap("wind_direction")));
        }
    }
    let tokens: Vec<&str> = u.split('_').collect();
    let mut at = tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| t.eq_ignore_ascii_case("u"));
    let (i, token) = at.next()?;
    if at.next().is_some() {
        return None;
    }
    let swap = |with: &str| {
        let mut t = tokens.clone();
        t[i] = with;
        t.join("_")
    };
    let v = if *token == "U" { "V" } else { "v" };
    Some((swap(v), swap("speed"), swap("direction")))
}

/// A derived quantity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DerivedKind {
    Speed,
    Direction,
}

impl DerivedKind {
    pub fn standard_name(self) -> &'static str {
        match self {
            DerivedKind::Speed => WIND_SPEED,
            DerivedKind::Direction => WIND_FROM_DIRECTION,
        }
    }
}

/// One planned derived parameter.
#[derive(Debug, Clone, PartialEq)]
pub struct DerivedParameter {
    /// Its name, the same on every API.
    pub name: String,
    pub kind: DerivedKind,
    /// The components' map names.
    pub u: String,
    pub v: String,
    /// The components' EDR names.
    pub edr_u: String,
    pub edr_v: String,
    pub title: String,
    /// The components' unit for speed, [`DIRECTION_UNIT`] for direction.
    pub unit: String,
}

impl DerivedParameter {
    /// This parameter's value from the component samples.
    #[inline]
    pub fn value(&self, u: Option<f64>, v: Option<f64>) -> Option<f64> {
        match self.kind {
            DerivedKind::Speed => speed(u, v),
            DerivedKind::Direction => from_direction(u, v),
        }
    }
}

/// What one u/v pair gave, for the load log: the derived name, or why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairOutcome {
    pub u: String,
    pub v: String,
    pub speed: Result<String, String>,
    pub direction: Result<String, String>,
}

impl fmt::Display for PairOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}: ", self.u, self.v)?;
        match (&self.speed, &self.direction) {
            (Ok(speed), Ok(direction)) => {
                write!(f, "derived speed '{speed}' and direction '{direction}'")
            }
            (Ok(speed), Err(why)) => {
                write!(f, "derived speed '{speed}' only; no direction: {why}")
            }
            (Err(why), Ok(direction)) => {
                write!(f, "derived direction '{direction}' only; no speed: {why}")
            }
            (Err(speed), Err(direction)) if speed == direction => {
                write!(f, "nothing derived: {speed}")
            }
            (Err(speed), Err(direction)) => write!(
                f,
                "nothing derived; no speed: {speed}; no direction: {direction}"
            ),
        }
    }
}

/// The parameters to derive for one source snapshot, and why each u/v pair
/// gave what it gave.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WindPlan {
    pub derived: Vec<DerivedParameter>,
    pub outcomes: Vec<PairOutcome>,
}

impl WindPlan {
    /// Pair the components and apply the frame, level, unit and
    /// already-present rules.
    ///
    /// A pair is a u and a v of one vocabulary (`10u`/`10v`, `UGRD`/`VGRD`,
    /// …), or CF-identified components (`standard_name`) named alike, with
    /// the same level and unit. A derived parameter is skipped when the
    /// collection already has that quantity at that level (by vocabulary
    /// name, GRIB 0/2/1 or 0/2/0, or CF `standard_name`), or when its name
    /// is taken. Speed and direction are independent.
    pub fn build(facts: &WindFacts) -> WindPlan {
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
        let mut claimed: HashSet<String> = HashSet::new();
        let mut plan = WindPlan::default();
        for u in &facts.parameters {
            let Some((v, speed_name, direction_name)) = partner(u, &by_name) else {
                continue;
            };
            let outcome = |speed, direction| PairOutcome {
                u: u.name.clone(),
                v: v.name.clone(),
                speed,
                direction,
            };
            let refuse = |why: String| outcome(Err(why.clone()), Err(why));
            if u.asserted_role().is_err()
                || v.asserted_role().is_err()
                || !matches!(u.role(), Some(WindRole::U))
                || !matches!(v.role(), Some(WindRole::V))
            {
                plan.outcomes.push(refuse(
                    "the source's metadata does not identify them as u and v wind components"
                        .into(),
                ));
                continue;
            }
            if u.level != v.level {
                plan.outcomes
                    .push(refuse("u and v are at different levels".into()));
                continue;
            }
            if u.unit != v.unit {
                plan.outcomes.push(refuse(format!(
                    "u and v have different units ('{}', '{}')",
                    u.unit, v.unit
                )));
                continue;
            }
            let (speed_ok, direction_ok) = match derivable(u.frame, v.frame, facts.grid) {
                Derivable::SpeedAndDirection => (Ok(()), Ok(())),
                Derivable::SpeedOnly(why) => (Ok(()), Err(why.to_string())),
                Derivable::Nothing(why) => (Err(why.to_string()), Err(why.to_string())),
            };
            let mut result = |kind: DerivedKind, name: String, allowed: Result<(), String>| {
                allowed?;
                let role = match kind {
                    DerivedKind::Speed => WindRole::Speed,
                    DerivedKind::Direction => WindRole::Direction,
                };
                if let Some(existing) = facts
                    .parameters
                    .iter()
                    .find(|p| p.role() == Some(role) && p.level == u.level)
                {
                    return Err(format!("the collection already has '{}'", existing.name));
                }
                if taken.contains(name.as_str()) {
                    return Err(format!("the name '{name}' is taken by another parameter"));
                }
                if !claimed.insert(name.clone()) {
                    return Err(format!("'{name}' is already derived from another pair"));
                }
                let (title, unit) = match kind {
                    DerivedKind::Speed => ("Wind speed", u.unit.clone()),
                    DerivedKind::Direction => ("Wind direction", DIRECTION_UNIT.to_string()),
                };
                let title = match &u.level_label {
                    Some(level) => format!("{title} ({level})"),
                    None => title.to_string(),
                };
                plan.derived.push(DerivedParameter {
                    name: name.clone(),
                    kind,
                    u: u.name.clone(),
                    v: v.name.clone(),
                    edr_u: u.edr_name().to_string(),
                    edr_v: v.edr_name().to_string(),
                    title,
                    unit,
                });
                Ok(name)
            };
            let speed = result(DerivedKind::Speed, speed_name, speed_ok);
            let direction = result(DerivedKind::Direction, direction_name, direction_ok);
            plan.outcomes.push(outcome(speed, direction));
        }
        plan
    }

    /// The derived parameter with this exact name.
    pub fn get(&self, name: &str) -> Option<&DerivedParameter> {
        self.derived.iter().find(|d| d.name == name)
    }

    /// The derived speed with this exact name: the only derived parameters
    /// the map APIs serve.
    pub fn map_parameter(&self, name: &str) -> Option<&DerivedParameter> {
        self.get(name).filter(|d| d.kind == DerivedKind::Speed)
    }

    /// The derived parameter an EDR `parameter-name` selects, matched
    /// case-insensitively like [`crate::edr_engine::select_parameters`].
    pub fn edr_parameter(&self, name: &str) -> Option<&DerivedParameter> {
        self.derived
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(name))
    }
}

/// The v partner of a u component and the names of the derived speed and
/// direction.
fn partner<'a>(
    u: &ParameterFacts,
    by_name: &HashMap<&str, &'a ParameterFacts>,
) -> Option<(&'a ParameterFacts, String, String)> {
    if let Some(w) = VOCABULARY.iter().find(|w| w.u == u.name) {
        return by_name
            .get(w.v)
            .map(|v| (*v, w.speed.to_string(), w.direction.to_string()));
    }
    // FMI parameter numbers, whatever a producer named them: newbase names.
    let fmi = |p: &ParameterFacts| p.fmi_param.and_then(WindRole::from_fmi_param);
    if fmi(u) == Some(WindRole::U) {
        let mut v = by_name.values().filter(|v| fmi(v) == Some(WindRole::V));
        return match (v.next(), v.next()) {
            (Some(v), None) => Some((*v, "WindSpeedMS".into(), "WindDirection".into())),
            _ => None,
        };
    }
    // A CF-identified pair, named in the store's own vocabulary.
    let cf_role = |p: &ParameterFacts| {
        p.standard_name
            .as_deref()
            .and_then(WindRole::from_standard_name)
    };
    if cf_role(u) != Some(WindRole::U) {
        return None;
    }
    let (v_name, speed, direction) = cf_names(&u.name)?;
    let v = by_name.get(v_name.as_str())?;
    (cf_role(v) == Some(WindRole::V)).then_some((*v, speed, direction))
}
