//! Station-symbol map layers (#1204): one WMS/Maps/Tiles layer per
//! parameter, each station a marker coloured by its latest report value
//! within `[bufr.map] max_age`, drawn through the shared helper
//! (`ds_render::station_symbols`, as PostGIS station collections are).
//!
//! - **Frame.** A frame at `TIME` (floored to `time_step`, default 10
//!   minutes; no `TIME` ⇒ the latest advertised step, over the newest 6 h of
//!   reports) shows each station's latest non-missing value in
//!   `[TIME - max_age, TIME]`. It is read from the retention store under its
//!   read lock, one reverse `BTreeMap` range walk a station that stops at the
//!   first value, at most `MAX_FRAME_ROWS` rows a frame. Stations come from
//!   the metadata snapshot, ordered by id, at the snapshot's positions.
//!   `resolve_time` and `resolve_parameter_time` return the same instant
//!   (#507).
//! - **Version.** The store revises in place: a WIS2 report for an instant
//!   already drawn arrives late, a correction replaces a row, a deletion or
//!   the prune removes one. `content_version` is the snapshot version, which
//!   moves on the rebuild after every store change (at most every 10 s), so
//!   the no-TTL rendered and meta-tile caches and the frame cache re-key and
//!   explicit-`TIME` responses revalidate. A frame can be up to that long
//!   behind the store, as every other read surface of the snapshot is.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, ParameterInfo, RasterInfo, RasterTile};
use ds_core::station_map::MAX_FRAME_ROWS;
use ds_render::station_symbols::{Marker, StationLayers};

use crate::engine::{round_stored, BufrEngine};
use crate::metadata::Snapshot;
use crate::params::ParameterTable;

/// The map layers' metadata for a snapshot: one layer per parameter, in
/// table order, each with its display unit (so the built-in per-parameter
/// default styles apply), and the TIME steps over the reports' extent.
pub(crate) fn raster_info(
    table: &ParameterTable,
    layers: &StationLayers,
    spatial_extent: Option<[f64; 4]>,
    temporal_extent: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> RasterInfo {
    let parameters: Vec<ParameterInfo> = table
        .params
        .iter()
        .map(|p| ParameterInfo {
            name: p.name.clone(),
            title: p.label.clone(),
            unit: p.unit.clone(),
        })
        .collect();
    RasterInfo {
        native_crs: "CRS:84".to_string(),
        spatial_extent,
        times: layers.advertised_times(temporal_extent),
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
    }
}

impl BufrEngine {
    /// The store column a layer draws: the requested parameter, else the
    /// first. An unknown name is the client's error.
    fn map_column(&self, parameter: Option<&str>) -> Result<(usize, &str), DataServerError> {
        match parameter {
            None => self
                .table
                .params
                .first()
                .map(|p| (0, p.name.as_str()))
                .ok_or_else(|| DataServerError::Engine("BUFR collection has no parameters".into())),
            Some(name) => self
                .table
                .index_of(name)
                .map(|i| (i, self.table.params[i].name.as_str()))
                .ok_or_else(|| {
                    DataServerError::InvalidParameter(format!("unknown parameter: {name}"))
                }),
        }
    }

    /// One marker per snapshot station, in its order: the station's latest
    /// non-missing value of `column` in `[start, end]`, `None` without one.
    fn read_frame(
        &self,
        snap: &Snapshot,
        column: usize,
        (start, end): (DateTime<Utc>, DateTime<Utc>),
    ) -> Result<Vec<Marker>, DataServerError> {
        let store = self.store.read().unwrap_or_else(|e| e.into_inner());
        let mut scanned = 0usize;
        let mut markers = Vec::with_capacity(snap.stations.len());
        for station in snap.stations.iter() {
            let mut value = None;
            if let Some(series) = store.get(&station.id) {
                for (_, row) in series.rows.range(start..=end).rev() {
                    scanned += 1;
                    if scanned > MAX_FRAME_ROWS {
                        return Err(DataServerError::QueryTooLarge(format!(
                            "a station map frame reads more than {MAX_FRAME_ROWS} reports — \
                             shorten [bufr.map] max_age"
                        )));
                    }
                    if let Some(&v) = row.get(column).filter(|v| v.is_finite()) {
                        value = Some(round_stored(v));
                        break;
                    }
                }
            }
            markers.push(Marker {
                lon: station.lon,
                lat: station.lat,
                value,
            });
        }
        Ok(markers)
    }
}

/// The content version a snapshot stands for: its version, never `0`
/// ("never revised" to the API layers).
fn version_of(snap: &Snapshot) -> u64 {
    snap.version.max(1)
}

impl MapEngine for BufrEngine {
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
        let (column, parameter) = self.map_column(parameter)?;
        let snap = self.snapshot.load_full();
        let end = self
            .map
            .frame_end(&snap.raster_info.times, time)
            .ok_or_else(|| {
                DataServerError::Engine(
                    "no observation data available for the requested time".into(),
                )
            })?;
        let markers = self
            .map
            .frame(parameter, end, version_of(&snap), |window| {
                self.read_frame(&snap, column, window)
            })?;
        Ok(self.map.draw(&markers, bbox, width, height, output_crs))
    }

    fn raster_info(&self) -> RasterInfo {
        (*self.snapshot.load().raster_info).clone()
    }

    /// O(1): the snapshot's `Arc`, rebuilt with the metadata (#211).
    fn raster_info_shared(&self) -> Arc<RasterInfo> {
        Arc::clone(&self.snapshot.load().raster_info)
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
            .frame_end(&self.snapshot.load().raster_info.times, time)
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

    /// Moves with the store, through the snapshot rebuild after each change.
    fn content_version(&self) -> u64 {
        version_of(&self.snapshot.load())
    }

    /// A marker shows the latest value within `max_age`, which the EDR
    /// position query (an exact instant window at the nearest station) does
    /// not answer: GetFeatureInfo must not read it there.
    fn layer_shows_values(&self, _parameter: &str) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use ds_core::config::{BufrConfig, StationMapConfig};
    use ds_core::map_engine::{MapEngine, OutputCrs};

    use crate::BufrEngine;

    const SMHI: &str = "synop_se-smhi_20260912T0800Z.bufr";

    fn fixtures() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/bufr-synop")
    }

    /// The SMHI value of the 08:00Z frame on a 1°-a-pixel world, if drawn.
    fn smhi_marker(e: &BufrEngine) -> Option<f64> {
        let time = chrono::Utc.with_ymd_and_hms(2026, 9, 12, 8, 0, 0).unwrap();
        let tile = e
            .get_raster_tile(
                [-180.0, -90.0, 180.0, 90.0],
                360,
                180,
                Some(time),
                &OutputCrs::Wgs84,
                Some("air_temperature"),
                None,
                None,
            )
            .unwrap();
        tile.values.value_at(32 * 360 + 198)
    }

    /// A report that arrives after its frame was drawn (WIS2 delivers late
    /// and corrections replace rows) moves `content_version` at the next
    /// snapshot rebuild, so the frame and the API caches re-key and the
    /// late value is drawn; a rebuild with nothing new keeps it.
    #[test]
    fn a_late_report_moves_the_content_version() {
        let dir = tempfile::tempdir().unwrap();
        for entry in std::fs::read_dir(fixtures()).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().and_then(|n| n.to_str()) != Some(SMHI) {
                std::fs::copy(&path, dir.path().join(path.file_name().unwrap())).unwrap();
            }
        }
        let cfg = BufrConfig {
            data_path: Some(dir.path().to_string_lossy().into_owned()),
            wis2: None,
            poll_interval_secs: 60,
            retention: "P36500D".to_string(),
            max_stations: 50_000,
            stale_after: "PT2H".to_string(),
            position_radius_km: 25.0,
            builtin_parameters: true,
            parameters: Vec::new(),
            map: Some(StationMapConfig {
                marker_size: Some(1),
                ..Default::default()
            }),
        };
        let e = BufrEngine::new(&cfg, "bufr-late").unwrap();
        let before = e.content_version();
        assert_ne!(before, 0);
        assert_eq!(smhi_marker(&e), None);

        let bytes = std::fs::read(fixtures().join(SMHI)).unwrap();
        assert_eq!(e.ingest_bytes(&bytes, SMHI, chrono::Utc::now()), 1);
        // Until the snapshot rebuild (at most 10 s on the poll loop) the
        // version, and so the cached frame, stand.
        assert_eq!(e.content_version(), before);
        e.snapshot_if_dirty();
        let after = e.content_version();
        assert_ne!(after, before);
        assert_eq!(smhi_marker(&e), Some(16.97));
        e.snapshot_if_dirty();
        assert_eq!(e.content_version(), after, "nothing new, no churn");

        // A correction replaces the row: the version moves again.
        assert_eq!(e.ingest_bytes(&bytes, SMHI, chrono::Utc::now()), 1);
        e.snapshot_if_dirty();
        assert_ne!(e.content_version(), after);
    }
}
