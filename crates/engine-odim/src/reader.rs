//! ODIM_H5 composite-reflectivity reader.
//!
//! Parses the four pieces of metadata that distinguish an ODIM
//! composite (COMP) file from an arbitrary HDF5 file:
//!
//! - `/what` — `object` (`"COMP"`), `date`/`time` (UTC stamp)
//! - `/where` — `projdef` (PROJ.4 string), `xsize`/`ysize`,
//!   four corner coordinates in WGS84
//! - per-quantity `what` group — `quantity`, `gain`, `offset`, `nodata`
//! - `/dataset1/data1/data` — the raw 2D scaled-integer pixel array
//!
//! Verified producers (and their layout quirks):
//!
//! | Producer | ODIM version | Pixel type | Renders | Notes                                       |
//! |----------|--------------|------------|---------|---------------------------------------------|
//! | DMI      | v2.0         | u8         | yes     | No `/where/xsize`/`ysize`; gain/offset/nodata at root `/what`; quantity as attr on `/dataset1/data1` |
//! | SMHI     | v2.2         | u8         | no      | Canonical layout; DEFLATE decompression fails in `hdf5-reader` 0.4 (upstream bug — `h5dump` reads the same file fine) |
//! | DWD      | v2.3         | u16        | yes     | Canonical layout; polar stere (lat_0=90, lat_ts=60); 250m grid over Germany; fine gain=0.00293, offset=-64 |
//! | OPERA    | v2.4         | f64        | yes     | Canonical layout; LAEA grid (EPSG:3035-style); already-decoded physical dBZ with `nodata=-9999000`, `undetect=-8888000` |
//!
//! "Canonical" here means the ODIM_H5 v2.4 §7.4 layout —
//! gain/offset/nodata/quantity under `/dataset<n>/data<m>/what`.
//!
//! Not currently exercised (no readily-available ODIM COMP source):
//! - FMI — open-data S3 ships only PVOL HDF5; composites are
//!   published as GeoTIFF, served by engine-geotiff
//! - Per-country OPERA contributions on cloudferro — only PVOL/SCAN
//!
//! The output is an [`OdimComposite`] containing the parsed
//! [`Crs`], a native-coordinate bbox, the timestamp, parameter
//! metadata, and the raw pixel array. The raw → physical-units
//! conversion (`raw * gain + offset`) and nodata/undetect masking
//! happen **at sample time**, not at read time, so a multi-megapixel
//! grid doesn't pay 16 bytes/cell to be carried around as
//! `Vec<Option<f64>>`.
//!
//! Phase 1 narrows the scope further than the format allows:
//! - Single dataset (`/dataset1` only), single data layer (`/data1`)
//! - Three raw pixel types: u8, u16, f64 (all the variants we've
//!   actually encountered)
//! - No quality layers, no `how/*` attributes, no PVOL volume data
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
    #[error(
        "dataset `/dataset1/data1/data` has unsupported pixel type (Phase 1: u8, u16, or f64)"
    )]
    UnsupportedPixelType,
    #[error("dataset read failed: {0}")]
    DatasetRead(String),
    #[error("attribute read failed: {0}")]
    AttributeRead(String),
}

/// Raw pixel storage as it appears on disk. ODIM composites ship
/// one of:
/// - `u8`  — single-byte reflectivity classes (DMI v2.0)
/// - `u16` — extended dynamic range (some EUMETNET v2.x producers)
/// - `f64` — already-decoded physical values (OPERA v2.4 ACRR/DBZH)
///
/// For the integer variants the physical value is `raw * gain + offset`.
/// For the f64 variant the values are already in physical units, but
/// the gain/offset metadata is still applied for symmetry — typically
/// gain=1, offset=0, so it's a no-op.
#[derive(Debug, Clone)]
pub enum RawPixels {
    U8(Array2<u8>),
    U16(Array2<u16>),
    F64(Array2<f64>),
}

impl RawPixels {
    /// Shape as `(height, width)` — same ordering ndarray uses
    /// internally. The first axis is rows (y), the second is columns
    /// (x), matching HDF5's row-major layout.
    pub fn shape(&self) -> (usize, usize) {
        match self {
            RawPixels::U8(a) => a.dim(),
            RawPixels::U16(a) => a.dim(),
            RawPixels::F64(a) => a.dim(),
        }
    }

    /// Read a single pixel at `(row, col)`, returning `None` for the
    /// nodata or undetect sentinels and `Some(physical)` otherwise.
    /// `physical = raw * gain + offset`. Out-of-range indices return
    /// `None`.
    ///
    /// Both `nodata` ("outside coverage") and `undetect` ("radar
    /// looked but saw nothing") are treated as masked in Phase 1 —
    /// neither renders as a colored pixel. Splitting them into
    /// distinct visible classes can be revisited if a use case asks
    /// for it (e.g. precipitation accumulation overlays often want
    /// undetect = 0 mm, not transparent).
    pub fn sample(
        &self,
        row: usize,
        col: usize,
        gain: f64,
        offset: f64,
        nodata: f64,
        undetect: Option<f64>,
    ) -> Option<f64> {
        let raw = match self {
            RawPixels::U8(a) => a.get((row, col)).map(|v| *v as f64)?,
            RawPixels::U16(a) => a.get((row, col)).map(|v| *v as f64)?,
            RawPixels::F64(a) => a.get((row, col)).copied()?,
        };
        // Exact equality is intentional for nodata/undetect: ODIM
        // producers store integer sentinels (e.g. OPERA's
        // -9_999_000 / -8_888_000) that are bit-exact representable
        // in f64, and writers don't apply any FP arithmetic to them
        // before serialising. An epsilon comparison here would risk
        // either masking legitimate values (false positive) or
        // letting a near-sentinel through (false negative); stick to
        // exact == until a producer is observed shipping non-integer
        // sentinels.
        if raw == nodata {
            return None;
        }
        if let Some(u) = undetect {
            if raw == u {
                return None;
            }
        }
        Some(raw * gain + offset)
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
    /// Optional raw value indicating "no echo detected" — distinct
    /// from `nodata` (which means the radar didn't look). ODIM v2.4
    /// §7.4.2 makes this mandatory; older producers may omit it.
    pub undetect: Option<f64>,
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

    // Conventions: ODIM_H5/V2_0, V2_1, V2_2, V2_3, V2_4 etc. Currently
    // logged for diagnostics only — the structural variations between
    // versions are handled below via per-attribute fallbacks rather
    // than version-gated branches.
    let conventions = read_string_attr("/", "Conventions").ok();
    if let Some(ref c) = conventions {
        tracing::debug!("ODIM file conventions: {}", c);
    }

    // /what — object kind + timestamp.
    let object = read_string_attr("/what", "object")?;
    if object.trim_end_matches('\0').trim() != "COMP" {
        return Err(ReadError::NotComposite(object));
    }
    let date = read_string_attr("/what", "date")?;
    let time_str = read_string_attr("/what", "time")?;
    let time = parse_odim_timestamp(&date, &time_str)?;

    // /where — projection + grid extents. ODIM_H5 v2.4 §6.1.2 makes
    // all four corner pairs mandatory (LL, LR, UL, UR); some older
    // producers ship only LL+UR (the canonical "axis-aligned in
    // lat/lon" case). Read all four when present and fall back to
    // synthesising LR/UL from LL/UR when they're missing.
    let projdef = read_string_attr("/where", "projdef")?;
    let crs = proj::parse(&projdef)?;
    let ll_lon = read_f64_attr("/where", "LL_lon")?;
    let ll_lat = read_f64_attr("/where", "LL_lat")?;
    let ur_lon = read_f64_attr("/where", "UR_lon")?;
    let ur_lat = read_f64_attr("/where", "UR_lat")?;
    // The `ul_lon = ll_lon, ul_lat = ur_lat` synthesis below is only
    // correct for axis-aligned lon/lat grids. On a projected grid
    // (stereographic, LAEA, TM, LCC) the UL corner's WGS84
    // longitude differs materially from LL's because the projection
    // bends the parallels — a 1984×1728 grid at 500 m/px can show
    // several degrees of offset. Refuse the synthesis there so a
    // future producer that omits UL on a projected composite fails
    // loudly rather than producing a corrupted bbox anchor.
    let ul_lon = match read_f64_attr("/where", "UL_lon") {
        Ok(v) => v,
        Err(_) if matches!(crs, Crs::Wgs84) => ll_lon,
        Err(_) => {
            return Err(ReadError::MissingAttribute {
                group: "/where".into(),
                name: "UL_lon (mandatory on projected grids)".into(),
            });
        }
    };
    let ul_lat = match read_f64_attr("/where", "UL_lat") {
        Ok(v) => v,
        Err(_) if matches!(crs, Crs::Wgs84) => ur_lat,
        Err(_) => {
            return Err(ReadError::MissingAttribute {
                group: "/where".into(),
                name: "UL_lat (mandatory on projected grids)".into(),
            });
        }
    };
    let wgs84_corners = [ll_lon, ll_lat, ur_lon, ur_lat];

    // /dataset1/data1/data — read shape early so xsize/ysize can fall
    // back to the data array dimensions for producers (e.g. DMI) that
    // don't ship `/where/xsize`/`ysize`. We re-open the dataset for the
    // pixel-array read below; the cost is negligible.
    let ds = file
        .dataset("/dataset1/data1/data")
        .map_err(|_| ReadError::MissingGroup("/dataset1/data1/data".into()))?;
    let shape = ds.shape();
    if shape.len() != 2 {
        return Err(ReadError::UnsupportedRank(shape.len()));
    }
    let rows = shape[0] as usize;
    let cols = shape[1] as usize;

    // Prefer explicit `/where/xsize`/`ysize` when present (SMHI v2.2
    // ships them); fall back to the data-array shape (DMI v2.0
    // doesn't). If both exist and disagree, trust /where — the spec
    // wins over what could be a transposed array on the producer
    // side.
    //
    // Range-check the f64 → u32 cast: a negative xsize wraps to a
    // huge u32, an out-of-range value silently truncates, and a NaN
    // becomes 0. All would silently corrupt the bbox computation
    // downstream. Reject them rather than fall back — we'd rather
    // refuse a bad file than render with a wrong extent.
    //
    // The cap is `MAX_GRID_DIM`, not `u32::MAX`: a malformed file
    // declaring `xsize = 4e9` would otherwise pass a `u32::MAX`
    // check and reach `hdf5-reader`'s allocation path, attempting a
    // multi-gigabyte allocation before any controlled error fires.
    // 100 000 is comfortably larger than any real ODIM composite
    // (OPERA's pan-European grid is ~4400 wide) so a legitimate
    // file is never rejected.
    const MAX_GRID_DIM: f64 = 100_000.0;
    let read_grid_dim = |name: &str, fallback: u32| -> Result<u32, ReadError> {
        match read_f64_attr("/where", name) {
            Ok(v) if v.is_finite() && (1.0..=MAX_GRID_DIM).contains(&v) => Ok(v as u32),
            Ok(v) => Err(ReadError::DatasetRead(format!(
                "/where/{name} is not a finite integer-valued f64 in [1, {MAX_GRID_DIM}]: {v}"
            ))),
            Err(_) => Ok(fallback),
        }
    };
    let xsize = read_grid_dim("xsize", cols as u32)?;
    let ysize = read_grid_dim("ysize", rows as u32)?;

    // Native bbox: build from `/where/xscale` + `/where/yscale` +
    // xsize/ysize (definitional grid dimensions in projected metres,
    // per ODIM v2.4 §6.1.2) anchored at the UL corner projected with
    // our forward function. This is **far more robust** than taking
    // the envelope of all four projected corners:
    //
    //   - Our `Crs::Stereographic` and the producer's projection
    //     may not numerically agree at the edges of the grid even
    //     when both implement the same EPSG formulation (different
    //     ellipsoid choices — DMI uses WGS84, SMHI uses Bessel —
    //     plus implementation details). The envelope of all four
    //     forward-projected corners thus picks up those errors and
    //     ends up off-axis or off-size; under that bbox most output
    //     samples miss the actual data.
    //
    //   - With xscale/yscale + anchor, the grid width/height is
    //     definitionally correct (= xsize·xscale, ysize·yscale)
    //     regardless of our projection's accuracy. Only the absolute
    //     position is biased — and that bias is invisible to the
    //     sampler because the same forward function projects both
    //     the anchor and per-pixel queries.
    //
    // xscale/yscale are mandatory in ODIM v2.x §6.1.2 for image
    // objects, so this path is reliable across producers.
    //
    // Validate finite + strictly positive: a zero, negative, or NaN
    // scale propagates silently into `src_dx`/`src_dy` in
    // `get_raster_tile`, where it produces an all-transparent tile
    // (every sample maps outside the grid) with no error — a
    // confusing failure mode. Reject at read time instead.
    let read_scale = |name: &str| -> Result<f64, ReadError> {
        let v = read_f64_attr("/where", name)?;
        if v.is_finite() && v > 0.0 {
            Ok(v)
        } else {
            Err(ReadError::DatasetRead(format!(
                "/where/{name} is not a finite positive f64: {v}"
            )))
        }
    };
    let xscale = read_scale("xscale")?;
    let yscale = read_scale("yscale")?;
    let (ul_x, ul_y) = crs.forward(ul_lon, ul_lat);
    let bbox = [
        ul_x,
        ul_y - ysize as f64 * yscale,
        ul_x + xsize as f64 * xscale,
        ul_y,
    ];
    tracing::debug!(
        "ODIM native bbox via UL+scale: {:?} ({}x{} pixels at {}x{} m/px)",
        bbox,
        xsize,
        ysize,
        xscale,
        yscale
    );

    // gain/offset/nodata/quantity location varies by producer:
    // - Earlier producers:           /dataset1/what/{...}          (some EUMETNET v2.x files)
    // - Canonical (ODIM v2.4 §7.4):  /dataset1/data1/what/{...}    (DWD v2.3, OPERA v2.4)
    // - DMI variant (v2.0):          /what/{gain,offset,nodata}    at the file root,
    //                                with `quantity` as an attribute on /dataset1/data1
    //                                (or `/what/product` as last resort)
    //
    // Order rationale: canonical path (`/dataset1/data1/what`,
    // ODIM v2.4 §7.4) wins over the older `/dataset1/what` form in
    // a hybrid file that ships both — a writer populating the
    // canonical location is the authoritative source, and tolerating
    // the older path is just a backwards-compat fallback.
    let what_paths = ["/dataset1/data1/what", "/dataset1/what", "/what"];
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
    let undetect = read_first_f64("undetect").ok();

    // Quantity has one extra fallback (DMI puts it as an attribute on
    // the `/dataset1/data1` data group itself, not in a what subgroup).
    let quantity = read_string_attr("/dataset1/data1/what", "quantity")
        .or_else(|_| read_string_attr("/dataset1/what", "quantity"))
        .or_else(|_| read_string_attr("/dataset1/data1", "quantity"))
        .or_else(|_| read_string_attr("/what", "product"))
        .map_err(|_| ReadError::MissingAttribute {
            group: "/dataset1/data1/what | /dataset1/what | /dataset1/data1 | /what".into(),
            name: "quantity".into(),
        })?;

    // Try u8 (DMI v2.0, SMHI v2.2), u16 (some EUMETNET v2.x, DWD
    // v2.3), then f64 (OPERA v2.4 — pre-decoded physical values).
    // `read_array` returns `Err` on dtype mismatch rather than
    // panicking, so the fallback chain is safe. Per-probe errors
    // are captured at `debug!` (every load of a DWD or OPERA file
    // would otherwise spam the u8 error) and only escalated to
    // `warn!` when every probe fails — at that point a likely
    // upstream cause (hdf5-reader 0.4 silently mis-handling
    // DEFLATE on certain SMHI files) is worth surfacing.
    let u8_probe = ds.read_array::<u8>();
    if let Err(ref e) = u8_probe {
        tracing::debug!("ODIM u8 pixel-array probe failed: {e}");
    }
    let pixels = if let Ok(arr) = u8_probe {
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
    } else {
        let u16_probe = ds.read_array::<u16>();
        if let Err(ref e) = u16_probe {
            tracing::debug!("ODIM u16 pixel-array probe failed: {e}");
        }
        if let Ok(arr) = u16_probe {
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
            let f64_probe = ds.read_array::<f64>();
            if let Err(ref e) = f64_probe {
                tracing::debug!("ODIM f64 pixel-array probe failed: {e}");
            }
            if let Ok(arr) = f64_probe {
                let a2 = arr
                    .into_dimensionality::<ndarray::Ix2>()
                    .map_err(|e| ReadError::DatasetRead(format!("f64 reshape failed: {e}")))?;
                if a2.dim() != (rows, cols) {
                    return Err(ReadError::DatasetRead(format!(
                        "f64 array shape {:?} doesn't match metadata {rows}x{cols}",
                        a2.dim()
                    )));
                }
                RawPixels::F64(a2)
            } else {
                // All three probes failed. This is the only point at
                // which a u8 probe error indicates something genuinely
                // wrong (a real u8 file that hdf5-reader couldn't
                // decompress, or an unsupported dtype like i32). Log
                // both the u8 and u16 errors at WARN since either may
                // be the diagnostic clue an operator needs.
                if let Err(e) = &u8_probe {
                    tracing::warn!(
                        "ODIM pixel-array unreadable as u8/u16/f64; u8 probe error \
                         (possible hdf5-reader DEFLATE bug on SMHI files): {e}"
                    );
                }
                return Err(ReadError::UnsupportedPixelType);
            }
        }
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
        undetect,
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
        assert_eq!(px.sample(0, 0, 0.5, -32.0, 255.0, None), Some(-32.0));
        assert_eq!(px.sample(0, 1, 0.5, -32.0, 255.0, None), Some(0.0));
        assert_eq!(px.sample(1, 0, 0.5, -32.0, 255.0, None), Some(32.0));
        assert_eq!(px.sample(1, 1, 0.5, -32.0, 255.0, None), None);
    }

    /// The same sample logic must work for u16 grids — different
    /// dtype, same arithmetic.
    #[test]
    fn raw_pixels_sample_handles_u16() {
        let arr = Array2::from_shape_vec((1, 3), vec![0u16, 10_000, 65_535]).unwrap();
        let px = RawPixels::U16(arr);

        assert_eq!(px.sample(0, 0, 0.01, 0.0, 65_535.0, None), Some(0.0));
        assert!((px.sample(0, 1, 0.01, 0.0, 65_535.0, None).unwrap() - 100.0).abs() < 1e-9);
        assert_eq!(px.sample(0, 2, 0.01, 0.0, 65_535.0, None), None);
    }

    /// OPERA v2.4 ships pre-decoded f64 pixels with nodata=-9999000
    /// and undetect=-8888000. Both sentinels must mask to None; real
    /// values fall through unchanged (gain=1, offset=0).
    #[test]
    fn raw_pixels_sample_handles_f64_with_undetect() {
        let arr =
            Array2::from_shape_vec((1, 4), vec![5.0_f64, 25.0, -9999000.0, -8888000.0]).unwrap();
        let px = RawPixels::F64(arr);

        // OPERA's real config: gain=1, offset=0
        assert_eq!(
            px.sample(0, 0, 1.0, 0.0, -9999000.0, Some(-8888000.0)),
            Some(5.0)
        );
        assert_eq!(
            px.sample(0, 1, 1.0, 0.0, -9999000.0, Some(-8888000.0)),
            Some(25.0)
        );
        assert_eq!(
            px.sample(0, 2, 1.0, 0.0, -9999000.0, Some(-8888000.0)),
            None,
            "nodata sentinel must mask"
        );
        assert_eq!(
            px.sample(0, 3, 1.0, 0.0, -9999000.0, Some(-8888000.0)),
            None,
            "undetect sentinel must mask"
        );
    }
}
