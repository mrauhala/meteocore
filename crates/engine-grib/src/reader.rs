//! GRIB message fetch + decode.
//!
//! Uses the `grib` crate to decode GRIB2 messages fetched via byte-range
//! reads from `ds_storage::DataStore`.

use std::sync::Arc;

use ds_core::error::DataServerError;
use ds_storage::DataStore;
use grib::{Grib2SubmessageDecoder, GridDefinitionTemplateValues};

use crate::cache::DecodedGrid;
use crate::catalog::MessageEntry;

/// Fetch and decode a single GRIB message from a data store.
///
/// If `entry.length` is `Some(_)`, a direct byte-range read is performed.
/// If it is `None` (the last record in a wgrib2 index file whose tail length
/// has not been resolved yet), this function issues a HEAD request on the
/// data file, computes the length as `file_size - offset`, and then fetches
/// the bytes.
pub fn read_message(
    store: &DataStore,
    path: &ds_storage::object_store::path::Path,
    entry: &MessageEntry,
) -> Result<DecodedGrid, DataServerError> {
    let length = match entry.length {
        Some(l) => l,
        None => {
            let meta = store.head(path).map_err(|e| {
                DataServerError::Storage(format!(
                    "Failed to HEAD {path} for tail-length resolution of {}: {e}",
                    entry.param
                ))
            })?;
            let file_size = meta.size;
            if file_size <= entry.offset {
                return Err(DataServerError::Storage(format!(
                    "File {path} size {file_size} <= tail offset {}; \
                     data file may be truncated or mid-upload",
                    entry.offset
                )));
            }
            file_size - entry.offset
        }
    };

    let range = entry.offset as usize..(entry.offset + length) as usize;
    let bytes = store.get_range(path, range).map_err(|e| {
        DataServerError::Storage(format!(
            "Failed to fetch GRIB message {}/{}: {}",
            entry.param,
            entry.level.map_or("sfc".to_string(), |l| l.to_string()),
            e
        ))
    })?;

    decode_message(&bytes, &entry.param)
}

/// Decode a GRIB2 message from raw bytes.
pub fn decode_message(bytes: &[u8], param: &str) -> Result<DecodedGrid, DataServerError> {
    let grib2 = grib::from_reader(std::io::Cursor::new(bytes)).map_err(|e| {
        DataServerError::Engine(format!("Failed to parse GRIB2 message for {param}: {e}"))
    })?;

    // A single byte-range-fetched message should contain exactly one submessage
    let (_index, submessage) = grib2.iter().next().ok_or_else(|| {
        DataServerError::Engine(format!("GRIB2 message for {param} contains no submessages"))
    })?;

    // Extract originating centre from Section 1 and discipline from Section 0.
    // Both are per-submessage accessors in grib 0.15.
    let centre = submessage.identification().centre_id();
    let discipline = submessage.indicator().discipline;

    // Extract grid definition
    let grid_def = submessage.grid_def();

    let (ni, nj, lon_first, lat_first, lon_inc, lat_inc) = extract_grid_params(grid_def)
        .ok_or_else(|| {
            DataServerError::Engine(format!(
                "Unsupported grid type in GRIB2 message for {param}"
            ))
        })?;

    // Extract the parameter triple from the Product Definition Section.
    // `parameter_category`/`parameter_number` are `Option<u8>` (absent for
    // some obscure templates); for anything we might render, both are set.
    let prod_def = submessage.prod_def();
    let category = prod_def.parameter_category().ok_or_else(|| {
        DataServerError::Engine(format!(
            "GRIB2 message for {param} missing parameter category"
        ))
    })?;
    let number = prod_def.parameter_number().ok_or_else(|| {
        DataServerError::Engine(format!(
            "GRIB2 message for {param} missing parameter number"
        ))
    })?;

    // First fixed surface (GRIB2 Code Table 4.5). Used to distinguish, e.g.,
    // mean sea level pressure from surface pressure (both encode as WMO
    // triple (0, 3, 0) "Pressure" but have different surface types).
    let (first_surface_type, first_surface_value) = match prod_def.fixed_surfaces() {
        Some((s1, _s2)) => {
            let v = s1.value();
            (s1.surface_type, if v.is_nan() { None } else { Some(v) })
        }
        None => (255, None),
    };

    // Decode values using Grib2SubmessageDecoder
    let decoder = Grib2SubmessageDecoder::from(submessage).map_err(|e| {
        DataServerError::Engine(format!("Failed to create decoder for {param}: {e}"))
    })?;
    let decoded = decoder.dispatch().map_err(|e| {
        DataServerError::Engine(format!("Failed to decode GRIB2 values for {param}: {e}"))
    })?;

    // Convert f32 values to f64, NaN → value (not filtered)
    let values: Vec<f64> = decoded.map(|v| v as f64).collect();

    let expected = ni * nj;
    if values.len() != expected {
        return Err(DataServerError::Engine(format!(
            "GRIB2 grid size mismatch for {param}: expected {expected}, got {}",
            values.len()
        )));
    }

    Ok(DecodedGrid {
        ni,
        nj,
        lon_first,
        lat_first,
        lon_inc,
        lat_inc,
        values: Arc::new(values),
        triple: (discipline, category, number),
        centre,
        first_surface_type,
        first_surface_value,
    })
}

/// Extract grid parameters from a GRIB2 grid definition.
/// Returns (ni, nj, lon_first, lat_first, lon_inc, lat_inc) for regular lat/lon grids.
fn extract_grid_params(
    grid_def: &grib::GridDefinition,
) -> Option<(usize, usize, f64, f64, f64, f64)> {
    let template_values = GridDefinitionTemplateValues::try_from(grid_def).ok()?;

    match template_values {
        GridDefinitionTemplateValues::Template0(template) => {
            let ni = template.lat_lon.grid.ni as usize;
            let nj = template.lat_lon.grid.nj as usize;

            // GRIB uses microdegrees (values * 1e-6)
            let lat_first = template.lat_lon.grid.first_point_lat as f64 / 1_000_000.0;
            let lon_first = template.lat_lon.grid.first_point_lon as f64 / 1_000_000.0;
            let lat_last = template.lat_lon.grid.last_point_lat as f64 / 1_000_000.0;

            let lon_inc = template.lat_lon.i_direction_inc as f64 / 1_000_000.0;
            let lat_inc = if lat_first > lat_last {
                -(template.lat_lon.j_direction_inc as f64 / 1_000_000.0) // N→S scan
            } else {
                template.lat_lon.j_direction_inc as f64 / 1_000_000.0 // S→N scan
            };

            // Normalize lon_first to [-180, 180)
            let lon_first = if lon_first > 180.0 {
                lon_first - 360.0
            } else {
                lon_first
            };

            Some((ni, nj, lon_first, lat_first, lon_inc, lat_inc))
        }
        _ => {
            tracing::warn!("Unsupported GRIB grid template type");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_ecmwf_sample() {
        // This test uses the sample GRIB message downloaded from ECMWF open data:
        // s3://ecmwf-forecasts/20260405/00z/ifs/0p25/oper/20260405000000-0h-oper-fc.grib2
        // bytes 0-572554 (first message: specific humidity at 150 hPa)
        let path = std::path::Path::new("../../testdata/ecmwf/sample-message.grib2");
        if !path.exists() {
            eprintln!("Skipping decode test: sample data not available");
            return;
        }

        let bytes = std::fs::read(path).unwrap();
        let grid = decode_message(&bytes, "q").unwrap();

        // ECMWF IFS 0.25° global grid
        assert_eq!(grid.ni, 1440, "Expected 1440 longitude points");
        assert_eq!(grid.nj, 721, "Expected 721 latitude points");
        assert!(
            (grid.lon_inc - 0.25).abs() < 1e-6,
            "Expected 0.25° lon increment"
        );
        assert!(
            (grid.lat_inc - (-0.25)).abs() < 1e-6,
            "Expected -0.25° lat increment (N→S)"
        );
        assert!(
            (grid.lat_first - 90.0).abs() < 1e-6,
            "Expected first lat = 90°N"
        );
        assert_eq!(grid.values.len(), 1440 * 721);

        // Sanity: values should be physically reasonable for specific humidity at 150 hPa
        // (typically 0 to ~0.001 kg/kg in the stratosphere)
        let non_nan_count = grid.values.iter().filter(|v| !v.is_nan()).count();
        assert!(non_nan_count > 0, "Expected some non-NaN values");

        // Test nearest value extraction
        let helsinki = grid.nearest_value(25.0, 60.0);
        assert!(helsinki.is_some(), "Expected a value at Helsinki");
    }
}
