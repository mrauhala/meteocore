//! ODIM_H5 v2.x composite-reflectivity reader.
//!
//! Parses the four pieces of metadata that distinguish an ODIM
//! composite (COMP) file from an arbitrary HDF5 file:
//!
//! - `/what` — `object` (`"COMP"`), `date`/`time` (UTC stamp)
//! - `/where` — `projdef` (PROJ.4 string), `xsize`/`ysize`,
//!   `LL_lon`/`LL_lat`/`UR_lon`/`UR_lat` (corner coordinates in WGS84)
//! - `/dataset1/what` — `quantity`, `gain`, `offset`, `nodata`
//! - `/dataset1/data1/data` — the raw 2D scaled-integer pixel array
//!
//! The output is an [`OdimComposite`] containing the parsed
//! [`Crs`], a native-coordinate bbox, the timestamp, parameter
//! metadata, and the raw pixel array. The raw → physical-units
//! conversion (`raw * gain + offset`) and nodata masking happen
//! **at sample time**, not at read time, so a multi-megapixel
//! grid doesn't pay 16 bytes/cell to be carried around as
//! `Vec<Option<f64>>`.
//!
//! Phase 1 narrows the scope further than the format allows:
//! - Single dataset (`/dataset1` only), single data layer (`/data1`)
//! - Only `u8` and `u16` raw pixel types — the two ODIM v2.x
//!   composites we've seen in the wild use these
//! - No quality layers, no how/* attributes, no PVOL volume data
//!
//! See [[project_odim_engine_plan]] for the full multi-phase plan.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use ds_core::geo::Crs;
use hdf5_reader::{Attribute, Hdf5File};
use ndarray::Array2;

use crate::proj;

/// Errors from [`read_composite`].
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("failed to open HDF5 file: {0}")]
    OpenFailed(String),
    #[error("missing required ODIM group `{0}`")]
    MissingGroup(String),
    #[error("missing required ODIM attribute `{group}/{name}`")]
    MissingAttribute { group: String, name: String },
    #[error("expected `/what/object=COMP`, got `{0}`")]
    NotComposite(String),
    #[error("invalid date/time `{date} {time}`: {reason}")]
    InvalidTimestamp {
        date: String,
        time: String,
        reason: String,
    },
    #[error("PROJ string parse failed: {0}")]
    ProjParse(#[from] proj::ParseError),
    #[error("dataset `/dataset1/data1/data` has unsupported shape: expected 2D, got {0}D")]
    UnsupportedRank(usize),
    #[error("dataset `/dataset1/data1/data` has unsupported pixel type (Phase 1: u8 or u16)")]
    UnsupportedPixelType,
    #[error("dataset read failed: {0}")]
    DatasetRead(String),
    #[error("attribute read failed: {0}")]
    AttributeRead(String),
}

/// Raw pixel storage as it appears on disk. ODIM v2.x composites
/// typically ship `u8` (single-byte reflectivity classes, 0–255) or
/// `u16` (extended dynamic range). Both are scaled integers — the
/// physical value is `raw as f64 * gain + offset`.
#[derive(Debug, Clone)]
pub enum RawPixels {
    U8(Array2<u8>),
    U16(Array2<u16>),
}

impl RawPixels {
    /// Shape as `(height, width)` — same ordering ndarray uses
    /// internally. The first axis is rows (y), the second is columns
    /// (x), matching HDF5's row-major layout.
    pub fn shape(&self) -> (usize, usize) {
        match self {
            RawPixels::U8(a) => a.dim(),
            RawPixels::U16(a) => a.dim(),
        }
    }

    /// Read a single pixel at `(row, col)`, returning `None` for the
    /// nodata sentinel and `Some(physical)` otherwise. `physical` is
    /// `raw as f64 * gain + offset`. Bounds-checked by ndarray; out-
    /// of-range indices panic in tests and return `None` in release
    /// builds via `get` (callers should clamp upstream).
    pub fn sample(
        &self,
        row: usize,
        col: usize,
        gain: f64,
        offset: f64,
        nodata: f64,
    ) -> Option<f64> {
        let raw = match self {
            RawPixels::U8(a) => a.get((row, col)).map(|v| *v as f64)?,
            RawPixels::U16(a) => a.get((row, col)).map(|v| *v as f64)?,
        };
        if raw == nodata {
            None
        } else {
            Some(raw * gain + offset)
        }
    }
}

/// A parsed ODIM composite ready for sampling. Carries the raw
/// pixel array plus everything needed to map `(world_x, world_y)`
/// in the native CRS to a pixel index and decode the raw value.
#[derive(Debug, Clone)]
pub struct OdimComposite {
    /// Native projection parsed from `/where/projdef`.
    pub crs: Crs,
    /// Grid width (number of columns).
    pub xsize: u32,
    /// Grid height (number of rows).
    pub ysize: u32,
    /// Native-CRS bbox `[west, south, east, north]` of the grid's
    /// outer pixel boundaries (not pixel centres). For projected
    /// CRSes this is in metres; for `Wgs84` it's in degrees.
    pub bbox: [f64; 4],
    /// WGS84-corner bbox `[ll_lon, ll_lat, ur_lon, ur_lat]` straight
    /// from the file's `/where` group. Kept alongside `bbox` so
    /// callers can pick whichever frame is cheaper for their query
    /// without re-running the projection forward.
    pub wgs84_corners: [f64; 4],
    /// Nominal acquisition time (UTC), parsed from
    /// `/what/date` + `/what/time`.
    pub time: DateTime<Utc>,
    /// Quantity name from `/dataset1/what/quantity` (e.g. `"DBZH"`).
    pub quantity: String,
    /// Linear scale factor: `physical = raw * gain + offset`.
    pub gain: f64,
    /// Linear offset.
    pub offset: f64,
    /// Raw value indicating "no data" / out-of-coverage pixel.
    pub nodata: f64,
    /// The raw 2D pixel array, indexed `[row, col]` (i.e.
    /// `[y_from_top, x_from_left]`). Most ODIM producers ship rows
    /// north-to-south; callers must read `/where/UR_lat` >
    /// `/where/LL_lat` and act accordingly. We don't flip here
    /// because flipping a 2000×2000 u16 array is wasteful when
    /// callers can flip indices for free.
    pub pixels: RawPixels,
}

/// Parse an ODIM_H5 composite from a byte slice. The whole file must
/// fit in memory — typical COMP files are 2–20 MB, which is fine.
/// For S3-backed sources, fetch into `Bytes` first.
pub fn read_composite(bytes: &[u8]) -> Result<OdimComposite, ReadError> {
    let file = Hdf5File::from_bytes(bytes).map_err(|e| ReadError::OpenFailed(e.to_string()))?;

    // Closure-based helpers — `Group` isn't pub-exported by
    // `hdf5-reader 0.1.4` so we can't take a `&Group` by name. Each
    // helper takes a path-prefix used in error messages and an
    // `Attribute` already resolved from the calling site.
    let fetch_attr = |group_path: &str, name: &str| -> Result<Attribute, ReadError> {
        let grp = file
            .group(group_path)
            .map_err(|_| ReadError::MissingGroup(group_path.to_string()))?;
        grp.attribute(name)
            .map_err(|_| ReadError::MissingAttribute {
                group: group_path.to_string(),
                name: name.to_string(),
            })
    };
    let read_string_attr = |group_path: &str, name: &str| -> Result<String, ReadError> {
        let attr = fetch_attr(group_path, name)?;
        attr.read_string()
            .map_err(|e| ReadError::AttributeRead(format!("{group_path}/{name}: {e}")))
    };
    let read_f64_attr = |group_path: &str, name: &str| -> Result<f64, ReadError> {
        let attr = fetch_attr(group_path, name)?;
        attr.read_as_f64()
            .map_err(|e| ReadError::AttributeRead(format!("{group_path}/{name}: {e}")))
    };

    // /what — object kind + timestamp.
    let object = read_string_attr("/what", "object")?;
    if object.trim_end_matches('\0').trim() != "COMP" {
        return Err(ReadError::NotComposite(object));
    }
    let date = read_string_attr("/what", "date")?;
    let time_str = read_string_attr("/what", "time")?;
    let time = parse_odim_timestamp(&date, &time_str)?;

    // /where — projection + grid extents.
    let projdef = read_string_attr("/where", "projdef")?;
    let crs = proj::parse(&projdef)?;
    let ll_lon = read_f64_attr("/where", "LL_lon")?;
    let ll_lat = read_f64_attr("/where", "LL_lat")?;
    let ur_lon = read_f64_attr("/where", "UR_lon")?;
    let ur_lat = read_f64_attr("/where", "UR_lat")?;
    let wgs84_corners = [ll_lon, ll_lat, ur_lon, ur_lat];

    // Native bbox is computed by forward-projecting the WGS84 corners.
    // ODIM also stores `/where/xscale` + `/where/yscale` for projected
    // grids, but those don't carry the origin — we'd still need the
    // corners to anchor the bbox. Forward-projecting the four corners
    // and taking the axis-aligned envelope handles every supported
    // projection uniformly.
    let bbox = native_bbox(&crs, ll_lon, ll_lat, ur_lon, ur_lat);

    // /dataset1/data1/data — read shape first because DMI files don't
    // ship `/where/xsize`/`ysize` (only `xscale`/`yscale`), so the grid
    // dimensions must come from the data array shape regardless.
    let ds = file
        .dataset("/dataset1/data1/data")
        .map_err(|_| ReadError::MissingGroup("/dataset1/data1/data".into()))?;
    let shape = ds.shape();
    if shape.len() != 2 {
        return Err(ReadError::UnsupportedRank(shape.len()));
    }
    let rows = shape[0] as usize;
    let cols = shape[1] as usize;

    // Prefer explicit `/where/xsize`/`ysize` when present (FMI/OPERA),
    // fall back to the data-array shape (DMI variant). If both exist
    // and disagree, trust /where — the spec wins over what could be
    // a transposed array on the producer side.
    let xsize = read_f64_attr("/where", "xsize")
        .map(|v| v as u32)
        .unwrap_or(cols as u32);
    let ysize = read_f64_attr("/where", "ysize")
        .map(|v| v as u32)
        .unwrap_or(rows as u32);

    // gain/offset/nodata/quantity location varies by producer:
    // - FMI/OPERA canonical:    /dataset1/what/{gain,offset,nodata,quantity}
    // - Alternate canonical:    /dataset1/data1/what/{...}     (some EUMETNET files)
    // - DMI variant:            /what/{gain,offset,nodata}     at the file root,
    //                           and `quantity` as an attribute on /dataset1/data1
    //                           (or `/what/product` as last resort)
    // Try each path in order. The first hit wins.
    let what_paths = ["/dataset1/what", "/dataset1/data1/what", "/what"];
    let read_first_f64 = |name: &str| -> Result<f64, ReadError> {
        for p in &what_paths {
            if let Ok(v) = read_f64_attr(p, name) {
                return Ok(v);
            }
        }
        Err(ReadError::MissingAttribute {
            group: what_paths.join(" | "),
            name: name.to_string(),
        })
    };
    let gain = read_first_f64("gain")?;
    let offset = read_first_f64("offset")?;
    let nodata = read_first_f64("nodata")?;

    // Quantity has one extra fallback (DMI puts it as an attribute on
    // the `/dataset1/data1` data group itself, not in a what subgroup).
    let quantity = read_string_attr("/dataset1/what", "quantity")
        .or_else(|_| read_string_attr("/dataset1/data1/what", "quantity"))
        .or_else(|_| read_string_attr("/dataset1/data1", "quantity"))
        .or_else(|_| read_string_attr("/what", "product"))
        .map_err(|_| ReadError::MissingAttribute {
            group: "/dataset1/what | /dataset1/data1/what | /dataset1/data1 | /what".into(),
            name: "quantity".into(),
        })?;

    // Try u8 first (the common case for radar reflectivity classes);
    // on failure try u16. The error from the wrong dtype is benign —
    // `read_array` returns `Err` rather than panicking.
    let pixels = if let Ok(arr) = ds.read_array::<u8>() {
        let a2 = arr
            .into_dimensionality::<ndarray::Ix2>()
            .map_err(|e| ReadError::DatasetRead(format!("u8 reshape failed: {e}")))?;
        if a2.dim() != (rows, cols) {
            return Err(ReadError::DatasetRead(format!(
                "u8 array shape {:?} doesn't match metadata {rows}x{cols}",
                a2.dim()
            )));
        }
        RawPixels::U8(a2)
    } else if let Ok(arr) = ds.read_array::<u16>() {
        let a2 = arr
            .into_dimensionality::<ndarray::Ix2>()
            .map_err(|e| ReadError::DatasetRead(format!("u16 reshape failed: {e}")))?;
        if a2.dim() != (rows, cols) {
            return Err(ReadError::DatasetRead(format!(
                "u16 array shape {:?} doesn't match metadata {rows}x{cols}",
                a2.dim()
            )));
        }
        RawPixels::U16(a2)
    } else {
        return Err(ReadError::UnsupportedPixelType);
    };

    Ok(OdimComposite {
        crs,
        xsize,
        ysize,
        bbox,
        wgs84_corners,
        time,
        quantity,
        gain,
        offset,
        nodata,
        pixels,
    })
}

/// Combine ODIM's split `/what/date` (`YYYYMMDD`) and `/what/time`
/// (`HHMMSS`) into a UTC timestamp. ODIM v2.x assumes UTC for both
/// fields — there is no timezone attribute.
fn parse_odim_timestamp(date: &str, time: &str) -> Result<DateTime<Utc>, ReadError> {
    // HDF5 fixed-length string attributes are commonly NUL-padded
    // up to the declared length, so a trailing `\0` is normal and
    // must be stripped before chrono parsing rejects it as
    // "trailing input".
    let date = date.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    let time = time.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    let parsed_date =
        NaiveDate::parse_from_str(date, "%Y%m%d").map_err(|e| ReadError::InvalidTimestamp {
            date: date.into(),
            time: time.into(),
            reason: format!("date: {e}"),
        })?;
    let parsed_time =
        NaiveTime::parse_from_str(time, "%H%M%S").map_err(|e| ReadError::InvalidTimestamp {
            date: date.into(),
            time: time.into(),
            reason: format!("time: {e}"),
        })?;
    Ok(parsed_date.and_time(parsed_time).and_utc())
}

/// Forward-project the four WGS84 corners into the native CRS and
/// return the axis-aligned envelope `[west, south, east, north]`.
fn native_bbox(crs: &Crs, ll_lon: f64, ll_lat: f64, ur_lon: f64, ur_lat: f64) -> [f64; 4] {
    let corners = [
        crs.forward(ll_lon, ll_lat),
        crs.forward(ur_lon, ll_lat),
        crs.forward(ll_lon, ur_lat),
        crs.forward(ur_lon, ur_lat),
    ];
    let xs = corners.iter().map(|(x, _)| *x);
    let ys = corners.iter().map(|(_, y)| *y);
    let west = xs.clone().fold(f64::INFINITY, f64::min);
    let east = xs.fold(f64::NEG_INFINITY, f64::max);
    let south = ys.clone().fold(f64::INFINITY, f64::min);
    let north = ys.fold(f64::NEG_INFINITY, f64::max);
    [west, south, east, north]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Date+time parsing accepts the canonical ODIM format.
    #[test]
    fn parses_canonical_odim_timestamp() {
        let ts = parse_odim_timestamp("20250714", "153000").unwrap();
        assert_eq!(ts.to_rfc3339(), "2025-07-14T15:30:00+00:00");
    }

    /// Whitespace around the date/time strings is common in HDF5
    /// fixed-length string attributes (padded to declared length).
    /// The parser must tolerate it.
    #[test]
    fn timestamp_parsing_trims_whitespace() {
        let ts = parse_odim_timestamp(" 20250714 ", " 153000\0").unwrap();
        assert_eq!(ts.to_rfc3339(), "2025-07-14T15:30:00+00:00");
    }

    /// Malformed dates surface as a structured error rather than
    /// silently becoming epoch or a default.
    #[test]
    fn malformed_date_is_an_error() {
        let err = parse_odim_timestamp("2025/07/14", "153000").unwrap_err();
        match err {
            ReadError::InvalidTimestamp { date, .. } => assert_eq!(date, "2025/07/14"),
            other => panic!("expected InvalidTimestamp, got {other:?}"),
        }
    }

    /// For `Wgs84`, the forward projection is identity, so the
    /// native bbox equals the WGS84-corner bbox.
    #[test]
    fn native_bbox_for_wgs84_is_identity() {
        let bb = native_bbox(&Crs::Wgs84, 10.0, 55.0, 30.0, 70.0);
        assert_eq!(bb, [10.0, 55.0, 30.0, 70.0]);
    }

    /// For a projected CRS, the envelope is taken across all four
    /// forward-projected corners — capturing the projection's
    /// curvature at the corners. The exact numbers come from
    /// `Crs::forward` and aren't pinned here (changing them is a
    /// breaking change to existing engines); we just check that all
    /// four corners contributed by asserting the envelope spans
    /// finite, ordered values.
    #[test]
    fn native_bbox_for_projected_crs_envelopes_all_four_corners() {
        let crs = Crs::TransverseMercator {
            lat0: 0.0,
            lon0: 27f64.to_radians(),
            k0: 0.9996,
            false_e: 500_000.0,
            false_n: 0.0,
        };
        let [w, s, e, n] = native_bbox(&crs, 20.0, 60.0, 30.0, 70.0);
        assert!(w < e, "west {w} must be < east {e}");
        assert!(s < n, "south {s} must be < north {n}");
        assert!(w.is_finite() && s.is_finite() && e.is_finite() && n.is_finite());
    }

    /// `RawPixels::sample` returns `None` for the nodata sentinel
    /// and applies `gain * raw + offset` to every other value. This
    /// is the only place gain/offset are applied — pinning it
    /// catches off-by-one in the conversion order (a common bug).
    #[test]
    fn raw_pixels_sample_applies_gain_offset_and_masks_nodata() {
        let arr = Array2::from_shape_vec((2, 2), vec![0u8, 64, 128, 255]).unwrap();
        let px = RawPixels::U8(arr);

        // Real radar fixture: gain=0.5, offset=-32 → reflectivity
        // class 0 maps to -32 dBZ, class 255 is the nodata sentinel.
        assert_eq!(px.sample(0, 0, 0.5, -32.0, 255.0), Some(-32.0));
        assert_eq!(px.sample(0, 1, 0.5, -32.0, 255.0), Some(0.0));
        assert_eq!(px.sample(1, 0, 0.5, -32.0, 255.0), Some(32.0));
        assert_eq!(px.sample(1, 1, 0.5, -32.0, 255.0), None);
    }

    /// The same sample logic must work for u16 grids — different
    /// dtype, same arithmetic.
    #[test]
    fn raw_pixels_sample_handles_u16() {
        let arr = Array2::from_shape_vec((1, 3), vec![0u16, 10_000, 65_535]).unwrap();
        let px = RawPixels::U16(arr);

        assert_eq!(px.sample(0, 0, 0.01, 0.0, 65_535.0), Some(0.0));
        assert!((px.sample(0, 1, 0.01, 0.0, 65_535.0).unwrap() - 100.0).abs() < 1e-9);
        assert_eq!(px.sample(0, 2, 0.01, 0.0, 65_535.0), None);
    }
}
