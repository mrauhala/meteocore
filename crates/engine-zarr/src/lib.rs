//! Zarr engine — reads cloud-native multidimensional arrays (Zarr V2/V3) with
//! CF-conventions metadata and serves them over the EDR API.
//!
//! **Scope** (issue #125, Phases 1-2): a local **or** remote (S3/HTTP) Zarr
//! store on a WGS84/geographic lat-lon grid, multi-variable EDR *position*
//! queries with bilinear interpolation, CF time-axis decoding, CF packing
//! (`scale_factor`/`add_offset`/`_FillValue`), byte-range chunk reads with an
//! LRU cache, and a startup warning for pathological chunk shapes. Map/Tiles/WMS
//! rendering (Phase 3) and projected/per-item-CRS sources (Phase 4) are not
//! implemented yet.
//!
//! The Zarr format and codec pipeline (blosc/zstd/gzip/crc32c/sharding) are
//! handled by the `zarrs` crate; all I/O goes through the shared `ds-storage`
//! object store via [`store::DsStore`]. This engine adds the CF semantics, the
//! OGC domain mapping, and the poll-and-swap lifecycle shared by the other
//! engines.

mod catalog;
mod cf;
mod store;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use tokio::sync::watch;

use ds_core::config::ZarrConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::model::{
    CoverageResponse, DomainDescription, Location, NdArray, ParameterDescription, QueryResult,
};

use catalog::Catalog;
use store::DsStore;

/// Engine for serving Zarr arrays over EDR.
pub struct ZarrEngine {
    collection_id: String,
    /// `ds-storage`-backed store (local / S3 / HTTP), shared by query and poll.
    store: Arc<DsStore>,
    /// Parsed snapshot, swapped atomically by the poll loop.
    catalog: ArcSwap<Catalog>,
    /// Variable filter from config (`None` = expose all).
    param_filter: Option<Vec<String>>,
    poll_interval: Duration,
    shutdown_tx: watch::Sender<()>,
}

impl ZarrEngine {
    /// Open a Zarr store (local directory or remote S3/HTTP) and build the
    /// initial catalog.
    pub fn new(collection_id: &str, config: &ZarrConfig) -> Result<Self, DataServerError> {
        let store = Arc::new(build_store(collection_id, config)?);

        let param_filter = config.parameters.clone();
        let catalog = catalog::build(store.clone(), collection_id, param_filter.as_deref())?;

        log_loaded(collection_id, &catalog);

        let (shutdown_tx, _) = watch::channel(());
        Ok(Self {
            collection_id: collection_id.to_string(),
            store,
            catalog: ArcSwap::from_pointee(catalog),
            param_filter,
            poll_interval: Duration::from_secs(config.poll_interval_secs.max(1)),
            shutdown_tx,
        })
    }

    /// Run the poll loop: periodically rebuild the catalog so appended time
    /// steps surface. Exits when [`shutdown`](Self::shutdown) is called.
    ///
    /// Must run on the dedicated background poll runtime — the rebuild does
    /// blocking store I/O (see the engine concurrency rules in CLAUDE.md).
    pub async fn poll_loop(&self) {
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let mut interval = tokio::time::interval(self.poll_interval);
        interval.tick().await; // skip the immediate first tick

        loop {
            tokio::select! {
                _ = interval.tick() => self.poll_once(),
                _ = shutdown_rx.changed() => {
                    tracing::info!("[{}] Zarr poll loop shutting down", self.collection_id);
                    break;
                }
            }
        }
    }

    /// Signal the poll loop to stop.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
    }

    /// The collection ID this engine serves.
    pub fn collection_id(&self) -> &str {
        &self.collection_id
    }

    fn poll_once(&self) {
        match catalog::build(
            self.store.clone(),
            &self.collection_id,
            self.param_filter.as_deref(),
        ) {
            Ok(new_catalog) => {
                let current = self.catalog.load();
                if new_catalog.times != current.times
                    || new_catalog.vars.len() != current.vars.len()
                {
                    log_loaded(&self.collection_id, &new_catalog);
                }
                self.catalog.store(Arc::new(new_catalog));
            }
            Err(e) => {
                tracing::warn!(
                    "[{}] Zarr poll rebuild failed (keeping previous catalog): {e}",
                    self.collection_id
                );
            }
        }
    }
}

impl EdrEngine for ZarrEngine {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(vec![])
    }

    fn query_location(
        &self,
        _location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError> {
        Err(DataServerError::InvalidParameter(
            "Zarr engine does not support location queries (use a position query)".into(),
        ))
    }

    fn get_parameters(&self) -> Vec<String> {
        self.catalog
            .load()
            .vars
            .iter()
            .map(|v| v.name.clone())
            .collect()
    }

    fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
        self.catalog
            .load()
            .vars
            .iter()
            .map(|v| {
                (
                    v.name.clone(),
                    ParameterDescription {
                        label: v.label.clone(),
                        unit: v.units.clone(),
                        observed_property: v.name.clone(),
                    },
                )
            })
            .collect()
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let cat = self.catalog.load();
        Some((*cat.times.first()?, *cat.times.last()?))
    }

    fn get_available_times(&self) -> Option<Vec<DateTime<Utc>>> {
        let times = self.catalog.load().times.clone();
        if times.is_empty() {
            None
        } else {
            Some(times)
        }
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some(self.catalog.load().extent)
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["position".to_string()]
    }

    fn query_position(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError> {
        let (lon, lat) = parse_coords(coords)?;
        let cat = self.catalog.load();
        if cat.times.is_empty() {
            return Err(DataServerError::Engine("No Zarr data available".into()));
        }

        let time_idx: Vec<usize> = match datetime {
            None => (0..cat.times.len()).collect(),
            Some((start, end)) => cat
                .times
                .iter()
                .enumerate()
                .filter(|(_, t)| **t >= start && **t <= end)
                .map(|(i, _)| i)
                .collect(),
        };
        if time_idx.is_empty() {
            return Err(DataServerError::InvalidParameter(
                "No data available for the requested time range".into(),
            ));
        }
        let out_times: Vec<DateTime<Utc>> = time_idx.iter().map(|&i| cat.times[i]).collect();

        let selected: Vec<&catalog::Variable> = cat
            .vars
            .iter()
            .filter(|v| {
                parameters.is_none_or(|f| f.iter().any(|p| p.eq_ignore_ascii_case(&v.name)))
            })
            .collect();
        if selected.is_empty() {
            return Err(DataServerError::InvalidParameter(
                "No matching parameters found".into(),
            ));
        }

        let mut params_map = HashMap::new();
        let mut ranges = HashMap::new();
        for v in selected {
            let values = cat.sample_series(v, lon, lat, &time_idx)?;
            params_map.insert(
                v.name.clone(),
                ParameterDescription {
                    label: v.label.clone(),
                    unit: v.units.clone(),
                    observed_property: v.name.clone(),
                },
            );
            ranges.insert(
                v.name.clone(),
                NdArray {
                    shape: vec![out_times.len()],
                    axis_names: vec!["t".to_string()],
                    values,
                },
            );
        }

        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::PointSeries {
                x: lon,
                y: lat,
                t: out_times,
                z: None,
            },
            parameters: params_map,
            ranges,
        }))
    }
}

/// Build the `ds-storage`-backed store for a Zarr collection.
///
/// - Remote: `endpoint` + `bucket` (+ required `path`) → an S3 store rooted at
///   `path` within the bucket.
/// - Local: `data_path` (a directory, or an `s3://` / `http(s)://` URL),
///   optionally suffixed by `path`. `ds_storage::build_store` picks the backend.
fn build_store(collection_id: &str, config: &ZarrConfig) -> Result<DsStore, DataServerError> {
    if let (Some(endpoint), Some(bucket)) = (config.endpoint.as_deref(), config.bucket.as_deref()) {
        // `path` is required for a remote source — enforced in
        // `ServerConfig::validate` ("remote zarr (endpoint+bucket) requires
        // 'path'"), so by here it is always present.
        let path = config.path.as_deref().unwrap_or_default();
        let ds = ds_storage::build_s3_store_from_parts(endpoint, bucket).map_err(|e| {
            DataServerError::Config(format!(
                "Collection '{collection_id}': failed to open S3 Zarr store \
                 (endpoint={endpoint}, bucket={bucket}): {e}"
            ))
        })?;
        return Ok(DsStore::new(ds, path, config.cache_mb));
    }

    let data_path = config.data_path.as_deref().ok_or_else(|| {
        DataServerError::Config(format!(
            "Collection '{collection_id}': zarr engine requires 'data_path' or 'endpoint'+'bucket'"
        ))
    })?;
    // Optional `path` is a relative sub-path under `data_path` (config-validated
    // to contain no leading slash or `..`).
    let location = match &config.path {
        Some(p) => format!(
            "{}/{}",
            data_path.trim_end_matches('/'),
            p.trim_matches('/')
        ),
        None => data_path.to_string(),
    };
    let (ds, prefix) = ds_storage::build_store(&location).map_err(|e| {
        DataServerError::Config(format!(
            "Collection '{collection_id}': failed to open Zarr store '{location}': {e}"
        ))
    })?;
    Ok(DsStore::new(
        ds,
        prefix.as_ref().to_string(),
        config.cache_mb,
    ))
}

fn log_loaded(collection_id: &str, cat: &Catalog) {
    tracing::info!(
        "[{}] Loaded Zarr store: {} variable(s) [{}], {} time step(s)",
        collection_id,
        cat.vars.len(),
        cat.vars
            .iter()
            .map(|v| v.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        cat.times.len(),
    );
}

/// Parse EDR position coordinates: `POINT(lon lat)` or `lon,lat`. Returns
/// `(lon, lat)`.
fn parse_coords(coords: &str) -> Result<(f64, f64), DataServerError> {
    let trimmed = coords.trim();
    if let Some(inner) = trimmed
        .strip_prefix("POINT(")
        .or_else(|| trimmed.strip_prefix("POINT ("))
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.split_whitespace().collect();
        if parts.len() == 2 {
            let lon = parts[0].parse().map_err(|_| {
                DataServerError::InvalidParameter(format!("Invalid longitude: {}", parts[0]))
            })?;
            let lat = parts[1].parse().map_err(|_| {
                DataServerError::InvalidParameter(format!("Invalid latitude: {}", parts[1]))
            })?;
            return Ok((lon, lat));
        }
    }
    let parts: Vec<&str> = trimmed.split(',').collect();
    if parts.len() == 2 {
        let lon = parts[0].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid longitude: {}", parts[0]))
        })?;
        let lat = parts[1].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid latitude: {}", parts[1]))
        })?;
        return Ok((lon, lat));
    }
    Err(DataServerError::InvalidParameter(
        "Expected POINT(lon lat) or lon,lat format".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_point_and_csv() {
        assert_eq!(parse_coords("POINT(24.9 60.2)").unwrap(), (24.9, 60.2));
        assert_eq!(parse_coords("24.9,60.2").unwrap(), (24.9, 60.2));
        assert!(parse_coords("garbage").is_err());
    }
}
