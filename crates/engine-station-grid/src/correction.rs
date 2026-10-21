//! Corrections of a station grid (#1206): lapse-rate correction over a
//! terrain model, a land/sea mask and station exclusions.
//!
//! - **Lapse rate.** A corrected parameter's station values are reduced to
//!   sea level, `v₀ = v − Γ·z_station`, interpolated, and each cell gets
//!   `Γ·z_cell` back, Γ in K/km. A station's height is the one its source
//!   states (`elevation` in the station inventory), else the terrain at its
//!   position; with neither it is left out of that parameter's slice and
//!   logged once. A cell over terrain nodata is nodata: a corrected value
//!   needs a height. Γ defaults to −6.5 K/km only where the source's
//!   parameter metadata identifies air temperature ([`identifies_air_temperature`]),
//!   never from a parameter's name.
//! - **Mask.** Cells outside the mask are nodata in every parameter that
//!   does not opt out, before the grid is cached, so maps and EDR read the
//!   same masked grid.
//! - **Exclusions.** Stations of the inventory matching `exclude_where` are
//!   left out of every slice.
//!
//! The terrain and the mask are sampled once per engine by the poll loop;
//! until both are, the engine publishes no analysis time, so nothing
//! uncorrected or unmasked is ever computed or served. The inventory is read
//! by the poll whenever the source's data version moves. A slice's
//! fingerprint covers the stations left out and the heights used, so a
//! changed exclusion or height is a revision like a late report.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};

use arc_swap::ArcSwap;
use ds_core::config::StationGridConfig;
use ds_core::error::DataServerError;
use ds_core::feature_engine::FeatureEngine;
use ds_core::map_engine::MapEngine;
use ds_core::model::ParameterDescription;
use ds_core::units::canonical_unit;
use tracing::{debug, info, warn};

use crate::geometry::GridGeom;
use crate::inventory::{self, Exclusions, Inventory, Key};
use crate::rasters::{dem_grid, Dem, DemGrid, Kind, RasterSource};
use crate::source::Slice;

/// The standard atmosphere's temperature lapse rate, K/km: the default for
/// a parameter whose source identifies it as air temperature.
pub const STANDARD_LAPSE_RATE: f64 = -6.5;
/// The largest lapse rate accepted, K/km either way.
pub const MAX_LAPSE_RATE: f64 = 50.0;
/// A stated station height this far from the terrain's is logged, metres.
pub const HEIGHT_DISAGREEMENT_M: f64 = 100.0;
/// Stations logged one by one per kind and engine, at most: a moving
/// station (a ship) is at a new position every report.
const MAX_LOGGED_STATIONS: usize = 256;

/// The stations of one kind already logged, bounded.
#[derive(Default)]
struct Logged(HashSet<Key>);

impl Logged {
    /// `Some(true)` the first time a station is seen, `Some(false)` once
    /// when the bound is reached, `None` otherwise.
    fn first(&mut self, key: Key) -> Option<bool> {
        if self.0.contains(&key) || self.0.len() > MAX_LOGGED_STATIONS {
            return None;
        }
        let under = self.0.len() < MAX_LOGGED_STATIONS;
        self.0.insert(key);
        Some(under)
    }
}

/// The other collections a station grid's corrections read, resolved by the
/// server from the config's ids.
#[derive(Default, Clone)]
pub struct Auxiliary {
    /// The `elevation_source` collection's map engine.
    pub elevation: Option<Arc<dyn MapEngine>>,
    /// The `land_mask_source` collection's map engine.
    pub land_mask: Option<Arc<dyn MapEngine>>,
    /// The source's station inventory: stated heights and the properties
    /// `exclude_where` reads.
    pub stations: Option<Arc<dyn FeatureEngine>>,
}

/// Whether a source's own metadata says a parameter is air temperature: its
/// CF standard name, or an observed property it states. CSV, PostGIS and
/// BUFR config entries fall back to the parameter's name for an observed
/// property they were not given, so an observed property equal to the name
/// is not taken as stated.
pub fn identifies_air_temperature(name: &str, description: &ParameterDescription) -> bool {
    const AIR_TEMPERATURE: &str = "air_temperature";
    description.standard_name.as_deref().map(str::trim) == Some(AIR_TEMPERATURE)
        || (description.observed_property.trim() == AIR_TEMPERATURE
            && description.observed_property != name)
}

/// Units per kelvin of a temperature unit; `None` when it is not one.
fn per_kelvin(unit: &str) -> Option<f64> {
    match canonical_unit(unit).as_ref() {
        "K" | "°C" => Some(1.0),
        "°F" | "degF" | "deg_F" | "degree_F" | "degrees_F" | "degree_Fahrenheit"
        | "degrees_Fahrenheit" | "fahrenheit" | "Fahrenheit" | "[degF]" => Some(1.8),
        _ => None,
    }
}

/// One parameter's corrections.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ParamCorrection {
    /// Γ in K/km, for the provenance.
    lapse_k_per_km: Option<f64>,
    /// Γ in the parameter's unit per metre.
    lapse_per_m: Option<f64>,
    masked: bool,
}

pub(crate) struct Corrections {
    collection_id: String,
    names: Vec<String>,
    params: Vec<ParamCorrection>,
    geom: GridGeom,
    elevation: Option<(RasterSource, DemGrid)>,
    land_mask: Option<RasterSource>,
    /// The terrain and the mask, each kept once sampled.
    dem: OnceLock<Dem>,
    /// Per analysis cell: inside the mask.
    inside: OnceLock<Vec<bool>>,
    exclusions: Exclusions,
    stations: Option<Arc<dyn FeatureEngine>>,
    inventory: ArcSwap<Inventory>,
    /// Why the rasters are not sampled yet.
    waiting: Mutex<Option<String>>,
    /// Stations already logged: (no height, disagreeing heights).
    logged: Mutex<(Logged, Logged)>,
}

impl Corrections {
    /// Check the corrections config against the source's parameters
    /// (`params`, in the engine's order, with the source's descriptions)
    /// and the auxiliary collections' metadata. Reads no data.
    pub fn new(
        collection_id: &str,
        config: &StationGridConfig,
        aux: Auxiliary,
        params: &[(String, ParameterDescription)],
        geom: &GridGeom,
        radius_m: f64,
    ) -> Result<Self, String> {
        let elevation_id = config.elevation_source.as_deref();
        let mask_id = config.land_mask_source.as_deref();
        let mut corrections = Vec::with_capacity(params.len());
        for (entry, (name, description)) in config.parameters.iter().zip(params) {
            if entry.lapse_rate().is_some() && elevation_id.is_none() {
                return Err(format!(
                    "parameter '{name}' sets lapse_rate, which needs elevation_source"
                ));
            }
            if entry.land_mask().is_some() && mask_id.is_none() {
                return Err(format!(
                    "parameter '{name}' sets land_mask, which needs land_mask_source"
                ));
            }
            let rate = match elevation_id {
                None => None,
                Some(_) => entry.lapse_rate().or_else(|| {
                    identifies_air_temperature(name, description).then_some(STANDARD_LAPSE_RATE)
                }),
            };
            if let Some(rate) = rate {
                if !(rate.is_finite() && rate.abs() <= MAX_LAPSE_RATE) {
                    return Err(format!(
                        "lapse_rate of '{name}' must be a number of K/km within \
                         ±{MAX_LAPSE_RATE}, not {rate}"
                    ));
                }
            }
            // Zero turns the correction off, the default's opt-out.
            let rate = rate.filter(|&r| r != 0.0);
            let lapse_per_m = match rate {
                None => None,
                Some(rate) => {
                    let scale = per_kelvin(&description.unit).ok_or_else(|| {
                        format!(
                            "parameter '{name}' is corrected with a lapse rate of {rate} K/km, \
                             but its unit '{}' is not a temperature (K, °C or °F)",
                            description.unit
                        )
                    })?;
                    Some(rate * scale / 1000.0)
                }
            };
            corrections.push(ParamCorrection {
                lapse_k_per_km: rate,
                lapse_per_m,
                masked: mask_id.is_some() && entry.land_mask().unwrap_or(true),
            });
        }
        if let Some(id) = elevation_id {
            if corrections.iter().all(|c| c.lapse_per_m.is_none()) {
                return Err(format!(
                    "elevation_source '{id}' is set but no parameter is corrected: set \
                     lapse_rate on a temperature parameter (the {STANDARD_LAPSE_RATE} K/km default \
                     applies only where the source's metadata identifies air temperature)"
                ));
            }
        }
        if let Some(id) = mask_id {
            if corrections.iter().all(|c| !c.masked) {
                return Err(format!(
                    "land_mask_source '{id}' is set but every parameter sets land_mask = false"
                ));
            }
        }
        let exclusions = Exclusions::new(&config.exclude_where)?;

        let raster = |key: &str,
                      id: Option<&str>,
                      engine: Option<Arc<dyn MapEngine>>,
                      kind: Kind,
                      parameter: Option<&str>|
         -> Result<Option<RasterSource>, String> {
            let Some(id) = id else {
                return Ok(None);
            };
            let engine = engine
                .ok_or_else(|| format!("{key} '{id}' is not wired to a raster collection"))?;
            let source = RasterSource::new(id, kind, engine, parameter);
            // A collection that reports its parameters now is checked now; a
            // remote one before its first scan is checked when sampled.
            source.check()?;
            Ok(Some(source))
        };
        let elevation = raster(
            "elevation_source",
            elevation_id,
            aux.elevation,
            Kind::Elevation,
            config.elevation_parameter.as_deref(),
        )?
        .map(|source| (source, dem_grid(geom, radius_m)));
        let land_mask = raster(
            "land_mask_source",
            mask_id,
            aux.land_mask,
            Kind::LandMask,
            config.land_mask_parameter.as_deref(),
        )?;
        for (key, set, id) in [
            (
                "elevation_parameter",
                config.elevation_parameter.is_some(),
                elevation_id,
            ),
            (
                "land_mask_parameter",
                config.land_mask_parameter.is_some(),
                mask_id,
            ),
        ] {
            if set && id.is_none() {
                return Err(format!(
                    "{key} is set without its {}",
                    key.replace("_parameter", "_source")
                ));
            }
        }
        let needs_inventory = !exclusions.is_empty();
        if needs_inventory && aux.stations.is_none() {
            return Err(format!(
                "source '{}' has no station inventory to read exclude_where from",
                config.source
            ));
        }
        Ok(Self {
            collection_id: collection_id.to_string(),
            names: params.iter().map(|(name, _)| name.clone()).collect(),
            params: corrections,
            geom: *geom,
            elevation,
            land_mask,
            dem: OnceLock::new(),
            inside: OnceLock::new(),
            exclusions,
            stations: aux.stations,
            inventory: ArcSwap::from_pointee(Inventory::default()),
            waiting: Mutex::new(None),
            logged: Mutex::new((Logged::default(), Logged::default())),
        })
    }

    /// Whether any parameter reads station heights.
    fn corrects(&self) -> bool {
        self.params.iter().any(|p| p.lapse_per_m.is_some())
    }

    /// Bytes the sampled terrain and mask hold for the engine's life: they
    /// count against the grid cache budget at load, with one grid.
    pub fn resident_bytes(&self) -> u64 {
        let dem = self
            .elevation
            .as_ref()
            .map_or(0, |(_, grid)| grid.geom.cells() as u64 * 4);
        let mask = self
            .land_mask
            .as_ref()
            .map_or(0, |_| self.geom.cells() as u64);
        dem + mask
    }

    /// Whether the station inventory is read for heights or exclusions.
    fn needs_inventory(&self) -> bool {
        self.stations.is_some() && (self.corrects() || !self.exclusions.is_empty())
    }

    /// Whether the terrain and the mask are sampled and the station
    /// inventory read, where they are needed: until then the engine has no
    /// analysis time.
    pub fn ready(&self) -> bool {
        self.rasters_ready() && (!self.needs_inventory() || self.inventory.load().version.is_some())
    }

    /// Why the engine has no analysis time yet, when the corrections are
    /// what it waits for.
    pub fn waiting_for(&self) -> Option<String> {
        if self.ready() {
            return None;
        }
        let waiting = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
        Some(waiting.clone().unwrap_or_else(|| {
            let ids: Vec<String> = self
                .elevation
                .iter()
                .map(|(s, _)| format!("elevation_source '{}'", s.id))
                .chain(
                    self.land_mask
                        .iter()
                        .map(|s| format!("land_mask_source '{}'", s.id)),
                )
                .chain(
                    self.needs_inventory()
                        .then(|| "the source's station inventory".to_string()),
                )
                .collect();
            format!("waiting for {}", ids.join(" and "))
        }))
    }

    /// The poll's part: sample the terrain and the mask until they are,
    /// then re-read the station inventory when the source's moved.
    /// Synchronous source I/O: the background poll runtime only.
    pub fn refresh(&self) {
        if !self.rasters_ready() {
            if let Err(e) = self.sample() {
                return self.wait(e);
            }
        }
        if let Err(e) = self.refresh_inventory() {
            if self.inventory.load().version.is_none() {
                return self.wait(e);
            }
            // A later read failing keeps the inventory last read.
            warn!("[{}] {e}", self.collection_id);
        }
        *self.waiting.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Record why the engine has no analysis time yet, logging each new
    /// reason once.
    fn wait(&self, reason: String) {
        let mut waiting = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
        if waiting.as_deref() != Some(reason.as_str()) {
            warn!(
                "[{}] no analysis until the corrections are ready: {reason}",
                self.collection_id
            );
        }
        *waiting = Some(reason);
    }

    /// Whether every raster the corrections read is sampled.
    fn rasters_ready(&self) -> bool {
        (self.elevation.is_none() || self.dem.get().is_some())
            && (self.land_mask.is_none() || self.inside.get().is_some())
    }

    /// Sample each raster not sampled yet, keeping each once it is: a
    /// terrain read is never repeated while the mask is still missing.
    fn sample(&self) -> Result<(), String> {
        let mut missing = Vec::new();
        if let Some((source, grid)) = &self.elevation {
            if self.dem.get().is_none() {
                match source.sample_metres(&grid.geom) {
                    Ok(metres) => {
                        let _ = self.dem.set(Dem {
                            grid: *grid,
                            metres,
                        });
                        info!(
                            "[{}] sampled the terrain of '{}' onto the analysis grid",
                            self.collection_id, source.id
                        );
                    }
                    Err(e) => missing.push(e),
                }
            }
        }
        if let Some(source) = &self.land_mask {
            if self.inside.get().is_none() {
                match source.sample_inside(&self.geom) {
                    Ok(inside) => {
                        if !inside.contains(&true) {
                            warn!(
                                "[{}] every cell of the grid is outside land_mask_source '{}'",
                                self.collection_id, source.id
                            );
                        }
                        let _ = self.inside.set(inside);
                        info!(
                            "[{}] sampled the mask of '{}' onto the analysis grid",
                            self.collection_id, source.id
                        );
                    }
                    Err(e) => missing.push(e),
                }
            }
        }
        if missing.is_empty() {
            Ok(())
        } else {
            Err(missing.join("; "))
        }
    }

    fn dem(&self) -> Option<&Dem> {
        self.dem.get()
    }

    /// Re-read the station inventory when the source's data version moved.
    fn refresh_inventory(&self) -> Result<(), String> {
        let Some(engine) = self.stations.as_ref().filter(|_| self.needs_inventory()) else {
            return Ok(());
        };
        let version = engine.data_version();
        if self.inventory.load().version == Some(version) {
            return Ok(());
        }
        let stations = inventory::read(engine.as_ref(), &self.exclusions)
            .map_err(|e| format!("reading the source's station inventory failed: {e}"))?;
        if let Some(dem) = self.dem() {
            let mut logged = self.logged.lock().unwrap_or_else(|e| e.into_inner());
            for (&(x, y), known) in &stations {
                let Some(stated) = known.stated_height else {
                    continue;
                };
                let (lon, lat) = (x as f64 / 1e6, y as f64 / 1e6);
                let Some(terrain) = dem.height_at(lon, lat) else {
                    continue;
                };
                if (stated - terrain).abs() <= HEIGHT_DISAGREEMENT_M {
                    continue;
                }
                match logged.1.first((x, y)) {
                    Some(true) => warn!(
                        "[{}] the station at ({lon}, {lat}) states a height of {stated} m, \
                         {:.0} m from the terrain's {terrain:.0} m; its stated height is used",
                        self.collection_id,
                        (stated - terrain).abs()
                    ),
                    Some(false) => warn!(
                        "[{}] more stations state a height over {HEIGHT_DISAGREEMENT_M} m from \
                         the terrain's; not logged one by one",
                        self.collection_id
                    ),
                    None => {}
                }
            }
        }
        let excluded = stations.values().filter(|k| k.excluded).count();
        debug!(
            "[{}] station inventory: {} stations, {excluded} excluded, {} with a stated height",
            self.collection_id,
            stations.len(),
            stations
                .values()
                .filter(|k| k.stated_height.is_some())
                .count()
        );
        self.inventory.store(Arc::new(Inventory {
            version: Some(version),
            stations,
        }));
        Ok(())
    }

    /// A slice as this parameter is gridded: excluded stations and, for a
    /// corrected parameter, stations without a height left out, and each
    /// other station's reduction to sea level, `Γ·z_station`, alongside its
    /// observed value (`Slice::reduction`: the grid interpolates the reduced
    /// values, a residual is put back at the station's height). Its
    /// fingerprint also covers what was left out and the heights used.
    pub fn prepare(&self, parameter: usize, slice: Slice) -> Result<Slice, DataServerError> {
        let lapse = self.params[parameter].lapse_per_m;
        if lapse.is_none() && self.exclusions.is_empty() {
            return Ok(slice);
        }
        let dem = match (lapse, self.dem()) {
            (Some(_), None) => return Err(not_ready()),
            (_, dem) => dem,
        };
        let inventory = self.inventory.load();
        let mut hasher = DefaultHasher::new();
        slice.fingerprint.hash(&mut hasher);
        let (mut excluded, mut no_height) = (0usize, 0usize);
        let mut kept = Vec::with_capacity(slice.stations.len());
        let mut reduction = Vec::new();
        for station in slice.stations {
            let known = inventory.get(station.lon, station.lat);
            if known.is_some_and(|k| k.excluded) {
                excluded += 1;
                (0u8, inventory::key(station.lon, station.lat)).hash(&mut hasher);
                continue;
            }
            if let Some(rate) = lapse {
                let height = known
                    .and_then(|k| k.stated_height)
                    .or_else(|| dem.and_then(|d| d.height_at(station.lon, station.lat)));
                let Some(z) = height else {
                    no_height += 1;
                    let key = inventory::key(station.lon, station.lat);
                    (1u8, key).hash(&mut hasher);
                    self.log_no_height(key, station.lon, station.lat);
                    continue;
                };
                (2u8, z.to_bits()).hash(&mut hasher);
                reduction.push(rate * z);
            }
            kept.push(station);
        }
        if excluded + no_height > 0 {
            debug!(
                "[{}] {}: {excluded} stations excluded, {no_height} without a height left out",
                self.collection_id, self.names[parameter]
            );
        }
        Ok(Slice {
            stations: kept,
            fingerprint: hasher.finish(),
            reduction,
        })
    }

    fn log_no_height(&self, key: Key, lon: f64, lat: f64) {
        let mut logged = self.logged.lock().unwrap_or_else(|e| e.into_inner());
        match logged.0.first(key) {
            Some(true) => warn!(
                "[{}] the station at ({lon}, {lat}) has no stated height and no terrain under \
                 it; it is left out of lapse-rate corrected parameters",
                self.collection_id
            ),
            Some(false) => warn!(
                "[{}] more stations have no height and are left out of lapse-rate corrected \
                 parameters; not logged one by one (the analysis log at debug counts them)",
                self.collection_id
            ),
            None => {}
        }
    }

    /// An interpolated grid of `parameter` as served: `Γ·z_cell` added back
    /// (nodata over terrain nodata), and nodata outside the mask.
    pub fn finish(&self, parameter: usize, values: &mut [f32]) -> Result<(), DataServerError> {
        let correction = self.params[parameter];
        if correction.lapse_per_m.is_none() && !correction.masked {
            return Ok(());
        }
        if let Some(rate) = correction.lapse_per_m {
            let dem = self.dem.get().ok_or_else(not_ready)?;
            let nx = self.geom.nx as usize;
            for (i, value) in values.iter_mut().enumerate() {
                let z = dem.at_cell((i % nx) as u32, (i / nx) as u32);
                *value = if z.is_finite() && value.is_finite() {
                    (f64::from(*value) + rate * f64::from(z)) as f32
                } else {
                    f32::NAN
                };
            }
        }
        if correction.masked {
            let inside = self.inside.get().ok_or_else(not_ready)?;
            for (value, &inside) in values.iter_mut().zip(inside) {
                if !inside {
                    *value = f32::NAN;
                }
            }
        }
        Ok(())
    }

    /// One or two sentences for the provenance; empty without corrections.
    pub fn describe(&self) -> String {
        let mut out = Vec::new();
        if let Some((source, _)) = &self.elevation {
            let rates: Vec<String> = self
                .names
                .iter()
                .zip(&self.params)
                .filter_map(|(name, p)| p.lapse_k_per_km.map(|r| format!("{name} {r} K/km")))
                .collect();
            out.push(format!(
                "Corrected for height with a lapse rate ({}): station values are reduced to sea \
                 level with the station's stated height, else the terrain of '{}' at it, and \
                 each cell adds back the lapse rate times its terrain height.",
                rates.join(", "),
                source.id
            ));
        }
        if let Some(source) = &self.land_mask {
            let masked: Vec<&str> = self
                .names
                .iter()
                .zip(&self.params)
                .filter(|(_, p)| p.masked)
                .map(|(name, _)| name.as_str())
                .collect();
            out.push(format!(
                "Cells outside the land mask of '{}' are nodata ({}).",
                source.id,
                masked.join(", ")
            ));
        }
        if !self.exclusions.is_empty() {
            out.push("Stations matching exclude_where are left out.".to_string());
        }
        out.join(" ")
    }
}

fn not_ready() -> DataServerError {
    // Unreachable through the engine, which publishes no analysis time
    // before the rasters are sampled: answer busy rather than a grid
    // without its corrections.
    DataServerError::ResourceExhausted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn description(observed: &str, standard: Option<&str>) -> ParameterDescription {
        ParameterDescription {
            label: String::new(),
            unit: "°C".into(),
            observed_property: observed.into(),
            standard_name: standard.map(Into::into),
        }
    }

    /// Air temperature from the source's metadata, never from the name.
    #[test]
    fn air_temperature_from_metadata_only() {
        // BUFR built-in: the CF standard name.
        assert!(identifies_air_temperature(
            "air_temperature",
            &description("air_temperature", Some("air_temperature"))
        ));
        // A stated observed property (PostGIS `observed_property`).
        assert!(identifies_air_temperature(
            "t2m",
            &description("air_temperature", None)
        ));
        // CSV and unstated config entries: the observed property is the name.
        assert!(!identifies_air_temperature(
            "air_temperature",
            &description("air_temperature", None)
        ));
        assert!(!identifies_air_temperature(
            "temperature",
            &description("temperature", None)
        ));
        assert!(!identifies_air_temperature(
            "dew_point_temperature",
            &description("dew_point_temperature", Some("dew_point_temperature"))
        ));
    }

    /// Each station once, the first few hundred, then one notice.
    #[test]
    fn logged_stations_are_bounded() {
        let mut logged = Logged::default();
        assert_eq!(logged.first((1, 1)), Some(true));
        assert_eq!(logged.first((1, 1)), None);
        for i in 2..=MAX_LOGGED_STATIONS as i64 {
            assert_eq!(logged.first((i, 0)), Some(true));
        }
        assert_eq!(logged.first((-1, 0)), Some(false));
        assert_eq!(logged.first((-2, 0)), None);
        assert_eq!(logged.0.len(), MAX_LOGGED_STATIONS + 1);
    }

    #[test]
    fn temperature_units() {
        assert_eq!(per_kelvin("K"), Some(1.0));
        assert_eq!(per_kelvin("degC"), Some(1.0));
        assert_eq!(per_kelvin("°C"), Some(1.0));
        assert_eq!(per_kelvin("degF"), Some(1.8));
        assert_eq!(per_kelvin("%"), None);
        assert_eq!(per_kelvin(""), None);
    }
}
