//! GRIB message fetch + decode.
//!
//! Uses the `grib` crate to decode GRIB2 messages fetched via byte-range
//! reads from `ds_storage::DataStore`.

use std::sync::Arc;

use ds_core::error::DataServerError;
use ds_storage::DataStore;
use grib::{Grib2SubmessageDecoder, GridDefinitionTemplateValues, GridPointIndex};

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

    let layout = extract_grid_params(grid_def).map_err(|reason| {
        DataServerError::Engine(format!(
            "Invalid or unsupported GRIB2 grid for {param}: {reason}"
        ))
    })?;
    let expected = layout.ni.checked_mul(layout.nj).ok_or_else(|| {
        DataServerError::Engine(format!("GRIB2 grid dimensions overflow for {param}"))
    })?;
    if u64::from(grid_def.num_points()) != expected as u64 {
        return Err(DataServerError::Engine(format!(
            "GRIB2 grid point count mismatch for {param}"
        )));
    }

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
    let mut decoded = decoder.dispatch().map_err(|e| {
        DataServerError::Engine(format!("Failed to decode GRIB2 values for {param}: {e}"))
    })?;

    // Preserve the usual row-major scan's allocation and decode fast path.
    // For other scan modes, place values using the decoder's storage-order
    // index iterator, then normalize both axes to west→east / north→south.
    let values: Vec<f64> = if layout.canonical_scan {
        decoded.map(f64::from).collect()
    } else {
        let mut values = vec![f64::NAN; expected];
        for (i, j) in layout.indices {
            let value = decoded.next().ok_or_else(|| {
                DataServerError::Engine(format!("Too few GRIB2 values for {param}"))
            })?;
            let col = if layout.reverse_i {
                layout.ni - 1 - i
            } else {
                i
            };
            let row = if layout.reverse_j {
                layout.nj - 1 - j
            } else {
                j
            };
            values[row * layout.ni + col] = f64::from(value);
        }
        if decoded.next().is_some() {
            return Err(DataServerError::Engine(format!(
                "Too many GRIB2 values for {param}"
            )));
        }
        values
    };
    if values.len() != expected {
        return Err(DataServerError::Engine(format!(
            "GRIB2 grid size mismatch for {param}: expected {expected}, got {}",
            values.len()
        )));
    }

    Ok(DecodedGrid {
        ni: layout.ni,
        nj: layout.nj,
        lon_first: layout.lon_first,
        lat_first: layout.lat_first,
        lon_inc: layout.lon_inc,
        lat_inc: layout.lat_inc,
        values: Arc::new(values),
        triple: (discipline, category, number),
        centre,
        first_surface_type,
        first_surface_value,
    })
}

struct GridLayout {
    ni: usize,
    nj: usize,
    lon_first: f64,
    lat_first: f64,
    lon_inc: f64,
    lat_inc: f64,
    indices: grib::GridPointIndexIterator,
    reverse_i: bool,
    reverse_j: bool,
    canonical_scan: bool,
}

/// Normalize the regular-grid geometry to west→east / north→south. The
/// index iterator handles column-major and alternating-row storage.
fn extract_grid_params(grid_def: &grib::GridDefinition) -> Result<GridLayout, String> {
    let GridDefinitionTemplateValues::Template0(template) =
        GridDefinitionTemplateValues::try_from(grid_def).map_err(|e| e.to_string())?
    else {
        return Err("only regular latitude/longitude grids are supported".into());
    };
    let ll = template.lat_lon;
    let grid = &ll.grid;
    if grid.ni == 0 || grid.nj == 0 || grid.ni == u32::MAX || grid.nj == u32::MAX {
        return Err("missing or zero grid dimensions".into());
    }
    let angle = if grid.initial_production_domain_basic_angle == 0 {
        1e-6
    } else {
        if grid.basic_angle_subdivisions == 0 || grid.basic_angle_subdivisions == u32::MAX {
            return Err("invalid basic-angle subdivisions".into());
        }
        f64::from(grid.initial_production_domain_basic_angle)
            / f64::from(grid.basic_angle_subdivisions)
    };
    if ll.i_direction_inc == 0
        || ll.j_direction_inc == 0
        || ll.i_direction_inc == u32::MAX
        || ll.j_direction_inc == u32::MAX
    {
        return Err("missing or zero grid increments".into());
    }
    let ni = grid.ni as usize;
    let nj = grid.nj as usize;
    let lon_inc = f64::from(ll.i_direction_inc) * angle;
    let lat_inc = f64::from(ll.j_direction_inc) * angle;
    let reverse_i = !ll.scanning_mode.scans_positively_for_i();
    let reverse_j = ll.scanning_mode.scans_positively_for_j();
    let lon_first = f64::from(grid.first_point_lon) * angle
        - if reverse_i {
            (ni - 1) as f64 * lon_inc
        } else {
            0.0
        };
    let lat_first = f64::from(grid.first_point_lat) * angle
        + if reverse_j {
            (nj - 1) as f64 * lat_inc
        } else {
            0.0
        };
    Ok(GridLayout {
        ni,
        nj,
        lon_first: ds_core::geo::wrap_lon(lon_first),
        lat_first,
        lon_inc,
        lat_inc: -lat_inc,
        indices: ll.ij().map_err(|e| e.to_string())?,
        reverse_i,
        reverse_j,
        canonical_scan: ll.scanning_mode.0 == 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_all_rectangular_scan_orders() {
        // Same physical grid in every storage order: NW=10, NE=20,
        // SW=30, SE=40. This table is independent of grib's index iterator.
        let cases = [
            (0x00, [10, 20, 30, 40]),
            (0x10, [10, 20, 40, 30]),
            (0x20, [10, 30, 20, 40]),
            (0x30, [10, 30, 40, 20]),
            (0x40, [30, 40, 10, 20]),
            (0x50, [30, 40, 20, 10]),
            (0x60, [30, 10, 40, 20]),
            (0x70, [30, 10, 20, 40]),
            (0x80, [20, 10, 40, 30]),
            (0x90, [20, 10, 30, 40]),
            (0xa0, [20, 40, 10, 30]),
            (0xb0, [20, 40, 30, 10]),
            (0xc0, [40, 30, 20, 10]),
            (0xd0, [40, 30, 10, 20]),
            (0xe0, [40, 20, 30, 10]),
            (0xf0, [40, 20, 10, 30]),
        ];
        for (scan, packed) in cases {
            let bytes = crate::test_support::message(scan, 0.0, packed, 103, 2);
            let grid = decode_message(&bytes, "TMP").unwrap();
            assert_eq!(*grid.values, vec![10.0, 20.0, 30.0, 40.0], "mode {scan:x}");
            assert_eq!(
                (grid.lon_first, grid.lat_first, grid.lon_inc, grid.lat_inc),
                (0.0, 1.0, 1.0, -1.0)
            );
            assert_eq!(grid.bilinear_value(0.5, 0.5), Some(25.0));
        }
    }

    #[test]
    fn basic_angle_is_respected_and_unsupported_scan_flags_rejected() {
        let mut bytes = crate::test_support::message(0, 0.0, [10, 20, 30, 40], 103, 2);
        // Section 3 starts after 16-byte indicator + 21-byte identification.
        let s3 = 37;
        bytes[s3 + 38..s3 + 42].copy_from_slice(&1u32.to_be_bytes());
        bytes[s3 + 42..s3 + 46].copy_from_slice(&1u32.to_be_bytes());
        for offset in [46, 59, 63, 67] {
            bytes[s3 + offset..s3 + offset + 4].copy_from_slice(&1u32.to_be_bytes());
        }
        let grid = decode_message(&bytes, "TMP").unwrap();
        assert_eq!(
            (grid.lon_inc, grid.lat_inc, grid.lat_first),
            (1.0, -1.0, 1.0)
        );
        bytes[s3 + 71] = 1; // unsupported staggered/offset grid flag
        assert!(decode_message(&bytes, "TMP").is_err());
    }

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
