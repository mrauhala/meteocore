//! Station-symbol map layers (#1204): one WMS/Maps/Tiles layer per CSV
//! parameter column, each station a marker coloured by its latest non-null
//! value within `[csv.map] max_age`, drawn through the shared helper
//! (`ds_render::station_symbols`, as PostGIS station collections are).
//!
//! - **Frame.** A frame at `TIME` (floored to `time_step`, default 10
//!   minutes; no `TIME` ⇒ the latest advertised step, over the last 6 h of
//!   the file) shows each station's latest finite value in
//!   `[TIME - max_age, TIME]`, read from the in-memory rows: per station a
//!   reverse walk of its time index from `TIME`, stopping at the first value,
//!   at most `MAX_FRAME_ROWS` rows a frame. A station is drawn at its
//!   inventory position (its first row), as `/locations` and Features list
//!   it. `resolve_time` and `resolve_parameter_time` return the same instant
//!   (#507).
//! - **Version.** The file is static until a reload, which rebuilds the
//!   engine (a new frame source, and the API caches of a rebuilt collection
//!   are evicted): `content_version` stays `0`.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, ParameterInfo, RasterInfo, RasterTile};
use ds_core::station_map::{StationMapSettings, MAX_FRAME_ROWS};
use ds_render::station_symbols::{Marker, StationLayers};

use crate::engine::CsvEngine;
use crate::loader::CsvDataStore;

/// The map layers' settings and metadata, built once with the engine.
pub(crate) struct StationMap {
    pub(crate) layers: StationLayers,
    /// O(1) for the per-request accessors (Critical Rule 10).
    raster_info: Arc<RasterInfo>,
}

impl StationMap {
    pub(crate) fn new(
        store: &CsvDataStore,
        time_extent: Option<(DateTime<Utc>, DateTime<Utc>)>,
        settings: StationMapSettings,
    ) -> Self {
        let layers = StationLayers::new(settings);
        // One layer per parameter column, in file order, each with the unit
        // the EDR parameter descriptions carry, so the built-in per-parameter
        // default styles apply.
        let parameters: Vec<ParameterInfo> = store
            .parameter_names
            .iter()
            .map(|name| ParameterInfo {
                name: name.clone(),
                title: name.replace('_', " "),
                unit: store.parameter_units.get(name).cloned().unwrap_or_default(),
            })
            .collect();
        let raster_info = RasterInfo {
            native_crs: "CRS:84".to_string(),
            spatial_extent: crate::engine::spatial_extent(store),
            times: layers.advertised_times(time_extent),
            parameter: parameters
                .first()
                .map(|p| p.name.clone())
                .unwrap_or_default(),
            unit: parameters
                .first()
                .map(|p| p.unit.clone())
                .unwrap_or_default(),
            parameters,
            vertical: None,
            grid_size: None,
            layer_subtitle: None,
            reference_times: Vec::new(),
            native_grid: None,
        };
        Self {
            layers,
            raster_info: Arc::new(raster_info),
        }
    }
}

impl CsvEngine {
    /// The parameter a layer draws: the requested one, else the first
    /// column. An unknown name is the client's error.
    fn map_parameter<'a>(&'a self, parameter: Option<&'a str>) -> Result<&'a str, DataServerError> {
        let names = &self.store.parameter_names;
        match parameter {
            None => names
                .first()
                .map(String::as_str)
                .ok_or_else(|| DataServerError::Engine("CSV has no parameter columns".into())),
            Some(name) if names.iter().any(|n| n == name) => Ok(name),
            Some(name) => Err(DataServerError::InvalidParameter(format!(
                "unknown parameter: {name}"
            ))),
        }
    }

    /// One marker per station, in inventory order: its latest finite value
    /// of `parameter` in `[start, end]`, `None` without one. Of two rows at
    /// one time the later in the file wins, as `LatestValues` keeps it.
    fn read_frame(
        &self,
        parameter: &str,
        (start, end): (DateTime<Utc>, DateTime<Utc>),
    ) -> Result<Vec<Marker>, DataServerError> {
        let mut scanned = 0usize;
        let mut markers = Vec::with_capacity(self.locations.len());
        for location in self.locations.iter() {
            let mut value = None;
            if let Some(times) = self.store.time_index.get(&location.id) {
                'rows: for (_, rows) in times.range(start..=end).rev() {
                    for &row in rows.iter().rev() {
                        scanned += 1;
                        if scanned > MAX_FRAME_ROWS {
                            return Err(DataServerError::QueryTooLarge(format!(
                                "a station map frame reads more than {MAX_FRAME_ROWS} \
                                 observation rows — shorten [csv.map] max_age"
                            )));
                        }
                        let v = self.store.rows[row].values.get(parameter).copied();
                        if let Some(v) = v.flatten().filter(|v| v.is_finite()) {
                            value = Some(v);
                            break 'rows;
                        }
                    }
                }
            }
            markers.push(Marker {
                lon: location.longitude,
                lat: location.latitude,
                value,
            });
        }
        Ok(markers)
    }
}

impl MapEngine for CsvEngine {
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        let parameter = self.map_parameter(parameter)?;
        let layers = &self.map.layers;
        let end = layers
            .frame_end(&self.map.raster_info.times, time)
            .ok_or_else(|| {
                DataServerError::Engine(
                    "no observation data available for the requested time".into(),
                )
            })?;
        let markers = layers.frame(parameter, end, self.content_version(), |window| {
            self.read_frame(parameter, window)
        })?;
        Ok(layers.draw(&markers, bbox, width, height, output_crs))
    }

    fn raster_info(&self) -> RasterInfo {
        (*self.map.raster_info).clone()
    }

    fn raster_info_shared(&self) -> Arc<RasterInfo> {
        Arc::clone(&self.map.raster_info)
    }

    /// The #507 cache-key authority: the frame instant the render draws,
    /// through the same selection. Without data and without TIME the render
    /// errors, so the requested time is safe to echo.
    fn resolve_time(
        &self,
        time: Option<DateTime<Utc>>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        self.map
            .layers
            .frame_end(&self.map.raster_info.times, time)
            .or(time)
    }

    /// Every parameter layer shares one time axis and frame selection.
    fn resolve_parameter_time(
        &self,
        _parameter: Option<&str>,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        self.resolve_time(time, reference_time)
    }

    /// Static until a reload rebuilds the engine.
    fn content_version(&self) -> u64 {
        0
    }

    /// A marker shows the latest value within `max_age`, which no EDR
    /// position query answers: GetFeatureInfo must not read it there.
    fn layer_shows_values(&self, _parameter: &str) -> bool {
        false
    }
}
